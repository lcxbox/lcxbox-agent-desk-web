//! Record state transitions, retention, discard, conversation deletion and
//! restart recovery. Removing records only removes roots; objects are freed
//! by the sweep that follows.
use super::*;
use ledger::DeletedConversation;

/// Periodic maintenance sweeps at most this often without other triggers.
const SWEEP_INTERVAL_MS: u64 = 60 * 60 * 1000;

impl LockedVault {
    pub fn transition(&mut self, scope: &Scope, id: &str, next: ChangeState) -> io::Result<()> {
        self.apply_transition(scope, id, next)?;
        self.persist()
    }

    fn apply_transition(&mut self, scope: &Scope, id: &str, next: ChangeState) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| &r.scope == scope)
            .ok_or_else(|| invalid("recovery operation unavailable"))?;
        let allowed = matches!(
            (record.change, next),
            (ChangeState::Preparing, ChangeState::Aborted)
                | (
                    ChangeState::BackupReady,
                    ChangeState::CommitIntent | ChangeState::Aborted
                )
                | (
                    ChangeState::CommitIntent,
                    ChangeState::Succeeded | ChangeState::Aborted
                )
        );
        if !allowed {
            return Err(invalid("invalid recovery operation transition"));
        }
        record.change = next;
        record.revision += 1;
        if next == ChangeState::Aborted {
            Self::drop_after(record);
        }
        Ok(())
    }

    /// Record the final outcome, remove the registered transaction objects
    /// and persist both in one ledger write. Returns whether the transaction
    /// was settled; a cleanup failure is retained on the record for retry.
    pub fn finish(&mut self, scope: &Scope, id: &str, outcome: ChangeState) -> io::Result<bool> {
        if !matches!(outcome, ChangeState::Succeeded | ChangeState::Aborted) {
            return Err(invalid("invalid recovery operation outcome"));
        }
        self.apply_transition(scope, id, outcome)?;
        let settled = self.settle_transaction_in_memory(id)?;
        self.persist()?;
        Ok(settled)
    }

    /// Explicit owner confirmation only abandons recovery material. It never
    /// changes the recorded outcome or replays a mutation of the user's file.
    pub fn discard(
        &mut self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        self.check_cleanup_clock(now_ms)?;
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|record| &record.scope == scope && record.conversation == conversation)
            .ok_or_else(|| invalid("recovery material unavailable"))?;
        if !matches!(
            record.change,
            ChangeState::Succeeded
                | ChangeState::Aborted
                | ChangeState::OutcomeUnknown
                | ChangeState::Completed
        ) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "recovery operation has not settled",
            ));
        }
        if !record.discard_requested {
            record.discard_requested = true;
            record.revision += 1;
            self.dirty = true;
        }
        self.cleanup(now_ms)
    }

    /// Tombstone the conversation, purge its records and sweep objects no
    /// other live record references. The tombstone is persisted even when
    /// cleanup is paused by the clock guard.
    pub fn delete_conversation(
        &mut self,
        scope: &Scope,
        conversation: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        scope.validate()?;
        identity(conversation)?;
        if now_ms == 0 {
            return Err(invalid("invalid recovery deletion time"));
        }
        let key = scope.key(conversation);
        if !self.ledger.deleted_conversations.contains_key(&key) {
            self.ledger.deleted_conversations.insert(
                key,
                DeletedConversation {
                    deleted_at_ms: now_ms,
                    quota_pending: true,
                    storage_epoch: self.ledger.execution_epoch,
                },
            );
            self.dirty = true;
        }
        match self.cleanup(now_ms) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.flush()?;
                Err(error)
            }
        }
    }

    /// Called under the exclusive lock after a worker restart. Missing syscall
    /// results become terminal unknown outcomes, never success or failure guesses.
    /// Unknown outcomes retain their materials until explicit discard; the journal fences replay.
    pub fn recover_interrupted(&mut self, now_ms: u64) -> io::Result<Vec<Record>> {
        self.check_cleanup_clock(now_ms)?;
        for record in self.ledger.records.values_mut() {
            match record.change {
                ChangeState::Preparing | ChangeState::BackupReady => {
                    record.change = ChangeState::Aborted;
                    record.expires_at_ms = now_ms.min(record.expires_at_ms);
                    record.revision += 1;
                    Self::drop_after(record);
                    self.dirty = true;
                }
                ChangeState::CommitIntent => {
                    record.change = ChangeState::OutcomeUnknown;
                    record.revision += 1;
                    self.dirty = true;
                }
                // A pending snapshot may belong to a running executor of
                // another worker; `cleanup` probes its executor lock.
                ChangeState::OutcomeUnknown
                | ChangeState::Succeeded
                | ChangeState::Aborted
                | ChangeState::SnapshotPending
                | ChangeState::Completed => (),
            }
        }
        // A failed immediate cleanup must be retried before expiry, too.
        // Cleanup only registered temporary objects, never replay the target mutation.
        let settled: Vec<_> = self
            .ledger
            .records
            .values()
            .filter(|r| r.transaction.is_some() && !r.in_flight())
            .map(|r| r.id.clone())
            .collect();
        for id in settled {
            self.settle_transaction_in_memory(&id)?;
        }
        self.cleanup(now_ms)?;
        self.remove_exited_executors();
        Ok(self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.change == ChangeState::OutcomeUnknown && r.material != MaterialState::Purged
            })
            .cloned()
            .collect())
    }

    /// Purge expired, discarded and deleted-conversation records and
    /// manifests, then sweep. Pending snapshots of exited executors become
    /// unknown outcomes. API success is sufficient; cleanup does not perform
    /// post-delete stat calls.
    pub fn cleanup(&mut self, now_ms: u64) -> io::Result<()> {
        self.check_cleanup_clock(now_ms)?;
        self.settle_abandoned_snapshots();
        let ids: Vec<_> = self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.material != MaterialState::Purged
                    && (r.discard_requested
                        || self.is_deleted(&r.scope, &r.conversation)
                        || (r.expires_at_ms <= now_ms
                            && matches!(
                                r.change,
                                ChangeState::Succeeded
                                    | ChangeState::Aborted
                                    | ChangeState::Completed
                            )))
            })
            .map(|r| r.id.clone())
            .collect();
        let manifests = self.ledger.manifests.len();
        let deleted = &self.ledger.deleted_conversations;
        self.ledger.manifests.retain(|_, manifest| {
            manifest.expires_at_ms > now_ms
                && !deleted.contains_key(&manifest.scope.key(&manifest.conversation))
        });
        let mut purged = self.ledger.manifests.len() != manifests;
        self.dirty |= purged;
        for id in ids {
            // Windows transaction cleanup compares the retained original with
            // the backup, so material stays until the transaction is settled.
            if !self.settle_transaction_in_memory(&id)? {
                continue;
            }
            let record = self.ledger.records.get_mut(&id).unwrap();
            record.material = MaterialState::Purged;
            record.revision += 1;
            purged = true;
            self.dirty = true;
        }
        // The conversation tombstone fences every old operation, so purged
        // per-file records can be dropped without permitting a replay.
        let deleted = &self.ledger.deleted_conversations;
        let before = self.ledger.records.len();
        self.ledger.records.retain(|_, record| {
            record.material != MaterialState::Purged
                || record.transaction.is_some()
                || !deleted.contains_key(&record.scope.key(&record.conversation))
        });
        self.dirty |= self.ledger.records.len() != before;
        if purged {
            // The purge is durable before any object it released is deleted.
            self.sweep(now_ms)?;
        } else {
            self.flush()?;
        }
        Ok(())
    }

    /// Periodic maintenance: sweep when the interval elapsed.
    pub fn sweep_if_due(&mut self, now_ms: u64) -> io::Result<Option<SweepReport>> {
        if now_ms.saturating_sub(self.ledger.last_sweep_at_ms) < SWEEP_INTERVAL_MS {
            return Ok(None);
        }
        self.sweep(now_ms).map(Some)
    }

    /// Production callers sample the OS clock before maintenance or mutation.
    /// Read-only management may still return records while cleanup is paused.
    pub fn observe_system_clock(&mut self) -> io::Result<()> {
        let before = serde_json::to_vec(&self.ledger.clock_guard)?;
        self.ledger.clock_guard.observe_system();
        if serde_json::to_vec(&self.ledger.clock_guard)? != before {
            self.persist()?;
        }
        Ok(())
    }

    /// Owner explicitly accepts the displayed device time. This only changes
    /// the clock fence, never original record timestamps or mutation outcomes.
    pub fn acknowledge_clock(&mut self, displayed_wall_ms: u64) -> io::Result<()> {
        if !self.cleanup_clock_paused() {
            return Ok(());
        }
        let generation = self
            .ledger
            .clock_generation
            .checked_add(1)
            .ok_or_else(|| invalid("recovery clock generation exhausted"))?;
        let now = self
            .ledger
            .clock_guard
            .acknowledge_system(displayed_wall_ms)?;
        self.ledger.clock_generation = generation;
        self.ledger.last_cleanup_at_ms = now;
        self.ledger.cleanup_clock_paused = false;
        self.persist()
    }

    /// A pause is persisted across worker restarts and never lowered by a retry:
    /// retrying cleanup is not permission to discard material after a clock
    /// reset. Advancing the fence is written with the next ledger update.
    pub(crate) fn check_cleanup_clock(&mut self, now_ms: u64) -> io::Result<()> {
        if self.ledger.clock_guard.initialized() {
            self.ledger.clock_guard.observe_system();
        }
        let latest_creation = self
            .ledger
            .records
            .values()
            .filter(|record| record.clock_generation == self.ledger.clock_generation)
            .map(|record| record.created_at_ms)
            .max()
            .unwrap_or(0);
        if self.ledger.clock_guard.blocked
            || now_ms == 0
            || now_ms < self.ledger.last_cleanup_at_ms.max(latest_creation)
        {
            self.ledger.cleanup_clock_paused = true;
            self.persist()?;
            return Err(io::Error::other(CleanupClockError));
        }
        self.ledger.last_cleanup_at_ms = now_ms;
        if self.ledger.cleanup_clock_paused {
            self.ledger.cleanup_clock_paused = false;
            self.dirty = true;
        }
        Ok(())
    }
}
