//! `restore_text_file` preflight and the owner/AI review of exact text inputs.
//!
//! A restore names the result of an earlier AI update, deletion or restore.
//! That receipt is immutable device evidence in this conversation: it fixes
//! the backup, the file name and the directory the original action was
//! dispatched in. The directory is re-selected through its current approval,
//! so a newer approval never redirects a restore and a revoked one blocks it.
use super::*;
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass, RestoreExpectation,
    TextFileMutationOperation, TextFileMutationOutput,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreArgs {
    pub mutation_result_call_id: String,
    #[serde(default)]
    pub file_result_call_id: Option<String>,
}

/// What a restore puts back, for the approval card and the AI review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReview {
    /// Operation of the result being undone.
    pub original_operation: TextFileMutationOperation,
    /// When the restored backup was taken.
    pub backup_created_at_unix_ms: u64,
    /// The current file is a version the undone operation did not produce.
    pub overrides_non_ai_version: bool,
    /// The file is expected to exist with this digest, or to be absent.
    pub expected_current: RestoreExpectation,
}

/// Owner/AI review of one exact text mutation input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChangeReview {
    pub file_name: String,
    pub operation: TextFileMutationOperation,
    /// Result whose file version (update/delete) or backup (restore) is used.
    pub source_result_call_id: String,
    /// Version an update or deletion requires.
    pub expected_sha256: Option<String>,
    pub change: Option<TextFileChange>,
    pub restore: Option<RestoreReview>,
}

impl TextChangeReview {
    /// Model view for the approval reviewer: the per-file change without
    /// result IDs or digests.
    pub fn projection(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "file_name": self.file_name,
            "operation": self.operation,
        });
        match &self.change {
            Some(TextFileChange::Edit { edits }) => value["edits"] = serde_json::json!(edits),
            Some(TextFileChange::Write { content_utf8 }) => {
                value["write_bytes"] = serde_json::json!(content_utf8.len());
                value["content_utf8"] = serde_json::json!(content_utf8);
            }
            None => {}
        }
        if let Some(restore) = &self.restore {
            value["restores_before"] = serde_json::json!(restore.original_operation);
            value["backup_created_at_unix_ms"] =
                serde_json::json!(restore.backup_created_at_unix_ms);
            value["overrides_non_ai_version"] = serde_json::json!(restore.overrides_non_ai_version);
            value["file_expected_to_exist"] = serde_json::json!(matches!(
                restore.expected_current,
                RestoreExpectation::Sha(_)
            ));
        }
        value
    }
}

/// The original operation's facts from its immutable receipt.
struct MutationReceipt {
    output: TextFileMutationOutput,
    /// State the operation left the file in, when its outcome is known.
    resulting_state: Option<RestoreExpectation>,
}

fn invalid(message: &str) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false, true)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

/// Resolve the receipt of an earlier text mutation in this conversation. An
/// expired model-egress window does not matter: no receipt content leaves the
/// server, and the device keeps the backup under its own retention.
fn mutation_receipt(
    history: &[crate::chat::ChatMessage],
    device_id: Option<&str>,
    call_id: &str,
) -> Result<MutationReceipt, AgentError> {
    use crate::chat::ChatRole;
    let not_found = || {
        invalid(
            "mutation_result_id must be the result ID of an update_text_file, delete_text_file or restore_text_file result in this conversation. No approval card was created.",
        )
    };
    if !valid_id(call_id) {
        return Err(not_found());
    }
    let mut calls = history
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == call_id);
    let call = calls.next().ok_or_else(not_found)?;
    let operation = match call.name.as_str() {
        UPDATE_TEXT_TOOL => TextFileMutationOperation::Update,
        DELETE_TEXT_TOOL => TextFileMutationOperation::Delete,
        RESTORE_TEXT_TOOL => TextFileMutationOperation::Restore,
        _ => return Err(not_found()),
    };
    if calls.next().is_some() {
        return Err(not_found());
    }
    let mut selected: Option<(ComputerActionResultClass, TextFileMutationOutput)> = None;
    for message in history {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            || message.tool_call_id.as_deref() != Some(call_id)
        {
            continue;
        }
        let result = message.trusted_tool_result();
        let Some(envelope) = &result.data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || envelope.provenance.source_tool_name != call.name
            || envelope.provenance.source_provider_id != crate::ai_assistant::TEXT_FILE_PROVIDER_ID
            || envelope.content.size_bytes() != result.text.len() as u64
            || device_id.is_some_and(|device| {
                envelope.provenance.source_object_id.as_deref()
                    != Some(format!("{device}:{call_id}").as_str())
            })
        {
            continue;
        }
        let Ok(completed) = serde_json::from_str::<ComputerActionCompleted>(&result.text) else {
            continue;
        };
        let Some(ComputerActionOutput::TextFileMutation(output)) = completed.output else {
            continue;
        };
        let class = completed.result;
        if output.operation != operation
            || !output.recovery.is_valid()
            || output.original_file_name.is_empty()
            || output.original_file_name.len() > 512
            || output.original_file_name.chars().any(char::is_control)
            || output.directory.object_kind != ObjectKind::Directory
            || output.verified != (class == ComputerActionResultClass::Verified)
            || !matches!(
                class,
                ComputerActionResultClass::Verified | ComputerActionResultClass::OutcomeUnknown
            )
        {
            continue;
        }
        if selected
            .as_ref()
            .is_some_and(|(prior_class, prior)| *prior_class != class || *prior != output)
        {
            return Err(not_found());
        }
        selected = Some((class, output));
    }
    let (_, output) = selected.ok_or_else(not_found)?;
    let resulting_state = if !output.verified {
        None
    } else {
        match output.operation {
            TextFileMutationOperation::Update => {
                output.updated_sha256.clone().map(RestoreExpectation::Sha)
            }
            TextFileMutationOperation::Delete => Some(RestoreExpectation::Absent),
            TextFileMutationOperation::Restore => Some(
                output
                    .updated_sha256
                    .clone()
                    .map_or(RestoreExpectation::Absent, RestoreExpectation::Sha),
            ),
        }
    };
    Ok(MutationReceipt {
        output,
        resulting_state,
    })
}

/// Expected current state: the state the undone operation produced, or the
/// separately read version, which the card flags when it differs.
fn plan(
    history: &[crate::chat::ChatMessage],
    device_id: Option<&str>,
    args: &RestoreArgs,
    now_unix_ms: u64,
) -> Result<(MutationReceipt, RestoreReview), AgentError> {
    let receipt = mutation_receipt(history, device_id, &args.mutation_result_call_id)?;
    let (expected_current, overrides_non_ai_version) = match &args.file_result_call_id {
        Some(id) => {
            let version = resolve_file_result_from_history(history, id, now_unix_ms)?;
            if version.file_name() != receipt.output.original_file_name {
                return Err(invalid(
                    "file_version_id must identify a verified version of the same file name as the operation being restored. No approval card was created.",
                ));
            }
            let expected = RestoreExpectation::Sha(version.sha256().into());
            let overrides = receipt.resulting_state.as_ref() != Some(&expected);
            (expected, overrides)
        }
        None => (
            receipt.resulting_state.clone().ok_or_else(|| {
                invalid(
                    "The outcome of that operation is unknown, so the file's current version is unknown. Read the file (read_text_file) and pass that read's file_version_id with mutation_result_id. No approval card was created.",
                )
            })?,
            false,
        ),
    };
    let review = RestoreReview {
        original_operation: receipt.output.operation,
        backup_created_at_unix_ms: receipt.output.recovery.created_at_unix_ms,
        overrides_non_ai_version,
        expected_current,
    };
    Ok((receipt, review))
}

/// Re-select the directory the original action was dispatched in through the
/// approval that names it. Only that approval counts; the single-root rule of
/// an omitted selector is never applied again.
fn original_directory(
    session: &crate::session::PersistedAgentSession,
    device_id: &str,
    directory: &ObjectRef,
    now_unix_ms: u64,
) -> Result<ObjectRef, AgentError> {
    let ids = session
        .file_scope
        .records()
        .iter()
        .filter(|record| record.proposal.directory == *directory)
        .map(|record| record.proposal.request_id.as_str())
        .chain(
            session
                .delegated_directories
                .iter()
                .filter(|delegated| {
                    delegated.device_id == device_id && delegated.directory == *directory
                })
                .map(|delegated| delegated.request_id.as_str()),
        );
    for id in ids {
        let selector = ToolCall {
            id: "restore-directory".into(),
            name: RESTORE_TEXT_TOOL.into(),
            arguments_json: serde_json::json!({"directory_request_id": id}).to_string(),
        };
        if let Ok(selected) =
            crate::file_scope::select_device_directory(session, device_id, &selector, now_unix_ms)
            && selected == *directory
        {
            return Ok(selected);
        }
    }
    Err(error(
        AgentErrorKind::PermissionDenied,
        "The directory approval used by the original operation was revoked or expired. A restore needs that directory approved for this conversation; the owner can still export the backup from file backup management. No approval card was created.",
        false,
        true,
    ))
}

pub(super) fn build(
    session: &crate::session::PersistedAgentSession,
    device_id: &str,
    call: &ToolCall,
    now_unix_ms: u64,
) -> Result<TextMutationPreflight, AgentError> {
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    let capability = registry
        .capability_for_tool(RESTORE_TEXT_TOOL)
        .ok_or_else(unavailable)?;
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .ok_or_else(unavailable)?;
    if call.name != RESTORE_TEXT_TOOL || now_unix_ms == 0 {
        return Err(unavailable());
    }
    let canonical_input_json = canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
    )
    .map_err(|_| unavailable())?;
    check_input_budget(&canonical_input_json)?;
    let args: RestoreArgs =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    let selected_device = session.device_id.is_empty().then_some(device_id);
    let (receipt, review) = plan(&session.conversation, selected_device, &args, now_unix_ms)?;
    let directory = original_directory(session, device_id, &receipt.output.directory, now_unix_ms)?;
    let action = FilePatchAction::RestoreText {
        directory: directory.clone(),
        file_name: receipt.output.original_file_name.clone(),
        recovery_id: receipt.output.recovery.recovery_id.clone(),
        expected_current: review.expected_current.clone(),
    };
    action.validate_text_mutation().map_err(|_| unavailable())?;
    Ok(TextMutationPreflight {
        capability: capability.clone(),
        provider_id: provider.wire.provider_id.clone(),
        surface: ProductSurface::OssPersonalOwner,
        operation_scope: canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .ok_or_else(unavailable)?
        .operations,
        target: directory.clone(),
        action,
        canonical_input_digest_sha256: format!(
            "{:x}",
            Sha256::digest(canonical_input_json.as_bytes())
        ),
        canonical_input_json,
        resource_scope: fresh_object_resource_scope(std::slice::from_ref(&directory)),
        // Directory references do not expire; dispatch revalidates approval.
        valid_until_unix_ms: u64::MAX,
        restore: Some(review),
    })
}

fn valid_change(change: &TextFileChange) -> bool {
    let text = |value: &str| value.len() <= MAX_TEXT_CHANGE_FIELD_BYTES && !value.contains('\0');
    match change {
        TextFileChange::Write { content_utf8 } => text(content_utf8),
        TextFileChange::Edit { edits } => {
            (1..=MAX_TEXT_EDITS).contains(&edits.len())
                && edits.iter().all(|edit| {
                    !edit.old_text.is_empty() && text(&edit.old_text) && text(&edit.new_text)
                })
        }
    }
}

/// Review of a persisted canonical exact input against this conversation's
/// immutable results. `None` means the input is not reviewable and must not
/// be approved. This never authorizes; dispatch revalidates everything.
pub fn review_exact_input(
    tool: &str,
    canonical: Option<&str>,
    history: &[crate::chat::ChatMessage],
) -> Option<TextChangeReview> {
    let canonical = canonical
        .filter(|text| text.len() <= crate::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES)?;
    if tool == RESTORE_TEXT_TOOL {
        let args: RestoreArgs = serde_json::from_str(canonical).ok()?;
        let (receipt, review) = plan(history, None, &args, 0).ok()?;
        return Some(TextChangeReview {
            file_name: receipt.output.original_file_name,
            operation: TextFileMutationOperation::Restore,
            source_result_call_id: args.mutation_result_call_id,
            expected_sha256: None,
            change: None,
            restore: Some(review),
        });
    }
    let (operation, id, expected_sha256, change) = match tool {
        UPDATE_TEXT_TOOL => {
            let args: UpdateArgs = serde_json::from_str(canonical).ok()?;
            if !valid_change(&args.change) {
                return None;
            }
            (
                TextFileMutationOperation::Update,
                args.file_result_call_id,
                args.expected_sha256,
                Some(args.change),
            )
        }
        DELETE_TEXT_TOOL => {
            let args: DeleteArgs = serde_json::from_str(canonical).ok()?;
            (
                TextFileMutationOperation::Delete,
                args.file_result_call_id,
                args.expected_sha256,
                None,
            )
        }
        _ => return None,
    };
    let evidence = resolve_file_result_from_history(history, &id, 0).ok()?;
    let file_name = evidence.file_name();
    if file_name.is_empty()
        || file_name.len() > 512
        || file_name.chars().any(char::is_control)
        || evidence.sha256() != expected_sha256
    {
        return None;
    }
    Some(TextChangeReview {
        file_name: file_name.into(),
        operation,
        source_result_call_id: id,
        expected_sha256: Some(expected_sha256),
        change,
        restore: None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, root_directory};
    use super::*;
    use crate::chat::{ChatMessage, ToolCallRef};
    use crate::session::PersistedAgentSession;
    use desk_agent_protocol::computer_use::{
        ComputerActionStepFact, CreatedFileArtifactOutput, FileRecoveryDescriptor, TextEdit,
    };
    use desk_agent_protocol::data_lineage::{ContentRef, DestinationIdentity};

    const NOW: u64 = 1_000;

    fn sha(text: &str) -> String {
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    fn owner() -> ChatMessage {
        crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Undo the change to notes.txt".into(),
            DestinationIdentity::Model {
                connection_id: "model".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
    }

    /// Append one tool call and its device result with a valid envelope.
    fn record(
        session: &mut PersistedAgentSession,
        call_id: &str,
        tool: &str,
        provider: &str,
        text: String,
    ) {
        let owner = owner();
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
        result
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = provider.into();
        session.conversation.push(result);
    }

    fn mutation(
        operation: TextFileMutationOperation,
        verified: bool,
        updated: Option<&str>,
    ) -> TextFileMutationOutput {
        let file = ObjectRef {
            token: "file".into(),
            snapshot_id: "file-snapshot".into(),
            object_kind: ObjectKind::File,
            expires_at: "2099-01-01T00:00:00Z".into(),
        };
        TextFileMutationOutput {
            operation,
            original: if operation == TextFileMutationOperation::Restore {
                root_directory()
            } else {
                file.clone()
            },
            directory: root_directory(),
            original_file_name: "notes.txt".into(),
            original_size_bytes: 3,
            original_sha256: Some(sha("old")),
            recovery: FileRecoveryDescriptor {
                recovery_id: "c".repeat(64),
                created_at_unix_ms: 500,
                expires_at_unix_ms: 9_000,
                cleanup_pending: false,
            },
            verified,
            updated_sha256: updated.filter(|_| verified).map(sha),
            updated_file: updated.filter(|_| verified).map(|text| {
                let file = ObjectRef {
                    token: "updated".into(),
                    ..file.clone()
                };
                CreatedFileArtifactOutput {
                    file: file.clone(),
                    file_name: "notes.txt".into(),
                    media_type: TEXT_ARTIFACT_MEDIA_TYPE.into(),
                    size_bytes: text.len() as u64,
                    digest_sha256: sha(text),
                    content: ContentRef::Artifact {
                        artifact_id: file.token,
                        sha256: sha(text),
                        size_bytes: text.len() as u64,
                        media_type: TEXT_ARTIFACT_MEDIA_TYPE.into(),
                    },
                }
            }),
        }
    }

    fn record_mutation(
        session: &mut PersistedAgentSession,
        call_id: &str,
        output: TextFileMutationOutput,
    ) {
        let tool = match output.operation {
            TextFileMutationOperation::Update => UPDATE_TEXT_TOOL,
            TextFileMutationOperation::Delete => DELETE_TEXT_TOOL,
            TextFileMutationOperation::Restore => RESTORE_TEXT_TOOL,
        };
        let completed = ComputerActionCompleted {
            work_id: "work".into(),
            action_request_id: call_id.into(),
            execution_generation: "generation".into(),
            result: if output.verified {
                ComputerActionResultClass::Verified
            } else {
                ComputerActionResultClass::OutcomeUnknown
            },
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: output.verified,
                summary: "text mutation".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::TextFileMutation(output)),
        };
        record(
            session,
            call_id,
            tool,
            crate::ai_assistant::TEXT_FILE_PROVIDER_ID,
            serde_json::to_string(&completed).unwrap(),
        );
    }

    fn record_read(session: &mut PersistedAgentSession, call_id: &str, name: &str, content: &str) {
        let read = FileContentReadOutput {
            file: ObjectRef {
                token: "current".into(),
                snapshot_id: "current-snapshot".into(),
                object_kind: ObjectKind::File,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            display_name: name.into(),
            content_utf8: content.into(),
            byte_len: content.len() as u64,
            sha256: sha(content),
            total_lines: 1,
            start_line: 1,
            end_line: 1,
            truncated: false,
            next_start_byte: None,
            matches: vec![],
        };
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        let capability = registry
            .capability_for_tool(crate::tool_names::READ_TEXT_FILE)
            .unwrap();
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
        record(
            session,
            call_id,
            crate::tool_names::READ_TEXT_FILE,
            &provider,
            serde_json::to_string(&desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::FileContentRead(read),
            ))
            .unwrap(),
        );
    }

    fn restore_call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "restore-call".into(),
            name: RESTORE_TEXT_TOOL.into(),
            arguments_json: args.to_string(),
        }
    }

    fn preflight(
        session: &PersistedAgentSession,
        args: serde_json::Value,
    ) -> Result<TextMutationPreflight, AgentError> {
        TextMutationPreflight::from_device_session(
            session,
            &session.device_id,
            ProductSurface::OssPersonalOwner,
            &restore_call(args),
            NOW,
        )
    }

    fn approve(session: &mut PersistedAgentSession, request_id: &str, token: &str) {
        use crate::file_scope::{DirectoryConsentSource, DirectoryProposal};
        let subject = session
            .file_scope_subject("owner", "device", "conversation")
            .unwrap();
        let revision = session.file_scope.revision();
        session
            .file_scope
            .propose(
                &subject,
                revision,
                DirectoryProposal {
                    request_id: request_id.into(),
                    requested_path: format!("/private/tmp/{token}"),
                    canonical_path: format!("/private/tmp/{token}"),
                    purpose: "other work".into(),
                    source: DirectoryConsentSource::ModelProposal,
                    directory: ObjectRef {
                        token: token.into(),
                        snapshot_id: format!("{token}-snapshot"),
                        object_kind: ObjectKind::Directory,
                        expires_at: String::new(),
                    },
                },
                1,
            )
            .unwrap();
        let revision = session.file_scope.revision();
        session
            .file_scope
            .decide(&subject, revision, request_id, true, 1)
            .unwrap();
    }

    #[test]
    fn restoring_an_update_expects_its_result_and_acts_on_the_original_directory() {
        let (mut session, _, _) = fixture();
        record_mutation(
            &mut session,
            "update-call",
            mutation(TextFileMutationOperation::Update, true, Some("new")),
        );
        // A later approval makes the single-root rule ambiguous; the restore
        // still selects the directory the update was dispatched in.
        approve(&mut session, "later", "later-root");
        let args = serde_json::json!({"mutation_result_call_id":"update-call"});
        let plan = preflight(&session, args.clone()).unwrap();
        assert_eq!(plan.target(), &root_directory());
        assert_eq!(plan.resource_scope().len(), 1);
        assert_eq!(
            plan.action(),
            &FilePatchAction::RestoreText {
                directory: root_directory(),
                file_name: "notes.txt".into(),
                recovery_id: "c".repeat(64),
                expected_current: RestoreExpectation::Sha(sha("new")),
            }
        );
        assert_eq!(
            ComputerActionKind::File(plan.action().clone()).required_capability(),
            Capability::FilePatchConfirmed
        );
        let restore = plan.restore().unwrap();
        assert!(!restore.overrides_non_ai_version);
        assert_eq!(
            restore.original_operation,
            TextFileMutationOperation::Update
        );
        assert_eq!(restore.backup_created_at_unix_ms, 500);
        crate::file_scope::validate_device_artifact_scope(
            &session,
            "device",
            RESTORE_TEXT_TOOL,
            plan.resource_scope(),
            NOW,
        )
        .unwrap();
        // Frozen wire and the device receipt agree with the preflight.
        let action = ComputerActionKind::File(plan.action().clone());
        let call = restore_call(args);
        assert_eq!(
            frozen_resources(&call, plan.target(), &action).unwrap(),
            plan.resource_scope()
        );
        let mut receipt = mutation(TextFileMutationOperation::Restore, true, Some("old"));
        receipt.original_sha256 = Some(sha("new"));
        let completed = ComputerActionCompleted {
            work_id: "work".into(),
            action_request_id: "restore".into(),
            execution_generation: "generation".into(),
            result: ComputerActionResultClass::Verified,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: true,
                summary: "restored".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::TextFileMutation(receipt)),
        };
        validate_completion(plan.target(), &action, &completed).unwrap();
        // Revoking the original approval blocks the restore even though
        // another directory remains approved.
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
            serde_json::json!({"mutation_result_call_id":"update-call"}),
        )
        .err()
        .unwrap();
        assert!(error.message.contains("revoked or expired"));
    }

    #[test]
    fn a_read_version_overrides_and_an_unknown_outcome_requires_one() {
        let (mut session, _, _) = fixture();
        record_mutation(
            &mut session,
            "unknown-call",
            mutation(TextFileMutationOperation::Update, false, None),
        );
        let error = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"unknown-call"}),
        )
        .err()
        .unwrap();
        assert!(error.message.contains("file_version_id"));
        assert!(
            validate_mutation_permission_input(
                &session,
                RESTORE_TEXT_TOOL,
                Some(r#"{"mutation_result_call_id":"unknown-call"}"#),
                NOW
            )
            .is_err()
        );
        record_read(&mut session, "read-now", "notes.txt", "edited by owner");
        let plan = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"unknown-call","file_result_call_id":"read-now"}),
        )
        .unwrap();
        let restore = plan.restore().unwrap();
        assert_eq!(
            restore.expected_current,
            RestoreExpectation::Sha(sha("edited by owner"))
        );
        assert!(restore.overrides_non_ai_version);
        // A verified update read back unchanged is not an override.
        record_mutation(
            &mut session,
            "update-call",
            mutation(TextFileMutationOperation::Update, true, Some("new")),
        );
        record_read(&mut session, "read-same", "notes.txt", "new");
        let same = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"update-call","file_result_call_id":"read-same"}),
        )
        .unwrap();
        assert!(!same.restore().unwrap().overrides_non_ai_version);
        // A version of another file cannot stand in for the current state.
        record_read(&mut session, "read-other", "other.txt", "new");
        assert!(
            preflight(
                &session,
                serde_json::json!({"mutation_result_call_id":"update-call","file_result_call_id":"read-other"}),
            )
            .is_err()
        );
        // Only text mutations can be restored.
        assert!(
            preflight(
                &session,
                serde_json::json!({"mutation_result_call_id":"read-now"}),
            )
            .is_err()
        );
    }

    #[test]
    fn deletions_and_removing_restores_expect_an_absent_file() {
        let (mut session, _, _) = fixture();
        record_mutation(
            &mut session,
            "delete-call",
            mutation(TextFileMutationOperation::Delete, true, None),
        );
        let plan = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"delete-call"}),
        )
        .unwrap();
        assert!(matches!(
            plan.action(),
            FilePatchAction::RestoreText {
                expected_current: RestoreExpectation::Absent,
                ..
            }
        ));
        // Undoing the restore of a deletion deletes the file again; undoing a
        // restore that removed the file recreates it.
        record_mutation(
            &mut session,
            "restore-call-1",
            mutation(TextFileMutationOperation::Restore, true, Some("old")),
        );
        let undo = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"restore-call-1"}),
        )
        .unwrap();
        assert_eq!(
            undo.restore().unwrap().expected_current,
            RestoreExpectation::Sha(sha("old"))
        );
        record_mutation(
            &mut session,
            "restore-call-2",
            mutation(TextFileMutationOperation::Restore, true, None),
        );
        let recreate = preflight(
            &session,
            serde_json::json!({"mutation_result_call_id":"restore-call-2"}),
        )
        .unwrap();
        assert_eq!(
            recreate.restore().unwrap().expected_current,
            RestoreExpectation::Absent
        );
        // A verified restore that wrote the file is a reusable file version.
        let version =
            resolve_file_result_from_history(&session.conversation, "restore-call-1", NOW).unwrap();
        assert_eq!(version.sha256(), sha("old"));
    }

    #[test]
    fn review_projects_the_change_per_file_without_ids_or_digests() {
        let (mut session, evidence, _) = fixture();
        record_read(&mut session, "read-call", "notes.txt", "old");
        let edits = vec![TextEdit {
            old_text: "old".into(),
            new_text: "new".into(),
            replace_all: false,
        }];
        let canonical = serde_json::json!({
            "directory_request_id":"directory","file_result_call_id":"read-call",
            "expected_sha256":evidence.sha256(),"change":{"kind":"edit","edits":edits}
        })
        .to_string();
        let review =
            review_exact_input(UPDATE_TEXT_TOOL, Some(&canonical), &session.conversation).unwrap();
        let projection = review.projection();
        assert_eq!(
            projection,
            serde_json::json!({"file_name":"notes.txt","operation":"update","edits":edits})
        );
        record_mutation(
            &mut session,
            "update-call",
            mutation(TextFileMutationOperation::Update, true, Some("new")),
        );
        let restore = review_exact_input(
            RESTORE_TEXT_TOOL,
            Some(r#"{"mutation_result_call_id":"update-call"}"#),
            &session.conversation,
        )
        .unwrap()
        .projection();
        assert_eq!(restore["restores_before"], "update");
        assert_eq!(restore["overrides_non_ai_version"], false);
        assert!(!restore.to_string().contains("update-call"));
        // The AI reviewer receives the projection, not the exact input.
        let item = crate::dynamic_run::GrantRequestItem {
            item_id: "mutation".into(),
            provider_id: crate::ai_assistant::TEXT_FILE_PROVIDER_ID.into(),
            tool_name: UPDATE_TEXT_TOOL.into(),
            expected_effect:
                desk_agent_protocol::capability_provider::CapabilityEffect::WriteArtifact,
            resource_scope: vec![],
            operation_scope: vec![],
            export_destinations: vec![],
            canonical_input_digest_sha256: Some(sha(&canonical)),
            canonical_input_json: Some(canonical.clone()),
            command_confirmation: None,
            selected_device_target: None,
            launch_confirmation: None,
            suggested_ttl_seconds: 60,
            suggested_max_uses: 1,
            reason: "edit".into(),
        };
        let action: serde_json::Value = serde_json::from_str(
            &crate::approval_review::permission_item_action_json(
                &session.conversation,
                &item,
                crate::approval_review::ApprovalInputKind::ExactCall,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(action["text_change"], projection);
        assert!(action.get("exact_input").is_none());
        assert!(!action.to_string().contains("read-call"));
        assert!(
            crate::approval_review::permission_item_action_json(
                &[],
                &item,
                crate::approval_review::ApprovalInputKind::ExactCall,
            )
            .is_err()
        );
        // Over-budget input is not reviewable.
        let oversized = "x".repeat(crate::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES + 1);
        assert!(
            review_exact_input(UPDATE_TEXT_TOOL, Some(&oversized), &session.conversation).is_none()
        );
    }
}
