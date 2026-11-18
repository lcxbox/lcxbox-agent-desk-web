//! Central work names no desktop session: with resident workers only the single
//! Assistant-ready session takes it, and a stop request reaches only the worker
//! still running its action.
use super::resident_request_routes::ExpectedReply;
use super::*;
use crate::daemon::session_target::SessionCandidate;
use desk_ipc_protocol::message::{
    ComputerActionCancelPayload, RemoteReadRequestPayload, SessionKey,
};
use desk_signal_facade::model::signal::SignalingType;

fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    WorkerManager::new(settings, PcRegistry::new())
}

fn key(name: &str) -> WorkerKey {
    WorkerKey {
        session: SessionKey {
            platform_session_id: name.to_string(),
            session_generation: 1,
        },
        desktop: DesktopTarget::LinuxSession,
    }
}

fn assistant_ready(manager: &WorkerManager, key: &WorkerKey) {
    manager.session_targets.upsert(SessionCandidate {
        session: key.session.clone(),
        display_name: key.session.platform_session_id.clone(),
        session_type: Some("wayland".to_string()),
        seat: None,
        foreground: true,
        remote_desktop_ready: false,
        terminal_ready: false,
        file_ready: false,
        assistant_ready: true,
    });
}

fn read(request_id: &str) -> ServiceToWorker {
    use desk_agent_protocol::*;
    let envelope = AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        request_id: RequestId(request_id.into()),
        parent_task_id: None,
        target: TargetRef::default(),
        actor: ActorRef {
            actor_type: ActorType::System,
            actor_id: "central".into(),
        },
        caller: CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: None,
            model_name: None,
            adapter: None,
        },
        scope: AgentScope {
            granted: vec![Capability::SystemInfo],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::SystemInfo(SystemInfoParams::default()),
            }),
        },
        audit: AuditMeta {
            approval_id: None,
            reason: None,
        },
    };
    ServiceToWorker::InvokeRemoteRead(RemoteReadRequestPayload {
        request_id: request_id.into(),
        envelope: envelope.try_into().unwrap(),
    })
}

fn cancel(request_id: &str, generation: &str) -> ServiceToWorker {
    ServiceToWorker::ComputerActionCancel(ComputerActionCancelPayload {
        request_id: request_id.into(),
        connection_id: None,
        approved_actor_id: "1".into(),
        cancel: desk_agent_protocol::computer_use::ComputerActionCancel {
            work_id: "work".into(),
            action_request_id: "action".into(),
            execution_generation: generation.into(),
            reason: "stop".into(),
        },
    })
}

#[tokio::test]
async fn the_single_ready_session_takes_central_work_and_only_its_worker_answers() {
    let (manager, _messages) = test_manager();
    manager.enable_session_targeting_for_test();
    let only = key("only");
    assistant_ready(&manager, &only);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let incarnation = manager.install_resident_for_test(only.clone(), tx).await;

    manager
        .send_to_central_worker(read("read-1"))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(ServiceToWorker::InvokeRemoteRead(payload)) if payload.request_id == "read-1"
    ));
    let routes = manager.resident_requests();
    assert!(!routes.admit_reply(
        "read-1",
        &key("other"),
        incarnation,
        SignalingType::RemoteToolOutputUpdated
    ));
    assert!(routes.admit_reply(
        "read-1",
        &only,
        incarnation,
        SignalingType::RemoteToolOutputUpdated
    ));
}

#[tokio::test]
async fn several_ready_sessions_are_refused_rather_than_guessed_between() {
    let (manager, _messages) = test_manager();
    manager.enable_session_targeting_for_test();
    let mut receivers = Vec::new();
    for name in ["first", "second"] {
        assistant_ready(&manager, &key(name));
        let (tx, rx) = mpsc::unbounded_channel();
        manager.install_resident_for_test(key(name), tx).await;
        receivers.push(rx);
    }
    let error = manager
        .send_to_central_worker(read("read-1"))
        .await
        .unwrap_err();
    assert!(error.contains("several desktop sessions"), "{error}");
    for mut rx in receivers {
        assert!(rx.try_recv().is_err());
    }
    // Nothing was sent, so nothing may answer.
    assert!(!manager.resident_requests().admit_reply(
        "read-1",
        &key("first"),
        WorkerIncarnation(1),
        SignalingType::RemoteToolOutputUpdated
    ));
}

#[tokio::test]
async fn a_stop_request_reaches_only_the_worker_still_running_its_action() {
    let (manager, _messages) = test_manager();
    manager.enable_session_targeting_for_test();
    let only = key("only");
    assistant_ready(&manager, &only);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let incarnation = manager.install_resident_for_test(only.clone(), tx).await;
    let action = ExpectedReply {
        terminal: SignalingType::ComputerActionCompleted,
        interim: Some(SignalingType::ComputerActionStarted),
    };
    manager
        .resident_requests
        .record("gen-1", only.clone(), incarnation, action);

    manager
        .send_to_central_worker(cancel("stop-1", "gen-1"))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(ServiceToWorker::ComputerActionCancel(payload)) if payload.request_id == "stop-1"
    ));
    assert!(manager.resident_requests().admit_reply(
        "stop-1",
        &only,
        incarnation,
        SignalingType::ComputerActionStateReported
    ));

    // A stop request for an action nobody dispatched goes nowhere.
    assert!(
        manager
            .send_to_central_worker(cancel("stop-2", "gen-unknown"))
            .await
            .is_err()
    );
    assert!(rx.try_recv().is_err());

    // The action's worker was replaced: its successor must not get the stop.
    let (successor_tx, mut successor_rx) = mpsc::unbounded_channel();
    manager
        .install_resident_for_test(only.clone(), successor_tx)
        .await;
    assert!(
        manager
            .send_to_central_worker(cancel("stop-3", "gen-1"))
            .await
            .is_err()
    );
    assert!(successor_rx.try_recv().is_err());
}

#[tokio::test]
async fn without_resident_workers_the_single_worker_takes_central_work() {
    let (manager, _messages) = test_manager();
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.install_active_for_test(tx).await;
    manager
        .send_to_central_worker(read("read-1"))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(ServiceToWorker::InvokeRemoteRead(_))
    ));
    manager
        .send_central_or_connection_worker(None, cancel("stop-1", "gen-1"))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(ServiceToWorker::ComputerActionCancel(_))
    ));
}
