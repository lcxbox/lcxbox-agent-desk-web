//! Source labels for Windows input synthesized by this product.
//!
//! These values are classification markers, not credentials. A low-level hook
//! can use them to avoid treating the product's own browser or AI injection as
//! external user input. Every missing, unknown, or stripped marker remains
//! external and must conservatively preempt an AI writer lease.

use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_KEYBOARD, INPUT_MOUSE};

pub const AI_INPUT_MARKER: usize = 0x4c58_4101;
pub const BROWSER_INPUT_MARKER: usize = 0x4c58_4201;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputSource {
    Ai,
    Browser,
    External,
}

#[must_use]
pub const fn classify_input(extra_info: usize, injected: bool) -> InputSource {
    match (extra_info, injected) {
        (AI_INPUT_MARKER, true) => InputSource::Ai,
        (BROWSER_INPUT_MARKER, true) => InputSource::Browser,
        _ => InputSource::External,
    }
}

/// Mark an input produced by the existing remote-controller/browser path.
///
/// `INPUT` is a tagged union. Callers must set `r#type` before invoking this
/// helper; unsupported input kinds are left unmarked and therefore fail closed
/// as external input in the ownership monitor.
pub fn mark_browser_input(input: &mut INPUT) {
    match input.r#type {
        INPUT_KEYBOARD => input.Anonymous.ki.dwExtraInfo = BROWSER_INPUT_MARKER,
        INPUT_MOUSE => input.Anonymous.mi.dwExtraInfo = BROWSER_INPUT_MARKER,
        _ => {}
    }
}

/// Mark one input produced by the AI Agent raw-input fallback. This is
/// intentionally distinct from browser/controller input: browser input must
/// preempt an AI writer lease, while the current AI step must not preempt
/// itself. Unknown or stripped markers still classify as external.
pub fn mark_ai_input(input: &mut INPUT) {
    match input.r#type {
        INPUT_KEYBOARD => input.Anonymous.ki.dwExtraInfo = AI_INPUT_MARKER,
        INPUT_MOUSE => input.Anonymous.mi.dwExtraInfo = AI_INPUT_MARKER,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_product_markers_are_internal() {
        assert_eq!(classify_input(AI_INPUT_MARKER, true), InputSource::Ai);
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER, true),
            InputSource::Browser
        );
        assert_eq!(classify_input(0, false), InputSource::External);
        assert_eq!(classify_input(0, true), InputSource::External);
        assert_eq!(
            classify_input(AI_INPUT_MARKER, false),
            InputSource::External
        );
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER, false),
            InputSource::External
        );
        assert_eq!(
            classify_input(AI_INPUT_MARKER + 1, true),
            InputSource::External
        );
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER + 1, true),
            InputSource::External
        );
    }

    #[test]
    fn browser_marker_is_written_to_keyboard_and_mouse_inputs() {
        let mut keyboard = INPUT::default();
        keyboard.r#type = INPUT_KEYBOARD;
        mark_browser_input(&mut keyboard);
        assert_eq!(
            unsafe { keyboard.Anonymous.ki.dwExtraInfo },
            BROWSER_INPUT_MARKER
        );

        let mut mouse = INPUT::default();
        mouse.r#type = INPUT_MOUSE;
        mark_browser_input(&mut mouse);
        assert_eq!(
            unsafe { mouse.Anonymous.mi.dwExtraInfo },
            BROWSER_INPUT_MARKER
        );
    }

    #[test]
    fn ai_marker_is_written_to_keyboard_and_mouse_inputs() {
        let mut keyboard = INPUT::default();
        keyboard.r#type = INPUT_KEYBOARD;
        mark_ai_input(&mut keyboard);
        assert_eq!(
            unsafe { keyboard.Anonymous.ki.dwExtraInfo },
            AI_INPUT_MARKER
        );

        let mut mouse = INPUT::default();
        mouse.r#type = INPUT_MOUSE;
        mark_ai_input(&mut mouse);
        assert_eq!(unsafe { mouse.Anonymous.mi.dwExtraInfo }, AI_INPUT_MARKER);
    }
}

/// Which input desktops this process injects into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputDesktopPolicy {
    /// Follow the input desktop to any desktop this process can open with
    /// full access (the session user's Default, Screen-saver, ...).
    AnyOpenable,
    /// Inject only while the input desktop is `Winlogon` (the restricted
    /// SYSTEM worker of the secure desktop).
    SecureDesktopOnly,
}

static INPUT_DESKTOP_POLICY: std::sync::OnceLock<InputDesktopPolicy> = std::sync::OnceLock::new();

/// Set once per process, before the first injection. Without a policy the
/// injecting thread keeps the desktop it started on.
pub fn set_input_desktop_policy(policy: InputDesktopPolicy) {
    let _ = INPUT_DESKTOP_POLICY.set(policy);
}

thread_local! {
    static LAST_DESKTOP_CHECK: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

const DESKTOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Attach the calling input thread to the current input desktop when the
/// process policy allows it, so remote input reaches a desktop the worker
/// was not started on (a screen saver, the secure desktop). Checked at most
/// every [`DESKTOP_CHECK_INTERVAL`]. Returns `false` when this process must
/// not inject into the current input desktop.
pub fn follow_input_desktop() -> bool {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
        GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;

    let Some(policy) = INPUT_DESKTOP_POLICY.get().copied() else {
        return true;
    };
    let now = std::time::Instant::now();
    let due = LAST_DESKTOP_CHECK.with(|last| {
        let due = last
            .get()
            .is_none_or(|checked| now.duration_since(checked) >= DESKTOP_CHECK_INTERVAL);
        if due {
            last.set(Some(now));
        }
        due
    });

    fn name_of(desktop: HDESK) -> Option<String> {
        let mut buffer = [0u16; 256];
        let mut needed = 0u32;
        unsafe {
            GetUserObjectInformationW(
                windows::Win32::Foundation::HANDLE(desktop.0),
                UOI_NAME,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * 2) as u32,
                Some(&mut needed),
            )
        }
        .ok()?;
        let len = (needed as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buffer[..len]))
    }

    let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
    let thread_name = thread_desktop.and_then(name_of);
    if !due {
        return match policy {
            InputDesktopPolicy::AnyOpenable => true,
            InputDesktopPolicy::SecureDesktopOnly => thread_name.as_deref() == Some("Winlogon"),
        };
    }
    // GENERIC_ALL: the access injection and capture need.
    let Ok(input) = (unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x1000_0000),
        )
    }) else {
        // A desktop this process cannot open is not one it may serve.
        return policy == InputDesktopPolicy::AnyOpenable && thread_name.is_some();
    };
    let input_name = name_of(input);
    let allowed = match policy {
        InputDesktopPolicy::AnyOpenable => true,
        InputDesktopPolicy::SecureDesktopOnly => input_name.as_deref() == Some("Winlogon"),
    };
    if allowed && input_name.is_some() && input_name != thread_name {
        match unsafe { SetThreadDesktop(input) } {
            Ok(()) => log::info!(
                "input thread follows the input desktop {:?} -> {:?}",
                thread_name,
                input_name
            ),
            Err(error) => log::warn!(
                "input thread could not move to the input desktop {:?}: {error}",
                input_name
            ),
        }
    }
    let _ = unsafe { CloseDesktop(input) };
    allowed
}
