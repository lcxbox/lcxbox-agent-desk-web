use super::*;

#[cfg(unix)]
fn planned(
    v: &mut LockedVault,
    conversation: &str,
    operation: &str,
    user_dir: &Path,
    original: &Path,
) -> (Record, PathBuf) {
    use std::os::unix::fs::MetadataExt;
    let parent = fs::metadata(user_dir).unwrap();
    let original_inode = fs::metadata(original).map(|m| m.ino()).unwrap_or(0);
    let mut backup = edit(conversation, operation, b"before", b"after");
    backup.transaction = Some(
        TransactionPlan::unix(
            user_dir.to_str().unwrap(),
            parent.dev(),
            parent.ino(),
            original_inode,
        )
        .unwrap(),
    );
    let record = v.backup(backup, 0, no_quota).unwrap();
    let directory = user_dir.join(v.transaction_directory(&scope(), &record.id).unwrap());
    (record, directory)
}
#[cfg(unix)]
fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
}

#[test]
fn default_policy_allows_512_mib() {
    let (_root, vault) = fixture();
    assert_eq!(vault.lock().unwrap().policy().max_bytes, 512 * 1024 * 1024);
}

#[test]
fn maintenance_skips_an_active_transaction_lock() {
    let (root, vault) = fixture();
    let lock = vault.lock().unwrap();
    assert!(vault.try_lock().unwrap().is_none());
    let other = Vault::open(root.path()).unwrap();
    assert!(other.try_lock().unwrap().is_none());
    drop(lock);
    assert!(vault.try_lock().unwrap().is_some());
}

#[test]
fn lock_child() {
    let Ok(path) = std::env::var("RECOVERY_TEST_LOCK_PATH") else {
        return;
    };
    assert!(
        Vault::open(Path::new(&path).parent().unwrap().parent().unwrap())
            .unwrap()
            .try_lock()
            .unwrap()
            .is_none()
    );
}
#[test]
fn worker_process_cannot_enter_another_process_transaction() {
    let (root, vault) = fixture();
    let _locked = vault.lock().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::lifecycle::lock_child"])
        .env(
            "RECOVERY_TEST_LOCK_PATH",
            root.path().join("file-recovery/lock"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "requires access to the host boot identity and continuous clock"]
fn confirmed_clock_recovery_preserves_timestamps_and_allows_explicit_discard() {
    let (_root, vault) = fixture();
    let now = || {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let mut locked = vault.lock().unwrap();
    let future = now() + 86_400_000;
    let mut backup = edit("conversation", "operation", b"before", b"after");
    backup.now_ms = future;
    let record = locked.backup(backup, 0, no_quota).unwrap();
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    locked.ledger.last_cleanup_at_ms = future;
    locked.ledger.cleanup_clock_paused = true;
    locked.persist().unwrap();
    assert!(locked.acknowledge_clock(future).is_err());
    assert!(locked.cleanup_clock_paused());
    locked.acknowledge_clock(now()).unwrap();
    assert!(!locked.cleanup_clock_paused());
    drop(locked);
    let mut reopened = vault.lock().unwrap();
    reopened.observe_system_clock().unwrap();
    reopened.cleanup(now()).unwrap();
    let retained = &reopened.list(&scope(), None)[0];
    assert_eq!(retained.created_at_ms, future);
    assert_eq!(retained.material, MaterialState::Saved);
    reopened
        .discard(&scope(), "conversation", &record.id, now())
        .unwrap();
    assert_eq!(
        reopened.list(&scope(), None)[0].material,
        MaterialState::Purged
    );
}

#[test]
fn persisted_forward_clock_pause_prevents_expired_material_cleanup() {
    let (root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    locked.ledger.clock_guard.blocked = true;
    locked.persist().unwrap();
    drop(locked);
    let mut reopened = vault.lock().unwrap();
    assert!(reopened.cleanup(record.expires_at_ms + 1).is_err());
    assert!(reopened.cleanup_clock_paused());
    assert_eq!(
        reopened.list(&scope(), None)[0].material,
        MaterialState::Saved
    );
    assert_eq!(objects(root.path(), &scope()).len(), 3);
}

#[test]
fn explicit_discard_requires_exact_scope_and_settled_operation_and_fences_replay() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "conversation", "operation");
    assert_eq!(
        v.discard(&scope(), "conversation", &record.id, 1001)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    v.transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    v.recover_interrupted(1002).unwrap();
    let mut wrong_owner = scope();
    wrong_owner.owner = "someone-else".into();
    assert!(
        v.discard(&wrong_owner, "conversation", &record.id, 1003)
            .is_err()
    );
    assert!(v.discard(&scope(), "different", &record.id, 1003).is_err());
    // An unknown outcome stays exportable after its retention period.
    assert!(
        v.entry_material(
            &scope(),
            "conversation",
            &record.id,
            0,
            record.expires_at_ms + 1
        )
        .is_ok()
    );
    v.discard(
        &scope(),
        "conversation",
        &record.id,
        record.expires_at_ms + 1,
    )
    .unwrap();
    assert!(objects(root.path(), &scope()).is_empty());
    drop(v);
    let mut reopened = vault.lock().unwrap();
    let retained = &reopened.list(&scope(), None)[0];
    assert_eq!(retained.change, ChangeState::OutcomeUnknown);
    assert_eq!(retained.material, MaterialState::Purged);
    assert!(retained.discard_requested);
    reopened
        .discard(
            &scope(),
            "conversation",
            &record.id,
            record.expires_at_ms + 2,
        )
        .unwrap();
    let mut replay = edit("conversation", "operation", b"replacement", b"x");
    replay.generation = "new-generation";
    assert_eq!(
        reopened.backup(replay, 0, no_quota).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn clock_rollback_pauses_recovery_across_restart_until_time_catches_up() {
    let (_root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    locked.flush().unwrap();
    locked.cleanup(2000).unwrap();
    // The advanced fence is written with the next ledger update.
    locked.set_policy(Policy::default()).unwrap();
    drop(locked);

    let mut reopened = vault.lock().unwrap();
    let error = reopened.recover_interrupted(1500).unwrap_err();
    assert!(error.to_string().contains("system clock changed"));
    assert_eq!(reopened.pending_cleanup_count(), 1);
    assert_eq!(
        reopened.list(&scope(), None)[0].change,
        ChangeState::BackupReady
    );
    assert_eq!(before_of(&reopened, &record), b"before");
    drop(reopened);

    let mut retried = vault.lock().unwrap();
    assert!(retried.cleanup(1600).is_err());
    retried.recover_interrupted(2001).unwrap();
    assert_eq!(retried.pending_cleanup_count(), 0);
    assert_eq!(
        retried.list(&scope(), None)[0].material,
        MaterialState::Purged
    );
}

#[test]
fn clock_before_backup_creation_pauses_first_cleanup() {
    let (_root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    assert!(locked.recover_interrupted(999).is_err());
    assert_eq!(
        locked.list(&scope(), None)[0].change,
        ChangeState::BackupReady
    );
    assert_eq!(before_of(&locked, &record), b"before");
}

#[cfg(unix)]
#[test]
fn maintenance_retries_registered_temporary_files_before_backup_expiry() {
    let (_root, vault) = fixture();
    let user_dir = tempfile::tempdir().unwrap();
    let original = user_dir.path().join("document.txt");
    fs::write(&original, b"before").unwrap();
    let mut v = vault.lock().unwrap();
    let (record, directory) = planned(&mut v, "s", "a", user_dir.path(), &original);
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("replacement"), b"after").unwrap();
    v.register_transaction(
        &scope(),
        &record.id,
        inode(&directory),
        Some(inode(&directory.join("replacement"))),
    )
    .unwrap();
    // Simulate a worker stopping before commit; only its staged file is removed.
    drop(v);
    let mut v = vault.lock().unwrap();
    assert!(v.recover_interrupted(1001).unwrap().is_empty());
    assert!(!directory.exists());
    assert_eq!(fs::read(&original).unwrap(), b"before");
}

#[cfg(unix)]
#[test]
fn missing_commit_result_cleans_only_registered_temporary_objects() {
    for committed in [false, true] {
        let (_root, vault) = fixture();
        let user_dir = tempfile::tempdir().unwrap();
        let original = user_dir.path().join("document.txt");
        fs::write(&original, b"before").unwrap();
        let mut v = vault.lock().unwrap();
        let (record, directory) = planned(&mut v, "s", "unknown", user_dir.path(), &original);
        fs::create_dir(&directory).unwrap();
        let replacement = directory.join("replacement");
        fs::write(&replacement, b"after").unwrap();
        v.register_transaction(
            &scope(),
            &record.id,
            inode(&directory),
            Some(inode(&replacement)),
        )
        .unwrap();
        v.transition(&scope(), &record.id, ChangeState::CommitIntent)
            .unwrap();
        if committed {
            fs::rename(&replacement, &original).unwrap();
        }
        drop(v);
        let mut v = vault.lock().unwrap();
        let unknown = v.recover_interrupted(1001).unwrap();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].change, ChangeState::OutcomeUnknown);
        assert_eq!(
            fs::read(&original).unwrap(),
            if committed {
                b"after".as_slice()
            } else {
                b"before".as_slice()
            }
        );
        assert!(!directory.exists());
        assert_eq!(before_of(&v, &record), b"before");
    }
}

#[cfg(unix)]
#[test]
fn cleanup_never_deletes_replaced_transaction_objects() {
    let (_root, vault) = fixture();
    let user_dir = tempfile::tempdir().unwrap();
    let mut v = vault.lock().unwrap();
    let missing = user_dir.path().join("missing");
    let (record, directory) = planned(&mut v, "s", "a", user_dir.path(), &missing);
    fs::create_dir(&directory).unwrap();
    let staged = directory.join("replacement");
    fs::write(&staged, b"staged").unwrap();
    v.register_transaction(
        &scope(),
        &record.id,
        inode(&directory),
        Some(inode(&staged)),
    )
    .unwrap();
    fs::rename(&staged, user_dir.path().join("moved-staged")).unwrap();
    fs::write(&staged, b"unrelated user content").unwrap();
    assert!(
        !v.finish(&scope(), &record.id, ChangeState::Aborted)
            .unwrap()
    );
    v.recover_interrupted(1001).unwrap();
    assert_eq!(fs::read(&staged).unwrap(), b"unrelated user content");
    assert!(v.list(&scope(), None)[0].cleanup_error.is_some());
    v.discard(&scope(), "s", &record.id, 1002).unwrap();
    assert_eq!(fs::read(&staged).unwrap(), b"unrelated user content");
    assert_ne!(v.list(&scope(), None)[0].material, MaterialState::Purged);
    assert!(v.list(&scope(), None)[0].discard_requested);
    drop(v);
    assert!(
        vault.lock().unwrap().list(&scope(), None)[0]
            .cleanup_error
            .is_some()
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates and mounts a disposable APFS disk image with hdiutil"]
fn detached_volume_retains_transaction_and_remount_retries_cleanup() {
    use std::process::Command;
    struct ImageMount {
        mount: PathBuf,
        attached: bool,
    }
    impl Drop for ImageMount {
        fn drop(&mut self) {
            if self.attached {
                let _ = Command::new("hdiutil")
                    .arg("detach")
                    .arg(&self.mount)
                    .output();
            }
        }
    }
    let image_mount = |image: &Path, mount: &Path| {
        let result = Command::new("hdiutil")
            .arg("attach")
            .arg(image)
            .args(["-nobrowse", "-noautoopen", "-mountpoint"])
            .arg(mount)
            .output()
            .unwrap();
        assert!(result.status.success(), "image attach failed");
    };
    let create_image = |image: &Path, name: &str| {
        let created = Command::new("hdiutil")
            .args(["create", "-size", "64m", "-fs", "APFS", "-volname", name])
            .arg(image)
            .output()
            .unwrap();
        assert!(created.status.success(), "image creation failed");
    };
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("recovery-test.dmg");
    let mount = temp.path().join("mounted");
    fs::create_dir(&mount).unwrap();
    create_image(&image, "RecoveryTest");
    image_mount(&image, &mount);
    let mut guard = ImageMount {
        mount: mount.clone(),
        attached: true,
    };
    let parent = mount.join("work");
    fs::create_dir(&parent).unwrap();
    let original = parent.join("document.txt");
    fs::write(&original, b"unchanged").unwrap();
    let parent_device = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&parent).unwrap().dev()
    };
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let (record, transaction) = planned(&mut v, "conversation", "volume-test", &parent, &original);
    fs::create_dir(&transaction).unwrap();
    fs::write(transaction.join("replacement"), b"staged").unwrap();
    v.register_transaction(
        &scope(),
        &record.id,
        inode(&transaction),
        Some(inode(&transaction.join("replacement"))),
    )
    .unwrap();
    drop(v);
    let detached = Command::new("hdiutil")
        .arg("detach")
        .arg(&mount)
        .output()
        .unwrap();
    assert!(detached.status.success(), "image detach failed");
    guard.attached = false;
    let mut v = vault.lock().unwrap();
    assert!(!v.settle_transaction(&scope(), &record.id).unwrap());
    assert!(
        v.list(&scope(), Some("conversation"))[0]
            .transaction
            .is_some()
    );
    drop(v);
    // Occupy the former device number before remounting the original volume.
    let blocker_image = temp.path().join("blocker.dmg");
    let blocker_mount = temp.path().join("blocker");
    fs::create_dir(&blocker_mount).unwrap();
    create_image(&blocker_image, "RecoveryBlocker");
    image_mount(&blocker_image, &blocker_mount);
    let _blocker_guard = ImageMount {
        mount: blocker_mount,
        attached: true,
    };
    image_mount(&image, &mount);
    guard.attached = true;
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(&parent).unwrap().dev(),
            parent_device,
            "test must exercise a reassigned device number"
        );
    }
    let mut v = vault.lock().unwrap();
    assert!(
        v.settle_transaction(&scope(), &record.id).unwrap(),
        "remounted transaction must remain safely addressable"
    );
    assert!(!transaction.exists());
    assert_eq!(fs::read(&original).unwrap(), b"unchanged");
}

#[cfg(unix)]
#[test]
fn private_persistent_backup() {
    use std::os::unix::fs::PermissionsExt;
    let (root, vault) = fixture();
    let record = save(&mut vault.lock().unwrap(), "s", "a");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root.path().join("file-recovery")), 0o700);
    let directory = namespace_dir(root.path(), &scope());
    for path in [
        root.path().join("file-recovery/objects"),
        directory.clone(),
        directory.join("tmp"),
    ] {
        assert_eq!(mode(&path), 0o700);
    }
    for (name, _) in objects(root.path(), &scope()) {
        assert_eq!(mode(&directory.join(name)), 0o600);
    }
    assert_eq!(mode(&root.path().join("file-recovery/index.json")), 0o600);
    // BackupReady is written by the next ledger update; without one the
    // durable Preparing record recovers as aborted and its objects are swept.
    let mut reopened = vault.lock().unwrap();
    assert_eq!(
        reopened.record(&scope(), &record.id).unwrap().change,
        ChangeState::Preparing
    );
    reopened.recover_interrupted(1001).unwrap();
    assert_eq!(
        reopened.record(&scope(), &record.id).unwrap().material,
        MaterialState::Purged
    );
    assert!(objects(root.path(), &scope()).is_empty());
}

#[test]
fn domains_devices_users_and_owners_are_isolated() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let first = save(&mut v, "s", "a");
    commit(&mut v, &first);
    for field in 0..4 {
        let mut other = scope();
        match field {
            0 => other.authority = "b".into(),
            1 => other.device = "b".into(),
            2 => other.os_user = "b".into(),
            _ => other.owner = "b".into(),
        }
        assert_ne!(other.namespace(), scope().namespace());
        assert!(v.entry_material(&other, "s", &first.id, 0, 1001).is_err());
        v.delete_conversation(&other, "s", 1002).unwrap();
        assert_eq!(before_of(&v, &first), b"before");
    }
}

#[test]
fn duplicate_operation_and_invalid_transitions_are_rejected() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    let mut duplicate = edit("s", "a", b"new", b"newer");
    duplicate.generation = "new-generation";
    assert_eq!(
        v.backup(duplicate, 0, no_quota).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(
        v.transition(&scope(), &r.id, ChangeState::Succeeded)
            .is_err()
    );
    v.transition(&scope(), &r.id, ChangeState::CommitIntent)
        .unwrap();
    drop(v);
    let mut v = vault.lock().unwrap();
    assert_eq!(v.list(&scope(), None)[0].change, ChangeState::CommitIntent);
    assert!(
        v.finish(&scope(), &r.id, ChangeState::CommitIntent)
            .is_err()
    );
    v.finish(&scope(), &r.id, ChangeState::Succeeded).unwrap();
    assert!(
        v.transition(&scope(), &r.id, ChangeState::CommitIntent)
            .is_err()
    );
    // An aborted change no longer holds its unwritten `after` object.
    let aborted = save(&mut v, "s", "b");
    v.transition(&scope(), &aborted.id, ChangeState::Aborted)
        .unwrap();
    assert!(v.list(&scope(), None).iter().any(|record| record.id == aborted.id
        && record.inline_entry().unwrap().after.is_none()));
}

#[test]
fn local_capacity_rejects_backups_without_evicting_unexpired_material() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    v.set_policy(Policy {
        retention_days: 7,
        max_bytes: 1024 * 1024,
    })
    .unwrap();
    let noisy = |seed: u64| {
        let mut state = seed;
        (0..1_200_000)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                b"0123456789abcdef"[(state >> 60) as usize]
            })
            .collect::<Vec<u8>>()
    };
    let first = noisy(1);
    let record = v
        .backup(edit("s", "first", &first, b"x"), 0, no_quota)
        .unwrap();
    commit(&mut v, &record);
    let before = v.used_bytes();
    let second = noisy(2);
    assert_eq!(
        v.backup(edit("s", "overflow", &second, b"y"), 0, no_quota)
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(v.used_bytes(), before);
    assert_eq!(v.list(&scope(), None).len(), 1);
}

#[test]
fn retention_uses_original_timestamp_and_preserves_unknown_commit() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let done = save(&mut v, "s", "done");
    let unknown = save(&mut v, "s", "unknown");
    commit(&mut v, &done);
    v.transition(&scope(), &unknown.id, ChangeState::CommitIntent)
        .unwrap();
    v.set_policy(Policy {
        retention_days: 1,
        max_bytes: 1024 * 1024,
    })
    .unwrap();
    let expiry = 1000 + 86_400_000;
    v.cleanup(expiry - 1).unwrap();
    assert!(
        v.entry_material(&scope(), "s", &done.id, 0, expiry - 1)
            .is_ok()
    );
    v.cleanup(expiry).unwrap();
    let rows = v.list(&scope(), None);
    assert_eq!(
        rows.iter().find(|r| r.id == done.id).unwrap().material,
        MaterialState::Purged
    );
    assert_eq!(
        rows.iter().find(|r| r.id == unknown.id).unwrap().material,
        MaterialState::Saved
    );
}

#[cfg(unix)]
#[test]
fn object_links_are_never_followed() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "s", "a");
    commit(&mut v, &record);
    let outside = root.path().join("user-file");
    fs::write(&outside, b"keep").unwrap();
    let before = namespace_dir(root.path(), &scope()).join(hash(b"before"));
    fs::remove_file(&before).unwrap();
    std::os::unix::fs::symlink(&outside, &before).unwrap();
    assert!(
        v.entry_material(&scope(), "s", &record.id, 0, 1001)
            .is_err()
    );
    v.delete_conversation(&scope(), "s", 2000).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
    assert!(fs::symlink_metadata(&before).is_err());
    assert!(objects(root.path(), &scope()).is_empty());
}

#[test]
fn other_ledger_formats_are_wiped_and_corruption_is_not_recreated() {
    let (root, vault) = fixture();
    let record = save(&mut vault.lock().unwrap(), "s", "a");
    let index = root.path().join("file-recovery/index.json");
    let previous_instance = vault.lock().unwrap().vault_instance_id().to_owned();
    fs::write(&index, b"broken").unwrap();
    let error = vault.lock().err().unwrap();
    assert!(
        error.to_string().contains("could not be decoded"),
        "{error}"
    );
    fs::write(&index, br#"{"format_version":4}"#).unwrap();
    let error = vault.lock().err().unwrap();
    assert!(error.to_string().contains("missing field"), "{error}");
    assert_eq!(fs::read(&index).unwrap(), br#"{"format_version":4}"#);
    // The previous storage format and its material are replaced.
    fs::write(&index, br#"{"format_version":3,"records":{}}"#).unwrap();
    let legacy = root
        .path()
        .join(format!("file-recovery/{}.body", "a".repeat(64)));
    fs::write(&legacy, b"old body").unwrap();
    let v = vault.lock().unwrap();
    assert!(v.list(&scope(), None).is_empty());
    assert_ne!(v.vault_instance_id(), previous_instance);
    assert!(!legacy.exists());
    assert!(objects(root.path(), &scope()).is_empty());
    assert!(v.record(&scope(), &record.id).is_none());
}

#[test]
fn backup_io_failure_is_persisted_as_aborted_before_commit() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    fs::write(
        root.path().join("file-recovery/objects"),
        b"not a directory",
    )
    .unwrap();
    assert!(
        v.backup(edit("s", "a", b"old", b"new"), 0, no_quota)
            .is_err()
    );
    drop(v);
    let rows = vault.lock().unwrap().list(&scope(), Some("s"));
    assert_eq!(rows[0].change, ChangeState::Aborted);
}
