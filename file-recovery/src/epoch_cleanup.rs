//! Execution epochs fence replays of old dispatches. Advancing the epoch lets
//! the vault prune purged replay records:
//! 0. no record may be Preparing or BackupReady (otherwise `WouldBlock`);
//! 1. the device quota stops accepting reservations of the old epoch;
//! 2. the vault prunes old purged records and tombstones and sweeps;
//! 3. a reconciliation reports the pruned usage and live operations;
//! 4. the quota finishes once a reconciliation at or after that revision
//!    was applied.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpochCleanupState {
    pub previous: u64,
    pub next: u64,
    pub pruned: bool,
}

/// The coordinator must authenticate the real OS user independently of this
/// vault. Every call is made while the vault lock is held.
pub trait EpochCoordinator {
    fn begin(&mut self, os_user: &str, expected: u64) -> io::Result<quota::EpochStatus>;
    fn reconcile(
        &mut self,
        os_user: &str,
        request: &quota::ReconcileRequest,
    ) -> io::Result<quota::EpochStatus>;
    fn finish(
        &mut self,
        os_user: &str,
        request: &quota::FinishEpochRequest,
    ) -> io::Result<quota::EpochStatus>;
}
impl EpochCoordinator for quota::LockedDeviceQuota {
    fn begin(&mut self, os_user: &str, expected: u64) -> io::Result<quota::EpochStatus> {
        self.begin_epoch_cleanup(os_user, expected)
    }
    fn reconcile(
        &mut self,
        os_user: &str,
        request: &quota::ReconcileRequest,
    ) -> io::Result<quota::EpochStatus> {
        quota::LockedDeviceQuota::reconcile(self, os_user, request)
    }
    fn finish(
        &mut self,
        os_user: &str,
        request: &quota::FinishEpochRequest,
    ) -> io::Result<quota::EpochStatus> {
        self.finish_epoch_cleanup(os_user, request)
    }
}

impl LockedVault {
    pub fn execution_epoch(&self) -> u64 {
        self.ledger.execution_epoch
    }
    pub fn epoch_cleanup_state(&self) -> Option<EpochCleanupState> {
        self.ledger.epoch_cleanup
    }
    pub fn require_execution_epoch(&self, expected: u64) -> io::Result<()> {
        if self.ledger.epoch_cleanup.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "backup replay index cleanup is pending",
            ));
        }
        if expected != self.ledger.execution_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution epoch changed",
            ));
        }
        Ok(())
    }
    fn validate_cleanup_user(&self, os_user: &str) -> io::Result<()> {
        identity(os_user)?;
        let user = hash(os_user.as_bytes());
        if self
            .ledger
            .records
            .values()
            .any(|record| record.scope.os_user != os_user)
            || self
                .ledger
                .deleted_conversations
                .keys()
                .any(|namespace| namespace_user(namespace) != Some(user.as_str()))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovery cleanup user changed",
            ));
        }
        Ok(())
    }
    pub fn prunable_index_count(&self) -> usize {
        self.ledger
            .records
            .values()
            .filter(|record| {
                record.material == MaterialState::Purged && record.transaction.is_none()
            })
            .count()
            + self
                .ledger
                .deleted_conversations
                .iter()
                .filter(|(namespace, deleted)| {
                    !deleted.quota_pending
                        && !self
                            .ledger
                            .records
                            .values()
                            .any(|record| record.scope.key(&record.conversation) == **namespace)
                })
                .count()
    }

    /// Live operations whose reservations a reconciliation must keep.
    fn live_operations(&self) -> Vec<String> {
        self.ledger
            .records
            .values()
            .filter(|record| record.in_flight())
            .map(|record| record.id.clone())
            .collect()
    }

    /// Absolute usage and live operations of this vault at the persisted revision.
    pub fn reconcile_request(&self) -> quota::ReconcileRequest {
        quota::ReconcileRequest {
            execution_epoch: self.ledger.execution_epoch,
            usage: self.usage_report(),
            live_operations: self.live_operations(),
        }
    }

    /// Send a reconciliation and adopt the epoch the quota returns.
    pub fn reconcile(
        &mut self,
        os_user: &str,
        coordinator: &mut impl EpochCoordinator,
    ) -> io::Result<quota::EpochStatus> {
        self.flush()?;
        let status = coordinator.reconcile(os_user, &self.reconcile_request())?;
        self.adopt_quota_epoch(status)?;
        Ok(status)
    }

    /// Realign a rebuilt vault with the device quota. Epochs only move
    /// forward; a cleanup the quota still has pending resumes as pruned,
    /// because a rebuilt vault has no replay indexes left to prune.
    pub fn adopt_quota_epoch(&mut self, status: quota::EpochStatus) -> io::Result<()> {
        if self.ledger.epoch_cleanup.is_some() || status.epoch < self.ledger.execution_epoch {
            return Ok(());
        }
        let resume = status.cleanup_pending && status.epoch > 0;
        if status.epoch == self.ledger.execution_epoch && !resume {
            return Ok(());
        }
        self.ledger.execution_epoch = status.epoch;
        if resume {
            self.ledger.epoch_cleanup = Some(EpochCleanupState {
                previous: status.epoch - 1,
                next: status.epoch,
                pruned: true,
            });
        }
        self.persist()
    }

    /// Resume durable work regardless of threshold. usize::MAX means resume only.
    /// The vault lock is held through every coordinator call.
    pub fn maintain_epoch_indexes(
        &mut self,
        os_user: &str,
        coordinator: &mut impl EpochCoordinator,
        minimum_indexes: usize,
    ) -> io::Result<()> {
        let state = match self.ledger.epoch_cleanup {
            Some(state) => state,
            None => {
                if minimum_indexes == usize::MAX || self.prunable_index_count() < minimum_indexes {
                    return Ok(());
                }
                self.validate_cleanup_user(os_user)?;
                if self.ledger.records.values().any(|record| {
                    matches!(
                        record.change,
                        ChangeState::Preparing | ChangeState::BackupReady
                    )
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "backup operations are in progress",
                    ));
                }
                let state = EpochCleanupState {
                    previous: self.ledger.execution_epoch,
                    next: self
                        .ledger
                        .execution_epoch
                        .checked_add(1)
                        .ok_or_else(|| invalid("backup epoch exhausted"))?,
                    pruned: false,
                };
                self.ledger.epoch_cleanup = Some(state);
                self.persist()?;
                state
            }
        };
        self.validate_cleanup_user(os_user)?;
        let advanced = coordinator.begin(os_user, state.previous)?;
        if advanced.epoch != state.next || (!state.pruned && !advanced.cleanup_pending) {
            return Err(invalid(
                "backup coordinator epoch does not match persisted cleanup",
            ));
        }
        if !state.pruned {
            self.prune_epoch_indexes(state)?;
        }
        let request = self.reconcile_request();
        coordinator.reconcile(os_user, &request)?;
        let finished = coordinator.finish(
            os_user,
            &quota::FinishEpochRequest {
                epoch: state.next,
                vault: request.usage.vault(),
            },
        )?;
        if finished.epoch != state.next || finished.cleanup_pending {
            return Err(invalid("backup coordinator cleanup is not acknowledged"));
        }
        self.ledger.epoch_cleanup = None;
        self.persist()
    }

    /// The quota has fenced old-epoch reservations: drop purged replay records
    /// and settled tombstones of older epochs, then sweep.
    fn prune_epoch_indexes(&mut self, mut state: EpochCleanupState) -> io::Result<()> {
        let advanced = state.next;
        self.ledger.records.retain(|_, record| {
            record.storage_epoch >= advanced
                || record.material != MaterialState::Purged
                || record.transaction.is_some()
        });
        let retained: BTreeSet<_> = self
            .ledger
            .records
            .values()
            .map(|record| record.scope.key(&record.conversation))
            .collect();
        self.ledger
            .deleted_conversations
            .retain(|namespace, deleted| {
                deleted.storage_epoch >= advanced
                    || deleted.quota_pending
                    || retained.contains(namespace)
            });
        self.ledger.execution_epoch = advanced;
        state.pruned = true;
        self.ledger.epoch_cleanup = Some(state);
        self.dirty = true;
        self.sweep(self.ledger.last_sweep_at_ms)?;
        Ok(())
    }
}
