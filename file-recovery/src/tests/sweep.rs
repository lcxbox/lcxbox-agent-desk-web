use super::*;
use std::time::Instant;

#[test]
fn sweep_keeps_base_chains_of_live_records_after_older_records_are_purged() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let mut current = encoding::text(1500, 5);
    let mut records = Vec::new();
    for step in 0..4 {
        let mut next = current.clone();
        next[step * 40..step * 40 + 4].copy_from_slice(b"EDIT");
        let record = v
            .backup(
                edit(&format!("c{step}"), "op", &current, &next),
                0,
                no_quota,
            )
            .unwrap();
        commit(&mut v, &record);
        records.push((record, current));
        current = next;
    }
    let (last, last_before) = records.pop().unwrap();
    assert!(
        !last
            .inline_entry()
            .unwrap()
            .before
            .as_ref()
            .unwrap()
            .bases
            .is_empty()
    );
    for (record, _) in &records {
        v.delete_conversation(&scope(), &record.conversation, 2000)
            .unwrap();
    }
    // Only the chain of the remaining record survives: the full root, three
    // deltas up to its before, its after and the shared metadata.
    assert_eq!(objects(root.path(), &scope()).len(), 6);
    assert_eq!(before_of(&v, &last), last_before);
}

#[test]
fn deleting_a_conversation_removes_only_objects_no_other_conversation_holds() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let shared = v
        .backup(
            edit("keep", "op", b"shared before", b"kept after"),
            0,
            no_quota,
        )
        .unwrap();
    commit(&mut v, &shared);
    let deleted = v
        .backup(
            edit("drop", "op", b"shared before", b"dropped after"),
            0,
            no_quota,
        )
        .unwrap();
    commit(&mut v, &deleted);
    let mut other_scope = scope();
    other_scope.owner = "owner-b".into();
    let mut foreign = edit("drop", "op", b"shared before", b"dropped after");
    foreign.scope = other_scope.clone();
    let foreign = v.backup(foreign, 0, no_quota).unwrap();
    assert_eq!(objects(root.path(), &scope()).len(), 4);
    v.delete_conversation(&scope(), "drop", 2000).unwrap();
    let names: Vec<_> = objects(root.path(), &scope())
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names.len(), 3);
    assert!(!names.contains(&hash(b"dropped after")));
    assert!(names.contains(&hash(b"shared before")));
    assert_eq!(before_of(&v, &shared), b"shared before");
    // Namespaces are isolated: the other owner's copy is untouched.
    assert_eq!(objects(root.path(), &other_scope).len(), 3);
    assert!(v.record(&other_scope, &foreign.id).is_some());
    assert!(v.list(&scope(), Some("drop")).is_empty());
    assert_eq!(v.list(&scope(), Some("keep")).len(), 1);
}

#[test]
fn cleanup_completes_after_the_device_quota_accepts_post_sweep_usage() {
    let (_root, vault) = fixture();
    let quota_root = tempfile::tempdir().unwrap();
    let device = quota::DeviceQuota::open(quota_root.path()).unwrap();
    let mut quota = device.lock().unwrap();
    let mut v = vault.lock().unwrap();
    let record = v
        .backup(edit("c", "op", b"a", b"b"), 0, |record, bytes| {
            quota.reserve(&quota::QuotaKey::for_record(record)?, bytes)
        })
        .unwrap();
    commit(&mut v, &record);
    v.delete_conversation(&scope(), "c", 2000).unwrap();
    assert!(!v.cleanup_complete(&scope(), "c"));
    assert_eq!(v.pending_cleanup_count(), 1);
    // A report from before the deletion does not settle it.
    let stale = quota::UsageReport {
        revision: v.revision() - 1,
        ..v.usage_report()
    };
    v.acknowledge_settlement(&stale).unwrap();
    assert!(!v.cleanup_complete(&scope(), "c"));
    let report = v.usage_report();
    assert!(
        quota
            .settle(
                "user-a",
                Some(&quota::QuotaKey::for_record(&record).unwrap()),
                &report
            )
            .unwrap()
    );
    v.acknowledge_settlement(&report).unwrap();
    assert!(v.cleanup_complete(&scope(), "c"));
    assert_eq!(quota.reserved_bytes(), 0);
    assert_eq!(quota.used_bytes(), report.total());
    drop(v);
    assert!(vault.lock().unwrap().cleanup_complete(&scope(), "c"));
}

#[test]
fn multi_entry_records_use_an_entry_table_and_page_their_entries() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let contents: Vec<Vec<u8>> = (0..150)
        .map(|index| format!("file {index}\n").into_bytes())
        .collect();
    let names: Vec<String> = (0..150)
        .map(|index| format!("src/file-{index:03}.rs"))
        .collect();
    let entries = names
        .iter()
        .zip(&contents)
        .map(|(name, content)| entry(name, Some(content), None))
        .collect();
    let record = v
        .backup(
            request("c", "snapshot", RecordKind::CommandSnapshot, entries),
            0,
            no_quota,
        )
        .unwrap();
    assert!(matches!(record.entries, Entries::Table { count: 150, .. }));
    let first = v.query_entries(&scope(), "c", &record.id, 0).unwrap();
    assert_eq!(
        (first.total, first.entries.len(), first.next_offset),
        (150, 100, Some(100))
    );
    let last = v.query_entries(&scope(), "c", &record.id, 100).unwrap();
    assert_eq!((last.entries.len(), last.next_offset), (50, None));
    assert_eq!(last.entries[49].rel_path, "src/file-149.rs");
    assert_eq!(last.entries[49].before_sha256, Some(hash(&contents[149])));
    assert!(v.query_entries(&scope(), "c", &record.id, 151).is_err());
    let material = v
        .entry_material(&scope(), "c", &record.id, 120, 1001)
        .unwrap();
    assert_eq!(material.before.unwrap(), contents[120]);
    // The table, every before object and the shared metadata are live.
    v.flush().unwrap();
    v.sweep(1000).unwrap();
    assert_eq!(objects(root.path(), &scope()).len(), 152);
}

/// Measures the lock hold time of one sweep over a large object set. Live
/// objects are written directly: only listing, marking and removal are timed.
#[test]
fn sweep_lock_time_for_a_large_object_set_is_measured() {
    const LIVE: usize = 20_000;
    const ORPHANS: usize = 20_000;
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = v
        .backup(
            request(
                "c",
                "snapshot",
                RecordKind::CommandSnapshot,
                vec![entry("a", Some(b"a"), None), entry("b", Some(b"b"), None)],
            ),
            0,
            no_quota,
        )
        .unwrap();
    let directory = namespace_dir(root.path(), &scope());
    let entries: Vec<Entry> = (0..LIVE)
        .map(|index| {
            let sha = hash(format!("live {index}").as_bytes());
            fs::write(directory.join(&sha), b"x").unwrap();
            Entry {
                rel_path: format!("tree/{index}"),
                before: Some(ObjRef::full(sha)),
                after: None,
                metadata: None,
                lineage: hash(index.to_string().as_bytes()),
            }
        })
        .collect();
    let (table, frame) = LockedVault::entries_storage(entries).unwrap();
    let (sha, frame) = frame.unwrap();
    v.publish(&scope().namespace(), &sha, &frame).unwrap();
    v.ledger.records.get_mut(&record.id).unwrap().entries = table;
    for index in 0..ORPHANS {
        fs::write(
            directory.join(hash(format!("orphan {index}").as_bytes())),
            b"x",
        )
        .unwrap();
    }
    v.flush().unwrap();
    let started = Instant::now();
    let report = v.sweep(1000).unwrap();
    let swept = started.elapsed();
    println!(
        "sweep over {} objects ({} removed): {:?} under lock",
        report.retained_objects + report.removed_objects,
        report.removed_objects,
        swept
    );
    // The two original entry objects, the original table and the shared
    // metadata are no longer referenced.
    assert_eq!(report.removed_objects, ORPHANS as u64 + 4);
    assert_eq!(report.retained_objects, LIVE as u64 + 1);
    assert_eq!(
        v.ledger.namespaces[&scope().namespace()].objects,
        LIVE as u64 + 1
    );
    // A steady-state sweep lists and marks without removing anything.
    let started = Instant::now();
    let report = v.sweep(1000).unwrap();
    println!(
        "sweep over {} live objects (nothing removed): {:?} under lock",
        report.retained_objects,
        started.elapsed()
    );
    assert_eq!(report.removed_objects, 0);
}
