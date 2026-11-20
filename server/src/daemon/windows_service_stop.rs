//! Stopping the Windows service: what SCM is told while the daemon shuts
//! down, the hard deadline on a stop, and the wait for every process the
//! daemon started (plan `2026-11-20_windows-service-stop-pending-and-uninstall-wait`).
//!
//! SCM tools such as PowerShell's `Stop-Service` give up when a service is
//! neither stopped nor pending after the stop control, so the service
//! reports `StopPending` with a moving checkpoint until it is done. A stop
//! that hangs ends at [`STOP_DEADLINE`]; `Stopped` is only reported once no
//! process of ours is left, so the exe can be replaced or removed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use windows_service::service::{
    ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};

/// Wait hint sent with every `StopPending`; each tick comes well within it.
pub(crate) const STOP_WAIT_HINT: Duration = Duration::from_secs(10);
/// How often the checkpoint moves while the daemon shuts down.
pub(crate) const STOP_TICK: Duration = Duration::from_secs(2);
/// A stop that has not finished by now terminates what is left and reports
/// `Stopped`. Below the uninstall's wait, so an uninstall never times out on
/// a hung daemon.
pub(crate) const STOP_DEADLINE: Duration = Duration::from_secs(40);
/// Time SCM gives the service at system shutdown (preshutdown).
pub(crate) const PRESHUTDOWN_TIMEOUT: Duration = Duration::from_secs(45);
/// After the daemon returned: how long its child processes get to exit, and
/// how long the ones terminated after that get to go.
pub(crate) const CHILD_EXIT_WAIT: Duration = Duration::from_secs(10);
pub(crate) const CHILD_KILL_WAIT: Duration = Duration::from_secs(3);
/// The uninstall's wait for the service to stop.
pub(crate) const UNINSTALL_STOP_WAIT: Duration = Duration::from_secs(60);
pub(crate) const UNINSTALL_POLL: Duration = Duration::from_millis(250);

pub(crate) fn status(
    state: ServiceState,
    checkpoint: u32,
    exit_code: ServiceExitCode,
) -> ServiceStatus {
    let (controls_accepted, wait_hint) = match state {
        ServiceState::Running => (
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::PRESHUTDOWN,
            Duration::ZERO,
        ),
        ServiceState::StopPending => (ServiceControlAccept::empty(), STOP_WAIT_HINT),
        _ => (ServiceControlAccept::empty(), Duration::ZERO),
    };
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    }
}

#[derive(Default)]
struct Progress {
    stopping: bool,
    /// The stop came from SCM (Stop, Shutdown or Preshutdown), not from the
    /// daemon ending on its own.
    requested: bool,
    checkpoint: u32,
    done: bool,
}

/// Every status report after `Running` goes through here, under one lock, so
/// nothing is reported after `Stopped`.
pub(crate) struct StopReporter<F: FnMut(ServiceStatus) + Send> {
    inner: Mutex<(Progress, F)>,
}

impl<F: FnMut(ServiceStatus) + Send> StopReporter<F> {
    pub(crate) fn new(report: F) -> Self {
        Self {
            inner: Mutex::new((Progress::default(), report)),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Progress, &mut F) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (progress, report) = &mut *guard;
        f(progress, report)
    }

    pub(crate) fn running(&self) {
        self.with(|progress, report| {
            if !progress.stopping && !progress.done {
                report(status(ServiceState::Running, 0, ServiceExitCode::Win32(0)));
            }
        });
    }

    /// Report `StopPending`. Returns true only for the call that began the
    /// stop; later calls (Stop after Shutdown, the daemon ending after a
    /// Stop) only record whether SCM asked for it.
    pub(crate) fn begin(&self, requested: bool) -> bool {
        self.with(|progress, report| {
            progress.requested |= requested;
            if progress.stopping || progress.done {
                return false;
            }
            progress.stopping = true;
            progress.checkpoint = 1;
            report(status(
                ServiceState::StopPending,
                1,
                ServiceExitCode::Win32(0),
            ));
            true
        })
    }

    /// Move the checkpoint. False once the service reported `Stopped`.
    pub(crate) fn tick(&self) -> bool {
        self.with(|progress, report| {
            if progress.done {
                return false;
            }
            progress.checkpoint += 1;
            report(status(
                ServiceState::StopPending,
                progress.checkpoint,
                ServiceExitCode::Win32(0),
            ));
            true
        })
    }

    /// Report `Stopped` once. A daemon failure gets a failure exit code (SCM
    /// then restarts the service) only when nobody asked it to stop.
    pub(crate) fn finish(&self, failed: bool) -> bool {
        self.with(|progress, report| {
            if progress.done {
                return false;
            }
            progress.done = true;
            let exit_code = if failed && !progress.requested {
                ServiceExitCode::ServiceSpecific(1)
            } else {
                ServiceExitCode::Win32(0)
            };
            report(status(ServiceState::Stopped, 0, exit_code));
            true
        })
    }
}

/// Keep the checkpoint moving until `Stopped` is reported; at `deadline`
/// (counted in ticks, so tests need no clock) run `on_deadline` instead.
pub(crate) fn tick_until_stopped<F: FnMut(ServiceStatus) + Send>(
    reporter: &StopReporter<F>,
    mut sleep: impl FnMut(Duration),
    on_deadline: impl FnOnce(),
) {
    let ticks = STOP_DEADLINE.as_millis() / STOP_TICK.as_millis();
    for _ in 0..ticks {
        sleep(STOP_TICK);
        if !reporter.tick() {
            return;
        }
    }
    on_deadline();
}

/// One process in a snapshot. `created` is the creation time (FILETIME
/// ticks); 0 when it could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessEntry {
    pub pid: u32,
    pub parent: u32,
    pub created: u64,
}

/// The processes started by the daemon, directly or through a worker,
/// remembered across snapshots: a worker killed during shutdown disappears
/// from later snapshots, but its children still name it as their parent.
#[derive(Default)]
pub(crate) struct ProcessTree {
    root: u32,
    known: HashMap<u32, u64>,
}

impl ProcessTree {
    pub(crate) fn new(root: u32, root_created: u64) -> Self {
        Self {
            root,
            known: HashMap::from([(root, root_created)]),
        }
    }

    /// Add every descendant visible in `snapshot`. A child must have been
    /// created after its parent, so a reused pid never pulls in a stranger.
    pub(crate) fn refresh(&mut self, snapshot: &[ProcessEntry]) {
        loop {
            let mut grew = false;
            for entry in snapshot {
                if entry.created == 0 || self.known.contains_key(&entry.pid) {
                    continue;
                }
                if let Some(&parent_created) = self.known.get(&entry.parent)
                    && entry.created >= parent_created
                {
                    self.known.insert(entry.pid, entry.created);
                    grew = true;
                }
            }
            if !grew {
                return;
            }
        }
    }

    /// Descendants still running in `snapshot` (same pid and creation time).
    pub(crate) fn alive(&self, snapshot: &[ProcessEntry]) -> Vec<u32> {
        snapshot
            .iter()
            .filter(|entry| {
                entry.pid != self.root && self.known.get(&entry.pid) == Some(&entry.created)
            })
            .map(|entry| entry.pid)
            .collect()
    }
}

/// How waiting for a service to stop ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopWait<E> {
    Stopped,
    TimedOut,
    QueryFailed(E),
}

/// Poll until `query` reports `Stopped`, sending the stop again while the
/// service still runs or is starting (it rejects the control while start is
/// pending). `polls` bounds the wait.
pub(crate) fn wait_until_stopped<E>(
    mut query: impl FnMut() -> Result<ServiceState, E>,
    mut stop: impl FnMut(),
    mut sleep: impl FnMut(Duration),
    polls: u32,
) -> StopWait<E> {
    for _ in 0..=polls {
        match query() {
            Ok(ServiceState::Stopped) => return StopWait::Stopped,
            Ok(ServiceState::Running | ServiceState::StartPending) => stop(),
            Ok(_) => {}
            Err(error) => return StopWait::QueryFailed(error),
        }
        sleep(UNINSTALL_POLL);
    }
    StopWait::TimedOut
}

#[cfg(target_os = "windows")]
pub(crate) mod os {
    //! The process snapshot, termination and waits behind the pure parts.
    use super::{ProcessEntry, ProcessTree};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{FILETIME, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        TerminateProcess, WaitForSingleObject,
    };
    use windows::core::Owned;

    fn created(process: HANDLE) -> u64 {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        match unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        {
            Ok(()) => {
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime)
            }
            Err(_) => 0,
        }
    }

    fn created_of(pid: u32) -> u64 {
        match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(process) => created(*unsafe { Owned::new(process) }),
            Err(_) => 0,
        }
    }

    pub(crate) fn snapshot() -> Vec<ProcessEntry> {
        let Ok(handle) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Vec::new();
        };
        let handle = unsafe { Owned::new(handle) };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut raw = Vec::new();
        let mut more = unsafe { Process32FirstW(*handle, &mut entry) }.is_ok();
        while more {
            raw.push((entry.th32ProcessID, entry.th32ParentProcessID));
            more = unsafe { Process32NextW(*handle, &mut entry) }.is_ok();
        }
        raw.into_iter()
            .filter(|&(pid, _)| pid != 0)
            .map(|(pid, parent)| ProcessEntry {
                pid,
                parent,
                created: created_of(pid),
            })
            .collect()
    }

    /// This process as the root of its tree.
    pub(crate) fn own_tree() -> ProcessTree {
        let root = unsafe { GetCurrentProcessId() };
        let mut tree = ProcessTree::new(root, created(unsafe { GetCurrentProcess() }));
        tree.refresh(&snapshot());
        tree
    }

    pub(crate) fn terminate(pid: u32) {
        if let Ok(process) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            let process = unsafe { Owned::new(process) };
            let _ = unsafe { TerminateProcess(*process, 1) };
        }
    }

    /// Wait until no descendant runs, up to `limit`; returns those left.
    fn wait_for_descendants(tree: &mut ProcessTree, limit: Duration) -> Vec<u32> {
        let start = Instant::now();
        loop {
            let processes = snapshot();
            tree.refresh(&processes);
            let alive = tree.alive(&processes);
            if alive.is_empty() || start.elapsed() >= limit {
                return alive;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Let the daemon's child processes exit, then terminate the rest.
    /// Returns how many had to be terminated and how many are still left.
    pub(crate) fn reap_descendants(
        tree: &mut ProcessTree,
        exit_wait: Duration,
        kill_wait: Duration,
    ) -> (usize, usize) {
        let left = wait_for_descendants(tree, exit_wait);
        if left.is_empty() {
            return (0, 0);
        }
        for &pid in &left {
            terminate(pid);
        }
        (left.len(), wait_for_descendants(tree, kill_wait).len())
    }

    /// A handle to wait for a process to exit (the service process during
    /// uninstall); opened before the stop so its pid cannot be reused.
    pub(crate) fn open_for_wait(pid: u32) -> Option<Owned<HANDLE>> {
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .ok()
            .map(|process| unsafe { Owned::new(process) })
    }

    pub(crate) fn wait_for_exit(process: &Owned<HANDLE>, limit: Duration) -> bool {
        let waited = unsafe { WaitForSingleObject(**process, limit.as_millis() as u32) };
        waited == WAIT_OBJECT_0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type StatusLog = Arc<Mutex<Vec<ServiceStatus>>>;

    fn recorder() -> (StatusLog, StopReporter<impl FnMut(ServiceStatus) + Send>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        (
            log,
            StopReporter::new(move |status| sink.lock().unwrap().push(status)),
        )
    }

    fn states(log: &Mutex<Vec<ServiceStatus>>) -> Vec<(ServiceState, u32)> {
        log.lock()
            .unwrap()
            .iter()
            .map(|status| (status.current_state, status.checkpoint))
            .collect()
    }

    /// Regression (on device 2026-11-19): the service stayed `Running` until
    /// shutdown ended, and `Stop-Service` reported "stop failed".
    #[test]
    fn a_stop_reports_pending_with_moving_checkpoints_then_stopped() {
        let (log, reporter) = recorder();
        reporter.running();
        assert!(reporter.begin(true));
        assert!(reporter.tick());
        assert!(reporter.tick());
        assert!(reporter.finish(false));
        assert_eq!(
            states(&log),
            [
                (ServiceState::Running, 0),
                (ServiceState::StopPending, 1),
                (ServiceState::StopPending, 2),
                (ServiceState::StopPending, 3),
                (ServiceState::Stopped, 0),
            ]
        );
        let reports = log.lock().unwrap();
        assert_eq!(reports[1].controls_accepted, ServiceControlAccept::empty());
        assert_eq!(reports[1].wait_hint, STOP_WAIT_HINT);
        assert!(
            reports[0]
                .controls_accepted
                .contains(ServiceControlAccept::PRESHUTDOWN)
        );
    }

    #[test]
    fn nothing_is_reported_after_stopped() {
        let (log, reporter) = recorder();
        assert!(reporter.begin(true));
        assert!(reporter.finish(false));
        assert!(!reporter.tick());
        assert!(!reporter.begin(true));
        assert!(!reporter.finish(true));
        reporter.running();
        assert_eq!(states(&log).len(), 2);
    }

    #[test]
    fn a_second_stop_control_reports_nothing_new() {
        let (log, reporter) = recorder();
        assert!(reporter.begin(true));
        assert!(!reporter.begin(true));
        assert_eq!(states(&log), [(ServiceState::StopPending, 1)]);
    }

    #[test]
    fn a_failure_restarts_the_service_only_when_nobody_asked_it_to_stop() {
        let (log, reporter) = recorder();
        reporter.begin(false);
        reporter.finish(true);
        assert_eq!(
            log.lock().unwrap().last().unwrap().exit_code,
            ServiceExitCode::ServiceSpecific(1)
        );

        // The daemon failed while shutting down after a Stop: no restart.
        let (log, reporter) = recorder();
        reporter.begin(true);
        reporter.begin(false);
        reporter.finish(true);
        assert_eq!(
            log.lock().unwrap().last().unwrap().exit_code,
            ServiceExitCode::Win32(0)
        );
    }

    #[test]
    fn the_ticker_ends_when_stopped_and_fires_the_deadline_otherwise() {
        let (_, reporter) = recorder();
        reporter.begin(true);
        let mut slept = 0;
        let mut fired = false;
        tick_until_stopped(
            &reporter,
            |_| {
                slept += 1;
                if slept == 3 {
                    reporter.finish(false);
                }
            },
            || fired = true,
        );
        assert_eq!(slept, 3);
        assert!(!fired);

        let (log, reporter) = recorder();
        reporter.begin(true);
        let mut fired = false;
        tick_until_stopped(&reporter, |_| {}, || fired = true);
        assert!(fired);
        let ticks = (STOP_DEADLINE.as_millis() / STOP_TICK.as_millis()) as u32;
        assert_eq!(
            states(&log).last(),
            Some(&(ServiceState::StopPending, 1 + ticks))
        );
    }

    const fn entry(pid: u32, parent: u32, created: u64) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent,
            created,
        }
    }

    #[test]
    fn the_tree_holds_children_and_grandchildren_but_not_strangers() {
        let mut tree = ProcessTree::new(10, 100);
        tree.refresh(&[
            entry(10, 1, 100),
            entry(20, 10, 110), // worker
            entry(30, 20, 120), // helper started by the worker
            entry(40, 1, 130),  // unrelated
            entry(50, 10, 90),  // reused parent pid: older than the daemon
        ]);
        let mut alive = tree.alive(&[
            entry(20, 10, 110),
            entry(30, 20, 120),
            entry(40, 1, 130),
            entry(50, 10, 90),
        ]);
        alive.sort();
        assert_eq!(alive, [20, 30]);
    }

    #[test]
    fn children_of_a_killed_worker_are_still_ours() {
        let mut tree = ProcessTree::new(10, 100);
        tree.refresh(&[entry(20, 10, 110)]);
        // The worker is gone; its helper still names it as parent.
        let later = [entry(30, 20, 120)];
        tree.refresh(&later);
        assert_eq!(tree.alive(&later), [30]);
        // A new process reusing the helper's pid is not ours.
        assert!(tree.alive(&[entry(30, 7, 500)]).is_empty());
    }

    #[test]
    fn uninstall_waits_until_stopped_and_resends_stop_while_running() {
        let mut states = vec![
            ServiceState::StartPending,
            ServiceState::Running,
            ServiceState::StopPending,
            ServiceState::Stopped,
        ]
        .into_iter();
        let mut stops = 0;
        let result: StopWait<()> =
            wait_until_stopped(|| Ok(states.next().unwrap()), || stops += 1, |_| {}, 10);
        assert_eq!(result, StopWait::Stopped);
        assert_eq!(stops, 2);
    }

    #[test]
    fn uninstall_wait_is_bounded_and_reports_query_errors() {
        let mut polls = 0;
        let result: StopWait<()> = wait_until_stopped(
            || {
                polls += 1;
                Ok(ServiceState::StopPending)
            },
            || {},
            |_| {},
            5,
        );
        assert_eq!(result, StopWait::TimedOut);
        assert_eq!(polls, 6);

        let result = wait_until_stopped(|| Err("denied"), || {}, |_| {}, 5);
        assert_eq!(result, StopWait::QueryFailed("denied"));
    }

    /// The real snapshot finds a child and the grandchild it started, and
    /// both can be terminated. Only these two are touched: other tests in
    /// this process may have children of their own.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_real_child_and_grandchild_are_found_and_terminated() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .expect("cmd starts");
        let child_pid = child.id();
        let mut ours = Vec::new();
        for _ in 0..50 {
            let snapshot = os::snapshot();
            let mut tree = os::own_tree();
            tree.refresh(&snapshot);
            let alive = tree.alive(&snapshot);
            ours = snapshot
                .iter()
                .filter(|entry| {
                    alive.contains(&entry.pid)
                        && (entry.pid == child_pid || entry.parent == child_pid)
                })
                .map(|entry| entry.pid)
                .collect::<Vec<_>>();
            if ours.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(ours.len(), 2, "child and grandchild found");
        for &pid in &ours {
            os::terminate(pid);
        }
        let _ = child.wait();
        for _ in 0..50 {
            let snapshot = os::snapshot();
            if !snapshot.iter().any(|entry| ours.contains(&entry.pid)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("terminated processes are gone");
    }
}
