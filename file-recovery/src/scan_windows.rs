//! Handle-relative traversal of snapshot roots on Windows. Every child is
//! opened relative to its parent handle with `FILE_OPEN_REPARSE_POINT`, so
//! reparse points (links, junctions, mount points) are reported as `Other`
//! instead of being followed. Handles share read, write and delete access, so
//! a scan never blocks the programs that use the files.
use ::windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_DISPOSITION,
            NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{
            HANDLE, OBJ_CASE_INSENSITIVE, STATUS_OBJECT_NAME_COLLISION,
            STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
    core::PWSTR,
};
use std::{
    fs::File,
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle},
};

/// 100-nanosecond intervals between 1601-01-01 and 1970-01-01.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    /// Volume serial; children on another volume are not traversed.
    pub device: u64,
    pub file_id: String,
    pub links: u64,
}

fn unix_ns(filetime: i64) -> i64 {
    filetime
        .saturating_sub(UNIX_EPOCH_FILETIME)
        .saturating_mul(100)
}

fn query<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut info = T::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            class,
            (&mut info as *mut T).cast(),
            size_of::<T>() as u32,
        )
    }
    .map_err(io::Error::other)?;
    Ok(info)
}

pub(crate) fn stat(file: &File) -> io::Result<Stat> {
    let basic: FILE_BASIC_INFO = query(file, FileBasicInfo)?;
    let standard: FILE_STANDARD_INFO = query(file, FileStandardInfo)?;
    let id: FILE_ID_INFO = query(file, FileIdInfo)?;
    let kind = if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        Kind::Other
    } else if standard.Directory {
        Kind::Directory
    } else {
        Kind::File
    };
    Ok(Stat {
        kind,
        size: u64::try_from(standard.EndOfFile).unwrap_or(0),
        mtime_ns: unix_ns(basic.LastWriteTime),
        ctime_ns: unix_ns(basic.ChangeTime),
        device: id.VolumeSerialNumber,
        file_id: format!(
            "{:x}:{}",
            id.VolumeSerialNumber,
            id.FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        links: u64::from(standard.NumberOfLinks),
    })
}

fn component(name: &str) -> io::Result<Vec<u16>> {
    // `:` would name an alternate data stream of the parent.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 1024
        || name.chars().any(|c| c.is_control() || "\\/:".contains(c))
    {
        return Err(crate::invalid("expected one directory entry name"));
    }
    Ok(name.encode_utf16().collect())
}

fn open_relative(
    parent: &File,
    name: &str,
    access: FILE_ACCESS_RIGHTS,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
) -> io::Result<File> {
    let mut name = component(name)?;
    let text = UNICODE_STRING {
        Length: (name.len() * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(parent.as_raw_handle()),
        ObjectName: &text,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if result.0 < 0 {
        let kind = match result {
            STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => io::ErrorKind::NotFound,
            STATUS_OBJECT_NAME_COLLISION => io::ErrorKind::AlreadyExists,
            _ => io::ErrorKind::Other,
        };
        return Err(io::Error::new(
            kind,
            format!("snapshot open NTSTATUS {:#x}", result.0),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// Metadata of a child without following a reparse point; `None` when it
/// vanished.
pub(crate) fn stat_at(directory: &File, name: &str) -> io::Result<Option<Stat>> {
    match open_relative(
        directory,
        name,
        FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        NTCREATEFILE_CREATE_OPTIONS(0),
    ) {
        Ok(file) => stat(&file).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn list(directory: &File) -> io::Result<Vec<String>> {
    crate::windows::list_names(directory)
}

const DIRECTORY_ACCESS: FILE_ACCESS_RIGHTS =
    FILE_ACCESS_RIGHTS(FILE_LIST_DIRECTORY.0 | FILE_TRAVERSE.0 | FILE_READ_ATTRIBUTES.0);

/// Open a child file or directory without following reparse points.
pub(crate) fn open(directory: &File, name: &str, want_directory: bool) -> io::Result<File> {
    let file = if want_directory {
        open_relative(
            directory,
            name,
            DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?
    } else {
        open_relative(
            directory,
            name,
            FILE_GENERIC_READ,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
        )?
    };
    let kind = stat(&file)?.kind;
    if kind
        != if want_directory {
            Kind::Directory
        } else {
            Kind::File
        }
    {
        return Err(crate::invalid("unexpected filesystem entry type"));
    }
    Ok(file)
}

/// Open the directory `path` (relative, `/`-separated) beneath `root`,
/// creating each missing component as a new directory that inherits its
/// parent's security. Existing components are opened without following
/// reparse points. The returned handle can create children.
pub fn create_directories(root: &File, path: &str) -> io::Result<File> {
    crate::validate_rel_path(path, false)?;
    let access = FILE_ACCESS_RIGHTS(
        crate::windows::MUTATION_DIRECTORY_ACCESS.0 | FILE_TRAVERSE.0 | FILE_LIST_DIRECTORY.0,
    );
    let mut current = root.try_clone()?;
    for part in path.split('/') {
        let next = match open_relative(&current, part, access, FILE_CREATE, FILE_DIRECTORY_FILE) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                open_relative(&current, part, access, FILE_OPEN, FILE_DIRECTORY_FILE)?
            }
            Err(error) => return Err(error),
        };
        if stat(&next)?.kind != Kind::Directory {
            return Err(crate::invalid("restore parent is not a plain directory"));
        }
        current = next;
    }
    Ok(current)
}
