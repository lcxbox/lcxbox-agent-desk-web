use super::denied;
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

pub(crate) fn check(path: &Path) -> io::Result<()> {
    let dir = path.parent().ok_or_else(denied)?;
    let uid = unsafe { libc::geteuid() };
    for item in [path, dir] {
        let m = fs::symlink_metadata(item)?;
        if m.uid() != uid
            || m.mode() & 0o077 != 0
            || (item == path && (!m.is_file() || m.nlink() != 1))
            || (item == dir && !m.is_dir())
        {
            return Err(denied());
        }
        check_acl(item)?;
    }
    for ancestor in dir.ancestors().skip(1) {
        check_acl(ancestor)?;
        let m = fs::metadata(ancestor)?;
        if (m.uid() != 0 && m.uid() != uid) || (m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0) {
            return Err(denied());
        }
    }
    Ok(())
}

// On Linux the group class masks all named POSIX ACL entries. With no group
// access on the containing directory, inherited ACLs cannot open a path to it.
#[cfg(target_os = "linux")]
fn check_acl(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn check_acl(path: &Path) -> io::Result<()> {
    use std::{
        ffi::{c_int, c_void},
        os::fd::AsRawFd,
        ptr,
    };
    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }
    let file = fs::File::open(path)?;
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP) => Ok(()),
            _ => Err(error),
        };
    }
    let mut entry = ptr::null_mut();
    let mut cursor = 0;
    let result = loop {
        if unsafe { acl_get_entry(acl, cursor, &mut entry) } != 0 {
            let error = io::Error::last_os_error();
            break if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let mut tag = 0;
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 || tag != 2 {
            break Err(denied());
        }
        cursor = -1;
    };
    unsafe { acl_free(acl) };
    result
}
