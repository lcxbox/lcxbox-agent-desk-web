use super::*;

fn no_quota(_: &Record, _: u64) -> io::Result<()> {
    Ok(())
}

fn scope() -> Scope {
    Scope {
        authority: "central-a".into(),
        device: "device-a".into(),
        os_user: "user-a".into(),
        owner: "owner-a".into(),
    }
}

/// A batch published by an unfinished scan is attached to the record before
/// the lock is released, so a sweep by another worker between batches keeps
/// its objects, including a delta's base.
#[test]
fn a_sweep_between_batches_keeps_published_batch_objects() {
    let data = tempfile::tempdir().unwrap();
    let vault = Vault::open(data.path()).unwrap();
    let executor = vault.start_executor().unwrap();
    let tree = tempfile::tempdir().unwrap();
    let base: Vec<u8> = (0..4000)
        .flat_map(|line| format!("line {line} of the original file\n").into_bytes())
        .collect();
    std::fs::write(tree.path().join("a.txt"), &base).unwrap();
    let root = File::open(tree.path()).unwrap();
    let request = SnapshotRequest {
        scope: scope(),
        conversation: "c",
        operation: "op",
        generation: "g",
        executor: &executor,
        roots: vec![SnapshotRoot {
            directory: &root,
            directory_identity: "root",
        }],
        expected_epoch: 0,
        now_ms: 1000,
        limits: ScanLimits::default(),
    };
    let SnapshotBegin::Taken(pending) = vault.begin_snapshot(request, no_quota).unwrap() else {
        panic!("snapshot skipped");
    };
    let before = pending.roots[0].manifest.files[0].clone();
    let mut changed = base.clone();
    changed[10..14].copy_from_slice(b"EDIT");
    let sha = hash(&changed);
    let mut files = vec![ManifestFile {
        content: Content::Text {
            sha256: sha.clone(),
            object: None,
        },
        ..before.clone()
    }];
    let mut reserve = no_quota;
    let mut run = Run {
        vault: &vault,
        scope: &pending.scope,
        conversation: &pending.conversation,
        id: &pending.id,
        reserve: &mut reserve,
        reserved: pending.reserved,
        exhausted: false,
        published: Vec::new(),
    };
    run.flush(
        &mut files,
        vec![PendingObject {
            file: 0,
            sha: sha.clone(),
            raw: changed.clone(),
            base: before.content.object().cloned(),
        }],
        false,
    )
    .unwrap();
    let object = files[0].content.object().unwrap().clone();
    assert_eq!(object.bases, vec![hash(&base)]);
    let mut locked = vault.lock().unwrap();
    assert_eq!(
        locked.ledger.records[&pending.id]
            .snapshot
            .as_ref()
            .unwrap()
            .batches
            .len(),
        1
    );
    let report = locked.sweep(2000).unwrap();
    // Only the pre-run scan's batch list, superseded by its manifest.
    assert_eq!(report.removed_objects, 1);
    assert_eq!(
        locked.read_object(&scope().namespace(), &object).unwrap(),
        changed
    );
}
