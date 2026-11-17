//! Best-effort directory snapshots around a mutating AI command.
//!
//! The trusted central seals snapshot roots into the plan of a mutating AI
//! command. Around the spawn the worker:
//! 1. opens every root without following its final component (a granted
//!    directory through its device-issued reference, the working directory by
//!    its canonical path) and refuses protected roots;
//! 2. scans the roots and stores their text files before the command is
//!    spawned (`begin_snapshot`); the scan is not part of `timeout_ms`;
//! 3. after the command exited, timed out or was cancelled, rescans the roots
//!    against the pre-run scan (`finish_snapshot`) and reports the differences
//!    as [`ExecFileChanges`], bounded to [`MAX_EXEC_FILE_CHANGES_BYTES`].
//!
//! A command that never started abandons its snapshot and reports nothing. A
//! worker that dies mid-run leaves a pending record whose executor lock is
//! released; maintenance marks it as an unknown outcome, which can be
//! exported but not restored. No snapshot is taken when the worker runs as the
//! system identity, and none for a daemon-owned root PTY.
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use desk_agent_protocol::exec::{
    ExecFileChange, ExecFileChangeKind, ExecFileChangeRoot, ExecFileChanges, ExecPlan,
    ExecSnapshotOutcome, ExecSnapshotSkipReason, MAX_EXEC_FILE_CHANGES_BYTES,
    is_snapshot_relative_path,
};
use desk_agent_protocol::{AgentOutcome, OperationOutput};
use desk_ipc_protocol::message::ExecFileRecoveryBinding;

mod protected;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod vault;

pub(crate) use protected::ProtectedRoots;

/// Snapshot facilities of one worker instance.
#[derive(Clone)]
pub(crate) struct ExecSnapshots(Availability);

#[derive(Clone)]
enum Availability {
    /// The worker has no private data directory or recovery storage.
    Unavailable,
    /// The worker runs as the system identity.
    SystemIdentity,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    Enabled(Arc<vault::Enabled>),
}

impl ExecSnapshots {
    pub(crate) fn unavailable() -> Self {
        Self(Availability::Unavailable)
    }

    /// Register this worker as a snapshot executor for its lifetime. Blocks on
    /// the recovery storage lock.
    pub(crate) fn start(
        user_session: bool,
        data_root: Option<PathBuf>,
        quota: crate::worker::session::QuotaClient,
        protected: ProtectedRoots,
    ) -> Self {
        if !user_session || runs_as_system_identity() {
            return Self(Availability::SystemIdentity);
        }
        let Some(data_root) = data_root else {
            return Self::unavailable();
        };
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        match vault::Enabled::start(data_root, Some(quota), protected) {
            Ok(enabled) => Self(Availability::Enabled(Arc::new(enabled))),
            Err(cause) => {
                log::warn!(
                    "command snapshots unavailable: recovery storage error kind={:?}",
                    cause.kind()
                );
                Self::unavailable()
            }
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = (data_root, quota, protected);
            Self::unavailable()
        }
    }

    /// Run `execute` around the plan's snapshot and attach the change report
    /// to its output. `execute` sets the flag it receives once the command
    /// was spawned.
    pub(crate) async fn run<F, Fut>(
        &self,
        plan: &ExecPlan,
        binding: Option<ExecFileRecoveryBinding>,
        execute: F,
    ) -> AgentOutcome
    where
        F: FnOnce(Arc<AtomicBool>) -> Fut,
        Fut: Future<Output = AgentOutcome>,
    {
        let started = Arc::new(AtomicBool::new(false));
        if plan.snapshot_roots.is_empty() {
            return execute(started).await;
        }
        let begun_at = now_ms();
        match self.prepare(plan, binding).await {
            Err(reason) => {
                let outcome = execute(started).await;
                attach(outcome, Some(skipped(reason, begun_at)))
            }
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Ok(pending) => {
                let outcome = execute(Arc::clone(&started)).await;
                let ran = started.load(Ordering::Acquire);
                let report = tokio::task::spawn_blocking(move || pending.finish(ran))
                    .await
                    .unwrap_or_else(|_| {
                        Some(skipped(
                            ExecSnapshotSkipReason::StorageUnavailable,
                            begun_at,
                        ))
                    });
                attach(outcome, report)
            }
        }
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    async fn prepare(
        &self,
        plan: &ExecPlan,
        binding: Option<ExecFileRecoveryBinding>,
    ) -> Result<vault::Pending, ExecSnapshotSkipReason> {
        let enabled = match &self.0 {
            Availability::Unavailable => return Err(ExecSnapshotSkipReason::StorageUnavailable),
            Availability::SystemIdentity => return Err(ExecSnapshotSkipReason::SystemIdentity),
            Availability::Enabled(enabled) => Arc::clone(enabled),
        };
        let plan = plan.clone();
        tokio::task::spawn_blocking(move || enabled.prepare(&plan, binding))
            .await
            .unwrap_or(Err(ExecSnapshotSkipReason::StorageUnavailable))
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    async fn prepare(
        &self,
        _plan: &ExecPlan,
        _binding: Option<ExecFileRecoveryBinding>,
    ) -> Result<std::convert::Infallible, ExecSnapshotSkipReason> {
        Err(match self.0 {
            Availability::SystemIdentity => ExecSnapshotSkipReason::SystemIdentity,
            Availability::Unavailable => ExecSnapshotSkipReason::StorageUnavailable,
        })
    }
}

#[cfg(unix)]
fn runs_as_system_identity() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn runs_as_system_identity() -> bool {
    desk_file_recovery::windows::current_user_sid().is_ok_and(|sid| sid == "S-1-5-18")
}

#[cfg(not(any(unix, windows)))]
fn runs_as_system_identity() -> bool {
    false
}

pub(crate) fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(1) as u64
}

/// The report of a plan with roots whose snapshot was not taken.
pub(crate) fn skipped(reason: ExecSnapshotSkipReason, started_at_ms: u64) -> ExecFileChanges {
    ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Skipped(reason),
        recovery_id: None,
        started_at_unix_ms: started_at_ms,
        finished_at_unix_ms: now_ms().max(started_at_ms),
        roots: Vec::new(),
        changes: Vec::new(),
        truncated: false,
    }
}

#[cfg(any(target_os = "linux", test))]
/// The report of a daemon-owned root PTY, which never snapshots: the
/// worker could neither read nor restore the files of the root identity.
pub(crate) fn root_pty_report(plan: &ExecPlan, started_at_ms: u64) -> Option<ExecFileChanges> {
    (!plan.snapshot_roots.is_empty())
        .then(|| skipped(ExecSnapshotSkipReason::SystemIdentity, started_at_ms))
}

/// Attach a report to the output of a command that started.
fn attach(mut outcome: AgentOutcome, report: Option<ExecFileChanges>) -> AgentOutcome {
    if let AgentOutcome::Ok(OperationOutput::Exec(output)) = &mut outcome
        && output.started
    {
        output.file_changes = report;
    }
    outcome
}

fn skip_reason(reason: desk_file_recovery::SkipReason) -> ExecSnapshotSkipReason {
    match reason {
        desk_file_recovery::SkipReason::TooManyFiles
        | desk_file_recovery::SkipReason::ManifestTooLarge => ExecSnapshotSkipReason::FileLimit,
        desk_file_recovery::SkipReason::TimeLimit => ExecSnapshotSkipReason::TimeLimit,
        desk_file_recovery::SkipReason::StorageFull => ExecSnapshotSkipReason::StorageUnavailable,
    }
}

fn change_kind(kind: desk_file_recovery::ChangeKind) -> ExecFileChangeKind {
    match kind {
        desk_file_recovery::ChangeKind::Added => ExecFileChangeKind::Added,
        desk_file_recovery::ChangeKind::Modified => ExecFileChangeKind::Modified,
        desk_file_recovery::ChangeKind::Deleted => ExecFileChangeKind::Deleted,
    }
}

/// The report of a taken snapshot. Changes are kept in order until the
/// serialized report would exceed its size bound; the rest are dropped and
/// `truncated` is set. Paths the protocol cannot carry are dropped the same
/// way.
pub(crate) fn taken(
    recovery_id: String,
    started_at_ms: u64,
    finished_at_ms: u64,
    roots: Vec<ExecFileChangeRoot>,
    changes: Vec<desk_file_recovery::FileChange>,
) -> ExecFileChanges {
    let mut report = ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Taken,
        recovery_id: Some(recovery_id),
        started_at_unix_ms: started_at_ms,
        finished_at_unix_ms: finished_at_ms.max(started_at_ms),
        roots,
        changes: Vec::new(),
        truncated: true,
    };
    // `false` is one byte longer than `true`; every kept change but the
    // first also takes a separator.
    let budget = MAX_EXEC_FILE_CHANGES_BYTES
        .saturating_sub(serde_json::to_vec(&report).map_or(usize::MAX, |bytes| bytes.len()) + 1);
    let mut used = 0;
    let mut truncated = false;
    for change in changes {
        let Ok(root) = u16::try_from(change.root) else {
            truncated = true;
            continue;
        };
        if usize::from(root) >= report.roots.len() || !is_snapshot_relative_path(&change.path) {
            truncated = true;
            continue;
        }
        let change = ExecFileChange {
            path: change.path,
            root,
            kind: change_kind(change.kind),
            before_sha256: change.before_sha256,
            after_sha256: change.after_sha256,
            size_bytes: change.size,
            protected: change.protected,
        };
        let size = serde_json::to_vec(&change).map_or(usize::MAX, |bytes| bytes.len()) + 1;
        if used + size > budget {
            truncated = true;
            break;
        }
        used += size;
        report.changes.push(change);
    }
    report.truncated = truncated;
    report
}

/// The trusted backup binding of one dispatched Computer Action.
pub(crate) struct ActionRecovery {
    pub binding: Option<desk_ipc_protocol::message::FileRecoveryExecutionContext>,
    pub data_root: Option<PathBuf>,
    pub quota: crate::worker::session::QuotaClient,
    pub device: String,
    pub owner: String,
    pub operation_id: String,
    pub generation: String,
}

/// Run a dispatched `RestoreCommandChanges` action in the conversation's
/// backup scope. Blocking; an error means nothing was changed.
pub(crate) fn restore_command_changes(
    recovery: ActionRecovery,
    target: &desk_agent_protocol::computer_use::ObjectRef,
    action: &desk_agent_protocol::computer_use::FilePatchAction,
    commit_guard: impl Fn() -> Result<(), desk_agent_protocol::AgentError>,
) -> Result<desk_agent_protocol::exec::CommandChangesRestoreOutput, desk_agent_protocol::AgentError>
{
    let denied = |message: &str| desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    let (Some(binding), Some(data_root)) = (recovery.binding, recovery.data_root) else {
        return Err(denied(
            "Trusted conversation backup context is unavailable; nothing was restored",
        ));
    };
    let os_user = crate::file_recovery_service::platform_user::current()
        .map_err(|_| denied("Backup execution user is unavailable; nothing was restored"))?;
    if binding.os_user != os_user {
        return Err(denied(
            "Backup execution user changed after registration; nothing was restored",
        ));
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        use crate::worker::agent::file_reference_store::{command_changes, text_mutation};
        let context = text_mutation::RecoveryContext {
            #[cfg(test)]
            _test_data: None,
            execution_epoch: binding.execution_epoch,
            data_root,
            quota: Some(recovery.quota),
            scope: desk_file_recovery::Scope {
                authority: binding.authority,
                device: recovery.device,
                owner: recovery.owner,
                os_user,
            },
            conversation_id: binding.conversation_id,
            operation_id: recovery.operation_id,
            generation: recovery.generation,
        };
        command_changes::restore(target, action, &context, commit_guard)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (data_root, os_user, target, action, commit_guard);
        Err(desk_agent_protocol::AgentError {
            kind: desk_agent_protocol::AgentErrorKind::UnsupportedCapability,
            message: "Restoring command changes is not implemented on this platform".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }
}

/// One-line summary of a command restore receipt.
pub(crate) fn restore_summary(
    output: &desk_agent_protocol::exec::CommandChangesRestoreOutput,
) -> String {
    use desk_agent_protocol::exec::CommandChangeRestoreOutcome as Outcome;
    let count = |outcome: Outcome| {
        output
            .results
            .iter()
            .filter(|result| result.outcome == outcome)
            .count()
    };
    let mut summary = format!(
        "{} of {} files restored; the replaced state is saved in device-private backup storage",
        count(Outcome::Restored),
        output.results.len()
    );
    for (outcome, label) in [
        (
            Outcome::Conflict,
            "changed since the command and left as they are",
        ),
        (Outcome::Failed, "not restored"),
        (
            Outcome::OutcomeUnknown,
            "with an unknown outcome; do not retry automatically",
        ),
    ] {
        let files = count(outcome);
        if files > 0 {
            summary.push_str(&format!("; {files} {label}"));
        }
    }
    summary
}

#[cfg(all(test, unix))]
mod tests;
