use super::worker_manager::WorkerManager;
use log::info;
#[cfg(target_os = "windows")]
use log::warn;
use std::time::Duration;

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSessionRegistration {
    pub session: desk_ipc_protocol::message::SessionKey,
    pub session_id: u32,
    pub display_name: String,
    pub station_name: String,
    pub foreground: bool,
    /// False for the console session at the sign-in screen (after boot or
    /// sign-out): it gets only the restricted Winlogon worker.
    pub has_user: bool,
    /// The session attached to the physical console. Only it can show a
    /// virtual display.
    pub console: bool,
}

/// One schedulable WTS session as enumerated.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedWindowsSession {
    pub session_id: u32,
    /// `None` for the console session at the sign-in screen.
    pub user: Option<String>,
    pub station_name: String,
    pub foreground: bool,
    pub console: bool,
}

/// Assigns each WTS session the generation part of its `SessionKey`.
///
/// A key, and every connection bound to it, survives as long as the session
/// is the same: a session that gains its first user (a sign-in at the
/// sign-in screen) keeps its key, so a viewer who signed in remotely stays
/// connected. A different user in the same session id, or a session that
/// loses its user, gets a new key.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Default)]
pub(crate) struct SessionGenerations {
    tracked: std::collections::HashMap<u32, (Option<String>, u64)>,
    next: u64,
}

#[cfg(any(target_os = "windows", test))]
impl SessionGenerations {
    pub(crate) fn register(
        &mut self,
        observed: Vec<ObservedWindowsSession>,
    ) -> Vec<WindowsSessionRegistration> {
        let mut tracked = std::collections::HashMap::new();
        let mut registrations = Vec::with_capacity(observed.len());
        for observed in observed {
            let previous = self.tracked.get(&observed.session_id);
            let kept = match previous {
                Some((user, generation))
                    if user == &observed.user || (user.is_none() && observed.user.is_some()) =>
                {
                    Some(*generation)
                }
                _ => None,
            };
            if kept.is_some() && matches!(previous, Some((None, _))) && observed.user.is_some() {
                info!(
                    "Windows session {} signed in; keeping its connections",
                    observed.session_id
                );
            }
            let generation = kept.unwrap_or_else(|| {
                self.next = self.next.saturating_add(1);
                self.next
            });
            tracked.insert(observed.session_id, (observed.user.clone(), generation));
            registrations.push(WindowsSessionRegistration {
                session: desk_ipc_protocol::message::SessionKey {
                    platform_session_id: observed.session_id.to_string(),
                    session_generation: generation,
                },
                session_id: observed.session_id,
                display_name: match &observed.user {
                    Some(user) => format!("{user} (session {})", observed.session_id),
                    None => format!("Sign-in screen (session {})", observed.session_id),
                },
                station_name: observed.station_name,
                foreground: observed.foreground,
                has_user: observed.user.is_some(),
                console: observed.console,
            });
        }
        self.tracked = tracked;
        registrations
    }
}

pub async fn run_session_monitor(
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Session monitor starting");

    #[cfg(target_os = "windows")]
    {
        run_windows_session_monitor(worker_mgr).await
    }

    #[cfg(not(target_os = "windows"))]
    {
        info!("Session monitor: no-op on this platform");
        let _ = worker_mgr;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[cfg(target_os = "windows")]
async fn run_windows_session_monitor(
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Windows resident-session worker monitor starting");
    let mut generations = SessionGenerations::default();
    loop {
        let observed = match enumerate_schedulable_windows_sessions() {
            Ok(observed) => observed,
            Err(error) => {
                warn!("Windows session enumeration failed; retaining current workers: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let registrations = generations.register(observed);
        worker_mgr
            .reconcile_windows_resident_workers(registrations)
            .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(target_os = "windows")]
fn enumerate_schedulable_windows_sessions() -> Result<Vec<ObservedWindowsSession>, String> {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSActive, WTSConnected, WTSDomainName, WTSEnumerateSessionsW,
        WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
        WTSWinStationName,
    };
    use windows::core::PWSTR;

    unsafe fn query(
        session_id: u32,
        class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
    ) -> Option<String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        if unsafe {
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                session_id,
                class,
                &mut buffer,
                &mut bytes,
            )
        }
        .is_err()
            || buffer.is_null()
        {
            return None;
        }
        let len = (bytes as usize / 2).saturating_sub(1);
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer.0, len) });
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Some(value)
    }

    unsafe {
        let mut session_info_ptr = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut session_info_ptr,
            &mut count,
        )
        .is_err()
            || session_info_ptr.is_null()
        {
            return Err("WTSEnumerateSessionsW returned no snapshot".to_string());
        }
        let active_console = WTSGetActiveConsoleSessionId();
        let mut result = Vec::new();
        for session in std::slice::from_raw_parts(session_info_ptr, count as usize) {
            if session.SessionId == 0
                || (session.State != WTSActive && session.State != WTSConnected)
            {
                continue;
            }
            let user = match query(session.SessionId, WTSUserName)
                .filter(|name| !name.trim().is_empty())
            {
                Some(user_name) => {
                    let domain = query(session.SessionId, WTSDomainName).unwrap_or_default();
                    Some(if domain.trim().is_empty() {
                        user_name
                    } else {
                        format!(r"{domain}\{user_name}")
                    })
                }
                // Only the console offers a sign-in screen to serve; other
                // user-less sessions (a listener, an RDP session mid-logon)
                // have nothing to show.
                None if session.SessionId == active_console => None,
                None => continue,
            };
            result.push(ObservedWindowsSession {
                session_id: session.SessionId,
                user,
                station_name: query(session.SessionId, WTSWinStationName)
                    .unwrap_or_else(|| "WinSta0".to_string()),
                foreground: session.SessionId == active_console || session.State == WTSActive,
                console: session.SessionId == active_console,
            });
        }
        WTSFreeMemory(session_info_ptr.cast());
        Ok(result)
    }
}

#[cfg(target_os = "windows")]
pub fn get_current_desktop_name() -> Result<String, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    unsafe {
        let desktop = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001),
        )
        .map_err(|e| format!("OpenInputDesktop failed: {}", e))?;

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

        // Always close the desktop handle before returning.
        let _ = CloseDesktop(desktop);

        if result.is_err() {
            return Err("GetUserObjectInformationW failed".to_string());
        }

        let len = (needed as usize / 2).saturating_sub(1);
        let name = String::from_utf16_lossy(&name_buf[..len]);
        Ok(name)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn get_active_session_id() -> u32 {
    0
}

#[cfg(target_os = "windows")]
pub fn get_active_session_id() -> u32 {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSGetActiveConsoleSessionId,
    };

    unsafe {
        let mut session_info_ptr = std::ptr::null_mut();
        let mut count: u32 = 0;

        let result = WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut session_info_ptr,
            &mut count,
        );

        if result.is_err() || session_info_ptr.is_null() {
            log::warn!("WTSEnumerateSessionsW failed, falling back to console session");
            return WTSGetActiveConsoleSessionId();
        }

        let sessions = std::slice::from_raw_parts(session_info_ptr, count as usize);
        let mut active_session_id = None;

        for session in sessions {
            log::debug!(
                "Session enumeration: id={}, state={:?}",
                session.SessionId,
                session.State
            );
            if session.State == WTSActive && session.SessionId != 0 {
                // Session 0 hosts system services, never a user desktop.
                active_session_id = Some(session.SessionId);
                break;
            }
        }

        WTSFreeMemory(session_info_ptr as *mut _);

        match active_session_id {
            Some(id) => {
                log::info!("Found active session via WTSEnumerateSessionsW: {}", id);
                id
            }
            None => {
                let fallback = WTSGetActiveConsoleSessionId();
                log::warn!(
                    "No active session found via enumeration, falling back to console session: {}",
                    fallback
                );
                fallback
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(session_id: u32, user: Option<&str>) -> ObservedWindowsSession {
        ObservedWindowsSession {
            session_id,
            user: user.map(str::to_string),
            station_name: "Console".into(),
            foreground: true,
            console: true,
        }
    }

    fn generation(registrations: &[WindowsSessionRegistration], session_id: u32) -> u64 {
        registrations
            .iter()
            .find(|registration| registration.session_id == session_id)
            .unwrap()
            .session
            .session_generation
    }

    #[test]
    fn signing_in_at_the_sign_in_screen_keeps_the_session_key() {
        let mut generations = SessionGenerations::default();
        let before = generations.register(vec![observed(1, None)]);
        assert!(!before[0].has_user);
        let after = generations.register(vec![observed(1, Some("owner"))]);
        assert!(after[0].has_user);
        assert_eq!(generation(&before, 1), generation(&after, 1));
    }

    #[test]
    fn another_user_or_a_lost_user_gets_a_new_session_key() {
        let mut generations = SessionGenerations::default();
        let first = generations.register(vec![observed(1, Some("a"))]);
        let other = generations.register(vec![observed(1, Some("b"))]);
        assert_ne!(generation(&first, 1), generation(&other, 1));
        let signed_out = generations.register(vec![observed(1, None)]);
        assert_ne!(generation(&other, 1), generation(&signed_out, 1));
        let same = generations.register(vec![observed(1, None)]);
        assert_eq!(generation(&signed_out, 1), generation(&same, 1));
    }

    #[test]
    fn a_session_that_disappears_and_returns_gets_a_new_key() {
        let mut generations = SessionGenerations::default();
        let first = generations.register(vec![observed(2, Some("a"))]);
        generations.register(vec![]);
        let again = generations.register(vec![observed(2, Some("a"))]);
        assert_ne!(generation(&first, 2), generation(&again, 2));
    }
}
