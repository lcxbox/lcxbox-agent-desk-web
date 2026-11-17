use super::*;
use std::{fs, path::PathBuf};

mod crash;
mod encoding;
mod epoch;
mod export;
mod lifecycle;
#[cfg(unix)]
mod restore;
#[cfg(unix)]
mod snapshot;
mod sweep;

pub(super) fn scope() -> Scope {
    Scope {
        authority: "central-a".into(),
        device: "device-a".into(),
        os_user: "user-a".into(),
        owner: "owner-a".into(),
    }
}
pub(super) fn fixture() -> (tempfile::TempDir, Vault) {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    (root, vault)
}
pub(super) fn entry<'a>(
    name: &'a str,
    before: Option<&'a [u8]>,
    after: Option<&'a [u8]>,
) -> EntryInput<'a> {
    EntryInput {
        rel_path: name,
        before,
        after,
        metadata: Some(br#"{"mode":420}"#),
    }
}
pub(super) fn request<'a>(
    conversation: &'a str,
    operation: &'a str,
    kind: RecordKind,
    entries: Vec<EntryInput<'a>>,
) -> BackupRequest<'a> {
    BackupRequest {
        scope: scope(),
        conversation,
        operation,
        generation: "generation",
        kind,
        directory_identity: "directory-1",
        entries,
        transaction: None,
        now_ms: 1000,
    }
}
pub(super) fn edit<'a>(
    conversation: &'a str,
    operation: &'a str,
    before: &'a [u8],
    after: &'a [u8],
) -> BackupRequest<'a> {
    request(
        conversation,
        operation,
        RecordKind::TextEdit,
        vec![entry("notes.txt", Some(before), Some(after))],
    )
}
pub(super) fn no_quota(_: &Record, _: u64) -> io::Result<()> {
    Ok(())
}
/// A multi-entry `CommandRestore` record backed up through the annotated path.
pub(super) fn multi(
    v: &mut LockedVault,
    conversation: &str,
    operation: &str,
    entries: Vec<EntryInput<'_>>,
) -> Record {
    let request = request(conversation, operation, RecordKind::CommandRestore, entries);
    let annotation = backup::Annotation {
        snapshot: SnapshotState {
            roots: vec![SnapshotRootState {
                directory_identity: "directory-1".into(),
                baseline: None,
                manifest: None,
            }],
            batches: Vec::new(),
            started_at_ms: 1000,
            finished_at_ms: None,
        },
        entries: request
            .entries
            .iter()
            .map(|entry| {
                (
                    0,
                    lineage("directory-1", entry.rel_path),
                    RecordedChange {
                        kind: ChangeKind::Deleted,
                        before_sha256: entry.before.map(hash),
                        after_sha256: None,
                        size: 0,
                        protected: true,
                    },
                )
            })
            .collect(),
    };
    let encoded = v.encode(&request).unwrap();
    v.backup_annotated(request, encoded, 0, no_quota, Some(annotation))
        .unwrap()
}
pub(super) fn save(v: &mut LockedVault, conversation: &str, operation: &str) -> Record {
    v.backup(
        edit(conversation, operation, b"before", b"after"),
        0,
        no_quota,
    )
    .unwrap()
}
/// Commit a backed-up change without a user-file transaction.
pub(super) fn commit(v: &mut LockedVault, record: &Record) {
    v.transition(&record.scope, &record.id, ChangeState::CommitIntent)
        .unwrap();
    assert!(
        v.finish(&record.scope, &record.id, ChangeState::Succeeded)
            .unwrap()
    );
}
pub(super) fn namespace_dir(root: &Path, scope: &Scope) -> PathBuf {
    root.join("file-recovery/objects").join(scope.namespace())
}
/// Object files of a namespace as (name, frame).
pub(super) fn objects(root: &Path, scope: &Scope) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = fs::read_dir(namespace_dir(root, scope)) else {
        return Vec::new();
    };
    let mut objects: Vec<_> = entries
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| {
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    objects.sort();
    objects
}
/// (full, delta) object counts of a namespace.
pub(super) fn object_kinds(root: &Path, scope: &Scope) -> (usize, usize) {
    objects(root, scope).iter().fold(
        (0, 0),
        |(full, delta), (_, frame)| match object::parse_header(frame).unwrap().0.kind {
            object::Kind::Full => (full + 1, delta),
            object::Kind::Delta => (full, delta + 1),
        },
    )
}
pub(super) fn before_of(v: &LockedVault, record: &Record) -> Vec<u8> {
    v.entry_material(&record.scope, &record.conversation, &record.id, 0, 1001)
        .unwrap()
        .before
        .unwrap()
}
