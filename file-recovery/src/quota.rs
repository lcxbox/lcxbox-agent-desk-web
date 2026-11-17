//! Daemon-owned device quota. Workers never open this directory directly.
//!
//! Usage is keyed by scope namespace and settled with absolute values that
//! each OS user's vault reports with its `(vault_instance_id, revision)`.
//! Operations hold idempotent in-flight reservations until a settlement or
//! reconciliation clears them; elapsed time never proves that a backup was
//! removed.
//!
//! Ordering: workers send reservation, settlement and reconciliation requests
//! while holding their vault lock and wait for the reply before releasing it.
//! A reconciliation therefore never interleaves with another worker's
//! reservation of the same vault.
use super::{hash, identity, invalid, namespace_user, storage, valid_id};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::Path};

const FORMAT_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 10_000;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;

/// Reservation identity. The daemon derives it from the authenticated OS user
/// and the worker-supplied scope fields; `operation` equals the record id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaKey {
    pub namespace: String,
    pub os_user: String,
    pub operation: String,
    pub epoch: u64,
}
impl QuotaKey {
    pub fn new(
        scope: &super::Scope,
        conversation: &str,
        operation: &str,
        epoch: u64,
    ) -> io::Result<Self> {
        scope.validate()?;
        identity(conversation)?;
        identity(operation)?;
        Ok(Self {
            namespace: scope.namespace(),
            os_user: hash(scope.os_user.as_bytes()),
            operation: super::record_id(scope, conversation, operation)?,
            epoch,
        })
    }
    /// The key of a vault record, for settling its reservation.
    pub fn for_record(record: &super::Record) -> io::Result<Self> {
        Self::new(
            &record.scope,
            &record.conversation,
            &record.operation,
            record.storage_epoch,
        )
    }
    fn validate(&self) -> io::Result<()> {
        if namespace_user(&self.namespace) == Some(self.os_user.as_str())
            && valid_id(&self.operation)
        {
            Ok(())
        } else {
            Err(invalid("invalid device quota identity"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRevision {
    pub vault_instance_id: String,
    pub revision: u64,
}
impl VaultRevision {
    /// Usage of another vault instance always replaces; otherwise only newer.
    fn supersedes(&self, current: Option<&VaultRevision>) -> bool {
        current.is_none_or(|current| {
            current.vault_instance_id != self.vault_instance_id || self.revision > current.revision
        })
    }
}

/// Absolute usage of one OS user's vault: shared ledger bytes plus each
/// namespace's block-rounded object bytes and record bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub vault_instance_id: String,
    pub revision: u64,
    pub ledger_bytes: u64,
    pub namespaces: BTreeMap<String, u64>,
}
impl UsageReport {
    pub fn total(&self) -> u64 {
        self.namespaces
            .values()
            .fold(self.ledger_bytes, |total, bytes| {
                total.saturating_add(*bytes)
            })
    }
    pub fn vault(&self) -> VaultRevision {
        VaultRevision {
            vault_instance_id: self.vault_instance_id.clone(),
            revision: self.revision,
        }
    }
    fn validate(&self, user: &str) -> io::Result<()> {
        if !valid_id(&self.vault_instance_id)
            || self.namespaces.len() > MAX_ENTRIES
            || self
                .namespaces
                .keys()
                .any(|namespace| namespace_user(namespace) != Some(user))
        {
            return Err(invalid("invalid device quota usage report"));
        }
        Ok(())
    }
}

/// Replaces the OS user's absolute usage and clears every reservation not
/// listed in `live_operations` (record ids). Sent after restart recovery and
/// a sweep, and as part of epoch cleanup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileRequest {
    pub execution_epoch: u64,
    pub usage: UsageReport,
    pub live_operations: Vec<String>,
}

/// Completes an epoch cleanup once a reconciliation at or after the pruned
/// ledger revision has been applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishEpochRequest {
    pub epoch: u64,
    pub vault: VaultRevision,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochStatus {
    pub epoch: u64,
    pub cleanup_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    namespace: String,
    epoch: u64,
    bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettledUsage {
    vault: VaultRevision,
    ledger_bytes: u64,
    namespaces: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserQuota {
    epoch: u64,
    cleanup_pending: bool,
    usage: Option<SettledUsage>,
    /// The newest reconciliation applied for this user.
    reconciled: Option<VaultRevision>,
    reservations: BTreeMap<String, Reservation>,
}
impl UserQuota {
    fn used(&self) -> u64 {
        let settled = self.usage.as_ref().map_or(0, |usage| {
            usage
                .namespaces
                .values()
                .fold(usage.ledger_bytes, |total, bytes| {
                    total.saturating_add(*bytes)
                })
        });
        self.reservations
            .values()
            .fold(settled, |total, reservation| {
                total.saturating_add(reservation.bytes)
            })
    }
    fn status(&self) -> EpochStatus {
        EpochStatus {
            epoch: self.epoch,
            cleanup_pending: self.cleanup_pending,
        }
    }
    fn apply_usage(&mut self, usage: &UsageReport) -> bool {
        let vault = usage.vault();
        if !vault.supersedes(self.usage.as_ref().map(|usage| &usage.vault)) {
            return false;
        }
        self.usage = Some(SettledUsage {
            vault,
            ledger_bytes: usage.ledger_bytes,
            namespaces: usage.namespaces.clone(),
        });
        true
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    max_bytes: u64,
    retention_days: u32,
    users: BTreeMap<String, UserQuota>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            max_bytes: super::Policy::default().max_bytes,
            retention_days: super::Policy::default().retention_days,
            users: BTreeMap::new(),
        }
    }
}

pub struct DeviceQuota {
    storage: storage::Root,
}
pub struct LockedDeviceQuota {
    storage: storage::Locked,
    ledger: Ledger,
    failed: bool,
}
impl DeviceQuota {
    /// Root belongs to the daemon's trusted runtime, never a worker-supplied path.
    pub fn open(daemon_data_root: &Path) -> io::Result<Self> {
        Ok(Self {
            storage: storage::Root::open_named(daemon_data_root, "file-recovery-quota")?,
        })
    }
    pub fn lock(&self) -> io::Result<LockedDeviceQuota> {
        let storage = self.storage.lock()?;
        let (ledger, reset) = match storage.read("index.json", MAX_INDEX_BYTES) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|error| super::undecodable("device quota index", &error))?;
                if value
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(FORMAT_VERSION))
                {
                    let ledger = serde_json::from_value(value)
                        .map_err(|error| super::undecodable("device quota index", &error))?;
                    (ledger, false)
                } else {
                    // Unreleased format: rebuilt, not migrated. Epochs realign
                    // through the workers' next reconciliation.
                    (Ledger::default(), true)
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                storage.validate_missing_index()?;
                (Ledger::default(), false)
            }
            Err(error) => return Err(error),
        };
        super::Policy {
            retention_days: ledger.retention_days,
            max_bytes: ledger.max_bytes,
        }
        .validate()?;
        if ledger.users.len() > MAX_ENTRIES {
            return Err(invalid("device quota index exceeds its bound"));
        }
        let mut reservations = 0usize;
        for (user, quota) in &ledger.users {
            reservations += quota.reservations.len();
            if !valid_id(user)
                || quota.reservations.iter().any(|(operation, reservation)| {
                    !valid_id(operation)
                        || namespace_user(&reservation.namespace) != Some(user.as_str())
                        || reservation.epoch > quota.epoch
                })
            {
                return Err(invalid("device quota index identity is corrupt"));
            }
        }
        if reservations > MAX_ENTRIES {
            return Err(invalid("device quota index exceeds its bound"));
        }
        let mut locked = LockedDeviceQuota {
            storage,
            ledger,
            failed: false,
        };
        if reset {
            let ledger = locked.ledger.clone();
            locked.persist(ledger)?;
        }
        Ok(locked)
    }
}
#[cfg(test)]
fn validate_limit(max_bytes: u64) -> io::Result<()> {
    super::Policy {
        retention_days: 7,
        max_bytes,
    }
    .validate()
}
impl LockedDeviceQuota {
    fn user(&self, os_user: &str) -> io::Result<(String, UserQuota)> {
        identity(os_user)?;
        let user = hash(os_user.as_bytes());
        let quota = self.ledger.users.get(&user).cloned().unwrap_or_default();
        Ok((user, quota))
    }
    pub fn epoch(&self, os_user: &str) -> io::Result<EpochStatus> {
        Ok(self.user(os_user)?.1.status())
    }
    fn store_user(&mut self, user: String, quota: UserQuota) -> io::Result<()> {
        if !self.ledger.users.contains_key(&user) && self.ledger.users.len() >= MAX_ENTRIES {
            return Err(invalid("device quota user index exhausted"));
        }
        let mut next = self.ledger.clone();
        next.users.insert(user, quota);
        self.persist(next)
    }
    /// Close the old execution epoch before the worker prunes replay indexes.
    pub fn begin_epoch_cleanup(&mut self, os_user: &str, expected: u64) -> io::Result<EpochStatus> {
        self.require_healthy()?;
        let (user, mut quota) = self.user(os_user)?;
        let epoch = expected
            .checked_add(1)
            .ok_or_else(|| invalid("quota epoch exhausted"))?;
        if quota.epoch == epoch {
            return Ok(quota.status());
        }
        if quota.epoch != expected {
            return Err(invalid("quota epoch changed"));
        }
        if quota.cleanup_pending {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "quota epoch cleanup is pending",
            ));
        }
        quota.epoch = epoch;
        quota.cleanup_pending = true;
        let status = quota.status();
        self.store_user(user, quota)?;
        Ok(status)
    }
    /// Complete the epoch cleanup after a reconciliation of the pruned vault.
    pub fn finish_epoch_cleanup(
        &mut self,
        os_user: &str,
        request: &FinishEpochRequest,
    ) -> io::Result<EpochStatus> {
        self.require_healthy()?;
        let (user, mut quota) = self.user(os_user)?;
        if quota.epoch != request.epoch || request.epoch == 0 {
            return Err(invalid("quota epoch changed"));
        }
        if !quota.cleanup_pending {
            return Ok(quota.status());
        }
        if !quota.reconciled.as_ref().is_some_and(|reconciled| {
            reconciled.vault_instance_id == request.vault.vault_instance_id
                && reconciled.revision >= request.vault.revision
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "quota reconciliation after epoch pruning is pending",
            ));
        }
        quota.cleanup_pending = false;
        quota
            .reservations
            .retain(|_, reservation| reservation.epoch >= request.epoch);
        let status = quota.status();
        self.store_user(user, quota)?;
        Ok(status)
    }
    fn require_healthy(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::other(
                "device quota write failed; reopen before further operations",
            ))
        } else {
            Ok(())
        }
    }
    pub fn policy(&self) -> super::Policy {
        super::Policy {
            retention_days: self.ledger.retention_days,
            max_bytes: self.ledger.max_bytes,
        }
    }
    pub fn set_policy(&mut self, policy: super::Policy) -> io::Result<()> {
        policy.validate()?;
        let mut next = self.ledger.clone();
        next.max_bytes = policy.max_bytes;
        next.retention_days = policy.retention_days;
        self.persist(next)
    }
    pub fn max_bytes(&self) -> u64 {
        self.ledger.max_bytes
    }
    /// Settled absolute usage plus in-flight reservations of all users.
    pub fn used_bytes(&self) -> u64 {
        self.ledger
            .users
            .values()
            .fold(0, |used, quota| used.saturating_add(quota.used()))
    }
    /// In-flight reservations; part of `used_bytes`, not additional usage.
    pub fn reserved_bytes(&self) -> u64 {
        self.ledger
            .users
            .values()
            .flat_map(|quota| quota.reservations.values())
            .fold(0, |total, reservation| {
                total.saturating_add(reservation.bytes)
            })
    }
    /// Publish the new in-memory state only after its durable write succeeds.
    fn persist(&mut self, next: Ledger) -> io::Result<()> {
        let bytes = serde_json::to_vec(&next)?;
        if bytes.len() as u64 > MAX_INDEX_BYTES {
            return Err(invalid("device quota index capacity exhausted"));
        }
        self.require_healthy()?;
        if let Err(error) = self.storage.write_index(&bytes) {
            // Rename may have succeeded before a later sync failure. Never
            // continue allocating from a potentially stale in-memory ledger.
            self.failed = true;
            return Err(error);
        }
        self.ledger = next;
        Ok(())
    }
    #[cfg(test)]
    /// Lower limits stop new reservations; existing backup materials are retained.
    pub fn set_limit(&mut self, max_bytes: u64) -> io::Result<()> {
        validate_limit(max_bytes)?;
        let mut next = self.ledger.clone();
        next.max_bytes = max_bytes;
        self.persist(next)
    }
    /// Reserve an upper bound before a worker publishes backup objects. The
    /// bound is cumulative per operation: a retry with the same or a smaller
    /// value changes nothing, a larger value grows the reservation.
    pub fn reserve(&mut self, key: &QuotaKey, bytes: u64) -> io::Result<()> {
        self.require_healthy()?;
        key.validate()?;
        if bytes == 0 {
            return Err(invalid("invalid device quota reservation"));
        }
        let mut quota = self
            .ledger
            .users
            .get(&key.os_user)
            .cloned()
            .unwrap_or_default();
        if key.epoch != quota.epoch || quota.cleanup_pending {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution epoch is closed or being cleaned",
            ));
        }
        let existing = quota.reservations.get(&key.operation);
        if let Some(existing) = existing {
            if existing.namespace != key.namespace || existing.epoch != key.epoch {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "device quota operation already recorded",
                ));
            }
            if bytes <= existing.bytes {
                return Ok(());
            }
        }
        let growth = bytes - existing.map_or(0, |existing| existing.bytes);
        let reservations: usize = self
            .ledger
            .users
            .values()
            .map(|quota| quota.reservations.len())
            .sum();
        if (existing.is_none() && reservations >= MAX_ENTRIES)
            || self.used_bytes().saturating_add(growth) > self.ledger.max_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device file backup capacity exhausted; file was not changed",
            ));
        }
        quota.reservations.insert(
            key.operation.clone(),
            Reservation {
                namespace: key.namespace.clone(),
                epoch: key.epoch,
                bytes,
            },
        );
        self.store_user(key.os_user.clone(), quota)
    }
    /// Two-part settlement: clearing the operation's reservation always
    /// applies (idempotently); the absolute usage applies only when it is
    /// newer than the stored report. Returns whether the usage was applied.
    pub fn settle(
        &mut self,
        os_user: &str,
        clear: Option<&QuotaKey>,
        usage: &UsageReport,
    ) -> io::Result<bool> {
        self.require_healthy()?;
        let (user, mut quota) = self.user(os_user)?;
        usage.validate(&user)?;
        if let Some(key) = clear {
            key.validate()?;
            if key.os_user != user {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "quota operation belongs to another OS user",
                ));
            }
            quota.reservations.remove(&key.operation);
        }
        let applied = quota.apply_usage(usage);
        if self.ledger.users.get(&user) != Some(&quota) {
            self.store_user(user, quota)?;
        }
        Ok(applied)
    }
    /// Replace the user's absolute usage, clear reservations of operations
    /// that are no longer live and align the execution epoch, which only moves
    /// forward: a rebuilt quota ledger adopts the vault's epoch and a rebuilt
    /// vault adopts the returned one.
    pub fn reconcile(
        &mut self,
        os_user: &str,
        request: &ReconcileRequest,
    ) -> io::Result<EpochStatus> {
        self.require_healthy()?;
        let (user, mut quota) = self.user(os_user)?;
        request.usage.validate(&user)?;
        if request.live_operations.len() > MAX_ENTRIES
            || request.live_operations.iter().any(|id| !valid_id(id))
        {
            return Err(invalid("invalid device quota reconciliation"));
        }
        quota.apply_usage(&request.usage);
        let vault = request.usage.vault();
        if vault.supersedes(quota.reconciled.as_ref()) || quota.reconciled.as_ref() == Some(&vault)
        {
            quota.reconciled = Some(vault);
        }
        quota
            .reservations
            .retain(|operation, _| request.live_operations.contains(operation));
        if !quota.cleanup_pending && request.execution_epoch > quota.epoch {
            quota.epoch = request.execution_epoch;
        }
        let status = quota.status();
        if self.ledger.users.get(&user) != Some(&quota) {
            self.store_user(user, quota)?;
        }
        Ok(status)
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
