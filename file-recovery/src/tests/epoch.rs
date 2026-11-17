use super::*;
use quota::{DeviceQuota, EpochStatus, FinishEpochRequest, QuotaKey, ReconcileRequest};
use std::sync::{Arc, Barrier, Mutex};

/// The daemon side: every request opens and locks the device quota, like an IPC handler.
#[derive(Clone)]
struct Daemon {
    root: PathBuf,
    finished: Arc<Mutex<Vec<FinishEpochRequest>>>,
    fail_finish: bool,
}
impl Daemon {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            finished: Arc::default(),
            fail_finish: false,
        }
    }
    fn quota(&self) -> quota::LockedDeviceQuota {
        DeviceQuota::open(&self.root).unwrap().lock().unwrap()
    }
    fn reserve(&self, record: &Record, bytes: u64) -> io::Result<()> {
        self.quota().reserve(&QuotaKey::for_record(record)?, bytes)
    }
    fn settle(&self, v: &mut LockedVault, record: &Record) {
        let report = v.usage_report();
        let applied = self
            .quota()
            .settle(
                "user-a",
                Some(&QuotaKey::for_record(record).unwrap()),
                &report,
            )
            .unwrap();
        assert!(applied);
        v.acknowledge_settlement(&report).unwrap();
    }
}
impl EpochCoordinator for Daemon {
    fn begin(&mut self, os_user: &str, expected: u64) -> io::Result<EpochStatus> {
        self.quota().begin_epoch_cleanup(os_user, expected)
    }
    fn reconcile(&mut self, os_user: &str, request: &ReconcileRequest) -> io::Result<EpochStatus> {
        self.quota().reconcile(os_user, request)
    }
    fn finish(&mut self, os_user: &str, request: &FinishEpochRequest) -> io::Result<EpochStatus> {
        if self.fail_finish {
            return Err(io::Error::other("finish reply lost"));
        }
        self.finished.lock().unwrap().push(request.clone());
        self.quota().finish_epoch_cleanup(os_user, request)
    }
}

fn discarded(v: &mut LockedVault, daemon: &Daemon, operation: &str, epoch: u64) -> Record {
    let record = v
        .backup(
            edit("c", operation, b"before", b"after"),
            epoch,
            |record, bytes| daemon.reserve(record, bytes),
        )
        .unwrap();
    commit(v, &record);
    daemon.settle(v, &record);
    v.discard(&scope(), "c", &record.id, 1001).unwrap();
    record
}

#[test]
fn epoch_cleanup_prunes_replay_records_and_finishes_after_reconciliation() {
    let (_root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(quota_root.path());
    let mut v = vault.lock().unwrap();
    let old = discarded(&mut v, &daemon, "old", 0);
    assert_eq!(v.prunable_index_count(), 1);
    v.maintain_epoch_indexes("user-a", &mut daemon, 1).unwrap();
    assert_eq!(v.execution_epoch(), 1);
    assert!(v.epoch_cleanup_state().is_none());
    assert!(v.record(&scope(), &old.id).is_none());
    assert_eq!(
        daemon.quota().epoch("user-a").unwrap(),
        EpochStatus {
            epoch: 1,
            cleanup_pending: false
        }
    );
    let finished = daemon.finished.lock().unwrap().clone();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].vault.vault_instance_id, v.vault_instance_id());
    assert_eq!(finished[0].vault.revision + 1, v.revision());
    assert_eq!(daemon.quota().reserved_bytes(), 0);
    // Old dispatches are fenced; the new epoch works end to end.
    assert_eq!(
        v.backup(edit("c", "late", b"a", b"b"), 0, no_quota)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    discarded(&mut v, &daemon, "fresh", 1);
}

#[test]
fn epoch_cleanup_would_block_while_a_backup_is_in_progress() {
    let (_root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(quota_root.path());
    let mut v = vault.lock().unwrap();
    discarded(&mut v, &daemon, "old", 0);
    let active = v
        .backup(edit("c", "active", b"x", b"y"), 0, |record, bytes| {
            daemon.reserve(record, bytes)
        })
        .unwrap();
    assert_eq!(
        v.maintain_epoch_indexes("user-a", &mut daemon, 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(v.epoch_cleanup_state().is_none());
    // The in-flight operation and its reservation are unaffected.
    assert!(daemon.quota().reserved_bytes() > 0);
    commit(&mut v, &active);
    daemon.settle(&mut v, &active);
    v.maintain_epoch_indexes("user-a", &mut daemon, 1).unwrap();
    assert_eq!(v.execution_epoch(), 1);
    assert!(v.record(&scope(), &active.id).is_some());
}

#[test]
fn interrupted_epoch_cleanup_resumes_after_restart() {
    let (_root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(quota_root.path());
    let mut v = vault.lock().unwrap();
    discarded(&mut v, &daemon, "old", 0);
    daemon.fail_finish = true;
    assert!(v.maintain_epoch_indexes("user-a", &mut daemon, 1).is_err());
    drop(v);
    let mut v = vault.lock().unwrap();
    assert_eq!(
        v.epoch_cleanup_state(),
        Some(EpochCleanupState {
            previous: 0,
            next: 1,
            pruned: true
        })
    );
    assert_eq!(
        v.backup(edit("c", "blocked", b"a", b"b"), 1, no_quota)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(daemon.quota().epoch("user-a").unwrap().cleanup_pending);
    daemon.fail_finish = false;
    v.maintain_epoch_indexes("user-a", &mut daemon, usize::MAX)
        .unwrap();
    assert!(v.epoch_cleanup_state().is_none());
    assert!(!daemon.quota().epoch("user-a").unwrap().cleanup_pending);
}

#[test]
fn rebuilt_vault_and_rebuilt_quota_realign_the_execution_epoch() {
    let (root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(quota_root.path());
    let mut v = vault.lock().unwrap();
    for epoch in 0..2 {
        discarded(&mut v, &daemon, &format!("old-{epoch}"), epoch);
        v.maintain_epoch_indexes("user-a", &mut daemon, 1).unwrap();
    }
    assert_eq!(v.execution_epoch(), 2);
    drop(v);
    // An unreleased-format vault is wiped and adopts the quota's epoch.
    let index = root.path().join("file-recovery/index.json");
    fs::write(&index, br#"{"format_version":2}"#).unwrap();
    let mut v = vault.lock().unwrap();
    assert_eq!(v.execution_epoch(), 0);
    let previous_instance = v.vault_instance_id().to_owned();
    assert_eq!(v.reconcile("user-a", &mut daemon).unwrap().epoch, 2);
    assert_eq!(v.execution_epoch(), 2);
    discarded(&mut v, &daemon, "after-vault-reset", 2);
    drop(v);
    // A rebuilt quota ledger adopts the vault's epoch on reconciliation.
    let quota_index = quota_root.path().join("file-recovery-quota/index.json");
    fs::write(&quota_index, br#"{"max_bytes":1048576}"#).unwrap();
    let mut v = vault.lock().unwrap();
    assert_eq!(daemon.quota().epoch("user-a").unwrap().epoch, 0);
    assert_eq!(v.reconcile("user-a", &mut daemon).unwrap().epoch, 2);
    assert_eq!(daemon.quota().used_bytes(), v.used_bytes());
    discarded(&mut v, &daemon, "after-quota-reset", 2);
    assert_eq!(v.vault_instance_id(), previous_instance);
}

#[test]
fn rebuilt_vault_resumes_a_cleanup_the_quota_began() {
    let (root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(quota_root.path());
    daemon.quota().begin_epoch_cleanup("user-a", 0).unwrap();
    drop(vault.lock().unwrap());
    fs::write(root.path().join("file-recovery/index.json"), b"{}").unwrap();
    let mut v = vault.lock().unwrap();
    let status = v.reconcile("user-a", &mut daemon).unwrap();
    assert!(status.cleanup_pending);
    assert_eq!(
        v.epoch_cleanup_state(),
        Some(EpochCleanupState {
            previous: 0,
            next: 1,
            pruned: true
        })
    );
    v.maintain_epoch_indexes("user-a", &mut daemon, usize::MAX)
        .unwrap();
    assert_eq!(v.execution_epoch(), 1);
    assert!(!daemon.quota().epoch("user-a").unwrap().cleanup_pending);
    discarded(&mut v, &daemon, "fresh", 1);
}

#[test]
fn interleaved_workers_never_clear_each_others_reservations() {
    let (root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let daemon = Daemon::new(quota_root.path());
    let reserved = Arc::new(Barrier::new(2));
    let events = Arc::new(Mutex::new(Vec::new()));
    let worker_a = {
        let (daemon, reserved, events) = (daemon.clone(), reserved.clone(), events.clone());
        let data = root.path().to_owned();
        std::thread::spawn(move || {
            let vault = Vault::open(&data).unwrap();
            let mut v = vault.lock().unwrap();
            let record = v
                .backup(edit("c", "a", b"a1", b"a2"), 0, |record, bytes| {
                    daemon.reserve(record, bytes)
                })
                .unwrap();
            events.lock().unwrap().push("a reserved");
            reserved.wait();
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                daemon.quota().reserved_bytes() > 0,
                "reservation kept while locked"
            );
            commit(&mut v, &record);
            daemon.settle(&mut v, &record);
            events.lock().unwrap().push("a settled");
        })
    };
    reserved.wait();
    // Worker B reconciles only once it holds the vault lock.
    let mut b = vault.lock().unwrap();
    events.lock().unwrap().push("b reconciling");
    b.recover_interrupted(2000).unwrap();
    b.sweep(2000).unwrap();
    b.reconcile("user-a", &mut daemon.clone()).unwrap();
    worker_a.join().unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        ["a reserved", "a settled", "b reconciling"]
    );
    assert_eq!(daemon.quota().reserved_bytes(), 0);
    assert_eq!(daemon.quota().used_bytes(), b.used_bytes());
}
