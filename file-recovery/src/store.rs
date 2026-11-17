//! Reading published objects. Published objects are trusted as written: the
//! rename-only publication makes a present name a complete object, so content
//! hashes are not recomputed here.
use super::*;

/// The object behind `Entries::Table`.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntryTable {
    pub entries: Vec<Entry>,
    /// Objects the record published itself; they stay live with the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<ObjRef>,
}

/// Frames are bounded by the largest raw object plus zstd's worst-case overhead.
pub(crate) const MAX_FRAME_BYTES: u64 =
    (MAX_TEXT_BYTES + MAX_TEXT_BYTES / 64 + object::HEADER_BYTES + 4096) as u64;

impl LockedVault {
    /// Decode an object by expanding its recorded base chain from the full
    /// root frame outward, holding at most one base's raw content at a time.
    pub(crate) fn read_object(&self, namespace: &str, object: &ObjRef) -> io::Result<Vec<u8>> {
        let names: Vec<&str> = std::iter::once(object.sha256.as_str())
            .chain(object.bases.iter().map(String::as_str))
            .collect();
        let frames = names
            .iter()
            .map(|sha| self.storage.read_object(namespace, sha, MAX_FRAME_BYTES))
            .collect::<io::Result<Vec<_>>>()?;
        let mut raw: Option<Vec<u8>> = None;
        for index in (0..frames.len()).rev() {
            let base = raw.as_deref().map(|raw| (names[index + 1], raw));
            raw = Some(object::decode(&frames[index], base, MAX_TEXT_BYTES as u64)?);
        }
        Ok(raw.expect("an object has at least one frame"))
    }

    /// The decoded entry table object of a `Table` record.
    pub(crate) fn table(&self, record: &Record) -> io::Result<EntryTable> {
        let Entries::Table { sha256, count } = &record.entries else {
            return Err(invalid("recovery record has no entry table"));
        };
        let raw = self.read_object(&record.namespace(), &ObjRef::full(sha256.clone()))?;
        let table: EntryTable =
            serde_json::from_slice(&raw).map_err(|_| invalid("recovery entry table is corrupt"))?;
        if table.entries.len() as u64 != *count {
            return Err(invalid("recovery entry table is corrupt"));
        }
        for entry in &table.entries {
            entry.validate()?;
        }
        for object in &table.published {
            object.validate()?;
        }
        Ok(table)
    }

    pub(crate) fn entry_table(&self, record: &Record) -> io::Result<Vec<Entry>> {
        match &record.entries {
            Entries::Inline { entry } => Ok(vec![entry.clone()]),
            Entries::Table { .. } => Ok(self.table(record)?.entries),
            Entries::PreRun { count } => {
                let entries = self.pre_run_entries(record)?;
                if entries.len() as u64 != *count {
                    return Err(invalid("recovery entry table is corrupt"));
                }
                Ok(entries)
            }
        }
    }

    /// The base chain of a published object, read from object headers, or
    /// `None` when the object or a base is missing or unreadable. Publication
    /// keeps an existing complete object instead of replacing it with another
    /// encoding, because roots that the caller does not inspect (manifests,
    /// pending snapshots) may reference it with its current chain.
    pub(crate) fn stored_chain(
        &self,
        namespace: &str,
        sha: &str,
    ) -> io::Result<Option<Vec<String>>> {
        let mut chain = Vec::new();
        let mut name = sha.to_owned();
        loop {
            // A missing, foreign or unreadable entry is not a complete object;
            // publication replaces it.
            let Ok(prefix) =
                self.storage
                    .read_object_prefix(namespace, &name, object::HEADER_BYTES as u64)
            else {
                return Ok(None);
            };
            let Ok((header, _)) = object::parse_header(&prefix) else {
                return Ok(None);
            };
            let Some(base) = header.base else {
                return Ok(Some(chain));
            };
            name = base.iter().map(|byte| format!("{byte:02x}")).collect();
            chain.push(name.clone());
            if chain.len() >= MAX_CHAIN_DEPTH {
                return Ok(None);
            }
        }
    }

    pub(crate) fn entry(&self, record: &Record, index: u64) -> io::Result<Entry> {
        match &record.entries {
            Entries::Inline { entry } if index == 0 => Ok(entry.clone()),
            Entries::Inline { .. } => Err(invalid("recovery entry is unavailable")),
            Entries::Table { count, .. } | Entries::PreRun { count } if index < *count => self
                .entry_table(record)?
                .into_iter()
                .nth(index as usize)
                .ok_or_else(|| invalid("recovery entry is unavailable")),
            Entries::Table { .. } | Entries::PreRun { .. } => {
                Err(invalid("recovery entry is unavailable"))
            }
        }
    }
}
