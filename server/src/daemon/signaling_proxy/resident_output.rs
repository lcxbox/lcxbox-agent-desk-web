//! Which output of a resident session worker may leave the daemon. Resident
//! workers of different sessions run as different users, so a reply is
//! admitted only from the worker its request or connection belongs to. The
//! same rules apply to Linux and Windows resident workers.
use super::resident_global_control;
use crate::daemon::exec_capacity::ExecCapacity;
use crate::daemon::worker_manager::WorkerIncarnation;
use crate::daemon::worker_manager::resident_request_routes::ResidentRequestRoutes;
use desk_ipc_protocol::message::{WorkerKey, WorkerToService};
use desk_signal_facade::model::signal::SignalingType;

pub(super) fn resident_output_admitted(
    message: &WorkerToService,
    key: &WorkerKey,
    incarnation: WorkerIncarnation,
    owns_connection: impl Fn(&str) -> bool,
    exec_capacity: &ExecCapacity,
    requests: &ResidentRequestRoutes,
) -> bool {
    if resident_global_control(message) {
        return true;
    }
    match message {
        // Checked afterwards against the stream's bound worker.
        WorkerToService::ExecPtyOpened(_)
        | WorkerToService::ExecPtyOutput(_)
        | WorkerToService::ExecPtyClosed(_) => true,
        // Requests from the worker itself; their handlers verify its identity.
        WorkerToService::FileRecoveryQuotaRequested(_)
        | WorkerToService::StartApplicationHost(_)
        | WorkerToService::LocalFileRecoveryManaged(_) => true,
        // An error reply to a connection-less request names no connection.
        WorkerToService::SignalingError(payload) if payload.connection_id.is_empty() => requests
            .admit_reply(
                &payload.request_id,
                key,
                incarnation,
                payload.signaling_type,
            ),
        _ => match message.connection_id() {
            Some(connection_id) => owns_connection(connection_id),
            None => {
                connection_less_reply_admitted(message, key, incarnation, exec_capacity, requests)
            }
        },
    }
}

fn connection_less_reply_admitted(
    message: &WorkerToService,
    key: &WorkerKey,
    incarnation: WorkerIncarnation,
    exec_capacity: &ExecCapacity,
    requests: &ResidentRequestRoutes,
) -> bool {
    match message {
        WorkerToService::ExecSpawnReport(payload) => {
            central_execution_of(exec_capacity, &payload.request_id, key, incarnation)
        }
        WorkerToService::ExecHeartbeat(payload) => {
            central_execution_of(exec_capacity, &payload.request_id, key, incarnation)
        }
        WorkerToService::ExecutionCompleted(payload) => {
            central_execution_of(exec_capacity, &payload.request_id, key, incarnation)
        }
        WorkerToService::FileRecoveryManaged(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::FileRecoveryManaged,
        ),
        WorkerToService::TerminalCommandsListed(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::TerminalCommandsListed,
        ),
        WorkerToService::RemoteReadCompleted(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::RemoteToolOutputUpdated,
        ),
        WorkerToService::ComputerActionStarted(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::ComputerActionStarted,
        ),
        WorkerToService::ComputerActionCompleted(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::ComputerActionCompleted,
        ),
        // A stop request's answer, under the stop request's own id.
        WorkerToService::ComputerActionStateReported(payload) => requests.admit_reply(
            &payload.request_id,
            key,
            incarnation,
            SignalingType::ComputerActionStateReported,
        ),
        _ => false,
    }
}

/// A central execution dispatched to this worker. A daemon-run root PTY never
/// takes worker output, and a browser-owned execution answers on its
/// connection, so neither admits a connection-less reply.
fn central_execution_of(
    exec_capacity: &ExecCapacity,
    generation: &str,
    key: &WorkerKey,
    incarnation: WorkerIncarnation,
) -> bool {
    exec_capacity.binding(generation).is_some_and(|binding| {
        !binding.daemon_pty
            && binding.owner_connection_id.is_none()
            && binding.target.worker_key.as_ref() == Some(key)
            && binding.target.source_incarnation == incarnation
    })
}

#[cfg(test)]
#[path = "resident_output_tests.rs"]
mod tests;
