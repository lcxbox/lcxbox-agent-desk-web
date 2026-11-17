use super::*;
use std::io::Read;

fn download(
    v: &LockedVault,
    cache: &mut ExportCache,
    record: &Record,
    now: u64,
) -> io::Result<(u64, Vec<u8>)> {
    let mut package = Vec::new();
    let mut revision = None;
    loop {
        let chunk = v.export_chunk(
            cache,
            &record.scope,
            &record.conversation,
            &record.id,
            0,
            package.len() as u64,
            now,
        )?;
        assert!(chunk.data.len() <= EXPORT_CHUNK_BYTES);
        assert_eq!(chunk.offset, package.len() as u64);
        assert_eq!(
            *revision.get_or_insert(chunk.package_revision),
            chunk.package_revision
        );
        package.extend_from_slice(&chunk.data);
        if package.len() as u64 == chunk.total_size {
            return Ok((chunk.package_revision, package));
        }
    }
}
fn archive_file(package: &[u8], name: &str) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(package)).unwrap();
    assert_eq!(archive.len(), 2);
    let mut file = archive.by_name(name).unwrap();
    assert_eq!(file.compression(), zip::CompressionMethod::Deflated);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn sixteen_mib_package_is_exported_in_chunks_and_reassembles_exactly() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let large = encoding::text(MAX_TEXT_BYTES / 34, 4);
    let large = &large[..MAX_TEXT_BYTES.min(large.len())];
    let record = v
        .backup(edit("c", "large", large, b"small"), 0, no_quota)
        .unwrap();
    commit(&mut v, &record);
    let mut cache = ExportCache::new();
    let (revision, package) = download(&v, &mut cache, &record, 1001).unwrap();
    assert_eq!(archive_file(&package, "before.txt"), large);
    let manifest: serde_json::Value =
        serde_json::from_slice(&archive_file(&package, "metadata.json")).unwrap();
    assert_eq!(manifest["file_name"], "notes.txt");
    assert_eq!(manifest["content_sha256"], hash(large));
    assert_eq!(manifest["filesystem"]["mode"], 420);
    assert!(manifest.get("scope").is_none());
    assert_eq!(cache.builds, 1);
    // A package rebuilt for the same revision is byte-identical.
    let (again, rebuilt) = download(&v, &mut ExportCache::new(), &record, 1002).unwrap();
    assert_eq!((again, &rebuilt), (revision, &package));
}

#[test]
fn retention_change_moves_the_package_revision() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "c", "op");
    commit(&mut v, &record);
    let mut cache = ExportCache::new();
    let first = v
        .export_chunk(&mut cache, &scope(), "c", &record.id, 0, 0, 1001)
        .unwrap();
    v.set_policy(Policy {
        retention_days: 30,
        max_bytes: 1024 * 1024 * 1024,
    })
    .unwrap();
    let second = v
        .export_chunk(&mut cache, &scope(), "c", &record.id, 0, 0, 1002)
        .unwrap();
    assert_ne!(first.package_revision, second.package_revision);
    assert_ne!(first.data, second.data);
    assert_eq!(cache.builds, 2);
}

#[test]
fn alternating_exports_hit_the_cache_and_a_third_stream_is_busy() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let records: Vec<_> = (0..3)
        .map(|index| {
            let large = encoding::text(100_000 + index, index as u64);
            let record = v
                .backup(edit("c", &format!("op{index}"), &large, b"x"), 0, no_quota)
                .unwrap();
            commit(&mut v, &record);
            record
        })
        .collect();
    let mut cache = ExportCache::new();
    let chunk = |cache: &mut ExportCache, record: &Record, offset, now| {
        v.export_chunk(cache, &scope(), "c", &record.id, 0, offset, now)
    };
    let a = chunk(&mut cache, &records[0], 0, 1001).unwrap();
    let b = chunk(&mut cache, &records[1], 0, 1001).unwrap();
    assert!(a.total_size > EXPORT_CHUNK_BYTES as u64);
    chunk(&mut cache, &records[0], a.data.len() as u64, 1002).unwrap();
    chunk(&mut cache, &records[1], b.data.len() as u64, 1002).unwrap();
    assert_eq!(cache.builds, 2);
    assert_eq!(
        chunk(&mut cache, &records[2], 0, 1003).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    // Completing a stream frees its slot; so does an idle stream's TTL.
    let mut offset = a.data.len() as u64;
    while offset < a.total_size {
        offset += chunk(&mut cache, &records[0], offset, 1004)
            .unwrap()
            .data
            .len() as u64;
    }
    chunk(&mut cache, &records[2], 0, 1005).unwrap();
    assert_eq!(
        chunk(&mut cache, &records[0], 0, 1006).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    chunk(&mut cache, &records[0], 0, 1002 + 60_000).unwrap();
    assert!(chunk(&mut cache, &records[0], a.total_size + 1, 1002 + 60_001).is_err());
}

#[test]
fn export_failures_are_typed_and_do_not_cross_scopes() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let mut cache = ExportCache::new();
    let error = |result: io::Result<ExportChunk>| {
        *result
            .unwrap_err()
            .get_ref()
            .unwrap()
            .downcast_ref::<ExportError>()
            .unwrap()
    };
    let preparing = save(&mut v, "c", "preparing");
    v.ledger.records.get_mut(&preparing.id).unwrap().change = ChangeState::Preparing;
    assert_eq!(
        error(v.export_chunk(&mut cache, &scope(), "c", &preparing.id, 0, 0, 1001)),
        ExportError::Preparing
    );
    let record = save(&mut v, "c", "op");
    commit(&mut v, &record);
    let mut other = scope();
    other.authority = "other-central".into();
    for (scope, conversation) in [(&other, "c"), (&scope(), "different")] {
        assert_eq!(
            error(v.export_chunk(&mut cache, scope, conversation, &record.id, 0, 0, 1001)),
            ExportError::Unavailable
        );
    }
    assert!(
        v.export_chunk(&mut cache, &scope(), "c", &record.id, 1, 0, 1001)
            .is_err()
    );
    assert_eq!(
        error(v.export_chunk(
            &mut cache,
            &scope(),
            "c",
            &record.id,
            0,
            0,
            record.expires_at_ms
        )),
        ExportError::Expired
    );
    v.discard(&scope(), "c", &record.id, 1002).unwrap();
    assert_eq!(
        error(v.export_chunk(&mut cache, &scope(), "c", &record.id, 0, 0, 1003)),
        ExportError::Cleaned
    );
    let local = save(&mut v, "c", "local");
    commit(&mut v, &local);
    assert!(
        v.export_local_chunk(&mut cache, &local.id, 0, 0, 1004)
            .is_ok()
    );
    v.delete_conversation(&scope(), "c", 1005).unwrap();
    assert_eq!(
        error(v.export_local_chunk(&mut cache, &local.id, 0, 0, 1006)),
        ExportError::Unavailable
    );
}

#[test]
fn local_owner_export_is_chunked_and_needs_no_scope() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let content = encoding::text(30_000, 6);
    let record = v
        .backup(edit("c", "op", &content, b"after"), 0, no_quota)
        .unwrap();
    commit(&mut v, &record);
    let listed = v.local_records(None).unwrap();
    assert_eq!(listed.len(), 1);
    let mut cache = ExportCache::new();
    let mut package = Vec::new();
    loop {
        let chunk = v
            .export_local_chunk(&mut cache, &listed[0].id, 0, package.len() as u64, 1001)
            .unwrap();
        assert!(chunk.data.len() <= EXPORT_CHUNK_BYTES);
        package.extend_from_slice(&chunk.data);
        if package.len() as u64 == chunk.total_size {
            break;
        }
    }
    assert!(package.len() > EXPORT_CHUNK_BYTES / 4);
    assert_eq!(archive_file(&package, "before.txt"), content);
    assert_eq!(
        v.export_local_chunk(&mut cache, &record.id, 1, 0, 1001)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn a_rebuilt_vault_never_serves_a_cached_package_of_the_previous_one() {
    // Record ids and revisions are derived from the operation, so a rebuilt
    // vault reproduces them; only the vault instance tells the packages apart.
    let (_first_root, first) = fixture();
    let (_second_root, second) = fixture();
    let mut cache = ExportCache::new();
    let mut old = first.lock().unwrap();
    let old_record = old
        .backup(edit("c", "op", b"old content", b"x"), 0, no_quota)
        .unwrap();
    commit(&mut old, &old_record);
    let mut new = second.lock().unwrap();
    let new_record = new
        .backup(edit("c", "op", b"new content", b"x"), 0, no_quota)
        .unwrap();
    commit(&mut new, &new_record);
    assert_eq!(
        (&old_record.id, old_record.revision),
        (&new_record.id, new_record.revision)
    );
    let (_, package) = download(&old, &mut cache, &old_record, 1001).unwrap();
    assert_eq!(archive_file(&package, "before.txt"), b"old content");
    let (_, package) = download(&new, &mut cache, &new_record, 1001).unwrap();
    assert_eq!(archive_file(&package, "before.txt"), b"new content");
    assert_eq!(cache.builds, 2);
}
