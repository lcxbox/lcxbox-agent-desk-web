//! Which input desktops this process may capture.
//!
//! Capture follows the input desktop: it re-attaches its thread whenever it
//! is (re)built. A process that may serve only one desktop (the restricted
//! SYSTEM worker, which serves the secure `Winlogon` desktop) sets that name
//! once, and every (re)build refuses to attach to any other desktop, so it
//! never captures a desktop the session user can open.

use crate::error::CaptureError;

static ALLOWED_DESKTOP: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Capture only while the input desktop is `name`. Set once, before the
/// first capture is built.
pub fn restrict_capture_to_desktop(name: &str) {
    let _ = ALLOWED_DESKTOP.set(name.to_string());
}

/// Refuse to attach to the input desktop unless it is the allowed one.
#[cfg(target_os = "windows")]
pub(crate) fn ensure_input_desktop_allowed() -> Result<(), CaptureError> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    let Some(allowed) = ALLOWED_DESKTOP.get() else {
        return Ok(());
    };
    let name = unsafe {
        let desktop = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001), // DESKTOP_READOBJECTS: the name only.
        )?;
        let mut buffer = [0u16; 256];
        let mut needed = 0u32;
        let read = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * 2) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(desktop);
        read?;
        String::from_utf16_lossy(&buffer[..(needed as usize / 2).saturating_sub(1)])
    };
    if &name == allowed {
        Ok(())
    } else {
        CaptureError::custom_error(
            desk_utils::error::DeskErrorCode::PERMISSION_ERROR,
            &format!("this capture serves only the {allowed} desktop, not {name}"),
        )
    }
}
