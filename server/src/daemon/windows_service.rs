#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use crate::daemon::windows_service_stop::{
        CHILD_EXIT_WAIT, CHILD_KILL_WAIT, PRESHUTDOWN_TIMEOUT, ProcessTree, STOP_DEADLINE,
        StopReporter, StopWait, UNINSTALL_POLL, UNINSTALL_STOP_WAIT, UPDATE_START_WAIT,
        os as stop_os, tick_until_stopped, wait_until_stopped,
    };
    use crate::daemon::windows_service_update::{
        StagedUpdate, installed_command, split_command_line,
    };
    use desk_utils::host_data_paths::HostDataPaths;
    use desk_virtual_display_driver_ops::{DriverInstallerOps, RealInstaller};
    use log::{error, info};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::sync::oneshot;
    use windows_service::{
        Result as WsResult, define_windows_service,
        service::{
            Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
            ServiceErrorControl, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
            ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    pub const SERVICE_NAME: &str = "LcxlDeskService";
    pub const SERVICE_DISPLAY_NAME: &str = "LCXL Remote Desktop Service";

    define_windows_service!(ffi_service_main, service_main);

    /// Restart delays after the first, second and later failures. The
    /// failure count resets after a day without failures.
    const RESTART_DELAYS: [Duration; 3] = [
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(30),
    ];
    const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

    /// The service has no other way back after a crash than a reboot, so SCM
    /// restarts it, also when it stops itself with a failure exit code.
    fn failure_recovery_actions() -> ServiceFailureActions {
        ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_PERIOD),
            reboot_msg: None,
            command: None,
            actions: Some(
                RESTART_DELAYS
                    .iter()
                    .map(|&delay| ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay,
                    })
                    .collect(),
            ),
        }
    }

    /// Restart actions after a failure, and time to stop cleanly at system
    /// shutdown (a wait hint does not extend the shutdown budget; only
    /// preshutdown does).
    fn configure_failure_recovery(service: &Service) -> WsResult<()> {
        service.update_failure_actions(failure_recovery_actions())?;
        service.set_failure_actions_on_non_crash_failures(true)?;
        service.set_preshutdown_timeout(PRESHUTDOWN_TIMEOUT)
    }

    /// Installs that predate the recovery actions get them on their next
    /// start, without a reinstall.
    fn ensure_failure_recovery() {
        let result = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|manager| manager.open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG))
            .and_then(|service| configure_failure_recovery(&service));
        if let Err(error) = result {
            log::warn!(
                "[WindowsService] could not set the recovery and preshutdown settings: {error}"
            );
        }
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            error!("[WindowsService] Fatal error: {e}");
        }
    }

    type Reporter = StopReporter<Box<dyn FnMut(ServiceStatus) + Send>>;

    /// A stop that outlived [`STOP_DEADLINE`]: terminate what is left of our
    /// processes, report `Stopped` (a success code, so SCM does not restart a
    /// service that is being stopped, uninstalled or updated) and exit.
    fn stop_deadline_passed(reporter: &Reporter, tree: &Mutex<ProcessTree>) {
        error!("[WindowsService] stop did not finish in {STOP_DEADLINE:?}; terminating");
        let mut tree = tree.lock().unwrap_or_else(|poison| poison.into_inner());
        let (terminated, left) =
            stop_os::reap_descendants(&mut tree, Duration::ZERO, CHILD_KILL_WAIT);
        error!("[WindowsService] terminated {terminated} process(es), {left} still running");
        reporter.finish(false);
        std::process::exit(0);
    }

    /// Report `StopPending` and keep its checkpoint moving until `Stopped`.
    fn begin_stop(reporter: &Arc<Reporter>, tree: &Arc<Mutex<ProcessTree>>, requested: bool) {
        if !reporter.begin(requested) {
            return;
        }
        // Remember the workers and their children now: a worker killed
        // during shutdown no longer links its children to us.
        if let Ok(mut tree) = tree.lock() {
            tree.refresh(&stop_os::snapshot());
        }
        let (reporter, tree) = (reporter.clone(), tree.clone());
        let _ = std::thread::Builder::new()
            .name("service-stop".into())
            .spawn(move || {
                tick_until_stopped(&reporter, std::thread::sleep, || {
                    stop_deadline_passed(&reporter, &tree)
                })
            });
    }

    fn run_service() -> WsResult<()> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop_tx = Mutex::new(Some(stop_tx));
        let reporter_cell: Arc<OnceLock<Arc<Reporter>>> = Arc::new(OnceLock::new());
        let tree = Arc::new(Mutex::new(stop_os::own_tree()));

        let handler_reporter = reporter_cell.clone();
        let handler_tree = tree.clone();
        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |ctrl| match ctrl {
                ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                    // Pending first, then the daemon; the daemon is told to
                    // stop even if status reporting is not set up.
                    if let Some(reporter) = handler_reporter.get() {
                        begin_stop(reporter, &handler_tree, true);
                    }
                    if let Ok(mut guard) = stop_tx.lock()
                        && let Some(tx) = guard.take()
                    {
                        let _ = tx.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        let report: Box<dyn FnMut(ServiceStatus) + Send> = Box::new(move |status| {
            if let Err(error) = status_handle.set_service_status(status) {
                error!("[WindowsService] could not report the service status: {error}");
            }
        });
        let reporter = reporter_cell
            .get_or_init(|| Arc::new(StopReporter::new(report)))
            .clone();
        // SCM sends Stop only once `Running` accepts it, so the handler
        // always finds the reporter.
        reporter.running();

        info!("[WindowsService] Service Running");
        ensure_failure_recovery();

        let failed = crate::daemon::run_daemon_system(move || {
            crate::daemon::run_actix_system(async {
                use crate::daemon::run_service_daemon_inner;
                use clap::Parser;
                let args = crate::model::settings::Args::parse();
                // Pass the shutdown receiver directly so the daemon can reach
                // shutdown_all() before we set the Stopped status.
                match run_service_daemon_inner(args, Some(stop_rx)).await {
                    Ok(()) => false,
                    Err(e) => {
                        error!("[WindowsService] Daemon error: {e}");
                        true
                    }
                }
            })
        });

        // The daemon may also end on its own; report the stop either way.
        begin_stop(&reporter, &tree, false);
        // `Stopped` means none of our processes is left, so the exe can be
        // replaced or removed right after it.
        {
            let mut tree = tree.lock().unwrap_or_else(|poison| poison.into_inner());
            let (terminated, left) =
                stop_os::reap_descendants(&mut tree, CHILD_EXIT_WAIT, CHILD_KILL_WAIT);
            if terminated > 0 {
                log::warn!(
                    "[WindowsService] terminated {terminated} leftover process(es), {left} still running"
                );
            }
        }
        reporter.finish(failed);

        info!("[WindowsService] Service Stopped");
        Ok(())
    }

    /// Try to start the Windows Service dispatcher.
    ///
    /// Returns `Ok(true)` if we ran as a Windows Service.
    /// Returns `Ok(false)` if not running under SCM (interactive).
    pub fn try_run_as_service() -> Result<bool, Box<dyn std::error::Error>> {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(true),
            Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1063) => Ok(false),
            Err(e) => Err(Box::new(e)),
        }
    }

    /// Copy the server binary + static/ + drivers/ to `install_dir`,
    /// optionally stage the IDD driver, and register the result as a
    /// Windows Service in SCM. The order is fixed so the daemon never
    /// boots before its IDD driver is available: copy → install
    /// driver → create SCM entry → start service.
    pub fn install_service(
        install_dir: &str,
        install_idd_driver: bool,
        config_override: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let explicit_config = match config_override {
            Some(path) => Some(
                HostDataPaths::resolve_current(Some(path))?
                    .config_file()
                    .to_path_buf(),
            ),
            None => None,
        };
        install_service_with(
            install_dir,
            install_idd_driver,
            explicit_config.as_deref(),
            &RealInstaller,
        )
    }

    /// Test seam used by [`install_service`]. Allows the file-copy +
    /// driver-staging steps to be unit-tested with a mock installer.
    /// SCM registration (the steps that need a real Windows host) still
    /// runs in the production path.
    pub fn install_service_with(
        install_dir: &str,
        install_idd_driver: bool,
        config_override: Option<&Path>,
        installer: &dyn DriverInstallerOps,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // An installed service is updated with --update-service; installing
        // over it would overwrite a running build and then fail to register.
        if let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            && manager
                .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
                .is_ok()
        {
            return Err(format!(
                "Service '{SERVICE_NAME}' is already installed; run --update-service to update it"
            )
            .into());
        }
        let src_exe = std::env::current_exe()?;
        let src_dir = src_exe
            .parent()
            .ok_or("could not determine source directory")?
            .to_path_buf();

        let install_path = std::path::Path::new(install_dir);
        std::fs::create_dir_all(install_path)?;

        let file_name = src_exe.file_name().ok_or("current exe has no file name")?;
        let dst_exe = install_path.join(file_name);

        // Skip copy when source is already inside the install directory.
        let same_dir = src_exe
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .zip(install_path.canonicalize().ok())
            .map(|(src_dir, dst_dir)| src_dir == dst_dir)
            .unwrap_or(false);

        if !same_dir {
            std::fs::copy(&src_exe, &dst_exe)?;
            println!("Copied binary to {}", dst_exe.display());
        } else {
            println!("Binary already in the install directory, nothing to copy");
        }

        // Copy static/ alongside the binary so the daemon can serve the frontend.
        // Fail fast if the directory is missing — better to catch this at install
        // time than to discover the frontend is broken after the service starts.
        let src_static = src_dir.join("static");
        if !src_static.exists() {
            return Err(format!(
                "static/ directory not found at '{}'. \
                 Build the vite project and place the output in a 'static/' directory \
                 alongside the server binary before installing the service.",
                src_static.display()
            )
            .into());
        }
        // Copying the install directory onto itself would delete it first.
        if !same_dir {
            let dst_static = install_path.join("static");
            if dst_static.exists() {
                std::fs::remove_dir_all(&dst_static)?;
            }
            copy_dir_recursive(&src_static, &dst_static)?;
            println!("Copied static/ to {}", dst_static.display());
        }

        // Copy drivers/ + (optionally) stage the IDD driver BEFORE the
        // SCM entry is created — the daemon must not be allowed to
        // start until the driver is available, otherwise its
        // `SwDeviceCreate` will race against `pnputil`.
        copy_drivers_and_install_if_requested(
            &src_dir,
            install_path,
            install_idd_driver,
            installer,
        )?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;

        let info = service_info(dst_exe, ServiceStartType::AutoStart, config_override);
        let service =
            manager.create_service(&info, ServiceAccess::START | ServiceAccess::CHANGE_CONFIG)?;
        println!("Service '{SERVICE_NAME}' installed at '{install_dir}'");
        configure_failure_recovery(&service)?;
        service.start(&[] as &[&std::ffi::OsStr])?;
        println!("Service '{SERVICE_NAME}' started");
        Ok(())
    }

    /// The SCM entry a fresh install creates and an update rewrites. Other
    /// hand-added launch arguments are not kept.
    fn service_info(
        executable_path: PathBuf,
        start_type: ServiceStartType,
        config_override: Option<&Path>,
    ) -> ServiceInfo {
        let mut launch_arguments = vec![
            OsString::from("--startup-mode"),
            OsString::from("service-daemon"),
        ];
        if let Some(path) = config_override {
            launch_arguments.push(OsString::from("--config-file-path"));
            launch_arguments.push(path.as_os_str().to_os_string());
        }
        ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type,
            error_control: ServiceErrorControl::Normal,
            executable_path,
            launch_arguments,
            dependencies: vec![],
            account_name: None,
            account_password: None,
        }
    }

    fn same_directory(left: &Path, right: &Path) -> bool {
        match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => left
                .to_string_lossy()
                .eq_ignore_ascii_case(&right.to_string_lossy()),
        }
    }

    /// How stopping the service for an uninstall or update ended.
    enum StopOutcome {
        WasStopped,
        Stopped,
        NotStopped(String),
    }

    /// Stop the service and wait until it reports `STOPPED` and its process
    /// has exited (SCM sees `STOPPED` a few ms before the process is gone).
    fn stop_and_wait(service: &Service) -> StopOutcome {
        let status = match service.query_status() {
            Ok(status) => status,
            Err(error) => {
                return StopOutcome::NotStopped(format!("could not query the service: {error}"));
            }
        };
        if status.current_state == ServiceState::Stopped {
            return StopOutcome::WasStopped;
        }
        // Opened before the stop so the pid cannot be reused meanwhile.
        let process = status.process_id.and_then(stop_os::open_for_wait);
        println!("Stopping the service...");
        let polls = (UNINSTALL_STOP_WAIT.as_millis() / UNINSTALL_POLL.as_millis()) as u32;
        match wait_until_stopped(
            || service.query_status().map(|status| status.current_state),
            || {
                let _ = service.stop();
            },
            std::thread::sleep,
            polls,
        ) {
            StopWait::Stopped => {}
            StopWait::TimedOut => {
                return StopOutcome::NotStopped(format!(
                    "the service did not stop within {UNINSTALL_STOP_WAIT:?}"
                ));
            }
            StopWait::QueryFailed(error) => {
                return StopOutcome::NotStopped(format!(
                    "could not query the service while stopping it: {error}"
                ));
            }
        }
        match (status.process_id, process) {
            (Some(_), Some(process)) if !stop_os::wait_for_exit(&process, CHILD_EXIT_WAIT) => {
                StopOutcome::NotStopped("the service process is still running".into())
            }
            (Some(pid), None) => {
                StopOutcome::NotStopped(format!("could not watch the service process {pid}"))
            }
            _ => StopOutcome::Stopped,
        }
    }

    /// Wait until a started service runs (or gives up), so a build that
    /// fails at startup is rolled back.
    fn wait_until_running(service: &Service) -> Result<(), String> {
        for _ in 0..(UPDATE_START_WAIT.as_millis() / UNINSTALL_POLL.as_millis()) {
            match service.query_status() {
                Ok(status) if status.current_state == ServiceState::Running => return Ok(()),
                Ok(status) if status.current_state == ServiceState::Stopped => {
                    return Err("the new build stopped right after starting".into());
                }
                Ok(_) => {}
                Err(error) => return Err(format!("could not query the service: {error}")),
            }
            std::thread::sleep(UNINSTALL_POLL);
        }
        Err(format!(
            "the new build did not reach RUNNING within {UPDATE_START_WAIT:?}"
        ))
    }

    /// While the files are replaced SCM must not restart the service (the
    /// recovery actions are set again afterwards).
    fn no_restart_actions() -> ServiceFailureActions {
        ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_PERIOD),
            reboot_msg: None,
            command: None,
            actions: Some(Vec::new()),
        }
    }

    /// `--update-service`: replace the installed build in place (plan
    /// `2026-11-20_service-update-and-bfcache-signaling`, part A). The new
    /// files are staged next to the old ones while the service runs; the
    /// service is then stopped (and its process gone), the files swapped,
    /// the service reconfigured and started. Any failure after the stop
    /// puts the previous build back and starts it.
    pub fn update_service(
        install_path: Option<&str>,
        install_idd_driver: bool,
        config_override: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let explicit_config = match config_override {
            Some(path) => Some(
                HostDataPaths::resolve_current(Some(path))?
                    .config_file()
                    .to_path_buf(),
            ),
            None => None,
        };
        update_service_with(
            install_path,
            install_idd_driver,
            explicit_config.as_deref(),
            &RealInstaller,
        )
    }

    pub fn update_service_with(
        install_path: Option<&str>,
        install_idd_driver: bool,
        config_override: Option<&Path>,
        installer: &dyn DriverInstallerOps,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_CONFIG
                    | ServiceAccess::QUERY_STATUS
                    | ServiceAccess::STOP
                    | ServiceAccess::START
                    | ServiceAccess::CHANGE_CONFIG,
            )
            .map_err(|error| {
                format!(
                    "Service '{SERVICE_NAME}' is not installed or not accessible ({error}); \
                     run --install-service to install it"
                )
            })?;
        let config = service.query_config()?;
        let installed = installed_command(&split_command_line(
            &config.executable_path.to_string_lossy(),
        ))
        .ok_or("could not read the installed service command line")?;
        let installed_dir = installed
            .exe
            .parent()
            .ok_or("the installed executable has no directory")?
            .to_path_buf();
        if let Some(requested) = install_path
            && !same_directory(Path::new(requested), &installed_dir)
        {
            return Err(format!(
                "The service is installed at '{}'; uninstall it first, or pass that directory",
                installed_dir.display()
            )
            .into());
        }
        let config_file_path = config_override
            .map(Path::to_path_buf)
            .or(installed.config_file_path.clone());

        let src_exe = std::env::current_exe()?;
        let src_dir = src_exe
            .parent()
            .ok_or("could not determine source directory")?
            .to_path_buf();
        let mut update = StagedUpdate::default();
        if same_directory(&src_dir, &installed_dir) {
            println!(
                "Running from the install directory: only the service configuration is refreshed"
            );
        } else {
            let src_static = src_dir.join("static");
            if !src_static.exists() {
                return Err(
                    format!("static/ directory not found at '{}'", src_static.display()).into(),
                );
            }
            println!("Staging the new build in {}", installed_dir.display());
            let staged = update
                .stage(&src_exe, &installed.exe)
                .and_then(|()| update.stage(&src_static, &installed_dir.join("static")))
                .and_then(|()| {
                    let src_drivers = src_dir.join("drivers");
                    if src_drivers.exists() {
                        update.stage(&src_drivers, &installed_dir.join("drivers"))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = staged {
                update.discard();
                return Err(format!("could not stage the new build: {error}").into());
            }
        }

        if let Err(error) = service.update_failure_actions(no_restart_actions()) {
            eprintln!("Warning: could not pause the restart-on-failure actions: {error}");
        }
        if let StopOutcome::NotStopped(why) = stop_and_wait(&service) {
            update.discard();
            let _ = configure_failure_recovery(&service);
            return Err(format!("{why}; nothing was changed").into());
        }

        let disabled = config.start_type == ServiceStartType::Disabled;
        let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            update.apply()?;
            if install_idd_driver {
                copy_drivers_and_install_if_requested(
                    &installed_dir,
                    &installed_dir,
                    true,
                    installer,
                )?;
            }
            service.change_config(&service_info(
                installed.exe.clone(),
                config.start_type,
                config_file_path.as_deref(),
            ))?;
            configure_failure_recovery(&service)?;
            if !disabled {
                service.start(&[] as &[&std::ffi::OsStr])?;
                wait_until_running(&service)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                update.commit();
                if disabled {
                    println!(
                        "Service '{SERVICE_NAME}' updated; it is disabled, so it was not started"
                    );
                } else {
                    println!("Service '{SERVICE_NAME}' updated and running");
                }
                Ok(())
            }
            Err(error) => {
                eprintln!("Update failed: {error}; restoring the previous build");
                let _ = service.stop();
                let _ = stop_and_wait(&service);
                if let Err(rollback) = update.rollback() {
                    eprintln!("Warning: could not restore every file: {rollback}");
                }
                let _ = configure_failure_recovery(&service);
                if !disabled {
                    match service.start(&[] as &[&std::ffi::OsStr]) {
                        Ok(()) => eprintln!("The previous build was started again"),
                        Err(start) => {
                            eprintln!("Warning: could not start the previous build: {start}")
                        }
                    }
                }
                Err(error)
            }
        }
    }

    /// Pure (SCM-free) helper extracted from [`install_service_with`]
    /// so it can be unit-tested with a mock installer on a non-Windows
    /// CI host. Copies `<src_dir>/drivers/` into `<install_path>/drivers/`
    /// (recreating an existing directory) and, when
    /// `install_idd_driver` is true, asks `installer` to stage the
    /// LcxlVirtualDisplay driver.
    pub fn copy_drivers_and_install_if_requested(
        src_dir: &Path,
        install_path: &Path,
        install_idd_driver: bool,
        installer: &dyn DriverInstallerOps,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let src_drivers = src_dir.join("drivers");
        let dst_drivers = install_path.join("drivers");
        if src_drivers.exists() && same_directory(&src_drivers, &dst_drivers) {
            // Already in place (copying a directory onto itself deletes it).
        } else if src_drivers.exists() {
            if dst_drivers.exists() {
                std::fs::remove_dir_all(&dst_drivers)?;
            }
            copy_dir_recursive(&src_drivers, &dst_drivers)?;
            println!("Copied drivers/ to {}", dst_drivers.display());
        } else {
            // Missing source drivers/ must not block plain installs;
            // the user can still stage the driver later from the
            // service-daemon UI once they place files manually.
            eprintln!(
                "Warning: drivers/ not found at '{}'; the IDD driver cannot be staged \
                 from the install directory",
                src_drivers.display()
            );
        }

        if install_idd_driver {
            let files = installer.discover(install_path)?.ok_or_else(|| {
                format!(
                    "IDD driver files missing under '{}'. \
                     Place LcxlVirtualDisplay driver files in <install_dir>/drivers/LcxlVirtualDisplay/.",
                    install_path.display()
                )
            })?;
            installer.install(&files)?;
            println!("LcxlVirtualDisplay driver staged via pnputil");
        }
        Ok(())
    }

    /// Remove the Windows Service registration. Strict order:
    ///   1. stop the service (so the daemon releases its `SwDevice` handle);
    ///   2. uninstall every published OEM copy of the IDD driver
    ///      (best-effort — failure is logged and we still proceed);
    ///   3. delete the SCM entry;
    ///   4. remove the install directory.
    pub fn uninstall_service() -> WsResult<()> {
        uninstall_service_with(&RealInstaller)
    }

    /// Test seam used by [`uninstall_service`]. SCM steps still run in
    /// the production path; the [`DriverInstallerOps`] indirection lets
    /// the driver-uninstall side be mocked independently.
    pub fn uninstall_service_with(installer: &dyn DriverInstallerOps) -> WsResult<()> {
        // No logger runs for `--uninstall-service`: progress goes to the console.
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        // Stop and query access is required: removing the driver and the
        // files of a service that may still run is not safe.
        let service = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_CONFIG
                | ServiceAccess::QUERY_STATUS
                | ServiceAccess::STOP
                | ServiceAccess::DELETE,
        )?;

        // cfg.executable_path is the raw SCM command line: exe_path + launch args.
        let exe_path = service.query_config().ok().and_then(|cfg| {
            installed_command(&split_command_line(&cfg.executable_path.to_string_lossy()))
                .map(|command| command.exe)
        });

        if let StopOutcome::NotStopped(why) = stop_and_wait(&service) {
            eprintln!("Warning: {why}; continuing");
        }

        // Uninstall the IDD driver BEFORE deleting the SCM entry so a
        // partial-failure leaves the user with a recoverable "service
        // entry still exists, retry uninstall" state instead of an
        // orphaned driver and no service to bind it to.
        uninstall_driver_after_service_stopped(installer);

        service.delete()?;
        println!("Service '{SERVICE_NAME}' removed");

        if let Some(exe_path) = exe_path {
            // Wait up to 5 seconds for the exe file lock to be released before
            // removing the whole install directory.
            for _ in 0..10 {
                if !exe_path.exists() || std::fs::remove_file(&exe_path).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }

            if let Some(parent) = exe_path.parent() {
                if std::fs::remove_dir_all(parent).is_ok() {
                    println!("Removed install directory {}", parent.display());
                } else {
                    eprintln!(
                        "Warning: could not fully remove install directory {}",
                        parent.display()
                    );
                }
            }
        }

        Ok(())
    }

    /// Pure (SCM-free) helper extracted from [`uninstall_service_with`]
    /// so the driver-uninstall side can be unit-tested with a mock
    /// installer. Failure is best-effort: a partial failure is logged
    /// and the caller proceeds with SCM cleanup so the user isn't left
    /// with a service-entry that refuses to delete because of a stale
    /// pnputil state.
    pub fn uninstall_driver_after_service_stopped(installer: &dyn DriverInstallerOps) {
        match installer.uninstall_all() {
            Ok(0) => println!("No LcxlVirtualDisplay driver package to uninstall"),
            Ok(n) => println!("Uninstalled {n} LcxlVirtualDisplay driver package(s)"),
            Err(e) => {
                eprintln!("Warning: driver uninstall failed: {e}; continuing with service removal")
            }
        }
    }

    fn copy_dir_recursive(
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_recursive(&entry.path(), &dst_path)?;
            } else {
                std::fs::copy(entry.path(), &dst_path)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_failed_service_is_restarted_with_growing_delays() {
            let actions = failure_recovery_actions();
            assert!(matches!(
                actions.reset_period,
                ServiceFailureResetPeriod::After(period) if period == FAILURE_RESET_PERIOD
            ));
            let delays: Vec<_> = actions
                .actions
                .unwrap()
                .iter()
                .map(|action| {
                    assert_eq!(action.action_type, ServiceActionType::Restart);
                    action.delay.as_secs()
                })
                .collect();
            assert_eq!(delays, [5, 10, 30]);
        }
        use desk_virtual_display_driver_ops::{
            DRIVER_CAT_BASENAME, DRIVER_DLL_BASENAME, DRIVER_HW_ID, DRIVER_INF_BASENAME,
            DRIVER_WUDFRD_BASENAME, DriverFiles, InstallerError,
        };
        use std::sync::Mutex;
        use tempfile::TempDir;

        struct MockInstaller {
            installed: Mutex<Vec<DriverFiles>>,
            uninstall_count: Mutex<u32>,
            uninstall_result: Mutex<Result<usize, InstallerError>>,
            discover_override: Mutex<Option<Option<DriverFiles>>>,
        }

        impl MockInstaller {
            fn new() -> Self {
                Self {
                    installed: Mutex::new(Vec::new()),
                    uninstall_count: Mutex::new(0),
                    uninstall_result: Mutex::new(Ok(0)),
                    discover_override: Mutex::new(None),
                }
            }
            fn set_uninstall_result(self, r: Result<usize, InstallerError>) -> Self {
                *self.uninstall_result.lock().unwrap() = r;
                self
            }
            fn force_discover_none(self) -> Self {
                *self.discover_override.lock().unwrap() = Some(None);
                self
            }
        }

        impl DriverInstallerOps for MockInstaller {
            fn discover(&self, base_dir: &Path) -> Result<Option<DriverFiles>, InstallerError> {
                if let Some(o) = self.discover_override.lock().unwrap().clone() {
                    return Ok(o);
                }
                // Default: pretend the files are always present so the
                // happy-path install flow exercises both `discover`
                // and `install` without needing real driver bits on
                // disk.
                Ok(Some(DriverFiles::from_dir(
                    base_dir.join("drivers").join(DRIVER_HW_ID),
                )))
            }
            fn install(&self, files: &DriverFiles) -> Result<(), InstallerError> {
                self.installed.lock().unwrap().push(files.clone());
                Ok(())
            }
            fn uninstall_all(&self) -> Result<usize, InstallerError> {
                *self.uninstall_count.lock().unwrap() += 1;
                // `replace` lets us pass the prebuilt result through
                // without needing InstallerError: Clone.
                std::mem::replace(&mut *self.uninstall_result.lock().unwrap(), Ok(0))
            }
        }

        fn seed_driver_dir(src: &Path) {
            let drivers = src.join("drivers").join(DRIVER_HW_ID);
            std::fs::create_dir_all(&drivers).unwrap();
            for name in [
                DRIVER_INF_BASENAME,
                DRIVER_CAT_BASENAME,
                DRIVER_DLL_BASENAME,
                DRIVER_WUDFRD_BASENAME,
            ] {
                std::fs::write(drivers.join(name), b"").unwrap();
            }
        }

        /// Happy path: drivers/ exists on the source side, the user
        /// opted into IDD installation, the helper copies the directory
        /// and asks the installer to stage the driver exactly once.
        #[test]
        fn copy_drivers_and_install_with_flag_invokes_installer_once() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), true, &installer)
                .expect("happy path must succeed");

            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists(),
                "drivers/ must be copied into the install dir"
            );
            let installed = installer.installed.lock().unwrap();
            assert_eq!(installed.len(), 1, "installer.install must be invoked once");
        }

        /// When the user does not opt in, drivers/ is still copied so the
        /// service-daemon UI can later stage the driver itself; but
        /// `installer.install` must NOT be called.
        #[test]
        fn copy_drivers_without_flag_skips_installer() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .expect("should succeed without staging");

            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists()
            );
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Missing source drivers/ + no opt-in: helper succeeds with a
        /// warn log.
        #[test]
        fn missing_drivers_dir_without_flag_is_ok() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            // No seed_driver_dir — src/drivers/ does not exist.

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .expect("missing drivers/ must not block plain installs");
            assert!(!dst.path().join("drivers").exists());
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Opt-in but `installer.discover` returns None — the install
        /// must fail (a partial install with the driver missing is
        /// strictly worse than a hard error the user can address).
        #[test]
        fn opt_in_with_missing_driver_files_errors() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new().force_discover_none();
            let err =
                copy_drivers_and_install_if_requested(src.path(), dst.path(), true, &installer)
                    .expect_err("missing driver files must error");
            assert!(err.to_string().contains("IDD driver files missing"));
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Existing drivers/ in the destination is removed first so a
        /// stale install layout never wins over the freshly-copied
        /// files.
        #[test]
        fn existing_destination_drivers_dir_is_replaced() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());
            std::fs::create_dir_all(dst.path().join("drivers").join("stale")).unwrap();
            std::fs::write(
                dst.path()
                    .join("drivers")
                    .join("stale")
                    .join("leftover.txt"),
                b"",
            )
            .unwrap();

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .unwrap();

            assert!(
                !dst.path().join("drivers").join("stale").exists(),
                "stale subdir must be cleared before copy"
            );
            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists()
            );
        }

        /// Uninstall helper calls `uninstall_all` exactly once.
        #[test]
        fn uninstall_helper_calls_installer_once() {
            let installer = MockInstaller::new().set_uninstall_result(Ok(2));
            uninstall_driver_after_service_stopped(&installer);
            assert_eq!(*installer.uninstall_count.lock().unwrap(), 1);
        }

        /// `installer.uninstall_all` failing must NOT panic the helper —
        /// SCM cleanup still needs to run.
        #[test]
        fn uninstall_helper_tolerates_installer_failure() {
            let installer =
                MockInstaller::new().set_uninstall_result(Err(InstallerError::StatusUnknown));
            uninstall_driver_after_service_stopped(&installer);
            assert_eq!(*installer.uninstall_count.lock().unwrap(), 1);
        }
    }
}

/// Returns the default service installation directory for this platform.
pub fn default_install_dir() -> String {
    #[cfg(target_os = "windows")]
    {
        let pf = std::env::var("PROGRAMFILES").unwrap_or_else(|_| "C:\\Program Files".to_string());
        format!("{}\\LCXL Remote Desktop", pf)
    }
    #[cfg(target_os = "linux")]
    {
        "/usr/lib/lcxl-remote-desk".to_string()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        "/opt/lcxl-remote-desk".to_string()
    }
}
