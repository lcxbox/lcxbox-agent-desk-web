//! Bounded same-display GDI fallback for one-shot DXGI captures.
//!
//! Two DXGI failures retry the same display once with GDI:
//! - `DXGI_ERROR_DEVICE_REMOVED`: the device went away;
//! - `E_INVALIDARG`: most often `DuplicateOutput` refusing a second
//!   duplication of an output this process already duplicates (the media
//!   pipeline streaming that display). Its other sources (a mode change, a
//!   desktop-switch call) are equally safe to retry.
//!
//! `E_ACCESSDENIED` (secure desktop) and every other error do not fall back.
//!
//! The configured backend may be DXGI or WGC: the capture factory replaces an
//! unavailable WGC with DXGI for the instance, so a WGC configuration can meet
//! the same DXGI errors. A configured GDI capture never falls back.

use desk_capture_engine::{
    error::CaptureError,
    model::image_capture::{ImageCaptureType, ImageCaptureTypeHelper, ImageInfo},
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use windows::Win32::{Foundation::E_INVALIDARG, Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED};

pub(super) fn capture(
    settings: &DeskSettings,
) -> Result<Box<dyn ImageInfo + Send + Sync>, CaptureError> {
    with_device_fallback(settings, super::capture_display_once)
}

/// Why a DXGI error is retried with GDI, or `None` when it is not.
fn fallback_reason(error: &CaptureError) -> Option<&'static str> {
    match error {
        CaptureError::WindowsResultError(_, error) if error.code() == DXGI_ERROR_DEVICE_REMOVED => {
            Some("DXGI device removed")
        }
        CaptureError::WindowsResultError(_, error) if error.code() == E_INVALIDARG => {
            Some("DXGI E_INVALIDARG (likely the display is already duplicated by this process)")
        }
        _ => None,
    }
}

fn with_device_fallback<T>(
    settings: &DeskSettings,
    mut capture: impl FnMut(&DeskSettings) -> Result<T, CaptureError>,
) -> Result<T, CaptureError> {
    match capture(settings) {
        Err(error)
            if matches!(
                settings.get_image_capture_type(),
                Ok(ImageCaptureType::DXGI | ImageCaptureType::WGC)
            ) && fallback_reason(&error).is_some() =>
        {
            // Preserve the exact resolved target. GDI's factory rejects missing
            // outputs; neither this retry nor the factory selects another one.
            let mut fallback = settings.clone();
            fallback.image_capture = Some("GDI".into());
            log::warn!(
                "One-shot screenshot: {} ({error}); retrying the same display with GDI",
                fallback_reason(&error).unwrap_or_default()
            );
            capture(&fallback)
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_ACCESSDENIED;

    /// Regression (on device 2026-11-21): with the DXGI backend, a screenshot
    /// of the display the remote page was streaming failed with E_INVALIDARG
    /// every time, because the media pipeline already duplicated that output.
    /// A WGC configuration is covered too: the factory may have replaced an
    /// unavailable WGC with DXGI for this capture.
    #[test]
    fn device_fallback_is_once_and_preserves_the_target_and_saved_settings() {
        for (backend, code) in [
            (None, DXGI_ERROR_DEVICE_REMOVED),
            (None, E_INVALIDARG),
            (Some("WGC"), DXGI_ERROR_DEVICE_REMOVED),
            (Some("WGC"), E_INVALIDARG),
        ] {
            let settings = DeskSettings {
                video_device_name: r"\\.\DISPLAY2".into(),
                image_capture: backend.map(Into::into),
                ..Default::default()
            };
            let mut calls = 0;
            let result: Result<(), CaptureError> = with_device_fallback(&settings, |current| {
                calls += 1;
                assert_eq!(current.video_device_name, settings.video_device_name);
                assert_eq!(
                    current.image_capture.as_deref(),
                    if calls == 1 { backend } else { Some("GDI") }
                );
                Err(windows::core::Error::from_hresult(code).into())
            });
            assert!(result.is_err());
            assert_eq!(calls, 2, "{backend:?} {code:?} must retry once with GDI");
            assert_eq!(settings.image_capture.as_deref(), backend);
        }
    }

    #[test]
    fn access_denied_and_non_dxgi_errors_do_not_fall_back() {
        for (backend, code) in [
            ("DXGI", E_ACCESSDENIED),
            ("WGC", E_ACCESSDENIED),
            ("GDI", DXGI_ERROR_DEVICE_REMOVED),
            ("GDI", E_INVALIDARG),
        ] {
            let settings = DeskSettings {
                image_capture: Some(backend.into()),
                ..Default::default()
            };
            let mut calls = 0;
            let result: Result<(), CaptureError> = with_device_fallback(&settings, |_| {
                calls += 1;
                Err(windows::core::Error::from_hresult(code).into())
            });
            assert!(result.is_err());
            assert_eq!(calls, 1, "{backend} {code:?} must not fall back");
        }
    }
}
