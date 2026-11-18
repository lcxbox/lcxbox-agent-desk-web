//! Worker-side input-desktop monitor.
//!
//! The daemon's `session_monitor` calls `OpenInputDesktop` from inside the
//! SYSTEM service window station (`Service-0x0-3e7$`), so it cannot observe a
//! user-session desktop switch — UAC's transition Default → Winlogon is
//! invisible from session 0. The worker, however, runs in the user session's
//! `WinSta0` and can observe the switch via the same API.
//!
//! The probe opens the input desktop with the access capture and input need.
//! A session-user worker can open `Default` and `Screen-saver` but gets
//! `ERROR_ACCESS_DENIED` for the secure `Winlogon` desktop; that is reported
//! as `restricted` (named "Winlogon"). The SYSTEM worker can open every
//! desktop and reports its real name.
//!
//! Reports are absolute and level-triggered: the current desktop is sent when
//! it changes, when the daemon changes this worker's route (it asks for a
//! fresh report), and at least every [`REPEAT_INTERVAL`]. The daemon routes
//! from the latest report of each worker, so a lost or early report is
//! corrected by the next one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Poll interval for the input-desktop watcher.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The current desktop is re-sent at least this often.
const REPEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Name reported when the input desktop cannot be opened: the secure
/// desktop UAC, the lock screen and the sign-in screen switch to.
pub const RESTRICTED_DESKTOP_NAME: &str = "Winlogon";

/// One report of the current input desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopReport {
    pub name: String,
    pub restricted: bool,
    /// System monotonic clock in milliseconds when probed.
    pub observed_tick_ms: u64,
}

/// Spawn the platform monitor on a dedicated thread. Setting `report_now`
/// makes the next poll send the current desktop even if it is unchanged.
pub fn spawn(tx: UnboundedSender<DesktopReport>, report_now: Arc<AtomicBool>) {
    if std::thread::Builder::new()
        .name("worker-desktop-monitor".to_string())
        .spawn(move || run_loop(tx, report_now))
        .is_err()
    {
        log::warn!("[DesktopMonitor] failed to spawn watcher thread");
    }
}

/// Outcome of one `OpenInputDesktop` poll. Distinguishing `Restricted`
/// from `OtherError` matters because UAC produces the former
/// deterministically and we want to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDesktopProbe {
    /// We could open the input desktop with the access capture and input need
    /// and read its name.
    Name(String),
    /// `OpenInputDesktop` failed with ERROR_ACCESS_DENIED — the input
    /// desktop has switched to one this process cannot serve (typically
    /// `Winlogon` during UAC).
    Restricted,
    /// Any other failure (rare). Surfaces the formatted error so a future
    /// occurrence shows up in the worker log.
    OtherError(String),
}

fn run_loop(tx: UnboundedSender<DesktopReport>, report_now: Arc<AtomicBool>) {
    log::info!("[DesktopMonitor] reporting the input desktop");
    let mut state = DesktopMonitorState::default();

    loop {
        if tx.is_closed() {
            return;
        }
        let forced = report_now.swap(false, Ordering::AcqRel);
        if let Some((name, restricted)) =
            state.observe(probe_input_desktop(), Instant::now(), forced)
            && tx
                .send(DesktopReport {
                    name,
                    restricted,
                    observed_tick_ms: monotonic_tick_ms(),
                })
                .is_err()
        {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Default)]
struct DesktopMonitorState {
    last_reported: Option<(String, bool)>,
    last_sent_at: Option<Instant>,
    last_logged_other_err: Option<String>,
}

impl DesktopMonitorState {
    /// The report to send for this poll, if any.
    fn observe(
        &mut self,
        probe: InputDesktopProbe,
        now: Instant,
        forced: bool,
    ) -> Option<(String, bool)> {
        let current = match probe {
            InputDesktopProbe::Name(name) => (name, false),
            InputDesktopProbe::Restricted => (RESTRICTED_DESKTOP_NAME.to_string(), true),
            InputDesktopProbe::OtherError(msg) => {
                // Log only on transitions between distinct errors to avoid
                // one warning per poll when the platform API is unavailable.
                if self.last_logged_other_err.as_deref() != Some(msg.as_str()) {
                    log::warn!("[DesktopMonitor] OpenInputDesktop error: {msg}");
                    self.last_logged_other_err = Some(msg);
                }
                return None;
            }
        };
        self.last_logged_other_err = None;
        let changed = self.last_reported.as_ref() != Some(&current);
        let due = self
            .last_sent_at
            .is_none_or(|sent| now.duration_since(sent) >= REPEAT_INTERVAL);
        if !(changed || due || forced) {
            return None;
        }
        if changed {
            log::info!(
                "[DesktopMonitor] input desktop is '{}'{}",
                current.0,
                if current.1 { " (restricted)" } else { "" }
            );
        }
        self.last_reported = Some(current.clone());
        self.last_sent_at = Some(now);
        Some(current)
    }
}

#[cfg(target_os = "windows")]
fn monotonic_tick_ms() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

#[cfg(not(target_os = "windows"))]
fn monotonic_tick_ms() -> u64 {
    0
}

/// Probe the current input desktop. Returns one of [`InputDesktopProbe`]'s
/// three branches so the caller can react to access-denied (UAC) without
/// pretending nothing happened.
#[cfg(target_os = "windows")]
pub fn probe_input_desktop() -> InputDesktopProbe {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    // Win32 ERROR_ACCESS_DENIED (5) wrapped as an HRESULT facility-Win32
    // value: 0x80070005.
    const E_ACCESSDENIED_HRESULT: i32 = 0x8007_0005u32 as i32;

    unsafe {
        let desktop = match OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            // GENERIC_ALL: the access capture opens the input desktop with.
            // A desktop this process cannot open that way it cannot serve.
            DESKTOP_ACCESS_FLAGS(0x1000_0000),
        ) {
            Ok(d) => d,
            Err(e) => {
                let hr = e.code().0;
                if hr == E_ACCESSDENIED_HRESULT {
                    return InputDesktopProbe::Restricted;
                }
                return InputDesktopProbe::OtherError(format!("OpenInputDesktop: {e}"));
            }
        };

        let mut name_buf = vec![0u16; 256];
        let mut needed = 0u32;

        let handle = HANDLE(desktop.0);
        let result = GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(name_buf.as_mut_ptr() as *mut _),
            (name_buf.len() * 2) as u32,
            Some(&mut needed),
        );

        let _ = CloseDesktop(desktop);

        match result {
            Ok(()) => {
                let len = (needed as usize / 2).saturating_sub(1);
                InputDesktopProbe::Name(String::from_utf16_lossy(&name_buf[..len]))
            }
            Err(e) => InputDesktopProbe::OtherError(format!("GetUserObjectInformationW: {e}")),
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn probe_input_desktop() -> InputDesktopProbe {
    InputDesktopProbe::OtherError("not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// `RESTRICTED_DESKTOP_NAME` is the wire name shipped to the daemon
    /// for the access-denied case. Locked down with a test so a rename
    /// has to update the daemon-side handler in lock-step.
    #[test]
    fn restricted_name_is_winlogon() {
        assert_eq!(RESTRICTED_DESKTOP_NAME, "Winlogon");
    }

    /// When the receiver is dropped, `run_loop` must observe
    /// `tx.is_closed()` and exit.
    #[test]
    fn run_loop_exits_when_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel::<DesktopReport>();
        drop(rx);
        let handle = std::thread::Builder::new()
            .name("desktop-monitor-test".to_string())
            .spawn(move || run_loop(tx, Arc::new(AtomicBool::new(false))))
            .unwrap();
        let start = Instant::now();
        while !handle.is_finished() && start.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            handle.is_finished(),
            "run_loop did not exit after receiver was dropped"
        );
        handle.join().unwrap();
    }

    /// Reports are absolute: the first poll, every change, every
    /// `REPEAT_INTERVAL` and every forced poll send the current desktop,
    /// including the worker's own desktop.
    #[test]
    fn reports_the_current_desktop_on_change_repeat_and_request() {
        let mut state = DesktopMonitorState::default();
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let default = || InputDesktopProbe::Name("Default".to_string());

        assert_eq!(
            state.observe(default(), at(0), false),
            Some(("Default".to_string(), false))
        );
        assert_eq!(state.observe(default(), at(1), false), None);
        assert_eq!(
            state.observe(InputDesktopProbe::Restricted, at(2), false),
            Some(("Winlogon".to_string(), true))
        );
        assert_eq!(
            state.observe(InputDesktopProbe::Restricted, at(3), false),
            None
        );
        assert_eq!(
            state.observe(InputDesktopProbe::Restricted, at(3), true),
            Some(("Winlogon".to_string(), true))
        );
        assert_eq!(
            state.observe(InputDesktopProbe::Restricted, at(8), false),
            Some(("Winlogon".to_string(), true))
        );
        assert_eq!(
            state.observe(default(), at(9), false),
            Some(("Default".to_string(), false))
        );
    }

    #[test]
    fn errors_send_nothing_and_are_logged_once() {
        let mut state = DesktopMonitorState::default();
        let now = Instant::now();
        let error = || InputDesktopProbe::OtherError("failure".into());

        assert_eq!(state.observe(error(), now, true), None);
        assert_eq!(state.observe(error(), now, true), None);
        assert!(state.last_logged_other_err.is_some());
        assert!(
            state
                .observe(InputDesktopProbe::Name("Default".into()), now, false)
                .is_some()
        );
        assert_eq!(state.last_logged_other_err, None);
    }
}
