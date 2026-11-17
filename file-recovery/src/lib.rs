//! Device-private storage. All methods are blocking and must run off async executors.
//! Callers supply identities only after validating their central authorization lane.
//!
//! Backups are content-addressed objects under `objects/<namespace>/<sha256>`,
//! published only by renaming a fully written temporary file, and indexed by
//! a JSON ledger (`index.json`) of records. Objects are shared between records
//! and conversations of the same scope namespace and are reclaimed by
//! mark-sweep; device quota is settled with absolute namespace usage.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io, path::Path};

#[cfg(windows)]
#[path = "storage_windows.rs"]
mod storage;
#[cfg(not(windows))]
#[path = "storage_path.rs"]
mod storage;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

mod backup;
pub use backup::{BackupRequest, EncodedBackup, EntryInput};
mod clock_guard;
mod epoch_cleanup;
pub use epoch_cleanup::{EpochCleanupState, EpochCoordinator};
mod export;
pub use export::{
    EXPORT_CHUNK_BYTES, EntriesPage, EntryMaterial, EntrySummary, ExportCache, ExportChunk,
};
mod ledger;
use ledger::Ledger;
mod lifecycle;
mod object;
pub mod quota;
mod store;
mod sweep;
pub use sweep::SweepReport;

mod transaction;
pub use transaction::{Transaction, TransactionPlan};
#[cfg(windows)]
pub use transaction::{WindowsCommitError, WindowsCommitRequest};
mod transaction_identity;
pub use transaction_identity::{InodeIdentity, TransactionIdentity, WindowsIdentity};
#[cfg(test)]
mod transaction_identity_tests;

/// Largest text version accepted for backup, restore and export.
pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_LEDGER_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 10_000;
/// Entries of one record; text tools always use exactly one.
const MAX_RECORD_ENTRIES: usize = 20_000;
/// Delta objects are allowed only while the new chain depth stays below this.
const MAX_CHAIN_DEPTH: usize = 8;
/// Object usage is billed in filesystem-sized blocks.
const BLOCK_BYTES: u64 = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub authority: String,
    pub device: String,
    pub os_user: String,
    pub owner: String,
}
impl Scope {
    fn validate(&self) -> io::Result<()> {
        for value in [&self.authority, &self.device, &self.os_user, &self.owner] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(invalid("invalid trusted recovery scope"));
            }
        }
        Ok(())
    }
    /// Conversation tombstone key; the OS user prefix allows per-user checks.
    fn key(&self, conversation: &str) -> String {
        format!(
            "{}{}",
            hash(self.os_user.as_bytes()),
            hash(&serde_json::to_vec(&(self, conversation)).expect("scope serialization"))
        )
    }
    /// Object namespace and quota key shared by all conversations of this scope.
    pub fn namespace(&self) -> String {
        format!(
            "{}{}",
            hash(self.os_user.as_bytes()),
            hash(&serde_json::to_vec(self).expect("scope serialization"))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub retention_days: u32,
    pub max_bytes: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            retention_days: 7,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> io::Result<()> {
        if !(1..=3650).contains(&self.retention_days)
            || !(1024 * 1024..=1024 * 1024 * 1024 * 10).contains(&self.max_bytes)
        {
            return Err(invalid(
                "retention_days must be 1..3650 and max_bytes must be 1 MiB..10 GiB",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Preparing,
    BackupReady,
    CommitIntent,
    OutcomeUnknown,
    Succeeded,
    Aborted,
}
/// Purged records keep only their replay fence; their objects are no longer roots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialState {
    Saved,
    Purged,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    TextEdit,
    TextDelete,
    TextRestore,
    CommandSnapshot,
}
impl RecordKind {
    fn is_text(self) -> bool {
        !matches!(self, Self::CommandSnapshot)
    }
}

/// A published object and its complete delta base chain, nearest base first.
/// The writer records the chain so sweep marking and depth limits never read
/// object headers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjRef {
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
}
impl ObjRef {
    pub fn base(&self) -> Option<&str> {
        self.bases.first().map(String::as_str)
    }
    fn full(sha256: String) -> Self {
        Self {
            sha256,
            bases: Vec::new(),
        }
    }
    fn validate(&self) -> io::Result<()> {
        if !valid_id(&self.sha256)
            || self.bases.len() >= MAX_CHAIN_DEPTH
            || self.bases.iter().any(|base| !valid_id(base))
        {
            return Err(invalid("recovery object reference is corrupt"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// File name for text records; a root-relative path for command snapshots.
    pub rel_path: String,
    pub before: Option<ObjRef>,
    pub after: Option<ObjRef>,
    pub metadata: Option<String>,
    /// `hash(directory identity, rel_path)`, used only to choose delta bases.
    pub lineage: String,
}
impl Entry {
    fn validate(&self) -> io::Result<()> {
        validate_rel_path(&self.rel_path, false)?;
        for object in self.before.iter().chain(&self.after) {
            object.validate()?;
        }
        if self.metadata.as_deref().is_some_and(|sha| !valid_id(sha)) || !valid_id(&self.lineage) {
            return Err(invalid("recovery entry is corrupt"));
        }
        Ok(())
    }
}

/// Text records keep their single entry inline; larger records store the
/// entry list as an object and keep only its name and length in the ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entries {
    Inline { entry: Entry },
    Table { sha256: String, count: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub scope: Scope,
    pub conversation: String,
    pub operation: String,
    pub generation: String,
    pub kind: RecordKind,
    /// Filesystem identity of the original directory, opaque to this crate.
    /// The worker that resolved the directory compares it on restore.
    pub directory_identity: String,
    pub entries: Entries,
    /// Insertion order within this vault; selects the newest lineage object.
    pub sequence: u64,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub change: ChangeState,
    pub material: MaterialState,
    /// Bumped by state and retention changes; identifies export packages.
    pub revision: u64,
    /// Block-rounded bytes of objects this record published itself. Shared
    /// objects may be held by other records, so discarding may free less.
    pub published_bytes: u64,
    /// Ledger bytes billed to the record's namespace.
    pub serialized_bytes: u64,
    pub cleanup_error: Option<String>,
    pub transaction: Option<Transaction>,
    pub discard_requested: bool,
    pub clock_generation: u64,
    pub storage_epoch: u64,
}
impl Record {
    pub fn entry_count(&self) -> u64 {
        match &self.entries {
            Entries::Inline { .. } => 1,
            Entries::Table { count, .. } => *count,
        }
    }
    pub fn inline_entry(&self) -> Option<&Entry> {
        match &self.entries {
            Entries::Inline { entry } => Some(entry),
            Entries::Table { .. } => None,
        }
    }
    /// The original file name of a single-entry text record.
    pub fn file_name(&self) -> Option<&str> {
        self.inline_entry().map(|entry| entry.rel_path.as_str())
    }
    pub fn namespace(&self) -> String {
        self.scope.namespace()
    }
    fn in_flight(&self) -> bool {
        matches!(
            self.change,
            ChangeState::Preparing | ChangeState::BackupReady | ChangeState::CommitIntent
        )
    }
}

/// Stable export failures carry no paths or material contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportError {
    Unavailable,
    Expired,
    Cleaning,
    Cleaned,
    Preparing,
}
impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "recovery export {:?}", self)
    }
}
impl std::error::Error for ExportError {}
fn export_error(reason: ExportError) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, reason)
}

pub struct Vault {
    storage: storage::Root,
}
/// The OS lock fences other workers, exports and cleanup until this guard drops.
pub struct LockedVault {
    storage: storage::Locked,
    ledger: Ledger,
    /// Size of the last persisted `index.json`.
    ledger_bytes: u64,
    /// In-memory changes that are written by the next ledger persist. Every
    /// such change leaves the persisted ledger in a state with identical
    /// crash semantics (see `backup` and `create_windows_transaction_directory`).
    dirty: bool,
    #[cfg(test)]
    persist_count: u64,
}

#[derive(Debug)]
pub struct CleanupClockError;
impl std::fmt::Display for CleanupClockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "file recovery cleanup paused: system clock changed or could not be verified",
        )
    }
}
impl std::error::Error for CleanupClockError {}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
/// Report the actual decoding failure: a changed field of the current format
/// is corruption and is never silently recreated.
fn undecodable(ledger: &str, error: &serde_json::Error) -> io::Error {
    invalid(&format!(
        "{ledger} could not be decoded and was not recreated: {error}"
    ))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_namespace(namespace: &str) -> bool {
    namespace_user(namespace).is_some()
}
fn namespace_user(namespace: &str) -> Option<&str> {
    let user = namespace.get(..64)?;
    let scope = namespace.get(64..)?;
    (valid_id(user) && valid_id(scope)).then_some(user)
}
fn identity(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(invalid("invalid recovery operation identity"))
    } else {
        Ok(())
    }
}
fn round_block(bytes: u64) -> u64 {
    bytes.div_ceil(BLOCK_BYTES).saturating_mul(BLOCK_BYTES)
}
/// Record ids double as quota operation ids.
fn record_id(scope: &Scope, conversation: &str, operation: &str) -> io::Result<String> {
    Ok(hash(&serde_json::to_vec(&(
        scope,
        conversation,
        operation,
    ))?))
}
fn lineage(directory_identity: &str, rel_path: &str) -> String {
    hash(&serde_json::to_vec(&(directory_identity, rel_path)).expect("lineage serialization"))
}
/// `single` requires one file name; otherwise a normalized relative path.
fn validate_rel_path(path: &str, single: bool) -> io::Result<()> {
    let bad_component = |part: &str| part.is_empty() || part == "." || part == "..";
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(|c| c.is_control() || c == '\\')
        || path.starts_with('/')
        || (single && path.contains('/'))
        || path.split('/').any(bad_component)
    {
        return Err(invalid("invalid recovery file path"));
    }
    Ok(())
}

impl Vault {
    /// `data_root` is resolved by the host for the verified worker OS user.
    pub fn open(data_root: &Path) -> io::Result<Self> {
        Ok(Self {
            storage: storage::Root::open(data_root)?,
        })
    }
    pub fn lock(&self) -> io::Result<LockedVault> {
        LockedVault::load(self.storage.lock()?)
    }
    /// Maintenance never waits behind an active file mutation or download.
    pub fn try_lock(&self) -> io::Result<Option<LockedVault>> {
        self.storage.try_lock()?.map(LockedVault::load).transpose()
    }
}

impl LockedVault {
    pub fn policy(&self) -> &Policy {
        &self.ledger.policy
    }
    /// Retention changes move every expiry and therefore every package revision.
    pub fn set_policy(&mut self, policy: Policy) -> io::Result<()> {
        policy.validate()?;
        for record in self.ledger.records.values_mut() {
            record.expires_at_ms = record
                .created_at_ms
                .saturating_add(u64::from(policy.retention_days) * 86_400_000);
            record.revision += 1;
        }
        self.ledger.policy = policy;
        self.persist()
    }
    pub fn vault_instance_id(&self) -> &str {
        &self.ledger.vault_instance_id
    }
    /// Revision of the last persisted ledger.
    pub fn revision(&self) -> u64 {
        self.ledger.revision
    }
    /// Write in-memory changes that would otherwise wait for the next update.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.dirty { self.persist() } else { Ok(()) }
    }
    pub fn record(&self, scope: &Scope, id: &str) -> Option<&Record> {
        self.ledger.records.get(id).filter(|r| &r.scope == scope)
    }
    pub fn pending_cleanup_count(&self) -> u64 {
        u64::from(self.cleanup_clock_paused())
            + self
                .ledger
                .records
                .values()
                .filter(|r| r.cleanup_error.is_some() || r.transaction.is_some())
                .count() as u64
            + self
                .ledger
                .deleted_conversations
                .values()
                .filter(|deleted| deleted.quota_pending)
                .count() as u64
    }
    pub fn cleanup_clock_paused(&self) -> bool {
        self.ledger.cleanup_clock_paused || self.ledger.clock_guard.blocked
    }
    pub fn unknown_outcome_count(&self, scope: Option<&Scope>) -> u64 {
        self.ledger
            .records
            .values()
            .filter(|record| {
                scope.is_none_or(|scope| &record.scope == scope)
                    && matches!(
                        record.change,
                        ChangeState::CommitIntent | ChangeState::OutcomeUnknown
                    )
                    && record.material != MaterialState::Purged
            })
            .count() as u64
    }
    pub fn oldest_pending_created_at(&self, scope: Option<&Scope>) -> Option<u64> {
        self.oldest_pending_record(scope, None)
            .map(|record| record.created_at_ms)
    }
    pub fn oldest_pending_record(
        &self,
        scope: Option<&Scope>,
        conversation: Option<&str>,
    ) -> Option<Record> {
        self.ledger
            .records
            .values()
            .filter(|record| {
                scope.is_none_or(|scope| &record.scope == scope)
                    && conversation.is_none_or(|conversation| record.conversation == conversation)
                    && (record.cleanup_error.is_some()
                        || record.transaction.is_some()
                        || (record.change == ChangeState::OutcomeUnknown
                            && record.material != MaterialState::Purged))
            })
            .min_by_key(|record| (record.created_at_ms, &record.id))
            .cloned()
    }
    pub fn list(&self, scope: &Scope, conversation: Option<&str>) -> Vec<Record> {
        self.ledger
            .records
            .values()
            .filter(|r| &r.scope == scope && conversation.is_none_or(|c| c == r.conversation))
            .cloned()
            .collect()
    }
    /// Original-OS-user recovery only. Never expose this through a remote route:
    /// remote callers must use `list` and an authenticated, frozen Scope.
    pub fn local_records(&self, after: Option<&str>) -> io::Result<Vec<Record>> {
        if after.is_some_and(|id| !valid_id(id)) {
            return Err(invalid("invalid recovery page cursor"));
        }
        Ok(self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.material != MaterialState::Purged && after.is_none_or(|id| r.id.as_str() > id)
            })
            .take(101)
            .cloned()
            .collect())
    }
    /// Local owner authentication is enforced by the loopback controller.
    pub fn discard_local(&mut self, conversation: &str, id: &str, now_ms: u64) -> io::Result<()> {
        let scope = self
            .ledger
            .records
            .get(id)
            .ok_or_else(|| invalid("recovery material unavailable"))?
            .scope
            .clone();
        self.discard(&scope, conversation, id, now_ms)
    }
    /// The conversation's records are gone, its unshared objects were swept
    /// and the resulting absolute usage was acknowledged by the device quota.
    pub fn cleanup_complete(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger
            .deleted_conversations
            .get(&scope.key(conversation))
            .is_some_and(|deleted| !deleted.quota_pending)
            && !self.conversation_retains_material(scope, conversation)
    }
    fn conversation_retains_material(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger.records.values().any(|r| {
            &r.scope == scope
                && r.conversation == conversation
                && (r.material != MaterialState::Purged || r.transaction.is_some())
        })
    }
    pub fn is_deleted(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger
            .deleted_conversations
            .contains_key(&scope.key(conversation))
    }
    /// A current connection cannot claim an old namespace's material was purged.
    pub fn has_foreign_conversation(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger.records.values().any(|r| {
            r.conversation == conversation
                && r.material != MaterialState::Purged
                && &r.scope != scope
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_export_tests;
#[cfg(test)]
mod tests;
