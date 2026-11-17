//! Handle-relative traversal of snapshot roots on Unix. Entries are inspected
//! with `fstatat(AT_SYMLINK_NOFOLLOW)` and opened one component at a time
//! without following links: through `openat2` beneath the parent on Linux and
//! `openat(O_NOFOLLOW)` on macOS. Devices, sockets and FIFOs are never opened.
use std::{
    ffi::{CStr, CString},
    fs::File,
    io,
    os::fd::AsRawFd,
};

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
    /// Filesystem identity; children on another device are not traversed.
    pub device: u64,
    pub file_id: String,
    pub links: u64,
}

fn component(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(crate::invalid("expected one directory entry name"));
    }
    CString::new(name).map_err(|_| crate::invalid("invalid directory entry name"))
}

#[allow(
    clippy::unnecessary_cast,
    reason = "stat field widths differ by platform"
)]
fn from_stat(stat: &libc::stat) -> Stat {
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Directory,
        _ => Kind::Other,
    };
    let ns = |seconds: i64, nanos: i64| seconds.saturating_mul(1_000_000_000).saturating_add(nanos);
    Stat {
        kind,
        size: stat.st_size as u64,
        mtime_ns: ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        ctime_ns: ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        device: stat.st_dev as u64,
        file_id: format!("{:x}:{:x}", stat.st_dev as u64, stat.st_ino as u64),
        links: stat.st_nlink as u64,
    }
}

pub(crate) fn stat(file: &File) -> io::Result<Stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(from_stat(&stat))
}

/// Metadata of a child without following a link; `None` when it vanished.
pub(crate) fn stat_at(directory: &File, name: &str) -> io::Result<Option<Stat>> {
    let name = component(name)?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(error),
        };
    }
    Ok(Some(from_stat(&stat)))
}

fn clear_errno() {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
}

/// Names of a directory's entries. Names that are not UTF-8 cannot be
/// represented in recovery records and are left out.
pub(crate) fn list(directory: &File) -> io::Result<Vec<String>> {
    // A fresh open file description keeps the listing offset independent of
    // the caller's handle, which may be listed again later.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let mut names = Vec::new();
    let result = loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            break match error.raw_os_error() {
                Some(0) | None => Ok(()),
                Some(_) => Err(error),
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if let Ok(name) = name.to_str()
            && name != "."
            && name != ".."
        {
            names.push(name.to_owned());
        }
    };
    unsafe { libc::closedir(stream) };
    result.map(|()| names)
}

/// Open a child file or directory without following links.
pub(crate) fn open(directory: &File, name: &str, want_directory: bool) -> io::Result<File> {
    let file = open_child(directory, name, want_directory)?;
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

#[cfg(target_os = "linux")]
fn open_child(directory: &File, name: &str, want_directory: bool) -> io::Result<File> {
    component(name)?;
    crate::linux::open_beneath(directory, std::path::Path::new(name), want_directory)
}

#[cfg(not(target_os = "linux"))]
fn open_child(directory: &File, name: &str, want_directory: bool) -> io::Result<File> {
    let name = component(name)?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if want_directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
}

/// Open the directory `path` (relative, `/`-separated) beneath `root`,
/// creating each missing component as a new directory. Existing components
/// are opened without following links, so a link planted in the path is
/// rejected instead of being written through.
pub fn create_directories(root: &File, path: &str) -> io::Result<File> {
    crate::validate_rel_path(path, false)?;
    let mut current = root.try_clone()?;
    for part in path.split('/') {
        let name = component(part)?;
        if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        current = open(&current, part, true)?;
    }
    Ok(current)
}
