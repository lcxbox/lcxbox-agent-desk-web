use super::*;
use crate::daemon::exec_capacity::ExecBinding;
use crate::daemon::exec_ledger::State;
use crate::daemon::worker_manager::{ExecWorkerTarget, RetiredWorkerReceiver, WorkerIncarnation};
use desk_agent_protocol::edge_exec::EdgeExecResultPayload;
use desk_agent_protocol::exec::{ExecRequestId, ExecResultPayload};
use desk_ipc_protocol::message::{DesktopTarget, SessionKey, WorkerKey};
use std::time::Duration;
use tokio::sync::broadcast;

const SLOT_TIMEOUT: Duration = Duration::from_secs(60);

fn worker_key(session: &str) -> WorkerKey {
    WorkerKey {
        session: SessionKey {
            platform_session_id: session.into(),
            session_generation: 1,
        },
        desktop: DesktopTarget::LinuxSession,
    }
}

fn central_binding(key: Option<&WorkerKey>, incarnation: WorkerIncarnation) -> ExecBinding {
    ExecBinding {
        target: ExecWorkerTarget {
            worker_key: key.cloned(),
            source_incarnation: incarnation,
            session_target_id: key.map_or("legacy".into(), |key| {
                key.session.platform_session_id.clone()
            }),
            registration_generation: 1,
            wire_worker_incarnation: incarnation.get(),
        },
        owner_connection_id: None,
        daemon_pty: false,
    }
}

/// Admit, bind and start one execution the way a dispatch does, registering
/// the central correlation when `central` is set.
async fn running_execution(
    ctx: &RouterContext,
    generation: &str,
    binding: ExecBinding,
    central: bool,
) {
    assert!(
        ctx.exec_capacity
            .try_admit(generation, 16, SLOT_TIMEOUT)
            .unwrap()
    );
    ctx.exec_capacity.bind(generation, binding).unwrap();
    ctx.exec_ledger
        .reserve(&format!("task-{generation}"), generation, "fp", None)
        .await
        .unwrap();
    assert!(
        ctx.exec_ledger
            .mark_running(generation, None)
            .await
            .unwrap()
    );
    if central {
        ctx.edge_exec_pending
            .lock()
            .unwrap()
            .insert(generation.to_string());
    }
}

async fn ledger_state(ctx: &RouterContext, generation: &str) -> String {
    ctx.exec_ledger
        .get(generation)
        .await
        .unwrap()
        .expect("ledger row")
        .state
}

async fn next_retired(rx: &mut RetiredWorkerReceiver) -> RetiredWorker {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the removal path must announce the departed worker")
        .expect("the manager keeps the sender")
}

fn frames(rx: &mut broadcast::Receiver<String>) -> Vec<SignalingModel> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|text| serde_json::from_str(&text).unwrap())
        .collect()
}

fn edge_result(frame: &SignalingModel) -> EdgeExecResultPayload {
    assert_eq!(frame.signaling_type, SignalingType::EdgeExecutionCompleted);
    frame.get_data::<EdgeExecResultPayload>().unwrap()
}

fn worker_result(generation: &str, connection_id: Option<&str>) -> ExecResultIpcPayload {
    ExecResultIpcPayload {
        request_id: generation.into(),
        connection_id: connection_id.map(str::to_string),
        result: ExecResultPayload {
            exec_request_id: ExecRequestId(format!("task-{generation}")),
            outcome: AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(
                desk_agent_protocol::ExecOutput {
                    started: true,
                    exit_code: Some(0),
                    termination_signal: None,
                    failure: None,
                    diagnostics: Vec::new(),
                    streams: desk_agent_protocol::ExecOutputStreams::Split {
                        stdout: String::new(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    },
                    duration_ms: 1,
                    redactions: Vec::new(),
                    file_changes: None,
                },
            )),
        },
        audit_source_request_id: Some(generation.into()),
    }
}

/// A resident worker that fails is removed through the real recovery path,
/// and only the central executions of that worker instance settle: once, as
/// an unknown outcome, with their slots kept until their deadlines.
#[tokio::test]
async fn a_failed_resident_worker_settles_only_its_own_central_executions() {
    let (ctx, _) = super::super::tests::make_router_ctx().await;
    let mut retired_rx = ctx.worker_mgr.take_retired_worker_receiver().unwrap();
    let carriers = ExecPtyCarrierRegistry::new();
    let failed = worker_key("session-1");
    let healthy = worker_key("session-2");
    let (tx, _worker_rx) = tokio::sync::mpsc::unbounded_channel();
    let failed_incarnation = ctx
        .worker_mgr
        .install_resident_for_test(failed.clone(), tx.clone())
        .await;
    let healthy_incarnation = ctx
        .worker_mgr
        .install_resident_for_test(healthy.clone(), tx)
        .await;
    let earlier_incarnation = WorkerIncarnation::for_test(failed_incarnation.get() + 100);

    let lost = central_binding(Some(&failed), failed_incarnation);
    let mut root_pty = lost.clone();
    root_pty.daemon_pty = true;
    let mut browser = lost.clone();
    browser.owner_connection_id = Some("browser".into());
    running_execution(&ctx, "lost", lost, true).await;
    running_execution(&ctx, "root-pty", root_pty, true).await;
    running_execution(&ctx, "browser", browser, false).await;
    running_execution(
        &ctx,
        "other-worker",
        central_binding(Some(&healthy), healthy_incarnation),
        true,
    )
    .await;
    running_execution(
        &ctx,
        "other-incarnation",
        central_binding(Some(&failed), earlier_incarnation),
        true,
    )
    .await;
    running_execution(
        &ctx,
        "legacy",
        central_binding(None, failed_incarnation),
        true,
    )
    .await;
    let mut outbound = ctx.outbound_tx.subscribe();

    ctx.worker_mgr.handle_worker_transport_failure(
        Some(failed.clone()),
        failed_incarnation,
        0,
        None,
    );
    let retired = next_retired(&mut retired_rx).await;
    assert_eq!(
        retired,
        RetiredWorker {
            worker_key: Some(failed.clone()),
            incarnation: failed_incarnation,
        }
    );
    settle_retired_worker(&ctx, &carriers, retired.clone()).await;

    let sent = frames(&mut outbound);
    assert_eq!(sent.len(), 1, "exactly one answer leaves the device");
    assert_eq!(
        edge_result(&sent[0]),
        EdgeExecResultPayload {
            request_id: "lost".into(),
            disposition: EdgeExecDisposition::outcome_lost_with_worker(),
        }
    );
    assert_eq!(
        ledger_state(&ctx, "lost").await,
        State::Indeterminate.as_str()
    );
    for untouched in [
        "root-pty",
        "browser",
        "other-worker",
        "other-incarnation",
        "legacy",
    ] {
        assert_eq!(
            ledger_state(&ctx, untouched).await,
            State::Running.as_str(),
            "{untouched}"
        );
        assert!(
            ctx.exec_capacity.binding(untouched).is_some(),
            "{untouched}"
        );
    }
    {
        let pending = ctx.edge_exec_pending.lock().unwrap();
        assert!(!pending.contains("lost"));
        for central in ["root-pty", "other-worker", "other-incarnation", "legacy"] {
            assert!(pending.contains(central), "{central}");
        }
    }
    assert_eq!(ctx.exec_capacity.binding("lost"), None);
    assert_eq!(
        ctx.exec_capacity.in_flight(),
        6,
        "the lost command keeps its slot until its deadline"
    );

    // A repeated notice answers nothing a second time.
    settle_retired_worker(&ctx, &carriers, retired).await;
    assert!(frames(&mut outbound).is_empty());
}

/// The anonymous worker of the legacy and in-process topologies is
/// identified by its incarnation alone.
#[tokio::test]
async fn a_retired_anonymous_worker_settles_its_central_executions() {
    let (ctx, _) = super::super::tests::make_router_ctx().await;
    let carriers = ExecPtyCarrierRegistry::new();
    let (tx, _worker_rx) = tokio::sync::mpsc::unbounded_channel();
    let incarnation = ctx.worker_mgr.install_active_for_test(tx).await;
    running_execution(&ctx, "lost", central_binding(None, incarnation), true).await;
    let mut outbound = ctx.outbound_tx.subscribe();

    settle_retired_worker(
        &ctx,
        &carriers,
        RetiredWorker {
            worker_key: None,
            incarnation,
        },
    )
    .await;

    let sent = frames(&mut outbound);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        edge_result(&sent[0]).disposition,
        EdgeExecDisposition::outcome_lost_with_worker()
    );
    assert_eq!(
        ledger_state(&ctx, "lost").await,
        State::Indeterminate.as_str()
    );
}

/// The worker's real result answers the central and leaves nothing for the
/// departed-worker settlement to answer.
#[tokio::test]
async fn a_real_result_first_leaves_nothing_to_settle() {
    let (ctx, _) = super::super::tests::make_router_ctx().await;
    let carriers = ExecPtyCarrierRegistry::new();
    let key = worker_key("session-1");
    let incarnation = WorkerIncarnation::for_test(3);
    running_execution(&ctx, "gen", central_binding(Some(&key), incarnation), true).await;
    let mut outbound = ctx.outbound_tx.subscribe();

    relay_execution_completed(&ctx, worker_result("gen", None)).await;
    settle_retired_worker(
        &ctx,
        &carriers,
        RetiredWorker {
            worker_key: Some(key),
            incarnation,
        },
    )
    .await;

    let sent = frames(&mut outbound);
    assert_eq!(sent.len(), 1);
    assert!(matches!(
        edge_result(&sent[0]).disposition,
        EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(ref output))
        } if output.exit_code == Some(0)
    ));
    assert_eq!(ledger_state(&ctx, "gen").await, State::Terminal.as_str());
    assert_eq!(ctx.exec_capacity.in_flight(), 0);
}

/// Once the departed-worker settlement has answered, a late connection-less
/// result is dropped: no second answer, and the recorded outcome stands.
#[tokio::test]
async fn a_settled_execution_drops_a_late_central_result() {
    let (ctx, _) = super::super::tests::make_router_ctx().await;
    let carriers = ExecPtyCarrierRegistry::new();
    let key = worker_key("session-1");
    let incarnation = WorkerIncarnation::for_test(3);
    running_execution(&ctx, "gen", central_binding(Some(&key), incarnation), true).await;
    let mut outbound = ctx.outbound_tx.subscribe();

    settle_retired_worker(
        &ctx,
        &carriers,
        RetiredWorker {
            worker_key: Some(key),
            incarnation,
        },
    )
    .await;
    relay_execution_completed(&ctx, worker_result("gen", None)).await;

    let sent = frames(&mut outbound);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        edge_result(&sent[0]).disposition,
        EdgeExecDisposition::outcome_lost_with_worker()
    );
    assert_eq!(
        ledger_state(&ctx, "gen").await,
        State::Indeterminate.as_str()
    );
    assert_eq!(ctx.exec_capacity.in_flight(), 1, "the slot is kept");
}

/// A browser-owned result has no central correlation and still reaches its
/// connection as `ExecutionCompleted`.
#[tokio::test]
async fn a_browser_result_still_reaches_its_connection() {
    let (ctx, _) = super::super::tests::make_router_ctx().await;
    let mut binding = central_binding(None, WorkerIncarnation::for_test(3));
    binding.owner_connection_id = Some("browser".into());
    running_execution(&ctx, "gen", binding, false).await;
    let mut outbound = ctx.outbound_tx.subscribe();

    relay_execution_completed(&ctx, worker_result("gen", Some("browser"))).await;

    let sent = frames(&mut outbound);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].signaling_type, SignalingType::ExecutionCompleted);
    assert_eq!(sent[0].to_connection_id.as_deref(), Some("browser"));
    assert_eq!(ledger_state(&ctx, "gen").await, State::Terminal.as_str());
    assert_eq!(ctx.exec_capacity.in_flight(), 0);
}
