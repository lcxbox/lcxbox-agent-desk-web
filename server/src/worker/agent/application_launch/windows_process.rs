//! Explicit executable creation with a verified ordinary/elevated user token.
//! No ShellExecute, runas, shell interpolation or exec containment is involved.
use super::windows_diagnostics::{explained, failure};
use desk_agent_protocol::application_launch::LaunchError;
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome,
};
use desk_agent_protocol::native_diagnostic::DiagnosticStage;
use desk_diagnose_core::application_launch::ResolvedApplicationIdentity;
use sha2::{Digest, Sha256};
use std::{ffi::c_void, fs::File, io::Read, os::windows::fs::OpenOptionsExt, path::Path};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_ASSIGN_PRIMARY,
            TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_LINKED_TOKEN,
            TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenLinkedToken, TokenSessionId, TokenUser,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            StationsAndDesktops::{GetThreadDesktop, GetUserObjectInformationW, UOI_NAME},
            Threading::{
                CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
                GetCurrentProcess, GetCurrentThreadId, OpenProcessToken, PROCESS_INFORMATION,
                STARTUPINFOW,
            },
        },
        UI::Shell::GetUserProfileDirectoryW,
    },
    core::{PCWSTR, PWSTR},
};

pub(crate) struct OwnedHandle(pub(crate) HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct UserEnvironment(*mut c_void);
impl Drop for UserEnvironment {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyEnvironmentBlock(self.0);
        }
    }
}

pub(crate) struct PreparedExecutable {
    request: LaunchApplicationRequest,
    command_line: Vec<u16>,
    pub(crate) identity: ResolvedApplicationIdentity,
    token: OwnedHandle,
    uses_current_token: bool,
    elevated: bool,
    /// The token was derived from this process's own elevated token.
    derived_token: bool,
    environment: UserEnvironment,
    // Deny concurrent replacement/write until after CreateProcess returns.
    _image: File,
}

fn token_value<T: Default>(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<T, LaunchError> {
    let mut value = T::default();
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(&mut value as *mut _ as *mut c_void),
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

fn token_sid(token: HANDLE) -> Result<String, LaunchError> {
    let mut size = 0;
    let sizing = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    if size == 0 || size > 65536 {
        return Err(sizing
            .err()
            .map(|error| {
                failure(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::Environment,
                    "GetTokenInformation(TokenUser size)",
                    &error,
                )
            })
            .unwrap_or_else(|| LaunchFailureReason::SessionUnavailable.into()));
    }
    // usize storage supplies the alignment required by TOKEN_USER and its SID.
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "GetTokenInformation(TokenUser)",
                &error,
            )
        })?;
        let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid).map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "ConvertSidToStringSidW",
                &error,
            )
        })?;
        let text = sid.to_string();
        let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
        text.map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "decode token SID",
                &error,
            )
        })
    }
}

pub(crate) fn current_elevated() -> Result<bool, LaunchError> {
    catalog_session_identity()?;
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let token = OwnedHandle(raw);
    Ok(token_value::<TOKEN_ELEVATION>(token.0, TokenElevation)?.TokenIsElevated != 0)
}

pub(crate) fn catalog_session_identity() -> Result<String, LaunchError> {
    require_default_desktop()?;
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let token = OwnedHandle(raw);
    let sid = token_sid(token.0)?;
    let session = token_value::<u32>(token.0, TokenSessionId)?;
    if session == 0 || matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    Ok(format!("{sid}:{session}"))
}

/// Storage identity does not depend on the currently visible input desktop.
pub(crate) fn storage_user_sid() -> Result<String, LaunchError> {
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let token = OwnedHandle(raw);
    let sid = token_sid(token.0)?;
    if matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    Ok(sid)
}

fn require_default_desktop() -> Result<(), LaunchError> {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, OpenInputDesktop,
    };
    fn is_default(desktop: HANDLE) -> Result<(), LaunchError> {
        let mut name = [0u16; 256];
        unsafe {
            GetUserObjectInformationW(
                desktop,
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                (name.len() * 2) as u32,
                None,
            )
        }
        .map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "GetUserObjectInformationW",
                &error,
            )
        })?;
        let end = name.iter().position(|v| *v == 0).unwrap_or(name.len());
        if !String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("default") {
            return Err(LaunchFailureReason::SessionUnavailable.into());
        }
        Ok(())
    }
    unsafe {
        let desktop = GetThreadDesktop(GetCurrentThreadId()).map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "GetThreadDesktop",
                &error,
            )
        })?;
        is_default(HANDLE(desktop.0))?;
        // A worker may remain attached to Default while Winlogon/UAC is active.
        // Check the actual input desktop too, without switching or opening UI.
        let input = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
            .map_err(|error| {
                failure(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::Environment,
                    "OpenInputDesktop",
                    &error,
                )
            })?;
        let result = is_default(HANDLE(input.0));
        let _ = CloseDesktop(input);
        result
    }
}

pub(crate) fn prepare(
    request: &LaunchApplicationRequest,
    expected_user_sid: &str,
    expected_session_id: u32,
) -> Result<PreparedExecutable, LaunchError> {
    request
        .validate()
        .map_err(|_| LaunchError::from(LaunchFailureReason::InvalidTarget))?;
    if request.target.kind != ApplicationTargetKind::Executable {
        return Err(LaunchFailureReason::Unsupported.into());
    }
    require_default_desktop()?;
    if expected_user_sid.is_empty() || expected_session_id == 0 {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    let mut raw = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            &mut raw,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let current = OwnedHandle(raw);
    let current_sid = token_sid(current.0)?;
    // Service and builtin service identities are never application identities.
    if current_sid != expected_user_sid
        || matches!(current_sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
        || token_value::<u32>(current.0, TokenSessionId)? != expected_session_id
    {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    let current_elevated =
        token_value::<TOKEN_ELEVATION>(current.0, TokenElevation)?.TokenIsElevated != 0;
    if current_elevated && !request.run_as_admin {
        // An elevated process never starts an ordinary launch itself: its
        // linked token is only identification-level without SeTcb, and the
        // current token would elevate an ordinarily approved application.
        // Ordinary launches go through the application host.
        return Err(elevated_ordinary_refusal(current.0)?);
    }
    if request.run_as_admin && !current_elevated {
        use windows::Win32::Security::{
            TOKEN_ELEVATION_TYPE, TokenElevationType, TokenElevationTypeDefault,
        };
        if token_value::<TOKEN_ELEVATION_TYPE>(current.0, TokenElevationType)?
            == TokenElevationTypeDefault
        {
            return Err(LaunchFailureReason::AdminRequired.into());
        }
    }
    let uses_current_token = current_elevated == request.run_as_admin;
    let token = if uses_current_token {
        current
    } else {
        let linked = token_value::<TOKEN_LINKED_TOKEN>(current.0, TokenLinkedToken).map_err(
            |mut error| {
                error.reason = if request.run_as_admin {
                    LaunchFailureReason::AdminLaunchUnavailable
                } else {
                    LaunchFailureReason::SessionUnavailable
                };
                error
            },
        )?;
        let linked = OwnedHandle(linked.LinkedToken);
        if token_sid(linked.0)? != current_sid
            || token_value::<u32>(linked.0, TokenSessionId)? != expected_session_id
            || (token_value::<TOKEN_ELEVATION>(linked.0, TokenElevation)?.TokenIsElevated != 0)
                != request.run_as_admin
        {
            return Err(LaunchFailureReason::SessionUnavailable.into());
        }
        linked
    };
    finish(
        request,
        token,
        uses_current_token,
        false,
        current_sid,
        expected_session_id,
    )
}

/// Whether the current process token is elevated, and its elevation type.
#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
pub(crate) fn current_elevation() -> Result<(bool, ElevationKind), LaunchError> {
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let token = OwnedHandle(raw);
    Ok((
        token_value::<TOKEN_ELEVATION>(token.0, TokenElevation)?.TokenIsElevated != 0,
        elevation_kind(token.0)?,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElevationKind {
    /// No split token: a standard user, UAC disabled, or a built-in
    /// Administrator whose token is not filtered.
    Default,
    /// The elevated half of a UAC split token.
    Full,
    /// The ordinary half of a UAC split token.
    Limited,
}

#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
fn elevation_kind(token: HANDLE) -> Result<ElevationKind, LaunchError> {
    use windows::Win32::Security::{
        TOKEN_ELEVATION_TYPE, TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited,
    };
    let kind = token_value::<TOKEN_ELEVATION_TYPE>(token, TokenElevationType)?;
    Ok(if kind == TokenElevationTypeFull {
        ElevationKind::Full
    } else if kind == TokenElevationTypeLimited {
        ElevationKind::Limited
    } else {
        ElevationKind::Default
    })
}

pub(crate) const NO_ORDINARY_TOKEN: &str =
    "this account has no unelevated token; request an administrator launch";

#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
fn elevated_ordinary_refusal(token: HANDLE) -> Result<LaunchError, LaunchError> {
    Ok(if elevation_kind(token)? == ElevationKind::Default {
        explained(
            LaunchFailureReason::Unsupported,
            DiagnosticStage::Environment,
            "ordinary application launch",
            NO_ORDINARY_TOKEN,
        )
    } else {
        explained(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "ordinary application launch",
            "an elevated process starts ordinary applications only through the application host",
        )
    })
}

/// The application host started with an ordinary-user token derived from this
/// elevated process's own token. It can only start the hidden host, never an
/// application, so the derived token is never used outside the host.
pub(crate) struct PreparedHost(PreparedExecutable);

impl PreparedHost {
    pub(crate) fn invoke_hidden_host(self, dispatch_id: String) -> LaunchApplicationResult {
        self.0.invoke_hidden_host(dispatch_id)
    }
}

/// Prepare the application host with a token derived from this process's own
/// elevated token, for an elevated host without the ServiceDaemon. See
/// `windows_derived_token` for the token and its known limitations.
#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
pub(crate) fn prepare_host_with_derived_token(
    request: &LaunchApplicationRequest,
    expected_user_sid: &str,
    expected_session_id: u32,
) -> Result<PreparedHost, LaunchError> {
    request
        .validate()
        .map_err(|_| LaunchError::from(LaunchFailureReason::InvalidTarget))?;
    if request.target.kind != ApplicationTargetKind::Executable || request.run_as_admin {
        return Err(LaunchFailureReason::Unsupported.into());
    }
    require_default_desktop()?;
    let mut raw = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            super::windows_derived_token::CURRENT_TOKEN_ACCESS,
            &mut raw,
        )
    }
    .map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let current = OwnedHandle(raw);
    let current_sid = token_sid(current.0)?;
    if current_sid != expected_user_sid
        || matches!(current_sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
        || token_value::<u32>(current.0, TokenSessionId)? != expected_session_id
        || token_value::<TOKEN_ELEVATION>(current.0, TokenElevation)?.TokenIsElevated == 0
        || elevation_kind(current.0)? != ElevationKind::Full
    {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    let derived =
        super::windows_derived_token::derive(current.0, expected_user_sid, expected_session_id)?;
    log::info!(
        "Starting the application host with a token derived from this elevated process; \
         drives mapped in the ordinary logon session are not visible to it and UAC \
         virtualization is not allowed"
    );
    finish(
        request,
        derived,
        false,
        true,
        current_sid,
        expected_session_id,
    )
    .map(PreparedHost)
}

/// The common tail of a prepared executable: environment, working directory,
/// canonical target and command line, and the pinned, hashed image.
#[allow(
    clippy::result_large_err,
    reason = "LaunchError carries the native diagnostic, as throughout application launch"
)]
fn finish(
    request: &LaunchApplicationRequest,
    token: OwnedHandle,
    uses_current_token: bool,
    derived_token: bool,
    current_sid: String,
    expected_session_id: u32,
) -> Result<PreparedExecutable, LaunchError> {
    let mut environment = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut environment, Some(token.0), false) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "CreateEnvironmentBlock",
            &error,
        )
    })?;
    let environment = UserEnvironment(environment);
    let cwd = if let Some(cwd) = &request.cwd {
        cwd.clone()
    } else {
        let mut size = 0;
        let sizing = unsafe { GetUserProfileDirectoryW(token.0, None, &mut size) };
        if size == 0 || size > 32768 {
            return Err(sizing
                .err()
                .map(|error| {
                    failure(
                        LaunchFailureReason::SessionUnavailable,
                        DiagnosticStage::Environment,
                        "GetUserProfileDirectoryW(size)",
                        &error,
                    )
                })
                .unwrap_or_else(|| LaunchFailureReason::SessionUnavailable.into()));
        }
        let mut profile = vec![0u16; size as usize];
        unsafe { GetUserProfileDirectoryW(token.0, Some(PWSTR(profile.as_mut_ptr())), &mut size) }
            .map_err(|error| {
                failure(
                    LaunchFailureReason::SessionUnavailable,
                    DiagnosticStage::Environment,
                    "GetUserProfileDirectoryW",
                    &error,
                )
            })?;
        let end = profile
            .iter()
            .position(|v| *v == 0)
            .unwrap_or(profile.len());
        String::from_utf16(&profile[..end]).map_err(|error| {
            failure(
                LaunchFailureReason::SessionUnavailable,
                DiagnosticStage::Environment,
                "decode user profile directory",
                &error,
            )
        })?
    };
    if !Path::new(&cwd).is_absolute()
        || !Path::new(&cwd).is_dir()
        || !Path::new(&request.target.value).is_absolute()
    {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let target = std::fs::canonicalize(&request.target.value).map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "canonicalize executable",
            &error,
        )
    })?;
    let cwd = std::fs::canonicalize(cwd).map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "canonicalize working directory",
            &error,
        )
    })?;
    let command_line = checked_command_line(&target.to_string_lossy(), &request.args)?;
    let mut image = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&target)
        .map_err(|error| {
            failure(
                LaunchFailureReason::InvalidTarget,
                DiagnosticStage::TargetResolution,
                "open executable",
                &error,
            )
        })?;
    if !image
        .metadata()
        .map_err(|error| {
            failure(
                LaunchFailureReason::InvalidTarget,
                DiagnosticStage::TargetResolution,
                "executable metadata",
                &error,
            )
        })?
        .is_file()
    {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = image.read(&mut buffer).map_err(|error| {
            failure(
                LaunchFailureReason::InvalidTarget,
                DiagnosticStage::TargetResolution,
                "read executable",
                &error,
            )
        })?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(PreparedExecutable {
        request: request.clone(),
        command_line,
        identity: ResolvedApplicationIdentity {
            canonical_target: target.to_string_lossy().into_owned(),
            identity_digest: format!("{:x}", hash.finalize()),
            resolved_cwd: Some(cwd.to_string_lossy().into_owned()),
            user_identity: current_sid,
            session_identity: expected_session_id.to_string(),
        },
        token,
        uses_current_token,
        elevated: request.run_as_admin,
        derived_token,
        environment,
        _image: image,
    })
}

/// Quote one argv item using the Windows C runtime backslash/quote convention.
pub(super) fn quote_argument(value: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn checked_command_line(target: &str, args: &[String]) -> Result<Vec<u16>, LaunchError> {
    let command = wide(
        &std::iter::once(target)
            .chain(args.iter().map(String::as_str))
            .map(quote_argument)
            .collect::<Vec<_>>()
            .join(" "),
    );
    // CreateProcess counts UTF-16 code units, including the terminating NUL.
    // Check the fully quoted command before presenting a launch for approval.
    if command.len() > 32767 {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    Ok(command)
}

impl PreparedExecutable {
    /// Caller must first persist the unique Invoking claim and recheck authority.
    /// A console program gets its own visible console, as when started from
    /// Explorer, instead of inheriting the application host's hidden one; GUI
    /// programs are not affected.
    pub(crate) fn invoke(self, dispatch_id: String) -> LaunchApplicationResult {
        self.invoke_with_flags(
            dispatch_id,
            CREATE_UNICODE_ENVIRONMENT | windows::Win32::System::Threading::CREATE_NEW_CONSOLE,
        )
    }

    /// Only the internal application host uses this: it runs without a
    /// visible console window.
    pub(crate) fn invoke_hidden_host(self, dispatch_id: String) -> LaunchApplicationResult {
        self.invoke_with_flags(
            dispatch_id,
            CREATE_UNICODE_ENVIRONMENT | windows::Win32::System::Threading::CREATE_NO_WINDOW,
        )
    }

    fn invoke_with_flags(
        self,
        dispatch_id: String,
        creation_flags: windows::Win32::System::Threading::PROCESS_CREATION_FLAGS,
    ) -> LaunchApplicationResult {
        let mut result = LaunchApplicationResult {
            diagnostic: None,
            dispatch_id,
            launch_outcome: LaunchOutcome::LaunchFailed,
            argument_delivery: if self.request.args.is_empty() {
                ArgumentDelivery::NotRequested
            } else {
                ArgumentDelivery::Unknown
            },
            failure_reason: None,
            requested_admin: self.request.run_as_admin,
            created_process_id: None,
            created_process_elevated: None,
            observations: vec![],
        };
        // External host Jobs are inherited normally; native launch does not
        // reject or escape them. It only avoids our per-command containment.
        if let Err(reason) = require_default_desktop() {
            result.failure_reason = Some(reason.reason);
            result.diagnostic = Some(reason.diagnostic);
            return result;
        }
        let application = wide(&self.identity.canonical_target);
        let cwd = wide(self.identity.resolved_cwd.as_deref().expect("prepared cwd"));
        let mut command = self.command_line;
        let mut desktop = wide("WinSta0\\Default");
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let created = unsafe {
            if self.uses_current_token {
                CreateProcessW(
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    false,
                    creation_flags,
                    Some(self.environment.0.cast_const()),
                    PCWSTR(cwd.as_ptr()),
                    &startup,
                    &mut process,
                )
            } else {
                CreateProcessAsUserW(
                    Some(self.token.0),
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    false,
                    creation_flags,
                    Some(self.environment.0.cast_const()),
                    PCWSTR(cwd.as_ptr()),
                    &startup,
                    &mut process,
                )
            }
        };
        match created {
            Ok(()) => {
                let _thread = OwnedHandle(process.hThread);
                let _process = OwnedHandle(process.hProcess);
                result.launch_outcome = LaunchOutcome::LaunchAccepted;
                result.argument_delivery = if self.request.args.is_empty() {
                    ArgumentDelivery::NotRequested
                } else {
                    ArgumentDelivery::Submitted
                };
                result.created_process_id = Some(process.dwProcessId);
                result.created_process_elevated = Some(self.elevated);
            }
            Err(error) => {
                let reason = match super::windows_diagnostics::win32_code(error.code().0) {
                    Some(740) => LaunchFailureReason::ElevationRequired,
                    Some(1314 | 1346 | 1349) if self.request.run_as_admin => {
                        LaunchFailureReason::AdminLaunchUnavailable
                    }
                    Some(5 | 1314) => LaunchFailureReason::PermissionDenied,
                    Some(2 | 3 | 193) => LaunchFailureReason::InvalidTarget,
                    _ => LaunchFailureReason::NativeFailure,
                };
                let failure = failure(
                    reason,
                    DiagnosticStage::ProcessCreation,
                    if self.uses_current_token {
                        "CreateProcessW"
                    } else if self.derived_token {
                        // A restricted version of the caller's own token needs
                        // no SeAssignPrimaryToken, but SeIncreaseQuota, which
                        // a policy can remove from Administrators (1314).
                        "CreateProcessAsUserW (derived token; requires SeIncreaseQuotaPrivilege)"
                    } else {
                        "CreateProcessAsUserW"
                    },
                    &error,
                );
                result.failure_reason = Some(failure.reason);
                result.diagnostic = Some(failure.diagnostic);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires LRDM_TEST_ELEVATION_FIXTURE, a controlled requireAdministrator executable"]
    fn controlled_elevation_failures_keep_native_diagnostics() {
        assert!(
            !current_elevated().unwrap(),
            "Run this check from an ordinary user process"
        );
        let target = std::env::var("LRDM_TEST_ELEVATION_FIXTURE").expect("controlled fixture path");
        let sid = storage_user_sid().unwrap();
        let session: u32 = catalog_session_identity()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let request = LaunchApplicationRequest {
            target: desk_agent_protocol::application_launch::ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: target,
            },
            args: vec![],
            cwd: None,
            run_as_admin: false,
        };
        let ordinary = prepare(&request, &sid, session)
            .unwrap_or_else(|error| panic!("{error:?}"))
            .invoke("test-ordinary".into());
        assert_eq!(
            ordinary.failure_reason,
            Some(LaunchFailureReason::ElevationRequired),
            "{ordinary:?}"
        );
        let diagnostic = ordinary.diagnostic.unwrap();
        assert_eq!(
            super::super::windows_diagnostics::win32_code(diagnostic.code.unwrap() as i32),
            Some(740)
        );
        assert_eq!(diagnostic.stage, DiagnosticStage::ProcessCreation);
        let administrator = LaunchApplicationRequest {
            run_as_admin: true,
            ..request
        };
        let result = match prepare(&administrator, &sid, session) {
            Ok(prepared) => prepared.invoke("test-administrator".into()),
            Err(error) => {
                assert!(
                    matches!(
                        error.reason,
                        LaunchFailureReason::AdminRequired
                            | LaunchFailureReason::AdminLaunchUnavailable
                    ),
                    "{error:?}"
                );
                assert!(!error.diagnostic.message.is_empty());
                return;
            }
        };
        assert!(
            matches!(
                result.failure_reason,
                Some(
                    LaunchFailureReason::AdminLaunchUnavailable
                        | LaunchFailureReason::PermissionDenied
                )
            ),
            "{result:?}"
        );
        assert_eq!(
            result.diagnostic.as_ref().unwrap().operation,
            "CreateProcessAsUserW"
        );
        assert!(result.diagnostic.unwrap().code.is_some());
    }
    #[test]
    fn command_limit_includes_quotes_separators_and_terminator() {
        let target = "C:\\app.exe";
        let overhead = checked_command_line(target, &[String::new()])
            .unwrap()
            .len();
        let at_limit = "a".repeat(32767 - overhead);
        assert_eq!(
            checked_command_line(target, &[at_limit.clone()])
                .unwrap()
                .len(),
            32767
        );
        assert_eq!(
            checked_command_line(target, &[at_limit + "a"]),
            Err(LaunchFailureReason::InvalidTarget.into())
        );
        assert_eq!(
            checked_command_line(target, &["\\".repeat(16384)]),
            Err(LaunchFailureReason::InvalidTarget.into())
        );
        assert_eq!(
            checked_command_line(target, &["\"".repeat(16384)]),
            Err(LaunchFailureReason::InvalidTarget.into())
        );
    }

    #[test]
    fn command_limit_counts_utf16_not_utf8_bytes() {
        let command = checked_command_line("C:\\app.exe", &["😀".repeat(8000)]).unwrap();
        let overhead = checked_command_line("C:\\app.exe", &[String::new()])
            .unwrap()
            .len();
        assert_eq!(command.len(), overhead + 16000);
    }
    #[test]
    fn argv_quoting_preserves_empty_space_quote_unicode_and_trailing_slashes() {
        assert_eq!(quote_argument(""), "\"\"");
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("中"), "\"中\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_argument("a\\"), "\"a\\\\\"");
        assert_eq!(quote_argument("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
