use super::snapshot::{Tree, begin, finish};
use super::*;
use std::io::Write;

/// The worker side of `restore_command_changes`: check every target's
/// current hash, then back up and write the matching files.
fn restore(
    vault: &Vault,
    tree: &Tree,
    source: &str,
    paths: Option<&[(u32, &str)]>,
    operation: &str,
) -> (Record, Vec<RestoreTarget>, Vec<bool>) {
    let mut v = vault.lock().unwrap();
    let targets = v
        .restore_targets(&scope(), "c", source, paths, 3000)
        .unwrap();
    let mut contents = Vec::new();
    let mut matched = Vec::new();
    for target in &targets {
        let current = fs::read(tree.path(&target.path)).ok();
        if current.as_deref().map(hash) != target.expected_sha256 {
            continue;
        }
        let restored = v
            .entry_material(&scope(), "c", source, target.index, 3000)
            .unwrap()
            .before;
        assert_eq!(restored.as_deref().map(hash), target.restored_sha256);
        contents.push((target.index, current, restored));
        matched.push(target.clone());
    }
    let record = v
        .begin_command_restore(
            RestoreRequest {
                scope: scope(),
                conversation: "c",
                operation,
                generation: "generation",
                source,
                files: contents
                    .iter()
                    .map(|(index, current, restored)| RestoreFile {
                        index: *index,
                        current: current.as_deref(),
                        restored: restored.as_deref(),
                    })
                    .collect(),
                now_ms: 3000,
            },
            0,
            no_quota,
        )
        .unwrap();
    assert_eq!(record.change, ChangeState::BackupReady);
    v.transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    let root = tree.handle();
    let mut applied = Vec::new();
    for (target, (_, _, restored)) in matched.iter().zip(&contents) {
        let path = tree.path(&target.path);
        match (target.kind, restored) {
            (ChangeKind::Added, None) => fs::remove_file(path).unwrap(),
            (ChangeKind::Modified, Some(content)) => fs::write(path, content).unwrap(),
            (ChangeKind::Deleted, Some(content)) => {
                let parent = target.path.rsplit_once('/').map(|(parent, _)| parent);
                if let Some(parent) = parent {
                    create_directories(&root, parent).unwrap();
                }
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .unwrap()
                    .write_all(content)
                    .unwrap();
            }
            other => panic!("unexpected restore shape {other:?}"),
        }
        applied.push(true);
    }
    let finished = v
        .finish_command_restore(&scope(), &record.id, &applied, 3001)
        .unwrap();
    (finished, targets, applied)
}

fn command(vault: &Vault, executor: &Executor, tree: &Tree) -> Record {
    tree.write("a.txt", b"a 1\n");
    tree.write("sub/dir/b.txt", b"b 1\n");
    tree.write("keep.txt", b"keep\n");
    let root = tree.handle();
    let pending = begin(vault, executor, &root, "c", "command");
    tree.write("a.txt", b"a 2\n");
    fs::remove_dir_all(tree.path("sub")).unwrap();
    tree.write("c.txt", b"c 2\n");
    finish(vault, pending).0
}

#[test]
fn restoring_command_changes_recreates_parents_and_can_be_undone() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    let record = command(&vault, &executor, &tree);
    let (restored, targets, _) = restore(&vault, &tree, &record.id, None, "restore");
    assert_eq!(
        targets
            .iter()
            .map(|target| (target.path.as_str(), target.kind))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", ChangeKind::Modified),
            ("c.txt", ChangeKind::Added),
            ("sub/dir/b.txt", ChangeKind::Deleted),
        ]
    );
    assert_eq!(restored.change, ChangeState::Succeeded);
    assert_eq!(restored.kind, RecordKind::CommandRestore);
    assert_eq!(fs::read(tree.path("a.txt")).unwrap(), b"a 1\n");
    assert!(!tree.path("c.txt").exists());
    assert_eq!(fs::read(tree.path("sub/dir/b.txt")).unwrap(), b"b 1\n");
    // The restore is itself restorable, with the inverse changes.
    let (undone, undo_targets, _) = restore(&vault, &tree, &restored.id, None, "undo");
    assert_eq!(
        undo_targets
            .iter()
            .map(|target| (target.path.as_str(), target.kind))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", ChangeKind::Modified),
            ("c.txt", ChangeKind::Deleted),
            ("sub/dir/b.txt", ChangeKind::Added),
        ]
    );
    assert_eq!(undone.entry_count(), 3);
    assert_eq!(fs::read(tree.path("a.txt")).unwrap(), b"a 2\n");
    assert_eq!(fs::read(tree.path("c.txt")).unwrap(), b"c 2\n");
    assert!(!tree.path("sub/dir/b.txt").exists());
}

#[test]
fn selected_paths_and_conflicts_restore_only_matching_files() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    let record = command(&vault, &executor, &tree);
    // Someone changed c.txt after the command: it no longer matches.
    tree.write("c.txt", b"user edit\n");
    let (restored, targets, applied) = restore(
        &vault,
        &tree,
        &record.id,
        Some(&[(0, "a.txt"), (0, "c.txt"), (0, "missing.txt")]),
        "restore",
    );
    assert_eq!(targets.len(), 2);
    assert_eq!(applied, vec![true]);
    assert_eq!(restored.entry_count(), 1);
    assert_eq!(fs::read(tree.path("a.txt")).unwrap(), b"a 1\n");
    assert_eq!(fs::read(tree.path("c.txt")).unwrap(), b"user edit\n");
    assert!(!tree.path("sub").exists());
}

#[test]
fn skipped_writes_are_dropped_from_the_restore_record() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    let record = command(&vault, &executor, &tree);
    let mut v = vault.lock().unwrap();
    let targets = v
        .restore_targets(&scope(), "c", &record.id, None, 3000)
        .unwrap();
    let a = fs::read(tree.path("a.txt")).unwrap();
    let a_before = v
        .entry_material(&scope(), "c", &record.id, targets[0].index, 3000)
        .unwrap()
        .before
        .unwrap();
    let c = fs::read(tree.path("c.txt")).unwrap();
    let files = vec![
        RestoreFile {
            index: targets[0].index,
            current: Some(&a),
            restored: Some(&a_before),
        },
        RestoreFile {
            index: targets[1].index,
            current: Some(&c),
            restored: None,
        },
    ];
    let request = |files| RestoreRequest {
        scope: scope(),
        conversation: "c",
        operation: "restore",
        generation: "generation",
        source: &record.id,
        files,
        now_ms: 3000,
    };
    // A file's shape must match its recorded change.
    let wrong = vec![RestoreFile {
        index: targets[1].index,
        current: None,
        restored: Some(b"x"),
    }];
    assert!(
        v.begin_command_restore(request(wrong), 0, no_quota)
            .is_err()
    );
    let record = v
        .begin_command_restore(request(files), 0, no_quota)
        .unwrap();
    v.transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    let finished = v
        .finish_command_restore(&scope(), &record.id, &[false, true], 3001)
        .unwrap();
    assert_eq!(finished.change, ChangeState::Succeeded);
    let page = v.query_entries(&scope(), "c", &record.id, 0).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].rel_path, "c.txt");
    assert_eq!(page.entries[0].change, Some(ChangeKind::Deleted));
    // Nothing applied: the record is aborted.
    let other = v
        .begin_command_restore(
            RestoreRequest {
                operation: "restore-2",
                ..request(vec![RestoreFile {
                    index: targets[0].index,
                    current: Some(&a),
                    restored: Some(&a_before),
                }])
            },
            0,
            no_quota,
        )
        .unwrap();
    v.transition(&scope(), &other.id, ChangeState::CommitIntent)
        .unwrap();
    let aborted = v
        .finish_command_restore(&scope(), &other.id, &[false], 3002)
        .unwrap();
    assert_eq!(aborted.change, ChangeState::Aborted);
}

#[test]
fn creating_restore_parents_never_follows_links() {
    let tree = Tree::new();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), tree.path("escape")).unwrap();
    let root = tree.handle();
    assert!(create_directories(&root, "escape/inner").is_err());
    assert!(!outside.path().join("inner").exists());
    tree.write("file", b"x");
    assert!(create_directories(&root, "file/inner").is_err());
    for invalid in ["", "/abs", "a/../b", "a//b"] {
        assert!(create_directories(&root, invalid).is_err());
    }
    let created = create_directories(&root, "new/nested/dir").unwrap();
    assert!(created.metadata().unwrap().is_dir());
    assert!(tree.path("new/nested/dir").is_dir());
    // Existing directories are reused.
    create_directories(&root, "new/nested").unwrap();
}
