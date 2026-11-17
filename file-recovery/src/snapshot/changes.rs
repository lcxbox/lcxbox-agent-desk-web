//! Change lists: a root's post-run manifest compared with the pre-run
//! manifest of the same record.
use super::*;
use manifest::{Content, ManifestFile};
use std::cmp::Ordering;

fn same_content(before: &ManifestFile, after: &ManifestFile) -> bool {
    match (&before.content, &after.content) {
        (Content::StatOnly, Content::StatOnly) => {
            before.size == after.size
                && before.mtime_ns == after.mtime_ns
                && before.ctime_ns == after.ctime_ns
                && before.file_id == after.file_id
        }
        (before, after) => before.sha256().is_some() && before.sha256() == after.sha256(),
    }
}

fn entry(
    root: u32,
    directory_identity: &str,
    kind: ChangeKind,
    before: Option<&ManifestFile>,
    after: Option<&ManifestFile>,
) -> Entry {
    let path = after.or(before).expect("a change has a side").path.clone();
    let before_object = before.and_then(|file| file.content.object()).cloned();
    let after_object = after.and_then(|file| file.content.object()).cloned();
    // Restoring needs the before content and, to verify and back up the
    // current file, the after content as text objects.
    let protected = (before.is_none() || before_object.is_some())
        && (after.is_none() || after_object.is_some());
    Entry {
        lineage: lineage(directory_identity, &path),
        rel_path: path,
        before: before_object,
        after: after_object,
        metadata: None,
        root,
        change: Some(RecordedChange {
            kind,
            before_sha256: before.and_then(|file| file.content.sha256().map(str::to_owned)),
            after_sha256: after.and_then(|file| file.content.sha256().map(str::to_owned)),
            size: after.or(before).map_or(0, |file| file.size),
            protected,
        }),
    }
}

/// Changed paths of one root; both lists are sorted by path.
pub(crate) fn changes(
    root: u32,
    directory_identity: &str,
    before: &[ManifestFile],
    after: &[ManifestFile],
) -> Vec<Entry> {
    let mut changes = Vec::new();
    let (mut b, mut a) = (0, 0);
    loop {
        let order = match (before.get(b), after.get(a)) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(old), Some(new)) => old.path.cmp(&new.path),
        };
        match order {
            Ordering::Less => {
                changes.push(entry(
                    root,
                    directory_identity,
                    ChangeKind::Deleted,
                    Some(&before[b]),
                    None,
                ));
                b += 1;
            }
            Ordering::Greater => {
                changes.push(entry(
                    root,
                    directory_identity,
                    ChangeKind::Added,
                    None,
                    Some(&after[a]),
                ));
                a += 1;
            }
            Ordering::Equal => {
                if !same_content(&before[b], &after[a]) {
                    changes.push(entry(
                        root,
                        directory_identity,
                        ChangeKind::Modified,
                        Some(&before[b]),
                        Some(&after[a]),
                    ));
                }
                b += 1;
                a += 1;
            }
        }
    }
    changes
}
