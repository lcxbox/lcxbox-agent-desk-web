use super::*;
use crate::daemon::{
    local_access_control_transport::{create_windows_pipe, windows_peer, windows_token_identity},
    pipe_security,
};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::SECURITY_IDENTIFICATION,
    System::{
        Pipes::GetNamedPipeServerProcessId,
        Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

fn endpoint(config: &Path) -> String {
    format!(r"\\.\pipe\lcxbox-account-{}", identity(config))
}

pub(super) fn start(config: &Path, service: Arc<Recovery>) -> Result<()> {
    let (host_sid, elevated_host) = windows_token_identity(unsafe { GetCurrentProcess() })?;
    let name = endpoint(config);
    let sddl = pipe_security::build_pipe_sddl(Some(&host_sid));
    let mut listener = create_windows_pipe(&name, &sddl, true)?;
    let id = identity(config);
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    tokio::spawn(async move {
        loop {
            if listener.connect().await.is_err() {
                break;
            }
            let Ok(next) = create_windows_pipe(&name, &sddl, false) else {
                break;
            };
            let stream = std::mem::replace(&mut listener, next);
            let Ok(peer) = windows_peer(&stream) else {
                continue;
            };
            if !peer.is_elevated() && (elevated_host || peer.user_id() != host_sid) {
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let (service, id) = (service.clone(), id.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let _ = serve_peer(stream, service, id).await;
            });
        }
        log::error!("Password recovery pipe listener stopped");
    });
    Ok(())
}

pub(super) async fn connect(config: &Path) -> Result<NamedPipeClient> {
    let pipe = ClientOptions::new()
        .security_qos_flags(SECURITY_IDENTIFICATION.0)
        .open(endpoint(config))?;
    let mut pid = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }?;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
    let _owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let (sid, elevated) = windows_token_identity(handle)?;
    ensure!(
        elevated || sid == "S-1-5-18" || sid == super::super::permissions::owner(config)?,
        "Unexpected recovery service identity"
    );
    Ok(pipe)
}

use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, GetConsoleMode,
    GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};
pub(super) struct TerminalMode(HANDLE, CONSOLE_MODE);
impl Drop for TerminalMode {
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleMode(self.0, self.1);
        }
    }
}
pub(super) fn hide_input() -> Result<TerminalMode> {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }?;
    let mut original = CONSOLE_MODE::default();
    unsafe { GetConsoleMode(handle, &mut original) }?;
    let hidden = CONSOLE_MODE(
        original.0 & !(ENABLE_ECHO_INPUT.0 | ENABLE_LINE_INPUT.0 | ENABLE_PROCESSED_INPUT.0),
    );
    unsafe { SetConsoleMode(handle, hidden) }?;
    Ok(TerminalMode(handle, original))
}
