//! Restoring the changes of a command snapshot (or undoing a command
//! restore). The worker resolves each root, compares every file with the
//! recorded after hash and writes through its own handle-based replacement;
//! this module supplies the targets, the before content (`entry_material`)
//! and the `CommandRestore` record.
//!
//! A command restore record is shaped like a completed snapshot: one entry
//! per restored file, with the content the restore replaced as `before`, the
//! restored content as `after`, and the inverse change. It is therefore
//! itself a restore target, which makes a restore undoable. Its lifecycle is
//! the text one: `begin_command_restore` leaves it `BackupReady`; the worker
//! moves it to `CommitIntent` before the first write and calls
//! `finish_command_restore` with per-file results. A crash in between leaves
//! `OutcomeUnknown`: exportable, not restorable.
use super::*;
use backup::Annotation;

/// One restorable change of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTarget {
    /// Entry index; `entry_material` returns its before content.
    pub index: u64,
    pub root: u32,
    /// Identity of the root directory the change was recorded in.
    pub directory_identity: String,
    pub path: String,
    pub kind: ChangeKind,
    /// The current file must have this hash (`None`: it must be absent).
    pub expected_sha256: Option<String>,
    /// Hash of the content the restore writes (`None`: the file is removed).
    pub restored_sha256: Option<String>,
}

pub struct RestoreRequest<'a> {
    pub scope: Scope,
    pub conversation: &'a str,
    pub operation: &'a str,
    pub generation: &'a str,
    /// The command snapshot or command restore record being restored.
    pub source: &'a str,
    /// Files whose current state matched their target, in any order.
    pub files: Vec<RestoreFile<'a>>,
    pub now_ms: u64,
}

pub struct RestoreFile<'a> {
    /// `RestoreTarget::index` in the source record.
    pub index: u64,
    /// The content being replaced or removed (`None` when it is absent).
    pub current: Option<&'a [u8]>,
    /// The content to write back (`None` when the file is removed).
    pub restored: Option<&'a [u8]>,
}

fn inverse(kind: ChangeKind) -> ChangeKind {
    match kind {
        ChangeKind::Added => ChangeKind::Deleted,
        ChangeKind::Deleted => ChangeKind::Added,
        ChangeKind::Modified => ChangeKind::Modified,
    }
}

impl LockedVault {
    fn restore_source(
        &self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        now_ms: u64,
    ) -> io::Result<&Record> {
        let record = self.exportable(Some(scope), Some(conversation), id, now_ms)?;
        let restorable = matches!(
            (record.kind, record.change),
            (RecordKind::CommandSnapshot, ChangeState::Completed)
                | (RecordKind::CommandRestore, ChangeState::Succeeded)
        );
        if !restorable || record.expires_at_ms <= now_ms {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "these command changes cannot be restored",
            ));
        }
        Ok(record)
    }

    /// Protected changes of a completed command snapshot or a command
    /// restore, limited to `paths` (root index and path) when given. Paths
    /// without a protected change are left out.
    pub fn restore_targets(
        &self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        paths: Option<&[(u32, &str)]>,
        now_ms: u64,
    ) -> io::Result<Vec<RestoreTarget>> {
        let record = self.restore_source(scope, conversation, id, now_ms)?;
        let roots = &record
            .snapshot
            .as_ref()
            .ok_or_else(|| invalid("recovery snapshot record is corrupt"))?
            .roots;
        let mut targets = Vec::new();
        for (index, entry) in self.entry_table(record)?.into_iter().enumerate() {
            let Some(change) = &entry.change else {
                continue;
            };
            let selected = paths.is_none_or(|paths| {
                paths
                    .iter()
                    .any(|(root, path)| *root == entry.root && *path == entry.rel_path)
            });
            let Some(root) = roots.get(entry.root as usize) else {
                return Err(invalid("recovery snapshot record is corrupt"));
            };
            if !change.protected || !selected {
                continue;
            }
            targets.push(RestoreTarget {
                index: index as u64,
                root: entry.root,
                directory_identity: root.directory_identity.clone(),
                path: entry.rel_path,
                kind: change.kind,
                expected_sha256: change.after_sha256.clone(),
                restored_sha256: change.before_sha256.clone(),
            });
        }
        Ok(targets)
    }

    /// Back up the files a restore is about to change in a new
    /// `CommandRestore` record (steps 1-4 of the backup order; `reserve` as
    /// in `backup`).
    pub fn begin_command_restore(
        &mut self,
        request: RestoreRequest<'_>,
        expected_epoch: u64,
        reserve: impl FnMut(&Record, u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        let source = self.restore_source(
            &request.scope,
            request.conversation,
            request.source,
            request.now_ms,
        )?;
        let snapshot = source
            .snapshot
            .clone()
            .ok_or_else(|| invalid("recovery snapshot record is corrupt"))?;
        let directory_identity = source.directory_identity.clone();
        let table = self.entry_table(source)?;
        let mut paths = Vec::with_capacity(request.files.len());
        let mut annotations = Vec::with_capacity(request.files.len());
        for file in &request.files {
            let entry = table
                .get(file.index as usize)
                .filter(|entry| entry.change.as_ref().is_some_and(|change| change.protected))
                .ok_or_else(|| invalid("recovery entry is unavailable"))?;
            let kind = entry.change.as_ref().unwrap().kind;
            let shape = match kind {
                ChangeKind::Modified => file.current.is_some() && file.restored.is_some(),
                ChangeKind::Added => file.current.is_some() && file.restored.is_none(),
                ChangeKind::Deleted => file.current.is_none() && file.restored.is_some(),
            };
            if !shape
                || paths
                    .iter()
                    .any(|(root, path)| *root == entry.root && path == &entry.rel_path)
            {
                return Err(invalid("restore file does not match its recorded change"));
            }
            let identity = &snapshot.roots[entry.root as usize].directory_identity;
            annotations.push((
                entry.root,
                lineage(identity, &entry.rel_path),
                RecordedChange {
                    kind: inverse(kind),
                    before_sha256: file.current.map(hash),
                    after_sha256: file.restored.map(hash),
                    size: file
                        .restored
                        .or(file.current)
                        .map_or(0, |raw| raw.len() as u64),
                    protected: true,
                },
            ));
            paths.push((entry.root, entry.rel_path.clone()));
        }
        let backup = BackupRequest {
            scope: request.scope,
            conversation: request.conversation,
            operation: request.operation,
            generation: request.generation,
            kind: RecordKind::CommandRestore,
            directory_identity: &directory_identity,
            entries: request
                .files
                .iter()
                .zip(&paths)
                .map(|(file, (_, path))| EntryInput {
                    rel_path: path,
                    before: file.current,
                    after: file.restored,
                    metadata: None,
                })
                .collect(),
            transaction: None,
            now_ms: request.now_ms,
        };
        let encoded = self.encode(&backup)?;
        let annotation = Annotation {
            snapshot: SnapshotState {
                roots: snapshot
                    .roots
                    .into_iter()
                    .map(|root| SnapshotRootState {
                        directory_identity: root.directory_identity,
                        baseline: None,
                        manifest: None,
                    })
                    .collect(),
                batches: Vec::new(),
                started_at_ms: request.now_ms,
                finished_at_ms: None,
            },
            entries: annotations,
        };
        self.backup_annotated(backup, encoded, expected_epoch, reserve, Some(annotation))
    }

    /// Record the per-file results of a command restore in `CommitIntent`:
    /// `applied[i]` tells whether entry `i` was written. Entries that were
    /// skipped are dropped from the record (the new table is smaller than
    /// the reserved one). Nothing applied ends the record as `Aborted`.
    pub fn finish_command_restore(
        &mut self,
        scope: &Scope,
        id: &str,
        applied: &[bool],
        now_ms: u64,
    ) -> io::Result<Record> {
        let record = self
            .ledger
            .records
            .get(id)
            .filter(|record| {
                &record.scope == scope
                    && record.kind == RecordKind::CommandRestore
                    && record.change == ChangeState::CommitIntent
            })
            .ok_or_else(|| invalid("recovery operation unavailable"))?
            .clone();
        let entries = self.entry_table(&record)?;
        if entries.len() != applied.len() {
            return Err(invalid("restore results do not match the record"));
        }
        let kept: Vec<Entry> = entries
            .into_iter()
            .zip(applied)
            .filter_map(|(entry, applied)| applied.then_some(entry))
            .collect();
        if kept.is_empty() {
            let record = self.ledger.records.get_mut(id).unwrap();
            record.expires_at_ms = now_ms.min(record.expires_at_ms);
            self.finish(scope, id, ChangeState::Aborted)?;
            return Ok(self.ledger.records[id].clone());
        }
        if kept.len() != applied.len() {
            let (table, frame) = Self::table_storage(EntryTable {
                entries: kept,
                published: Vec::new(),
            })?;
            let (sha, frame) = frame.unwrap();
            let namespace = record.namespace();
            if self.stored_chain(&namespace, &sha)?.is_none() {
                let written = self.publish(&namespace, &sha, &frame)?;
                self.ledger.records.get_mut(id).unwrap().published_bytes += written;
            }
            self.ledger.records.get_mut(id).unwrap().entries = table;
        }
        let record = self.ledger.records.get_mut(id).unwrap();
        record.snapshot.as_mut().unwrap().finished_at_ms = Some(now_ms);
        Self::rebill(record)?;
        self.finish(scope, id, ChangeState::Succeeded)?;
        Ok(self.ledger.records[id].clone())
    }
}
