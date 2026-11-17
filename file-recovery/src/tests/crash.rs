use super::*;
use quota::{DeviceQuota, LockedDeviceQuota, QuotaKey};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn quota() -> (tempfile::TempDir, DeviceQuota) {
    let root = tempfile::tempdir().unwrap();
    let quota = DeviceQuota::open(root.path()).unwrap();
    (root, quota)
}
fn reserve(quota: &mut LockedDeviceQuota, record: &Record, bytes: u64) -> io::Result<()> {
    quota.reserve(&QuotaKey::for_record(record)?, bytes)
}
/// Worker restart: recover, sweep, then reconcile under the vault lock.
fn restart(vault: &Vault, quota: &mut LockedDeviceQuota) -> LockedVault {
    let mut v = vault.lock().unwrap();
    v.recover_interrupted(2000).unwrap();
    v.sweep(2000).unwrap();
    v.reconcile("user-a", quota).unwrap();
    v
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Crash {
    BeforeReserve,
    AfterReserve,
    AfterPublish,
    AfterBackupReady,
    AfterCommitIntent,
    AfterCommit,
    AfterSucceeded,
    AfterSettle,
}

#[test]
fn crash_matrix_leaks_no_quota_and_leaves_a_determined_state() {
    use Crash::*;
    for crash in [
        BeforeReserve,
        AfterReserve,
        AfterPublish,
        AfterBackupReady,
        AfterCommitIntent,
        AfterCommit,
        AfterSucceeded,
        AfterSettle,
    ] {
        let (root, vault) = fixture();
        let (_quota_root, device) = quota();
        let mut quota = device.lock().unwrap();
        let id = record_id(&scope(), "c", "op").unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut v = vault.lock().unwrap();
            let record = v
                .backup(edit("c", "op", b"before", b"after"), 0, |record, bytes| {
                    if crash == BeforeReserve {
                        panic!("injected crash before reservation");
                    }
                    reserve(&mut quota, record, bytes)?;
                    if crash == AfterReserve {
                        panic!("injected crash after reservation");
                    }
                    Ok(())
                })
                .unwrap();
            if crash == AfterPublish {
                return;
            }
            v.flush().unwrap();
            if crash == AfterBackupReady {
                return;
            }
            v.transition(&scope(), &record.id, ChangeState::CommitIntent)
                .unwrap();
            if matches!(crash, AfterCommitIntent | AfterCommit) {
                return;
            }
            v.finish(&scope(), &record.id, ChangeState::Succeeded)
                .unwrap();
            if crash == AfterSucceeded {
                return;
            }
            let report = v.usage_report();
            assert!(
                quota
                    .settle(
                        "user-a",
                        Some(&QuotaKey::for_record(&record).unwrap()),
                        &report
                    )
                    .unwrap()
            );
            v.acknowledge_settlement(&report).unwrap();
        }));
        assert_eq!(
            outcome.is_err(),
            matches!(crash, BeforeReserve | AfterReserve)
        );
        let v = restart(&vault, &mut quota);
        assert_eq!(quota.reserved_bytes(), 0, "{crash:?} leaked a reservation");
        assert_eq!(
            quota.used_bytes(),
            v.used_bytes(),
            "{crash:?} usage differs"
        );
        let record = &v.ledger.records[&id];
        let (state, material, files) = match crash {
            BeforeReserve | AfterReserve | AfterPublish | AfterBackupReady => {
                (ChangeState::Aborted, MaterialState::Purged, 0)
            }
            AfterCommitIntent | AfterCommit => {
                (ChangeState::OutcomeUnknown, MaterialState::Saved, 3)
            }
            AfterSucceeded | AfterSettle => (ChangeState::Succeeded, MaterialState::Saved, 3),
        };
        assert_eq!(
            (record.change, record.material),
            (state, material),
            "{crash:?}"
        );
        assert_eq!(objects(root.path(), &scope()).len(), files, "{crash:?}");
        if material == MaterialState::Saved {
            assert_eq!(before_of(&v, record), b"before");
        }
        // The fenced operation is never repeated after any crash point.
        let mut v = v;
        assert_eq!(
            v.backup(edit("c", "op", b"before", b"after"), 0, no_quota)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }
}

#[test]
fn half_written_material_is_replaced_and_swept() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let first = save(&mut v, "c", "first");
    commit(&mut v, &first);
    let directory = namespace_dir(root.path(), &scope());
    let content = b"content written twice".as_slice();
    let sha = hash(content);
    // A crash inside the temporary area, and a truncated file at the final
    // name that no record references.
    fs::write(
        directory.join("tmp").join(format!("{sha}-crashed")),
        &b"LRD"[..],
    )
    .unwrap();
    fs::write(directory.join(&sha), b"LRDO").unwrap();
    let record = v
        .backup(edit("c", "second", content, b"later"), 0, no_quota)
        .unwrap();
    commit(&mut v, &record);
    assert_eq!(before_of(&v, &record), content);
    v.sweep(1000).unwrap();
    assert_eq!(fs::read_dir(directory.join("tmp")).unwrap().count(), 0);
    assert_eq!(before_of(&v, &record), content);
}

#[test]
fn reused_object_swept_before_publication_is_published_as_a_full_frame() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let old = v
        .backup(edit("old", "one", b"base text", b"next text"), 0, no_quota)
        .unwrap();
    commit(&mut v, &old);
    let pending = edit("new", "two", b"next text", b"final text");
    let encoded = v.encode(&pending).unwrap();
    drop(v);
    // Another worker deletes the only conversation that referenced the object.
    let mut other = vault.lock().unwrap();
    other.delete_conversation(&scope(), "old", 2000).unwrap();
    drop(other);
    assert!(objects(root.path(), &scope()).is_empty());
    let mut v = vault.lock().unwrap();
    let mut reserved = Vec::new();
    let record = v
        .backup_encoded(pending, encoded, 0, |_, bytes| {
            reserved.push(bytes);
            Ok(())
        })
        .unwrap();
    commit(&mut v, &record);
    let entry = record.inline_entry().unwrap();
    assert!(entry.before.as_ref().unwrap().bases.is_empty());
    assert!(entry.after.as_ref().unwrap().bases.is_empty());
    assert_eq!(before_of(&v, &record), b"next text");
    assert_eq!(reserved.len(), 2, "fallback frames grow the reservation");
    assert!(reserved[1] > reserved[0]);
}

#[test]
fn delta_base_swept_before_publication_falls_back_to_a_full_frame() {
    let (_root, vault) = fixture();
    let base = encoding::text(2000, 1);
    let mut similar = base.clone();
    similar[10..14].copy_from_slice(b"EDIT");
    let mut v = vault.lock().unwrap();
    let old = v
        .backup(edit("old", "one", b"first", &base), 0, no_quota)
        .unwrap();
    commit(&mut v, &old);
    let pending = edit("new", "two", &similar, b"final");
    let encoded = v.encode(&pending).unwrap();
    drop(v);
    vault
        .lock()
        .unwrap()
        .delete_conversation(&scope(), "old", 2000)
        .unwrap();
    let mut v = vault.lock().unwrap();
    let record = v.backup_encoded(pending, encoded, 0, no_quota).unwrap();
    assert!(
        record
            .inline_entry()
            .unwrap()
            .before
            .as_ref()
            .unwrap()
            .bases
            .is_empty()
    );
    commit(&mut v, &record);
    assert_eq!(before_of(&v, &record), similar);
}

#[test]
fn crash_after_rename_before_ledger_persist_is_recounted_by_sweep() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "c", "op");
    drop(v);
    let mut v = vault.lock().unwrap();
    assert!(v.ledger.namespaces.is_empty());
    assert_eq!(v.ledger.records[&record.id].change, ChangeState::Preparing);
    let files = objects(root.path(), &scope());
    assert_eq!(files.len(), 3);
    v.sweep(1000).unwrap();
    let usage = v.ledger.namespaces[&scope().namespace()];
    assert_eq!(usage.objects, 3);
    assert_eq!(
        usage.bytes,
        files
            .iter()
            .map(|(_, frame)| round_block(frame.len() as u64))
            .sum::<u64>()
    );
}

#[test]
fn late_reservation_after_conversation_deletion_is_fenced_by_the_tombstone() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    v.delete_conversation(&scope(), "c", 1000).unwrap();
    let error = v
        .backup(edit("c", "late", b"a", b"b"), 0, |_, _| {
            panic!("a deleted conversation must not reserve")
        })
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    // Another conversation of the same scope is unaffected.
    save(&mut v, "other", "op");
}

#[test]
fn failed_reservation_aborts_before_any_object_is_published() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut backup = edit("c", "op", b"a", b"b");
    // The planned transaction directory is created only after a successful
    // backup, so an aborted backup must not keep it as pending cleanup.
    #[cfg(unix)]
    {
        backup.transaction = Some(TransactionPlan::unix("/planned-parent", 1, 2, 3).unwrap());
    }
    let error = v
        .backup(backup, 0, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device quota exhausted",
            ))
        })
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    assert!(objects(root.path(), &scope()).is_empty());
    drop(v);
    let v = vault.lock().unwrap();
    let stored = &v.list(&scope(), None)[0];
    assert_eq!(stored.change, ChangeState::Aborted);
    assert!(stored.transaction.is_none());
}

#[cfg(unix)]
#[test]
fn one_change_rewrites_the_ledger_at_most_four_times() {
    use std::os::unix::fs::MetadataExt;
    for deleting in [false, true] {
        let (_root, vault) = fixture();
        let user_dir = tempfile::tempdir().unwrap();
        let original = user_dir.path().join("document.txt");
        fs::write(&original, b"before").unwrap();
        let parent = fs::metadata(user_dir.path()).unwrap();
        let mut v = vault.lock().unwrap();
        // The first clock observation anchors the guard; afterwards the
        // maintenance that precedes every mutation writes nothing when idle.
        v.observe_system_clock().unwrap();
        let start = v.persist_count;
        v.observe_system_clock().unwrap();
        v.cleanup(1000).unwrap();
        let mut backup = request(
            "c",
            "op",
            if deleting {
                RecordKind::TextDelete
            } else {
                RecordKind::TextEdit
            },
            vec![entry(
                "document.txt",
                Some(b"before"),
                (!deleting).then_some(b"after".as_slice()),
            )],
        );
        backup.transaction = Some(
            TransactionPlan::unix(
                user_dir.path().to_str().unwrap(),
                parent.dev(),
                parent.ino(),
                fs::metadata(&original).unwrap().ino(),
            )
            .unwrap(),
        );
        let record = v.backup(backup, 0, no_quota).unwrap();
        let directory = user_dir
            .path()
            .join(v.transaction_directory(&scope(), &record.id).unwrap());
        fs::create_dir(&directory).unwrap();
        let staged = (!deleting).then(|| {
            fs::write(directory.join("replacement"), b"after").unwrap();
            fs::metadata(directory.join("replacement")).unwrap().ino()
        });
        v.register_transaction(
            &scope(),
            &record.id,
            fs::metadata(&directory).unwrap().ino(),
            staged,
        )
        .unwrap();
        v.transition(&scope(), &record.id, ChangeState::CommitIntent)
            .unwrap();
        if deleting {
            fs::rename(&original, directory.join("original")).unwrap();
        } else {
            // Swap: the old original ends up under the staged name.
            fs::rename(&original, directory.join("swap")).unwrap();
            fs::rename(directory.join("replacement"), &original).unwrap();
            fs::rename(directory.join("swap"), directory.join("replacement")).unwrap();
        }
        assert!(
            v.finish(&scope(), &record.id, ChangeState::Succeeded)
                .unwrap()
        );
        let writes = v.persist_count - start;
        println!(
            "{} ledger rewrites for one {}",
            writes,
            if deleting { "delete" } else { "update" }
        );
        assert!(writes <= 4, "{writes} ledger rewrites");
        assert!(!directory.exists());
        drop(v);
        let v = vault.lock().unwrap();
        let stored = &v.list(&scope(), None)[0];
        assert_eq!(stored.change, ChangeState::Succeeded);
        assert!(stored.transaction.is_none());
    }
}

#[test]
fn interrupted_backup_objects_are_never_reused_without_their_files() {
    let (_root, vault) = fixture();
    let base = encoding::text(2000, 2);
    let mut next = base.clone();
    next[40..44].copy_from_slice(b"EDIT");
    // A worker crashes after its Preparing record became durable and before
    // anything was published; no restart recovery has run yet.
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        let mut v = vault.lock().unwrap();
        v.backup(edit("c", "crashed", &base, &next), 0, |_, _| {
            panic!("injected crash before publication")
        })
    }));
    assert!(crashed.is_err());
    // Another worker of the same OS user changes the same file to the same
    // content: the ledger names those objects, but their files do not exist.
    let mut v = vault.lock().unwrap();
    assert_eq!(
        v.list(&scope(), None)[0].change,
        ChangeState::Preparing,
        "the interrupted record is still a sweep root"
    );
    let record = v
        .backup(edit("c", "retry", &base, &next), 0, no_quota)
        .unwrap();
    commit(&mut v, &record);
    assert!(record.published_bytes > 0);
    assert_eq!(before_of(&v, &record), base);
    let after = record.inline_entry().unwrap().after.clone().unwrap();
    assert_eq!(v.read_object(&scope().namespace(), &after).unwrap(), next);
    // Restart recovery aborts the interrupted record; the retry keeps its material.
    v.recover_interrupted(2000).unwrap();
    v.sweep(2000).unwrap();
    assert_eq!(before_of(&v, &record), base);
}

#[test]
fn reintroduced_objects_and_the_shared_ledger_settle_to_actual_usage() {
    let (root, vault) = fixture();
    let (_quota_root, device) = quota();
    let mut quota = device.lock().unwrap();
    let mut other = scope();
    other.owner = "owner-b".into();
    let mut v = vault.lock().unwrap();
    // Usage on disk: the whole index file once, plus every namespace's
    // block-rounded object files.
    let actual = |v: &LockedVault| {
        let index = fs::metadata(root.path().join("file-recovery/index.json"))
            .unwrap()
            .len();
        let objects: u64 = [scope(), other.clone()]
            .iter()
            .flat_map(|scope| objects(root.path(), scope))
            .map(|(_, frame)| round_block(frame.len() as u64))
            .sum();
        assert_eq!(v.used_bytes(), index + objects);
        index + objects
    };
    let change = |v: &mut LockedVault,
                  quota: &mut LockedDeviceQuota,
                  scope: &Scope,
                  conversation: &str,
                  operation: &str| {
        let mut backup = edit(conversation, operation, b"shared text", b"next text");
        backup.scope = scope.clone();
        let record = v
            .backup(backup, 0, |record, bytes| reserve(quota, record, bytes))
            .unwrap();
        commit(v, &record);
        let report = v.usage_report();
        assert!(
            quota
                .settle(
                    "user-a",
                    Some(&QuotaKey::for_record(&record).unwrap()),
                    &report
                )
                .unwrap()
        );
        assert_eq!(quota.reserved_bytes(), 0);
        assert_eq!(quota.used_bytes(), actual(v));
        record
    };
    change(&mut v, &mut quota, &scope(), "first", "op");
    change(&mut v, &mut quota, &other, "first", "op");
    // Two namespaces of one OS user share one ledger file, billed once.
    let report = v.usage_report();
    assert_eq!(report.namespaces.len(), 2);
    v.delete_conversation(&scope(), "first", 2000).unwrap();
    assert!(objects(root.path(), &scope()).is_empty());
    let report = v.usage_report();
    assert!(quota.settle("user-a", None, &report).unwrap());
    assert_eq!(quota.used_bytes(), actual(&v));
    // Clearing the tombstone's flag is itself reported by the next settlement.
    v.acknowledge_settlement(&report).unwrap();
    assert!(v.cleanup_complete(&scope(), "first"));
    // The same objects are introduced again by a new conversation.
    let again = change(&mut v, &mut quota, &scope(), "second", "op");
    assert!(again.published_bytes > 0);
    assert_eq!(quota.used_bytes(), actual(&v));
}
