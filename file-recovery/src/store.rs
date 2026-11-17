//! Reading published objects. Published objects are trusted as written: the
//! rename-only publication makes a present name a complete object, so content
//! hashes are not recomputed here.
use super::*;

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

    pub(crate) fn entry_table(&self, record: &Record) -> io::Result<Vec<Entry>> {
        match &record.entries {
            Entries::Inline { entry } => Ok(vec![entry.clone()]),
            Entries::Table { sha256, count } => {
                let raw = self.read_object(&record.namespace(), &ObjRef::full(sha256.clone()))?;
                let entries: Vec<Entry> = serde_json::from_slice(&raw)
                    .map_err(|_| invalid("recovery entry table is corrupt"))?;
                if entries.len() as u64 != *count {
                    return Err(invalid("recovery entry table is corrupt"));
                }
                for entry in &entries {
                    entry.validate()?;
                }
                Ok(entries)
            }
        }
    }

    pub(crate) fn entry(&self, record: &Record, index: u64) -> io::Result<Entry> {
        match &record.entries {
            Entries::Inline { entry } if index == 0 => Ok(entry.clone()),
            Entries::Inline { .. } => Err(invalid("recovery entry is unavailable")),
            Entries::Table { count, .. } if index < *count => self
                .entry_table(record)?
                .into_iter()
                .nth(index as usize)
                .ok_or_else(|| invalid("recovery entry is unavailable")),
            Entries::Table { .. } => Err(invalid("recovery entry is unavailable")),
        }
    }
}
