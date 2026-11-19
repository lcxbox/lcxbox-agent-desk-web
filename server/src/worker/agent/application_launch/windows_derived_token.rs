//! An ordinary-user token derived from the caller's own elevated token, for
//! an elevated host without the ServiceDaemon (portable app or DeskServer run
//! as administrator) to start its application host.
//!
//! A process without SeTcb only gets an identification-level linked token,
//! which cannot start a process. A restricted version of the caller's own
//! primary token can be assigned without SeAssignPrimaryToken, so the host
//! reduces its own token: `CreateRestrictedToken(LUA_TOKEN)` makes
//! Administrators deny-only and filters the privileges, Medium integrity is
//! set, and the default owner and DACL are rewritten from Administrators to
//! the user (otherwise the process cannot open its own objects and fails with
//! STATUS_DLL_INIT_FAILED, and could not create files).
//!
//! Known differences from the ordinary token Windows creates at logon: the
//! token stays in the elevated logon session, so drives mapped in the ordinary
//! session are not visible; `TokenElevationType` still reads Full while
//! `TokenIsElevated` is 0; UAC file and registry virtualization is not allowed.
//! A process with this token cannot define drive letters in the shared logon
//! session (verified on Windows 11 26H2).
#![allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
use super::windows_diagnostics::{explained, failure};
use super::windows_process::OwnedHandle;
use desk_agent_protocol::application_launch::{LaunchError, LaunchFailureReason};
use desk_agent_protocol::native_diagnostic::DiagnosticStage;
use std::ffi::c_void;
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LUID, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                ConvertStringSidToSidW, SDDL_REVISION_1,
            },
            CreateRestrictedToken, GetSecurityDescriptorDacl, GetSidSubAuthority,
            GetSidSubAuthorityCount, GetTokenInformation, IsTokenRestricted, LUA_TOKEN,
            LookupPrivilegeNameW, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
            SetTokenInformation, TOKEN_DEFAULT_DACL, TOKEN_ELEVATION, TOKEN_GROUPS,
            TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_PRIVILEGES,
            TokenDefaultDacl, TokenElevation, TokenGroups, TokenIntegrityLevel, TokenLogonSid,
            TokenOwner, TokenPrivileges, TokenSessionId, TokenUser,
        },
    },
    core::{HSTRING, PWSTR},
};

/// Access the current token is opened with for `derive`. The derived token
/// handle carries the same access: TOKEN_ADJUST_DEFAULT to set its integrity,
/// owner and default DACL, TOKEN_ASSIGN_PRIMARY to start the host with it.
pub(crate) const CURRENT_TOKEN_ACCESS: windows::Win32::Security::TOKEN_ACCESS_MASK =
    windows::Win32::Security::TOKEN_ACCESS_MASK(
        windows::Win32::Security::TOKEN_QUERY.0
            | windows::Win32::Security::TOKEN_DUPLICATE.0
            | windows::Win32::Security::TOKEN_ASSIGN_PRIMARY.0
            | windows::Win32::Security::TOKEN_ADJUST_DEFAULT.0,
    );

const MEDIUM_INTEGRITY_SID: &str = "S-1-16-8192";
const MEDIUM_INTEGRITY_RID: u32 = 0x2000;
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;
const SE_GROUP_INTEGRITY: u32 = 0x20;
/// The privileges an ordinary UAC token of an administrator keeps.
const ORDINARY_PRIVILEGES: [&str; 5] = [
    "SeChangeNotifyPrivilege",
    "SeShutdownPrivilege",
    "SeUndockPrivilege",
    "SeIncreaseWorkingSetPrivilege",
    "SeTimeZonePrivilege",
];

/// The default DACL of the ordinary token Windows creates at logon: the user
/// and SYSTEM with full access and the logon SID with read and execute. The
/// elevated token has Administrators in the user's place.
pub(crate) fn default_dacl_sddl(user_sid: &str, logon_sid: Option<&str>) -> String {
    let mut sddl = format!("D:(A;;GA;;;{user_sid})(A;;GA;;;SY)");
    if let Some(logon) = logon_sid {
        sddl.push_str(&format!("(A;;GXGR;;;{logon})"));
    }
    sddl
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupState {
    Absent,
    Enabled,
    DenyOnly,
}

/// What the derived token must look like before anything is started with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenFacts {
    pub(crate) user_sid: String,
    pub(crate) session_id: u32,
    pub(crate) elevated: bool,
    pub(crate) integrity_rid: u32,
    pub(crate) administrators: GroupState,
    pub(crate) restricted: bool,
    pub(crate) privileges: Vec<String>,
}

/// Fail closed: any difference from an ordinary-user token of the same user
/// and session refuses the derived token.
pub(crate) fn validate(
    facts: &TokenFacts,
    expected_user_sid: &str,
    expected_session_id: u32,
) -> Result<(), &'static str> {
    if facts.user_sid != expected_user_sid || facts.session_id != expected_session_id {
        return Err("derived token changed user or session");
    }
    if facts.elevated {
        return Err("derived token is still elevated");
    }
    if facts.integrity_rid != MEDIUM_INTEGRITY_RID {
        return Err("derived token is not at Medium integrity");
    }
    if facts.administrators != GroupState::DenyOnly {
        return Err("Administrators is not deny-only in the derived token");
    }
    if facts.restricted {
        return Err("derived token carries restricting SIDs");
    }
    if facts
        .privileges
        .iter()
        .any(|name| !ORDINARY_PRIVILEGES.contains(&name.as_str()))
    {
        return Err("derived token keeps a privilege an ordinary user does not have");
    }
    Ok(())
}

/// Variable-length token information in usize-aligned storage.
fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>, LaunchError> {
    let mut size = 0;
    let sizing = unsafe { GetTokenInformation(token, class, None, 0, &mut size) };
    if size == 0 || size > 65536 {
        return Err(sizing
            .err()
            .map(|error| {
                failure(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::Environment,
                    "GetTokenInformation(size)",
                    &error,
                )
            })
            .unwrap_or_else(|| LaunchFailureReason::SessionUnavailable.into()));
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "GetTokenInformation",
            &error,
        )
    })?;
    Ok(buffer)
}

fn fixed<T: Default>(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<T, LaunchError> {
    let mut value = T::default();
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some((&mut value as *mut T).cast()),
            std::mem::size_of::<T>() as u32,
            &mut size,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "GetTokenInformation",
            &error,
        )
    })?;
    Ok(value)
}

fn sid_text(sid: PSID) -> Result<String, LaunchError> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "ConvertSidToStringSidW",
            &error,
        )
    })?;
    let value = unsafe { text.to_string() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    value.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "decode SID",
            &error,
        )
    })
}

/// A SID or security descriptor allocated by the system; freed on drop.
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0)));
        }
    }
}

fn string_sid(text: &str) -> Result<(PSID, LocalAllocation), LaunchError> {
    let mut sid = PSID::default();
    unsafe { ConvertStringSidToSidW(&HSTRING::from(text), &mut sid) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "ConvertStringSidToSidW",
            &error,
        )
    })?;
    Ok((sid, LocalAllocation(sid.0)))
}

fn groups(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<(String, u32)>, LaunchError> {
    let buffer = token_buffer(token, class)?;
    let groups = unsafe { &*buffer.as_ptr().cast::<TOKEN_GROUPS>() };
    let entries =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    entries
        .iter()
        .map(|entry| Ok((sid_text(entry.Sid)?, entry.Attributes)))
        .collect()
}

fn privileges(token: HANDLE) -> Result<Vec<String>, LaunchError> {
    let buffer = token_buffer(token, TokenPrivileges)?;
    let privileges = unsafe { &*buffer.as_ptr().cast::<TOKEN_PRIVILEGES>() };
    let entries = unsafe {
        std::slice::from_raw_parts(
            privileges.Privileges.as_ptr(),
            privileges.PrivilegeCount as usize,
        )
    };
    entries
        .iter()
        .map(|entry| privilege_name(&entry.Luid))
        .collect()
}

fn privilege_name(luid: &LUID) -> Result<String, LaunchError> {
    let mut name = [0u16; 128];
    let mut length = name.len() as u32;
    unsafe { LookupPrivilegeNameW(None, luid, Some(PWSTR(name.as_mut_ptr())), &mut length) }
        .map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "LookupPrivilegeNameW",
                &error,
            )
        })?;
    Ok(String::from_utf16_lossy(&name[..length as usize]))
}

fn token_user(token: HANDLE) -> Result<String, LaunchError> {
    let buffer = token_buffer(token, TokenUser)?;
    let user = unsafe {
        &*buffer
            .as_ptr()
            .cast::<windows::Win32::Security::TOKEN_USER>()
    };
    sid_text(user.User.Sid)
}

/// The facts `validate` checks, read from a live token.
pub(crate) fn facts(token: HANDLE) -> Result<TokenFacts, LaunchError> {
    let integrity = token_buffer(token, TokenIntegrityLevel)?;
    let label = unsafe { &*integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let integrity_rid = unsafe {
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        if count == 0 {
            0
        } else {
            *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1)
        }
    };
    let administrators = groups(token, TokenGroups)?
        .into_iter()
        .find(|(sid, _)| sid == ADMINISTRATORS_SID)
        .map_or(GroupState::Absent, |(_, attributes)| {
            if attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0 {
                GroupState::DenyOnly
            } else {
                GroupState::Enabled
            }
        });
    Ok(TokenFacts {
        user_sid: token_user(token)?,
        session_id: fixed::<u32>(token, TokenSessionId)?,
        elevated: fixed::<TOKEN_ELEVATION>(token, TokenElevation)?.TokenIsElevated != 0,
        integrity_rid,
        administrators,
        restricted: unsafe { IsTokenRestricted(token) }.is_ok(),
        privileges: privileges(token)?,
    })
}

fn set<T>(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
    value: &T,
    operation: &str,
) -> Result<(), LaunchError> {
    unsafe {
        SetTokenInformation(
            token,
            class,
            (value as *const T).cast(),
            std::mem::size_of::<T>() as u32,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            operation,
            &error,
        )
    })
}

/// Derive and validate an ordinary-user primary token from `current`, which
/// must be opened with [`CURRENT_TOKEN_ACCESS`].
pub(crate) fn derive(
    current: HANDLE,
    expected_user_sid: &str,
    expected_session_id: u32,
) -> Result<OwnedHandle, LaunchError> {
    let mut raw = HANDLE::default();
    unsafe { CreateRestrictedToken(current, LUA_TOKEN, None, None, None, &mut raw) }.map_err(
        |error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "CreateRestrictedToken(LUA_TOKEN)",
                &error,
            )
        },
    )?;
    let derived = OwnedHandle(raw);

    let (medium, _medium) = string_sid(MEDIUM_INTEGRITY_SID)?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: medium,
            Attributes: SE_GROUP_INTEGRITY,
        },
    };
    set(
        derived.0,
        TokenIntegrityLevel,
        &label,
        "SetTokenInformation(TokenIntegrityLevel)",
    )?;

    let user = token_user(current)?;
    let (user_sid, _user_sid) = string_sid(&user)?;
    let owner = TOKEN_OWNER { Owner: user_sid };
    set(
        derived.0,
        TokenOwner,
        &owner,
        "SetTokenInformation(TokenOwner)",
    )?;

    let logon = groups(current, TokenLogonSid)?
        .into_iter()
        .next()
        .map(|(sid, _)| sid);
    let sddl = default_dacl_sddl(&user, logon.as_deref());
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl.as_str()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            &error,
        )
    })?;
    let _descriptor = LocalAllocation(descriptor.0);
    let mut present = windows::core::BOOL::default();
    let mut defaulted = windows::core::BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "GetSecurityDescriptorDacl",
                &error,
            )
        })?;
    if !present.as_bool() || dacl.is_null() {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    let default_dacl = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    set(
        derived.0,
        TokenDefaultDacl,
        &default_dacl,
        "SetTokenInformation(TokenDefaultDacl)",
    )?;

    let observed = facts(derived.0)?;
    validate(&observed, expected_user_sid, expected_session_id).map_err(|reason| {
        explained(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "derive ordinary-user token",
            reason,
        )
    })?;
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";

    fn ordinary() -> TokenFacts {
        TokenFacts {
            user_sid: USER.into(),
            session_id: 2,
            elevated: false,
            integrity_rid: MEDIUM_INTEGRITY_RID,
            administrators: GroupState::DenyOnly,
            restricted: false,
            privileges: vec![
                "SeChangeNotifyPrivilege".into(),
                "SeShutdownPrivilege".into(),
                "SeTimeZonePrivilege".into(),
            ],
        }
    }

    #[test]
    fn the_default_dacl_matches_the_ordinary_logon_token() {
        assert_eq!(
            default_dacl_sddl(USER, Some("S-1-5-5-0-105197322")),
            format!("D:(A;;GA;;;{USER})(A;;GA;;;SY)(A;;GXGR;;;S-1-5-5-0-105197322)")
        );
        assert_eq!(
            default_dacl_sddl(USER, None),
            format!("D:(A;;GA;;;{USER})(A;;GA;;;SY)")
        );
    }

    #[test]
    fn only_an_ordinary_token_of_the_same_user_and_session_passes() {
        assert_eq!(validate(&ordinary(), USER, 2), Ok(()));
        let changes: [fn(&mut TokenFacts); 9] = [
            |f| f.user_sid = "S-1-5-21-1-2-3-1002".into(),
            |f| f.session_id = 3,
            |f| f.elevated = true,
            |f| f.integrity_rid = 0x3000,
            |f| f.integrity_rid = 0x1000,
            |f| f.administrators = GroupState::Enabled,
            |f| f.administrators = GroupState::Absent,
            |f| f.restricted = true,
            |f| f.privileges.push("SeDebugPrivilege".into()),
        ];
        for change in changes {
            let mut facts = ordinary();
            change(&mut facts);
            assert!(validate(&facts, USER, 2).is_err(), "{facts:?}");
        }
    }

    /// Run in an elevated shell of a UAC administrator:
    /// `cargo test -p lcxl-remote-desk-server --lib -- --ignored derived_token_starts_an_ordinary_process`
    #[test]
    #[ignore = "requires an elevated process of a UAC administrator (elevation type Full)"]
    fn derived_token_starts_an_ordinary_process() {
        use windows::Win32::Security::{
            TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION_TYPE,
            TOKEN_QUERY, TokenElevationType, TokenElevationTypeFull,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut raw = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), CURRENT_TOKEN_ACCESS, &mut raw) }.unwrap();
        let current = OwnedHandle(raw);
        assert_eq!(
            fixed::<TOKEN_ELEVATION_TYPE>(current.0, TokenElevationType).unwrap(),
            TokenElevationTypeFull,
            "run from an elevated shell of a UAC administrator"
        );
        let user = token_user(current.0).unwrap();
        assert_ne!(user, "S-1-5-18");
        let session = fixed::<u32>(current.0, TokenSessionId).unwrap();
        let derived = derive(current.0, &user, session).unwrap();
        let observed = facts(derived.0).unwrap();
        assert_eq!(validate(&observed, &user, session), Ok(()));
        assert_eq!(observed.administrators, GroupState::DenyOnly);

        // The derived process runs and can create files (with Administrators
        // still the default owner it could not), and it cannot define a drive
        // letter in the logon session it shares with this process.
        let dir = tempfile::tempdir().unwrap();
        let letter = (b'M'..=b'Z')
            .map(|c| format!("{}:", c as char))
            .find(|drive| !std::path::Path::new(&format!("{drive}\\")).exists())
            .unwrap();
        let command = format!(
            "cmd.exe /c echo x> created.txt & subst {letter} \"{}\" & exit /b 0",
            dir.path().display()
        );
        assert_eq!(run_with_token(&derived, &command, dir.path()), 0);
        assert!(dir.path().join("created.txt").exists());
        assert!(!std::path::Path::new(&format!("{letter}\\")).exists());
    }

    fn run_with_token(token: &OwnedHandle, command: &str, cwd: &std::path::Path) -> u32 {
        use windows::Win32::System::Threading::{
            CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, INFINITE,
            PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
        };
        use windows::core::PCWSTR;
        let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let application = wide("C:\\Windows\\System32\\cmd.exe");
        let mut command = wide(command);
        let cwd = wide(&cwd.to_string_lossy());
        let mut desktop = wide("WinSta0\\Default");
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR(cwd.as_ptr()),
                &startup,
                &mut process,
            )
        }
        .unwrap();
        let _thread = OwnedHandle(process.hThread);
        let process = OwnedHandle(process.hProcess);
        let mut code = 1;
        unsafe {
            WaitForSingleObject(process.0, INFINITE);
            GetExitCodeProcess(process.0, &mut code).unwrap();
        }
        code
    }
}
