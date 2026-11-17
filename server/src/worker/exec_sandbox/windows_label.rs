//! Low mandatory labels on write roots (plan 6.5.3, part two), after
//! `pocs/windows-exec-sandbox` (research 8.7, 8.8).
//!
//! Labels are set and removed with `TreeSetNamedSecurityInfoW`: its progress
//! callback reports every object it could not update, whereas
//! `SetNamedSecurityInfoW` returns success and silently skips a subtree the
//! worker has no `WRITE_OWNER` on (research 8.8). The root's own label changes
//! only after the descendants were processed, so an explicit label on the root
//! means the last propagation completed.
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    PROG_INVOKE_SETTING, ProgressInvokeEveryObject, SDDL_REVISION_1, SE_FILE_OBJECT,
    TREE_SEC_INFO_SET, TreeSetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACE_FLAGS, ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE, GetLengthSid,
    InitializeAcl, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_NAME_NORMALIZED, FILE_REMOTE_PROTOCOL_INFO,
    FileRemoteProtocolInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, GetVolumeInformationByHandleW, VOLUME_NAME_DOS,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::windows_token::Sid;

/// `FILE_PERSISTENT_ACLS`.
const PERSISTENT_ACLS: u32 = 0x0000_0008;
/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP`.
const NO_WRITE_UP: u32 = 0x1;

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win(error: windows::core::Error, what: &str) -> io::Error {
    io::Error::other(format!("{what}: {}", error.message().trim()))
}

/// The real path of an opened directory, without the `\\?\` prefix.
pub(crate) fn real_path(handle: &File) -> io::Result<PathBuf> {
    let h = HANDLE(handle.as_raw_handle());
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h,
                &mut buf,
                windows::Win32::Storage::FileSystem::GETFINALPATHNAMEBYHANDLE_FLAGS(
                    FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0,
                ),
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            let s = String::from_utf16_lossy(&buf[..n]);
            let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{rest}")
            } else if let Some(rest) = s.strip_prefix(r"\\?\") {
                rest.to_owned()
            } else {
                s
            };
            return Ok(PathBuf::from(s));
        }
        buf.resize(n + 1, 0);
    }
}

/// Volume serial number and file index of an opened object.
pub(crate) fn identity(handle: &File) -> io::Result<(u32, u64)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &mut info) }
        .map_err(|e| win(e, "GetFileInformationByHandle"))?;
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Refuse roots whose volume cannot carry the label (no persistent ACLs:
/// FAT, exFAT) or that are reached over the network: a Low token cannot write
/// there at all, and labelling a remote path would change the server's
/// directory (research 8.8). Checked before any label is written.
pub(crate) fn check_volume(handle: &File) -> Result<(), String> {
    let h = HANDLE(handle.as_raw_handle());
    let mut remote = FILE_REMOTE_PROTOCOL_INFO::default();
    let is_remote = unsafe {
        GetFileInformationByHandleEx(
            h,
            FileRemoteProtocolInfo,
            &mut remote as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_REMOTE_PROTOCOL_INFO>() as u32,
        )
    }
    .is_ok();
    if is_remote {
        return Err("it is on a network volume, which a sandboxed command cannot write".into());
    }
    let mut flags = 0u32;
    unsafe { GetVolumeInformationByHandleW(h, None, None, None, Some(&mut flags), None) }
        .map_err(|e| format!("its volume could not be inspected: {}", e.message().trim()))?;
    if flags & PERSISTENT_ACLS == 0 {
        return Err(
            "its volume has no file permissions (for example FAT or exFAT), so a sandboxed \
             command cannot write it"
                .into(),
        );
    }
    Ok(())
}

/// Label part of an object's security descriptor as SDDL (`S:(ML;...)`).
fn label_sddl(path: &Path) -> io::Result<String> {
    let w = wide_path(path);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let code = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut sd,
        )
    };
    if code.0 != 0 {
        return Err(io::Error::from_raw_os_error(code.0 as i32));
    }
    let mut s = PWSTR::null();
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            LABEL_SECURITY_INFORMATION,
            &mut s,
            None,
        )
    };
    let text = converted
        .map_err(|e| win(e, "ConvertSecurityDescriptorToStringSecurityDescriptorW"))
        .and_then(|()| unsafe { s.to_string() }.map_err(io::Error::other));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    text
}

/// Whether `path` itself carries the explicit inheritable Low label this
/// module writes.
pub(crate) fn has_root_label(path: &Path) -> bool {
    label_sddl(path).is_ok_and(|s| s.contains("(ML;OICI;NW;;;LW)"))
}

/// Whether `path` is writable by a Low token through its label (explicit or
/// inherited Low label).
#[cfg(test)]
pub(crate) fn is_low_labelled(path: &Path) -> bool {
    label_sddl(path).is_ok_and(|s| s.contains(";LW)") || s.contains(";S-1-16-4096)"))
}

/// A label ACL: the inheritable Low label, or no label at all.
fn label_acl(low: bool) -> io::Result<Vec<u64>> {
    let sid = Sid::parse("S-1-16-4096")?;
    let total =
        (std::mem::size_of::<ACL>() + 8 + unsafe { GetLengthSid(sid.psid()) } as usize + 3) & !3;
    let mut buf = vec![0u64; total.div_ceil(8)];
    let acl = buf.as_mut_ptr() as *mut ACL;
    unsafe {
        InitializeAcl(acl, total as u32, ACL_REVISION).map_err(|e| win(e, "InitializeAcl"))?;
        if low {
            AddMandatoryAce(
                acl,
                ACL_REVISION,
                ACE_FLAGS(OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0),
                NO_WRITE_UP,
                sid.psid(),
            )
            .map_err(|e| win(e, "AddMandatoryAce"))?;
        }
    }
    Ok(buf)
}

/// Objects a tree operation could not update.
#[derive(Debug, Default)]
pub(crate) struct TreeFailures {
    pub count: u64,
    pub sample: Vec<String>,
}

struct Tally {
    failures: TreeFailures,
}

unsafe extern "system" fn progress(
    name: PCWSTR,
    status: u32,
    invoke: *mut PROG_INVOKE_SETTING,
    args: *const c_void,
    _set: BOOL,
) {
    let tally = unsafe { &mut *(args as *mut Tally) };
    if status != 0 {
        tally.failures.count += 1;
        if tally.failures.sample.len() < 3 {
            let path = unsafe { name.to_string() }.unwrap_or_default();
            tally
                .failures
                .sample
                .push(format!("{path} (error {status})"));
        }
    }
    unsafe { *invoke = ProgressInvokeEveryObject };
}

fn tree_set(path: &Path, low: bool) -> io::Result<TreeFailures> {
    let acl = label_acl(low)?;
    let w = wide_path(path);
    let mut tally = Tally {
        failures: TreeFailures::default(),
    };
    let code = unsafe {
        TreeSetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(acl.as_ptr() as *const ACL),
            TREE_SEC_INFO_SET,
            Some(progress),
            ProgressInvokeEveryObject,
            Some(&mut tally as *mut Tally as *const c_void),
        )
    };
    if code.0 != 0 {
        return Err(io::Error::from_raw_os_error(code.0 as i32));
    }
    Ok(tally.failures)
}

/// Put the inheritable Low label on `path` and propagate it.
pub(crate) fn set_label(path: &Path) -> io::Result<TreeFailures> {
    tree_set(path, true)
}

/// Remove the label from `path`; descendants lose the inherited label.
pub(crate) fn clear_label(path: &Path) -> io::Result<TreeFailures> {
    tree_set(path, false)
}

/// The first files below `root` with more than one hard link: labelling the
/// root would also label their other names, which may lie outside it
/// (research 8.7.3). Symbolic links and junctions are not followed.
pub(crate) fn multi_link_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(entry.path());
                continue;
            }
            let links = std::fs::OpenOptions::new()
                .access_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(entry.path())
                .ok()
                .and_then(|file| {
                    let mut info = BY_HANDLE_FILE_INFORMATION::default();
                    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
                        .ok()
                        .map(|_| info.nNumberOfLinks)
                });
            if links.is_some_and(|n| n > 1) {
                found.push(entry.path());
                if found.len() >= limit {
                    return found;
                }
            }
        }
    }
    found
}
