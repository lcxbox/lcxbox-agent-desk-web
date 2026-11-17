//! Command restore writes on Windows. The root and every directory on the
//! way to a file are held open without delete sharing, so no component can be
//! renamed or replaced during a write; reparse points are rejected component
//! by component. Content is staged beside the target and moved into place by
//! handle without replacing anything; a replaced file is first moved aside by
//! its own verified handle and then deleted.
use super::super::text_recovery_context::{digest, directory_identity};
use super::Applied;
use super::*;
use desk_file_recovery::windows::{
    FileIdentity as NativeIdentity, FileKind, MUTATION_DIRECTORY_ACCESS, MoveOutcome,
    file_identity, move_no_replace,
};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FileDispositionInfo, SetFileInformationByHandle,
};

pub(super) struct Root {
    path: PathBuf,
    /// Keeps the root and its ancestors in place while the restore runs.
    _pinned: (OpenedFile, Vec<File>),
    /// A handle beneath which missing directories are created.
    handle: File,
}

pub(super) fn open_directory(path: &Path) -> Result<OpenedFile, AgentError> {
    open_verified_with_access(
        path,
        (FILE_GENERIC_READ | FILE_TRAVERSE).0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0,
    )
}

pub(super) fn open_root(directory: &super::CommandDirectory) -> Result<Root, AgentError> {
    let (pinned, ancestors, _) =
        windows_path_anchor::open_anchored(&directory.path, FileKind::Directory)?;
    if directory_identity(&pinned.identity) != directory.identity {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "the directory was replaced after it was selected",
            false,
        ));
    }
    Ok(Root {
        path: directory.path.clone(),
        _pinned: (pinned, ancestors),
        handle: directory
            .handle
            .try_clone()
            .map_err(|cause| io_error("open the snapshot root", cause))?,
    })
}

fn invalid() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid restore path")
}

/// The native path of the parent of `path` and its leaf name.
fn locate<'a>(root: &Root, path: &'a str) -> std::io::Result<(PathBuf, Option<&'a str>, &'a str)> {
    let (directory, leaf) = match path.rsplit_once('/') {
        Some((directory, leaf)) => (Some(directory), leaf),
        None => (None, path),
    };
    let mut parent = root.path.clone();
    for part in directory
        .into_iter()
        .flat_map(|directory| directory.split('/'))
    {
        parent.push(checked(part)?);
    }
    Ok((parent, directory, checked(leaf)?))
}

fn checked(part: &str) -> std::io::Result<&str> {
    if part.is_empty()
        || part.len() > 255
        || matches!(part, "." | "..")
        || part.ends_with(['.', ' '])
        || part
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
    {
        return Err(invalid());
    }
    Ok(part)
}

fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(ref cause) if cause.kind() == std::io::ErrorKind::NotFound)
}

/// The file at `path` opened for reading and deletion with only read sharing,
/// so it cannot change while it is held; `None` when nothing is there.
fn open_current(path: &Path) -> std::io::Result<Option<OpenedFile>> {
    if absent(path) {
        return Ok(None);
    }
    let opened = open_verified_with_access(path, (FILE_GENERIC_READ | DELETE).0, FILE_SHARE_READ.0)
        .map_err(|cause| std::io::Error::other(cause.message))?;
    if !opened.metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    Ok(Some(opened))
}

fn read_all(file: &File) -> std::io::Result<Vec<u8>> {
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
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
    let (parent, _, leaf) = locate(root, path)?;
    if absent(&parent) {
        return Ok(None);
    }
    let _anchors = windows_path_anchor::open_anchored(&parent, FileKind::Directory)
        .map_err(|cause| std::io::Error::other(cause.message))?;
    open_current(&parent.join(leaf))?
        .map(|file| read_all(&file.handle))
        .transpose()
}

/// Mark an open file for deletion; it disappears when its last handle closes.
fn delete_on_close(file: &File) -> std::io::Result<()> {
    let delete = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&delete as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|cause| std::io::Error::other(cause.to_string()))
}

/// Write `content` to a new file `stage` in `parent`; it inherits the
/// directory's permissions like any new file.
fn stage(parent: &Path, stage: &str, content: &[u8]) -> std::io::Result<(File, NativeIdentity)> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(parent.join(stage))?;
    let staged = (|| {
        file.write_all(content)?;
        file.sync_all()?;
        file_identity(&file, FileKind::File)
    })();
    match staged {
        Ok(identity) => Ok((file, identity)),
        Err(cause) => {
            let _ = delete_on_close(&file);
            Err(cause)
        }
    }
}

fn moved(result: Result<(), desk_file_recovery::windows::MoveError>) -> Result<(), Applied> {
    result.map_err(|cause| match cause.outcome {
        MoveOutcome::NotStarted => Applied::NotStarted,
        MoveOutcome::OutcomeUnknown => Applied::Unknown,
    })
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
    let Ok((parent_path, directory, leaf)) = locate(root, path) else {
        return Applied::Conflict;
    };
    if expected.is_none()
        && let Some(directory) = directory
        && desk_file_recovery::create_directories(&root.handle, directory).is_err()
    {
        return Applied::NotStarted;
    }
    if absent(&parent_path) {
        return Applied::Conflict;
    }
    let Ok((_anchor, _ancestors, parent_id)) =
        windows_path_anchor::open_anchored(&parent_path, FileKind::Directory)
    else {
        return Applied::NotStarted;
    };
    let Ok(parent) = open_verified_with_access(
        &parent_path,
        MUTATION_DIRECTORY_ACCESS.0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE).0,
    ) else {
        return Applied::NotStarted;
    };
    if file_identity(&parent.handle, FileKind::Directory).ok() != Some(parent_id) {
        return Applied::Conflict;
    }
    let current = match open_current(&parent_path.join(leaf)) {
        Ok(current) => current,
        Err(_) => return Applied::Conflict,
    };
    match (&current, expected) {
        (None, None) => {}
        (Some(file), Some(sha256)) => match read_all(&file.handle) {
            Ok(bytes) if digest(&bytes) == sha256 => {}
            _ => return Applied::Conflict,
        },
        _ => return Applied::Conflict,
    }
    let result = match (current, content) {
        (Some(file), None) => delete_on_close(&file.handle).map_err(|_| Applied::NotStarted),
        (None, Some(content)) => {
            let Ok((staged, staged_id)) = stage(&parent_path, stage_name, content) else {
                return Applied::NotStarted;
            };
            let result = moved(move_no_replace(
                &staged,
                staged_id,
                &parent.handle,
                parent_id,
                leaf,
            ));
            if result == Err(Applied::NotStarted) {
                let _ = delete_on_close(&staged);
            }
            result
        }
        (Some(file), Some(content)) => replace(
            &parent,
            parent_id,
            &parent_path,
            leaf,
            file,
            content,
            stage_name,
        ),
        (None, None) => Err(Applied::NotStarted),
    };
    match result {
        Ok(()) => {
            if let Err(cause) = parent.handle.sync_all() {
                tracing::warn!(error_kind = ?cause.kind(), "command restore succeeded but directory flush failed");
            }
            Applied::Done
        }
        Err(applied) => applied,
    }
}

/// Move the verified file aside by its own handle, move the staged content to
/// its name, then delete the old file. A failed second move puts the old file
/// back.
fn replace(
    parent: &OpenedFile,
    parent_id: NativeIdentity,
    parent_path: &Path,
    leaf: &str,
    current: OpenedFile,
    content: &[u8],
    stage_name: &str,
) -> Result<(), Applied> {
    let current_id =
        file_identity(&current.handle, FileKind::File).map_err(|_| Applied::Conflict)?;
    let (staged, staged_id) =
        stage(parent_path, stage_name, content).map_err(|_| Applied::NotStarted)?;
    let aside = format!("{stage_name}-replaced");
    if let Err(applied) = moved(move_no_replace(
        &current.handle,
        current_id,
        &parent.handle,
        parent_id,
        &aside,
    )) {
        if applied == Applied::NotStarted {
            let _ = delete_on_close(&staged);
        }
        return Err(applied);
    }
    match moved(move_no_replace(
        &staged,
        staged_id,
        &parent.handle,
        parent_id,
        leaf,
    )) {
        Ok(()) => {
            if delete_on_close(&current.handle).is_err() {
                tracing::warn!("restored file's replaced version could not be removed");
            }
            Ok(())
        }
        Err(Applied::NotStarted) => {
            let _ = delete_on_close(&staged);
            moved(move_no_replace(
                &current.handle,
                current_id,
                &parent.handle,
                parent_id,
                leaf,
            ))
            .map_err(|_| Applied::Unknown)?;
            Err(Applied::NotStarted)
        }
        Err(applied) => Err(applied),
    }
}
