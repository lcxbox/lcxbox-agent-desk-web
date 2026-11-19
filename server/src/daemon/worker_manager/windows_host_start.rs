//! Native part of starting the ordinary-user application host.
use super::super::file_recovery_quota::windows_worker_user;
use super::super::{ProcessHandle, WorkerHandle};
use super::LiveHost;
use desk_ipc_protocol::message::ApplicationHostStartRefusal;
use std::ffi::c_void;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_TIMEOUT},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_ELEVATION,
            TOKEN_USER, TokenElevation, TokenUser,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            RemoteDesktop::WTSQueryUserToken,
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetProcessId,
                PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};

struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
// The handle is only waited on with a zero timeout.
unsafe impl Send for Owned {}

struct Environment(*mut c_void);
impl Drop for Environment {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyEnvironmentBlock(self.0);
        }
    }
}

/// A started host; the daemon keeps its handle only to count live hosts.
pub(super) struct HostProcess(Owned);
impl LiveHost for HostProcess {
    fn running(&self) -> bool {
        unsafe { WaitForSingleObject(self.0.0, 0) == WAIT_TIMEOUT }
    }
}

/// The daemon's facts about the requesting worker.
pub(super) struct WorkerIdentity {
    process_id: u32,
    session_id: u32,
    user_sid: String,
}

/// Read under the worker manager lock from the daemon-owned process handle.
pub(super) fn worker_identity(worker: &WorkerHandle) -> Option<WorkerIdentity> {
    let Some(ProcessHandle::WindowsNative(process)) = worker.process_handle.as_ref() else {
        return None;
    };
    let user_sid = windows_worker_user(worker)?;
    let process_id = unsafe { GetProcessId(process.raw_handle()) };
    (process_id != 0).then_some(WorkerIdentity {
        process_id,
        session_id: worker.session_id,
        user_sid,
    })
}

fn native(error: &windows::core::Error) -> ApplicationHostStartRefusal {
    ApplicationHostStartRefusal::NativeFailure {
        code: error.code().0,
    }
}

fn token_user(token: HANDLE) -> Option<String> {
    let mut size = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    if size == 0 || size > 65536 {
        return None;
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }
    .ok()?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.ok()?;
    let sid = unsafe { text.to_string() }.ok();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    sid
}

fn elevated(token: HANDLE) -> Option<bool> {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    }
    .ok()?;
    Some(elevation.TokenIsElevated != 0)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Start `<this executable> <host mode> <pipe> <worker pid>` with the
/// session's ordinary logon token. `pipe_name` must already be validated.
pub(super) fn start_host(
    identity: &WorkerIdentity,
    pipe_name: &str,
) -> Result<(u32, HostProcess), ApplicationHostStartRefusal> {
    let mut raw = HANDLE::default();
    unsafe { WTSQueryUserToken(identity.session_id, &mut raw) }.map_err(|error| {
        tracing::warn!(%error, "WTSQueryUserToken failed for the application host");
        ApplicationHostStartRefusal::SessionUnavailable
    })?;
    // WTSQueryUserToken returns the ordinary (filtered) half of a UAC split
    // token; it is deliberately not exchanged for the linked elevated token.
    let token = Owned(raw);
    if token_user(token.0).as_deref() != Some(identity.user_sid.as_str())
        || elevated(token.0) != Some(false)
    {
        return Err(ApplicationHostStartRefusal::UserMismatch);
    }
    let mut block = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut block, Some(token.0), false) }
        .map_err(|error| native(&error))?;
    let environment = Environment(block);

    let executable = std::env::current_exe()
        .map_err(|_| ApplicationHostStartRefusal::NativeFailure { code: 0 })?;
    let executable = executable.to_string_lossy().into_owned();
    // The pipe name is a fixed prefix and a UUID and the PID is decimal, so
    // only the executable path needs quoting.
    let mut command = wide(&format!(
        "\"{executable}\" {} {pipe_name} {}",
        crate::windows_application_host::MODE,
        identity.process_id
    ));
    let application = wide(&executable);
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let cwd = wide(&format!("{system}\\System32"));
    let mut desktop = wide("WinSta0\\Default");
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.0.cast_const()),
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut process,
        )
    }
    .map_err(|error| {
        tracing::warn!(%error, "CreateProcessAsUserW failed for the application host");
        native(&error)
    })?;
    drop(Owned(process.hThread));
    Ok((process.dwProcessId, HostProcess(Owned(process.hProcess))))
}
