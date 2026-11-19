//! Private ordinary-user launch host; no server, UI, or model endpoint is started.
use crate::worker::agent::application_launch::windows_diagnostics::{explained, failure};
use crate::worker::agent::application_launch::{windows_package, windows_process};
use desk_agent_protocol::application_launch::LaunchError;
use desk_agent_protocol::application_launch::*;
use desk_agent_protocol::native_diagnostic::DiagnosticStage;
use serde::{Deserialize, Serialize};
use std::{io, os::windows::io::AsRawHandle, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
    },
    core::PCWSTR,
};

pub const MODE: &str = "application-launch-user-host";
use desk_ipc_protocol::message::APPLICATION_HOST_PIPE_PREFIX as PIPE_PREFIX;
const MAX_FRAME: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    version: u32,
    user_sid: String,
    session_id: u32,
    request: LaunchApplicationRequest,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invoke {
    dispatch_id: String,
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> io::Result<T> {
    let size = reader.read_u32_le().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::other("invalid launch host frame"));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::other("oversized launch host frame"));
    }
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn server(pipe: &str, sid: &str) -> io::Result<NamedPipeServer> {
    if !sid.starts_with("S-1-")
        || !sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
    {
        return Err(io::Error::other("invalid launch host user"));
    }
    let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut security = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(descriptor.as_ptr()),
            1,
            &mut security,
            None,
        )
        .map_err(io::Error::other)?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: false.into(),
        };
        let result = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                pipe,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            );
        let _ = LocalFree(Some(HLOCAL(security.0)));
        result
    }
}

pub(crate) struct PreparedHost {
    pipe: NamedPipeServer,
    pub(crate) identity: ResolvedApplicationIdentity,
    request: LaunchApplicationRequest,
}
impl PreparedHost {
    pub(crate) async fn invoke(mut self, dispatch_id: String) -> LaunchApplicationResult {
        let mut unknown = LaunchApplicationResult {
            diagnostic: None,
            dispatch_id: dispatch_id.clone(),
            launch_outcome: LaunchOutcome::OutcomeUnknown,
            argument_delivery: if self.request.args.is_empty() {
                ArgumentDelivery::NotRequested
            } else {
                ArgumentDelivery::Unknown
            },
            failure_reason: None,
            requested_admin: false,
            created_process_id: None,
            created_process_elevated: None,
            observations: vec![],
        };
        let result = tokio::time::timeout(TIMEOUT, async {
            write_frame(
                &mut self.pipe,
                &Invoke {
                    dispatch_id: dispatch_id.clone(),
                },
            )
            .await?;
            read_frame::<LaunchApplicationResult>(&mut self.pipe).await
        })
        .await;
        match result {
            Ok(Ok(result)) if result.dispatch_id == dispatch_id && !result.requested_admin => {
                result
            }
            Ok(Err(error)) => {
                unknown.diagnostic = Some(
                    failure(
                        LaunchFailureReason::NativeFailure,
                        DiagnosticStage::HelperCommunication,
                        "application host invocation",
                        &error,
                    )
                    .diagnostic,
                );
                unknown
            }
            Err(error) => {
                unknown.diagnostic = Some(
                    failure(
                        LaunchFailureReason::NativeFailure,
                        DiagnosticStage::HelperCommunication,
                        "application host invocation deadline",
                        &error,
                    )
                    .diagnostic,
                );
                unknown
            }
            Ok(Ok(_)) => {
                unknown.diagnostic = Some(
                    desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                        DiagnosticStage::HelperCommunication,
                        "validate host receipt",
                        "application",
                        None,
                        "Helper receipt identity did not match dispatch",
                    ),
                );
                unknown
            }
        }
    }
}

pub(crate) async fn prepare(
    request: &LaunchApplicationRequest,
) -> Result<PreparedHost, LaunchError> {
    if request.run_as_admin {
        return Err(LaunchFailureReason::PermissionDenied.into());
    }
    request
        .validate()
        .map_err(|_| LaunchError::from(LaunchFailureReason::InvalidTarget))?;
    let session = windows_process::catalog_session_identity()?;
    let (sid, session_id) = session
        .rsplit_once(':')
        .ok_or(LaunchFailureReason::SessionUnavailable)?;
    let session_id = session_id.parse().map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::HelperCommunication,
            "application host preparation",
            &error,
        )
    })?;
    let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
    let pipe = server(&name, sid).map_err(|error| {
        failure(
            LaunchFailureReason::NativeFailure,
            DiagnosticStage::HelperCommunication,
            "application host preparation",
            &error,
        )
    })?;
    let executable = std::env::current_exe().map_err(|error| {
        failure(
            LaunchFailureReason::NativeFailure,
            DiagnosticStage::HelperCommunication,
            "application host preparation",
            &error,
        )
    })?;
    let executable = executable
        .to_str()
        .ok_or(LaunchFailureReason::InvalidTarget)?;
    let helper = LaunchApplicationRequest {
        target: ApplicationTarget {
            kind: ApplicationTargetKind::Executable,
            value: executable.into(),
        },
        args: vec![MODE.into(), name.clone(), std::process::id().to_string()],
        cwd: None,
        run_as_admin: false,
    };
    let (elevated, kind) = windows_process::current_elevation()?;
    let service = SERVICE_HOST_STARTER.get();
    let pid = match host_start_route(elevated, kind, service.is_some()) {
        HostStart::Local => created_host_pid(
            windows_process::prepare(&helper, sid, session_id)?
                .invoke_hidden_host(uuid::Uuid::new_v4().to_string()),
        )?,
        HostStart::Derived => created_host_pid(
            windows_process::prepare_host_with_derived_token(&helper, sid, session_id)?
                .invoke_hidden_host(uuid::Uuid::new_v4().to_string()),
        )?,
        HostStart::Service => match service {
            Some(starter) => starter.start(&name).await?,
            None => return Err(LaunchFailureReason::SessionUnavailable.into()),
        },
        HostStart::NoOrdinaryToken => {
            return Err(explained(
                LaunchFailureReason::Unsupported,
                DiagnosticStage::HelperStart,
                "application host start",
                windows_process::NO_ORDINARY_TOKEN,
            ));
        }
        HostStart::Unavailable => return Err(LaunchFailureReason::SessionUnavailable.into()),
    };
    let mut pipe = pipe;
    let exchange = tokio::time::timeout(TIMEOUT, async {
        pipe.connect().await.map_err(|error| {
            failure(
                LaunchFailureReason::NativeFailure,
                DiagnosticStage::HelperCommunication,
                "application host preparation",
                &error,
            )
        })?;
        let mut peer = 0;
        unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut peer) }.map_err(
            |error| {
                failure(
                    LaunchFailureReason::PermissionDenied,
                    DiagnosticStage::HelperCommunication,
                    "application host preparation",
                    &error,
                )
            },
        )?;
        if peer != pid {
            return Err(LaunchFailureReason::PermissionDenied.into());
        }
        write_frame(
            &mut pipe,
            &Prepare {
                version: 2,
                user_sid: sid.into(),
                session_id,
                request: request.clone(),
            },
        )
        .await
        .map_err(|error| {
            failure(
                LaunchFailureReason::NativeFailure,
                DiagnosticStage::HelperCommunication,
                "application host preparation",
                &error,
            )
        })?;
        let identity: Result<ResolvedApplicationIdentity, LaunchError> =
            read_frame(&mut pipe).await.map_err(|error| {
                failure(
                    LaunchFailureReason::NativeFailure,
                    DiagnosticStage::HelperCommunication,
                    "application host preparation",
                    &error,
                )
            })?;
        identity
    })
    .await
    .map_err(|error| {
        failure(
            LaunchFailureReason::NativeFailure,
            DiagnosticStage::HelperCommunication,
            "application host preparation",
            &error,
        )
    })??;
    Ok(PreparedHost {
        pipe,
        identity: exchange,
        request: request.clone(),
    })
}

#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
fn created_host_pid(created: LaunchApplicationResult) -> Result<u32, LaunchError> {
    created.created_process_id.ok_or_else(|| {
        let mut error: LaunchError = created
            .failure_reason
            .unwrap_or(LaunchFailureReason::NativeFailure)
            .into();
        if let Some(mut diagnostic) = created.diagnostic {
            diagnostic.stage = DiagnosticStage::HelperStart;
            error.diagnostic = diagnostic;
        }
        error
    })
}

/// How the ordinary-user application host is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostStart {
    /// This process is not elevated: start the host with its own token.
    Local,
    /// Elevated half of a UAC split token in a ServiceDaemon worker: the
    /// daemon starts the host with the ordinary token Windows created at logon.
    Service,
    /// Elevated half of a UAC split token without the daemon (portable app or
    /// DeskServer run as administrator): a token derived from this process's.
    Derived,
    /// Elevated without an ordinary token (UAC off, unfiltered built-in
    /// Administrator): an ordinary launch is refused.
    NoOrdinaryToken,
    Unavailable,
}

/// Preflight and execution both start the host here, so they always agree.
pub(crate) fn host_start_route(
    elevated: bool,
    kind: windows_process::ElevationKind,
    service_starter: bool,
) -> HostStart {
    use windows_process::ElevationKind;
    match (elevated, kind) {
        (false, _) => HostStart::Local,
        (true, ElevationKind::Full) if service_starter => HostStart::Service,
        (true, ElevationKind::Full) => HostStart::Derived,
        (true, ElevationKind::Default) => HostStart::NoOrdinaryToken,
        (true, ElevationKind::Limited) => HostStart::Unavailable,
    }
}

const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);
/// The daemon allows the same number of live hosts per worker.
const MAX_PENDING_SERVICE_STARTS: usize = 4;

static SERVICE_HOST_STARTER: std::sync::OnceLock<ServiceHostStarter> = std::sync::OnceLock::new();

/// Registered by a Windows session-user resident worker of the ServiceDaemon,
/// whose process serves exactly one worker. Other hosts never register it.
pub(crate) fn install_service_host_starter(
    sender: tokio::sync::mpsc::UnboundedSender<desk_ipc_protocol::message::WorkerToService>,
) {
    let _ = SERVICE_HOST_STARTER.set(ServiceHostStarter::new(sender));
}

/// Deliver the daemon's answer; a late or unknown answer is ignored.
pub(crate) fn complete_service_host_start(
    outcome: desk_ipc_protocol::message::ApplicationHostStartOutcome,
) {
    if let Some(starter) = SERVICE_HOST_STARTER.get() {
        starter.complete(outcome);
    }
}

type PendingStarts = std::collections::HashMap<
    String,
    tokio::sync::oneshot::Sender<desk_ipc_protocol::message::ApplicationHostStartResult>,
>;

pub(crate) struct ServiceHostStarter {
    sender: tokio::sync::mpsc::UnboundedSender<desk_ipc_protocol::message::WorkerToService>,
    pending: std::sync::Mutex<PendingStarts>,
}

/// Removes the pending entry however the wait ends, including cancellation.
struct PendingStart<'a> {
    pending: &'a std::sync::Mutex<PendingStarts>,
    request_id: String,
}
impl Drop for PendingStart<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.request_id);
    }
}

impl ServiceHostStarter {
    fn new(
        sender: tokio::sync::mpsc::UnboundedSender<desk_ipc_protocol::message::WorkerToService>,
    ) -> Self {
        Self {
            sender,
            pending: std::sync::Mutex::new(PendingStarts::new()),
        }
    }

    fn complete(&self, outcome: desk_ipc_protocol::message::ApplicationHostStartOutcome) {
        let sender = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&outcome.request_id);
        if let Some(sender) = sender {
            let _ = sender.send(outcome.outcome);
        }
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    async fn start(&self, pipe_name: &str) -> Result<u32, LaunchError> {
        self.start_with_timeout(pipe_name, SERVICE_START_TIMEOUT)
            .await
    }

    async fn start_with_timeout(
        &self,
        pipe_name: &str,
        timeout: Duration,
    ) -> Result<u32, LaunchError> {
        use desk_ipc_protocol::message::{
            ApplicationHostStart, ApplicationHostStartResult, WorkerToService,
        };
        const OPERATION: &str = "daemon application host start";
        let request_id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() >= MAX_PENDING_SERVICE_STARTS {
                return Err(explained(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::HelperStart,
                    OPERATION,
                    "too many application host starts are pending",
                ));
            }
            pending.insert(request_id.clone(), sender);
        }
        let _pending = PendingStart {
            pending: &self.pending,
            request_id: request_id.clone(),
        };
        self.sender
            .send(WorkerToService::StartApplicationHost(
                ApplicationHostStart {
                    request_id,
                    pipe_name: pipe_name.into(),
                },
            ))
            .map_err(|_| {
                explained(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::HelperStart,
                    OPERATION,
                    "the service connection is closed",
                )
            })?;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(ApplicationHostStartResult::Started { process_id })) if process_id != 0 => {
                Ok(process_id)
            }
            Ok(Ok(ApplicationHostStartResult::Refused { reason })) => Err(refusal(reason)),
            Ok(Ok(ApplicationHostStartResult::Started { .. })) | Ok(Err(_)) => Err(explained(
                LaunchFailureReason::NativeFailure,
                DiagnosticStage::HelperStart,
                OPERATION,
                "the service returned no application host",
            )),
            Err(_) => Err(explained(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::HelperStart,
                OPERATION,
                "the service did not answer in time",
            )),
        }
    }
}

fn refusal(reason: desk_ipc_protocol::message::ApplicationHostStartRefusal) -> LaunchError {
    use desk_ipc_protocol::message::ApplicationHostStartRefusal as Refusal;
    let (failure, message) = match reason {
        Refusal::NotResidentUser => (
            LaunchFailureReason::SessionUnavailable,
            "the service does not start hosts for this worker".to_string(),
        ),
        Refusal::InvalidPipe => (
            LaunchFailureReason::NativeFailure,
            "the service rejected the host pipe".to_string(),
        ),
        Refusal::SessionUnavailable => (
            LaunchFailureReason::SessionUnavailable,
            "the user session is unavailable".to_string(),
        ),
        Refusal::UserMismatch => (
            LaunchFailureReason::SessionUnavailable,
            "the session's ordinary user token does not match this worker".to_string(),
        ),
        Refusal::TooManyHosts => (
            LaunchFailureReason::SessionUnavailable,
            "too many application hosts are running".to_string(),
        ),
        Refusal::Unsupported => (
            LaunchFailureReason::Unsupported,
            "the service cannot start an application host on this platform".to_string(),
        ),
        Refusal::NativeFailure { code } => (
            LaunchFailureReason::NativeFailure,
            format!("the service could not start the application host (0x{code:08X})"),
        ),
    };
    explained(
        failure,
        DiagnosticStage::HelperStart,
        "daemon application host start",
        &message,
    )
}

/// Strict private entry point shared by the standalone server and Tauri binary.
/// This runs before either application initializes a UI, settings, or logging.
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(MODE)) {
        return None;
    }
    let parsed = (|| {
        let name = args.next()?.into_string().ok()?;
        let pid: u32 = args.next()?.into_string().ok()?.parse().ok()?;
        let suffix = name.strip_prefix(PIPE_PREFIX)?;
        uuid::Uuid::parse_str(suffix).ok()?;
        if pid == 0 || args.next().is_some() {
            return None;
        }
        Some((name, pid))
    })();
    let Some((name, pid)) = parsed else {
        return Some(2);
    };
    // The watchdog exits only this helper, never its launched process tree.
    // Parent-side Invoking remains unknown if the native call does not return.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(50));
        std::process::exit(124);
    });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(v) => v,
        Err(_) => return Some(1),
    };
    Some(match runtime.block_on(serve(&name, pid)) {
        Ok(()) => 0,
        Err(_) => 1,
    })
}

async fn serve(name: &str, parent_pid: u32) -> io::Result<()> {
    if windows_process::current_elevated().map_err(|_| io::Error::other("invalid host token"))? {
        return Err(io::Error::other("host must be an ordinary user"));
    }
    let mut pipe = ClientOptions::new().open(name)?;
    let mut server_pid = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server_pid) }
        .map_err(io::Error::other)?;
    if server_pid != parent_pid {
        return Err(io::Error::other("unexpected host parent"));
    }
    let request: Prepare = tokio::time::timeout(TIMEOUT, read_frame(&mut pipe))
        .await
        .map_err(io::Error::other)??;
    let session = windows_process::catalog_session_identity()
        .map_err(|_| io::Error::other("host user unavailable"))?;
    if request.version != 2
        || request.request.run_as_admin
        || session != format!("{}:{}", request.user_sid, request.session_id)
    {
        return Err(io::Error::other("launch host identity mismatch"));
    }
    let executable = if request.request.target.kind == ApplicationTargetKind::Executable {
        Some(windows_process::prepare(
            &request.request,
            &request.user_sid,
            request.session_id,
        ))
    } else {
        None
    };
    let identity = match &executable {
        Some(Ok(prepared)) => Ok(prepared.identity.clone()),
        Some(Err(reason)) => Err(reason.clone()),
        None => windows_package::resolve(&request.request),
    };
    write_frame(&mut pipe, &identity).await?;
    let Ok(identity) = identity else {
        return Ok(());
    };
    let invoke: Invoke = tokio::time::timeout(TIMEOUT, read_frame(&mut pipe))
        .await
        .map_err(io::Error::other)??;
    if invoke.dispatch_id.is_empty()
        || invoke.dispatch_id.len() > 256
        || invoke.dispatch_id.chars().any(char::is_control)
    {
        return Err(io::Error::other("invalid launch dispatch"));
    }
    let result = match executable {
        Some(Ok(prepared)) => prepared.invoke(invoke.dispatch_id),
        _ => windows_package::invoke(&request.request, &identity, invoke.dispatch_id),
    };
    write_frame(&mut pipe, &result).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::{
        ApplicationHostStartOutcome, ApplicationHostStartRefusal, ApplicationHostStartResult,
        WorkerToService,
    };

    #[test]
    fn the_host_start_follows_the_token_and_the_service() {
        use windows_process::ElevationKind::{Default, Full, Limited};
        for kind in [Default, Full, Limited] {
            for service in [false, true] {
                // Not elevated, including a host on a derived token (Full).
                assert_eq!(host_start_route(false, kind, service), HostStart::Local);
            }
        }
        assert_eq!(host_start_route(true, Full, true), HostStart::Service);
        assert_eq!(host_start_route(true, Full, false), HostStart::Derived);
        assert_eq!(
            host_start_route(true, Default, true),
            HostStart::NoOrdinaryToken
        );
        assert_eq!(
            host_start_route(true, Default, false),
            HostStart::NoOrdinaryToken
        );
        assert_eq!(
            host_start_route(true, Limited, false),
            HostStart::Unavailable
        );
    }

    fn starter() -> (
        ServiceHostStarter,
        tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (ServiceHostStarter::new(sender), receiver)
    }

    async fn next_request(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
    ) -> String {
        match receiver.recv().await {
            Some(WorkerToService::StartApplicationHost(request)) => request.request_id,
            _ => panic!("start request expected"),
        }
    }

    #[tokio::test]
    async fn the_daemon_answer_decides_the_host_start() {
        let (starter, mut receiver) = starter();
        let pipe = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
        let start = starter.start(&pipe);
        let answer = async {
            let request_id = next_request(&mut receiver).await;
            starter.complete(ApplicationHostStartOutcome {
                request_id,
                outcome: ApplicationHostStartResult::Started { process_id: 4242 },
            });
        };
        let (pid, ()) = tokio::join!(start, answer);
        assert_eq!(pid.unwrap(), 4242);
        assert_eq!(starter.pending_len(), 0);

        let start = starter.start(&pipe);
        let answer = async {
            let request_id = next_request(&mut receiver).await;
            starter.complete(ApplicationHostStartOutcome {
                request_id,
                outcome: ApplicationHostStartResult::Refused {
                    reason: ApplicationHostStartRefusal::TooManyHosts,
                },
            });
        };
        let (refused, ()) = tokio::join!(start, answer);
        assert_eq!(
            refused.unwrap_err().reason,
            LaunchFailureReason::SessionUnavailable
        );
        assert_eq!(starter.pending_len(), 0);
    }

    #[tokio::test]
    async fn a_late_answer_is_ignored_and_pending_starts_are_bounded() {
        let (starter, mut receiver) = starter();
        let pipe = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
        let timed_out = starter
            .start_with_timeout(&pipe, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert_eq!(timed_out.reason, LaunchFailureReason::SessionUnavailable);
        assert_eq!(starter.pending_len(), 0);
        let late = next_request(&mut receiver).await;
        starter.complete(ApplicationHostStartOutcome {
            request_id: late,
            outcome: ApplicationHostStartResult::Started { process_id: 7 },
        });
        assert_eq!(starter.pending_len(), 0);

        // Cancelled waits release their slot, and only four may be pending.
        let mut waits: Vec<_> = (0..MAX_PENDING_SERVICE_STARTS)
            .map(|_| Box::pin(starter.start_with_timeout(&pipe, Duration::from_secs(30))))
            .collect();
        for wait in &mut waits {
            assert!(
                tokio::time::timeout(Duration::from_millis(5), wait.as_mut())
                    .await
                    .is_err()
            );
        }
        assert_eq!(starter.pending_len(), MAX_PENDING_SERVICE_STARTS);
        let refused = starter.start(&pipe).await.unwrap_err();
        assert_eq!(refused.reason, LaunchFailureReason::SessionUnavailable);
        drop(waits);
        assert_eq!(starter.pending_len(), 0);
    }

    #[tokio::test]
    async fn private_host_pipe_uses_a_valid_local_namespace() {
        let sid = windows_process::storage_user_sid().unwrap();
        let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
        let pipe = server(&name, &sid).expect("private launch pipe must be creatable");
        let client = ClientOptions::new().open(&name).unwrap();
        pipe.connect().await.unwrap();
        let mut peer = 0;
        unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut peer) }.unwrap();
        assert_eq!(peer, std::process::id());
        drop(client);
    }
    #[tokio::test]
    async fn protocol_rejects_oversized_and_unreviewed_fields() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32_le((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(read_frame::<Invoke>(&mut reader).await.is_err());
        assert!(
            serde_json::from_str::<Invoke>(r#"{"dispatch_id":"one","run_as_admin":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Prepare>(r#"{"version":1,"user_sid":"S-1-5-21-1","session_id":1,"request":{"target":{"kind":"executable","value":"C:\\app.exe"}},"command":"unreviewed"}"#).is_err());
    }
    #[tokio::test]
    async fn closing_preflight_without_invoke_is_not_an_invocation() {
        let (writer, mut reader) = tokio::io::duplex(32);
        drop(writer);
        assert_eq!(
            read_frame::<Invoke>(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
