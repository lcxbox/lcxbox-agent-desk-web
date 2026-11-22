//! Token, desktop and window-station access of a Windows sandboxed command
//! (plan 6.5.3, part one), after `pocs/windows-exec-sandbox` `--shape low-il`.
//!
//! - Token: `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE | LUA_TOKEN)` of this
//!   process's token, Administrators and the logon SIDs deny-only, no
//!   restricting SIDs (they break Schannel), integrity level Low, default DACL
//!   SYSTEM + user.
//! - Desktop: a private desktop per execution with a Low label.
//! - WinSta0: the user SID is granted when missing and the grant is never
//!   removed; WinSta0 is destroyed at logoff, and removing it would break other
//!   workers of the session.
use std::ffi::c_void;
use std::io;
use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SDDL_REVISION_1,
    SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    AddAccessAllowedAce, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    GetAce, GetAclInformation, GetLengthSid, GetSecurityDescriptorDacl, GetTokenInformation,
    GetUserObjectSecurity, InitializeAcl, InitializeSecurityDescriptor, LUA_TOKEN, NO_INHERITANCE,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SID_AND_ATTRIBUTES,
    SetSecurityDescriptorDacl, SetTokenInformation, SetUserObjectSecurity, TOKEN_ALL_ACCESS,
    TOKEN_DEFAULT_DACL, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TokenDefaultDacl, TokenGroups,
    TokenIntegrityLevel,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CONTROL_FLAGS, GetProcessWindowStation,
    GetUserObjectInformationW, HDESK, UOI_NAME,
};
use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// A kernel handle closed on drop.
#[derive(Debug)]
pub(crate) struct OwnedHandle(pub HANDLE);

impl OwnedHandle {
    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: kernel handles have no thread affinity.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win(error: windows::core::Error, what: &str) -> io::Error {
    io::Error::other(format!("{what}: {}", error.message().trim()))
}

/// A SID allocated by `ConvertStringSidToSidW`, freed with `LocalFree`.
pub(crate) struct Sid(PSID);

impl Sid {
    pub(crate) fn parse(sid: &str) -> io::Result<Self> {
        let w = wide(sid);
        let mut psid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(w.as_ptr()), &mut psid) }
            .map_err(|e| win(e, "ConvertStringSidToSidW"))?;
        Ok(Self(psid))
    }

    pub(crate) fn psid(&self) -> PSID {
        self.0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

pub(crate) fn sid_string(psid: PSID) -> io::Result<String> {
    let mut s = PWSTR::null();
    unsafe { ConvertSidToStringSidW(psid, &mut s) }
        .map_err(|e| win(e, "ConvertSidToStringSidW"))?;
    let out = unsafe { s.to_string() }.map_err(io::Error::other);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
    }
    out
}

const SE_GROUP_INTEGRITY: u32 = 0x20;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const LOW_INTEGRITY: &str = "S-1-16-4096";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";

fn token_groups(token: HANDLE) -> io::Result<(Vec<u64>, Vec<SID_AND_ATTRIBUTES>)> {
    let mut len = 0u32;
    unsafe {
        let _ = GetTokenInformation(token, TokenGroups, None, 0, &mut len);
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        )
    }
    .map_err(|e| win(e, "GetTokenInformation(TokenGroups)"))?;
    let groups = unsafe {
        let tg = buf.as_ptr() as *const TOKEN_GROUPS;
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*tg).Groups) as *const SID_AND_ATTRIBUTES,
            (*tg).GroupCount as usize,
        )
        .to_vec()
    };
    Ok((buf, groups))
}

/// The sandbox token derived from this process's token. `user` is the
/// current user's SID string.
pub(crate) fn sandbox_token(user: &str) -> io::Result<OwnedHandle> {
    let mut base = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut base) }
        .map_err(|e| win(e, "OpenProcessToken"))?;
    let base = OwnedHandle(base);
    let admins = Sid::parse(ADMINISTRATORS)?;
    // `_groups` keeps the buffer the logon SID pointers refer to alive.
    let (_groups, groups) = token_groups(base.raw())?;
    let mut disable: Vec<SID_AND_ATTRIBUTES> = vec![SID_AND_ATTRIBUTES {
        Sid: admins.psid(),
        Attributes: 0,
    }];
    disable.extend(
        groups
            .iter()
            .filter(|g| g.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            }),
    );
    let mut out = HANDLE::default();
    unsafe {
        CreateRestrictedToken(
            base.raw(),
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
            Some(&disable),
            None,
            None,
            &mut out,
        )
    }
    .map_err(|e| win(e, "CreateRestrictedToken"))?;
    let token = OwnedHandle(out);
    let low = Sid::parse(LOW_INTEGRITY)?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: low.psid(),
            Attributes: SE_GROUP_INTEGRITY,
        },
    };
    unsafe {
        SetTokenInformation(
            token.raw(),
            TokenIntegrityLevel,
            &label as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(low.psid()),
        )
    }
    .map_err(|e| win(e, "SetTokenInformation(TokenIntegrityLevel)"))?;
    let system = Sid::parse(SYSTEM)?;
    let user = Sid::parse(user)?;
    set_default_dacl(token.raw(), &[&system, &user])?;
    Ok(token)
}

fn set_default_dacl(token: HANDLE, sids: &[&Sid]) -> io::Result<()> {
    let ace_base = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>();
    let total = std::mem::size_of::<ACL>()
        + sids
            .iter()
            .map(|s| ace_base + unsafe { GetLengthSid(s.psid()) } as usize)
            .sum::<usize>();
    let total = (total + 3) & !3;
    let mut buf = vec![0u64; total.div_ceil(8)];
    let acl = buf.as_mut_ptr() as *mut ACL;
    unsafe {
        InitializeAcl(acl, total as u32, ACL_REVISION).map_err(|e| win(e, "InitializeAcl"))?;
        for s in sids {
            AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL.0, s.psid())
                .map_err(|e| win(e, "AddAccessAllowedAce"))?;
        }
        let info = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            &info as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
        .map_err(|e| win(e, "SetTokenInformation(TokenDefaultDacl)"))
    }
}

/// `DESKTOP_ALL_ACCESS | STANDARD_RIGHTS_REQUIRED`.
const DESKTOP_ALL: u32 = 0x000F_01FF;
/// `WINSTA_ALL_ACCESS | READ_CONTROL`.
const WINSTA_GRANT: u32 = 0x0002_037F;

fn object_name(h: HANDLE) -> io::Result<String> {
    let mut needed = 0u32;
    unsafe {
        let _ = GetUserObjectInformationW(h, UOI_NAME, None, 0, Some(&mut needed));
    }
    let mut buf = vec![0u16; (needed as usize).div_ceil(2).max(1)];
    unsafe {
        GetUserObjectInformationW(
            h,
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut c_void),
            needed,
            Some(&mut needed),
        )
    }
    .map_err(|e| win(e, "GetUserObjectInformationW"))?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..end]))
}

/// A private desktop for one execution, closed on drop.
pub(crate) struct PrivateDesktop {
    handle: HDESK,
    startup: Vec<u16>,
}

// SAFETY: a desktop handle is only closed once, by its owner.
unsafe impl Send for PrivateDesktop {}

impl PrivateDesktop {
    pub(crate) fn create(user: &str) -> io::Result<Self> {
        let ws =
            unsafe { GetProcessWindowStation() }.map_err(|e| win(e, "GetProcessWindowStation"))?;
        let winsta = object_name(HANDLE(ws.0))?;
        let name = format!("lcxbox-exec-sandbox-{}", uuid::Uuid::new_v4().simple());
        // A Low IL token must be able to create windows on its desktop:
        // unlabelled objects count as Medium.
        let sddl = wide(&format!(
            "D:P(A;;GA;;;SY)(A;;0x{DESKTOP_ALL:X};;;{user})S:(ML;;NW;;;LW)"
        ));
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
        }
        .map_err(|e| {
            win(
                e,
                "ConvertStringSecurityDescriptorToSecurityDescriptorW(desktop)",
            )
        })?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        let wname = wide(&name);
        let created = unsafe {
            CreateDesktopW(
                PCWSTR(wname.as_ptr()),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                DESKTOP_ALL,
                Some(&sa),
            )
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        let handle = created.map_err(|e| win(e, "CreateDesktopW"))?;
        Ok(Self {
            handle,
            startup: wide(&format!("{winsta}\\{name}")),
        })
    }

    /// `<winsta>\<desktop>` for `STARTUPINFOW.lpDesktop`; valid while `self`
    /// lives.
    pub(crate) fn startup_ptr(&mut self) -> PWSTR {
        PWSTR(self.startup.as_mut_ptr())
    }
}

impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseDesktop(self.handle);
        }
    }
}

static WINSTA_LOCK: Mutex<()> = Mutex::new(());

/// Grant `user` access to the process window station when it has none. The
/// grant is never removed.
pub(crate) fn ensure_winsta_grant(user: &str) -> io::Result<()> {
    let _guard = WINSTA_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let ws = unsafe { GetProcessWindowStation() }.map_err(|e| win(e, "GetProcessWindowStation"))?;
    let h = HANDLE(ws.0);
    let si = DACL_SECURITY_INFORMATION.0;
    let mut needed = 0u32;
    unsafe {
        let _ = GetUserObjectSecurity(h, &si, None, 0, &mut needed);
    }
    let mut sd = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    let psd = PSECURITY_DESCRIPTOR(sd.as_mut_ptr() as *mut c_void);
    unsafe { GetUserObjectSecurity(h, &si, Some(psd), needed, &mut needed) }
        .map_err(|e| win(e, "GetUserObjectSecurity"))?;
    let mut present = Default::default();
    let mut defaulted = Default::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|e| win(e, "GetSecurityDescriptorDacl"))?;
    if present.as_bool() && dacl.is_null() {
        // A NULL DACL already grants everyone.
        return Ok(());
    }
    if dacl_grants(dacl, user, WINSTA_GRANT)? {
        return Ok(());
    }
    let sid = Sid::parse(user)?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: WINSTA_GRANT,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.psid().0 as *mut u16),
        },
    };
    let mut new: *mut ACL = std::ptr::null_mut();
    let code = unsafe {
        SetEntriesInAclW(
            Some(&[entry]),
            if dacl.is_null() { None } else { Some(dacl) },
            &mut new,
        )
    };
    if code.0 != 0 {
        return Err(io::Error::from_raw_os_error(code.0 as i32));
    }
    let result = (|| {
        let mut sd_new = SECURITY_DESCRIPTOR::default();
        let psd_new = PSECURITY_DESCRIPTOR(&mut sd_new as *mut _ as *mut c_void);
        unsafe {
            InitializeSecurityDescriptor(psd_new, SECURITY_DESCRIPTOR_REVISION)
                .map_err(|e| win(e, "InitializeSecurityDescriptor"))?;
            SetSecurityDescriptorDacl(psd_new, true, Some(new), false)
                .map_err(|e| win(e, "SetSecurityDescriptorDacl"))?;
            SetUserObjectSecurity(h, &DACL_SECURITY_INFORMATION, psd_new)
                .map_err(|e| win(e, "SetUserObjectSecurity(WinSta0)"))
        }
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(new as *mut c_void)));
    }
    result
}

/// Whether `dacl` has an allow ACE for `user` covering `mask`.
fn dacl_grants(dacl: *const ACL, user: &str, mask: u32) -> io::Result<bool> {
    if dacl.is_null() {
        return Ok(false);
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(|e| win(e, "GetAclInformation"))?;
    for i in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(dacl, i, &mut ace) }.map_err(|e| win(e, "GetAce"))?;
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if header.AceType != 0 {
            continue;
        }
        let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let psid = PSID(std::ptr::addr_of!(body.SidStart) as *mut c_void);
        if body.Mask & mask == mask && sid_string(psid)?.eq_ignore_ascii_case(user) {
            return Ok(true);
        }
    }
    Ok(false)
}
