//! The recovery-storage side of a command snapshot: opening and checking the
//! roots, `begin_snapshot` before the spawn and `finish_snapshot` (or
//! `abandon_snapshot`) after it. Every path that does not end in a completed
//! record clears the operation's quota reservation under the vault lock.
use std::io;
use std::path::PathBuf;

use desk_agent_protocol::exec::{ExecFileChangeRoot, ExecPlan, ExecSnapshotSkipReason};
use desk_file_recovery::{
    Executor, PendingSnapshot, Record, ScanLimits, Scope, SnapshotBegin, SnapshotFinish,
    SnapshotRequest, SnapshotRoot, Vault,
};
use desk_ipc_protocol::message::ExecFileRecoveryBinding;

use super::{ProtectedRoots, now_ms, skip_reason, skipped, taken};
use crate::worker::agent::file_reference_store::command_changes::{
    CommandDirectory, open_snapshot_root,
};
use crate::worker::agent::file_reference_store::text_mutation::RecoveryContext;
use crate::worker::session::QuotaClient;

pub(super) struct Enabled {
    data_root: PathBuf,
    /// Always present outside unit tests.
    quota: Option<QuotaClient>,
    executor: Executor,
    protected: ProtectedRoots,
    limits: ScanLimits,
}

/// A snapshot taken before the spawn, waiting for the command to end.
pub(crate) struct Pending {
    vault: Vault,
    context: RecoveryContext,
    pending: PendingSnapshot,
    roots: Vec<ExecFileChangeRoot>,
}

fn reserve(quota: &Option<QuotaClient>, record: &Record, bytes: u64) -> io::Result<()> {
    match quota {
        Some(quota) => quota.reserve(record, bytes),
        None if cfg!(test) => Ok(()),
        None => Err(io::Error::other("device backup quota is unavailable")),
    }
}

/// Clear the operation's reservation after a run that left no completed
/// record.
fn clear(context: &RecoveryContext) {
    match Vault::open(&context.data_root).and_then(|vault| vault.lock()) {
        Ok(mut locked) => context.settle_operation(&mut locked),
        Err(cause) => log::warn!(
            "command snapshot reservation not cleared; reconciliation will clear it: kind={:?}",
            cause.kind()
        ),
    }
}

impl Enabled {
    pub(super) fn start(
        data_root: PathBuf,
        quota: Option<QuotaClient>,
        protected: ProtectedRoots,
    ) -> io::Result<Self> {
        let executor = Vault::open(&data_root)?.start_executor()?;
        Ok(Self {
            data_root,
            quota,
            executor,
            protected,
            limits: ScanLimits::default(),
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(
        data_root: PathBuf,
        protected: ProtectedRoots,
        limits: ScanLimits,
    ) -> io::Result<Self> {
        let mut enabled = Self::start(data_root, None, protected)?;
        enabled.limits = limits;
        Ok(enabled)
    }

    /// Open the plan's roots and scan them. Blocking.
    pub(super) fn prepare(
        &self,
        plan: &ExecPlan,
        binding: Option<ExecFileRecoveryBinding>,
    ) -> Result<Pending, ExecSnapshotSkipReason> {
        let binding = binding.ok_or(ExecSnapshotSkipReason::StorageUnavailable)?;
        let os_user = crate::file_recovery_service::platform_user::current()
            .map_err(|_| ExecSnapshotSkipReason::StorageUnavailable)?;
        if binding.recovery.os_user != os_user {
            return Err(ExecSnapshotSkipReason::StorageUnavailable);
        }
        let mut directories = Vec::with_capacity(plan.snapshot_roots.len());
        for root in &plan.snapshot_roots {
            let directory =
                open_snapshot_root(root).map_err(|_| ExecSnapshotSkipReason::RootUnavailable)?;
            if self.protected.refuses(&directory.path) {
                return Err(ExecSnapshotSkipReason::ProtectedRoot);
            }
            if directories
                .iter()
                .any(|other: &CommandDirectory| other.identity == directory.identity)
            {
                return Err(ExecSnapshotSkipReason::RootUnavailable);
            }
            directories.push(directory);
        }
        let context = RecoveryContext {
            #[cfg(test)]
            _test_data: None,
            execution_epoch: binding.recovery.execution_epoch,
            data_root: self.data_root.clone(),
            quota: self.quota.clone(),
            scope: Scope {
                authority: binding.recovery.authority,
                device: binding.device,
                owner: binding.owner,
                os_user,
            },
            conversation_id: binding.recovery.conversation_id,
            operation_id: plan.execution_generation.clone(),
            generation: plan.execution_generation.clone(),
        };
        // Clock, policy and resumable epoch maintenance, under its own guard.
        drop(
            context
                .open_vault()
                .map_err(|_| ExecSnapshotSkipReason::StorageUnavailable)?,
        );
        let vault =
            Vault::open(&self.data_root).map_err(|_| ExecSnapshotSkipReason::StorageUnavailable)?;
        let request = SnapshotRequest {
            scope: context.scope.clone(),
            conversation: &context.conversation_id,
            operation: &context.operation_id,
            generation: &context.generation,
            executor: &self.executor,
            roots: directories
                .iter()
                .map(|directory| SnapshotRoot {
                    directory: &directory.handle,
                    directory_identity: &directory.identity,
                })
                .collect(),
            expected_epoch: context.execution_epoch,
            now_ms: now_ms(),
            limits: self.limits,
        };
        let begun =
            vault.begin_snapshot(request, |record, bytes| reserve(&self.quota, record, bytes));
        match begun {
            Ok(SnapshotBegin::Taken(pending)) => Ok(Pending {
                roots: plan
                    .snapshot_roots
                    .iter()
                    .cloned()
                    .zip(directories)
                    .map(|(root, directory)| ExecFileChangeRoot {
                        root,
                        directory: directory.reference,
                    })
                    .collect(),
                vault,
                context,
                pending,
            }),
            Ok(SnapshotBegin::Skipped(reason)) => {
                clear(&context);
                Err(skip_reason(reason))
            }
            Err(cause) => {
                log::warn!(
                    "command snapshot not taken: recovery storage error kind={:?}",
                    cause.kind()
                );
                clear(&context);
                Err(ExecSnapshotSkipReason::StorageUnavailable)
            }
        }
    }
}

impl Pending {
    #[cfg(test)]
    pub(super) fn recovery_id(&self) -> &str {
        self.pending.record_id()
    }

    /// Rescan after the command ended (`ran`) and build its report, or
    /// abandon the snapshot of a command that never started. Blocking.
    pub(super) fn finish(self, ran: bool) -> Option<desk_agent_protocol::exec::ExecFileChanges> {
        let Self {
            vault,
            context,
            pending,
            roots,
        } = self;
        let started_at = pending.started_at_ms();
        if !ran {
            match vault.abandon_snapshot(pending, false, now_ms()) {
                Ok(mut locked) => context.settle_operation(&mut locked),
                Err(_) => clear(&context),
            }
            return None;
        }
        let quota = context.quota.clone();
        match vault.finish_snapshot(pending, now_ms(), |record, bytes| {
            reserve(&quota, record, bytes)
        }) {
            Ok((mut locked, finish)) => {
                context.settle_operation(&mut locked);
                Some(match finish {
                    SnapshotFinish::Taken { record, changes } => {
                        let (started, finished) =
                            record
                                .snapshot
                                .as_ref()
                                .map_or((started_at, now_ms()), |state| {
                                    (
                                        state.started_at_ms,
                                        state.finished_at_ms.unwrap_or_else(now_ms),
                                    )
                                });
                        taken(record.id, started, finished, roots, changes)
                    }
                    SnapshotFinish::Skipped(reason) => skipped(skip_reason(reason), started_at),
                    SnapshotFinish::Discarded => {
                        skipped(ExecSnapshotSkipReason::StorageUnavailable, started_at)
                    }
                })
            }
            Err(cause) => {
                // The record is already an unknown outcome: exportable, not
                // restorable.
                log::warn!(
                    "command snapshot not finished: recovery storage error kind={:?}",
                    cause.kind()
                );
                clear(&context);
                Some(skipped(
                    ExecSnapshotSkipReason::StorageUnavailable,
                    started_at,
                ))
            }
        }
    }
}
