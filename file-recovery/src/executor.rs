//! Executor liveness for command snapshots.
//!
//! Every worker instance registers an executor: a random id whose lock file
//! `executors/<id>.lock` stays exclusively locked by an open handle for the
//! worker's lifetime. A `SnapshotPending` record names its executor; when
//! maintenance can acquire that lock, the executor exited without finishing
//! and the record becomes `OutcomeUnknown`. Lock files are created and probed
//! only under the vault lock, so a probe never observes a file between its
//! creation and its lock.
use super::*;
use std::fs::File;

/// A registered executor. Dropping it releases the lock, after which other
/// workers treat its unfinished snapshots as interrupted.
pub struct Executor {
    id: String,
    _lock: File,
}
impl Executor {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Vault {
    /// Register this worker instance as an executor of command snapshots.
    pub fn start_executor(&self) -> io::Result<Executor> {
        let locked = self.lock()?;
        let id = ledger::random_id();
        let lock = locked.storage.create_executor(&id)?;
        Ok(Executor { id, _lock: lock })
    }
}

impl LockedVault {
    /// Mark pending snapshots whose executor exited as `OutcomeUnknown`. The
    /// pre-run material is kept for export; no change list is computed after
    /// the fact. Written by the next ledger persist.
    pub(crate) fn settle_abandoned_snapshots(&mut self) {
        let executors: Vec<(String, String)> = self
            .ledger
            .records
            .values()
            .filter(|record| record.change == ChangeState::SnapshotPending)
            .filter_map(|record| Some((record.id.clone(), record.executor.clone()?)))
            .collect();
        for (id, executor) in executors {
            // A lock file that cannot be probed counts as alive: a pending
            // run is never ended on uncertain evidence, and cleanup of
            // other records proceeds.
            if self.storage.executor_alive(&executor).unwrap_or(true) {
                continue;
            }
            let record = self.ledger.records.get_mut(&id).unwrap();
            record.change = ChangeState::OutcomeUnknown;
            record.revision += 1;
            self.dirty = true;
        }
    }

    /// Remove lock files of executors that exited. Called on worker startup;
    /// a file that cannot be removed is retried by the next startup.
    pub(crate) fn remove_exited_executors(&self) {
        for id in self.storage.list_executors().unwrap_or_default() {
            let _ = self.storage.executor_alive(&id);
        }
    }
}
