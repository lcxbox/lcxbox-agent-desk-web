//! Content-addressed objects in private subdirectories of the locked vault.
//! Objects are published only by renaming a flushed temporary file from the
//! namespace's `tmp` directory; no object is ever written through `FILE_CREATE`
//! at its final name, so a present name always denotes a complete object.
use super::*;
use ::windows::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
};
use ::windows::{Win32::Foundation::ERROR_NO_MORE_FILES, core::HRESULT};
use std::{
    mem::offset_of,
    sync::atomic::{AtomicU64, Ordering},
};

const OBJECTS: &str = "objects";
const TEMPORARY: &str = "tmp";
const EXECUTORS: &str = "executors";
/// Bound on entries enumerated from one private directory.
const MAX_LISTED: usize = 1_000_000;

struct DirectoryEntry {
    name: String,
    len: u64,
    directory: bool,
}

impl PrivateDirectoryLock {
    /// Open (or create) a private child directory; existing directories are
    /// validated, never repaired.
    fn subdirectory(&self, parent: &File, leaf: &str, create: bool) -> io::Result<Option<File>> {
        let kind = if create {
            OpenKind::Directory
        } else {
            OpenKind::ExistingDirectory
        };
        let handle = match open_relative(parent, leaf, &self.user, kind) {
            Ok(handle) => handle,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        file_identity(&handle, FileKind::Directory)?;
        security::validate_private(&handle, &self.user)?;
        Ok(Some(handle))
    }

    fn namespace_directory(&self, namespace: &str, create: bool) -> io::Result<Option<File>> {
        if !crate::valid_namespace(namespace) {
            return Err(crate::invalid("invalid recovery object namespace"));
        }
        self.validate()?;
        let Some(objects) = self.subdirectory(&self._directory, OBJECTS, create)? else {
            return Ok(None);
        };
        self.subdirectory(&objects, namespace, create)
    }

    fn require_namespace(&self, namespace: &str) -> io::Result<File> {
        self.namespace_directory(namespace, false)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "recovery namespace is absent"))
    }

    /// Write, flush and rename a temporary file to the object name. Returns
    /// the replaced object's size when the name already existed.
    pub(crate) fn publish_object(
        &self,
        namespace: &str,
        name: &str,
        bytes: &[u8],
    ) -> io::Result<Option<u64>> {
        if self.write_uncertain.get() {
            return Err(io::Error::other("private index publication is unresolved"));
        }
        if !crate::valid_id(name) {
            return Err(crate::invalid("invalid recovery object name"));
        }
        let directory = self.namespace_directory(namespace, true)?.unwrap();
        let temporary = self.subdirectory(&directory, TEMPORARY, true)?.unwrap();
        let previous = match open_relative(&directory, name, &self.user, OpenKind::ReadFile) {
            Ok(file) => {
                file_identity(&file, FileKind::File)?;
                Some(file.metadata()?.len())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut file = open_relative(
            &temporary,
            &temporary_name(name)?,
            &self.user,
            OpenKind::Temporary,
        )?;
        let result = (|| {
            file_identity(&file, FileKind::File)?;
            security::validate_private_file(&file, &self.user)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            rename_into(&file, &directory, name)?;
            directory.sync_all()
        })();
        if let Err(error) = result {
            // The handle names only this invocation's file; an unreferenced
            // object is safe to remove even if the rename completed.
            let _ = delete_handle(&file);
            return Err(error);
        }
        Ok(previous)
    }

    /// Whether a published object of this name exists as a single-link file.
    pub(crate) fn object_exists(&self, namespace: &str, name: &str) -> io::Result<bool> {
        if !crate::valid_id(name) {
            return Err(crate::invalid("invalid recovery object name"));
        }
        let Some(directory) = self.namespace_directory(namespace, false)? else {
            return Ok(false);
        };
        match open_relative(&directory, name, &self.user, OpenKind::ReadFile) {
            Ok(file) => Ok(file_identity(&file, FileKind::File).is_ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn read_object(
        &self,
        namespace: &str,
        name: &str,
        limit: u64,
    ) -> io::Result<Vec<u8>> {
        if !crate::valid_id(name) {
            return Err(crate::invalid("invalid recovery object name"));
        }
        let directory = self.require_namespace(namespace)?;
        let file = open_relative(&directory, name, &self.user, OpenKind::ReadFile)?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        if file.metadata()?.len() > limit {
            return Err(crate::invalid("recovery object exceeds its read bound"));
        }
        let mut bytes = Vec::new();
        (&file).take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(crate::invalid("recovery object exceeds its read bound"));
        }
        Ok(bytes)
    }

    /// The first `len` bytes of an object, for reading its header.
    pub(crate) fn read_object_prefix(
        &self,
        namespace: &str,
        name: &str,
        len: u64,
    ) -> io::Result<Vec<u8>> {
        if !crate::valid_id(name) {
            return Err(crate::invalid("invalid recovery object name"));
        }
        let directory = self.require_namespace(namespace)?;
        let file = open_relative(&directory, name, &self.user, OpenKind::ReadFile)?;
        file_identity(&file, FileKind::File)?;
        let mut bytes = Vec::new();
        (&file).take(len).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn executor_name(id: &str) -> io::Result<String> {
        if !crate::valid_id(id) {
            return Err(crate::invalid("invalid recovery executor"));
        }
        Ok(format!("{id}.lock"))
    }

    /// Create and lock the lock file of a new executor. The returned handle
    /// holds the lock until it is dropped.
    pub(crate) fn create_executor(&self, id: &str) -> io::Result<File> {
        self.validate()?;
        let directory = self
            .subdirectory(&self._directory, EXECUTORS, true)?
            .unwrap();
        let file = open_relative(
            &directory,
            &Self::executor_name(id)?,
            &self.user,
            OpenKind::Lock,
        )?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recovery executor is already running",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    /// Whether an executor still holds its lock. The lock file of an exited
    /// executor is removed.
    pub(crate) fn executor_alive(&self, id: &str) -> io::Result<bool> {
        self.validate()?;
        let name = Self::executor_name(id)?;
        let Some(directory) = self.subdirectory(&self._directory, EXECUTORS, false)? else {
            return Ok(false);
        };
        let file = match open_relative(
            &directory,
            &name,
            &self.user,
            OpenKind::InspectTransactionFile,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        file_identity(&file, FileKind::File)?;
        match file.try_lock() {
            Ok(()) => {
                drop(file);
                remove_child(&directory, &name, &self.user)?;
                Ok(false)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    pub(crate) fn list_executors(&self) -> io::Result<Vec<String>> {
        self.validate()?;
        let Some(directory) = self.subdirectory(&self._directory, EXECUTORS, false)? else {
            return Ok(Vec::new());
        };
        Ok(list_entries(&directory)?
            .into_iter()
            .filter(|entry| !entry.directory)
            .filter_map(|entry| entry.name.strip_suffix(".lock").map(str::to_owned))
            .filter(|id| crate::valid_id(id))
            .collect())
    }

    pub(crate) fn remove_object(&self, namespace: &str, name: &str) -> io::Result<()> {
        let directory = self.require_namespace(namespace)?;
        remove_child(&directory, name, &self.user)
    }

    pub(crate) fn list_namespaces(&self) -> io::Result<Vec<String>> {
        self.validate()?;
        let Some(objects) = self.subdirectory(&self._directory, OBJECTS, false)? else {
            return Ok(Vec::new());
        };
        Ok(list_entries(&objects)?
            .into_iter()
            .filter(|entry| entry.directory && crate::valid_namespace(&entry.name))
            .map(|entry| entry.name)
            .collect())
    }

    pub(crate) fn list_objects(&self, namespace: &str) -> io::Result<Vec<(String, u64)>> {
        let directory = self.require_namespace(namespace)?;
        Ok(list_entries(&directory)?
            .into_iter()
            .filter(|entry| !entry.directory)
            .map(|entry| (entry.name, entry.len))
            .collect())
    }

    pub(crate) fn clear_temporary(&self, namespace: &str) -> io::Result<()> {
        let directory = self.require_namespace(namespace)?;
        let Some(temporary) = self.subdirectory(&directory, TEMPORARY, false)? else {
            return Ok(());
        };
        for entry in list_entries(&temporary)? {
            if !entry.directory {
                remove_child(&temporary, &entry.name, &self.user)?;
            }
        }
        Ok(())
    }

    /// Remove material of an earlier storage format from the vault root.
    /// Pending index files remain governed by index publication.
    pub(crate) fn remove_legacy_material(&self) -> io::Result<()> {
        self.validate()?;
        for entry in list_entries(&self._directory)? {
            if !entry.directory
                && [".body", ".metadata", ".tmp"]
                    .iter()
                    .any(|suffix| entry.name.ends_with(suffix))
            {
                remove_child(&self._directory, &entry.name, &self.user)?;
            }
        }
        self._directory.sync_all()
    }
}

fn remove_child(directory: &File, name: &str, user: &str) -> io::Result<()> {
    let file = open_relative(directory, name, user, OpenKind::DeleteFile)?;
    file_identity(&file, FileKind::File)?;
    delete_handle(&file)
}

fn delete_handle(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)
}

fn temporary_name(name: &str) -> io::Result<String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| io::Error::other("private temporary sequence exhausted"))?;
    // FILE_CREATE, not name unpredictability, guarantees non-overwrite.
    Ok(format!(
        "{}-{}-{sequence:x}",
        &name[..16],
        std::process::id()
    ))
}

/// Rename an open file into `directory`, replacing an existing name.
fn rename_into(file: &File, directory: &File, name: &str) -> io::Result<()> {
    let encoded = leaf_name(name)?;
    let size = size_of::<FILE_RENAME_INFORMATION>() + encoded.len() * 2 + 2;
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = HANDLE(directory.as_raw_handle());
        (*info).FileNameLength = (encoded.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast(),
            encoded.len(),
        );
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut status,
            info.cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if result.0 < 0 {
        return Err(io::Error::other(format!(
            "private object rename NTSTATUS {:#x}",
            result.0
        )));
    }
    Ok(())
}

/// Names of a directory's entries, for command snapshot traversal.
pub(crate) fn list_names(directory: &File) -> io::Result<Vec<String>> {
    Ok(list_entries(directory)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

/// Enumerate a directory through its handle; names are never resolved
/// through paths and reparse points are reported as plain entries.
fn list_entries(directory: &File) -> io::Result<Vec<DirectoryEntry>> {
    let mut entries = Vec::new();
    let mut batch = 0usize;
    loop {
        let mut buffer = vec![0u64; 8192];
        let query = unsafe {
            GetFileInformationByHandleEx(
                HANDLE(directory.as_raw_handle()),
                if batch == 0 {
                    FileIdBothDirectoryRestartInfo
                } else {
                    FileIdBothDirectoryInfo
                },
                buffer.as_mut_ptr().cast(),
                65_536,
            )
        };
        if let Err(error) = query {
            if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                return Ok(entries);
            }
            return Err(io::Error::other(error));
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), 65_536) };
        parse_entries(bytes, &mut entries)?;
        if entries.len() > MAX_LISTED {
            return Err(crate::invalid(
                "private directory listing exceeds its bound",
            ));
        }
        batch += 1;
    }
}

fn parse_entries(bytes: &[u8], entries: &mut Vec<DirectoryEntry>) -> io::Result<()> {
    let header = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let length_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
    let size_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile);
    let attributes_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes);
    let invalid = || crate::invalid("invalid directory information buffer");
    let mut offset = 0usize;
    loop {
        let entry = bytes.get(offset..).ok_or_else(invalid)?;
        if entry.len() < header {
            return Err(invalid());
        }
        let field = |at: usize| u32::from_le_bytes(entry[at..at + 4].try_into().unwrap());
        let next = field(0) as usize;
        let length = field(length_offset) as usize;
        if length == 0 || length > 512 || !length.is_multiple_of(2) {
            return Err(invalid());
        }
        let end = header + length;
        let raw = entry.get(header..end).ok_or_else(invalid)?;
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let name = String::from_utf16(&units).map_err(|_| invalid())?;
        let len = i64::from_le_bytes(entry[size_offset..size_offset + 8].try_into().unwrap());
        let attributes = field(attributes_offset);
        if name != "." && name != ".." {
            entries.push(DirectoryEntry {
                name,
                len: u64::try_from(len).map_err(|_| invalid())?,
                directory: attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
            });
        }
        if next == 0 {
            return Ok(());
        }
        if next < end || !next.is_multiple_of(8) || next >= entry.len() {
            return Err(invalid());
        }
        offset += next;
    }
}
