use super::denied;
use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT},
            *,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
        },
    },
    core::PWSTR,
};

struct Local(HLOCAL);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(self.0));
        }
    }
}
fn sid_text(sid: PSID) -> io::Result<String> {
    let mut value = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut value) }.map_err(io::Error::other)?;
    let _memory = Local(HLOCAL(value.0.cast()));
    unsafe { value.to_string() }.map_err(io::Error::other)
}
fn descriptor(file: &File) -> io::Result<(Local, String, *mut ACL)> {
    let mut desc = PSECURITY_DESCRIPTOR::default();
    let mut sid = PSID::default();
    let mut acl = std::ptr::null_mut();
    unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut sid),
            None,
            Some(&mut acl),
            None,
            Some(&mut desc),
        )
    }
    .ok()
    .map_err(io::Error::other)?;
    let mem = Local(HLOCAL(desc.0));
    Ok((mem, sid_text(sid)?, acl))
}
pub(crate) fn owner(path: &Path) -> io::Result<String> {
    Ok(descriptor(&File::open(path)?)?.1)
}

pub(crate) fn check(path: &Path) -> io::Result<()> {
    let current = desk_file_recovery::windows::current_user_sid()?;
    const TRUSTED_INSTALLER: &str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    let trusted = |sid: &str| sid == current || matches!(sid, "S-1-5-18" | "S-1-5-32-544");
    for (depth, item) in path.ancestors().enumerate() {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(item)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(io::Error::other)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            || (depth == 0 && info.nNumberOfLinks != 1)
        {
            return Err(denied());
        }
        let (_mem, owner, acl) = descriptor(&file)?;
        if (!trusted(&owner) && !(depth > 1 && owner == TRUSTED_INSTALLER)) || acl.is_null() {
            return Err(denied());
        }
        for i in 0..u32::from(unsafe { (*acl).AceCount }) {
            let mut p = std::ptr::null_mut();
            unsafe { GetAce(acl, i, &mut p) }.map_err(io::Error::other)?;
            let header = unsafe { &*p.cast::<ACE_HEADER>() };
            if header.AceType == 1 {
                continue;
            }
            if header.AceType != 0 {
                return Err(denied());
            }
            let ace = unsafe { &*p.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = sid_text(PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast()))?;
            // Ancestors may be readable or allow creating siblings. Delete-child,
            // DELETE, WRITE_DAC, WRITE_OWNER and generic write/all allow replacement.
            const REPLACEMENT_RIGHTS: u32 = 0x000d_0040 | 0x5000_0000;
            // CREATOR OWNER only grants the creator access to its own child.
            if header.AceFlags & 0x08 != 0 && sid == "S-1-3-0" {
                continue;
            }
            if !(trusted(&sid) || (depth > 1 && sid == TRUSTED_INSTALLER))
                && (depth < 2
                    || (header.AceFlags & 0x08 == 0 && ace.Mask & REPLACEMENT_RIGHTS != 0))
            {
                return Err(denied());
            }
        }
    }
    Ok(())
}
