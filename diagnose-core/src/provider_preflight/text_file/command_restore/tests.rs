use super::super::tests::{fixture, root_directory};
use super::*;
use crate::chat::{ChatMessage, ToolCallRef};
use crate::session::PersistedAgentSession;
use desk_agent_protocol::computer_use::ComputerActionStepFact;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_agent_protocol::exec::{
    CommandChangeRestoreOutcome, CommandChangeRestoreResult, CommandChangesRestoreOutput,
    ExecFileChange, ExecSnapshotSkipReason,
};
use desk_agent_protocol::{ExecOutput, ExecOutputStreams, OperationOutput};

const NOW: u64 = 1_000;

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn cwd_reference() -> ObjectRef {
    ObjectRef {
        token: "cwd-ref".into(),
        snapshot_id: "cwd-gen".into(),
        object_kind: ObjectKind::Directory,
        expires_at: String::new(),
    }
}

fn grant_root() -> ExecSnapshotRoot {
    ExecSnapshotRoot::Grant {
        directory_request_id: "directory".into(),
        directory: root_directory(),
    }
}

fn cwd_root() -> ExecSnapshotRoot {
    ExecSnapshotRoot::Cwd {
        path: "/repo".into(),
    }
}

fn change(path: &str, kind: ExecFileChangeKind, root: u16, protected: bool) -> ExecFileChange {
    let (before, after) = match kind {
        ExecFileChangeKind::Added => (None, Some(sha('b'))),
        ExecFileChangeKind::Modified => (Some(sha('a')), Some(sha('b'))),
        ExecFileChangeKind::Deleted => (Some(sha('a')), None),
    };
    ExecFileChange {
        path: path.into(),
        root,
        kind,
        before_sha256: before.filter(|_| protected),
        after_sha256: after.filter(|_| protected),
        size_bytes: 4,
        protected,
    }
}

fn report(roots: &[(ExecSnapshotRoot, ObjectRef)]) -> ExecFileChanges {
    ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Taken,
        recovery_id: Some(sha('c')),
        started_at_unix_ms: 500,
        finished_at_unix_ms: 700,
        roots: roots
            .iter()
            .map(|(root, directory)| ExecFileChangeRoot {
                root: root.clone(),
                directory: directory.clone(),
            })
            .collect(),
        changes: vec![
            change("src/main.rs", ExecFileChangeKind::Modified, 0, true),
            change("new.txt", ExecFileChangeKind::Added, 0, true),
            change("old.txt", ExecFileChangeKind::Deleted, 0, true),
            change("logo.png", ExecFileChangeKind::Modified, 0, false),
        ],
        truncated: false,
    }
}

/// Append one tool call and its device result with a valid envelope.
fn record(session: &mut PersistedAgentSession, call_id: &str, tool: &str, text: String) {
    let owner = crate::model_message_labels::model_bound_user_message(
        "owner".into(),
        "Run the formatter".into(),
        DestinationIdentity::Model {
            connection_id: "model".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    session.conversation.push(ChatMessage::assistant_tool_calls(
        format!("{call_id}-proposal"),
        "",
        vec![ToolCallRef {
            id: call_id.into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        }],
    ));
    let mut result = ChatMessage::tool_result(format!("{call_id}-result"), call_id, &text);
    result.data_envelope = crate::model_message_labels::named_envelope(
        &crate::model_content::ModelContent::builder()
            .unverified(&crate::model_content::Unverified::new(&text))
            .build(),
        owner.data_envelope.as_ref(),
        call_id,
        tool,
        false,
    )
    .unwrap();
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
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

fn record_command(session: &mut PersistedAgentSession, call_id: &str, changes: ExecFileChanges) {
    let output = OperationOutput::Exec(ExecOutput {
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
    });
    record(
        session,
        call_id,
        crate::exec_tools::EXEC_TOOL_NAME,
        serde_json::to_string(&output).unwrap(),
    );
}

fn call(args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "restore-call".into(),
        name: COMMAND_RESTORE_TOOL.into(),
        arguments_json: args.to_string(),
    }
}

fn preflight(
    session: &PersistedAgentSession,
    args: serde_json::Value,
    mode: Option<ExecutionMode>,
) -> Result<TextMutationPreflight, AgentError> {
    TextMutationPreflight::command_restore(
        session,
        &session.device_id,
        ProductSurface::OssPersonalOwner,
        &call(args),
        mode,
        NOW,
    )
}

fn session_with_command(roots: &[(ExecSnapshotRoot, ObjectRef)]) -> PersistedAgentSession {
    let (mut session, _, _) = fixture();
    record_command(&mut session, "command-call", report(roots));
    session
}

#[test]
fn a_granted_root_restores_every_protected_change_by_default() {
    let session = session_with_command(&[(grant_root(), root_directory())]);
    let plan = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call"}),
        None,
    )
    .unwrap();
    assert_eq!(plan.target(), &root_directory());
    assert_eq!(plan.resource_scope().len(), 1);
    assert_eq!(plan.required_capability(), Capability::FilePatchConfirmed);
    let FilePatchAction::RestoreCommandChanges {
        recovery_id,
        root,
        entries,
    } = plan.action()
    else {
        panic!("unexpected action");
    };
    assert_eq!(recovery_id, &sha('c'));
    assert_eq!(root, &grant_root());
    // The unprotected binary change is never offered for restore.
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["src/main.rs", "new.txt", "old.txt"]
    );
    assert_eq!(
        ComputerActionKind::File(plan.action().clone()).required_capability(),
        Capability::FilePatchConfirmed
    );
    let review = plan.command_restore_review().unwrap();
    assert_eq!(review.started_at_unix_ms, 500);
    assert_eq!(review.finished_at_unix_ms, 700);
    // An omitted root and the declared default share one approval.
    let explicit = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call","root":0}),
        None,
    )
    .unwrap();
    assert_eq!(
        explicit.canonical_input_digest_sha256(),
        plan.canonical_input_digest_sha256()
    );
    assert_eq!(
        crate::provider_preflight::classify_provider_call(
            crate::ai_assistant::ai_assistant_provider_registry()
                .capability_for_tool(COMMAND_RESTORE_TOOL)
                .unwrap(),
            &call(serde_json::json!({"command_result_call_id":"command-call"})),
        )
        .unwrap(),
        desk_agent_protocol::capability_grant::CapabilityRiskTier::R3
    );
}

#[test]
fn paths_select_a_subset_of_protected_entries() {
    let session = session_with_command(&[(grant_root(), root_directory())]);
    let plan = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call","paths":["old.txt","src/main.rs"]}),
        None,
    )
    .unwrap();
    let FilePatchAction::RestoreCommandChanges { entries, .. } = plan.action() else {
        panic!("unexpected action");
    };
    // Receipt order, not request order.
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["src/main.rs", "old.txt"]
    );
    assert_ne!(
        plan.canonical_input_digest_sha256(),
        preflight(
            &session,
            serde_json::json!({"command_result_call_id":"command-call"}),
            None
        )
        .unwrap()
        .canonical_input_digest_sha256()
    );
    for paths in [
        serde_json::json!(["logo.png"]),
        serde_json::json!(["missing.txt"]),
        serde_json::json!(["new.txt", "new.txt"]),
        serde_json::json!([]),
    ] {
        let error = preflight(
            &session,
            serde_json::json!({"command_result_call_id":"command-call","paths":paths}),
            None,
        )
        .err()
        .expect("restore must be refused");
        assert_eq!(error.kind, AgentErrorKind::InvalidInput, "{paths}");
    }
}

#[test]
fn a_revoked_directory_approval_blocks_the_restore() {
    let mut session = session_with_command(&[(grant_root(), root_directory())]);
    let subject = session
        .file_scope_subject("owner", "device", "conversation")
        .unwrap();
    let revision = session.file_scope.revision();
    session
        .file_scope
        .revoke(&subject, revision, "directory")
        .unwrap();
    let error = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call"}),
        Some(ExecutionMode::Automated),
    )
    .err()
    .expect("restore must be refused");
    assert_eq!(error.kind, AgentErrorKind::PermissionDenied);
}

#[test]
fn a_cwd_root_needs_an_execution_mode_that_admits_mutating_commands() {
    let session = session_with_command(&[(cwd_root(), cwd_reference())]);
    let args = serde_json::json!({"command_result_call_id":"command-call"});
    for mode in [
        None,
        Some(ExecutionMode::SuggestOnly),
        Some(ExecutionMode::ReadOnly),
    ] {
        let error = preflight(&session, args.clone(), mode)
            .err()
            .expect("restore must be refused");
        assert_eq!(error.kind, AgentErrorKind::PermissionDenied, "{mode:?}");
    }
    let plan = preflight(&session, args, Some(ExecutionMode::ConfirmEachAction)).unwrap();
    // The device-issued reference of the working directory is the target.
    assert_eq!(plan.target(), &cwd_reference());
    // Directory approvals play no part for a working-directory root.
    let (mut other, _, _) = fixture();
    other.file_scope = Default::default();
    record_command(
        &mut other,
        "command-call",
        report(&[(cwd_root(), cwd_reference())]),
    );
    preflight(
        &other,
        serde_json::json!({"command_result_call_id":"command-call"}),
        Some(ExecutionMode::Automated),
    )
    .unwrap();
}

#[test]
fn a_multi_root_command_is_restored_one_root_at_a_time() {
    let (mut session, _, _) = fixture();
    let mut changes = report(&[
        (cwd_root(), cwd_reference()),
        (grant_root(), root_directory()),
    ]);
    changes
        .changes
        .push(change("src/main.rs", ExecFileChangeKind::Modified, 1, true));
    record_command(&mut session, "command-call", changes);
    let second = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call","root":1}),
        None,
    )
    .unwrap();
    assert_eq!(second.target(), &root_directory());
    let FilePatchAction::RestoreCommandChanges { entries, .. } = second.action() else {
        panic!("unexpected action");
    };
    assert_eq!(entries.len(), 1);
    let error = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call","root":2}),
        None,
    )
    .err()
    .expect("restore must be refused");
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
}

#[test]
fn unprotected_or_unrelated_results_are_not_restore_sources() {
    let (mut session, _, _) = fixture();
    let skipped = ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::FileLimit),
        recovery_id: None,
        roots: vec![],
        changes: vec![],
        ..report(&[])
    };
    record_command(&mut session, "skipped-call", skipped);
    record(
        &mut session,
        "read-call",
        crate::tool_names::READ_TEXT_FILE,
        "{}".into(),
    );
    for id in ["skipped-call", "read-call", "unknown-call"] {
        let error = preflight(
            &session,
            serde_json::json!({"command_result_call_id":id}),
            Some(ExecutionMode::Automated),
        )
        .err()
        .expect("restore must be refused");
        assert_eq!(error.kind, AgentErrorKind::InvalidInput, "{id}");
    }
    // The generic text preflight refuses this tool: it needs the policy.
    assert!(
        TextMutationPreflight::from_device_session(
            &session,
            "device",
            ProductSurface::OssPersonalOwner,
            &call(serde_json::json!({"command_result_call_id":"skipped-call"})),
            NOW,
        )
        .is_err()
    );
}

fn restore_output(entries: &[CommandChangeRestore]) -> CommandChangesRestoreOutput {
    let restored = &entries[0];
    CommandChangesRestoreOutput {
        results: entries
            .iter()
            .enumerate()
            .map(|(index, entry)| CommandChangeRestoreResult {
                path: entry.path.clone(),
                outcome: if index == 0 {
                    CommandChangeRestoreOutcome::Restored
                } else {
                    CommandChangeRestoreOutcome::Conflict
                },
            })
            .collect(),
        file_changes: ExecFileChanges {
            recovery_id: Some(sha('d')),
            started_at_unix_ms: 800,
            finished_at_unix_ms: 810,
            roots: vec![ExecFileChangeRoot {
                root: grant_root(),
                directory: root_directory(),
            }],
            changes: vec![ExecFileChange {
                path: restored.path.clone(),
                root: 0,
                kind: restored.inverse_kind(),
                before_sha256: restored.after_sha256.clone(),
                after_sha256: restored.before_sha256.clone(),
                size_bytes: 4,
                protected: true,
            }],
            ..report(&[])
        },
    }
}

fn completion(output: CommandChangesRestoreOutput) -> ComputerActionCompleted {
    ComputerActionCompleted {
        work_id: "work".into(),
        action_request_id: "restore-call".into(),
        execution_generation: "generation".into(),
        result: if output.verified() {
            ComputerActionResultClass::Verified
        } else {
            ComputerActionResultClass::OutcomeUnknown
        },
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: output.file_changes.snapshot == ExecSnapshotOutcome::Taken,
            verified: output.verified(),
            summary: "restore".into(),
        }],
        message: None,
        output: Some(ComputerActionOutput::CommandChangesRestore(output)),
    }
}

#[test]
fn restore_receipts_are_validated_and_can_be_undone_again() {
    let mut session = session_with_command(&[(grant_root(), root_directory())]);
    let plan = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call"}),
        None,
    )
    .unwrap();
    let FilePatchAction::RestoreCommandChanges { entries, .. } = plan.action() else {
        panic!("unexpected action");
    };
    let action = ComputerActionKind::File(plan.action().clone());
    let completed = completion(restore_output(entries));
    validate_command_restore_completion(plan.target(), &action, &completed).unwrap();
    // A receipt for another directory or with a wrong class is rejected.
    assert!(validate_command_restore_completion(&cwd_reference(), &action, &completed).is_err());
    let mut wrong = completed.clone();
    wrong.result = ComputerActionResultClass::OutcomeUnknown;
    assert!(validate_command_restore_completion(plan.target(), &action, &wrong).is_err());
    let mut not_started = completed.clone();
    not_started.output = None;
    not_started.result = ComputerActionResultClass::Failed;
    not_started.facts.clear();
    validate_command_restore_completion(plan.target(), &action, &not_started).unwrap();
    // The restore's own report names it as a new restore source.
    record(
        &mut session,
        "restore-call",
        COMMAND_RESTORE_TOOL,
        serde_json::to_string(&completed).unwrap(),
    );
    let undo = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"restore-call"}),
        None,
    )
    .unwrap();
    let FilePatchAction::RestoreCommandChanges {
        recovery_id,
        entries: undo_entries,
        ..
    } = undo.action()
    else {
        panic!("unexpected action");
    };
    assert_eq!(recovery_id, &sha('d'));
    assert_eq!(undo_entries.len(), 1);
    assert_eq!(undo_entries[0].before_sha256, entries[0].after_sha256);
    assert_eq!(undo_entries[0].after_sha256, entries[0].before_sha256);
}

#[test]
fn model_ids_resolve_to_the_command_result() {
    let mut session = session_with_command(&[(grant_root(), root_directory())]);
    // The model sees command_result_id on the projected result.
    let mut projected = session.conversation.last().unwrap().clone();
    crate::result_model_ids::project_result_message(&mut projected);
    let shown: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
    assert_eq!(shown["command_result_id"], "command-call-result");
    let mut arguments = serde_json::json!({"command_result_id":"command-call-result"});
    crate::result_model_ids::resolve(
        COMMAND_RESTORE_TOOL,
        &mut arguments,
        &session.conversation,
        NOW,
    )
    .unwrap();
    assert_eq!(arguments["command_result_call_id"], "command-call");
    assert!(arguments.get("command_result_id").is_none());
    for bad in [
        serde_json::json!({"command_result_id":"unknown"}),
        serde_json::json!({}),
        serde_json::json!({"command_result_id":"command-call-result","command_result_call_id":"command-call"}),
    ] {
        let mut bad_arguments = bad.clone();
        assert!(
            crate::result_model_ids::resolve(
                COMMAND_RESTORE_TOOL,
                &mut bad_arguments,
                &session.conversation,
                NOW,
            )
            .is_err(),
            "{bad}"
        );
    }
    // A skipped snapshot offers no restore id.
    record_command(
        &mut session,
        "skipped-call",
        ExecFileChanges {
            snapshot: ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::TimeLimit),
            recovery_id: None,
            roots: vec![],
            changes: vec![],
            ..report(&[])
        },
    );
    let mut projected = session.conversation.last().unwrap().clone();
    crate::result_model_ids::project_result_message(&mut projected);
    let shown: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
    assert!(shown.get("command_result_id").is_none());
}

#[test]
fn review_lists_files_operations_window_and_warning_within_budget() {
    let (mut session, _, _) = fixture();
    let mut changes = report(&[(grant_root(), root_directory())]);
    changes.changes.clear();
    // Fill the report as far as one restore of all of it stays within the
    // 32 KiB bound, with every size reported at its longest.
    let size_growth = u64::MAX.to_string().len() - 1;
    let mut i = 0;
    loop {
        let mut next = changes.clone();
        next.changes.push(change(
            &format!("src/{}/file-{i:04}.rs", "d".repeat(40)),
            ExecFileChangeKind::Modified,
            0,
            true,
        ));
        if serde_json::to_vec(&next).unwrap().len() + size_growth * next.changes.len()
            > desk_agent_protocol::exec::MAX_EXEC_FILE_CHANGES_BYTES
        {
            break;
        }
        changes = next;
        i += 1;
    }
    let count = changes.changes.len();
    record_command(&mut session, "command-call", changes);
    let canonical = crate::permission_tools::canonical_tool_permission_input_json(
        COMMAND_RESTORE_TOOL,
        serde_json::json!({"command_result_call_id":"command-call"}),
    )
    .unwrap();
    let review = review_command_restore_input(Some(&canonical), &session.conversation).unwrap();
    assert_eq!(review.entries.len(), count);
    let projection = review.projection();
    assert_eq!(projection["files"].as_array().unwrap().len(), count);
    assert_eq!(projection["files"][0]["restore"], "rewrite_previous");
    assert_eq!(projection["root_kind"], "approved_directory");
    assert_eq!(projection["execution_started_at_unix_ms"], 500);
    assert!(
        projection["warning"]
            .as_str()
            .unwrap()
            .contains("may include changes the command did not make")
    );
    assert!(!projection.to_string().contains(&sha('a')));
    assert!(
        projection.to_string().len() < crate::approval_review::MAX_APPROVAL_ACTION_BYTES / 2,
        "{}",
        projection.to_string().len()
    );
    // A cwd root names the working directory.
    let cwd = CommandRestoreReview {
        root: cwd_root(),
        ..review
    };
    assert_eq!(cwd.projection()["working_directory"], "/repo");
}

#[test]
fn a_restore_whose_receipt_could_exceed_the_report_limit_is_split() {
    // A report just under the limit; its restore receipt reports the same
    // entries, but every size may need more digits.
    let mut changes = report(&[(grant_root(), root_directory())]);
    changes.changes.clear();
    let limit = desk_agent_protocol::exec::MAX_EXEC_FILE_CHANGES_BYTES;
    loop {
        let index = changes.changes.len();
        changes.changes.push(change(
            &format!("src/generated/module_{index:04}_{}.rs", "x".repeat(80)),
            ExecFileChangeKind::Modified,
            0,
            true,
        ));
        if serde_json::to_vec(&changes).unwrap().len() > limit - 200 {
            changes.changes.pop();
            break;
        }
    }
    let (mut session, _, _) = fixture();
    let first_paths: Vec<String> = changes.changes[..10]
        .iter()
        .map(|change| change.path.clone())
        .collect();
    record_command(&mut session, "command-call", changes);
    let error = preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call"}),
        None,
    )
    .err()
    .expect("the whole root cannot be restored in one call");
    assert!(
        error.message.contains("smaller groups"),
        "{}",
        error.message
    );
    preflight(
        &session,
        serde_json::json!({"command_result_call_id":"command-call","paths":first_paths}),
        None,
    )
    .unwrap();
}
