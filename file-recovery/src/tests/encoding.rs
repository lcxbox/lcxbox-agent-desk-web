use super::*;

pub(super) fn text(lines: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::new();
    for line in 0..lines {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(format!("{line:06} {:016x} some source text\n", state).as_bytes());
    }
    out
}

#[test]
fn twenty_small_edits_store_three_full_frames_and_block_sized_deltas() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let mut versions = vec![text(2400, 3)];
    let mut records = Vec::new();
    for step in 0..20 {
        let current = versions.last().unwrap().clone();
        let mut next = current.clone();
        let at = (step * 4919 + 7) % (current.len() - 16);
        next[at..at + 8].copy_from_slice(format!("EDIT{step:04}").as_bytes());
        let operation = format!("edit-{step}");
        let record = v
            .backup(edit("c", &operation, &current, &next), 0, no_quota)
            .unwrap();
        commit(&mut v, &record);
        records.push(record);
        versions.push(next);
    }
    let (full, delta) = object_kinds(root.path(), &scope());
    // 21 versions plus one shared metadata object. Chains stop below depth 8.
    assert_eq!((full, delta), (3 + 1, 18));
    let frames = objects(root.path(), &scope());
    let largest_full = frames
        .iter()
        .filter(|(_, frame)| object::parse_header(frame).unwrap().0.kind == object::Kind::Full)
        .map(|(_, frame)| round_block(frame.len() as u64))
        .max()
        .unwrap();
    assert!(
        frames
            .iter()
            .filter(|(_, frame)| object::parse_header(frame).unwrap().0.kind == object::Kind::Delta)
            .all(|(_, frame)| frame.len() as u64 <= BLOCK_BYTES)
    );
    let usage = v.ledger.namespaces[&scope().namespace()];
    let stored: u64 = frames
        .iter()
        .map(|(_, frame)| round_block(frame.len() as u64))
        .sum();
    assert_eq!(usage.bytes, stored);
    assert_eq!(usage.objects, frames.len() as u64);
    assert!(stored <= 3 * largest_full + 18 * BLOCK_BYTES + BLOCK_BYTES);
    let previous_design = 20 * (versions[0].len() as u64 + 32 * 1024);
    println!(
        "20 edits of a {} byte file: {} bytes stored ({} full frames of <= {} bytes, 18 deltas), report total {}; previous design about {} bytes",
        versions[0].len(),
        stored,
        3,
        largest_full,
        v.usage_report().total(),
        previous_design
    );
    let depth = records
        .iter()
        .map(|record| {
            record
                .inline_entry()
                .unwrap()
                .after
                .as_ref()
                .unwrap()
                .bases
                .len()
        })
        .max()
        .unwrap();
    assert_eq!(depth, MAX_CHAIN_DEPTH - 1);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(before_of(&v, record), versions[index]);
    }
}

#[test]
fn existing_objects_are_reused_without_writing() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let first = v
        .backup(
            edit("c", "one", b"version one", b"version two"),
            0,
            no_quota,
        )
        .unwrap();
    commit(&mut v, &first);
    let files = objects(root.path(), &scope()).len();
    let second = v
        .backup(
            edit("c", "two", b"version two", b"version one"),
            0,
            no_quota,
        )
        .unwrap();
    commit(&mut v, &second);
    assert_eq!(second.published_bytes, 0);
    assert_eq!(objects(root.path(), &scope()).len(), files);
    let entry = second.inline_entry().unwrap();
    assert_eq!(
        entry.before,
        first.inline_entry().unwrap().after,
        "the recorded chain of a reused object is kept"
    );
    // Another conversation of the same scope shares the objects too.
    let shared = v
        .backup(
            edit("other", "three", b"version one", b"version two"),
            0,
            no_quota,
        )
        .unwrap();
    assert_eq!(shared.published_bytes, 0);
}

#[test]
fn lineage_delta_is_used_only_below_half_of_the_full_frame() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let base = text(3000, 9);
    let first = v
        .backup(edit("c", "one", &base, &text(3000, 10)), 0, no_quota)
        .unwrap();
    commit(&mut v, &first);
    let latest = first.inline_entry().unwrap().after.clone().unwrap();
    // An external edit of the latest version is stored against it.
    let mut external = text(3000, 10);
    external[100..110].copy_from_slice(b"0123456789");
    let related = v
        .backup(edit("c", "two", &external, b"short"), 0, no_quota)
        .unwrap();
    commit(&mut v, &related);
    assert_eq!(
        related
            .inline_entry()
            .unwrap()
            .before
            .as_ref()
            .unwrap()
            .base(),
        Some(latest.sha256.as_str())
    );
    // A rewrite of unrelated content gains nothing from the base.
    let unrelated = v
        .backup(edit("c", "three", &text(3000, 77), b"x"), 0, no_quota)
        .unwrap();
    assert!(
        unrelated
            .inline_entry()
            .unwrap()
            .before
            .as_ref()
            .unwrap()
            .bases
            .is_empty()
    );
    assert_eq!(before_of(&v, &related), external);
    assert_eq!(before_of(&v, &unrelated), text(3000, 77));
}

#[test]
fn requests_are_validated_before_anything_is_written() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let too_large = vec![b'x'; MAX_TEXT_BYTES + 1];
    let binary = b"nul\0byte".to_vec();
    for request in [
        edit("c", "large", &too_large, b"x"),
        edit("c", "binary", &binary, b"x"),
        request(
            "c",
            "path",
            RecordKind::TextEdit,
            vec![entry("a/b.txt", Some(b"a"), Some(b"b"))],
        ),
        request(
            "c",
            "shape",
            RecordKind::TextDelete,
            vec![entry("a.txt", Some(b"a"), Some(b"b"))],
        ),
        request("c", "empty", RecordKind::CommandSnapshot, Vec::new()),
        request(
            "c",
            "parent",
            RecordKind::CommandSnapshot,
            vec![entry("../a", Some(b"a"), None)],
        ),
    ] {
        assert_eq!(
            v.backup(request, 0, no_quota).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(v.ledger.records.is_empty());
    assert!(objects(root.path(), &scope()).is_empty());
}
