//! Best-effort directory snapshots around a mutating AI command.
//!
//! The trusted central derives the snapshot roots of a mutating AI command and
//! seals them into [`crate::ExecInput`], [`super::ExecPlanDraft`] and
//! [`super::ExecPlan`] (fingerprinted). The device snapshots the text files under
//! those roots before the command runs, compares them afterwards and reports
//! [`ExecFileChanges`] in [`crate::ExecOutput`]. A later
//! `FilePatchAction::RestoreCommandChanges` puts protected entries back.
//!
//! Snapshots are protection, not authority: a root never widens what the
//! command may write. Non-AI executions (fleet, local `ConfirmExec`, Manager
//! PTY proxy) always carry no roots.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use super::CommandContractError;
use crate::computer_use::{ObjectKind, ObjectRef};

/// Upper bound on the roots one command plan may snapshot. A session with more
/// approved directories must name a `cwd` for a mutating command.
pub const MAX_EXEC_SNAPSHOT_ROOTS: usize = 32;
/// Files one snapshot scan may cover before the snapshot is skipped.
pub const EXEC_SNAPSHOT_MAX_FILES: u32 = 20_000;
/// Wall time of one scan (before or after the command).
pub const EXEC_SNAPSHOT_SCAN_LIMIT_MS: u32 = 10_000;
/// Time both scans may add to a command; host result deadlines and the device
/// capacity slot grace include it. It is not taken from `timeout_ms`.
pub const EXEC_SNAPSHOT_TIME_BUDGET_MS: u32 = 2 * EXEC_SNAPSHOT_SCAN_LIMIT_MS;
/// Largest text file whose content a snapshot keeps.
pub const EXEC_SNAPSHOT_MAX_FILE_BYTES: u64 = crate::computer_use::MAX_TEXT_FILE_BYTES;
/// Directory names a snapshot never scans, besides `.gitignore` and `.ignore`
/// rules found under the root.
pub const EXEC_SNAPSHOT_BUILTIN_EXCLUDES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];
/// Serialized size bound of one [`ExecFileChanges`] report.
pub const MAX_EXEC_FILE_CHANGES_BYTES: usize = 32 * 1024;
/// Entries one `RestoreCommandChanges` action may carry.
pub const MAX_COMMAND_RESTORE_ENTRIES: usize = 512;
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_REQUEST_ID_BYTES: usize = 256;

/// One directory a mutating command's snapshot covers.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecSnapshotRoot {
    /// The command's own working directory, as the raw `cwd` string. The
    /// worker opens and canonicalizes it.
    Cwd { path: String },
    /// A directory the session currently has approved on the target device.
    Grant {
        directory_request_id: String,
        directory: ObjectRef,
    },
}

impl ExecSnapshotRoot {
    pub fn validate(&self) -> Result<(), CommandContractError> {
        match self {
            Self::Cwd { path } => {
                super::validate_command_text("snapshot_root", path, MAX_RELATIVE_PATH_BYTES)
            }
            Self::Grant {
                directory_request_id,
                directory,
            } => {
                super::validate_command_text(
                    "snapshot_root",
                    directory_request_id,
                    MAX_REQUEST_ID_BYTES,
                )?;
                if directory_request_id.chars().any(char::is_control)
                    || !is_directory_ref(directory)
                {
                    return Err(CommandContractError::InvalidField("snapshot_root"));
                }
                Ok(())
            }
        }
    }

    /// Validate a sealed root list: bounded, each valid, no duplicates.
    pub fn validate_all(roots: &[Self]) -> Result<(), CommandContractError> {
        if roots.len() > MAX_EXEC_SNAPSHOT_ROOTS {
            return Err(CommandContractError::InvalidField("snapshot_roots"));
        }
        for (index, root) in roots.iter().enumerate() {
            root.validate()?;
            if roots[..index].contains(root) {
                return Err(CommandContractError::InvalidField("snapshot_roots"));
            }
        }
        Ok(())
    }

    /// Fingerprint contribution, kept explicit so reordering variants or
    /// fields never shifts an approved plan's fingerprint silently.
    pub(crate) fn feed_fingerprint(&self, feed: &mut impl FnMut(&[u8])) {
        match self {
            Self::Cwd { path } => {
                feed(&[0]);
                feed(path.as_bytes());
            }
            Self::Grant {
                directory_request_id,
                directory,
            } => {
                feed(&[1]);
                feed(directory_request_id.as_bytes());
                feed(directory.token.as_bytes());
                feed(directory.snapshot_id.as_bytes());
                feed(directory.expires_at.as_bytes());
            }
        }
    }
}

fn is_directory_ref(directory: &ObjectRef) -> bool {
    directory.object_kind == ObjectKind::Directory
        && !directory.token.trim().is_empty()
        && !directory.snapshot_id.trim().is_empty()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A `/`-separated path relative to its snapshot root, without `.`/`..`
/// components or a leading separator.
pub fn is_snapshot_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_RELATIVE_PATH_BYTES
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// What a root is, for the approval card.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSnapshotRootKind {
    Cwd,
    Grant,
}

/// One snapshot root as the owner sees it: the `cwd` string or the approved
/// directory's canonical path.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecSnapshotRootLabel {
    pub kind: ExecSnapshotRootKind,
    pub path: String,
}

/// Areas a best-effort snapshot does not detect or cannot restore.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSnapshotGap {
    /// Excluded directories and ignore-file matches are not scanned.
    ExcludedPaths,
    /// Changes outside the roots are neither detected nor restorable.
    OutsideRoots,
    /// Binary files are compared by metadata only and cannot be restored.
    BinaryFiles,
    /// Text files over [`EXEC_SNAPSHOT_MAX_FILE_BYTES`] are not kept.
    OversizedFiles,
    /// Files with more than one hard link are skipped.
    HardLinkedFiles,
    /// Permission, owner and timestamp changes are not restored.
    MetadataChanges,
    /// Processes that leave the command's process group may write after the
    /// final scan.
    DetachedProcesses,
}

/// Approval-card and preview data for a command whose plan has snapshot roots.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecSnapshotNotice {
    pub roots: Vec<ExecSnapshotRootLabel>,
    /// Directory names never scanned.
    pub excluded_names: Vec<String>,
    /// `.gitignore` and `.ignore` rules under a root are honored.
    pub honors_ignore_files: bool,
    pub max_files: u32,
    pub max_file_bytes: u64,
    pub scan_time_limit_ms: u32,
    /// Size of the device's latest manifest for these roots, when known.
    pub estimated_file_count: Option<u32>,
    pub unprotected: Vec<ExecSnapshotGap>,
}

impl ExecSnapshotNotice {
    pub fn new(roots: Vec<ExecSnapshotRootLabel>) -> Self {
        Self {
            roots,
            excluded_names: EXEC_SNAPSHOT_BUILTIN_EXCLUDES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            honors_ignore_files: true,
            max_files: EXEC_SNAPSHOT_MAX_FILES,
            max_file_bytes: EXEC_SNAPSHOT_MAX_FILE_BYTES,
            scan_time_limit_ms: EXEC_SNAPSHOT_SCAN_LIMIT_MS,
            estimated_file_count: None,
            unprotected: vec![
                ExecSnapshotGap::ExcludedPaths,
                ExecSnapshotGap::OutsideRoots,
                ExecSnapshotGap::BinaryFiles,
                ExecSnapshotGap::OversizedFiles,
                ExecSnapshotGap::HardLinkedFiles,
                ExecSnapshotGap::MetadataChanges,
                ExecSnapshotGap::DetachedProcesses,
            ],
        }
    }
}

// ============================ Device report ============================

/// Why a planned snapshot was not taken. The command still ran unprotected.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSnapshotSkipReason {
    /// The roots hold more than [`EXEC_SNAPSHOT_MAX_FILES`] files.
    FileLimit,
    /// A scan exceeded [`EXEC_SNAPSHOT_SCAN_LIMIT_MS`].
    TimeLimit,
    /// The worker runs as the system identity and keeps no snapshot.
    SystemIdentity,
    /// A root could not be opened as a directory.
    RootUnavailable,
    /// A root is a filesystem or volume root, the user's home directory, a
    /// system directory, or holds (or lies in) this product's configuration
    /// or data; such roots are never snapshotted.
    ProtectedRoot,
    /// The recovery storage refused the snapshot.
    StorageUnavailable,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum ExecSnapshotOutcome {
    Taken,
    Skipped(ExecSnapshotSkipReason),
}

/// A sealed root and the device-issued directory reference the worker opened
/// for it. For a [`ExecSnapshotRoot::Grant`] root the reference is that grant's
/// directory; a restore targets this reference.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecFileChangeRoot {
    pub root: ExecSnapshotRoot,
    pub directory: ObjectRef,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecFileChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One file that differs between the scans before and after the command.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecFileChange {
    /// Relative to its root, `/`-separated.
    pub path: String,
    /// Index into [`ExecFileChanges::roots`].
    pub root: u16,
    pub kind: ExecFileChangeKind,
    /// Content SHA-256 before the command; absent for an added or unhashed file.
    pub before_sha256: Option<String>,
    /// Content SHA-256 after the command; absent for a deleted or unhashed file.
    pub after_sha256: Option<String>,
    /// Size after the command, or before it for a deleted file.
    pub size_bytes: u64,
    /// The previous state is kept and can be restored.
    pub protected: bool,
}

impl ExecFileChange {
    fn validate(&self, root_count: usize) -> bool {
        let sha_ok = |sha: &Option<String>| sha.as_deref().is_none_or(is_sha256);
        let shape = match self.kind {
            ExecFileChangeKind::Added => {
                self.before_sha256.is_none() && (!self.protected || self.after_sha256.is_some())
            }
            ExecFileChangeKind::Modified => {
                !self.protected || (self.before_sha256.is_some() && self.after_sha256.is_some())
            }
            ExecFileChangeKind::Deleted => {
                self.after_sha256.is_none() && (!self.protected || self.before_sha256.is_some())
            }
        };
        usize::from(self.root) < root_count
            && is_snapshot_relative_path(&self.path)
            && sha_ok(&self.before_sha256)
            && sha_ok(&self.after_sha256)
            && shape
    }
}

/// Files a command changed under its snapshot roots. The list covers every
/// change in the roots during the execution window, including changes made by
/// other programs.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecFileChanges {
    pub snapshot: ExecSnapshotOutcome,
    /// Recovery record holding the snapshot; present when it was taken.
    pub recovery_id: Option<String>,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    /// Present when the snapshot was taken, in the order of the plan's roots.
    pub roots: Vec<ExecFileChangeRoot>,
    pub changes: Vec<ExecFileChange>,
    /// More changes happened than the report holds.
    pub truncated: bool,
}

impl ExecFileChanges {
    /// Check the report against the sealed roots it was produced for.
    pub fn validate_for(&self, plan_roots: &[ExecSnapshotRoot]) -> Result<(), &'static str> {
        let invalid = Err("invalid command file change report");
        if plan_roots.is_empty()
            || self.started_at_unix_ms == 0
            || self.finished_at_unix_ms < self.started_at_unix_ms
        {
            return invalid;
        }
        match self.snapshot {
            ExecSnapshotOutcome::Skipped(_) => {
                if self.recovery_id.is_some()
                    || !self.roots.is_empty()
                    || !self.changes.is_empty()
                    || self.truncated
                {
                    return invalid;
                }
            }
            ExecSnapshotOutcome::Taken => {
                if !self.recovery_id.as_deref().is_some_and(is_sha256)
                    || self.roots.len() != plan_roots.len()
                    || self.roots.iter().zip(plan_roots).any(|(reported, sealed)| {
                        reported.root != *sealed
                            || !is_directory_ref(&reported.directory)
                            || matches!(&reported.root, ExecSnapshotRoot::Grant { directory, .. } if *directory != reported.directory)
                    })
                {
                    return invalid;
                }
            }
        }
        for (index, change) in self.changes.iter().enumerate() {
            if !change.validate(self.roots.len())
                || self.changes[..index]
                    .iter()
                    .any(|prior| prior.root == change.root && prior.path == change.path)
            {
                return invalid;
            }
        }
        let size = serde_json::to_vec(self).map_err(|_| "invalid command file change report")?;
        if size.len() > MAX_EXEC_FILE_CHANGES_BYTES {
            return Err("command file change report exceeds its size limit");
        }
        Ok(())
    }

    /// The restorable entries of one root, in report order.
    pub fn protected_entries(&self, root: u16) -> impl Iterator<Item = &ExecFileChange> {
        self.changes
            .iter()
            .filter(move |change| change.root == root && change.protected)
    }
}

// ============================ Restore ============================

/// One protected change to undo. The worker checks the current state against
/// the state the change produced and skips the file as a conflict otherwise.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandChangeRestore {
    pub path: String,
    pub kind: ExecFileChangeKind,
    /// Content to put back for a modified or deleted file.
    pub before_sha256: Option<String>,
    /// Content the file must still have for a modified or added file.
    pub after_sha256: Option<String>,
}

impl CommandChangeRestore {
    pub fn from_change(change: &ExecFileChange) -> Self {
        Self {
            path: change.path.clone(),
            kind: change.kind,
            before_sha256: change.before_sha256.clone(),
            after_sha256: change.after_sha256.clone(),
        }
    }

    fn validate(&self) -> bool {
        let sha = |value: &Option<String>| value.as_deref().is_some_and(is_sha256);
        is_snapshot_relative_path(&self.path)
            && match self.kind {
                ExecFileChangeKind::Added => {
                    self.before_sha256.is_none() && sha(&self.after_sha256)
                }
                ExecFileChangeKind::Modified => sha(&self.before_sha256) && sha(&self.after_sha256),
                ExecFileChangeKind::Deleted => {
                    sha(&self.before_sha256) && self.after_sha256.is_none()
                }
            }
    }

    /// The change a successful restore of this entry makes.
    pub fn inverse_kind(&self) -> ExecFileChangeKind {
        match self.kind {
            ExecFileChangeKind::Added => ExecFileChangeKind::Deleted,
            ExecFileChangeKind::Modified => ExecFileChangeKind::Modified,
            ExecFileChangeKind::Deleted => ExecFileChangeKind::Added,
        }
    }
}

/// Validate the body of a `RestoreCommandChanges` action.
pub fn validate_command_restore(
    recovery_id: &str,
    root: &ExecSnapshotRoot,
    entries: &[CommandChangeRestore],
) -> Result<(), CommandContractError> {
    root.validate()?;
    if !is_sha256(recovery_id)
        || entries.is_empty()
        || entries.len() > MAX_COMMAND_RESTORE_ENTRIES
        || entries.iter().enumerate().any(|(index, entry)| {
            !entry.validate()
                || entries[..index]
                    .iter()
                    .any(|prior| prior.path == entry.path)
        })
    {
        return Err(CommandContractError::InvalidField("command_restore"));
    }
    Ok(())
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CommandChangeRestoreOutcome {
    Restored,
    /// The file no longer is in the state the command left; nothing was written.
    Conflict,
    /// The restore of this file failed before any change.
    Failed,
    /// The file may or may not have been restored.
    OutcomeUnknown,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandChangeRestoreResult {
    pub path: String,
    pub outcome: CommandChangeRestoreOutcome,
}

/// Device receipt of a `RestoreCommandChanges` action: one result per entry in
/// action order, and the changes the restore made as a report that a later
/// restore can undo again.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandChangesRestoreOutput {
    pub results: Vec<CommandChangeRestoreResult>,
    /// The restore's own report, with the dispatched root as its single root.
    pub file_changes: ExecFileChanges,
}

impl CommandChangesRestoreOutput {
    /// Check the receipt against the dispatched action.
    pub fn validate_for(
        &self,
        root: &ExecSnapshotRoot,
        entries: &[CommandChangeRestore],
    ) -> Result<(), &'static str> {
        let invalid = Err("command restore receipt does not match the approved action");
        if self.results.len() != entries.len()
            || self
                .results
                .iter()
                .zip(entries)
                .any(|(result, entry)| result.path != entry.path)
        {
            return invalid;
        }
        self.file_changes.validate_for(std::slice::from_ref(root))?;
        let changed = self.results.iter().any(|result| {
            matches!(
                result.outcome,
                CommandChangeRestoreOutcome::Restored | CommandChangeRestoreOutcome::OutcomeUnknown
            )
        });
        if changed != (self.file_changes.snapshot == ExecSnapshotOutcome::Taken)
            || self.file_changes.truncated
        {
            return invalid;
        }
        // Every reported change is the inverse of a restored entry.
        for change in &self.file_changes.changes {
            let Some((entry, result)) = entries
                .iter()
                .zip(&self.results)
                .find(|(entry, _)| entry.path == change.path)
            else {
                return invalid;
            };
            if result.outcome != CommandChangeRestoreOutcome::Restored
                || change.kind != entry.inverse_kind()
                || change.before_sha256 != entry.after_sha256
                || change.after_sha256 != entry.before_sha256
            {
                return invalid;
            }
        }
        Ok(())
    }

    /// Whether every entry has a known outcome.
    pub fn verified(&self) -> bool {
        self.results
            .iter()
            .all(|result| result.outcome != CommandChangeRestoreOutcome::OutcomeUnknown)
    }
}

#[cfg(test)]
mod tests;
