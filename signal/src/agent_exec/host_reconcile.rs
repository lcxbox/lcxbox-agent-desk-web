//! Background commands whose host reconnected or restarted. Their dispatch
//! connection is gone, so the device's current connection — the same device,
//! routed by its client id — answers for them, and a final answer settles the
//! task instead of leaving it running forever.

use super::*;
use crate::entity::agent_exec_task;

/// How often unsettled tasks are checked against their device's current host.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);
/// Tasks checked per pass, oldest first.
const RECONCILE_BATCH: u64 = 64;

/// The device's current host connection, when it is not the dispatch's.
async fn reconnected_host(
    connections: &SharedConnectionMap,
    task: &agent_exec_task::Model,
) -> Option<ConnectionState> {
    connections
        .read()
        .await
        .current_oss_server(&task.target_device_id)
        .filter(|state| {
            !state.lifecycle.is_retired() && state.model.connection_id != task.target_connection_id
        })
        .cloned()
}

/// Ask the device's current host about a task dispatched over a connection
/// that is gone, and record the answer when it is final. Returns the task when
/// it settled.
pub(super) async fn settle_on_reconnected_host(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    pending: &SignalAgentExecPending,
    task: &agent_exec_task::Model,
) -> Result<Option<agent_exec_task::Model>, AgentError> {
    let Some(host) = reconnected_host(connections, task).await else {
        return Ok(None);
    };
    let Some(reply) = query_state_on(pending, &host, &task.execution_generation).await? else {
        return Ok(None);
    };
    let Some(disposition) = EdgeExecDisposition::settle_from_host_reply(&reply, true) else {
        return Ok(None);
    };
    Ok(
        crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
            .finalize_from_reconnected_host(
                &task.target_device_id,
                &host.model.connection_id,
                &task.execution_generation,
                &disposition,
            )
            .await?
            .filter(|row| row.status == crate::agent_exec_store::STATUS_DONE),
    )
}

/// One pass over unsettled tasks. A task whose host cannot be reached now is
/// left for a later pass.
pub(super) async fn reconcile_once(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    pending: &SignalAgentExecPending,
) -> Result<usize, AgentError> {
    let tasks = crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
        .unsettled(RECONCILE_BATCH)
        .await?;
    let mut settled = 0;
    for task in &tasks {
        match settle_on_reconnected_host(db, connections, pending, task).await {
            Ok(Some(_)) => settled += 1,
            Ok(None) => {}
            Err(error) => log::debug!(
                "[agent-exec] reconnected host did not answer for {}: {}",
                task.execution_generation,
                error.message
            ),
        }
    }
    Ok(settled)
}

/// Settle background commands left by a host that reconnected, restarted or
/// was reinstalled, including after this signal itself restarted.
pub async fn run_reconnected_host_reconciler(
    db: DatabaseConnection,
    connections: actix_web::web::Data<SharedConnectionMap>,
) {
    let pending = global_agent_exec_pending();
    loop {
        tokio::time::sleep(RECONCILE_INTERVAL).await;
        match reconcile_once(&db, &connections, &pending).await {
            Ok(0) => {}
            Ok(settled) => log::info!(
                "[agent-exec] settled {settled} background command(s) left by a reconnected host"
            ),
            Err(error) => log::warn!(
                "[agent-exec] reconnected host reconciliation failed: {}",
                error.message
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::exec_lifecycle::ExecState;

    async fn fixture() -> (
        DatabaseConnection,
        crate::agent_exec_store::SignalAgentExecStore,
    ) {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
        store
            .create(
                "exec-1",
                "generation-1",
                "conversation-1",
                "call-1",
                "host-old",
                "device-1",
                desk_agent_data_store::datetime::utc_now()
                    + desk_agent_data_store::datetime::database_interval(
                        jiff::SignedDuration::from_mins(5),
                    ),
            )
            .await
            .unwrap();
        (db, store)
    }

    async fn host(connections: &SharedConnectionMap, connection_id: &str, device: &str) {
        let state =
            crate::command_policy::tests::owner_host(connection_id, device, &["bash"], None).await;
        connections
            .write()
            .await
            .insert(connection_id.into(), state);
    }

    /// Answer the next state query of `generation` as `connection_id`.
    fn answer(pending: Arc<SignalAgentExecPending>, connection_id: &'static str, state: ExecState) {
        actix_web::rt::spawn(async move {
            for _ in 0..200 {
                let reply = ExecStateReplyPayload {
                    execution_generation: "generation-1".into(),
                    state,
                    containment_identity: None,
                    running_ms: None,
                    detail: None,
                    result_json: None,
                };
                if pending.deliver_state_reply(connection_id, reply) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }

    /// A host that reconnected after a restart has no record of the command:
    /// the task settles as an unknown outcome, never success or failure, once.
    #[actix_web::test]
    async fn a_reconnected_host_settles_a_lost_command_once() {
        let (db, store) = fixture().await;
        let connections = SharedConnectionMap::new_oss();
        host(&connections, "host-new", "device-1").await;
        let pending = Arc::new(SignalAgentExecPending::new());
        answer(pending.clone(), "host-new", ExecState::Unknown);

        assert_eq!(
            reconcile_once(&db, &connections, &pending).await.unwrap(),
            1
        );
        let task = store.find("exec-1", "generation-1").await.unwrap().unwrap();
        assert_eq!(task.status, crate::agent_exec_store::STATUS_DONE);
        let result = task.result_text.unwrap();
        assert!(
            result.contains(EdgeExecDisposition::OUTCOME_UNAVAILABLE_MESSAGE),
            "{result}"
        );
        // Nothing is left to settle; a late real result cannot overwrite it.
        assert_eq!(
            reconcile_once(&db, &connections, &pending).await.unwrap(),
            0
        );
        let late = EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "late".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        };
        store
            .finalize("host-old", "generation-1", &late)
            .await
            .unwrap();
        let task = store.find("exec-1", "generation-1").await.unwrap().unwrap();
        assert!(
            task.result_text
                .unwrap()
                .contains(EdgeExecDisposition::OUTCOME_UNAVAILABLE_MESSAGE)
        );
    }

    /// The dispatch connection itself, another device's host and a command
    /// still running never settle a task here.
    #[actix_web::test]
    async fn only_a_reconnected_host_of_the_same_device_with_a_final_answer_settles() {
        let (db, store) = fixture().await;
        let connections = SharedConnectionMap::new_oss();
        let pending = Arc::new(SignalAgentExecPending::new());
        host(&connections, "host-old", "device-1").await;
        assert_eq!(
            reconcile_once(&db, &connections, &pending).await.unwrap(),
            0
        );

        let connections = SharedConnectionMap::new_oss();
        host(&connections, "host-other", "device-2").await;
        assert_eq!(
            reconcile_once(&db, &connections, &pending).await.unwrap(),
            0
        );

        let connections = SharedConnectionMap::new_oss();
        host(&connections, "host-new", "device-1").await;
        answer(pending.clone(), "host-new", ExecState::Running);
        assert_eq!(
            reconcile_once(&db, &connections, &pending).await.unwrap(),
            0
        );
        let task = store.find("exec-1", "generation-1").await.unwrap().unwrap();
        assert_eq!(task.status, crate::agent_exec_store::STATUS_DISPATCHING);
    }
}
