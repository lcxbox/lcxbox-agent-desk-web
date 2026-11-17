//! Node-local private response channel. Manager routes HTTP to the socket-owning node.
use crate::{
    model::{
        auth_context::AuthKind,
        connection::{ConnectionState, SharedConnectionMap},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
    },
    service::FileRecoveryObserver,
};
use desk_agent_protocol::{
    AgentScope, ExecutionMode, RiskLevel,
    authz::{
        AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
        AuthzDevice, ExecAdmissionPolicy,
    },
    file_recovery::*,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy)]
pub enum RecoveryRequestError {
    Offline,
    Timeout,
    InvalidRequest,
    Capacity,
    InvalidReply,
}
struct Pending {
    connection: String,
    sender: oneshot::Sender<FileRecoveryReply>,
}
#[derive(Default)]
pub struct FileRecoveryHub {
    pending: Mutex<HashMap<String, Pending>>,
}
pub fn global_hub() -> Arc<FileRecoveryHub> {
    static HUB: OnceLock<Arc<FileRecoveryHub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(FileRecoveryHub::default()))
        .clone()
}
struct Registration {
    hub: Arc<FileRecoveryHub>,
    id: String,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.pending.lock().unwrap().remove(&self.id);
    }
}
impl FileRecoveryHub {
    fn register(
        self: &Arc<Self>,
        connection: &str,
    ) -> Result<(Registration, oneshot::Receiver<FileRecoveryReply>), RecoveryRequestError> {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= 256 {
            return Err(RecoveryRequestError::Capacity);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        pending.insert(
            id.clone(),
            Pending {
                connection: connection.into(),
                sender,
            },
        );
        Ok((
            Registration {
                hub: self.clone(),
                id,
            },
            receiver,
        ))
    }
    fn complete(&self, connection: &str, id: &str, reply: FileRecoveryReply) {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(id).is_none_or(|p| p.connection != connection) {
            return;
        }
        if let Some(p) = pending.remove(id) {
            let _ = p.sender.send(reply);
        }
    }
}
impl FileRecoveryObserver for FileRecoveryHub {
    fn on_file_recovery_reply<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.auth_context.auth_kind != AuthKind::TokenAuth
                || source.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
                || source.model.peer.remote_desk_type != RemoteDeskTypeEnum::Server
                || model.signaling_type != SignalingType::FileRecoveryManaged
                || model.to_connection_id.is_some()
                || model
                    .response_state
                    .as_ref()
                    .is_some_and(|s| s.error_code != 0)
            {
                return;
            }
            let Ok(reply) = model.get_data::<FileRecoveryReply>() else {
                return;
            };
            if reply.authority.len() != 64 || reply.os_user.len() > 128 {
                return;
            }
            match &reply.outcome {
                FileRecoveryOutcome::ExportChunk { data_base64, .. }
                    if data_base64.len() > 900_000 =>
                {
                    return;
                }
                FileRecoveryOutcome::Page { page } if page.records.len() > 100 => return,
                FileRecoveryOutcome::Entries { page } if page.entries.len() > MAX_ENTRY_PAGE => {
                    return;
                }
                FileRecoveryOutcome::Unavailable { reason } => {
                    log::warn!("File recovery device request unavailable: {:?}", reason);
                }
                _ => (),
            }
            self.complete(&source.model.connection_id, &model.request_id, reply);
        })
    }
}

/// Caller has already checked current device ownership and (for exports) that
/// the conversation is not deleted. No model or browser can provide `owner`.
pub async fn request_authorized(
    connections: &SharedConnectionMap,
    connection_id: &str,
    owner: i32,
    device_id: Option<i32>,
    request: FileRecoveryRequest,
) -> Result<FileRecoveryReply, RecoveryRequestError> {
    if owner <= 0
        || request.command.validate().is_err()
        || request
            .expected_os_user
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        || request
            .expected_authority
            .as_deref()
            .is_some_and(|s| s.len() != 64)
    {
        return Err(RecoveryRequestError::InvalidRequest);
    }
    let target = connections
        .read()
        .await
        .get(connection_id)
        .cloned()
        .ok_or(RecoveryRequestError::Offline)?;
    if target.auth_context.auth_kind != AuthKind::TokenAuth
        || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        || device_id.is_some_and(|id| target.auth_context.bound_device_id != Some(id))
    {
        return Err(RecoveryRequestError::Offline);
    }
    let audience = target
        .client_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or(RecoveryRequestError::Offline)?;
    let (registration, receiver) = global_hub().register(connection_id)?;
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
    let expected_authority = request.expected_authority.clone();
    let expected_os_user = request.expected_os_user.clone();
    let wrapper = AuthorizedControlPayload {
        inner: request,
        authz: AuthorizationBlock {
            file_recovery_registration: None,
            version: AUTHORIZATION_BLOCK_VERSION,
            scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: Some("owner-file-recovery".into()),
            },
            orchestrator_grants: vec![MANAGEMENT_GRANT.into()],
            max_risk: RiskLevel::Low,
            exec_admission_policy: ExecAdmissionPolicy::TemplateOnly,
            actor: AuthzActor {
                user_id: Some(owner),
            },
            device: AuthzDevice { device_id },
            request_id: registration.id.clone(),
            session_id: None,
            expires_at: Some(expires),
            issuer: "owner-file-recovery".into(),
            audience: audience.into(),
            signature: None,
        },
    };
    let frame = SignalingModel::new(
        &registration.id,
        SignalingType::ManageFileRecovery,
        None,
        None,
        Some(serde_json::to_value(wrapper).map_err(|_| RecoveryRequestError::InvalidRequest)?),
        None,
    );
    let body = serde_json::to_string(&frame).map_err(|_| RecoveryRequestError::InvalidRequest)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        target.session.write().await.text(body).await
    })
    .await
    .map_err(|_| RecoveryRequestError::Timeout)?
    .map_err(|_| RecoveryRequestError::Offline)?;
    let reply = tokio::time::timeout(Duration::from_secs(25), receiver)
        .await
        .map_err(|_| RecoveryRequestError::Timeout)?
        .map_err(|_| RecoveryRequestError::InvalidReply)?;
    if !connections.read().await.contains_key(connection_id) {
        return Err(RecoveryRequestError::Offline);
    }
    if (expected_authority.is_some_and(|id| id != reply.authority)
        || expected_os_user.is_some_and(|id| id != reply.os_user))
        && !matches!(
            reply.outcome,
            FileRecoveryOutcome::Unavailable {
                reason: FileRecoveryFailure::IdentityChanged
            }
        )
    {
        return Err(RecoveryRequestError::InvalidReply);
    }
    Ok(reply)
}

/// Why an export could not start or stopped.
#[derive(Debug)]
pub enum ExportError {
    Request(RecoveryRequestError),
    /// The device reported a typed failure for the first chunk.
    Unavailable(FileRecoveryFailure),
    /// The owner or conversation check failed, or chunks disagreed.
    Rejected,
}

/// One validated chunk.
struct Chunk {
    revision: u64,
    total: u64,
    offset: u64,
    bytes: Vec<u8>,
}

fn chunk(reply: FileRecoveryReply, offset: u64) -> Result<Chunk, ExportError> {
    use base64::Engine as _;
    let (revision, total, at, data) = match reply.outcome {
        FileRecoveryOutcome::ExportChunk {
            package_revision,
            total_size,
            offset,
            data_base64,
        } => (package_revision, total_size, offset, data_base64),
        FileRecoveryOutcome::Unavailable { reason } => {
            return Err(ExportError::Unavailable(reason));
        }
        _ => return Err(ExportError::Rejected),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ExportError::Rejected)?;
    let end = offset.checked_add(bytes.len() as u64);
    if at != offset
        || total > MAX_EXPORT_PACKAGE_BYTES
        || bytes.len() > MAX_EXPORT_CHUNK_BYTES
        || end.is_none_or(|end| end > total)
        || (bytes.is_empty() && offset < total)
    {
        return Err(ExportError::Rejected);
    }
    Ok(Chunk {
        revision,
        total,
        offset,
        bytes,
    })
}

/// Fetches the chunk at an offset from the device.
pub type FetchChunk = Box<
    dyn Fn(
            u64,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<FileRecoveryReply, RecoveryRequestError>>
                    + Send,
            >,
        > + Send
        + Sync,
>;
/// Whether the requester still owns the device and the conversation still
/// exists; checked after every chunk.
pub type StillAllowed = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync,
>;

/// A started export: its size and a stream of the remaining bytes.
pub struct ExportStream {
    pub total_size: u64,
    pub body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, ExportError>> + Send>,
    >,
}

/// Fetches and checks the first chunk before any response starts, so a
/// deleted conversation or an offline device still returns a typed failure.
/// Every later chunk is checked again and must keep the first chunk's
/// revision and size at the next offset; otherwise the stream ends with an
/// error and the client sees an incomplete download.
pub async fn start_export(
    fetch: FetchChunk,
    allowed: StillAllowed,
) -> Result<ExportStream, ExportError> {
    let first = fetch(0).await.map_err(ExportError::Request);
    if !allowed().await {
        return Err(ExportError::Rejected);
    }
    let first = chunk(first?, 0)?;
    let (revision, total_size) = (first.revision, first.total);
    let state = (Some(first), 0_u64, fetch, allowed);
    let body =
        futures_util::stream::unfold(state, move |(pending, next, fetch, allowed)| async move {
            if let Some(chunk) = pending {
                let next = chunk.offset + chunk.bytes.len() as u64;
                return Some((
                    Ok(bytes::Bytes::from(chunk.bytes)),
                    (None, next, fetch, allowed),
                ));
            }
            if next >= total_size {
                return None;
            }
            let reply = fetch(next).await;
            let checked = match reply {
                Ok(reply) if allowed().await => chunk(reply, next),
                Ok(_) => Err(ExportError::Rejected),
                Err(error) => Err(ExportError::Request(error)),
            };
            match checked {
                Ok(chunk) if chunk.revision == revision && chunk.total == total_size => {
                    let after = next + chunk.bytes.len() as u64;
                    Some((
                        Ok(bytes::Bytes::from(chunk.bytes)),
                        (None, after, fetch, allowed),
                    ))
                }
                Ok(_) | Err(_) => {
                    Some((Err(ExportError::Rejected), (None, u64::MAX, fetch, allowed)))
                }
            }
        });
    Ok(ExportStream {
        total_size,
        body: Box::pin(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn reply(revision: u64, total: u64, offset: u64, data: &[u8]) -> FileRecoveryReply {
        use base64::Engine as _;
        FileRecoveryReply {
            authority: "a".repeat(64),
            os_user: "501".into(),
            outcome: FileRecoveryOutcome::ExportChunk {
                package_revision: revision,
                total_size: total,
                offset,
                data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            },
        }
    }

    /// A device with a 10-byte package served 4 bytes at a time; the package
    /// revision changes when the read at `change_at` happens.
    fn device(change_at: Option<u64>) -> FetchChunk {
        let package = b"0123456789".to_vec();
        Box::new(move |offset| {
            let package = package.clone();
            Box::pin(async move {
                let end = (offset as usize + 4).min(package.len());
                let revision = if change_at.is_some_and(|at| offset >= at) {
                    2
                } else {
                    1
                };
                Ok(reply(revision, 10, offset, &package[offset as usize..end]))
            })
        })
    }

    fn allowed_while(limit: usize) -> StillAllowed {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Box::new(move || {
            let calls = calls.clone();
            Box::pin(async move { calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < limit })
        })
    }

    async fn collect(stream: ExportStream) -> (Vec<u8>, bool) {
        let mut body = stream.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(_) => return (bytes, false),
            }
        }
        (bytes, true)
    }

    #[tokio::test]
    async fn chunks_stream_in_order_and_stop_when_anything_changes() {
        let stream = start_export(device(None), allowed_while(usize::MAX))
            .await
            .unwrap();
        assert_eq!(stream.total_size, 10);
        assert_eq!(collect(stream).await, (b"0123456789".to_vec(), true));
        // A revision change between chunks ends the download incomplete.
        let stream = start_export(device(Some(4)), allowed_while(usize::MAX))
            .await
            .unwrap();
        assert_eq!(collect(stream).await, (b"0123".to_vec(), false));
        // So does a deleted conversation or a changed owner.
        let stream = start_export(device(None), allowed_while(2)).await.unwrap();
        assert_eq!(collect(stream).await, (b"01234567".to_vec(), false));
        // A failed check before the first chunk is a typed failure, no stream.
        assert!(matches!(
            start_export(device(None), allowed_while(0)).await,
            Err(ExportError::Rejected)
        ));
    }

    #[tokio::test]
    async fn the_first_chunk_reports_device_failures_and_rejects_bad_chunks() {
        let unavailable: FetchChunk = Box::new(|_| {
            Box::pin(async {
                Ok(FileRecoveryReply {
                    authority: "a".repeat(64),
                    os_user: "501".into(),
                    outcome: FileRecoveryOutcome::Unavailable {
                        reason: FileRecoveryFailure::MaterialExpired,
                    },
                })
            })
        });
        assert!(matches!(
            start_export(unavailable, allowed_while(usize::MAX)).await,
            Err(ExportError::Unavailable(
                FileRecoveryFailure::MaterialExpired
            ))
        ));
        for bad in [
            reply(1, 10, 4, b"0123"),
            reply(1, 2, 0, b"0123"),
            reply(1, MAX_EXPORT_PACKAGE_BYTES + 1, 0, b"0"),
        ] {
            let fetch: FetchChunk = Box::new(move |_| {
                let bad = bad.clone();
                Box::pin(async move { Ok(bad) })
            });
            assert!(matches!(
                start_export(fetch, allowed_while(usize::MAX)).await,
                Err(ExportError::Rejected)
            ));
        }
    }

    #[tokio::test]
    async fn private_reply_requires_exact_connection_and_cancellation_removes_waiter() {
        let hub = Arc::new(FileRecoveryHub::default());
        let (ticket, mut reply) = hub.register("device-connection").unwrap();
        let response = FileRecoveryReply {
            authority: "a".repeat(64),
            os_user: "501".into(),
            outcome: FileRecoveryOutcome::Deleted { complete: true },
        };
        hub.complete("wrong-connection", &ticket.id, response.clone());
        assert!(matches!(
            reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        hub.complete("device-connection", &ticket.id, response);
        assert!(reply.await.is_ok());
        assert!(hub.pending.lock().unwrap().is_empty());
        let (ticket, reply) = hub.register("device-connection").unwrap();
        drop(ticket);
        assert!(reply.await.is_err());
        assert!(hub.pending.lock().unwrap().is_empty());
    }
}
