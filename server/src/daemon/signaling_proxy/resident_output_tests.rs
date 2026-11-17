use super::*;
use crate::daemon::exec_capacity::ExecBinding;
use crate::daemon::worker_manager::ExecWorkerTarget;
use desk_ipc_protocol::message::{
    DesktopTarget, ExecHeartbeatPayload, ExecSpawnReport, ExecSpawnReportPayload,
    FileRecoveryQuotaCommand, FileRecoveryQuotaRequest, SessionKey, SignalingErrorPayload,
    TerminalCommandsListedPayload,
};
use std::time::Duration;

fn key(session: &str) -> WorkerKey {
    WorkerKey {
        session: SessionKey {
            platform_session_id: session.into(),
            session_generation: 1,
        },
        desktop: DesktopTarget::LinuxSession,
    }
}

const INCARNATION: u64 = 5;

fn incarnation() -> WorkerIncarnation {
    WorkerIncarnation::for_test(INCARNATION)
}

fn admitted(
    message: &WorkerToService,
    from: &WorkerKey,
    capacity: &ExecCapacity,
    requests: &ResidentRequestRoutes,
) -> bool {
    resident_output_admitted(
        message,
        from,
        incarnation(),
        |connection| connection == "owned",
        capacity,
        requests,
    )
}

fn bound(
    capacity: &ExecCapacity,
    generation: &str,
    worker: &WorkerKey,
    owner: Option<&str>,
    daemon_pty: bool,
) {
    capacity
        .try_admit(generation, 8, Duration::from_secs(60))
        .unwrap();
    capacity
        .bind(
            generation,
            ExecBinding {
                target: ExecWorkerTarget {
                    worker_key: Some(worker.clone()),
                    source_incarnation: incarnation(),
                    session_target_id: worker.session.platform_session_id.clone(),
                    registration_generation: 1,
                    wire_worker_incarnation: INCARNATION,
                },
                owner_connection_id: owner.map(str::to_owned),
                daemon_pty,
            },
        )
        .unwrap();
}

fn heartbeat(generation: &str) -> WorkerToService {
    WorkerToService::ExecHeartbeat(ExecHeartbeatPayload {
        request_id: generation.into(),
        connection_id: None,
        running_ms: 1,
    })
}

fn spawn_failed(generation: &str) -> WorkerToService {
    WorkerToService::ExecSpawnReport(ExecSpawnReportPayload {
        request_id: generation.into(),
        connection_id: None,
        report: ExecSpawnReport::Failed {
            reason: "refused".into(),
        },
    })
}

#[test]
fn central_execution_output_comes_only_from_its_dispatched_worker() {
    let capacity = ExecCapacity::new();
    let requests = ResidentRequestRoutes::default();
    bound(&capacity, "g1", &key("a"), None, false);
    assert!(admitted(&heartbeat("g1"), &key("a"), &capacity, &requests));
    assert!(!admitted(&heartbeat("g1"), &key("b"), &capacity, &requests));
    assert!(!resident_output_admitted(
        &heartbeat("g1"),
        &key("a"),
        WorkerIncarnation::for_test(INCARNATION + 1),
        |_| true,
        &capacity,
        &requests,
    ));
    assert!(!admitted(
        &heartbeat("unbound"),
        &key("a"),
        &capacity,
        &requests
    ));
}

#[test]
fn a_failed_spawn_still_admits_the_result_that_follows() {
    let capacity = ExecCapacity::new();
    let requests = ResidentRequestRoutes::default();
    bound(&capacity, "g1", &key("a"), None, false);
    assert!(admitted(
        &spawn_failed("g1"),
        &key("a"),
        &capacity,
        &requests
    ));
    // Admission does not consume the binding; the result after it still passes.
    assert!(admitted(&heartbeat("g1"), &key("a"), &capacity, &requests));
}

#[test]
fn root_pty_and_browser_owned_executions_take_no_connection_less_worker_output() {
    let capacity = ExecCapacity::new();
    let requests = ResidentRequestRoutes::default();
    bound(&capacity, "root", &key("a"), None, true);
    bound(&capacity, "browser", &key("a"), Some("owned"), false);
    assert!(!admitted(
        &spawn_failed("root"),
        &key("a"),
        &capacity,
        &requests
    ));
    assert!(!admitted(
        &heartbeat("browser"),
        &key("a"),
        &capacity,
        &requests
    ));
}

#[test]
fn connection_less_replies_need_the_recorded_route_and_type() {
    let capacity = ExecCapacity::new();
    let requests = ResidentRequestRoutes::default();
    let listed = |request_id: &str| {
        WorkerToService::TerminalCommandsListed(TerminalCommandsListedPayload {
            request_id: request_id.into(),
            connection_id: None,
            terminals: desk_signal_facade::model::terminal::TerminalList {
                commands: vec![],
                current: 0,
            },
        })
    };
    let error = |request_id: &str, signaling_type| {
        WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: request_id.into(),
            connection_id: String::new(),
            signaling_type,
            error_code: 1,
            error_message: None,
        })
    };
    let record = |request_id: &str| {
        crate::daemon::worker_manager::resident_request_routes::record_for_test(
            &requests,
            request_id,
            key("a"),
            incarnation(),
            SignalingType::TerminalCommandsListed,
        )
    };
    assert!(!admitted(&listed("t1"), &key("a"), &capacity, &requests));
    record("t1");
    assert!(!admitted(&listed("t1"), &key("b"), &capacity, &requests));
    assert!(admitted(&listed("t1"), &key("a"), &capacity, &requests));
    // Answered once.
    assert!(!admitted(&listed("t1"), &key("a"), &capacity, &requests));

    record("t2");
    assert!(!admitted(
        &error("t2", SignalingType::FileRecoveryManaged),
        &key("a"),
        &capacity,
        &requests
    ));
    assert!(admitted(
        &error("t2", SignalingType::TerminalCommandsListed),
        &key("a"),
        &capacity,
        &requests
    ));
}

#[test]
fn connection_output_and_worker_requests_keep_their_own_checks() {
    let capacity = ExecCapacity::new();
    let requests = ResidentRequestRoutes::default();
    let browser_list = |connection: &str| {
        WorkerToService::TerminalCommandsListed(TerminalCommandsListedPayload {
            request_id: "t".into(),
            connection_id: Some(connection.into()),
            terminals: desk_signal_facade::model::terminal::TerminalList {
                commands: vec![],
                current: 0,
            },
        })
    };
    assert!(admitted(
        &browser_list("owned"),
        &key("a"),
        &capacity,
        &requests
    ));
    assert!(!admitted(
        &browser_list("other"),
        &key("a"),
        &capacity,
        &requests
    ));
    let quota = WorkerToService::FileRecoveryQuotaRequested(FileRecoveryQuotaRequest {
        request_id: "q".into(),
        os_user: "1000".into(),
        command: FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch: 1 },
    });
    assert!(admitted(&quota, &key("a"), &capacity, &requests));
}
