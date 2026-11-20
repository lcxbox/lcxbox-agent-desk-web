//! Named-pipe client for the UMDF driver's JSON control channel.
//!
//! Wire format (4-byte little-endian u32 length header + UTF-8 JSON
//! body) matches the contract defined alongside [`crate::DriverRequest`]
//! / [`crate::DriverResponse`] in `lib.rs`. The driver's C++ side
//! implements the same framing.
//!
//! IO is synchronous: every `set_mode` opens a fresh pipe handle, sends
//! one request, reads one response and closes. The driver-side server
//! enforces single-connection-at-a-time, so callers in the worker stay
//! serialised by the OS rather than by an in-process lock.

use std::io::Read;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, HANDLE, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
    OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

use crate::{
    DRIVER_MAX_MESSAGE_SIZE, DriverRequest, DriverResponse, PIPE_NAME, VirtualDisplayError,
    VirtualDisplayMode,
};

/// Default time to wait for the driver pipe to become available. The
/// driver server publishes the pipe immediately on adapter init, so any
/// wait beyond ~1s indicates the driver is not loaded or has crashed.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_millis(1000);

/// `status_code` range reserved for driver-side protocol / validation
/// errors. NTSTATUS codes from IddCx fall outside this range.
const PROTOCOL_ERROR_LO: i32 = 1;
const PROTOCOL_ERROR_HI: i32 = 1000;

/// The driver's answer to a command it does not know.
const STATUS_UNKNOWN_COMMAND: i32 = 100;

/// Time a plug/unplug may spend getting the pipe. The pipe is
/// single-instance: between two server iterations no instance exists, and
/// a worker's `set_mode` may hold it briefly. Short enough that a failed
/// plug plus the fallback device creation stays within the attach budget.
const MONITOR_COMMAND_BUDGET: Duration = Duration::from_millis(1500);
const MONITOR_COMMAND_RETRY: Duration = Duration::from_millis(20);

/// Encode a `DriverRequest` as a length-prefixed framed message.
///
/// Pure helper exposed for unit testing — the wire transmitter calls
/// this then writes the resulting bytes through `WriteFile`.
fn frame_request(req: &DriverRequest) -> Result<Vec<u8>, VirtualDisplayError> {
    let body = serde_json::to_vec(req)
        .map_err(|e| VirtualDisplayError::PipeIo(format!("serialise request: {e}")))?;
    if body.len() as u64 > DRIVER_MAX_MESSAGE_SIZE as u64 {
        return Err(VirtualDisplayError::PipeIo(format!(
            "request body {} bytes exceeds DRIVER_MAX_MESSAGE_SIZE {}",
            body.len(),
            DRIVER_MAX_MESSAGE_SIZE
        )));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one length-prefixed framed `DriverResponse` from a generic
/// reader. Useful for unit-testing the parsing path without a real pipe.
fn read_framed_response<R: Read>(reader: &mut R) -> Result<DriverResponse, VirtualDisplayError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|e| VirtualDisplayError::PipeIo(format!("read length header: {e}")))?;
    let len = u32::from_le_bytes(len_buf);
    if len == 0 {
        return Err(VirtualDisplayError::PipeIo(
            "driver response has zero-length body".into(),
        ));
    }
    if len > DRIVER_MAX_MESSAGE_SIZE {
        return Err(VirtualDisplayError::PipeIo(format!(
            "driver response length {len} exceeds DRIVER_MAX_MESSAGE_SIZE {DRIVER_MAX_MESSAGE_SIZE}"
        )));
    }
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .map_err(|e| VirtualDisplayError::PipeIo(format!("read body {len} bytes: {e}")))?;
    serde_json::from_slice::<DriverResponse>(&body)
        .map_err(|e| VirtualDisplayError::PipeIo(format!("parse driver response JSON: {e}")))
}

/// Translate a `DriverResponse` into either the applied mode or a
/// `VirtualDisplayError`. Pure helper, no IO.
fn extract_applied_mode(
    requested: VirtualDisplayMode,
    resp: DriverResponse,
) -> Result<VirtualDisplayMode, VirtualDisplayError> {
    if !resp.success {
        let msg = resp.error.unwrap_or_else(|| "no error message".into());
        return Err(
            if (PROTOCOL_ERROR_LO..PROTOCOL_ERROR_HI).contains(&resp.status_code) {
                VirtualDisplayError::InvalidMode(msg)
            } else {
                VirtualDisplayError::DriverFailed(resp.status_code as u32)
            },
        );
    }
    // The driver MAY snap to the closest mode; if it reports the actual
    // applied mode under data.applied_mode, prefer that value.
    if let Some(data) = resp.data
        && let Some(applied) = data.get("applied_mode")
        && let (Some(w), Some(h), Some(hz)) = (
            applied.get("width").and_then(|v| v.as_u64()),
            applied.get("height").and_then(|v| v.as_u64()),
            applied.get("refresh_hz").and_then(|v| v.as_u64()),
        )
    {
        return Ok(VirtualDisplayMode {
            width: w as u32,
            height: h as u32,
            refresh_hz: hz as u32,
        });
    }
    Ok(requested)
}

/// Translate the answer to `plug_monitor` / `unplug_monitor`. A driver
/// that predates the commands answers "unknown command", which the caller
/// treats as "remove the device instead". Pure helper, no IO.
fn extract_monitor_result(resp: DriverResponse) -> Result<(), VirtualDisplayError> {
    if resp.success {
        return Ok(());
    }
    if resp.status_code == STATUS_UNKNOWN_COMMAND {
        return Err(VirtualDisplayError::NotSupported);
    }
    let msg = resp.error.unwrap_or_else(|| "no error message".into());
    Err(
        if (PROTOCOL_ERROR_LO..PROTOCOL_ERROR_HI).contains(&resp.status_code) {
            VirtualDisplayError::PipeIo(format!("driver refused: {msg}"))
        } else {
            VirtualDisplayError::DriverFailed(resp.status_code as u32)
        },
    )
}

/// Pipe handle wrapper that closes on drop. Saves manual `CloseHandle`
/// in every early-return branch and survives panics.
struct OwnedPipe(HANDLE);

impl Drop for OwnedPipe {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: handle was obtained from CreateFileW and is owned
            // by us; CloseHandle is idempotent for invalid handles.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn encode_pcwstr_pipe(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Synchronously open the driver pipe, send a `DriverRequest`, read the
/// `DriverResponse`. The pipe is closed before returning so each call
/// is atomic from the driver's perspective.
pub fn send_request(req: &DriverRequest) -> Result<DriverResponse, VirtualDisplayError> {
    send_request_with_timeout(req, DEFAULT_WAIT_TIMEOUT)
}

pub fn send_request_with_timeout(
    req: &DriverRequest,
    wait_timeout: Duration,
) -> Result<DriverResponse, VirtualDisplayError> {
    let pipe_name_w = encode_pcwstr_pipe(PIPE_NAME);
    let pipe_pcwstr = PCWSTR(pipe_name_w.as_ptr());

    let timeout_ms: u32 = wait_timeout
        .as_millis()
        .try_into()
        .unwrap_or(u32::MAX.saturating_sub(1));
    // SAFETY: pipe_name_w outlives this call.
    let waited = unsafe { WaitNamedPipeW(pipe_pcwstr, timeout_ms) };
    if !waited.as_bool() {
        // GetLastError values:
        //   ERROR_FILE_NOT_FOUND  → driver isn't running
        //   WAIT_TIMEOUT          → driver is busy beyond timeout_ms
        // Both translate to PipeIo so the daemon emits the same
        // "virtual display unavailable" error code upstream.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        let detail = match err.0 {
            x if x == ERROR_FILE_NOT_FOUND.0 => "driver pipe not available".into(),
            x if x == WAIT_TIMEOUT.0 => format!("driver pipe busy for {wait_timeout:?}"),
            other => format!("WaitNamedPipeW failed: WIN32 error {other}"),
        };
        return Err(VirtualDisplayError::PipeIo(detail));
    }

    // Open the pipe with read+write access. FILE_SHARE_NONE: the driver
    // never expects multiple concurrent clients on the control channel.
    // SAFETY: pipe_name_w outlives this call.
    let handle = unsafe {
        CreateFileW(
            pipe_pcwstr,
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| VirtualDisplayError::PipeIo(format!("CreateFileW({PIPE_NAME}): {e}")))?;
    let pipe = OwnedPipe(handle);

    let frame = frame_request(req)?;
    // SAFETY: pipe.0 is a valid open handle owned by us.
    unsafe { WriteFile(pipe.0, Some(&frame), None, None) }
        .map_err(|e| VirtualDisplayError::PipeIo(format!("WriteFile request frame: {e}")))?;

    // Adapt the pipe handle to std::io::Read so we can share the framing
    // parser with the unit tests rather than duplicating it.
    let mut reader = PipeReader { handle: pipe.0 };
    read_framed_response(&mut reader)
}

/// `std::io::Read` adapter over a Win32 pipe HANDLE. Used by
/// [`send_request_with_timeout`] so the production path goes through
/// the same [`read_framed_response`] helper that the unit tests cover.
struct PipeReader {
    handle: HANDLE,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut bytes_read: u32 = 0;
        // SAFETY: self.handle is a valid open pipe handle for the
        // lifetime of the PipeReader (owned by the caller's OwnedPipe).
        let result = unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut bytes_read as *mut u32),
                None,
            )
        };
        match result {
            Ok(()) => Ok(bytes_read as usize),
            Err(e) => Err(std::io::Error::other(format!("ReadFile: {e}"))),
        }
    }
}

/// A pipe open that may succeed when tried again shortly: no instance
/// exists between two server iterations, or another client got it first.
fn is_transient_open_error(code: u32) -> bool {
    code == ERROR_FILE_NOT_FOUND.0
        || code == ERROR_PIPE_BUSY.0
        || code == ERROR_SEM_TIMEOUT.0
        || code == WAIT_TIMEOUT.0
}

/// Open the driver pipe, retrying transient failures until `budget` ends.
fn open_pipe_retrying(budget: Duration) -> Result<OwnedPipe, VirtualDisplayError> {
    let pipe_name_w = encode_pcwstr_pipe(PIPE_NAME);
    let pipe_pcwstr = PCWSTR(pipe_name_w.as_ptr());
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = remaining.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32;
        // SAFETY: pipe_name_w outlives both calls.
        let code = if unsafe { WaitNamedPipeW(pipe_pcwstr, wait_ms) }.as_bool() {
            match unsafe {
                CreateFileW(
                    pipe_pcwstr,
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            } {
                Ok(handle) => return Ok(OwnedPipe(handle)),
                Err(error) => (error.code().0 as u32) & 0xFFFF,
            }
        } else {
            unsafe { windows::Win32::Foundation::GetLastError() }.0
        };
        if !is_transient_open_error(code) || Instant::now() >= deadline {
            return Err(VirtualDisplayError::PipeIo(format!(
                "driver pipe not available within {budget:?}: WIN32 error {code}"
            )));
        }
        std::thread::sleep(MONITOR_COMMAND_RETRY);
    }
}

/// Send `plug_monitor` / `unplug_monitor` from the daemon (SYSTEM, session
/// 0; the pipe lives in the global namespace and its DACL grants SYSTEM).
pub fn send_monitor_command(req: &DriverRequest) -> Result<(), VirtualDisplayError> {
    let pipe = open_pipe_retrying(MONITOR_COMMAND_BUDGET)?;
    let frame = frame_request(req)?;
    // SAFETY: pipe.0 is a valid open handle owned by us.
    unsafe { WriteFile(pipe.0, Some(&frame), None, None) }
        .map_err(|e| VirtualDisplayError::PipeIo(format!("WriteFile request frame: {e}")))?;
    let mut reader = PipeReader { handle: pipe.0 };
    extract_monitor_result(read_framed_response(&mut reader)?)
}

/// Convenience wrapper used by `WindowsController::set_mode`.
pub fn send_set_mode(mode: VirtualDisplayMode) -> Result<VirtualDisplayMode, VirtualDisplayError> {
    let req = DriverRequest::SetMode {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
    };
    let resp = send_request(&req)?;
    extract_applied_mode(mode, resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_mode() -> VirtualDisplayMode {
        VirtualDisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }
    }

    #[test]
    fn monitor_command_answers_are_mapped() {
        assert!(extract_monitor_result(DriverResponse::success(None)).is_ok());
        // A driver that predates plug/unplug: the caller removes the device.
        assert!(matches!(
            extract_monitor_result(DriverResponse::failure(
                100,
                "unknown command: plug_monitor"
            )),
            Err(VirtualDisplayError::NotSupported)
        ));
        assert!(matches!(
            extract_monitor_result(DriverResponse::failure(104, "adapter not initialised")),
            Err(VirtualDisplayError::PipeIo(_))
        ));
        assert!(matches!(
            extract_monitor_result(DriverResponse::failure(0xC000_00A3_u32 as i32, "failed")),
            Err(VirtualDisplayError::DriverFailed(0xC000_00A3))
        ));
    }

    #[test]
    fn only_vanished_or_busy_pipes_are_retried() {
        assert!(is_transient_open_error(ERROR_FILE_NOT_FOUND.0));
        assert!(is_transient_open_error(ERROR_PIPE_BUSY.0));
        assert!(!is_transient_open_error(5)); // access denied
    }

    #[test]
    fn frame_request_emits_le_length_then_json_body() {
        let req = DriverRequest::SetMode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let frame = frame_request(&req).expect("frame");
        assert!(frame.len() > 4);
        let body_len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(body_len, frame.len() - 4);
        // Body is parseable JSON matching the request.
        let body_str = std::str::from_utf8(&frame[4..]).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(body_str).expect("json");
        assert_eq!(parsed["command"], "set_mode");
        assert_eq!(parsed["params"]["width"], 1280);
        assert_eq!(parsed["params"]["height"], 720);
        assert_eq!(parsed["params"]["refresh_hz"], 60);
    }

    #[test]
    fn read_framed_response_round_trips_through_cursor() {
        // Build a known response, frame it the way the driver would,
        // then verify our reader recovers it byte-for-byte.
        let resp = DriverResponse::success(Some(serde_json::json!({
            "applied_mode": { "width": 1280, "height": 720, "refresh_hz": 60 }
        })));
        let body = serde_json::to_vec(&resp).unwrap();
        let mut buf = Vec::with_capacity(4 + body.len());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&body);

        let mut cursor = Cursor::new(buf);
        let parsed = read_framed_response(&mut cursor).expect("parse ok");
        assert!(parsed.success);
        assert_eq!(parsed.status_code, 0);
    }

    #[test]
    fn read_framed_response_rejects_oversized_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(DRIVER_MAX_MESSAGE_SIZE + 1).to_le_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_framed_response(&mut cursor).expect_err("must reject");
        match err {
            VirtualDisplayError::PipeIo(m) => assert!(
                m.contains("exceeds DRIVER_MAX_MESSAGE_SIZE"),
                "unexpected: {m}"
            ),
            other => panic!("expected PipeIo, got {other}"),
        }
    }

    #[test]
    fn read_framed_response_rejects_zero_length() {
        let buf = vec![0u8, 0, 0, 0];
        let mut cursor = Cursor::new(buf);
        let err = read_framed_response(&mut cursor).expect_err("must reject");
        match err {
            VirtualDisplayError::PipeIo(m) => {
                assert!(m.contains("zero-length"), "unexpected: {m}")
            }
            other => panic!("expected PipeIo, got {other}"),
        }
    }

    #[test]
    fn read_framed_response_propagates_short_body() {
        // Length header claims 50 bytes but body is empty.
        let mut buf = Vec::new();
        buf.extend_from_slice(&50u32.to_le_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_framed_response(&mut cursor).expect_err("must fail");
        match err {
            VirtualDisplayError::PipeIo(m) => assert!(m.contains("read body"), "unexpected: {m}"),
            other => panic!("expected PipeIo, got {other}"),
        }
    }

    #[test]
    fn read_framed_response_rejects_invalid_json() {
        let body = b"not valid json {{".to_vec();
        let mut buf = Vec::new();
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&body);
        let mut cursor = Cursor::new(buf);
        let err = read_framed_response(&mut cursor).expect_err("must fail");
        match err {
            VirtualDisplayError::PipeIo(m) => {
                assert!(m.contains("parse driver response JSON"), "unexpected: {m}")
            }
            other => panic!("expected PipeIo, got {other}"),
        }
    }

    #[test]
    fn extract_applied_mode_prefers_applied_payload_over_request() {
        // Driver snapped 2000x1100 to a supported 1920x1080@60.
        let resp = DriverResponse::success(Some(serde_json::json!({
            "applied_mode": { "width": 1920, "height": 1080, "refresh_hz": 60 }
        })));
        let requested = VirtualDisplayMode {
            width: 2000,
            height: 1100,
            refresh_hz: 60,
        };
        let applied = extract_applied_mode(requested, resp).expect("ok");
        assert_eq!(applied.width, 1920);
        assert_eq!(applied.height, 1080);
        assert_eq!(applied.refresh_hz, 60);
    }

    #[test]
    fn extract_applied_mode_falls_back_to_request_without_data() {
        let resp = DriverResponse::success(None);
        let applied = extract_applied_mode(sample_mode(), resp).expect("ok");
        assert_eq!(applied, sample_mode());
    }

    #[test]
    fn extract_applied_mode_falls_back_when_applied_mode_is_missing_field() {
        // data exists but doesn't carry the applied_mode object.
        let resp = DriverResponse::success(Some(serde_json::json!({ "other": 1 })));
        let applied = extract_applied_mode(sample_mode(), resp).expect("ok");
        assert_eq!(applied, sample_mode());
    }

    #[test]
    fn extract_applied_mode_maps_protocol_error_range_to_invalid_mode() {
        let mut resp = DriverResponse::failure(103, "missing width");
        // Inside protocol range [1, 1000) → InvalidMode.
        resp.status_code = 103;
        let err = extract_applied_mode(sample_mode(), resp).expect_err("must fail");
        assert!(
            matches!(&err, VirtualDisplayError::InvalidMode(m) if m.contains("missing width")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn extract_applied_mode_maps_ntstatus_to_driver_failed() {
        // 0xC0000001 = STATUS_UNSUCCESSFUL (as signed int = -1073741823).
        let mut resp = DriverResponse::failure(0xC0000001u32 as i32, "iddcx failure");
        resp.status_code = 0xC0000001u32 as i32;
        let err = extract_applied_mode(sample_mode(), resp).expect_err("must fail");
        match err {
            VirtualDisplayError::DriverFailed(code) => assert_eq!(code, 0xC0000001),
            other => panic!("expected DriverFailed, got {other}"),
        }
    }

    #[test]
    fn frame_request_then_parse_roundtrips() {
        // Frame a request, then parse it back through the same framing
        // logic — guarantees the wire format is symmetric and the
        // driver's C++ implementation has a fixed contract to match.
        let req = DriverRequest::SetMode {
            width: 2560,
            height: 1440,
            refresh_hz: 144,
        };
        let frame = frame_request(&req).expect("frame");
        let body_len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(body_len, frame.len() - 4);
        let back: DriverRequest = serde_json::from_slice(&frame[4..]).expect("parse");
        assert_eq!(back, req);
    }
}
