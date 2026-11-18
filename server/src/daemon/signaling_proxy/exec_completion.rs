//! How a central execution is answered: with the worker's own result, or, when
//! the worker instance running it is gone, with an unknown outcome. Every
//! answer first takes the dispatch's correlation, so a central hears exactly
//! one answer per dispatch whichever of them comes first.
use super::take_edge_exec_correlation;
use crate::daemon::exec_ledger::Terminal;
use crate::daemon::exec_pty_carrier::ExecPtyCarrierRegistry;
use crate::daemon::signaling_router::{self, RouterContext};
use crate::daemon::worker_manager::RetiredWorker;
use desk_agent_protocol::AgentOutcome;
use desk_agent_protocol::audit::AuditEvent;
use desk_agent_protocol::edge_exec::EdgeExecDisposition;
use desk_ipc_protocol::message::ExecResultIpcPayload;
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use log::{error, warn};

/// Relay a worker's `ExecutionCompleted`. A central execution goes back as an
/// `EdgeExecutionCompleted`, a browser-owned one as `ExecutionCompleted(609)`
/// on its connection. A connection-less result whose correlation is already
/// gone was answered for elsewhere and is dropped.
pub(super) async fn relay_execution_completed(
    router_ctx: &RouterContext,
    payload: ExecResultIpcPayload,
) {
    let central = take_edge_exec_correlation(&router_ctx.edge_exec_pending, &payload.request_id);
    if !central && payload.connection_id.is_none() {
        warn!(
            "[exec] dropping a result for {} that was already answered",
            payload.request_id
        );
        return;
    }
    // Close out the ledger entry first, so the host's own record is settled
    // before the answer leaves the machine. The generation is the frame id the
    // plan was dispatched under.
    let result_json =
        serde_json::to_string(&payload.result.outcome).unwrap_or_else(|_| "null".to_string());
    if let Err(e) = router_ctx
        .exec_ledger
        .mark_terminal(&payload.request_id, Terminal::Completed(result_json))
        .await
    {
        // The command did run; failing to record that is bad but withholding
        // the result would be worse, so log and relay.
        error!(
            "[exec-ledger] could not record the result of {}: {e}",
            payload.request_id
        );
    }
    // The execution is accounted for; free its slot for the next one.
    router_ctx.exec_capacity.release(&payload.request_id);
    // The summary is content-free (exit code / error kind only), never stdout.
    let (success, summary, redactions) = match &payload.result.outcome {
        AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(o)) => {
            (o.succeeded(), o.audit_summary(), o.redactions.len() as i32)
        }
        AgentOutcome::Ok(_) => (true, "ok".to_string(), 0),
        AgentOutcome::Err(e) => (false, format!("{:?}", e.kind), 0),
    };
    router_ctx
        .audit
        .record(
            AuditEvent::command_completed(
                uuid::Uuid::new_v4().to_string(),
                chrono::Utc::now().to_rfc3339(),
                &payload.result.exec_request_id.0,
                success,
                summary,
                redactions,
                0,
            )
            // The worker echoes the originating frame request_id (the manager's
            // ledger key) so the completion is attributed to the real operator.
            .with_task_id(payload.audit_source_request_id.as_deref()),
        )
        .await;
    if central {
        signaling_router::send_edge_execution_completed(
            &router_ctx.outbound_tx,
            &payload.request_id,
            EdgeExecDisposition::Executed {
                outcome: payload.result.outcome,
            },
        );
        return;
    }
    let value = match serde_json::to_value(&payload.result) {
        Ok(value) => value,
        Err(e) => {
            warn!(
                "[SignalingProxy] Failed to serialise ExecResultPayload for {:?}: {e} \
                 (request_id={})",
                payload.connection_id, payload.request_id,
            );
            return;
        }
    };
    let frame = SignalingModel::new(
        &payload.request_id,
        SignalingType::ExecutionCompleted,
        None,
        payload.connection_id.clone(),
        Some(value),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = router_ctx.outbound_tx.send(text);
        }
        Err(e) => warn!(
            "[SignalingProxy] Failed to serialise ExecutionCompleted frame for {:?}: {e} \
             (request_id={})",
            payload.connection_id, payload.request_id,
        ),
    }
}

/// Answer for the central executions a departed worker instance was running.
///
/// The command may still be running or may have finished, so each one settles
/// as an unknown outcome: the ledger records it as indeterminate, the central
/// receives the same frame a real result travels on, and an interactive stream
/// is closed toward the central end. The slots stay until their deadlines (see
/// [`crate::daemon::exec_capacity::ExecCapacity::take_bound_to`]).
pub(super) async fn settle_retired_worker(
    router_ctx: &RouterContext,
    exec_pty_carriers: &ExecPtyCarrierRegistry,
    retired: RetiredWorker,
) {
    let generations = router_ctx
        .exec_capacity
        .take_bound_to(retired.worker_key.as_ref(), retired.incarnation);
    for generation in generations {
        if !take_edge_exec_correlation(&router_ctx.edge_exec_pending, &generation) {
            continue;
        }
        warn!(
            "[exec] worker {} exited before reporting {generation}; its outcome is unknown",
            retired.incarnation
        );
        if let Err(e) = router_ctx
            .exec_ledger
            .mark_terminal(&generation, Terminal::Indeterminate)
            .await
        {
            error!("[exec-ledger] could not record the lost result of {generation}: {e}");
        }
        signaling_router::send_edge_execution_completed(
            &router_ctx.outbound_tx,
            &generation,
            EdgeExecDisposition::outcome_lost_with_worker(),
        );
        exec_pty_carriers.close_lost_worker_execution(
            &generation,
            retired.worker_key.as_ref(),
            retired.incarnation,
        );
        // The ledger's task id is the plan's exec_request_id.
        let exec_request_id = match router_ctx.exec_ledger.get(&generation).await {
            Ok(Some(entry)) => entry.task_id,
            Ok(None) => generation.clone(),
            Err(e) => {
                error!("[exec-ledger] could not read {generation} for its audit: {e}");
                generation.clone()
            }
        };
        router_ctx
            .audit
            .record(
                AuditEvent::command_completed(
                    uuid::Uuid::new_v4().to_string(),
                    chrono::Utc::now().to_rfc3339(),
                    &exec_request_id,
                    false,
                    "outcome_unknown:worker_exited".to_string(),
                    0,
                    0,
                )
                .with_task_id(Some(&generation)),
            )
            .await;
    }
}

#[cfg(test)]
#[path = "exec_completion_tests.rs"]
mod tests;
