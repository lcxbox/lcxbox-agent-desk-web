//! Snapshot manifests: one object per scanned root listing every included
//! file with its stat identity and content. The latest manifest of each
//! (conversation, root) is a ledger pointer owned by the conversation; the
//! pre-run manifest of an unfinished run is a root of its record.
use super::*;

/// Stat-only comparison: content is not saved and not hashed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Content {
    /// UTF-8 text. Without an object (quota exhausted) the next scan rereads it.
    Text {
        sha256: String,
        object: Option<ObjRef>,
    },
    Binary {
        sha256: String,
    },
    /// Too large or unreadable; compared by stat only.
    StatOnly,
}
impl Content {
    pub fn sha256(&self) -> Option<&str> {
        match self {
            Content::Text { sha256, .. } | Content::Binary { sha256 } => Some(sha256),
            Content::StatOnly => None,
        }
    }
    pub fn object(&self) -> Option<&ObjRef> {
        match self {
            Content::Text { object, .. } => object.as_ref(),
            Content::Binary { .. } | Content::StatOnly => None,
        }
    }
    /// Whether an unchanged stat lets a later scan keep this entry unread.
    pub fn reusable(&self) -> bool {
        !matches!(self, Content::Text { object: None, .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// ctime (Unix) or ChangeTime (Windows): catches content changes that
    /// preserve mtime (`touch -r`, `cp -p`, `rsync -t`).
    pub ctime_ns: i64,
    pub file_id: String,
    pub content: Content,
}
impl ManifestFile {
    pub fn new(path: String, stat: &scan_fs::Stat, content: Content) -> Self {
        Self {
            path,
            size: stat.size,
            mtime_ns: stat.mtime_ns,
            ctime_ns: stat.ctime_ns,
            file_id: stat.file_id.clone(),
            content,
        }
    }
    pub fn same_stat(&self, stat: &scan_fs::Stat) -> bool {
        self.size == stat.size
            && self.mtime_ns == stat.mtime_ns
            && self.ctime_ns == stat.ctime_ns
            && self.file_id == stat.file_id
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    /// Wall clock at the start of the scan, for the racy-file rule.
    pub scanned_at_ns: i64,
    /// Sorted by path.
    pub files: Vec<ManifestFile>,
}
impl Manifest {
    pub fn objects(&self) -> impl Iterator<Item = &ObjRef> {
        self.files.iter().filter_map(|file| file.content.object())
    }
}

/// The latest manifest of one (conversation, root).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestPointer {
    pub scope: Scope,
    pub conversation: String,
    pub directory_identity: String,
    pub sha256: String,
    pub files: u64,
    pub updated_at_ms: u64,
    pub expires_at_ms: u64,
    /// Ledger bytes billed to the scope's namespace.
    pub serialized_bytes: u64,
}
impl ManifestPointer {
    pub fn key(&self) -> String {
        manifest_key(&self.scope, &self.conversation, &self.directory_identity)
    }
    pub fn validate(&self) -> io::Result<()> {
        self.scope.validate()?;
        identity(&self.conversation)?;
        identity(&self.directory_identity)?;
        if !valid_id(&self.sha256) || self.files as usize > MAX_RECORD_ENTRIES {
            return Err(invalid("recovery manifest index is corrupt"));
        }
        Ok(())
    }
    pub fn rebill(&mut self) -> io::Result<()> {
        self.serialized_bytes = 0;
        self.serialized_bytes = serde_json::to_vec(&*self)?.len() as u64 + 64 + 24;
        Ok(())
    }
}

pub(crate) fn manifest_key(scope: &Scope, conversation: &str, directory_identity: &str) -> String {
    hash(
        &serde_json::to_vec(&(scope.key(conversation), directory_identity))
            .expect("manifest key serialization"),
    )
}

/// Serialize a manifest as a full-frame object, bounded like any object.
pub(crate) fn encode(manifest: &Manifest) -> Result<(String, Vec<u8>), Stop> {
    let raw = serde_json::to_vec(manifest).map_err(io::Error::from)?;
    if raw.len() > MAX_TEXT_BYTES {
        return Err(Stop::Skip(SkipReason::ManifestTooLarge));
    }
    Ok((hash(&raw), object::encode_full(&raw)?))
}

impl LockedVault {
    pub(crate) fn read_manifest(&self, namespace: &str, sha: &str) -> io::Result<Manifest> {
        let raw = self.read_object(namespace, &ObjRef::full(sha.to_owned()))?;
        let manifest: Manifest =
            serde_json::from_slice(&raw).map_err(|_| invalid("recovery manifest is corrupt"))?;
        for file in &manifest.files {
            validate_rel_path(&file.path, false)?;
            if let Some(object) = file.content.object() {
                object.validate()?;
            }
        }
        Ok(manifest)
    }

    pub(crate) fn read_batch(&self, namespace: &str, sha: &str) -> io::Result<Vec<ObjRef>> {
        let raw = self.read_object(namespace, &ObjRef::full(sha.to_owned()))?;
        let objects: Vec<ObjRef> = serde_json::from_slice(&raw)
            .map_err(|_| invalid("recovery snapshot batch is corrupt"))?;
        for object in &objects {
            object.validate()?;
        }
        Ok(objects)
    }

    /// Objects a manifest keeps alive, including the manifest itself. A
    /// missing manifest holds nothing.
    pub(crate) fn manifest_roots(&self, namespace: &str, sha: &str) -> io::Result<Vec<ObjRef>> {
        let mut roots = vec![ObjRef::full(sha.to_owned())];
        match self.read_manifest(namespace, sha) {
            Ok(manifest) => roots.extend(manifest.objects().cloned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        Ok(roots)
    }

    /// Objects an unfinished run keeps alive: its baseline and pre-run
    /// manifests and the objects of every published batch.
    pub(crate) fn snapshot_roots(
        &self,
        namespace: &str,
        snapshot: &SnapshotState,
    ) -> io::Result<Vec<ObjRef>> {
        let mut roots = Vec::new();
        for root in &snapshot.roots {
            for sha in root.baseline.iter().chain(&root.manifest) {
                roots.extend(self.manifest_roots(namespace, sha)?);
            }
        }
        for sha in &snapshot.batches {
            roots.push(ObjRef::full(sha.clone()));
            match self.read_batch(namespace, sha) {
                Ok(objects) => roots.extend(objects),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(roots)
    }

    /// Entries of an unfinished or interrupted run: the saved files of its
    /// pre-run manifests, for export.
    pub(crate) fn pre_run_entries(&self, record: &Record) -> io::Result<Vec<Entry>> {
        let namespace = record.namespace();
        let mut entries = Vec::new();
        let Some(snapshot) = &record.snapshot else {
            return Ok(entries);
        };
        for (index, root) in snapshot.roots.iter().enumerate() {
            let Some(sha) = &root.manifest else {
                continue;
            };
            for file in self.read_manifest(&namespace, sha)?.files {
                let Some(object) = file.content.object() else {
                    continue;
                };
                entries.push(Entry {
                    lineage: lineage(&root.directory_identity, &file.path),
                    before: Some(object.clone()),
                    rel_path: file.path,
                    after: None,
                    metadata: None,
                    root: index as u32,
                    change: None,
                });
            }
        }
        Ok(entries)
    }

    /// Files of the latest manifest of a root in this conversation, as an
    /// estimate for an approval card.
    pub fn latest_manifest_files(
        &self,
        scope: &Scope,
        conversation: &str,
        directory_identity: &str,
    ) -> Option<u64> {
        self.ledger
            .manifests
            .get(&manifest_key(scope, conversation, directory_identity))
            .map(|manifest| manifest.files)
    }
}
