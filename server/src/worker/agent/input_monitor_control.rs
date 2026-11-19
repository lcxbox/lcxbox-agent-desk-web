//! Runs the Computer Use input-ownership monitor only while the local policy
//! needs it.
//!
//! The monitor is a global keyboard and mouse hook on Windows and an event tap
//! on macOS: it sees every key press. It exists so that interactive Computer
//! Use writers yield to a person using the desktop, and so that generic UI and
//! raw input can be offered at all. A device with no interactive Computer Use
//! capability turned on, which is the default, never installs it.
//!
//! One task owns the monitor. Policy changes only update its target, so starts
//! and stops never interleave, and both run on the blocking pool: a Windows
//! start waits up to five seconds for the hook thread and a stop joins it,
//! neither of which may hold up the policy acknowledgement.

use std::time::Duration;

use log::{info, warn};
use tokio::sync::{oneshot, watch};

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// Starts the platform monitor. A started monitor stops when dropped.
pub(crate) trait InputMonitorStarter: Send + Sync + 'static {
    type Monitor: Send + 'static;
    fn start(&self) -> Result<Self::Monitor, String>;
}

/// Handle to the control task. Dropping it without `shutdown` also stops the
/// monitor, without waiting.
pub(crate) struct InputMonitorControl {
    target: watch::Sender<bool>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl InputMonitorControl {
    /// Start controlling the monitor; `wanted` is the current policy's need.
    pub(crate) fn spawn<S: InputMonitorStarter>(starter: S, wanted: bool) -> Self {
        let (target, target_rx) = watch::channel(wanted);
        let (stop, stop_rx) = oneshot::channel();
        let task = tokio::spawn(run(starter, target_rx, stop_rx));
        Self { target, stop, task }
    }

    /// A sender the policy applier uses to update the target.
    pub(crate) fn target(&self) -> watch::Sender<bool> {
        self.target.clone()
    }

    /// Stop the monitor and wait until it is gone, so a replacement worker in
    /// the same process can install its own. Clones of the target may still be
    /// alive, so this uses its own signal rather than waiting for them.
    pub(crate) async fn shutdown(self) {
        let Self { stop, task, .. } = self;
        let _ = stop.send(());
        let _ = task.await;
    }
}

async fn run<S: InputMonitorStarter>(
    starter: S,
    mut target: watch::Receiver<bool>,
    mut stop: oneshot::Receiver<()>,
) {
    let starter = std::sync::Arc::new(starter);
    let mut monitor: Option<S::Monitor> = None;
    let mut retry = RETRY_INITIAL;
    loop {
        let wanted = *target.borrow_and_update();
        if wanted && monitor.is_none() {
            let starting = std::sync::Arc::clone(&starter);
            match tokio::task::spawn_blocking(move || starting.start()).await {
                Ok(Ok(started)) => {
                    info!("Computer Use input ownership monitor started");
                    monitor = Some(started);
                    retry = RETRY_INITIAL;
                }
                Ok(Err(error)) => {
                    warn!(
                        "Computer Use input ownership monitor is unavailable: {error}; retrying in {retry:?}"
                    );
                }
                Err(error) => {
                    warn!("Computer Use input ownership monitor start failed: {error}");
                }
            }
        } else if !wanted && let Some(stopping) = monitor.take() {
            let _ = tokio::task::spawn_blocking(move || drop(stopping)).await;
            info!("Computer Use input ownership monitor stopped");
        }

        let needs_retry = *target.borrow() && monitor.is_none();
        tokio::select! {
            // A sent stop or a dropped handle both end the task.
            _ = &mut stop => break,
            changed = target.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            () = tokio::time::sleep(retry), if needs_retry => {
                retry = (retry * 2).min(RETRY_MAX);
            }
        }
    }
    if let Some(stopping) = monitor.take() {
        let _ = tokio::task::spawn_blocking(move || drop(stopping)).await;
        info!("Computer Use input ownership monitor stopped");
    }
}

/// The real monitor of this platform.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct PlatformInputMonitor {
    broker: std::sync::Arc<super::computer_use_broker::ComputerUseBroker>,
}

#[cfg(any(windows, target_os = "macos"))]
impl PlatformInputMonitor {
    pub(crate) fn new(
        broker: &std::sync::Arc<super::computer_use_broker::ComputerUseBroker>,
    ) -> Self {
        Self {
            broker: std::sync::Arc::clone(broker),
        }
    }
}

#[cfg(windows)]
impl InputMonitorStarter for PlatformInputMonitor {
    type Monitor = super::windows_input_ownership::WindowsInputOwnershipMonitor;
    fn start(&self) -> Result<Self::Monitor, String> {
        super::windows_input_ownership::WindowsInputOwnershipMonitor::start(&self.broker)
    }
}

#[cfg(target_os = "macos")]
impl InputMonitorStarter for PlatformInputMonitor {
    type Monitor = super::macos_input_ownership::MacosInputOwnershipMonitor;
    fn start(&self) -> Result<Self::Monitor, String> {
        super::macos_input_ownership::MacosInputOwnershipMonitor::start(&self.broker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// Counts live monitors; fails the first `failures` starts.
    #[derive(Clone, Default)]
    struct FakeStarter {
        live: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
        failures: usize,
    }

    struct FakeMonitor(Arc<AtomicUsize>);

    impl Drop for FakeMonitor {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl InputMonitorStarter for FakeStarter {
        type Monitor = FakeMonitor;
        fn start(&self) -> Result<FakeMonitor, String> {
            let attempt = self.starts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.failures {
                return Err("not yet".into());
            }
            assert_eq!(
                self.live.fetch_add(1, Ordering::SeqCst),
                0,
                "one monitor at a time"
            );
            Ok(FakeMonitor(Arc::clone(&self.live)))
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn the_monitor_follows_the_policy_and_is_gone_after_shutdown() {
        let starter = FakeStarter::default();
        let live = Arc::clone(&starter.live);
        let control = InputMonitorControl::spawn(starter, false);
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 0);

        let target = control.target();
        target.send(true).unwrap();
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 1);

        // Rapid toggles end in the state last asked for.
        for wanted in [false, true, false, true, false] {
            target.send(wanted).unwrap();
        }
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 0);

        target.send(true).unwrap();
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 1);
        // The policy applier still holds `target`; shutdown must not wait for it.
        tokio::time::timeout(Duration::from_secs(10), control.shutdown())
            .await
            .expect("shutdown does not wait for target clones");
        assert_eq!(live.load(Ordering::SeqCst), 0);
        drop(target);
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_monitor() {
        let starter = FakeStarter::default();
        let live = Arc::clone(&starter.live);
        let control = InputMonitorControl::spawn(starter, true);
        let target = control.target();
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 1);
        drop(control);
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 0);
        drop(target);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_start_is_retried_while_the_policy_still_wants_it() {
        let starter = FakeStarter {
            failures: 2,
            ..FakeStarter::default()
        };
        let live = Arc::clone(&starter.live);
        let starts = Arc::clone(&starter.starts);
        let control = InputMonitorControl::spawn(starter, true);
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            settle().await;
        }
        assert_eq!(live.load(Ordering::SeqCst), 1);
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        control.shutdown().await;
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}
