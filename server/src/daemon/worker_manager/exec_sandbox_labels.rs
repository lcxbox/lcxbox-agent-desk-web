//! "Revoke all sandbox labels" under ServiceDaemon (plan 6.5.3, part six):
//! the labels and their lease store belong to the session user, so the local
//! HTTP request is forwarded to that user's session worker and answered from
//! its reply. Replies never enter the signaling output lane.
use super::*;
use desk_ipc_protocol::message::{RevokeExecSandboxLabelsReply, RevokeExecSandboxLabelsRequest};

/// How long the HTTP request waits for the worker. Revoking walks every
/// labelled tree, which is bounded by what the user labelled.
const REVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

struct Pending {
    key: WorkerKey,
    incarnation: WorkerIncarnation,
    tx: oneshot::Sender<RevokeExecSandboxLabelsReply>,
}

#[derive(Clone, Default)]
pub(super) struct PendingRevocations(Arc<StdMutex<HashMap<String, Pending>>>);

struct RemovePending {
    requests: PendingRevocations,
    id: String,
}
impl Drop for RemovePending {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.requests.0.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Why a forwarded revocation did not produce a worker answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RevokeForwardError {
    /// No live session worker runs as this user.
    WorkerUnavailable,
    /// Too many revocations are already waiting.
    Busy,
    /// The worker did not answer in time.
    TimedOut,
    /// The requesting process ended or left its session.
    IdentityChanged,
}

impl WorkerManager {
    pub(crate) async fn request_exec_sandbox_revoke(
        &self,
        user: &crate::windows_local_user::LocalUser,
    ) -> Result<RevokeExecSandboxLabelsReply, RevokeForwardError> {
        if !user.is_alive() {
            return Err(RevokeForwardError::IdentityChanged);
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let cleanup = RemovePending {
            requests: self.exec_sandbox_revocations.clone(),
            id: request_id.clone(),
        };
        let (tx, rx) = oneshot::channel();
        {
            let inner = self.inner.lock().await;
            let (key, worker) = inner
                .resident_workers
                .iter()
                .filter(|(key, worker)| {
                    key.desktop == DesktopTarget::WindowsDefault
                        && worker.session_id == user.session_id
                        && file_recovery_quota::windows_worker_user(worker).as_deref()
                            == Some(user.sid.as_str())
                })
                .max_by_key(|(_, worker)| worker.incarnation.0)
                .ok_or(RevokeForwardError::WorkerUnavailable)?;
            {
                let mut pending = self
                    .exec_sandbox_revocations
                    .0
                    .lock()
                    .map_err(|_| RevokeForwardError::WorkerUnavailable)?;
                if pending.len() >= 4 {
                    return Err(RevokeForwardError::Busy);
                }
                pending.insert(
                    request_id.clone(),
                    Pending {
                        key: key.clone(),
                        incarnation: worker.incarnation,
                        tx,
                    },
                );
            }
            worker
                .ipc_tx
                .send(ServiceToWorker::RevokeExecSandboxLabels(
                    RevokeExecSandboxLabelsRequest {
                        request_id,
                        os_user: user.sid.clone(),
                    },
                ))
                .map_err(|_| RevokeForwardError::WorkerUnavailable)?;
        }
        let reply = tokio::time::timeout(REVOKE_TIMEOUT, rx)
            .await
            .map_err(|_| RevokeForwardError::TimedOut)?
            .map_err(|_| RevokeForwardError::WorkerUnavailable)?;
        drop(cleanup);
        Ok(reply)
    }

    /// Hand a worker's reply to the waiting request, only when it comes from
    /// the worker incarnation the request was sent to.
    pub(crate) async fn complete_exec_sandbox_revoke(
        &self,
        key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        reply: RevokeExecSandboxLabelsReply,
    ) {
        let Some(key) = key else { return };
        let Ok(mut pending) = self.exec_sandbox_revocations.0.lock() else {
            return;
        };
        let matches = pending
            .get(&reply.request_id)
            .is_some_and(|entry| &entry.key == key && entry.incarnation == incarnation);
        if matches && let Some(entry) = pending.remove(&reply.request_id) {
            let _ = entry.tx.send(reply);
        }
    }
}
