//! Best-effort snapshots of the directories a command may change.
//!
//! `Vault::begin_snapshot` runs before the command is spawned:
//! 1. under the vault lock it checks the conversation tombstone and writes a
//!    `SnapshotPending` record naming its executor and pinning the latest
//!    manifest of each root as the scan baseline;
//! 2. it scans every root outside the lock, rereading only files whose stat
//!    changed (or whose mtime is racy), and publishes new text objects in
//!    batches: each batch reserves its upper bound under the record's
//!    operation id, publishes, and attaches its objects to the record while
//!    the lock is held;
//! 3. under the lock it publishes the pre-run manifests, which replace the
//!    baselines and batches as the record's roots.
//!
//! `Vault::finish_snapshot` runs after the command exited (also on timeout
//! or cancellation). It rescans with the frozen ignore rules against this
//! record's pre-run manifests, so concurrent commands on overlapping roots do
//! not interfere, publishes after objects in batches, and then, under one
//! lock hold that first checks the tombstone, publishes the entry table and
//! post-run manifests, replaces the conversation's latest manifests and marks
//! the record `Completed`.
//!
//! When quota is insufficient, new objects are not stored: their files are
//! recorded without an object, reread by the next scan, and reported as
//! unprotected changes. Every quota request is made while the vault lock is
//! held.
use super::*;
use manifest::{Content, Manifest, ManifestFile};
use scan::{Rules, Stop, Walk};
use std::{
    collections::HashMap,
    fs::File,
    time::{Duration, Instant},
};

mod changes;
mod manifest;
pub(crate) use manifest::ManifestPointer;
#[cfg(all(test, unix))]
mod batch_tests;
mod scan;

/// File contents read by snapshot scans on this thread.
#[cfg(all(test, unix))]
pub(crate) fn content_reads() -> u64 {
    scan::CONTENT_READS.with(std::cell::Cell::get)
}

/// A batch is published once its raw content or object count reaches these.
const BATCH_RAW_BYTES: usize = 32 * 1024 * 1024;
const BATCH_OBJECTS: usize = 2_000;
/// Files modified this close to (or after) the previous scan are reread.
const RACY_NS: i64 = 2_000_000_000;
const MAX_ROOTS: usize = 64;
/// Ledger growth allowance of one record update.
const RECORD_GROWTH: u64 = 512;

/// Per-scan bounds; exceeding one skips the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    /// Included files of one scan across all roots.
    pub max_files: usize,
    /// Wall time of one scan, including reading and publishing.
    pub max_duration: Duration,
}
impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_duration: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    TooManyFiles,
    TimeLimit,
    StorageFull,
    ManifestTooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A change of one path as recorded in a command snapshot or command
/// restore entry. Hashes are absent for files compared by stat only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedChange {
    pub kind: ChangeKind,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    /// Size after the change, or before it for a deletion.
    pub size: u64,
    /// Both sides are saved text, so the change can be restored.
    pub protected: bool,
}
impl RecordedChange {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self
            .before_sha256
            .iter()
            .chain(&self.after_sha256)
            .any(|sha| !valid_id(sha))
        {
            return Err(invalid("recovery change is corrupt"));
        }
        Ok(())
    }
}

/// One entry of a change list, for the worker to map to its protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Index of the entry in the record, for export and restore.
    pub index: u64,
    pub root: u32,
    /// Relative to the root, `/`-separated.
    pub path: String,
    pub kind: ChangeKind,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub size: u64,
    pub protected: bool,
}
impl FileChange {
    pub(crate) fn from_entry(index: u64, entry: &Entry) -> Option<Self> {
        let change = entry.change.as_ref()?;
        Some(Self {
            index,
            root: entry.root,
            path: entry.rel_path.clone(),
            kind: change.kind,
            before_sha256: change.before_sha256.clone(),
            after_sha256: change.after_sha256.clone(),
            size: change.size,
            protected: change.protected,
        })
    }
}

/// Snapshot roots and run state of a command snapshot or command restore
/// record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotState {
    pub roots: Vec<SnapshotRootState>,
    /// Object lists of batches published by an unfinished scan.
    pub batches: Vec<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}
impl SnapshotState {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.roots.is_empty() || self.roots.len() > MAX_ROOTS {
            return Err(invalid("recovery snapshot roots are corrupt"));
        }
        for root in &self.roots {
            identity(&root.directory_identity)?;
            if root
                .baseline
                .iter()
                .chain(&root.manifest)
                .any(|sha| !valid_id(sha))
            {
                return Err(invalid("recovery snapshot roots are corrupt"));
            }
        }
        if self.batches.iter().any(|sha| !valid_id(sha)) {
            return Err(invalid("recovery snapshot batches are corrupt"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRootState {
    /// The worker's identity of the root directory; it compares it on restore.
    pub directory_identity: String,
    /// The latest manifest pinned as the pre-run scan's baseline.
    pub baseline: Option<String>,
    /// The pre-run manifest of an unfinished or interrupted run.
    pub manifest: Option<String>,
}

/// A root directory opened by the worker without following links. The handle
/// must stay valid until `finish_snapshot`; the snapshot keeps its own clone.
pub struct SnapshotRoot<'a> {
    pub directory: &'a File,
    pub directory_identity: &'a str,
}

pub struct SnapshotRequest<'a> {
    pub scope: Scope,
    pub conversation: &'a str,
    pub operation: &'a str,
    pub generation: &'a str,
    pub executor: &'a Executor,
    pub roots: Vec<SnapshotRoot<'a>>,
    pub expected_epoch: u64,
    pub now_ms: u64,
    pub limits: ScanLimits,
}

#[allow(
    clippy::large_enum_variant,
    reason = "returned once per command and moved into the run"
)]
pub enum SnapshotBegin {
    Taken(PendingSnapshot),
    /// No snapshot: the command runs unprotected. The record is aborted; the
    /// caller still clears the operation's quota reservation.
    Skipped(SkipReason),
}

#[allow(
    clippy::large_enum_variant,
    reason = "returned once per command and consumed by the caller"
)]
pub enum SnapshotFinish {
    Taken {
        record: Record,
        /// Sorted by root and path.
        changes: Vec<FileChange>,
    },
    Skipped(SkipReason),
    /// The conversation was deleted while the command ran; nothing was
    /// published.
    Discarded,
}

struct RootRun {
    directory: File,
    identity: String,
    rules: Rules,
    manifest: Manifest,
}

/// The state between `begin_snapshot` and `finish_snapshot`.
pub struct PendingSnapshot {
    scope: Scope,
    conversation: String,
    id: String,
    roots: Vec<RootRun>,
    reserved: u64,
    published: Vec<ObjRef>,
    limits: ScanLimits,
    started_at_ms: u64,
}
impl PendingSnapshot {
    pub fn record_id(&self) -> &str {
        &self.id
    }
    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }
    /// Included files per root at the pre-run scan.
    pub fn files(&self) -> Vec<u64> {
        self.roots
            .iter()
            .map(|root| root.manifest.files.len() as u64)
            .collect()
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// New content waiting for its batch.
struct PendingObject {
    file: usize,
    sha: String,
    raw: Vec<u8>,
    /// The previous version of the same path, as a delta base.
    base: Option<ObjRef>,
}

/// Publication state of one scan.
struct Run<'a> {
    vault: &'a Vault,
    scope: &'a Scope,
    conversation: &'a str,
    id: &'a str,
    reserve: &'a mut dyn FnMut(&Record, u64) -> io::Result<()>,
    /// Cumulative reservation of the operation.
    reserved: u64,
    /// Quota was refused; later content of this scan is hashed only.
    exhausted: bool,
    /// Content objects this record wrote itself.
    published: Vec<ObjRef>,
}

impl Run<'_> {
    /// Lock the vault while the record is still a pending snapshot of a live
    /// conversation.
    fn lock_pending(&self) -> Result<LockedVault, Stop> {
        let locked = self.vault.lock()?;
        let pending = locked
            .ledger
            .records
            .get(self.id)
            .is_some_and(|record| record.change == ChangeState::SnapshotPending);
        if !pending || locked.is_deleted(self.scope, self.conversation) {
            return Err(Stop::Discarded);
        }
        Ok(locked)
    }

    /// Reserve `growth` more bytes for this operation. `false` when the
    /// vault or the device quota has no room.
    fn secure(&mut self, locked: &LockedVault, growth: u64) -> Result<bool, Stop> {
        if locked.used_bytes().saturating_add(growth) > locked.ledger.policy.max_bytes {
            return Ok(false);
        }
        let record = &locked.ledger.records[self.id];
        let total = if self.reserved == 0 {
            record.serialized_bytes + growth
        } else {
            self.reserved + growth
        };
        match (self.reserve)(record, total) {
            Ok(()) => {
                self.reserved = total;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::StorageFull => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Publish a full-frame object (a manifest, batch list or entry table)
    /// unless a complete object of this name exists.
    fn put(&self, locked: &mut LockedVault, sha: &str, frame: &[u8]) -> io::Result<()> {
        let namespace = self.scope.namespace();
        if locked.stored_chain(&namespace, sha)?.is_some() {
            return Ok(());
        }
        let written = locked.publish(&namespace, sha, frame)?;
        locked
            .ledger
            .records
            .get_mut(self.id)
            .unwrap()
            .published_bytes += written;
        Ok(())
    }

    /// Encode a batch outside the lock and publish it under one lock hold,
    /// attaching its objects to the record.
    fn flush(
        &mut self,
        files: &mut [ManifestFile],
        batch: Vec<PendingObject>,
        require_half: bool,
    ) -> Result<(), Stop> {
        if batch.is_empty() {
            return Ok(());
        }
        let namespace = self.scope.namespace();
        let mut bases: HashMap<String, Vec<u8>> = HashMap::new();
        let wanted: Vec<&ObjRef> = batch
            .iter()
            .filter_map(|object| object.base.as_ref())
            .filter(|base| base.bases.len() + 1 < MAX_CHAIN_DEPTH)
            .collect();
        if !wanted.is_empty() {
            let locked = self.lock_pending()?;
            for base in wanted {
                // An unreadable base only means a full frame.
                if !bases.contains_key(&base.sha256)
                    && let Ok(raw) = locked.read_object(&namespace, base)
                {
                    bases.insert(base.sha256.clone(), raw);
                }
            }
        }
        let mut encoded: Vec<(String, ObjRef, Vec<u8>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for object in &batch {
            if !seen.insert(object.sha.as_str()) {
                continue;
            }
            let delta = match object
                .base
                .as_ref()
                .and_then(|base| Some((base, bases.get(&base.sha256)?)))
            {
                Some((base, raw)) => {
                    let mut chain = vec![base.sha256.clone()];
                    chain.extend(base.bases.iter().cloned());
                    let planned = ObjRef {
                        sha256: object.sha.clone(),
                        bases: chain,
                    };
                    Some((
                        planned,
                        object::encode_delta(&object.raw, &base.sha256, raw)?,
                    ))
                }
                None => None,
            };
            let (planned, frame) = match delta {
                Some((planned, delta)) if !require_half => (planned, delta),
                delta => {
                    let full = object::encode_full(&object.raw)?;
                    match delta {
                        Some((planned, delta)) if delta.len() * 2 < full.len() => (planned, delta),
                        _ => (ObjRef::full(object.sha.clone()), full),
                    }
                }
            };
            encoded.push((object.sha.clone(), planned, frame));
        }
        drop(bases);
        let growth = encoded
            .iter()
            .map(|(_, _, frame)| round_block(frame.len() as u64))
            .sum::<u64>()
            + round_block(encoded.len() as u64 * 640 + 1024)
            + RECORD_GROWTH;
        let mut locked = self.lock_pending()?;
        if !self.secure(&locked, growth)? {
            self.exhausted = true;
            return Ok(());
        }
        // Existing complete objects are kept; the rest share one durability
        // barrier. Delta bases are pinned by the record.
        let mut resolved: HashMap<String, ObjRef> = HashMap::new();
        let mut frames = Vec::new();
        for (sha, planned, frame) in encoded {
            match locked.stored_chain(&namespace, &sha)? {
                Some(bases) => {
                    resolved.insert(sha.clone(), ObjRef { sha256: sha, bases });
                }
                None => {
                    self.published.push(planned.clone());
                    resolved.insert(sha.clone(), planned);
                    frames.push((sha, frame));
                }
            }
        }
        let written = locked.publish_batch(&namespace, &frames)?;
        drop(frames);
        locked
            .ledger
            .records
            .get_mut(self.id)
            .unwrap()
            .published_bytes += written;
        let list: Vec<&ObjRef> = resolved.values().collect();
        let raw = serde_json::to_vec(&list).map_err(io::Error::from)?;
        let list_sha = hash(&raw);
        self.put(&mut locked, &list_sha, &object::encode_full(&raw)?)?;
        let record = locked.ledger.records.get_mut(self.id).unwrap();
        record.snapshot.as_mut().unwrap().batches.push(list_sha);
        LockedVault::rebill(record)?;
        locked.persist()?;
        drop(locked);
        for object in batch {
            files[object.file].content = Content::Text {
                object: resolved.get(&object.sha).cloned(),
                sha256: object.sha,
            };
        }
        Ok(())
    }

    /// Scan one root against `baseline`, publishing new text content.
    #[allow(clippy::too_many_arguments)]
    fn scan_root(
        &mut self,
        directory: &File,
        rules: &mut Rules,
        load_rules: bool,
        baseline: Option<&Manifest>,
        deadline: Instant,
        seen: &mut usize,
        max_files: usize,
        require_half: bool,
    ) -> Result<Manifest, Stop> {
        let scanned_at_ns = now_ns();
        let racy = baseline.map_or(i64::MIN, |manifest| {
            manifest.scanned_at_ns.saturating_sub(RACY_NS)
        });
        let previous: HashMap<&str, &ManifestFile> = baseline
            .iter()
            .flat_map(|manifest| &manifest.files)
            .map(|file| (file.path.as_str(), file))
            .collect();
        let mut files: Vec<ManifestFile> = Vec::new();
        let mut batch: Vec<PendingObject> = Vec::new();
        let mut batch_raw = 0usize;
        scan::walk(
            directory,
            Walk {
                rules,
                load_rules,
                deadline,
                files: seen,
                max_files,
            },
            &mut |parent, name, path, stat| {
                let prev = previous.get(path.as_str()).copied();
                if let Some(prev) = prev
                    && prev.same_stat(&stat)
                    && prev.mtime_ns < racy
                    && prev.content.reusable()
                {
                    files.push(prev.clone());
                    return Ok(());
                }
                let stat_only =
                    |stat: &scan_fs::Stat| ManifestFile::new(path.clone(), stat, Content::StatOnly);
                if stat.size > MAX_TEXT_BYTES as u64 {
                    files.push(stat_only(&stat));
                    return Ok(());
                }
                let file = match scan_fs::open(parent, name, false) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    Err(_) => {
                        files.push(stat_only(&stat));
                        return Ok(());
                    }
                };
                let opened = scan_fs::stat(&file)?;
                if opened.file_id != stat.file_id || opened.links != 1 {
                    return Ok(());
                }
                let bytes = match scan::read_bounded(file) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) | Err(_) => {
                        files.push(stat_only(&opened));
                        return Ok(());
                    }
                };
                let sha = hash(&bytes);
                if !scan::is_text(&bytes) {
                    files.push(ManifestFile::new(
                        path,
                        &opened,
                        Content::Binary { sha256: sha },
                    ));
                    return Ok(());
                }
                let prev_object = prev.and_then(|prev| prev.content.object());
                if let Some(object) = prev_object.filter(|object| object.sha256 == sha) {
                    let content = Content::Text {
                        sha256: sha,
                        object: Some(object.clone()),
                    };
                    files.push(ManifestFile::new(path, &opened, content));
                    return Ok(());
                }
                files.push(ManifestFile::new(
                    path,
                    &opened,
                    Content::Text {
                        sha256: sha.clone(),
                        object: None,
                    },
                ));
                if self.exhausted {
                    return Ok(());
                }
                batch_raw += bytes.len();
                batch.push(PendingObject {
                    file: files.len() - 1,
                    sha,
                    raw: bytes,
                    base: prev_object.cloned(),
                });
                if batch_raw >= BATCH_RAW_BYTES || batch.len() >= BATCH_OBJECTS {
                    self.flush(&mut files, std::mem::take(&mut batch), require_half)?;
                    batch_raw = 0;
                }
                Ok(())
            },
        )?;
        self.flush(&mut files, batch, require_half)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Manifest {
            scanned_at_ns,
            files,
        })
    }
}

impl LockedVault {
    /// End an unfinished run without a change list.
    fn end_snapshot(&mut self, id: &str, outcome: ChangeState, now_ms: u64) -> io::Result<()> {
        let Some(record) = self.ledger.records.get_mut(id) else {
            return Ok(());
        };
        if record.change != ChangeState::SnapshotPending {
            return Ok(());
        }
        if outcome == ChangeState::Aborted {
            self.abort_record(id, now_ms);
        } else {
            record.change = ChangeState::OutcomeUnknown;
            record.revision += 1;
        }
        self.persist()
    }
}

fn validate_roots(roots: &[SnapshotRoot<'_>]) -> io::Result<()> {
    if roots.is_empty() || roots.len() > MAX_ROOTS {
        return Err(invalid("invalid snapshot root count"));
    }
    for (index, root) in roots.iter().enumerate() {
        identity(root.directory_identity)?;
        if roots[..index]
            .iter()
            .any(|other| other.directory_identity == root.directory_identity)
        {
            return Err(invalid("snapshot roots must be distinct"));
        }
    }
    Ok(())
}

impl Vault {
    /// Write the pending record and scan every root before the command is
    /// spawned. `reserve` receives the record and the operation's cumulative
    /// reservation bound and is called while the vault lock is held; a
    /// `StorageFull` error means no room (content is then not stored).
    ///
    /// A deleted conversation fails with `PermissionDenied`. On `Skipped`
    /// and on errors the caller clears the operation's reservation.
    pub fn begin_snapshot(
        &self,
        request: SnapshotRequest<'_>,
        mut reserve: impl FnMut(&Record, u64) -> io::Result<()>,
    ) -> io::Result<SnapshotBegin> {
        request.scope.validate()?;
        for value in [request.conversation, request.operation, request.generation] {
            identity(value)?;
        }
        validate_roots(&request.roots)?;
        let started = Instant::now();
        let scope = request.scope.clone();
        let id = record_id(&scope, request.conversation, request.operation)?;
        let identities: Vec<&str> = request
            .roots
            .iter()
            .map(|root| root.directory_identity)
            .collect();
        let mut baselines = Vec::with_capacity(request.roots.len());
        {
            let mut locked = self.lock()?;
            locked.require_execution_epoch(request.expected_epoch)?;
            if locked.is_deleted(&scope, request.conversation) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "conversation was deleted",
                ));
            }
            if locked.ledger.records.contains_key(&id) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recovery operation already exists; do not repeat the command",
                ));
            }
            if locked.ledger.records.len() >= MAX_RECORDS {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "file backup capacity exhausted",
                ));
            }
            let namespace = scope.namespace();
            let mut roots = Vec::with_capacity(identities.len());
            for identity in &identities {
                let pointer = locked.ledger.manifests.get(&manifest::manifest_key(
                    &scope,
                    request.conversation,
                    identity,
                ));
                let baseline = match pointer {
                    Some(pointer) => match locked.read_manifest(&namespace, &pointer.sha256) {
                        Ok(manifest) => Some((pointer.sha256.clone(), manifest)),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error),
                    },
                    None => None,
                };
                roots.push(SnapshotRootState {
                    directory_identity: (*identity).to_owned(),
                    baseline: baseline.as_ref().map(|(sha, _)| sha.clone()),
                    manifest: None,
                });
                baselines.push(baseline.map(|(_, manifest)| manifest));
            }
            let mut record = Record {
                id: id.clone(),
                scope: scope.clone(),
                conversation: request.conversation.into(),
                operation: request.operation.into(),
                generation: request.generation.into(),
                kind: RecordKind::CommandSnapshot,
                directory_identity: hash(&serde_json::to_vec(&identities)?),
                entries: Entries::PreRun { count: 0 },
                sequence: locked.ledger.sequence + 1,
                created_at_ms: request.now_ms,
                expires_at_ms: request
                    .now_ms
                    .saturating_add(u64::from(locked.ledger.policy.retention_days) * 86_400_000),
                change: ChangeState::SnapshotPending,
                material: MaterialState::Saved,
                revision: 1,
                published_bytes: 0,
                serialized_bytes: 0,
                cleanup_error: None,
                transaction: None,
                discard_requested: false,
                clock_generation: locked.ledger.clock_generation,
                storage_epoch: locked.ledger.execution_epoch,
                executor: Some(request.executor.id().to_owned()),
                snapshot: Some(SnapshotState {
                    roots,
                    batches: Vec::new(),
                    started_at_ms: request.now_ms,
                    finished_at_ms: None,
                }),
            };
            LockedVault::rebill(&mut record)?;
            locked.ledger.sequence += 1;
            locked.ledger.records.insert(id.clone(), record);
            locked.persist()?;
        }
        let mut run = Run {
            vault: self,
            scope: &scope,
            conversation: request.conversation,
            id: &id,
            reserve: &mut reserve,
            reserved: 0,
            exhausted: false,
            published: Vec::new(),
        };
        let deadline = started + request.limits.max_duration;
        let mut seen = 0;
        let mut runs = Vec::with_capacity(request.roots.len());
        let scanned = (|| -> Result<(), Stop> {
            for (root, baseline) in request.roots.iter().zip(&baselines) {
                let mut rules = Rules::default();
                let manifest = run.scan_root(
                    root.directory,
                    &mut rules,
                    true,
                    baseline.as_ref(),
                    deadline,
                    &mut seen,
                    request.limits.max_files,
                    true,
                )?;
                runs.push(RootRun {
                    directory: root.directory.try_clone()?,
                    identity: root.directory_identity.to_owned(),
                    rules,
                    manifest,
                });
            }
            run.publish_pre_run(&runs)
        })();
        match scanned {
            Ok(()) => Ok(SnapshotBegin::Taken(PendingSnapshot {
                reserved: run.reserved,
                published: run.published,
                scope,
                conversation: request.conversation.to_owned(),
                id,
                roots: runs,
                limits: request.limits,
                started_at_ms: request.now_ms,
            })),
            Err(Stop::Skip(reason)) => {
                self.lock()?
                    .end_snapshot(&id, ChangeState::Aborted, request.now_ms)?;
                Ok(SnapshotBegin::Skipped(reason))
            }
            Err(Stop::Discarded) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "conversation was deleted",
            )),
            Err(Stop::Io(error)) => {
                if let Ok(mut locked) = self.lock() {
                    let _ = locked.end_snapshot(&id, ChangeState::Aborted, request.now_ms);
                }
                Err(error)
            }
        }
    }

    /// Rescan after the command exited and record its changes. Returns the
    /// vault guard so that the caller settles quota under the same lock:
    /// clear the operation's reservation and report `usage_report()`.
    pub fn finish_snapshot(
        &self,
        pending: PendingSnapshot,
        now_ms: u64,
        mut reserve: impl FnMut(&Record, u64) -> io::Result<()>,
    ) -> io::Result<(LockedVault, SnapshotFinish)> {
        let PendingSnapshot {
            scope,
            conversation,
            id,
            mut roots,
            reserved,
            published,
            limits,
            ..
        } = pending;
        let mut run = Run {
            vault: self,
            scope: &scope,
            conversation: &conversation,
            id: &id,
            reserve: &mut reserve,
            reserved,
            exhausted: false,
            published,
        };
        let result = (|| -> Result<(LockedVault, SnapshotFinish), Stop> {
            let deadline = Instant::now() + limits.max_duration;
            let mut seen = 0;
            let mut after = Vec::with_capacity(roots.len());
            for root in &mut roots {
                after.push(run.scan_root(
                    &root.directory,
                    &mut root.rules,
                    false,
                    Some(&root.manifest),
                    deadline,
                    &mut seen,
                    limits.max_files,
                    false,
                )?);
            }
            run.publish_post_run(&roots, after, now_ms)
        })();
        match result {
            Ok(finished) => Ok(finished),
            Err(Stop::Skip(reason)) => {
                let mut locked = self.lock()?;
                locked.end_snapshot(&id, ChangeState::Aborted, now_ms)?;
                Ok((locked, SnapshotFinish::Skipped(reason)))
            }
            Err(Stop::Discarded) => Ok((self.lock()?, SnapshotFinish::Discarded)),
            Err(Stop::Io(error)) => {
                if let Ok(mut locked) = self.lock() {
                    let _ = locked.end_snapshot(&id, ChangeState::OutcomeUnknown, now_ms);
                }
                Err(error)
            }
        }
    }

    /// End a run that will not be finished: `OutcomeUnknown` once the
    /// command may have started (its pre-run material stays exportable),
    /// otherwise `Aborted`. The caller settles quota under the returned guard.
    pub fn abandon_snapshot(
        &self,
        pending: PendingSnapshot,
        command_started: bool,
        now_ms: u64,
    ) -> io::Result<LockedVault> {
        let mut locked = self.lock()?;
        let outcome = if command_started {
            ChangeState::OutcomeUnknown
        } else {
            ChangeState::Aborted
        };
        locked.end_snapshot(&pending.id, outcome, now_ms)?;
        Ok(locked)
    }
}

impl Run<'_> {
    /// Publish the pre-run manifests; they replace the baselines and batches
    /// as the record's roots.
    fn publish_pre_run(&mut self, roots: &[RootRun]) -> Result<(), Stop> {
        let frames = roots
            .iter()
            .map(|root| manifest::encode(&root.manifest))
            .collect::<Result<Vec<_>, Stop>>()?;
        let growth = frames
            .iter()
            .map(|(_, frame)| round_block(frame.len() as u64))
            .sum::<u64>()
            + RECORD_GROWTH;
        let mut locked = self.lock_pending()?;
        if !self.secure(&locked, growth)? {
            return Err(Stop::Skip(SkipReason::StorageFull));
        }
        let mut shas = Vec::with_capacity(frames.len());
        for (sha, frame) in frames {
            self.put(&mut locked, &sha, &frame)?;
            shas.push(sha);
        }
        let count = roots
            .iter()
            .flat_map(|root| root.manifest.objects())
            .count() as u64;
        let record = locked.ledger.records.get_mut(self.id).unwrap();
        let snapshot = record.snapshot.as_mut().unwrap();
        for (root, sha) in snapshot.roots.iter_mut().zip(shas) {
            root.baseline = None;
            root.manifest = Some(sha);
        }
        snapshot.batches.clear();
        record.entries = Entries::PreRun { count };
        LockedVault::rebill(record)?;
        locked.persist()?;
        Ok(())
    }

    /// Publish the entry table and post-run manifests and complete the record.
    fn publish_post_run(
        &mut self,
        roots: &[RootRun],
        after: Vec<Manifest>,
        now_ms: u64,
    ) -> Result<(LockedVault, SnapshotFinish), Stop> {
        let mut entries = Vec::new();
        for (index, (root, manifest)) in roots.iter().zip(&after).enumerate() {
            entries.extend(changes::changes(
                index as u32,
                &root.identity,
                &root.manifest.files,
                &manifest.files,
            ));
        }
        if entries.len() > MAX_RECORD_ENTRIES {
            return Err(Stop::Skip(SkipReason::TooManyFiles));
        }
        let (table, table_frame) = match LockedVault::table_storage(EntryTable {
            entries,
            published: std::mem::take(&mut self.published),
        }) {
            Ok((table, Some(frame))) => (table, frame),
            Ok((_, None)) => unreachable!("a table storage always has a frame"),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                return Err(Stop::Skip(SkipReason::ManifestTooLarge));
            }
            Err(error) => return Err(error.into()),
        };
        let frames = after
            .iter()
            .map(manifest::encode)
            .collect::<Result<Vec<_>, Stop>>()?;
        let growth = frames
            .iter()
            .chain(std::iter::once(&table_frame))
            .map(|(_, frame)| round_block(frame.len() as u64))
            .sum::<u64>()
            + (RECORD_GROWTH + 256) * roots.len() as u64;
        // The tombstone check of `lock_pending` precedes any publication.
        let mut locked = self.lock_pending()?;
        if !self.secure(&locked, growth)? {
            return Err(Stop::Skip(SkipReason::StorageFull));
        }
        let (table_sha, table_frame) = table_frame;
        self.put(&mut locked, &table_sha, &table_frame)?;
        let retention = u64::from(locked.ledger.policy.retention_days) * 86_400_000;
        for ((root, manifest), (sha, frame)) in roots.iter().zip(&after).zip(frames) {
            self.put(&mut locked, &sha, &frame)?;
            let mut pointer = ManifestPointer {
                scope: self.scope.clone(),
                conversation: self.conversation.to_owned(),
                directory_identity: root.identity.clone(),
                sha256: sha,
                files: manifest.files.len() as u64,
                updated_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(retention),
                serialized_bytes: 0,
            };
            pointer.rebill()?;
            locked.ledger.manifests.insert(pointer.key(), pointer);
        }
        let record = locked.ledger.records.get_mut(self.id).unwrap();
        record.entries = table;
        record.change = ChangeState::Completed;
        record.revision += 1;
        let snapshot = record.snapshot.as_mut().unwrap();
        for root in &mut snapshot.roots {
            root.baseline = None;
            root.manifest = None;
        }
        snapshot.batches.clear();
        snapshot.finished_at_ms = Some(now_ms);
        LockedVault::rebill(record)?;
        locked.persist()?;
        let record = locked.ledger.records[self.id].clone();
        let changes = locked
            .entry_table(&record)?
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| FileChange::from_entry(index as u64, entry))
            .collect();
        Ok((locked, SnapshotFinish::Taken { record, changes }))
    }
}
