//! Deterministic per-entry export packages, chunked reads, entry paging and
//! restore material. A package is identified by the record's revision, which
//! changes with every state or retention change, so hosts can detect a
//! package that changed between chunks.
use super::*;
use std::io::Write;

/// Raw bytes per export chunk; base64 stays below the signaling bounds.
pub const EXPORT_CHUNK_BYTES: usize = 512 * 1024;
const ENTRIES_PAGE: usize = 100;
const CACHED_PACKAGES: usize = 2;
const CACHE_TTL_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportChunk {
    pub package_revision: u64,
    pub total_size: u64,
    pub offset: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrySummary {
    pub index: u64,
    pub rel_path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntriesPage {
    pub total: u64,
    pub entries: Vec<EntrySummary>,
    pub next_offset: Option<u64>,
}

/// What a restore needs from one backed-up entry. `directory_identity` must
/// match the identity of the directory the worker resolves for the restore.
#[derive(Clone, Debug)]
pub struct EntryMaterial {
    pub record: Record,
    pub entry: Entry,
    pub before: Option<Vec<u8>>,
    pub metadata: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PackageKey {
    /// Record ids and revisions restart in a rebuilt vault.
    vault_instance: String,
    namespace: String,
    recovery_id: String,
    entry: u64,
    revision: u64,
}
struct CachedPackage {
    key: PackageKey,
    bytes: Vec<u8>,
    last_used_ms: u64,
    complete: bool,
}

/// Worker-local LRU of built packages. Each cached package is one export
/// stream; a third concurrent stream is refused as busy (`WouldBlock`) until
/// a stream completes or stays idle past its time to live.
#[derive(Default)]
pub struct ExportCache {
    packages: Vec<CachedPackage>,
    #[cfg(test)]
    pub(crate) builds: u64,
}

impl ExportCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn chunk(
        &mut self,
        key: PackageKey,
        offset: u64,
        now_ms: u64,
        build: impl FnOnce() -> io::Result<Vec<u8>>,
    ) -> io::Result<ExportChunk> {
        let index = match self.packages.iter().position(|package| package.key == key) {
            Some(index) => index,
            None => {
                self.packages.retain(|package| {
                    package.key.namespace != key.namespace
                        || package.key.recovery_id != key.recovery_id
                        || package.key.entry != key.entry
                });
                if self.packages.len() >= CACHED_PACKAGES {
                    let evictable = self
                        .packages
                        .iter()
                        .enumerate()
                        .filter(|(_, package)| {
                            package.complete
                                || now_ms.saturating_sub(package.last_used_ms) >= CACHE_TTL_MS
                        })
                        .min_by_key(|(_, package)| package.last_used_ms)
                        .map(|(index, _)| index);
                    match evictable {
                        Some(index) => {
                            self.packages.remove(index);
                        }
                        None => {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "recovery export capacity is busy",
                            ));
                        }
                    }
                }
                #[cfg(test)]
                {
                    self.builds += 1;
                }
                self.packages.push(CachedPackage {
                    key: key.clone(),
                    bytes: build()?,
                    last_used_ms: now_ms,
                    complete: false,
                });
                self.packages.len() - 1
            }
        };
        let package = &mut self.packages[index];
        let total = package.bytes.len() as u64;
        if offset > total {
            return Err(invalid("recovery export offset exceeds the package"));
        }
        let end = (offset as usize + EXPORT_CHUNK_BYTES).min(package.bytes.len());
        package.last_used_ms = now_ms;
        package.complete = end == package.bytes.len();
        Ok(ExportChunk {
            package_revision: key.revision,
            total_size: total,
            offset,
            data: package.bytes[offset as usize..end].to_vec(),
        })
    }
}

impl LockedVault {
    /// Exportable material of a record; failures carry no other scope's data.
    fn exportable(
        &self,
        scope: Option<&Scope>,
        conversation: Option<&str>,
        id: &str,
        now_ms: u64,
    ) -> io::Result<&Record> {
        if !valid_id(id) {
            return Err(invalid("invalid recovery id"));
        }
        let record = self
            .ledger
            .records
            .get(id)
            .filter(|r| {
                scope.is_none_or(|scope| &r.scope == scope)
                    && conversation.is_none_or(|conversation| r.conversation == conversation)
                    && !self.is_deleted(&r.scope, &r.conversation)
            })
            .ok_or_else(|| export_error(ExportError::Unavailable))?;
        if record.material == MaterialState::Purged {
            return Err(export_error(ExportError::Cleaned));
        }
        if record.discard_requested {
            return Err(export_error(ExportError::Cleaning));
        }
        if record.change == ChangeState::Preparing {
            return Err(export_error(ExportError::Preparing));
        }
        if record.expires_at_ms <= now_ms && record.change != ChangeState::OutcomeUnknown {
            return Err(export_error(ExportError::Expired));
        }
        Ok(record)
    }

    /// Material of one entry for an in-place restore. Content is decoded from
    /// published objects without rehashing.
    pub fn entry_material(
        &self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        entry: u64,
        now_ms: u64,
    ) -> io::Result<EntryMaterial> {
        let record = self.exportable(Some(scope), Some(conversation), id, now_ms)?;
        let namespace = record.namespace();
        let entry = self.entry(record, entry)?;
        let before = entry
            .before
            .as_ref()
            .map(|before| self.read_object(&namespace, before))
            .transpose()?;
        let metadata = entry
            .metadata
            .as_ref()
            .map(|sha| self.read_object(&namespace, &ObjRef::full(sha.clone())))
            .transpose()?;
        Ok(EntryMaterial {
            record: record.clone(),
            entry,
            before,
            metadata,
        })
    }

    pub fn query_entries(
        &self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        offset: u64,
    ) -> io::Result<EntriesPage> {
        let record = self
            .ledger
            .records
            .get(id)
            .filter(|r| {
                &r.scope == scope
                    && r.conversation == conversation
                    && r.material == MaterialState::Saved
                    && !self.is_deleted(scope, conversation)
            })
            .ok_or_else(|| export_error(ExportError::Unavailable))?;
        let total = record.entry_count();
        if offset > total {
            return Err(invalid("recovery entry offset exceeds the record"));
        }
        let entries: Vec<EntrySummary> = self
            .entry_table(record)?
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .take(ENTRIES_PAGE)
            .map(|(index, entry)| EntrySummary {
                index: index as u64,
                rel_path: entry.rel_path,
                before_sha256: entry.before.map(|object| object.sha256),
                after_sha256: entry.after.map(|object| object.sha256),
            })
            .collect();
        let next = offset + entries.len() as u64;
        Ok(EntriesPage {
            total,
            entries,
            next_offset: (next < total).then_some(next),
        })
    }

    /// One chunk of an entry's package. Every chunk revalidates availability;
    /// the package bytes come from the worker cache while its revision holds.
    #[allow(clippy::too_many_arguments)]
    pub fn export_chunk(
        &self,
        cache: &mut ExportCache,
        scope: &Scope,
        conversation: &str,
        id: &str,
        entry: u64,
        offset: u64,
        now_ms: u64,
    ) -> io::Result<ExportChunk> {
        self.export_chunk_inner(
            cache,
            Some(scope),
            Some(conversation),
            id,
            entry,
            offset,
            now_ms,
        )
    }

    /// Authenticated local OS owner may retrieve old credential-domain material.
    pub fn export_local_chunk(
        &self,
        cache: &mut ExportCache,
        id: &str,
        entry: u64,
        offset: u64,
        now_ms: u64,
    ) -> io::Result<ExportChunk> {
        self.export_chunk_inner(cache, None, None, id, entry, offset, now_ms)
    }

    #[allow(clippy::too_many_arguments)]
    fn export_chunk_inner(
        &self,
        cache: &mut ExportCache,
        scope: Option<&Scope>,
        conversation: Option<&str>,
        id: &str,
        entry: u64,
        offset: u64,
        now_ms: u64,
    ) -> io::Result<ExportChunk> {
        let record = self.exportable(scope, conversation, id, now_ms)?;
        if entry >= record.entry_count() {
            return Err(invalid("recovery entry is unavailable"));
        }
        let key = PackageKey {
            vault_instance: self.vault_instance_id().to_owned(),
            namespace: record.namespace(),
            recovery_id: record.id.clone(),
            entry,
            revision: record.revision,
        };
        cache.chunk(key, offset, now_ms, || self.package(record, entry))
    }

    /// Fixed archive names prevent original paths from controlling extraction.
    /// The same record revision always yields the same bytes.
    pub(crate) fn package(&self, record: &Record, index: u64) -> io::Result<Vec<u8>> {
        let namespace = record.namespace();
        let entry = self.entry(record, index)?;
        let before = entry
            .before
            .as_ref()
            .ok_or_else(|| export_error(ExportError::Unavailable))?;
        let content = self.read_object(&namespace, before)?;
        let filesystem: serde_json::Value = match &entry.metadata {
            Some(sha) => {
                serde_json::from_slice(&self.read_object(&namespace, &ObjRef::full(sha.clone()))?)
                    .map_err(|_| invalid("backup metadata is unavailable"))?
            }
            None => serde_json::Value::Null,
        };
        let manifest = serde_json::json!({
            "recovery_id": record.id,
            "entry": index,
            "file_name": entry.rel_path,
            "created_at_unix_ms": record.created_at_ms,
            "expires_at_unix_ms": record.expires_at_ms,
            "content_sha256": before.sha256,
            "after_sha256": entry.after.as_ref().map(|after| &after.sha256),
            "change_state": record.change,
            "filesystem": filesystem,
            "restoration": "Content and metadata export only; no automatic restoration of ownership, ACLs, or the original path."
        });
        let mut archive = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(0o600);
        archive
            .start_file("before.txt", options)
            .map_err(io::Error::other)?;
        archive.write_all(&content)?;
        archive
            .start_file("metadata.json", options)
            .map_err(io::Error::other)?;
        archive.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        Ok(archive.finish().map_err(io::Error::other)?.into_inner())
    }
}
