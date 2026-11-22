//! Bounded zip unpacking. Entries are read through explicit byte limits and are
//! never written to disk.

use super::{
    MAX_BODY_BYTES, MAX_FRONTMATTER_BYTES, MAX_RESOURCE_BYTES, MAX_UNPACKED_BYTES, MAX_ZIP_BYTES,
    PackageError, SKILL_FILE, skip,
};
use desk_agent_protocol::ai_agent::skill::{SkillSkipReason, SkillSkippedFile};
use std::io::Read;

const MAX_ENTRIES: usize = 512;
/// Larger expansion ratios are treated as a compression bomb.
const MAX_RATIO: u64 = 200;

pub(super) struct Unpacked {
    pub skill_md: String,
    pub files: Vec<(String, Vec<u8>)>,
    /// The single top-level directory that holds `SKILL.md`, if any.
    pub directory: Option<String>,
    pub skipped: Vec<SkillSkippedFile>,
}

fn ignored(name: &str) -> bool {
    name.starts_with("__MACOSX/") || name.rsplit('/').next() == Some(".DS_Store")
}

pub(super) fn unpack(bytes: &[u8]) -> Result<Unpacked, PackageError> {
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(PackageError::ZipTooLarge);
    }
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| PackageError::InvalidZip)?;
    if archive.len() > MAX_ENTRIES {
        return Err(PackageError::InvalidZip);
    }
    let mut names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| PackageError::InvalidZip)?;
        if !entry.is_dir() && !ignored(entry.name()) {
            names.push((index, entry.name().to_owned()));
        }
    }
    let directory = if names.iter().any(|(_, name)| name == SKILL_FILE) {
        None
    } else {
        let dir = names
            .iter()
            .find_map(|(_, name)| {
                let (dir, file) = name.split_once('/')?;
                (file == SKILL_FILE && !dir.is_empty()).then(|| dir.to_owned())
            })
            .ok_or(PackageError::MissingSkillFile)?;
        let prefix = format!("{dir}/");
        if !names.iter().all(|(_, name)| name.starts_with(&prefix)) {
            return Err(PackageError::MissingSkillFile);
        }
        Some(dir)
    };
    let prefix = directory.as_ref().map(|dir| format!("{dir}/"));
    let mut unpacked = Unpacked {
        skill_md: String::new(),
        files: Vec::new(),
        directory,
        skipped: Vec::new(),
    };
    let mut total = 0usize;
    let mut skill_md = None;
    for (index, name) in names {
        let path = match &prefix {
            Some(prefix) => name.strip_prefix(prefix.as_str()).unwrap_or(&name),
            None => &name,
        }
        .to_owned();
        let entry = archive
            .by_index(index)
            .map_err(|_| PackageError::InvalidZip)?;
        if entry.is_symlink() {
            unpacked
                .skipped
                .push(skip(path, SkillSkipReason::InvalidPath));
            continue;
        }
        if entry.compressed_size() > 0 && entry.size() / entry.compressed_size() > MAX_RATIO {
            return Err(PackageError::InvalidZip);
        }
        let limit = if path == SKILL_FILE {
            MAX_BODY_BYTES + MAX_FRONTMATTER_BYTES + 1024
        } else {
            MAX_RESOURCE_BYTES
        };
        if entry.size() > limit as u64 {
            if path == SKILL_FILE {
                return Err(PackageError::BodyTooLarge);
            }
            unpacked.skipped.push(skip(path, SkillSkipReason::TooLarge));
            continue;
        }
        let mut data = Vec::new();
        entry
            .take(limit as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|_| PackageError::InvalidZip)?;
        if data.len() > limit {
            if path == SKILL_FILE {
                return Err(PackageError::BodyTooLarge);
            }
            unpacked.skipped.push(skip(path, SkillSkipReason::TooLarge));
            continue;
        }
        total += data.len();
        if total > MAX_UNPACKED_BYTES {
            return Err(PackageError::UnpackedTooLarge);
        }
        if path == SKILL_FILE {
            skill_md = Some(
                String::from_utf8(data)
                    .map_err(|_| PackageError::InvalidFrontmatter("SKILL.md must be UTF-8"))?,
            );
        } else {
            unpacked.files.push((path, data));
        }
    }
    unpacked.skill_md = skill_md.ok_or(PackageError::MissingSkillFile)?;
    Ok(unpacked)
}
