//! Backup in the fixed crash-safe order: durable Preparing record, idempotent
//! quota reservation, rename-only object publication, BackupReady.
//!
//! Encoding decisions (reuse an existing object, choose a delta base) may be
//! made under an earlier lock guard. Publication rechecks each reused object
//! and delta base against the current ledger and falls back to a full frame
//! when a sweep has removed it in between.
use super::*;
use std::collections::{BTreeMap, HashMap};

pub struct BackupRequest<'a> {
    pub scope: Scope,
    pub conversation: &'a str,
    pub operation: &'a str,
    pub generation: &'a str,
    pub kind: RecordKind,
    /// The worker's identity of the resolved original directory.
    pub directory_identity: &'a str,
    pub entries: Vec<EntryInput<'a>>,
    /// The user-file transaction, persisted with the Preparing record.
    pub transaction: Option<TransactionPlan>,
    pub now_ms: u64,
}

/// `before` is the content being replaced or removed (absent when a restore
/// recreates a deleted file); `after` is the content about to be written.
#[derive(Clone, Copy)]
pub struct EntryInput<'a> {
    pub rel_path: &'a str,
    pub before: Option<&'a [u8]>,
    pub after: Option<&'a [u8]>,
    pub metadata: Option<&'a [u8]>,
}

/// Encoded objects for one request; valid for publication under any later
/// lock guard of the same vault.
pub struct EncodedBackup {
    namespace: String,
    entries: Vec<EncodedEntry>,
}
struct EncodedEntry {
    before: Option<Planned>,
    after: Option<Planned>,
    metadata: Option<Planned>,
}
#[derive(Clone)]
enum Planned {
    /// An object referenced by the ledger (or earlier in this batch).
    Reuse(ObjRef),
    New {
        object: ObjRef,
        frame: Vec<u8>,
    },
}
impl Planned {
    fn object(&self) -> &ObjRef {
        match self {
            Planned::Reuse(object) | Planned::New { object, .. } => object,
        }
    }
    fn frame_bytes(&self) -> u64 {
        match self {
            Planned::Reuse(_) => 0,
            Planned::New { frame, .. } => round_block(frame.len() as u64),
        }
    }
}

enum BaseSource<'a> {
    /// Raw content already in memory (the entry's `before`).
    Raw(&'a [u8]),
    /// A ledger object that must be decoded first.
    Stored,
}

/// Roots, lineages and changes of a command restore record's entries.
pub(crate) struct Annotation {
    pub snapshot: SnapshotState,
    /// Per entry: root index, lineage and change.
    pub entries: Vec<(u32, String, RecordedChange)>,
}
impl Annotation {
    fn apply(&self, entries: &mut [Entry]) {
        for (entry, (root, lineage, change)) in entries.iter_mut().zip(&self.entries) {
            entry.root = *root;
            entry.lineage = lineage.clone();
            entry.change = Some(change.clone());
        }
    }
}

/// Objects referenced by live records of one namespace: sha -> base chain.
pub(crate) type LiveObjects = HashMap<String, Vec<String>>;
/// An object name and the frame to publish under it.
pub(crate) type Frame = (String, Vec<u8>);

impl LockedVault {
    fn validate_request(&self, request: &BackupRequest<'_>) -> io::Result<()> {
        request.scope.validate()?;
        for value in [
            request.conversation,
            request.operation,
            request.generation,
            request.directory_identity,
        ] {
            identity(value)?;
        }
        let count = request.entries.len();
        let text = request.kind.is_text();
        // Command snapshots are written by `Vault::begin_snapshot`.
        if count == 0
            || count > MAX_RECORD_ENTRIES
            || (text && count != 1)
            || request.kind == RecordKind::CommandSnapshot
        {
            return Err(invalid("invalid recovery entry count"));
        }
        for entry in &request.entries {
            validate_rel_path(entry.rel_path, text)?;
            let shape = match request.kind {
                RecordKind::TextEdit => entry.before.is_some() && entry.after.is_some(),
                RecordKind::TextDelete => entry.before.is_some() && entry.after.is_none(),
                RecordKind::TextRestore
                | RecordKind::CommandSnapshot
                | RecordKind::CommandRestore => entry.before.is_some() || entry.after.is_some(),
            };
            let content_ok = |content: &[u8]| {
                content.len() <= MAX_TEXT_BYTES
                    && (!text || (!content.contains(&0) && std::str::from_utf8(content).is_ok()))
            };
            if !shape
                || !entry.before.is_none_or(content_ok)
                || !entry.after.is_none_or(content_ok)
                || entry
                    .metadata
                    .is_some_and(|metadata| metadata.len() > MAX_METADATA_BYTES)
            {
                return Err(invalid("recovery text or metadata exceeds its bound"));
            }
        }
        Ok(())
    }

    /// Encode a request's objects without changing the vault.
    pub fn encode(&self, request: &BackupRequest<'_>) -> io::Result<EncodedBackup> {
        self.validate_request(request)?;
        let namespace = request.scope.namespace();
        let live = self.live_objects(&namespace, None)?;
        let mut batch = HashMap::new();
        let mut entries = Vec::with_capacity(request.entries.len());
        for input in &request.entries {
            let lineage = lineage(request.directory_identity, input.rel_path);
            let latest = self.lineage_latest(&namespace, &lineage);
            let before = input
                .before
                .map(|raw| {
                    self.plan_object(
                        &namespace,
                        &live,
                        &mut batch,
                        raw,
                        latest.as_ref().map(|object| (object, BaseSource::Stored)),
                        true,
                    )
                })
                .transpose()?;
            let after = input
                .after
                .map(|raw| match (&before, input.before) {
                    (Some(before), Some(before_raw)) => self.plan_object(
                        &namespace,
                        &live,
                        &mut batch,
                        raw,
                        Some((before.object(), BaseSource::Raw(before_raw))),
                        false,
                    ),
                    _ => self.plan_object(
                        &namespace,
                        &live,
                        &mut batch,
                        raw,
                        latest.as_ref().map(|object| (object, BaseSource::Stored)),
                        true,
                    ),
                })
                .transpose()?;
            let metadata = input
                .metadata
                .map(|raw| self.plan_object(&namespace, &live, &mut batch, raw, None, false))
                .transpose()?;
            entries.push(EncodedEntry {
                before,
                after,
                metadata,
            });
        }
        Ok(EncodedBackup { namespace, entries })
    }

    /// Encoding rules, all bounded by the chain depth limit:
    /// 1. an object the ledger already references is reused without writing;
    /// 2. a lineage base yields a delta only below half the full frame size;
    /// 3. `after` is a delta on its own `before`.
    fn plan_object(
        &self,
        namespace: &str,
        live: &LiveObjects,
        batch: &mut HashMap<String, ObjRef>,
        raw: &[u8],
        base: Option<(&ObjRef, BaseSource<'_>)>,
        require_half: bool,
    ) -> io::Result<Planned> {
        let sha = hash(raw);
        if let Some(object) = batch.get(&sha) {
            return Ok(Planned::Reuse(object.clone()));
        }
        if let Some(bases) = live.get(&sha) {
            return Ok(Planned::Reuse(ObjRef {
                sha256: sha,
                bases: bases.clone(),
            }));
        }
        let mut planned = None;
        if let Some((base, source)) = base
            && base.bases.len() + 1 < MAX_CHAIN_DEPTH
        {
            // A stored base only selects a delta: when it cannot be read (for
            // example an object of an interrupted backup that was never
            // published), the content is stored as a full frame instead.
            let stored;
            let base_raw = match source {
                BaseSource::Raw(raw) => Some(raw),
                BaseSource::Stored => {
                    stored = self.read_object(namespace, base).ok();
                    stored.as_deref()
                }
            };
            let delta = base_raw
                .map(|base_raw| object::encode_delta(raw, &base.sha256, base_raw))
                .transpose()?;
            if let Some(delta) = delta
                && (!require_half || delta.len() * 2 < object::encode_full(raw)?.len())
            {
                let mut bases = Vec::with_capacity(base.bases.len() + 1);
                bases.push(base.sha256.clone());
                bases.extend(base.bases.iter().cloned());
                planned = Some(Planned::New {
                    object: ObjRef {
                        sha256: sha.clone(),
                        bases,
                    },
                    frame: delta,
                });
            }
        }
        let planned = match planned {
            Some(planned) => planned,
            None => Planned::New {
                object: ObjRef::full(sha.clone()),
                frame: object::encode_full(raw)?,
            },
        };
        batch.insert(sha, planned.object().clone());
        Ok(planned)
    }

    /// The newest live object of a lineage: the latest record's `after`, or
    /// its `before` when nothing was written after it.
    fn lineage_latest(&self, namespace: &str, lineage: &str) -> Option<ObjRef> {
        self.ledger
            .records
            .values()
            .filter(|record| record.material == MaterialState::Saved)
            .filter_map(|record| {
                let entry = record.inline_entry()?;
                (entry.lineage == lineage && record.namespace() == namespace).then(|| {
                    (
                        record.sequence,
                        entry.after.as_ref().or(entry.before.as_ref()),
                    )
                })
            })
            .max_by_key(|(sequence, _)| *sequence)
            .and_then(|(_, object)| object.cloned())
    }

    /// Objects referenced by live records of a namespace, excluding one record.
    pub(crate) fn live_objects(
        &self,
        namespace: &str,
        exclude: Option<&str>,
    ) -> io::Result<LiveObjects> {
        let mut live = LiveObjects::new();
        for record in self.ledger.records.values() {
            if record.material != MaterialState::Saved
                || exclude == Some(record.id.as_str())
                || record.namespace() != namespace
            {
                continue;
            }
            for object in self.record_objects(record)? {
                live.insert(object.sha256, object.bases);
            }
        }
        Ok(live)
    }

    /// Every object a record's entries keep alive, including its entry table
    /// and the objects it published. Snapshot roots (manifests and batches of
    /// an unfinished run) are marked by the sweep separately.
    pub(crate) fn record_objects(&self, record: &Record) -> io::Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        let entries = match &record.entries {
            Entries::Inline { entry } => vec![entry.clone()],
            Entries::Table { sha256, .. } => {
                objects.push(ObjRef::full(sha256.clone()));
                match self.table(record) {
                    Ok(table) => {
                        objects.extend(table.published);
                        table.entries
                    }
                    // A Preparing record may have crashed before publishing its table.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Err(error),
                }
            }
            Entries::PreRun { .. } => Vec::new(),
        };
        for entry in entries {
            objects.extend(entry.before);
            objects.extend(entry.after);
            objects.extend(entry.metadata.map(ObjRef::full));
        }
        Ok(objects)
    }

    /// Full backup with encoding under this guard. `reserve` receives the
    /// record and the cumulative upper bound of its quota reservation; it may
    /// be called again with a larger bound when publication falls back to full
    /// frames.
    ///
    /// Quota requests must be sent and answered while this guard is held, so
    /// that a reconciliation by another worker cannot interleave with them.
    pub fn backup(
        &mut self,
        request: BackupRequest<'_>,
        expected_epoch: u64,
        reserve: impl FnMut(&Record, u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        let encoded = self.encode(&request)?;
        self.backup_encoded(request, encoded, expected_epoch, reserve)
    }

    /// Steps 1-4 of the fixed order. The BackupReady state is persisted by
    /// the next ledger write (transaction registration, commit intent or
    /// abort), which always precedes the user-file commit. Until then the
    /// durable Preparing state has the same recovery outcome (Aborted).
    pub fn backup_encoded(
        &mut self,
        request: BackupRequest<'_>,
        encoded: EncodedBackup,
        expected_epoch: u64,
        reserve: impl FnMut(&Record, u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.backup_annotated(request, encoded, expected_epoch, reserve, None)
    }

    /// `backup_encoded` with per-entry roots and changes and the snapshot
    /// state of a command restore record.
    pub(crate) fn backup_annotated(
        &mut self,
        request: BackupRequest<'_>,
        encoded: EncodedBackup,
        expected_epoch: u64,
        mut reserve: impl FnMut(&Record, u64) -> io::Result<()>,
        annotation: Option<Annotation>,
    ) -> io::Result<Record> {
        self.require_execution_epoch(expected_epoch)?;
        self.validate_request(&request)?;
        // Restore records need their roots and changes.
        if (request.kind == RecordKind::CommandRestore) != annotation.is_some()
            || annotation
                .as_ref()
                .is_some_and(|annotation| annotation.entries.len() != request.entries.len())
        {
            return Err(invalid("invalid recovery entry count"));
        }
        let namespace = request.scope.namespace();
        if encoded.namespace != namespace || encoded.entries.len() != request.entries.len() {
            return Err(invalid("encoded backup does not match its request"));
        }
        if self.is_deleted(&request.scope, request.conversation) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "conversation was deleted",
            ));
        }
        let id = record_id(&request.scope, request.conversation, request.operation)?;
        // An existing operation is never permission to repeat its file mutation.
        if self.ledger.records.contains_key(&id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recovery operation already exists; do not repeat the file change",
            ));
        }
        if self.ledger.records.len() >= MAX_RECORDS {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "file backup capacity exhausted; file was not changed",
            ));
        }
        let transaction = request
            .transaction
            .clone()
            .map(|plan| plan.into_transaction(&id))
            .transpose()?;
        let mut entries = self.entries_of(&request, &encoded.entries);
        if let Some(annotation) = &annotation {
            annotation.apply(&mut entries);
        }
        let (record_entries, table_frame) = Self::entries_storage(entries)?;
        let mut record = Record {
            id: id.clone(),
            scope: request.scope.clone(),
            conversation: request.conversation.into(),
            operation: request.operation.into(),
            generation: request.generation.into(),
            kind: request.kind,
            directory_identity: request.directory_identity.into(),
            entries: record_entries,
            sequence: self.ledger.sequence + 1,
            created_at_ms: request.now_ms,
            expires_at_ms: request
                .now_ms
                .saturating_add(u64::from(self.ledger.policy.retention_days) * 86_400_000),
            change: ChangeState::Preparing,
            material: MaterialState::Saved,
            revision: 1,
            published_bytes: 0,
            serialized_bytes: 0,
            cleanup_error: None,
            transaction,
            discard_requested: false,
            clock_generation: self.ledger.clock_generation,
            storage_epoch: self.ledger.execution_epoch,
            executor: None,
            snapshot: annotation
                .as_ref()
                .map(|annotation| annotation.snapshot.clone()),
        };
        Self::rebill(&mut record)?;
        let planned_bytes = encoded
            .entries
            .iter()
            .flat_map(|entry| [&entry.before, &entry.after, &entry.metadata])
            .flatten()
            .map(Planned::frame_bytes)
            .sum::<u64>()
            + table_frame
                .as_ref()
                .map_or(0, |(_, frame)| round_block(frame.len() as u64));
        let mut reserved = planned_bytes + record.serialized_bytes;
        if self.used_bytes().saturating_add(reserved) > self.ledger.policy.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "file backup capacity exhausted; file was not changed",
            ));
        }
        self.ledger.sequence += 1;
        self.ledger.records.insert(id.clone(), record);
        // Step 1: the durable record precedes the shared reservation, including
        // when the reply is lost after the daemon has reserved.
        self.persist()?;
        let result = (|| {
            // Step 2.
            reserve(&self.ledger.records[&id], reserved)?;
            // Step 3.
            let (mut entries, frames) = self.finalize(&id, &request, encoded)?;
            if let Some(annotation) = &annotation {
                annotation.apply(&mut entries);
            }
            let (record_entries, table_frame) = Self::entries_storage(entries)?;
            let mut publish: Vec<Frame> = frames;
            if let (Entries::Table { sha256, .. }, Some((_, frame))) =
                (&record_entries, table_frame)
            {
                publish.push((sha256.clone(), frame));
            }
            let total = publish
                .iter()
                .map(|(_, frame)| round_block(frame.len() as u64))
                .sum::<u64>()
                + self.ledger.records[&id].serialized_bytes;
            if total > reserved {
                reserved = total;
                reserve(&self.ledger.records[&id], reserved)?;
            }
            let mut published = 0;
            for (sha, frame) in &publish {
                published += self.publish(&namespace, sha, frame)?;
            }
            // Step 4.
            let record = self.ledger.records.get_mut(&id).unwrap();
            record.entries = record_entries;
            record.published_bytes = published;
            record.change = ChangeState::BackupReady;
            record.revision += 1;
            Self::rebill(record)?;
            self.dirty = true;
            Ok(())
        })();
        if let Err(error) = result {
            self.abort_record(&id, request.now_ms);
            let _ = self.persist();
            return Err(error);
        }
        Ok(self.ledger.records[&id].clone())
    }

    fn entries_of(&self, request: &BackupRequest<'_>, encoded: &[EncodedEntry]) -> Vec<Entry> {
        request
            .entries
            .iter()
            .zip(encoded)
            .map(|(input, planned)| Entry {
                rel_path: input.rel_path.into(),
                before: planned.before.as_ref().map(|p| p.object().clone()),
                after: planned.after.as_ref().map(|p| p.object().clone()),
                metadata: planned.metadata.as_ref().map(|p| p.object().sha256.clone()),
                lineage: lineage(request.directory_identity, input.rel_path),
                root: 0,
                change: None,
            })
            .collect()
    }

    /// One text entry stays inline; other records use a table object.
    pub(crate) fn entries_storage(mut entries: Vec<Entry>) -> io::Result<(Entries, Option<Frame>)> {
        if entries.len() == 1 && entries[0].change.is_none() {
            return Ok((
                Entries::Inline {
                    entry: entries.pop().unwrap(),
                },
                None,
            ));
        }
        Self::table_storage(EntryTable {
            entries,
            published: Vec::new(),
        })
    }

    pub(crate) fn table_storage(table: EntryTable) -> io::Result<(Entries, Option<Frame>)> {
        let count = table.entries.len() as u64;
        let raw = serde_json::to_vec(&table)?;
        if raw.len() > MAX_TEXT_BYTES {
            return Err(invalid("recovery entry table exceeds its bound"));
        }
        let sha = hash(&raw);
        Ok((
            Entries::Table {
                sha256: sha.clone(),
                count,
            },
            Some((sha, object::encode_full(&raw)?)),
        ))
    }

    /// Recheck encoding decisions under the lock that publishes them (R1). A
    /// reused object, or a delta base, is kept only when the current ledger
    /// still references it with the same chain and every file of that chain
    /// exists now; the Preparing record persisted in step 1 then keeps it from
    /// any later sweep. Otherwise the raw content is published as a full
    /// frame. Existence, not ledger liveness alone, decides: an interrupted
    /// backup may reference objects it never published.
    fn finalize(
        &self,
        id: &str,
        request: &BackupRequest<'_>,
        encoded: EncodedBackup,
    ) -> io::Result<(Vec<Entry>, Vec<Frame>)> {
        let namespace = encoded.namespace.as_str();
        let live = self.live_objects(namespace, Some(id))?;
        let mut present: HashMap<String, bool> = HashMap::new();
        let mut stored = |sha: &str, chain: &[String]| -> io::Result<bool> {
            if live.get(sha).is_none_or(|bases| bases.as_slice() != chain) {
                return Ok(false);
            }
            for name in std::iter::once(sha).chain(chain.iter().map(String::as_str)) {
                let exists = match present.get(name) {
                    Some(exists) => *exists,
                    None => {
                        let exists = self.storage.object_exists(namespace, name)?;
                        present.insert(name.to_owned(), exists);
                        exists
                    }
                };
                if !exists {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut published: BTreeMap<String, ObjRef> = BTreeMap::new();
        let mut frames = Vec::new();
        let mut settle =
            |planned: Option<Planned>, raw: Option<&[u8]>| -> io::Result<Option<ObjRef>> {
                let (Some(planned), Some(raw)) = (planned, raw) else {
                    return Ok(None);
                };
                let sha = planned.object().sha256.clone();
                if let Some(object) = published.get(&sha) {
                    return Ok(Some(object.clone()));
                }
                if let Some(bases) = live.get(&sha)
                    && stored(&sha, bases)?
                {
                    return Ok(Some(ObjRef {
                        sha256: sha,
                        bases: bases.clone(),
                    }));
                }
                // An object held only by roots outside the record ledger (a
                // manifest or a pending snapshot) is kept with its chain.
                if let Some(bases) = self.stored_chain(namespace, &sha)? {
                    let object = ObjRef {
                        sha256: sha.clone(),
                        bases,
                    };
                    published.insert(sha, object.clone());
                    return Ok(Some(object));
                }
                let delta_base_alive = match &planned {
                    Planned::New { object, .. } => match object.bases.split_first() {
                        None => true,
                        Some((base, chain)) => {
                            published
                                .get(base)
                                .is_some_and(|known| known.bases == chain)
                                || stored(base, chain)?
                        }
                    },
                    Planned::Reuse(_) => false,
                };
                let (object, frame) = match planned {
                    Planned::New { object, frame } if delta_base_alive => (object, frame),
                    _ => (ObjRef::full(sha.clone()), object::encode_full(raw)?),
                };
                published.insert(sha.clone(), object.clone());
                frames.push((sha, frame));
                Ok(Some(object))
            };
        let mut entries = Vec::with_capacity(encoded.entries.len());
        for (input, planned) in request.entries.iter().zip(encoded.entries) {
            let before = settle(planned.before, input.before)?;
            let after = settle(planned.after, input.after)?;
            let metadata = settle(planned.metadata, input.metadata)?.map(|object| object.sha256);
            entries.push(Entry {
                rel_path: input.rel_path.into(),
                before,
                after,
                metadata,
                lineage: lineage(request.directory_identity, input.rel_path),
                root: 0,
                change: None,
            });
        }
        Ok((entries, frames))
    }

    /// Publish one object and update its namespace's counts. Returns the
    /// block-rounded bytes written.
    pub(crate) fn publish(&mut self, namespace: &str, sha: &str, frame: &[u8]) -> io::Result<u64> {
        let previous = self.storage.publish_object(namespace, sha, frame)?;
        let written = round_block(frame.len() as u64);
        let usage = self
            .ledger
            .namespaces
            .entry(namespace.to_owned())
            .or_default();
        match previous {
            None => usage.objects += 1,
            Some(len) => usage.bytes = usage.bytes.saturating_sub(round_block(len)),
        }
        usage.bytes += written;
        Ok(written)
    }

    /// Publish several objects with one durability barrier and update their
    /// namespace's counts. Returns the block-rounded bytes written.
    pub(crate) fn publish_batch(
        &mut self,
        namespace: &str,
        frames: &[(String, Vec<u8>)],
    ) -> io::Result<u64> {
        if frames.is_empty() {
            return Ok(0);
        }
        let previous = self.storage.publish_objects(namespace, frames)?;
        let usage = self
            .ledger
            .namespaces
            .entry(namespace.to_owned())
            .or_default();
        let mut written = 0;
        for ((_, frame), previous) in frames.iter().zip(previous) {
            match previous {
                None => usage.objects += 1,
                Some(len) => usage.bytes = usage.bytes.saturating_sub(round_block(len)),
            }
            let bytes = round_block(frame.len() as u64);
            usage.bytes += bytes;
            written += bytes;
        }
        Ok(written)
    }

    /// Terminal failure before the user file was touched.
    pub(crate) fn abort_record(&mut self, id: &str, now_ms: u64) {
        if let Some(record) = self.ledger.records.get_mut(id) {
            record.change = ChangeState::Aborted;
            record.expires_at_ms = now_ms.min(record.expires_at_ms);
            record.revision += 1;
            Self::drop_after(record);
        }
    }

    /// An aborted change never wrote its `after` version; it is not a root.
    pub(crate) fn drop_after(record: &mut Record) {
        if let Entries::Inline { entry } = &mut record.entries {
            entry.after = None;
        }
    }
}
