//! Owner-only review of a persisted `restore_command_changes` input. No
//! references or grants are minted here; dispatch remains responsible for
//! current authorization.
use desk_agent_protocol::exec::{ExecFileChangeKind, ExecSnapshotRoot, ExecSnapshotRootKind};
use desk_diagnose_core::chat::ChatMessage;
use serde::Serialize;
use utoipa::ToSchema;

/// One command restore awaiting approval. The card always warns that the list
/// may include changes the command did not make, including the owner's or the
/// AI's own edits during the execution window.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandRestoreConfirmationDto {
    /// Result whose recorded changes are undone.
    pub source_result_call_id: String,
    pub root_kind: ExecSnapshotRootKind,
    /// The command's working directory, for a working-directory root.
    pub root_path: Option<String>,
    /// The directory approval, for a root from an approved directory.
    pub directory_request_id: Option<String>,
    pub execution_started_at_unix_ms: u64,
    pub execution_finished_at_unix_ms: u64,
    pub files: Vec<CommandRestoreFileDto>,
    pub one_shot: bool,
}

/// One file the restore puts back. `change` is what the command did: an added
/// file is deleted, a modified file gets its previous content, a deleted file
/// is recreated.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandRestoreFileDto {
    pub path: String,
    pub change: ExecFileChangeKind,
}

pub(super) fn project(
    canonical: Option<&str>,
    messages: &[ChatMessage],
) -> Option<CommandRestoreConfirmationDto> {
    let review = desk_diagnose_core::provider_preflight::text_file::review_command_restore_input(
        canonical, messages,
    )?;
    let (root_kind, root_path, directory_request_id) = match review.root {
        ExecSnapshotRoot::Cwd { path } => (ExecSnapshotRootKind::Cwd, Some(path), None),
        ExecSnapshotRoot::Grant {
            directory_request_id,
            ..
        } => (
            ExecSnapshotRootKind::Grant,
            None,
            Some(directory_request_id),
        ),
    };
    Some(CommandRestoreConfirmationDto {
        source_result_call_id: review.source_result_call_id,
        root_kind,
        root_path,
        directory_request_id,
        execution_started_at_unix_ms: review.started_at_unix_ms,
        execution_finished_at_unix_ms: review.finished_at_unix_ms,
        files: review
            .entries
            .into_iter()
            .map(|entry| CommandRestoreFileDto {
                path: entry.path,
                change: entry.kind,
            })
            .collect(),
        one_shot: true,
    })
}
