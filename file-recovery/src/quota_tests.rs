use super::*;
use crate::Scope;

fn scope(user: &str) -> Scope {
    Scope {
        authority: "authority".into(),
        device: "device".into(),
        os_user: user.into(),
        owner: "owner".into(),
    }
}
fn key(user: &str, operation: &str) -> QuotaKey {
    QuotaKey::new(&scope(user), "conversation", operation, 0).unwrap()
}
fn usage(user: &str, instance: &str, revision: u64, bytes: u64) -> UsageReport {
    UsageReport {
        vault_instance_id: hash(instance.as_bytes()),
        revision,
        ledger_bytes: 100,
        namespaces: BTreeMap::from([(scope(user).namespace(), bytes)]),
    }
}
fn open() -> (tempfile::TempDir, DeviceQuota) {
    let root = tempfile::tempdir().unwrap();
    let quota = DeviceQuota::open(root.path()).unwrap();
    (root, quota)
}

#[test]
fn default_quota_is_512_mib() {
    let (_root, quota) = open();
    let locked = quota.lock().unwrap();
    assert_eq!(locked.max_bytes(), 512 * 1024 * 1024);
    assert_eq!(locked.policy(), crate::Policy::default());
}

#[test]
fn reservation_is_idempotent_by_operation_and_only_grows() {
    let (_root, quota) = open();
    let mut locked = quota.lock().unwrap();
    let first = key("501", "op");
    locked.reserve(&first, 4096).unwrap();
    locked.reserve(&first, 4096).unwrap();
    locked.reserve(&first, 1024).unwrap();
    assert_eq!(locked.reserved_bytes(), 4096);
    locked.reserve(&first, 8192).unwrap();
    assert_eq!(locked.reserved_bytes(), 8192);
    let mut other_epoch = first.clone();
    other_epoch.epoch = 1;
    assert_eq!(
        locked.reserve(&other_epoch, 1).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    locked.set_limit(1024 * 1024).unwrap();
    assert_eq!(
        locked
            .reserve(&key("501", "large"), 1024 * 1024)
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(
        locked.reserve(&first, 2 * 1024 * 1024).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    drop(locked);
    assert_eq!(quota.lock().unwrap().reserved_bytes(), 8192);
}

#[test]
fn stale_settlement_is_rejected_but_still_clears_its_reservation() {
    let (_root, quota) = open();
    let mut locked = quota.lock().unwrap();
    let operation = key("501", "op");
    locked.reserve(&operation, 50_000).unwrap();
    assert!(
        locked
            .settle("501", None, &usage("501", "vault", 10, 7000))
            .unwrap()
    );
    // An older report arriving out of order neither lowers usage nor keeps
    // the reservation it settles.
    assert!(
        !locked
            .settle("501", Some(&operation), &usage("501", "vault", 9, 1000))
            .unwrap()
    );
    assert_eq!(locked.reserved_bytes(), 0);
    assert_eq!(locked.used_bytes(), 7100);
    // A rebuilt vault restarts its revisions under a new instance id.
    assert!(
        locked
            .settle("501", None, &usage("501", "rebuilt", 1, 300))
            .unwrap()
    );
    assert_eq!(locked.used_bytes(), 400);
    assert!(
        locked
            .settle("502", None, &usage("501", "vault", 11, 1))
            .is_err(),
        "another user's namespace is rejected"
    );
    assert!(
        locked
            .settle(
                "501",
                Some(&key("502", "op")),
                &usage("501", "vault", 12, 1)
            )
            .is_err()
    );
}

#[test]
fn reconcile_clears_orphan_reservations_and_keeps_live_ones() {
    let (_root, quota) = open();
    let mut locked = quota.lock().unwrap();
    let live = key("501", "live");
    let orphan = key("501", "orphan");
    let other_user = key("502", "other");
    for key in [&live, &orphan, &other_user] {
        locked.reserve(key, 4096).unwrap();
    }
    let status = locked
        .reconcile(
            "501",
            &ReconcileRequest {
                execution_epoch: 0,
                usage: usage("501", "vault", 3, 9000),
                live_operations: vec![live.operation.clone()],
            },
        )
        .unwrap();
    assert_eq!(status, EpochStatus::default());
    assert_eq!(locked.reserved_bytes(), 8192);
    assert_eq!(locked.used_bytes(), 9100 + 8192);
    assert!(
        locked
            .reconcile(
                "501",
                &ReconcileRequest {
                    execution_epoch: 0,
                    usage: usage("501", "vault", 3, 9000),
                    live_operations: vec!["not-an-id".into()],
                },
            )
            .is_err()
    );
}

#[test]
fn reconcile_aligns_epochs_forward_only() {
    let (_root, quota) = open();
    let mut locked = quota.lock().unwrap();
    let request = |epoch| ReconcileRequest {
        execution_epoch: epoch,
        usage: usage("501", "vault", epoch + 1, 0),
        live_operations: Vec::new(),
    };
    assert_eq!(locked.reconcile("501", &request(5)).unwrap().epoch, 5);
    assert_eq!(locked.reconcile("501", &request(2)).unwrap().epoch, 5);
    let mut current = key("501", "op");
    current.epoch = 5;
    locked.reserve(&current, 1).unwrap();
}

#[test]
fn epoch_cleanup_finishes_only_after_a_reconciliation_at_the_pruned_revision() {
    let (_root, quota) = open();
    let mut locked = quota.lock().unwrap();
    let old = key("501", "old");
    locked.reserve(&old, 4096).unwrap();
    assert_eq!(
        locked.begin_epoch_cleanup("501", 0).unwrap(),
        EpochStatus {
            epoch: 1,
            cleanup_pending: true
        }
    );
    assert_eq!(locked.begin_epoch_cleanup("501", 0).unwrap().epoch, 1);
    assert_eq!(
        locked.reserve(&key("501", "late"), 1).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let finish = FinishEpochRequest {
        epoch: 1,
        vault: usage("501", "vault", 20, 0).vault(),
    };
    assert_eq!(
        locked
            .finish_epoch_cleanup("501", &finish)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    locked
        .reconcile(
            "501",
            &ReconcileRequest {
                execution_epoch: 1,
                usage: usage("501", "vault", 19, 0),
                live_operations: Vec::new(),
            },
        )
        .unwrap();
    assert!(locked.finish_epoch_cleanup("501", &finish).is_err());
    locked
        .reconcile(
            "501",
            &ReconcileRequest {
                execution_epoch: 1,
                usage: usage("501", "vault", 20, 0),
                live_operations: Vec::new(),
            },
        )
        .unwrap();
    drop(locked);
    let mut restarted = quota.lock().unwrap();
    assert_eq!(
        restarted.finish_epoch_cleanup("501", &finish).unwrap(),
        EpochStatus {
            epoch: 1,
            cleanup_pending: false
        }
    );
    assert_eq!(restarted.reserved_bytes(), 0);
    assert!(restarted.finish_epoch_cleanup("502", &finish).is_err());
    let mut fresh = key("501", "fresh");
    fresh.epoch = 1;
    restarted.reserve(&fresh, 10).unwrap();
}

#[test]
fn other_format_is_rebuilt_and_corruption_is_not() {
    let (root, quota) = open();
    let index = root.path().join("file-recovery-quota/index.json");
    quota
        .lock()
        .unwrap()
        .reserve(&key("501", "op"), 10)
        .unwrap();
    let legacy = serde_json::json!({
        "max_bytes": 1048576, "retention_days": 7, "entries": {},
        "closed_namespaces": {}, "user_epochs": {}
    });
    std::fs::write(&index, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let locked = quota.lock().unwrap();
    assert_eq!(locked.reserved_bytes(), 0);
    assert_eq!(locked.max_bytes(), 512 * 1024 * 1024);
    drop(locked);
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    assert_eq!(stored["format_version"], 1);
    std::fs::write(&index, b"broken").unwrap();
    assert!(quota.lock().is_err());
    std::fs::write(&index, br#"{"format_version":1}"#).unwrap();
    assert!(quota.lock().is_err());
    assert_eq!(std::fs::read(&index).unwrap(), br#"{"format_version":1}"#);
}
