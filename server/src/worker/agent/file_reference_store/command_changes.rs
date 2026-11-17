//! Directories of command snapshots and the restore of command changes.
//!
//! A snapshot root is opened without following its final component: a granted
//! directory through its device-issued reference (whose identity must still
//! match), a working directory by its canonical path, which is then issued as
//! a durable directory reference so that a later restore can target it.
//!
//! A `RestoreCommandChanges` action runs under one held vault lock:
//! 1. the record's protected targets under the dispatched root are matched
//!    with the action's entries and the current content of each file is
//!    compared with the state the change produced (a mismatch is a conflict);
//! 2. the matching files are backed up into a new `CommandRestore` record,
//!    which moves to `CommitIntent` before the first write;
//! 3. each file is written through the platform module, which checks the
//!    expected state again right before its change;
//! 4. the per-file results finish the record, and quota is settled.
use super::text_recovery_context::{RecoveryContext, digest, directory_identity, now_ms};
use super::*;
use desk_agent_protocol::computer_use::FilePatchAction;
use desk_agent_protocol::exec::{
    CommandChangeRestore, CommandChangeRestoreOutcome, CommandChangeRestoreResult,
    CommandChangesRestoreOutput, ExecFileChange, ExecFileChangeKind, ExecFileChangeRoot,
    ExecFileChanges, ExecSnapshotOutcome, ExecSnapshotRoot, ExecSnapshotSkipReason,
};
use desk_file_recovery::{ChangeKind, ChangeState, RestoreFile, RestoreRequest, RestoreTarget};

#[cfg(unix)]
#[path = "command_changes_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "command_changes_windows.rs"]
mod platform;

/// A snapshot root opened without following its final component.
pub(crate) struct CommandDirectory {
    pub handle: File,
    /// Canonical path, for the protected-root check.
    pub path: PathBuf,
    /// The identity a snapshot records and a restore compares.
    pub identity: String,
    /// The device-issued reference a restore of this root targets.
    pub reference: ObjectRef,
}

fn unavailable(message: &str) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

/// Open the directory a sealed snapshot root names.
pub(crate) fn open_snapshot_root(root: &ExecSnapshotRoot) -> Result<CommandDirectory, AgentError> {
    match root {
        ExecSnapshotRoot::Grant { directory, .. } => open_reference(directory),
        ExecSnapshotRoot::Cwd { path } => open_working_directory(path),
    }
}

/// Reopen a device-issued directory reference and check that the directory
/// still has the identity it was issued for.
pub(crate) fn open_reference(reference: &ObjectRef) -> Result<CommandDirectory, AgentError> {
    if reference.object_kind != ObjectKind::Directory {
        return Err(unavailable("command changes require a directory reference"));
    }
    let stored = resolve(reference)?;
    let opened = platform::open_directory(&stored.path)?;
    if !opened.metadata.is_dir() || opened.identity != stored.identity {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "the directory was replaced after it was selected",
            false,
        ));
    }
    Ok(CommandDirectory {
        handle: opened.handle,
        path: stored.path,
        identity: directory_identity(&opened.identity),
        reference: reference.clone(),
    })
}

/// Open a command's working directory by its canonical path and issue (or
/// reuse) a durable reference for it.
fn open_working_directory(candidate: &str) -> Result<CommandDirectory, AgentError> {
    let path = Path::new(candidate);
    if candidate.is_empty()
        || candidate.len() > 4096
        || candidate.chars().any(char::is_control)
        || !path.is_absolute()
    {
        return Err(unavailable(
            "the working directory must be a bounded absolute path",
        ));
    }
    // The command itself resolves links in its working directory; the
    // snapshot covers the directory it actually runs in.
    let canonical = std::fs::canonicalize(path)
        .map_err(|cause| io_error("resolve the working directory", cause))?;
    let opened = platform::open_directory(&canonical)?;
    if !opened.metadata.is_dir() {
        return Err(unavailable("the working directory is not a directory"));
    }
    let reference = issue_command_directory(&canonical, &opened.identity)?;
    Ok(CommandDirectory {
        handle: opened.handle,
        path: canonical,
        identity: directory_identity(&opened.identity),
        reference,
    })
}

/// A durable reference to `path`, reusing one already issued for the same
/// directory so that repeated commands in one directory do not grow the store.
fn issue_command_directory(path: &Path, identity: &FileIdentity) -> Result<ObjectRef, AgentError> {
    {
        let state = store().lock().map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "file reference store is unavailable",
                true,
            )
        })?;
        if let Some((token, stored)) = state.objects.iter().find(|(_, stored)| {
            stored.durable_artifact
                && stored.object_kind == ObjectKind::Directory
                && stored.path == path
                && stored.identity == *identity
        }) {
            return Ok(ObjectRef {
                token: token.clone(),
                snapshot_id: stored.snapshot_id.clone(),
                object_kind: ObjectKind::Directory,
                expires_at: String::new(),
            });
        }
    }
    let reference = issue_with_lifetime(path, DURABLE_ARTIFACT_REF_TTL_SECS, true)?;
    if resolve(&reference)?.identity != *identity {
        return Err(unavailable(
            "the working directory changed while it was opened",
        ));
    }
    Ok(reference)
}

/// Outcome of one file write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    Done,
    /// The file is no longer in the state the change produced.
    Conflict,
    /// Nothing was changed.
    NotStarted,
    /// The change may or may not have happened.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the two-step Windows replacement can stop in between"
        )
    )]
    Unknown,
}

/// One entry that matched its recorded target.
struct Matched<'a> {
    position: usize,
    entry: &'a CommandChangeRestore,
    target: RestoreTarget,
    current: Option<Vec<u8>>,
    restored: Option<Vec<u8>>,
}

fn change_kind(kind: ExecFileChangeKind) -> ChangeKind {
    match kind {
        ExecFileChangeKind::Added => ChangeKind::Added,
        ExecFileChangeKind::Modified => ChangeKind::Modified,
        ExecFileChangeKind::Deleted => ChangeKind::Deleted,
    }
}

/// Run one dispatched `RestoreCommandChanges` action. An error means nothing
/// was changed.
pub(crate) fn restore(
    target: &ObjectRef,
    action: &FilePatchAction,
    context: &RecoveryContext,
    commit_guard: impl Fn() -> Result<(), AgentError>,
) -> Result<CommandChangesRestoreOutput, AgentError> {
    action
        .validate_command_restore()
        .map_err(|_| unavailable("invalid command change restore"))?;
    let FilePatchAction::RestoreCommandChanges {
        recovery_id,
        root,
        entries,
    } = action
    else {
        return Err(unavailable("not a command change restore"));
    };
    if matches!(root, ExecSnapshotRoot::Grant { directory, .. } if directory != target) {
        return Err(unavailable(
            "the restore target is not the snapshot root's directory",
        ));
    }
    let directory = open_reference(target)?;
    let root_handle = platform::open_root(&directory)?;
    let started_at = now_ms();
    let mut vault = context.open_vault()?;
    let targets = vault
        .restore_targets(
            &context.scope,
            &context.conversation_id,
            recovery_id,
            None,
            started_at,
        )
        .map_err(|e| io_error("read the command's recorded changes", e))?;
    let mut results: Vec<CommandChangeRestoreOutcome> =
        vec![CommandChangeRestoreOutcome::Failed; entries.len()];
    let mut matched = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        // A target must be a protected change under this root with exactly
        // the approved shape and hashes.
        let Some(target) = targets
            .iter()
            .find(|target| {
                target.directory_identity == directory.identity
                    && target.path == entry.path
                    && target.kind == change_kind(entry.kind)
                    && target.expected_sha256 == entry.after_sha256
                    && target.restored_sha256 == entry.before_sha256
            })
            .cloned()
        else {
            continue;
        };
        let current = match platform::read(&root_handle, &entry.path) {
            Ok(current) => current,
            Err(_) => {
                results[position] = CommandChangeRestoreOutcome::Conflict;
                continue;
            }
        };
        if current.as_deref().map(digest) != target.expected_sha256 {
            results[position] = CommandChangeRestoreOutcome::Conflict;
            continue;
        }
        let restored = match vault.entry_material(
            &context.scope,
            &context.conversation_id,
            recovery_id,
            target.index,
            started_at,
        ) {
            Ok(material) if material.before.as_deref().map(digest) == target.restored_sha256 => {
                material.before
            }
            _ => continue,
        };
        matched.push(Matched {
            position,
            entry,
            target,
            current,
            restored,
        });
    }
    let unchanged = |results: Vec<CommandChangeRestoreOutcome>| CommandChangesRestoreOutput {
        results: entries
            .iter()
            .zip(results)
            .map(|(entry, outcome)| CommandChangeRestoreResult {
                path: entry.path.clone(),
                outcome,
            })
            .collect(),
        file_changes: ExecFileChanges {
            snapshot: ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::StorageUnavailable),
            recovery_id: None,
            started_at_unix_ms: started_at,
            finished_at_unix_ms: now_ms().max(started_at),
            roots: Vec::new(),
            changes: Vec::new(),
            truncated: false,
        },
    };
    if matched.is_empty() {
        return Ok(unchanged(results));
    }
    let request = RestoreRequest {
        scope: context.scope.clone(),
        conversation: &context.conversation_id,
        operation: &context.operation_id,
        generation: &context.generation,
        source: recovery_id,
        files: matched
            .iter()
            .map(|file| RestoreFile {
                index: file.target.index,
                current: file.current.as_deref(),
                restored: file.restored.as_deref(),
            })
            .collect(),
        now_ms: now_ms(),
    };
    let begun =
        vault.begin_command_restore(
            request,
            context.execution_epoch,
            |record, bytes| match &context.quota {
                Some(quota) => quota.reserve(record, bytes),
                None if cfg!(test) => Ok(()),
                None => Err(std::io::Error::other("device backup quota is unavailable")),
            },
        );
    let record = match begun {
        Ok(record) => record,
        Err(cause) => {
            context.settle_operation(&mut vault);
            return Err(io_error(
                "save private file backup; nothing was restored",
                cause,
            ));
        }
    };
    let committed = commit_guard().and_then(|()| {
        vault
            .transition(&context.scope, &record.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))
    });
    if let Err(cause) = committed {
        if let Err(e) = vault.finish(&context.scope, &record.id, ChangeState::Aborted) {
            tracing::warn!(error_kind = ?e.kind(), "aborted command restore needs journal recovery");
        }
        context.settle(&mut vault, &record);
        return Err(cause);
    }
    let mut applied = Vec::with_capacity(matched.len());
    for (index, file) in matched.iter().enumerate() {
        let stage = format!(".assistant-restore-{}-{index}", &record.id[..16]);
        let outcome = platform::apply(
            &root_handle,
            &file.entry.path,
            file.target.expected_sha256.as_deref(),
            file.restored.as_deref(),
            &stage,
        );
        results[file.position] = match outcome {
            Applied::Done => CommandChangeRestoreOutcome::Restored,
            Applied::Conflict => CommandChangeRestoreOutcome::Conflict,
            Applied::NotStarted => CommandChangeRestoreOutcome::Failed,
            Applied::Unknown => CommandChangeRestoreOutcome::OutcomeUnknown,
        };
        applied.push(matches!(outcome, Applied::Done | Applied::Unknown));
    }
    let finished = vault.finish_command_restore(&context.scope, &record.id, &applied, now_ms());
    context.settle(&mut vault, &record);
    let finished = match finished {
        Ok(record) => record,
        Err(cause) => {
            // The record stays in CommitIntent and recovers as an unknown
            // outcome; no write is reported as verified.
            tracing::warn!(error_kind = ?cause.kind(), "command restore outcome needs journal recovery");
            for outcome in &mut results {
                if *outcome == CommandChangeRestoreOutcome::Restored {
                    *outcome = CommandChangeRestoreOutcome::OutcomeUnknown;
                }
            }
            record
        }
    };
    if !applied.iter().any(|applied| *applied) {
        return Ok(unchanged(results));
    }
    let changes = matched
        .iter()
        .filter(|file| results[file.position] == CommandChangeRestoreOutcome::Restored)
        .map(|file| ExecFileChange {
            path: file.entry.path.clone(),
            root: 0,
            kind: file.entry.inverse_kind(),
            before_sha256: file.entry.after_sha256.clone(),
            after_sha256: file.entry.before_sha256.clone(),
            size_bytes: file
                .restored
                .as_ref()
                .or(file.current.as_ref())
                .map_or(0, |bytes| bytes.len() as u64),
            protected: true,
        })
        .collect();
    let snapshot = finished.snapshot.as_ref();
    let started = snapshot.map_or(started_at, |state| state.started_at_ms);
    Ok(CommandChangesRestoreOutput {
        results: entries
            .iter()
            .zip(results)
            .map(|(entry, outcome)| CommandChangeRestoreResult {
                path: entry.path.clone(),
                outcome,
            })
            .collect(),
        file_changes: ExecFileChanges {
            snapshot: ExecSnapshotOutcome::Taken,
            recovery_id: Some(finished.id.clone()),
            started_at_unix_ms: started,
            finished_at_unix_ms: snapshot
                .and_then(|state| state.finished_at_ms)
                .unwrap_or_else(now_ms)
                .max(started),
            roots: vec![ExecFileChangeRoot {
                root: root.clone(),
                directory: target.clone(),
            }],
            changes,
            truncated: false,
        },
    })
}
