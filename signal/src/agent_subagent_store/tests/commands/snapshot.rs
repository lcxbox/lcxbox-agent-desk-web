//! Command snapshot roots at the OSS host: approvals are revalidated against
//! the session's current directory approvals, device change reports are
//! admitted only against the approved plan, and `restore_command_changes`
//! reaches its own preflight.
use super::*;
use crate::command_policy::tests::{approve, mark_ready, owner_host};
use desk_agent_protocol::exec::{
    EXEC_SNAPSHOT_TIME_BUDGET_MS, ExecFileChange, ExecFileChangeKind, ExecFileChangeRoot,
    ExecFileChanges, ExecSnapshotOutcome, ExecSnapshotRoot,
};
use desk_agent_protocol::{Capability, ExecOutput, ExecOutputStreams, OperationOutput};
use desk_diagnose_core::provider_preflight::text_file::COMMAND_RESTORE_TOOL;
use desk_signal_facade::model::connection::SharedConnectionMap;
use std::sync::Arc;

pub(super) const PLAN_CHANGED: &str =
    "the command plan, target or policy changed; request permission again";

/// Serializes tests that report readiness for the shared `host` connection
/// in the process-wide readiness cache.
static HOST_READINESS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The owner host the harness approval was prepared for: connection `host`,
/// interactive session `session-1`, bash only, 60 s ceiling. The guard keeps
/// other tests from reporting a different `host` readiness meanwhile.
pub(super) async fn owner_host_for(
    child: &PersistedAgentSession,
) -> (
    tokio::sync::MutexGuard<'static, ()>,
    Arc<SharedConnectionMap>,
) {
    let guard = HOST_READINESS.lock().await;
    let connections = Arc::new(SharedConnectionMap::new_oss());
    connections.write().await.insert(
        "host".into(),
        owner_host("host", &child.device_id, &["bash"], Some(60_000)).await,
    );
    mark_ready("host", "session-1", &[Capability::ShellExecConfirmed]);
    crate::ai_agent_gate::enable_test_host();
    (guard, connections)
}

pub(super) fn agent_tools(
    db: &DatabaseConnection,
    child: &PersistedAgentSession,
    connections: Arc<SharedConnectionMap>,
) -> crate::remote_tool_edge::SignalAiAgentTools {
    crate::remote_tool_edge::SignalAiAgentTools::new(
        db.clone(),
        desk_diagnose_core::ai_agent::ai_agent_provider_registry(),
        connections,
        Arc::new(crate::remote_tool_edge::SignalRemoteToolPendingStore::default()),
        "host".into(),
        child.device_id.clone(),
        child.actor_id.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        vec![],
        None,
        None,
        "Delegated check".into(),
        child.conversation_id.clone(),
        child.current_turn_id.clone().unwrap(),
        child.policy_revision,
        1,
        vec!["bash".into()],
        60000,
    )
}

pub(super) fn context(child: &PersistedAgentSession, call: &ToolCall) -> ExecContext {
    let mut scope = child.scope_snapshot.clone();
    scope.mode = desk_agent_protocol::ExecutionMode::ConfirmEachAction;
    ExecContext {
        agent_turn_fence: desk_diagnose_core::action_turn_fence::AgentTurnFence::from_session(
            child,
        )
        .unwrap(),
        conversation_id: child.conversation_id.clone(),
        turn_id: child.current_turn_id.clone().unwrap(),
        tool_call_id: call.id.clone(),
        actor_id: child.actor_id.clone(),
        policy_revision: child.policy_revision,
        scope,
        connection_id: None,
    }
}

pub(super) fn approved(child: &PersistedAgentSession, call: &ToolCall) -> CommandConfirmation {
    CommandConfirmation::approved_request_for_call(child, &child.device_id, &call.arguments_json)
        .unwrap()
        .1
        .clone()
}

pub(super) async fn assert_still_current(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    child: &PersistedAgentSession,
    call: &ToolCall,
) {
    crate::command_policy::current(
        db,
        connections,
        "host",
        &child.actor_id,
        child,
        jiff::Timestamp::now().as_millisecond() as u64,
    )
    .await
    .unwrap()
    .revalidate(
        &approved(child, call),
        &call.arguments_json,
        child.input_revision,
    )
    .unwrap();
}

#[actix_web::test]
async fn an_approved_mutating_command_is_not_sent_after_a_directory_approval_is_added() {
    let (db, _, mut child, call, _) = approved_child().await;
    let (_host, connections) = owner_host_for(&child).await;
    // With unchanged approvals the approval matches the current policy.
    assert_still_current(&db, &connections, &child, &call).await;
    let device = child.device_id.clone();
    approve(&mut child, &device, "added-after-approval");
    save_child_session(&db, &mut child).await.unwrap();

    let error = agent_tools(&db, &child, connections)
        .confirm_and_exec(&call, &context(&child, &call))
        .await
        .unwrap_err();
    assert_eq!(error.message, PLAN_CHANGED);
    assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 0);
}

#[actix_web::test]
async fn dispatch_revalidates_the_plan_against_the_sessions_directory_approvals() {
    let (db, _, child, call, _) = approved_child().await;
    let (_host, connections) = owner_host_for(&child).await;
    let confirmation = approved(&child, &call);
    let tools = crate::agent_exec::SignalAgentTools::new(
        db.clone(),
        connections,
        Arc::new(crate::agent_exec::SignalAgentExecPending::new()),
        "host".into(),
        desk_agent_protocol::evidence::EvidenceSnapshot::record(
            "fixture",
            "fixture",
            desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
            Vec::new(),
        ),
        ExecAdmissionPolicy::OwnerInteractive,
        RiskLevel::Critical,
        vec!["bash".into()],
        60000,
    );
    let dispatch = |session: PersistedAgentSession| {
        let tools = &tools;
        let confirmation = &confirmation;
        let call = &call;
        async move {
            tools
                .execute_preapproved(
                    &session,
                    None,
                    confirmation.validation_input.clone(),
                    confirmation,
                    &call.arguments_json,
                    &context(&session, call),
                    desk_agent_protocol::exec::ExecRequestId("revalidated-command".into()),
                    "revalidated-generation".into(),
                    desk_agent_protocol::exec::ApprovalId("revalidated-approval".into()),
                    desk_agent_protocol::exec::ExecAuthorization::PerAction,
                )
                .await
                .unwrap_err()
        }
    };
    // With the approving session's directory approvals the plan passes the
    // policy check and stops only for want of a claimed task.
    assert_eq!(
        dispatch(child.clone()).await.message,
        "confirmed command has no durable execution task"
    );
    let mut added = child.clone();
    approve(&mut added, &child.device_id, "added-before-dispatch");
    assert_eq!(dispatch(added).await.message, PLAN_CHANGED);
}

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn cwd_root() -> ExecSnapshotRoot {
    ExecSnapshotRoot::Cwd {
        path: "/repo".into(),
    }
}

fn change_report(root: ExecSnapshotRoot) -> ExecFileChanges {
    ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Taken,
        recovery_id: Some(sha('c')),
        started_at_unix_ms: 500,
        finished_at_unix_ms: 700,
        roots: vec![ExecFileChangeRoot {
            root,
            directory: desk_agent_protocol::computer_use::ObjectRef {
                token: "cwd-ref".into(),
                snapshot_id: "cwd-gen".into(),
                object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
                expires_at: String::new(),
            },
        }],
        changes: vec![ExecFileChange {
            path: "src/main.rs".into(),
            root: 0,
            kind: ExecFileChangeKind::Modified,
            before_sha256: Some(sha('a')),
            after_sha256: Some(sha('b')),
            size_bytes: 4,
            protected: true,
        }],
        truncated: false,
    }
}

fn executed(
    changes: Option<ExecFileChanges>,
) -> desk_agent_protocol::edge_exec::EdgeExecDisposition {
    desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed {
        outcome: desk_agent_protocol::AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
            started: true,
            exit_code: Some(0),
            termination_signal: None,
            failure: None,
            diagnostics: vec![],
            streams: ExecOutputStreams::Split {
                stdout: "formatted".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            },
            duration_ms: 200,
            redactions: vec![],
            file_changes: changes,
        })),
    }
}

fn reported(
    disposition: &desk_agent_protocol::edge_exec::EdgeExecDisposition,
) -> Option<&ExecFileChanges> {
    match disposition {
        desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed {
            outcome: desk_agent_protocol::AgentOutcome::Ok(OperationOutput::Exec(output)),
        } => output.file_changes.as_ref(),
        _ => panic!("expected an executed command"),
    }
}

#[tokio::test]
async fn a_device_change_report_is_admitted_only_against_the_claimed_plan_roots() {
    let (db, _, child, call, grant) = approved_child().await;
    let store = SignalCapabilityGrantStore::new(db.clone());
    store
        .prepare(prepared(&child, &call, &grant))
        .await
        .unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } = store
        .record_dispatch_intent(prepared(&child, &call, &grant))
        .await
        .unwrap()
    else {
        panic!("approved input must dispatch")
    };
    let now = jiff::Timestamp::now().as_millisecond() as u64;
    let origin = ActionResultOrigin::capture_confirmed_command(
        &desk_diagnose_core::ai_agent::ai_agent_provider_registry(),
        &child,
        &child.device_id,
        &with_target_ref(&call, &child.device_id),
        now,
    )
    .unwrap();
    assert!(matches!(
        store
            .claim_command_dispatch(
                &dispatch_id,
                now,
                "host",
                &child.conversation_id,
                &call.id,
                60000,
                &[cwd_root()],
                &origin,
                None,
            )
            .await
            .unwrap(),
        DispatchClaimResult::Claimed(_)
    ));
    // The result deadline covers the command timeout, the snapshot budget and
    // transport slack.
    let task = agent_exec_task::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (task.deadline - task.created_at).num_milliseconds(),
        60_000 + i64::from(EXEC_SNAPSHOT_TIME_BUDGET_MS) + 60_000
    );

    let exec = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    let mut matching = executed(Some(change_report(cwd_root())));
    exec.admit_file_changes(&dispatch_id, &mut matching).await;
    assert_eq!(reported(&matching), Some(&change_report(cwd_root())));

    let mut other_root = executed(Some(change_report(ExecSnapshotRoot::Cwd {
        path: "/elsewhere".into(),
    })));
    exec.admit_file_changes(&dispatch_id, &mut other_root).await;
    assert_eq!(reported(&other_root), None);

    // An execution without an approved plan (no roots) keeps no report.
    let mut unplanned = executed(Some(change_report(cwd_root())));
    exec.admit_file_changes("unplanned-generation", &mut unplanned)
        .await;
    assert_eq!(reported(&unplanned), None);
}

/// Append an approved `exec_command` and its device result carrying `changes`.
fn record_command(session: &mut PersistedAgentSession, call_id: &str, changes: ExecFileChanges) {
    let text = serde_json::to_string(&OperationOutput::Exec(ExecOutput {
        started: true,
        exit_code: Some(0),
        termination_signal: None,
        failure: None,
        diagnostics: vec![],
        streams: ExecOutputStreams::Split {
            stdout: "formatted".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        },
        duration_ms: 200,
        redactions: vec![],
        file_changes: Some(changes),
    }))
    .unwrap();
    let tool = desk_diagnose_core::exec_tools::EXEC_TOOL_NAME;
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "owner".into(),
        "Run the formatter".into(),
        super::super::creation::destination(),
    )
    .unwrap();
    session.conversation.push(ChatMessage::agent_tool_calls(
        format!("{call_id}-proposal"),
        "",
        vec![ToolCallRef {
            id: call_id.into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        }],
    ));
    let mut result = ChatMessage::tool_result(format!("{call_id}-result"), call_id, &text);
    result.data_envelope = desk_diagnose_core::model_message_labels::named_envelope(
        &desk_diagnose_core::model_content::ModelContent::builder()
            .unverified(&desk_diagnose_core::model_content::Unverified::new(&text))
            .build(),
        owner.data_envelope.as_ref(),
        call_id,
        tool,
        false,
    )
    .unwrap();
    let registry = desk_diagnose_core::ai_agent::ai_agent_provider_registry();
    let capability = registry.capability_for_tool(tool).unwrap();
    result
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_provider_id = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap()
        .wire
        .provider_id
        .clone();
    session.conversation.push(result);
}

fn restore_call(command_result: &str) -> ToolCall {
    ToolCall {
        id: "restore-command-changes".into(),
        name: COMMAND_RESTORE_TOOL.into(),
        arguments_json: serde_json::json!({"command_result_call_id": command_result, "root": 0})
            .to_string(),
    }
}

#[actix_web::test]
async fn restore_command_changes_runs_its_own_preflight() {
    let (db, _, child, _, _) = approved_child().await;
    let call = restore_call("no-such-command");
    let error = agent_tools(&db, &child, Arc::new(SharedConnectionMap::new_oss()))
        .confirm_and_exec(&call, &context(&child, &call))
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("command_result_id must be the result ID of an exec_command"),
        "{}",
        error.message
    );
}

#[actix_web::test]
async fn a_working_directory_restore_follows_the_devices_current_execution_mode() {
    let (db, _, mut child, _, _) = approved_child().await;
    record_command(&mut child, "formatter", change_report(cwd_root()));
    save_child_session(&db, &mut child).await.unwrap();
    let call = restore_call("formatter");

    // Without a current command policy for the device there is no execution
    // mode that admits changes in the command's working directory.
    let error = agent_tools(&db, &child, Arc::new(SharedConnectionMap::new_oss()))
        .confirm_and_exec(&call, &context(&child, &call))
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("The device execution policy no longer allows commands that change files"),
        "{}",
        error.message
    );

    // Under the owner's ConfirmEachAction mode the restore passes its
    // preflight and targets the snapshot directory; this host has not
    // reported file mutations as ready.
    let (_host, connections) = owner_host_for(&child).await;
    let error = agent_tools(&db, &child, connections)
        .confirm_and_exec(&call, &context(&child, &call))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Provider readiness changed; refresh capabilities before retrying"
    );
}
