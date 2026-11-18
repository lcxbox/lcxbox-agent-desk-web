//! "Revoke all sandbox labels" under ServiceDaemon (plan 6.5.3, part six):
//! the labels and their lease store belong to the session user, so the local
//! HTTP request is forwarded to that user's session worker (a resident one, or
//! the single legacy worker) and answered from its reply. Replies never enter
//! the signaling output lane.
use super::*;
use desk_ipc_protocol::message::{RevokeExecSandboxLabelsReply, RevokeExecSandboxLabelsRequest};

/// How long the HTTP request waits for the worker. Revoking walks every
/// labelled tree, which is bounded by what the user labelled.
const REVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

struct Pending {
    /// `None` for the legacy single worker.
    key: Option<WorkerKey>,
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
            let runs_as_user = |worker: &WorkerHandle| {
                worker.session_id == user.session_id
                    && file_recovery_quota::windows_worker_user(worker).as_deref()
                        == Some(user.sid.as_str())
            };
            let (key, worker) = inner
                .resident_workers
                .iter()
                .filter(|(key, worker)| {
                    key.desktop == DesktopTarget::WindowsDefault && runs_as_user(worker)
                })
                .max_by_key(|(_, worker)| worker.incarnation.0)
                .map(|(key, worker)| (Some(key.clone()), worker))
                .or_else(|| {
                    inner
                        .active_worker
                        .as_ref()
                        .filter(|worker| runs_as_user(worker))
                        .map(|worker| (None, worker))
                })
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
                        key,
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
        let Ok(mut pending) = self.exec_sandbox_revocations.0.lock() else {
            return;
        };
        let matches = pending
            .get(&reply.request_id)
            .is_some_and(|entry| entry.key.as_ref() == key && entry.incarnation == incarnation);
        if matches && let Some(entry) = pending.remove(&reply.request_id) {
            let _ = entry.tx.send(reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::RevokeExecSandboxLabelsReply;

    #[tokio::test]
    async fn the_legacy_worker_of_the_requesting_user_answers_a_revocation() {
        let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
            crate::model::settings::Settings::default(),
        )));
        let (manager, _messages) = WorkerManager::new(settings, PcRegistry::new());
        let user = crate::windows_local_user::LocalUser::of_this_process().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // An in-process task stands for a live worker running as this user.
        let alive = tokio::spawn(std::future::pending::<()>());
        manager.inner.lock().await.active_worker = Some(WorkerHandle {
            incarnation: WorkerIncarnation(7),
            pipe_name: "test-revoke".into(),
            ipc_tx: tx,
            process_handle: None,
            last_heartbeat_at: Instant::now(),
            capabilities: None,
            session_id: user.session_id,
            desktop_name: None,
            file_sender_tx: Arc::new(RwLock::new(None)),
            inprocess_task: Some(alive),
            inprocess_restart: None,
            lane_tasks: vec![],
        });
        let request = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.request_exec_sandbox_revoke(&user).await })
        };
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
        let Ok(Some(ServiceToWorker::RevokeExecSandboxLabels(sent))) = received else {
            panic!("the worker received no revocation");
        };
        let reply = |revoked| RevokeExecSandboxLabelsReply {
            request_id: sent.request_id.clone(),
            revoked,
            in_use: 0,
            error: None,
        };
        // Another incarnation's answer is ignored; the worker's own is taken.
        manager
            .complete_exec_sandbox_revoke(None, WorkerIncarnation(6), reply(9))
            .await;
        manager
            .complete_exec_sandbox_revoke(None, WorkerIncarnation(7), reply(2))
            .await;
        assert_eq!(request.await.unwrap(), Ok(reply(2)));
        if let Some(worker) = manager.inner.lock().await.active_worker.take() {
            worker.inprocess_task.unwrap().abort();
        }
    }
}
