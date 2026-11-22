//! `restore_command_changes` preflight and the owner/AI review of its input.
//!
//! A restore names the result of an earlier AI command (or of an earlier
//! restore of one). That receipt is immutable device evidence in this
//! conversation: it fixes the recovery record, the snapshot roots and the
//! protected changes. One call restores entries of one root. Authority is
//! current, never inherited from the command's approval: a root from an
//! approved directory needs that approval to still be valid, and a root from
//! the command's `cwd` needs the device execution mode to still admit mutating
//! commands.
use super::*;
use desk_agent_protocol::ExecutionMode;
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
};
use desk_agent_protocol::exec::{
    CommandChangeRestore, ExecFileChangeKind, ExecFileChangeRoot, ExecFileChanges,
    ExecSnapshotOutcome, ExecSnapshotRoot, MAX_COMMAND_RESTORE_ENTRIES,
};

pub const COMMAND_RESTORE_TOOL: &str = "restore_command_changes";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandRestoreArgs {
    pub command_result_call_id: String,
    pub root: u16,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

/// What a command restore puts back, for the approval card and the AI review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRestoreReview {
    /// Result whose recorded changes are undone.
    pub source_result_call_id: String,
    pub root: ExecSnapshotRoot,
    /// Execution window of the undone command or restore.
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    pub entries: Vec<CommandChangeRestore>,
}

/// What restoring one entry does to the file.
fn restore_operation(kind: ExecFileChangeKind) -> &'static str {
    match kind {
        ExecFileChangeKind::Added => "delete",
        ExecFileChangeKind::Modified => "rewrite_previous",
        ExecFileChangeKind::Deleted => "recreate",
    }
}

impl CommandRestoreReview {
    /// Model view for the approval reviewer: files and operations without
    /// digests or references, and the warning the owner also sees.
    pub fn projection(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "root_kind": match &self.root {
                ExecSnapshotRoot::Cwd { .. } => "command_working_directory",
                ExecSnapshotRoot::Grant { .. } => "approved_directory",
            },
            "execution_started_at_unix_ms": self.started_at_unix_ms,
            "execution_finished_at_unix_ms": self.finished_at_unix_ms,
            "files": self.entries.iter().map(|entry| serde_json::json!({
                "path": entry.path,
                "change": entry.kind,
                "restore": restore_operation(entry.kind),
            })).collect::<Vec<_>>(),
            "warning": "The change list covers everything that changed under this directory while the command ran, which may include changes the command did not make, such as the owner's or the AI's own edits in that window.",
        });
        if let ExecSnapshotRoot::Cwd { path } = &self.root {
            value["working_directory"] = serde_json::json!(path);
        }
        value
    }
}

fn invalid(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false, true)
}

fn not_found() -> AgentError {
    invalid(
        "command_result_id must be the result ID of an exec_command or restore_command_changes result in this conversation that reports file_changes. No approval card was created.",
    )
}

/// Resolve the change report of an earlier command or command restore in this
/// conversation. Only the conversation's own authenticated results count.
pub(crate) fn command_changes(
    history: &[crate::chat::ChatMessage],
    call_id: &str,
) -> Result<ExecFileChanges, AgentError> {
    use crate::chat::ChatRole;
    if call_id.is_empty() || call_id.len() > 256 || call_id.chars().any(char::is_control) {
        return Err(not_found());
    }
    let mut calls = history
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == call_id);
    let call = calls.next().ok_or_else(not_found)?;
    if calls.next().is_some()
        || !matches!(
            call.name.as_str(),
            crate::exec_tools::EXEC_TOOL_NAME | COMMAND_RESTORE_TOOL
        )
    {
        return Err(not_found());
    }
    let mut selected: Option<ExecFileChanges> = None;
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
            || envelope.content.size_bytes() != result.text.len() as u64
        {
            continue;
        }
        let changes = if call.name == COMMAND_RESTORE_TOOL {
            if envelope.provenance.source_provider_id != crate::ai_agent::TEXT_FILE_PROVIDER_ID {
                continue;
            }
            let Ok(completed) = serde_json::from_str::<ComputerActionCompleted>(&result.text)
            else {
                continue;
            };
            match (completed.result, completed.output) {
                (
                    ComputerActionResultClass::Verified | ComputerActionResultClass::OutcomeUnknown,
                    Some(ComputerActionOutput::CommandChangesRestore(output)),
                ) => output.file_changes,
                _ => continue,
            }
        } else {
            // Large streams may be stored as attachments; the change report
            // stays inline in the command result either way.
            let Some(changes) = serde_json::from_str::<serde_json::Value>(&result.text)
                .ok()
                .and_then(|value| value.pointer("/Exec/file_changes").cloned())
                .and_then(|value| serde_json::from_value::<ExecFileChanges>(value).ok())
            else {
                continue;
            };
            changes
        };
        if selected.as_ref().is_some_and(|prior| *prior != changes) {
            return Err(not_found());
        }
        selected = Some(changes);
    }
    let changes = selected.ok_or_else(not_found)?;
    let roots: Vec<_> = changes.roots.iter().map(|root| root.root.clone()).collect();
    if changes.snapshot != ExecSnapshotOutcome::Taken {
        return Err(invalid(
            "That command ran without a snapshot, so its file changes are not protected and cannot be restored. No approval card was created.",
        ));
    }
    changes.validate_for(&roots).map_err(|_| not_found())?;
    Ok(changes)
}

/// The root and protected entries a restore input selects, without any
/// authorization.
fn plan(
    history: &[crate::chat::ChatMessage],
    args: &CommandRestoreArgs,
) -> Result<(ExecFileChanges, ExecFileChangeRoot, CommandRestoreReview), AgentError> {
    let changes = command_changes(history, &args.command_result_call_id)?;
    let root = changes
        .roots
        .get(usize::from(args.root))
        .cloned()
        .ok_or_else(|| {
            invalid(format!(
                "root must be the index of one of the {} snapshot roots in that result's file_changes.roots. No approval card was created.",
                changes.roots.len()
            ))
        })?;
    let protected: Vec<_> = changes.protected_entries(args.root).collect();
    let entries: Vec<CommandChangeRestore> = match &args.paths {
        None => protected
            .iter()
            .map(|change| CommandChangeRestore::from_change(change))
            .collect(),
        Some(paths) => {
            if paths.is_empty() {
                return Err(invalid(
                    "paths must name at least one file; omit it to restore every protected change of the root. No approval card was created.",
                ));
            }
            for (index, path) in paths.iter().enumerate() {
                if paths[..index].contains(path)
                    || !protected.iter().any(|change| change.path == *path)
                {
                    return Err(invalid(format!(
                        "paths entry {path:?} is not a protected change of root {} in that result (or is repeated). Use paths exactly as listed in file_changes with protected=true. No approval card was created.",
                        args.root
                    )));
                }
            }
            protected
                .iter()
                .filter(|change| paths.contains(&change.path))
                .map(|change| CommandChangeRestore::from_change(change))
                .collect()
        }
    };
    if entries.is_empty() {
        return Err(invalid(
            "That root has no protected change to restore. No approval card was created.",
        ));
    }
    if entries.len() > MAX_COMMAND_RESTORE_ENTRIES {
        return Err(invalid(format!(
            "At most {MAX_COMMAND_RESTORE_ENTRIES} files can be restored in one call; pass paths to restore a subset. No approval card was created."
        )));
    }
    if !receipt_fits(&changes, args.root, &entries) {
        return Err(invalid(format!(
            "The receipt of restoring these {} files would exceed the {} KiB report limit; pass paths to restore them in smaller groups. No approval card was created.",
            entries.len(),
            desk_agent_protocol::exec::MAX_EXEC_FILE_CHANGES_BYTES / 1024
        )));
    }
    let review = CommandRestoreReview {
        source_result_call_id: args.command_result_call_id.clone(),
        root: root.root.clone(),
        started_at_unix_ms: changes.started_at_unix_ms,
        finished_at_unix_ms: changes.finished_at_unix_ms,
        entries,
    };
    Ok((changes, root, review))
}

fn denied(message: &str) -> AgentError {
    error(AgentErrorKind::PermissionDenied, message, false, true)
}

/// Current authority over the root: its directory approval for a granted
/// root, the device execution mode for a working-directory root.
fn authorize_root(
    session: &crate::session::PersistedAgentSession,
    device_id: &str,
    root: &ExecSnapshotRoot,
    execution_mode: Option<ExecutionMode>,
    now_unix_ms: u64,
) -> Result<(), AgentError> {
    match root {
        ExecSnapshotRoot::Grant {
            directory_request_id,
            directory,
        } => {
            crate::file_scope::TrustedDirectoryGrants::from_session(session, device_id, now_unix_ms)
                .find(directory_request_id, directory)
                .ok_or_else(|| {
                    denied(
                        "The directory approval this command's snapshot covered was revoked. A restore needs that directory approved for this conversation; the owner can still export the backup from file backup management. No approval card was created.",
                    )
                })?;
        }
        ExecSnapshotRoot::Cwd { .. } => {
            if !execution_mode.is_some_and(ExecutionMode::allows_confirmed_execution) {
                return Err(denied(
                    "The device execution policy no longer allows commands that change files, so changes in the command's working directory cannot be restored. The owner can still export the backup from file backup management. No approval card was created.",
                ));
            }
        }
    }
    Ok(())
}

/// The restore receipt reports the inverse of every restored entry so the
/// restore can be undone; it must stay within the report size limit even when
/// every restored file is reported at the largest size.
fn receipt_fits(changes: &ExecFileChanges, root: u16, entries: &[CommandChangeRestore]) -> bool {
    let mut receipt = changes.clone();
    receipt.truncated = false;
    receipt.changes = entries
        .iter()
        .map(|entry| desk_agent_protocol::exec::ExecFileChange {
            path: entry.path.clone(),
            root,
            kind: entry.inverse_kind(),
            before_sha256: entry.after_sha256.clone(),
            after_sha256: entry.before_sha256.clone(),
            size_bytes: u64::MAX,
            protected: true,
        })
        .collect();
    serde_json::to_vec(&receipt)
        .is_ok_and(|bytes| bytes.len() <= desk_agent_protocol::exec::MAX_EXEC_FILE_CHANGES_BYTES)
}

pub(super) fn build(
    session: &crate::session::PersistedAgentSession,
    device_id: &str,
    call: &ToolCall,
    execution_mode: Option<ExecutionMode>,
    now_unix_ms: u64,
) -> Result<TextMutationPreflight, AgentError> {
    let registry = crate::ai_agent::ai_agent_provider_registry();
    let capability = registry
        .capability_for_tool(COMMAND_RESTORE_TOOL)
        .ok_or_else(unavailable)?;
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .ok_or_else(unavailable)?;
    if call.name != COMMAND_RESTORE_TOOL || now_unix_ms == 0 {
        return Err(unavailable());
    }
    let canonical_input_json = canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
    )
    .map_err(|_| unavailable())?;
    check_input_budget(&canonical_input_json)?;
    let args: CommandRestoreArgs =
        serde_json::from_str(&canonical_input_json).map_err(|_| unavailable())?;
    let (changes, root, review) = plan(&session.conversation, &args)?;
    authorize_root(session, device_id, &root.root, execution_mode, now_unix_ms)?;
    let action = FilePatchAction::RestoreCommandChanges {
        recovery_id: changes.recovery_id.clone().ok_or_else(unavailable)?,
        root: root.root.clone(),
        entries: review.entries.clone(),
    };
    action
        .validate_command_restore()
        .map_err(|_| unavailable())?;
    let target = root.directory;
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
        resource_scope: fresh_object_resource_scope(std::slice::from_ref(&target)),
        target,
        action,
        canonical_input_digest_sha256: format!(
            "{:x}",
            Sha256::digest(canonical_input_json.as_bytes())
        ),
        canonical_input_json,
        // Directory references do not expire; dispatch revalidates authority.
        valid_until_unix_ms: u64::MAX,
        restore: None,
        command_restore: Some(review),
    })
}

/// Review of a persisted canonical restore input against this conversation's
/// immutable results. `None` means the input is not reviewable and must not
/// be approved. This never authorizes; dispatch revalidates everything.
pub fn review_command_restore_input(
    canonical: Option<&str>,
    history: &[crate::chat::ChatMessage],
) -> Option<CommandRestoreReview> {
    let canonical = canonical
        .filter(|text| text.len() <= crate::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES)?;
    let args: CommandRestoreArgs = serde_json::from_str(canonical).ok()?;
    plan(history, &args).ok().map(|(_, _, review)| review)
}

/// Identical receipt interpretation for OSS and Manager.
pub fn validate_command_restore_completion(
    target: &ObjectRef,
    action: &ComputerActionKind,
    completed: &ComputerActionCompleted,
) -> Result<(), AgentError> {
    let ComputerActionKind::File(
        action @ FilePatchAction::RestoreCommandChanges { root, entries, .. },
    ) = action
    else {
        return Err(unavailable());
    };
    action
        .validate_command_restore()
        .map_err(|_| unavailable())?;
    let Some(ComputerActionOutput::CommandChangesRestore(output)) = &completed.output else {
        return if completed.output.is_none()
            && matches!(
                completed.result,
                ComputerActionResultClass::DefinitelyNotStarted
                    | ComputerActionResultClass::Failed
                    | ComputerActionResultClass::OutcomeUnknown
            )
            && completed
                .facts
                .iter()
                .all(|fact| !fact.changed && !fact.verified)
        {
            Ok(())
        } else {
            Err(unavailable())
        };
    };
    output
        .validate_for(root, entries)
        .map_err(|_| unavailable())?;
    let changed = output.file_changes.snapshot == ExecSnapshotOutcome::Taken;
    if completed.result
        != if output.verified() {
            ComputerActionResultClass::Verified
        } else {
            ComputerActionResultClass::OutcomeUnknown
        }
        || completed.facts.len() != 1
        || completed.facts[0].index != 0
        || completed.facts[0].changed != changed
        || completed.facts[0].verified != output.verified()
        || output
            .file_changes
            .roots
            .first()
            .is_some_and(|reported| reported.directory != *target)
    {
        return Err(unavailable());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
