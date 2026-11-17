//! Handle-relative command restore writes on Linux and macOS. Every path
//! component is opened with `O_NOFOLLOW` beneath the root handle; content is
//! staged in the same directory and committed with one rename.
use super::super::text_recovery_context::digest;
use super::Applied;
use super::*;
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

pub(super) struct Root(File);

pub(super) fn open_directory(path: &Path) -> Result<OpenedFile, AgentError> {
    open_verified(path)
}

pub(super) fn open_root(directory: &super::CommandDirectory) -> Result<Root, AgentError> {
    directory
        .handle
        .try_clone()
        .map(Root)
        .map_err(|cause| io_error("open the snapshot root", cause))
}

fn name(part: &str) -> std::io::Result<CString> {
    if part.is_empty() || part == "." || part == ".." || part.contains('/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid path component",
        ));
    }
    CString::new(part)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))
}

fn open_at(directory: &File, part: &str, flags: i32) -> std::io::Result<File> {
    let part = name(part)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            part.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The parent directory of `path` and its leaf name. Missing directories are
/// created (without following links) only when `create` is set.
fn parent<'a>(root: &Root, path: &'a str, create: bool) -> std::io::Result<(File, &'a str)> {
    let Some((directory, leaf)) = path.rsplit_once('/') else {
        return Ok((root.0.try_clone()?, path));
    };
    if create {
        return Ok((
            desk_file_recovery::create_directories(&root.0, directory)?,
            leaf,
        ));
    }
    let mut current = root.0.try_clone()?;
    for part in directory.split('/') {
        current = open_at(&current, part, libc::O_RDONLY | libc::O_DIRECTORY)?;
    }
    Ok((current, leaf))
}

/// The regular single-link file at `leaf`, or `None` when nothing is there.
fn open_leaf(parent: &File, leaf: &str) -> std::io::Result<Option<File>> {
    let file = match open_at(parent, leaf, libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(file) => file,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(cause),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a single-link regular file",
        ));
    }
    Ok(Some(file))
}

fn read_all(file: &File) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::Start(0))?;
    handle
        .take(MAX_TEXT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TEXT_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds the text size limit",
        ));
    }
    Ok(bytes)
}

/// Current content of `path` beneath the root; `None` when it is absent.
pub(super) fn read(root: &Root, path: &str) -> std::io::Result<Option<Vec<u8>>> {
    let (parent, leaf) = match parent(root, path, false) {
        Ok(found) => found,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(cause),
    };
    open_leaf(&parent, leaf)?
        .map(|file| read_all(&file))
        .transpose()
}

/// Whether `leaf` still names the file behind `file`.
fn names(parent: &File, leaf: &str, file: &File) -> bool {
    let Ok(Some(current)) = open_leaf(parent, leaf) else {
        return false;
    };
    match (current.metadata(), file.metadata()) {
        (Ok(current), Ok(file)) => current.dev() == file.dev() && current.ino() == file.ino(),
        _ => false,
    }
}

fn unlink(parent: &File, leaf: &str) -> std::io::Result<()> {
    let leaf = name(leaf)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn rename_replace(parent: &File, from: &str, to: &str) -> std::io::Result<()> {
    let (from, to) = (name(from)?, name(to)?);
    let fd = parent.as_raw_fd();
    if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_no_replace(parent: &File, from: &str, to: &str) -> std::io::Result<()> {
    desk_file_recovery::linux::move_no_replace(parent, from, to)
}

#[cfg(target_os = "macos")]
fn rename_no_replace(parent: &File, from: &str, to: &str) -> std::io::Result<()> {
    let (from, to) = (name(from)?, name(to)?);
    let fd = parent.as_raw_fd();
    if unsafe { libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Write `content` to a new file named `stage` beside the target. A replaced
/// file keeps its permission bits; a recreated one gets the default
/// permissions of a new file.
fn stage(parent: &File, stage: &str, content: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let leaf = name(stage)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let written = (|| {
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(content)?;
        file.sync_all()
    })();
    if written.is_err() {
        let _ = unlink(parent, stage);
    }
    written
}

/// Restore one file: replace (`expected` and `content`), remove (`expected`
/// only) or recreate (`content` only). The expected state is checked again
/// right before the change.
pub(super) fn apply(
    root: &Root,
    path: &str,
    expected: Option<&str>,
    content: Option<&[u8]>,
    stage_name: &str,
) -> Applied {
    let (parent, leaf) = match parent(root, path, expected.is_none()) {
        Ok(found) => found,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Applied::Conflict,
        Err(_) => return Applied::NotStarted,
    };
    let current = match open_leaf(&parent, leaf) {
        Ok(current) => current,
        Err(_) => return Applied::Conflict,
    };
    match (&current, expected) {
        (None, None) => {}
        (Some(file), Some(sha256)) => match read_all(file) {
            Ok(bytes) if digest(&bytes) == sha256 => {}
            _ => return Applied::Conflict,
        },
        _ => return Applied::Conflict,
    }
    let applied = match (current, content) {
        (Some(file), None) => {
            if !names(&parent, leaf, &file) {
                return Applied::Conflict;
            }
            unlink(&parent, leaf)
        }
        (Some(file), Some(content)) => {
            let mode = match file.metadata() {
                Ok(metadata) => metadata.mode() & 0o777,
                Err(_) => return Applied::NotStarted,
            };
            if stage(&parent, stage_name, content, Some(mode)).is_err() {
                return Applied::NotStarted;
            }
            if !names(&parent, leaf, &file) {
                let _ = unlink(&parent, stage_name);
                return Applied::Conflict;
            }
            rename_replace(&parent, stage_name, leaf)
        }
        (None, Some(content)) => {
            if stage(&parent, stage_name, content, None).is_err() {
                return Applied::NotStarted;
            }
            match rename_no_replace(&parent, stage_name, leaf) {
                Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = unlink(&parent, stage_name);
                    return Applied::Conflict;
                }
                other => other,
            }
        }
        (None, None) => return Applied::NotStarted,
    };
    if applied.is_err() {
        // A failed rename or unlink changed nothing; only the staged copy is
        // left to remove.
        let _ = unlink(&parent, stage_name);
        return Applied::NotStarted;
    }
    if let Err(cause) = parent.sync_all() {
        tracing::warn!(error_kind = ?cause.kind(), "command restore succeeded but directory sync failed");
    }
    Applied::Done
}
