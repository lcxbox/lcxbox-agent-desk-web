//! The JSON ledger: records, conversation tombstones and per-namespace object
//! counts. A ledger of another format version is wiped and rebuilt.
use super::*;
use std::collections::{BTreeMap, hash_map::RandomState};
use std::hash::{BuildHasher, Hasher};

pub(crate) const FORMAT_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamespaceUsage {
    pub objects: u64,
    /// Block-rounded bytes of the namespace's object files.
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeletedConversation {
    pub deleted_at_ms: u64,
    /// Cleared once the device quota acknowledged usage measured after the
    /// conversation's records were removed and its objects swept.
    pub quota_pending: bool,
    pub storage_epoch: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    pub format_version: u32,
    /// Random per ledger creation; quota usage of a different instance
    /// replaces older reports regardless of revision.
    pub vault_instance_id: String,
    /// Incremented by every persist.
    pub revision: u64,
    pub sequence: u64,
    pub policy: Policy,
    pub records: BTreeMap<String, Record>,
    pub deleted_conversations: BTreeMap<String, DeletedConversation>,
    /// The latest command snapshot manifest of each (conversation, root),
    /// owned by its conversation.
    pub manifests: BTreeMap<String, snapshot::ManifestPointer>,
    /// Absolute counts, recomputed from directory listings by every sweep.
    pub namespaces: BTreeMap<String, NamespaceUsage>,
    pub last_cleanup_at_ms: u64,
    pub last_sweep_at_ms: u64,
    pub cleanup_clock_paused: bool,
    pub clock_guard: clock_guard::Guard,
    pub clock_generation: u64,
    pub execution_epoch: u64,
    pub epoch_cleanup: Option<EpochCleanupState>,
}

/// A unique identifier without a random number dependency: process-seeded
/// hasher keys, time and process identity, hashed together.
pub(crate) fn random_id() -> String {
    let mut seed = Vec::new();
    for _ in 0..4 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(seed.len() as u64);
        seed.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    seed.extend_from_slice(&now.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    hash(&seed)
}

impl Ledger {
    fn new() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            vault_instance_id: random_id(),
            revision: 0,
            sequence: 0,
            policy: Policy::default(),
            records: BTreeMap::new(),
            deleted_conversations: BTreeMap::new(),
            manifests: BTreeMap::new(),
            namespaces: BTreeMap::new(),
            last_cleanup_at_ms: 0,
            last_sweep_at_ms: 0,
            cleanup_clock_paused: false,
            clock_guard: clock_guard::Guard::default(),
            clock_generation: 0,
            execution_epoch: 0,
            epoch_cleanup: None,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if !valid_id(&self.vault_instance_id) {
            return Err(invalid("recovery vault identity is corrupt"));
        }
        self.policy.validate()?;
        self.clock_guard.validate()?;
        if let Some(state) = self.epoch_cleanup
            && (state.previous.checked_add(1) != Some(state.next)
                || self.execution_epoch
                    != if state.pruned {
                        state.next
                    } else {
                        state.previous
                    })
        {
            return Err(invalid("recovery epoch cleanup state is corrupt"));
        }
        if self.records.len() > MAX_RECORDS {
            return Err(invalid("recovery index exceeds its record bound"));
        }
        for (namespace, deleted) in &self.deleted_conversations {
            if !valid_namespace(namespace)
                || deleted.deleted_at_ms == 0
                || deleted.storage_epoch > self.execution_epoch
            {
                return Err(invalid("recovery conversation index is corrupt"));
            }
        }
        if self
            .namespaces
            .keys()
            .any(|namespace| !valid_namespace(namespace))
        {
            return Err(invalid("recovery namespace index is corrupt"));
        }
        for (key, manifest) in &self.manifests {
            if key != &manifest.key() {
                return Err(invalid("recovery manifest index is corrupt"));
            }
            manifest.validate()?;
        }
        for (id, record) in &self.records {
            if record.clock_generation > self.clock_generation
                || record.storage_epoch > self.execution_epoch
                || record.sequence > self.sequence
                || id != &record.id
                || !valid_id(id)
            {
                return Err(invalid("recovery record identity is corrupt"));
            }
            record.scope.validate()?;
            identity(&record.conversation)?;
            identity(&record.operation)?;
            identity(&record.generation)?;
            identity(&record.directory_identity)?;
            match &record.entries {
                Entries::Inline { entry } => entry.validate()?,
                Entries::Table { sha256, count } => {
                    if !valid_id(sha256) || *count as usize > MAX_RECORD_ENTRIES {
                        return Err(invalid("recovery entry table is corrupt"));
                    }
                }
                Entries::PreRun { count } => {
                    if *count as usize > MAX_RECORD_ENTRIES {
                        return Err(invalid("recovery entry table is corrupt"));
                    }
                }
            }
            if let Some(snapshot) = &record.snapshot {
                snapshot.validate()?;
            }
            if record.executor.as_deref().is_some_and(|id| !valid_id(id))
                || (record.kind == RecordKind::CommandSnapshot)
                    != (record.snapshot.is_some() && record.executor.is_some())
                || (record.kind == RecordKind::CommandRestore && record.snapshot.is_none())
            {
                return Err(invalid("recovery snapshot record is corrupt"));
            }
            if let Some(transaction) = &record.transaction {
                transaction.validate(id)?;
            }
        }
        Ok(())
    }
}

impl LockedVault {
    pub(crate) fn load(storage: storage::Locked) -> io::Result<Self> {
        let (ledger, ledger_bytes, reset) = match storage.read("index.json", MAX_LEDGER_BYTES) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|error| undecodable("recovery index", &error))?;
                if value
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(FORMAT_VERSION))
                {
                    let ledger: Ledger = serde_json::from_value(value)
                        .map_err(|error| undecodable("recovery index", &error))?;
                    (ledger, bytes.len() as u64, false)
                } else {
                    // The store is unreleased: another format is wiped, not migrated.
                    storage.remove_legacy_material()?;
                    (Ledger::new(), 0, true)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                storage.validate_missing_index()?;
                (Ledger::new(), 0, true)
            }
            Err(e) => return Err(e),
        };
        ledger.validate()?;
        let mut locked = LockedVault {
            storage,
            ledger,
            ledger_bytes,
            dirty: false,
            #[cfg(test)]
            persist_count: 0,
        };
        if reset {
            // Persist the new instance identity before any usage report names it,
            // then remove objects that no record of this ledger can reference.
            locked.persist()?;
            locked.sweep(0)?;
        }
        Ok(locked)
    }

    pub(crate) fn persist(&mut self) -> io::Result<()> {
        self.ledger.revision += 1;
        let bytes = serde_json::to_vec(&self.ledger)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            self.ledger.revision -= 1;
            return Err(invalid("recovery index capacity exhausted"));
        }
        self.storage.write_index(&bytes)?;
        self.ledger_bytes = bytes.len() as u64;
        self.dirty = false;
        #[cfg(test)]
        {
            self.persist_count += 1;
        }
        Ok(())
    }

    /// Recompute the ledger bytes billed to a record's namespace. Includes the
    /// map key and separators; a small margin covers the field's own digits.
    pub(crate) fn rebill(record: &mut Record) -> io::Result<()> {
        record.serialized_bytes = 0;
        record.serialized_bytes =
            serde_json::to_vec(&*record)?.len() as u64 + record.id.len() as u64 + 24;
        Ok(())
    }

    /// Absolute usage of this OS user's vault at the persisted revision. Every
    /// mutating call persists before returning except `backup`, whose
    /// BackupReady state waits for the next write; reports are sent after that
    /// write (or after `flush`) so a revision always names the reported state.
    pub fn usage_report(&self) -> quota::UsageReport {
        let mut namespaces: BTreeMap<String, u64> = self
            .ledger
            .namespaces
            .iter()
            .map(|(namespace, usage)| (namespace.clone(), usage.bytes))
            .collect();
        let mut records = 0u64;
        for record in self.ledger.records.values() {
            *namespaces.entry(record.namespace()).or_default() += record.serialized_bytes;
            records = records.saturating_add(record.serialized_bytes);
        }
        for manifest in self.ledger.manifests.values() {
            *namespaces.entry(manifest.scope.namespace()).or_default() += manifest.serialized_bytes;
            records = records.saturating_add(manifest.serialized_bytes);
        }
        namespaces.retain(|_, bytes| *bytes > 0);
        quota::UsageReport {
            vault_instance_id: self.ledger.vault_instance_id.clone(),
            revision: self.ledger.revision,
            ledger_bytes: self
                .ledger_bytes
                .saturating_sub(records)
                // An unreadable inventory fails closed rather than undercounting.
                .saturating_add(self.storage.extra_used_bytes().unwrap_or(u64::MAX)),
            namespaces,
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.usage_report().total()
    }

    /// Clear tombstones whose conversation was fully purged by a ledger state
    /// that the device quota has now accepted. `report` must have been produced
    /// by this lock guard and accepted by the quota.
    pub fn acknowledge_settlement(&mut self, report: &quota::UsageReport) -> io::Result<()> {
        if report.vault_instance_id != self.ledger.vault_instance_id
            || report.revision != self.ledger.revision
        {
            return Ok(());
        }
        let settled: Vec<String> = self
            .ledger
            .deleted_conversations
            .iter()
            .filter(|(key, deleted)| {
                deleted.quota_pending
                    && !self.ledger.records.values().any(|record| {
                        record.scope.key(&record.conversation) == **key
                            && (record.material != MaterialState::Purged
                                || record.transaction.is_some())
                    })
                    && !self
                        .ledger
                        .manifests
                        .values()
                        .any(|manifest| manifest.scope.key(&manifest.conversation) == **key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        if settled.is_empty() {
            return Ok(());
        }
        for key in settled {
            self.ledger
                .deleted_conversations
                .get_mut(&key)
                .unwrap()
                .quota_pending = false;
        }
        self.persist()
    }
}
