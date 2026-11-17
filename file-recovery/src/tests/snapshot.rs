use super::*;
use quota::{DeviceQuota, QuotaKey};
use std::{
    fs::File,
    time::{Duration, Instant, SystemTime},
};

pub(super) struct Tree {
    pub dir: tempfile::TempDir,
}
impl Tree {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    pub fn write(&self, rel: &str, content: &[u8]) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    /// Move a file's mtime out of the racy window.
    pub fn age(&self, rel: &str) {
        set_mtime(
            &self.path(rel),
            SystemTime::now() - Duration::from_secs(3600),
        );
    }
    pub fn handle(&self) -> File {
        File::open(self.dir.path()).unwrap()
    }
}
pub(super) fn set_mtime(path: &Path, time: SystemTime) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
}
pub(super) fn reads() -> u64 {
    crate::snapshot::content_reads()
}
pub(super) fn snapshot_request<'a>(
    executor: &'a Executor,
    conversation: &'a str,
    operation: &'a str,
    roots: Vec<SnapshotRoot<'a>>,
    limits: ScanLimits,
) -> SnapshotRequest<'a> {
    SnapshotRequest {
        scope: scope(),
        conversation,
        operation,
        generation: "generation",
        executor,
        roots,
        expected_epoch: 0,
        now_ms: 1000,
        limits,
    }
}
pub(super) fn begin(
    vault: &Vault,
    executor: &Executor,
    root: &File,
    conversation: &str,
    operation: &str,
) -> PendingSnapshot {
    let request = snapshot_request(
        executor,
        conversation,
        operation,
        vec![SnapshotRoot {
            directory: root,
            directory_identity: "root-1",
        }],
        ScanLimits::default(),
    );
    match vault.begin_snapshot(request, no_quota).unwrap() {
        SnapshotBegin::Taken(pending) => pending,
        SnapshotBegin::Skipped(reason) => panic!("snapshot skipped: {reason:?}"),
    }
}
pub(super) fn finish(vault: &Vault, pending: PendingSnapshot) -> (Record, Vec<FileChange>) {
    match vault.finish_snapshot(pending, 2000, no_quota).unwrap().1 {
        SnapshotFinish::Taken { record, changes } => (record, changes),
        SnapshotFinish::Skipped(reason) => panic!("snapshot skipped: {reason:?}"),
        SnapshotFinish::Discarded => panic!("snapshot discarded"),
    }
}
fn summary(changes: &[FileChange]) -> Vec<(String, ChangeKind, bool)> {
    changes
        .iter()
        .map(|change| (change.path.clone(), change.kind, change.protected))
        .collect()
}

#[test]
fn a_formatting_command_reports_modified_added_and_deleted_files() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("src/main.rs", b"fn main(){}\n");
    tree.write("src/lib.rs", b"pub fn a(){}\n");
    tree.write("README.md", b"readme\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "fmt");
    tree.write("src/main.rs", b"fn main() {}\n");
    fs::remove_file(tree.path("src/lib.rs")).unwrap();
    tree.write("src/new.rs", b"// new\n");
    let (record, changes) = finish(&vault, pending);
    assert_eq!(record.change, ChangeState::Completed);
    assert_eq!(
        summary(&changes),
        vec![
            ("src/lib.rs".into(), ChangeKind::Deleted, true),
            ("src/main.rs".into(), ChangeKind::Modified, true),
            ("src/new.rs".into(), ChangeKind::Added, true),
        ]
    );
    assert_eq!(changes[1].before_sha256, Some(hash(b"fn main(){}\n")));
    assert_eq!(changes[1].after_sha256, Some(hash(b"fn main() {}\n")));
    let v = vault.lock().unwrap();
    // Before content of the modified file is restorable material.
    let material = v
        .entry_material(&scope(), "c", &record.id, changes[1].index, 2001)
        .unwrap();
    assert_eq!(material.before.unwrap(), b"fn main(){}\n");
    assert_eq!(v.latest_manifest_files(&scope(), "c", "root-1"), Some(3));
    let page = v.query_entries(&scope(), "c", &record.id, 0).unwrap();
    assert_eq!(page.entries[2].change, Some(ChangeKind::Added));
}

#[test]
fn unchanged_stats_are_reused_without_reading_and_changes_are_reread() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    for index in 0..20 {
        let name = format!("file-{index}.txt");
        tree.write(&name, format!("content {index}\n").as_bytes());
        tree.age(&name);
    }
    let root = tree.handle();
    let start = reads();
    let pending = begin(&vault, &executor, &root, "c", "first");
    assert_eq!(reads() - start, 20);
    let start = reads();
    let (_, changes) = finish(&vault, pending);
    assert!(changes.is_empty());
    assert_eq!(
        reads() - start,
        0,
        "the post-run scan reread unchanged files"
    );
    // The next command of the conversation starts from the latest manifest.
    tree.write("file-3.txt", b"changed 3\n");
    tree.age("file-3.txt");
    let start = reads();
    let pending = begin(&vault, &executor, &root, "c", "second");
    assert_eq!(reads() - start, 1);
    finish(&vault, pending);
    // Another conversation has no manifest: it rereads, but objects are
    // shared by content and not written again.
    let objects_before = objects(_root.path(), &scope()).len();
    let start = reads();
    let pending = begin(&vault, &executor, &root, "other", "first");
    assert_eq!(reads() - start, 20);
    finish(&vault, pending);
    let added = objects(_root.path(), &scope()).len() - objects_before;
    // Only manifests, batch lists and the entry table are new.
    assert!(added <= 4, "{added} new objects");
}

#[test]
fn ctime_detects_content_changes_that_preserve_mtime() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("config.txt", b"value = 1\n");
    tree.age("config.txt");
    let original = fs::metadata(tree.path("config.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    // Same size, mtime restored like `touch -r` or `cp -p`.
    tree.write("config.txt", b"value = 2\n");
    set_mtime(&tree.path("config.txt"), original);
    let start = reads();
    let (_, changes) = finish(&vault, pending);
    assert_eq!(reads() - start, 1);
    assert_eq!(
        summary(&changes),
        vec![("config.txt".into(), ChangeKind::Modified, true)]
    );
}

#[test]
fn files_modified_near_the_previous_scan_are_reread() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("old.txt", b"old\n");
    tree.age("old.txt");
    // Within the racy window of the pre-run scan.
    tree.write("recent.txt", b"recent\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    let start = reads();
    let (_, changes) = finish(&vault, pending);
    assert!(changes.is_empty());
    assert_eq!(reads() - start, 1, "only the racy file is reread");
}

#[test]
fn ignore_rules_are_frozen_before_the_command_runs() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write(".gitignore", b"*.log\n");
    tree.write("app.log", b"log\n");
    tree.write("keep.txt", b"keep\n");
    tree.write("sub/.ignore", b"secret.txt\n");
    tree.write("sub/secret.txt", b"secret\n");
    tree.write("sub/visible.txt", b"visible\n");
    tree.write("node_modules/pkg/index.js", b"module\n");
    tree.write(".git/HEAD", b"ref\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    assert_eq!(pending.files(), vec![4]);
    // The command rewrites the ignore rules and touches excluded paths.
    tree.write(".gitignore", b"keep.txt\n");
    tree.write("sub/.ignore", b"");
    tree.write("app.log", b"log 2\n");
    tree.write("sub/secret.txt", b"secret 2\n");
    tree.write("node_modules/pkg/index.js", b"module 2\n");
    tree.write("target/debug/out", b"build\n");
    let (_, changes) = finish(&vault, pending);
    assert_eq!(
        summary(&changes),
        vec![
            (".gitignore".into(), ChangeKind::Modified, true),
            ("sub/.ignore".into(), ChangeKind::Modified, true),
        ]
    );
}

#[test]
fn links_and_multiply_linked_files_are_not_followed_or_saved() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("target.txt"), b"outside\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), tree.path("linked-dir")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("target.txt"), tree.path("link.txt")).unwrap();
    tree.write("plain.txt", b"plain\n");
    tree.write("hard.txt", b"hard\n");
    fs::hard_link(tree.path("hard.txt"), outside.path().join("hard-copy.txt")).unwrap();
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    assert_eq!(pending.files(), vec![1]);
    fs::write(outside.path().join("target.txt"), b"changed\n").unwrap();
    let (_, changes) = finish(&vault, pending);
    assert!(changes.is_empty());
}

#[test]
fn binary_and_oversized_files_are_compared_but_not_protected() {
    let (root_dir, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("image.bin", &[0, 1, 2, 3]);
    let large = vec![b'a'; MAX_TEXT_BYTES + 1];
    tree.write("large.txt", &large);
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    tree.write("image.bin", &[0, 1, 2, 4]);
    let mut larger = large.clone();
    larger.push(b'b');
    tree.write("large.txt", &larger);
    let (_, changes) = finish(&vault, pending);
    assert_eq!(
        summary(&changes),
        vec![
            ("image.bin".into(), ChangeKind::Modified, false),
            ("large.txt".into(), ChangeKind::Modified, false),
        ]
    );
    assert!(changes[0].after_sha256.is_some());
    assert_eq!(changes[1].after_sha256, None);
    // Neither content was stored.
    let v = vault.lock().unwrap();
    for (name, _) in objects(root_dir.path(), &scope()) {
        let manifest = v.read_manifest(&scope().namespace(), &name);
        if let Ok(manifest) = manifest {
            assert!(manifest.objects().next().is_none());
        }
    }
}

#[test]
fn scan_limits_skip_the_snapshot() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    for index in 0..5 {
        tree.write(&format!("{index}.txt"), b"x\n");
    }
    let root = tree.handle();
    let roots = || {
        vec![SnapshotRoot {
            directory: &root,
            directory_identity: "root-1",
        }]
    };
    let few = ScanLimits {
        max_files: 3,
        ..ScanLimits::default()
    };
    let request = snapshot_request(&executor, "c", "many", roots(), few);
    assert!(matches!(
        vault.begin_snapshot(request, no_quota).unwrap(),
        SnapshotBegin::Skipped(SkipReason::TooManyFiles)
    ));
    let instant = ScanLimits {
        max_duration: Duration::ZERO,
        ..ScanLimits::default()
    };
    let request = snapshot_request(&executor, "c", "slow", roots(), instant);
    assert!(matches!(
        vault.begin_snapshot(request, no_quota).unwrap(),
        SnapshotBegin::Skipped(SkipReason::TimeLimit)
    ));
    // The command may push the post-run scan over the limit, too.
    let six = ScanLimits {
        max_files: 6,
        ..ScanLimits::default()
    };
    let request = snapshot_request(&executor, "c", "grows", roots(), six);
    let SnapshotBegin::Taken(pending) = vault.begin_snapshot(request, no_quota).unwrap() else {
        panic!("snapshot skipped");
    };
    tree.write("5.txt", b"x\n");
    tree.write("6.txt", b"x\n");
    let (v, finished) = vault.finish_snapshot(pending, 2000, no_quota).unwrap();
    assert!(matches!(
        finished,
        SnapshotFinish::Skipped(SkipReason::TooManyFiles)
    ));
    let records = v.list(&scope(), Some("c"));
    assert_eq!(records.len(), 3);
    assert!(
        records
            .iter()
            .all(|record| record.change == ChangeState::Aborted)
    );
    assert_eq!(v.latest_manifest_files(&scope(), "c", "root-1"), None);
}

#[test]
fn insufficient_quota_stores_no_after_objects_and_rereads_them_next_time() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a 1\n");
    tree.age("a.txt");
    tree.write("b.txt", b"b 1\n");
    tree.age("b.txt");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    tree.write("a.txt", b"a 2\n");
    tree.age("a.txt");
    // The after batch is refused; the final publication still fits.
    let mut calls = 0;
    let (_, finished) = vault
        .finish_snapshot(pending, 2000, |_, _| {
            calls += 1;
            if calls == 1 {
                Err(io::Error::new(io::ErrorKind::StorageFull, "quota"))
            } else {
                Ok(())
            }
        })
        .unwrap();
    let SnapshotFinish::Taken { changes, record } = finished else {
        panic!("snapshot not taken");
    };
    assert_eq!(
        summary(&changes),
        vec![("a.txt".into(), ChangeKind::Modified, false)]
    );
    assert_eq!(changes[0].after_sha256, Some(hash(b"a 2\n")));
    // The before content is still saved for export.
    let v = vault.lock().unwrap();
    assert_eq!(
        v.entry_material(&scope(), "c", &record.id, 0, 2001)
            .unwrap()
            .before
            .unwrap(),
        b"a 1\n"
    );
    drop(v);
    // The file without an object is reread although its stat is unchanged.
    let start = reads();
    let pending = begin(&vault, &executor, &root, "c", "next");
    assert_eq!(reads() - start, 1);
    finish(&vault, pending);
}

#[test]
fn a_vault_without_room_degrades_instead_of_failing() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a 1\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    let used = vault.lock().unwrap().used_bytes();
    let mut v = vault.lock().unwrap();
    v.set_policy(Policy {
        retention_days: 7,
        max_bytes: 1024 * 1024,
    })
    .unwrap();
    drop(v);
    assert!(used < 1024 * 1024);
    // A new 1.9 MiB text cannot be stored next to the existing material.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let big: Vec<u8> = (0..1900 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            b'a' + (state % 26) as u8
        })
        .collect();
    tree.write("a.txt", &big);
    let (_, changes) = finish(&vault, pending);
    assert_eq!(
        summary(&changes),
        vec![("a.txt".into(), ChangeKind::Modified, false)]
    );
}

#[test]
fn a_crashed_executor_leaves_an_exportable_unknown_outcome() {
    let (_root, vault) = fixture();
    let alive = vault.start_executor().unwrap();
    let crashed = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a\n");
    let root = tree.handle();
    let running = begin(&vault, &alive, &root, "c", "running");
    let lost = begin(&vault, &crashed, &root, "c", "lost");
    let lost_id = lost.record_id().to_owned();
    let crashed_id = crashed.id().to_owned();
    // The worker process dies: its pending state and executor lock are gone.
    drop(lost);
    drop(crashed);
    let mut v = vault.lock().unwrap();
    let unknown = v.recover_interrupted(3000).unwrap();
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].id, lost_id);
    assert_eq!(
        v.record(&scope(), running.record_id()).unwrap().change,
        ChangeState::SnapshotPending
    );
    // Export of the pre-run material works; restore does not.
    let chunk = v
        .export_chunk(&mut ExportCache::new(), &scope(), "c", &lost_id, 0, 0, 3000)
        .unwrap();
    assert!(chunk.total_size > 0);
    assert_eq!(
        v.restore_targets(&scope(), "c", &lost_id, None, 3000)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    // The exited executor's lock file was removed; the live one remains.
    assert!(
        !_root
            .path()
            .join(format!("file-recovery/executors/{crashed_id}.lock"))
            .exists()
    );
    assert!(
        _root
            .path()
            .join(format!("file-recovery/executors/{}.lock", alive.id()))
            .exists()
    );
    drop(v);
    // The running command still completes.
    tree.write("a.txt", b"b\n");
    let (record, changes) = finish(&vault, running);
    assert_eq!(record.change, ChangeState::Completed);
    assert_eq!(changes.len(), 1);
}

#[test]
fn periodic_cleanup_marks_only_exited_executors() {
    let (_root, vault) = fixture();
    let first = vault.start_executor().unwrap();
    let second = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a\n");
    let root = tree.handle();
    let one = begin(&vault, &first, &root, "c", "one");
    let two = begin(&vault, &second, &root, "c", "two");
    let mut v = vault.lock().unwrap();
    v.cleanup(3000).unwrap();
    assert_eq!(v.unknown_outcome_count(None), 0);
    drop(v);
    drop(second);
    let mut v = vault.lock().unwrap();
    v.cleanup(3001).unwrap();
    assert_eq!(
        v.record(&scope(), two.record_id()).unwrap().change,
        ChangeState::OutcomeUnknown
    );
    assert_eq!(
        v.record(&scope(), one.record_id()).unwrap().change,
        ChangeState::SnapshotPending
    );
    drop(v);
    // A finish after maintenance marked the run publishes nothing.
    let (_, finished) = vault.finish_snapshot(two, 3002, no_quota).unwrap();
    assert!(matches!(finished, SnapshotFinish::Discarded));
    finish(&vault, one);
}

#[test]
fn a_sweep_during_a_pending_snapshot_keeps_its_objects() {
    let (root_dir, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"original a\n");
    tree.write("b.txt", b"original b\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    let before = objects(root_dir.path(), &scope()).len();
    let report = vault.lock().unwrap().sweep(5000).unwrap();
    // Only the batch list superseded by the pre-run manifest is garbage.
    assert_eq!(report.removed_objects, 1);
    assert_eq!(objects(root_dir.path(), &scope()).len(), before - 1);
    tree.write("a.txt", b"changed a\n");
    let (record, changes) = finish(&vault, pending);
    let mut v = vault.lock().unwrap();
    v.sweep(6000).unwrap();
    let material = v
        .entry_material(&scope(), "c", &record.id, changes[0].index, 6001)
        .unwrap();
    assert_eq!(material.before.unwrap(), b"original a\n");
}

#[test]
fn overlapping_roots_run_concurrently_against_their_own_baselines() {
    let (_root, vault) = fixture();
    let vault = std::sync::Arc::new(vault);
    let tree = std::sync::Arc::new(Tree::new());
    tree.write("top.txt", b"top\n");
    tree.write("sub/inner.txt", b"inner\n");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let run = |operation: &'static str, rel: &'static str, file: &'static str| {
        let (vault, tree, barrier) = (vault.clone(), tree.clone(), barrier.clone());
        std::thread::spawn(move || {
            let executor = vault.start_executor().unwrap();
            let root = File::open(tree.path(rel)).unwrap();
            let request = snapshot_request(
                &executor,
                "c",
                operation,
                vec![SnapshotRoot {
                    directory: &root,
                    directory_identity: rel,
                }],
                ScanLimits::default(),
            );
            let SnapshotBegin::Taken(pending) = vault.begin_snapshot(request, no_quota).unwrap()
            else {
                panic!("snapshot skipped");
            };
            barrier.wait();
            tree.write(file, operation.as_bytes());
            barrier.wait();
            finish(&vault, pending).1
        })
    };
    let outer = run("outer", ".", "top.txt");
    let inner = run("inner", "sub", "sub/inner.txt");
    let outer = outer.join().unwrap();
    let inner = inner.join().unwrap();
    // Each run sees every change under its root during its own run window.
    assert_eq!(
        summary(&outer),
        vec![
            ("sub/inner.txt".into(), ChangeKind::Modified, true),
            ("top.txt".into(), ChangeKind::Modified, true),
        ]
    );
    assert_eq!(
        summary(&inner),
        vec![("inner.txt".into(), ChangeKind::Modified, true)]
    );
    let v = vault.lock().unwrap();
    assert_eq!(v.latest_manifest_files(&scope(), "c", "."), Some(2));
    assert_eq!(v.latest_manifest_files(&scope(), "c", "sub"), Some(1));
}

#[test]
fn deleting_the_conversation_during_a_pending_snapshot_publishes_nothing() {
    let (root_dir, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    fs::create_dir(root_dir.path().join("daemon")).unwrap();
    let daemon = DeviceQuota::open(&root_dir.path().join("daemon")).unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a\n");
    let root = tree.handle();
    let request = snapshot_request(
        &executor,
        "c",
        "op",
        vec![SnapshotRoot {
            directory: &root,
            directory_identity: "root-1",
        }],
        ScanLimits::default(),
    );
    let reserve = |record: &Record, bytes: u64| {
        daemon
            .lock()
            .unwrap()
            .reserve(&QuotaKey::for_record(record)?, bytes)
    };
    let SnapshotBegin::Taken(pending) = vault.begin_snapshot(request, reserve).unwrap() else {
        panic!("snapshot skipped");
    };
    assert!(daemon.lock().unwrap().reserved_bytes() > 0);
    let mut v = vault.lock().unwrap();
    v.delete_conversation(&scope(), "c", 3000).unwrap();
    drop(v);
    tree.write("a.txt", b"b\n");
    let (mut v, finished) = vault.finish_snapshot(pending, 4000, reserve).unwrap();
    assert!(matches!(finished, SnapshotFinish::Discarded));
    assert!(v.list(&scope(), Some("c")).is_empty());
    assert_eq!(v.latest_manifest_files(&scope(), "c", "root-1"), None);
    assert!(objects(root_dir.path(), &scope()).is_empty());
    // The caller clears the reservation under the returned guard.
    let report = v.usage_report();
    let key = QuotaKey::new(&scope(), "c", "op", 0).unwrap();
    daemon
        .lock()
        .unwrap()
        .settle("user-a", Some(&key), &report)
        .unwrap();
    v.acknowledge_settlement(&report).unwrap();
    assert_eq!(daemon.lock().unwrap().reserved_bytes(), 0);
    assert!(v.cleanup_complete(&scope(), "c"));
}

#[test]
fn deleting_a_conversation_removes_its_latest_manifests() {
    let (root_dir, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a\n");
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    finish(&vault, pending);
    let mut v = vault.lock().unwrap();
    assert!(!v.cleanup_complete(&scope(), "c"));
    v.delete_conversation(&scope(), "c", 3000).unwrap();
    assert_eq!(v.latest_manifest_files(&scope(), "c", "root-1"), None);
    assert!(objects(root_dir.path(), &scope()).is_empty());
    let report = v.usage_report();
    v.acknowledge_settlement(&report).unwrap();
    assert!(v.cleanup_complete(&scope(), "c"));
}

#[test]
fn epoch_cleanup_waits_for_pending_snapshots() {
    let (root_dir, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    fs::create_dir(root_dir.path().join("daemon")).unwrap();
    let daemon = DeviceQuota::open(&root_dir.path().join("daemon")).unwrap();
    let tree = Tree::new();
    tree.write("a.txt", b"a\n");
    let root = tree.handle();
    let reserve = |record: &Record, bytes: u64| {
        daemon
            .lock()
            .unwrap()
            .reserve(&QuotaKey::for_record(record)?, bytes)
    };
    let request = snapshot_request(
        &executor,
        "c",
        "op",
        vec![SnapshotRoot {
            directory: &root,
            directory_identity: "root-1",
        }],
        ScanLimits::default(),
    );
    let SnapshotBegin::Taken(pending) = vault.begin_snapshot(request, reserve).unwrap() else {
        panic!("snapshot skipped");
    };
    let mut v = vault.lock().unwrap();
    let mut quota = daemon.lock().unwrap();
    assert_eq!(
        v.maintain_epoch_indexes("user-a", &mut quota, 0)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    // A reconciliation keeps the running snapshot's reservation.
    v.reconcile("user-a", &mut quota).unwrap();
    assert!(quota.reserved_bytes() > 0);
    drop(quota);
    drop(v);
    tree.write("a.txt", b"b\n");
    let (mut v, finished) = vault.finish_snapshot(pending, 2000, reserve).unwrap();
    assert!(matches!(finished, SnapshotFinish::Taken { .. }));
    let mut quota = daemon.lock().unwrap();
    v.maintain_epoch_indexes("user-a", &mut quota, 0).unwrap();
    assert_eq!(v.execution_epoch(), 1);
}

#[test]
fn text_backups_keep_objects_held_only_by_a_manifest() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    let base = encoding::text(800, 3);
    let mut held = base.clone();
    held[100..104].copy_from_slice(b"HELD");
    tree.write("notes.txt", &held);
    let root = tree.handle();
    let pending = begin(&vault, &executor, &root, "c", "op");
    finish(&vault, pending);
    // The edit's after equals the manifest's object; the encoder plans a
    // delta, but the complete full object is kept as it is.
    let mut v = vault.lock().unwrap();
    let record = v
        .backup(edit("t", "edit", &base, &held), 0, no_quota)
        .unwrap();
    let after = record.inline_entry().unwrap().after.clone().unwrap();
    assert!(after.bases.is_empty());
    v.sweep(5000).unwrap();
    let pointer = v.ledger.manifests.values().next().unwrap().clone();
    let manifest = v
        .read_manifest(&scope().namespace(), &pointer.sha256)
        .unwrap();
    let object = manifest.objects().next().unwrap().clone();
    assert_eq!(v.read_object(&scope().namespace(), &object).unwrap(), held);
}

/// Scan time over a tree of 20,000 small text files and sweep lock time with
/// a pending snapshot holding them. Run in release with `--ignored`.
#[test]
#[ignore = "measurement"]
fn snapshot_scan_and_sweep_times_for_a_large_tree_are_measured() {
    let (_root, vault) = fixture();
    let executor = vault.start_executor().unwrap();
    let tree = Tree::new();
    for index in 0..20_000 {
        tree.write(
            &format!("dir-{}/file-{index}.rs", index % 200),
            format!("// file {index}\nfn f{index}() {{}}\n").as_bytes(),
        );
    }
    let root = tree.handle();
    let started = Instant::now();
    // Within the default scan limits.
    let pending = begin(&vault, &executor, &root, "c", "first");
    println!(
        "first pre-run scan of 20,000 files: {:?}",
        started.elapsed()
    );
    let started = Instant::now();
    let mut v = vault.lock().unwrap();
    let report = v.sweep(5000).unwrap();
    println!(
        "sweep with a pending snapshot ({} objects): {:?} under lock",
        report.retained_objects,
        started.elapsed()
    );
    drop(v);
    tree.write("dir-0/file-0.rs", b"// changed\n");
    let started = Instant::now();
    let (_, changes) = finish(&vault, pending);
    println!("post-run scan and publish: {:?}", started.elapsed());
    assert_eq!(changes.len(), 1);
    let started = Instant::now();
    let pending = begin(&vault, &executor, &root, "c", "second");
    println!("pre-run scan reusing the manifest: {:?}", started.elapsed());
    finish(&vault, pending);
    let started = Instant::now();
    vault.lock().unwrap().sweep(6000).unwrap();
    println!(
        "sweep with a latest manifest: {:?} under lock",
        started.elapsed()
    );
}

#[test]
fn a_failed_publication_batch_renames_nothing_and_leaves_no_temporaries() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let namespace = scope().namespace();
    let frame = |raw: &[u8]| (hash(raw), object::encode_full(raw).unwrap());
    let invalid = vec![frame(b"a"), ("not-an-object-name".into(), vec![1])];
    assert!(v.publish_batch(&namespace, &invalid).is_err());
    assert!(objects(root.path(), &scope()).is_empty());
    let temporary = namespace_dir(root.path(), &scope()).join("tmp");
    assert_eq!(fs::read_dir(&temporary).unwrap().count(), 0);
    let written = v
        .publish_batch(&namespace, &[frame(b"a"), frame(b"b")])
        .unwrap();
    assert_eq!(written, 2 * BLOCK_BYTES);
    assert_eq!(objects(root.path(), &scope()).len(), 2);
    assert_eq!(v.ledger.namespaces[&namespace].objects, 2);
    assert_eq!(fs::read_dir(&temporary).unwrap().count(), 0);
}
