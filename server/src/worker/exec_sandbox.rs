//! Write sandbox of an AI command.
//!
//! A sandboxed command may write only below its write roots, a temporary
//! directory created for this one execution, and a few platform locations
//! (`/dev/shm` on Linux, the user temporary directory except this product's
//! entries on macOS, the terminal and null devices). Reading and executing are
//! not restricted. The sandbox is defence in depth against mistaken and
//! out-of-scope writes, not an isolation boundary: same-user brokers (service
//! managers, D-Bus, launchd, container sockets) remain reachable.
//!
//! The worker probes the platform once ([`sandbox_support`]) and keeps
//! the result for its lifetime. Before each sandboxed spawn it
//! 1. takes the already opened write roots, resolves their real paths from
//!    the handles and refuses forbidden roots
//!    ([`ProtectedRoots::refuses_write_root`]);
//! 2. creates the execution's private temporary directory ([`PerExecTemp`]);
//! 3. builds the platform restriction ([`PreparedSandbox`]): a Landlock
//!    ruleset on Linux, a `sandbox-exec` profile with path parameters on macOS.
//!
//! Any failure refuses the command. Process-tree reclamation stays with
//! `exec_containment`; sandboxed commands use its `setsid` form so they have
//! no controlling terminal.
// Without a backend no restriction is ever built.
#![cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use desk_agent_protocol::exec::{
    ExecIdentity, ExecPlan, ExecSandboxSupport, ExecSandboxUnsupportedReason,
};
use desk_agent_protocol::native_diagnostic::{DiagnosticStage, NativeDiagnostic};

use crate::worker::exec_snapshot::ProtectedRoots;

#[cfg(target_os = "linux")]
#[path = "exec_sandbox/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "exec_sandbox/macos.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "exec_sandbox/unsupported.rs"]
mod platform;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "exec_sandbox/temp_unix.rs"]
mod temp;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use temp::PerExecTemp;

#[cfg(test)]
#[path = "exec_sandbox/tests.rs"]
mod tests;

/// The probe result, computed once for the process lifetime. Blocking on its
/// first call: on macOS it runs the full profile once.
pub(crate) fn sandbox_support() -> ExecSandboxSupport {
    static SUPPORT: OnceLock<ExecSandboxSupport> = OnceLock::new();
    *SUPPORT.get_or_init(probe_support)
}

/// Probe the identity and the platform mechanism.
pub(crate) fn probe_support() -> ExecSandboxSupport {
    if execution_identity().is_privileged() {
        return ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity);
    }
    platform::probe()
}

/// The identity commands of this process run as.
#[cfg(unix)]
pub(crate) fn execution_identity() -> ExecIdentity {
    ExecIdentity {
        elevated: false,
        system: false,
        root: unsafe { libc::geteuid() } == 0,
    }
}

/// The identity commands of this process run as. An unreadable token is
/// reported as the most privileged identity it could be.
#[cfg(windows)]
pub(crate) fn execution_identity() -> ExecIdentity {
    use windows::Win32::System::Threading::GetCurrentProcess;
    match crate::daemon::local_access_control_transport::windows_token_identity(unsafe {
        GetCurrentProcess()
    }) {
        Ok((sid, elevated)) => ExecIdentity {
            elevated,
            system: sid == "S-1-5-18",
            root: false,
        },
        Err(_) => ExecIdentity {
            elevated: true,
            system: true,
            root: false,
        },
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn execution_identity() -> ExecIdentity {
    ExecIdentity {
        elevated: false,
        system: false,
        root: false,
    }
}

/// This product's directories whose content a sandboxed command must not
/// read where the platform can express it (macOS): the worker's own data and
/// log directories (`product`, which with an explicit configuration file is
/// that file's directory) and every default configuration and data location.
/// Credentials such as `config.toml` tokens live there.
pub(crate) fn read_protected_directories(product: Vec<PathBuf>) -> Vec<PathBuf> {
    use desk_utils::host_data_paths::{HostDataPaths, HostDataScope};
    let mut directories = product;
    for scope in [
        HostDataScope::Machine,
        HostDataScope::System,
        HostDataScope::User,
    ] {
        if let Ok(paths) = HostDataPaths::resolve(scope, None) {
            directories.extend(paths.config_file().parent().map(Path::to_path_buf));
            directories.push(paths.data_root().to_path_buf());
        }
    }
    directories
}

/// The run-after hint of a failed sandboxed command whose output looks like
/// the platform refused to run a program inside the sandbox (macOS: a setuid
/// or restricted program exits 126 with a permission error; Linux: a setuid
/// program such as `sudo` fails under `no_new_privs`).
pub(crate) fn unavailable_program_hint(
    exit_code: Option<i32>,
    stderr: &str,
) -> Option<NativeDiagnostic> {
    platform::refused_program(exit_code, stderr).then(|| {
        NativeDiagnostic::new(
            DiagnosticStage::Environment,
            "run sandboxed command",
            "exec_sandbox",
            None,
            "A program this command runs is probably unavailable inside the command sandbox; \
             request the command again with sandbox=false if it needs that program",
        )
    })
}

/// Why a sandboxed command was not started.
#[derive(Debug)]
pub(crate) enum SandboxError {
    Unsupported(ExecSandboxUnsupportedReason),
    /// A write root could not be opened, no longer has the identity it was
    /// granted for, or could not be resolved from its handle.
    RootUnavailable(String),
    /// A write root is on the forbidden list.
    ForbiddenRoot(PathBuf),
    /// The execution's temporary directory could not be created.
    TempUnavailable(std::io::Error),
    /// The platform restriction could not be built or applied.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Restriction(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => {
                write!(f, "this device cannot sandbox commands: {reason:?}")
            }
            Self::RootUnavailable(cause) => write!(f, "a write root is unavailable: {cause}"),
            Self::ForbiddenRoot(path) => write!(
                f,
                "{} cannot be a writable directory of a sandboxed command",
                path.display()
            ),
            Self::TempUnavailable(cause) => {
                write!(
                    f,
                    "the command's temporary directory is unavailable: {cause}"
                )
            }
            Self::Restriction(message) => write!(f, "the sandbox could not be applied: {message}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// A write root resolved from its opened handle.
pub(crate) struct SandboxRoot {
    /// Landlock rules attach to the handle; Seatbelt matches the path.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub handle: File,
    /// The real path of the handle, never re-resolved from a string.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub path: PathBuf,
}

/// Write roots that passed the forbidden-root check.
pub(crate) struct SandboxRoots(Vec<SandboxRoot>);

/// How an executor starts a sealed plan.
pub(crate) enum ExecLaunch {
    /// No write restriction: an unrestricted plan.
    Unrestricted,
    /// A sandboxed pipe execution with its restriction built.
    Sandboxed(PreparedSandbox),
    /// A sandboxed PTY execution: the roots are checked, and the restriction
    /// is built once the terminal device exists.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    SandboxedTerminal {
        sandbox: Arc<ExecSandbox>,
        roots: SandboxRoots,
    },
    /// The sandbox of a sandboxed plan could not be established; the command
    /// must not start.
    Refused(SandboxError),
}

impl ExecLaunch {
    pub(crate) fn is_sandboxed(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }
}

/// Sandbox facilities of one worker instance.
pub(crate) struct ExecSandbox {
    support: ExecSandboxSupport,
    protected: ProtectedRoots,
    /// The worker's private data directory; per-execution temporary
    /// directories are created below it.
    private_dir: PathBuf,
    /// This product's configuration and data directories, whose content a
    /// sandboxed command must not read where the platform can express it
    /// (macOS).
    read_protected: Vec<PathBuf>,
}

impl ExecSandbox {
    /// Facilities of a worker with the private data directory `private_dir`.
    /// A worker without one cannot place per-execution temporary
    /// directories and refuses sandboxed commands. Probes the platform on
    /// first use and removes temporary directories left behind by workers
    /// that no longer run. Blocking.
    pub(crate) fn start(
        private_dir: Option<PathBuf>,
        protected: ProtectedRoots,
        read_protected: Vec<PathBuf>,
    ) -> Self {
        match private_dir {
            Some(private_dir) => {
                Self::with_support(sandbox_support(), private_dir, protected, read_protected)
            }
            None => Self::with_support(
                ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::NotImplemented),
                PathBuf::new(),
                protected,
                read_protected,
            ),
        }
    }

    fn with_support(
        support: ExecSandboxSupport,
        private_dir: PathBuf,
        protected: ProtectedRoots,
        read_protected: Vec<PathBuf>,
    ) -> Self {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if support == ExecSandboxSupport::Supported {
            PerExecTemp::sweep_stale(&private_dir);
        }
        Self {
            support,
            protected,
            private_dir,
            read_protected,
        }
    }

    #[cfg(test)]
    pub(crate) fn support(&self) -> ExecSandboxSupport {
        self.support
    }

    /// The launch of a sandboxed plan: open every write root (the plan's
    /// snapshot roots) without following its final component, checking a
    /// granted directory's identity; refuse forbidden roots; build the
    /// restriction of a pipe execution. Independent of the recovery
    /// binding: any failure refuses the command. The opened directories are
    /// returned for the snapshot, which covers exactly these handles.
    /// Blocking.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn launch(
        self: &Arc<Self>,
        plan: &ExecPlan,
    ) -> Result<
        (
            ExecLaunch,
            Vec<crate::worker::agent::file_reference_store::command_changes::CommandDirectory>,
        ),
        SandboxError,
    > {
        use crate::worker::agent::file_reference_store::command_changes::open_snapshot_root;
        self.supported()?;
        let directories = plan
            .snapshot_roots
            .iter()
            .map(|root| {
                open_snapshot_root(root)
                    .map_err(|error| SandboxError::RootUnavailable(error.message))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let handles = directories
            .iter()
            .map(|directory| directory.handle.try_clone())
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| SandboxError::RootUnavailable(error.to_string()))?;
        let roots = self.check_roots(handles)?;
        let launch = if plan.io_mode.is_pty() {
            ExecLaunch::SandboxedTerminal {
                sandbox: Arc::clone(self),
                roots,
            }
        } else {
            ExecLaunch::Sandboxed(self.prepare(&roots, None)?)
        };
        Ok((launch, directories))
    }

    fn supported(&self) -> Result<(), SandboxError> {
        match self.support {
            ExecSandboxSupport::Supported => Ok(()),
            ExecSandboxSupport::Unsupported(reason) => Err(SandboxError::Unsupported(reason)),
        }
    }

    /// Resolve the opened write roots from their handles and refuse
    /// forbidden ones.
    pub(crate) fn check_roots(&self, handles: Vec<File>) -> Result<SandboxRoots, SandboxError> {
        self.supported()?;
        let mut roots = Vec::with_capacity(handles.len());
        for handle in handles {
            let path = platform::real_path(&handle)
                .map_err(|error| SandboxError::RootUnavailable(error.to_string()))?;
            if self.protected.refuses_write_root(&path) {
                return Err(SandboxError::ForbiddenRoot(path));
            }
            roots.push(SandboxRoot { handle, path });
        }
        Ok(SandboxRoots(roots))
    }

    /// Build the restriction of one execution over checked roots.
    /// `pty_slave` is the terminal device of a PTY execution, which stays
    /// writable by path. Blocking.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn prepare(
        &self,
        roots: &SandboxRoots,
        pty_slave: Option<&Path>,
    ) -> Result<PreparedSandbox, SandboxError> {
        let temp = PerExecTemp::create(&self.private_dir).map_err(SandboxError::TempUnavailable)?;
        let platform = platform::Prepared::build(&roots.0, &temp, pty_slave, &self.read_protected)?;
        Ok(PreparedSandbox { platform, temp })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn prepare(
        &self,
        _roots: &SandboxRoots,
        _pty_slave: Option<&Path>,
    ) -> Result<PreparedSandbox, SandboxError> {
        let _ = (&self.private_dir, &self.read_protected);
        Err(SandboxError::Unsupported(
            ExecSandboxUnsupportedReason::NotImplemented,
        ))
    }

    /// Check `roots` and build the restriction in one step.
    #[cfg(test)]
    pub(crate) fn prepare_handles(
        &self,
        roots: &[&File],
        pty_slave: Option<&Path>,
    ) -> Result<PreparedSandbox, SandboxError> {
        let handles = roots
            .iter()
            .map(|handle| handle.try_clone())
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| SandboxError::RootUnavailable(error.to_string()))?;
        let roots = self.check_roots(handles)?;
        self.prepare(&roots, pty_slave)
    }
}

/// The restriction of one execution. It must stay alive until the spawn
/// returned; dropping it afterwards removes the execution's temporary
/// directory, so the caller keeps it until the process tree was reclaimed.
pub(crate) struct PreparedSandbox {
    // Dropped in declaration order: the restriction before the directory.
    platform: platform::Prepared,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    temp: PerExecTemp,
}

impl PreparedSandbox {
    /// The execution's temporary directory.
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn temp_dir(&self) -> &Path {
        self.temp.path()
    }

    /// The variables that point temporary files at the execution's
    /// temporary directory.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn environment(&self) -> [(&'static str, &Path); 3] {
        let path = self.temp.path();
        [("TMPDIR", path), ("TEMP", path), ("TMP", path)]
    }

    /// The pipe executor's command for `program` with `args`: the sandbox and
    /// the temporary-directory variables applied, everything else (stdio,
    /// working directory, containment) left to the caller. On Linux the
    /// restriction is applied in the forked child, so `self` must outlive the
    /// spawn.
    pub(crate) fn pipe_command(&self, program: &str, args: &[String]) -> tokio::process::Command {
        #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(unused_mut))]
        let mut command = self.platform.pipe_command(program, args);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        command.envs(self.environment());
        command
    }

    /// Run `spawn` on a dedicated thread that first restricts itself, so
    /// that only the process it spawns inherits the restriction; the thread
    /// then exits. For executors without a pre-exec hook (portable-pty). The
    /// closure must do nothing but spawn: every file it opens is restricted.
    #[cfg(target_os = "linux")]
    pub(crate) fn spawn_restricted<T: Send>(
        &self,
        spawn: impl FnOnce() -> T + Send,
    ) -> std::io::Result<T> {
        self.platform.spawn_restricted(spawn)
    }
}

/// The argv of a command, as an owned list; shared by the platform builders.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn command_line(program: &str, args: &[String]) -> Vec<OsString> {
    std::iter::once(OsString::from(program))
        .chain(args.iter().map(OsString::from))
        .collect()
}
