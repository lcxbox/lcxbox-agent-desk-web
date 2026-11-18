//! Linux handle-relative filesystem operations. Where `openat2` is filtered out
//! (systemd's `RestrictSUIDSGID=` answers it with `ENOSYS`), resolution falls
//! back to an equally strict component walk, never to a path-based open.
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

mod metadata;
pub use metadata::{capture_metadata, copy_metadata, make_private};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Resolve an existing entry beneath an already-authorized directory handle.
/// Symlinks, magic links and mount crossings are all rejected by the kernel.
pub fn open_beneath(directory: &File, path: &Path, directory_only: bool) -> io::Result<File> {
    open_beneath_flags(directory, path, directory_only, 0)
}

/// Text transaction reads must not change the atime stored in their metadata fence.
/// No fallback is permitted when the kernel denies O_NOATIME.
pub fn open_text_beneath(directory: &File, path: &Path) -> io::Result<File> {
    let file = open_beneath_flags(directory, path, false, libc::O_NOATIME)?;
    validate_source(&file)?;
    Ok(file)
}

fn open_beneath_flags(
    directory: &File,
    path: &Path,
    directory_only: bool,
    extra: i32,
) -> io::Result<File> {
    open_beneath_with(directory, path, directory_only, extra, openat2_beneath)
}

fn open_beneath_with(
    directory: &File,
    path: &Path,
    directory_only: bool,
    extra: i32,
    openat2: impl FnOnce(&File, &CString, i32) -> io::Result<File>,
) -> io::Result<File> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(super::invalid("a relative authorized path is required"));
    }
    let flags = extra
        | libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory_only { libc::O_DIRECTORY } else { 0 };
    let c_path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| super::invalid("invalid path"))?;
    let file = match openat2(directory, &c_path, flags) {
        Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
            walk_beneath(directory, path, flags)?
        }
        result => result?,
    };
    let stat = file.metadata()?;
    if !(if directory_only {
        stat.is_dir()
    } else {
        stat.is_file()
    }) {
        return Err(super::invalid("unsupported filesystem entry"));
    }
    Ok(file)
}

fn openat2_beneath(directory: &File, path: &CString, flags: i32) -> io::Result<File> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV.
        resolve: 0x08 | 0x04 | 0x01,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

/// The same guarantees as `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
/// | RESOLVE_NO_XDEV`, one component at a time: each component is opened
/// relative to the handle of its parent, so no path is re-resolved; only plain
/// names are accepted, so nothing can climb out; `O_NOFOLLOW` refuses every
/// symlink; and every handle must stay on the starting mount.
fn walk_beneath(directory: &File, path: &Path, flags: i32) -> io::Result<File> {
    use std::path::Component;
    let names = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => {
                CString::new(name.as_bytes()).map_err(|_| super::invalid("invalid path"))
            }
            _ => Err(super::invalid(
                "a plain relative authorized path is required",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let Some((last, parents)) = names.split_last() else {
        return Err(super::invalid("a relative authorized path is required"));
    };
    let mount = mount_id(directory)?;
    let mut current: Option<File> = None;
    for name in parents {
        let parent = current.as_ref().unwrap_or(directory);
        let next = openat(
            parent,
            name,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )?;
        same_mount(&next, mount)?;
        current = Some(next);
    }
    let file = openat(current.as_ref().unwrap_or(directory), last, flags)?;
    same_mount(&file, mount)?;
    Ok(file)
}

fn openat(directory: &File, name: &CString, flags: i32) -> io::Result<File> {
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn same_mount(file: &File, mount: u64) -> io::Result<()> {
    if mount_id(file)? != mount {
        // What RESOLVE_NO_XDEV reports for a mount crossing.
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(())
}

/// The mount a handle lives on; a bind mount of the same filesystem is a
/// different mount even with an identical `st_dev`.
fn mount_id(file: &File) -> io::Result<u64> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(super::invalid(
            "the kernel does not report mount identities",
        ));
    }
    Ok(stat.stx_mnt_id)
}

/// Only an unelevated owner may mutate a single-link ordinary source file.
pub fn validate_source(file: &File) -> io::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let stat = file.metadata()?;
    if uid == 0
        || uid != unsafe { libc::getuid() }
        || !stat.is_file()
        || stat.uid() != uid
        || stat.nlink() != 1
        || stat.mode() & 0o7000 != 0
    {
        return Err(super::invalid(
            "text mutation requires an owned single-link regular file without special mode bits",
        ));
    }
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Explicit local filesystem scope; a successful rename alone does not
    // establish the durability semantics of remote or userspace filesystems.
    if !matches!(fs.f_type as u64, 0xef53 | 0x58465342 | 0x9123683e) {
        return Err(super::invalid(
            "text mutation requires a supported local ext4, XFS or Btrfs filesystem",
        ));
    }
    Ok(())
}

pub fn exchange(directory: &File, source: &str, staged: &str) -> io::Result<()> {
    rename(directory, source, staged, libc::RENAME_EXCHANGE)
}

pub fn move_no_replace(directory: &File, source: &str, destination: &str) -> io::Result<()> {
    rename(directory, source, destination, libc::RENAME_NOREPLACE)
}

fn rename(directory: &File, source: &str, destination: &str, flags: u32) -> io::Result<()> {
    fn name(value: &str) -> io::Result<CString> {
        if value.is_empty() || value == "." || value == ".." || value.contains('/') {
            return Err(super::invalid("rename requires a single entry name"));
        }
        CString::new(value).map_err(|_| super::invalid("invalid entry name"))
    }
    let source = name(source)?;
    let destination = name(destination)?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(_: &File, _: &CString, _: i32) -> io::Result<File> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

    /// systemd's `RestrictSUIDSGID=` answers `openat2` with ENOSYS; the walk that
    /// replaces it must refuse everything `openat2` would.
    #[test]
    fn a_filtered_openat2_falls_back_to_an_equally_strict_walk() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("text"), b"hello").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/inner"), b"inner").unwrap();
        std::os::unix::fs::symlink(root.path().join("text"), root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(root.path().join("sub"), root.path().join("sublink")).unwrap();
        let directory = File::open(root.path()).unwrap();
        let open = |path: &str, directory_only: bool| {
            open_beneath_with(&directory, Path::new(path), directory_only, 0, filtered)
        };

        let mut text = String::new();
        open("text", false)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
        let mut inner = String::new();
        open("sub/inner", false)
            .unwrap()
            .read_to_string(&mut inner)
            .unwrap();
        assert_eq!(inner, "inner");
        assert!(open("sub", true).unwrap().metadata().unwrap().is_dir());

        for refused in [
            "link",
            "sublink/inner",
            "..",
            "sub/../text",
            "./text",
            "missing",
        ] {
            assert!(open(refused, false).is_err(), "{refused} must be refused");
        }
        assert!(open("sub", false).is_err(), "a directory is not a file");
        assert!(open("text", true).is_err(), "a file is not a directory");
    }

    #[test]
    fn only_an_unavailable_openat2_falls_back() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("text"), b"hello").unwrap();
        let directory = File::open(root.path()).unwrap();
        let denied = open_beneath_with(&directory, Path::new("text"), false, 0, |_, _, _| {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!(denied.raw_os_error(), Some(libc::EACCES));
    }

    #[test]
    fn the_walk_refuses_another_mount() {
        let root = tempfile::tempdir().unwrap();
        let directory = File::open(root.path()).unwrap();
        let mount = mount_id(&directory).unwrap();
        assert!(same_mount(&directory, mount).is_ok());
        let elsewhere = File::open("/proc/self/status").unwrap();
        assert_eq!(
            same_mount(&elsewhere, mount).unwrap_err().raw_os_error(),
            Some(libc::EXDEV)
        );
    }

    #[test]
    fn the_walk_keeps_noatime_reads_from_touching_metadata() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text");
        std::fs::write(&path, b"content").unwrap();
        let file = File::open(&path).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        file.set_times(std::fs::FileTimes::new().set_accessed(old))
            .unwrap();
        let before = capture_metadata(&file, "text").unwrap();
        let directory = File::open(root.path()).unwrap();
        let mut opened = open_beneath_with(
            &directory,
            Path::new("text"),
            false,
            libc::O_NOATIME,
            filtered,
        )
        .unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"content");
        assert_eq!(capture_metadata(&opened, "text").unwrap(), before);
    }

    #[test]
    fn beneath_rejects_symlinks_and_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("text"), b"hello").unwrap();
        std::os::unix::fs::symlink("text", root.path().join("link")).unwrap();
        let directory = File::open(root.path()).unwrap();
        assert!(open_beneath(&directory, Path::new("text"), false).is_ok());
        assert!(open_beneath(&directory, Path::new("link"), false).is_err());
        assert!(open_beneath(&directory, Path::new(".."), true).is_err());
    }
    #[test]
    fn noatime_reads_preserve_metadata_even_when_relatime_would_update_it() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text");
        std::fs::write(&path, b"content").unwrap();
        let file = File::open(&path).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        file.set_times(std::fs::FileTimes::new().set_accessed(old))
            .unwrap();
        let before = capture_metadata(&file, "text").unwrap();
        let directory = File::open(root.path()).unwrap();
        let mut opened =
            open_beneath_flags(&directory, Path::new("text"), false, libc::O_NOATIME).unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"content");
        assert_eq!(capture_metadata(&opened, "text").unwrap(), before);
    }

    #[test]
    fn no_replace_never_overwrites_and_exchange_swaps_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"original").unwrap();
        std::fs::write(root.path().join("b"), b"staged").unwrap();
        let directory = File::open(root.path()).unwrap();
        assert!(move_no_replace(&directory, "a", "b").is_err());
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"staged");
        exchange(&directory, "a", "b").unwrap();
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"staged");
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"original");
    }
}
