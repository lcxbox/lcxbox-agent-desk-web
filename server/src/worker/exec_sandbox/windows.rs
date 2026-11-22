//! Windows backend of the command write sandbox (plan 6.5): a Low integrity
//! token, inheritable Low labels on the write roots managed by a per-user
//! lease store, a private desktop, job UI limits, and a launcher that starts
//! the target with std so the command line matches an unrestricted command.
//! Validated on Windows 11 26H2 by `pocs/windows-exec-sandbox` (research
//! section 8).
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::Instant;

use desk_agent_protocol::exec::{ExecPlan, ExecSandboxSupport, ExecSandboxUnsupportedReason};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SE_KERNEL_OBJECT};
use windows::Win32::Security::{
    ACE_FLAGS, ACL, ACL_REVISION, AddMandatoryAce, GetLengthSid, GetTokenInformation,
    InitializeAcl, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::temp::PerExecTemp;
use super::windows_lease::{self, Lease};
use super::windows_spawn::{self, SpawnOutcome};
use super::windows_token::{self, OwnedHandle, PrivateDesktop, Sid, sid_string};
use super::{SandboxError, SandboxRoot, windows_label};

pub(super) fn real_path(handle: &File) -> io::Result<PathBuf> {
    windows_label::real_path(handle)
}

/// Windows refuses no program by name inside the sandbox; elevation is
/// handled by the prepare-time refusal (plan 6.3).
pub(super) fn refused_program(_exit_code: Option<i32>, _stderr: &str) -> bool {
    false
}

fn user_sid() -> io::Result<String> {
    desk_file_recovery::windows::current_user_sid()
}

/// Start-up self-test (plan 6.5.3, part four): label a one-file probe root,
/// let a launcher with the sandbox token write inside it (must succeed) and
/// next to it (must fail), then remove everything. Measured at 375–783 ms
/// (research 8.8).
pub(super) fn probe() -> ExecSandboxSupport {
    let started = std::time::Instant::now();
    match self_test() {
        Ok(()) => {
            log::info!(
                "exec sandbox: the Windows self-test passed in {} ms",
                started.elapsed().as_millis()
            );
            windows_lease::start_reaper();
            ExecSandboxSupport::Supported
        }
        Err(error) => {
            log::warn!("exec sandbox: the Windows self-test failed: {error}");
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported)
        }
    }
}

pub(super) fn self_test() -> io::Result<()> {
    self_test_in(&probe_local_root()?)
}

/// The folder the self-test treats as the local application data folder.
/// Tests use a folder of their own, so they never create the user's
/// application directory.
#[cfg(not(test))]
fn probe_local_root() -> io::Result<std::path::PathBuf> {
    windows_lease::local_app_data()
}

#[cfg(test)]
fn probe_local_root() -> io::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "lcxbox-exec-sandbox-test-local-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Run the self-test with `local` standing for the local application data
/// folder. The probe lives in a private child of the worker's application
/// directory, which only the private-directory code creates.
pub(super) fn self_test_in(local: &std::path::Path) -> io::Result<()> {
    let base = windows_lease::private_app_child(local, "exec-sandbox-probe")?;
    let dir = base.join(format!(
        "probe-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let root = dir.join("root");
    let sibling = dir.join("sibling");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&sibling)?;
    let result = (|| {
        std::fs::write(root.join("probe.txt"), b"lcxbox")?;
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS: required to open a directory.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(0x0200_0000)
                .open(&root)?
        };
        windows_label::check_volume(&handle).map_err(io::Error::other)?;
        let failures = windows_label::set_label(&root)?;
        if failures.count > 0 {
            return Err(io::Error::other(failures.sample.join("; ")));
        }
        let user = user_sid()?;
        windows_token::ensure_winsta_grant(&user)?;
        let token = windows_token::sandbox_token(&user)?;
        let mut desktop = PrivateDesktop::create(&user)?;
        let job = windows_spawn::anonymous_job()?;
        let env = windows_spawn::environment_block(&[]);
        let (inside, outside) = windows_spawn::self_test(
            windows_spawn::Spawn {
                token: token.raw(),
                job: job.raw(),
                desktop: &mut desktop,
                env_block: &env,
                launcher_cwd: &root,
                user: &user,
            },
            &root.join("inside.txt"),
            &sibling.join("outside.txt"),
        )?;
        if !inside || outside {
            return Err(io::Error::other(format!(
                "the sandbox boundary did not hold: inside written={inside}, outside written={outside}"
            )));
        }
        Ok(())
    })();
    let _ = windows_label::clear_label(&root);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// The owner SID of an opened object.
fn owner_sid(handle: &File) -> Option<String> {
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let code = unsafe {
        GetSecurityInfo(
            HANDLE(handle.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    let sid = (code.0 == 0).then(|| sid_string(owner).ok()).flatten();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    sid
}

/// The restriction of one execution: leases on its write roots and its
/// temporary directory. Dropping it records the roots' last use and releases
/// them, then removes the temporary directory.
pub(super) struct Prepared {
    leases: Vec<Lease>,
    temp: PerExecTemp,
    /// Roots owned by someone else (an elevated worker creates directories
    /// owned by Administrators, which is deny-only in the sandbox token): git
    /// gets `safe.directory` for them.
    foreign_owned: Vec<PathBuf>,
    user: String,
}

impl Prepared {
    /// Volume check, lease (labelling within the deadline) and temporary
    /// directory for `roots`. Blocking.
    pub(super) fn build(
        roots: &[SandboxRoot],
        private_dir: &Path,
        deadline: Instant,
    ) -> Result<Self, SandboxError> {
        let user = user_sid().map_err(|e| SandboxError::Restriction(e.to_string()))?;
        for root in roots {
            windows_label::check_volume(&root.handle).map_err(|reason| {
                SandboxError::Restriction(format!(
                    "{} cannot be a writable directory of a sandboxed command: {reason}",
                    root.path.display()
                ))
            })?;
        }
        let mut leases = Vec::with_capacity(roots.len());
        for root in roots {
            leases.push(windows_lease::acquire(&root.path, &root.handle, deadline)?);
        }
        let foreign_owned = roots
            .iter()
            .filter(|root| owner_sid(&root.handle).is_some_and(|o| !o.eq_ignore_ascii_case(&user)))
            .map(|root| root.path.clone())
            .collect();
        let temp = PerExecTemp::create(private_dir).map_err(SandboxError::TempUnavailable)?;
        Ok(Self {
            leases,
            temp,
            foreign_owned,
            user,
        })
    }

    /// `TEMP`/`TMP`, plus `safe.directory` for foreign-owned roots appended
    /// after any `GIT_CONFIG_*` entries already in the environment.
    pub(super) fn environment(&self) -> Vec<(String, String)> {
        let temp = self.temp.path().display().to_string();
        let mut vars = vec![("TEMP".to_owned(), temp.clone()), ("TMP".to_owned(), temp)];
        if !self.foreign_owned.is_empty() {
            let mut count: usize = std::env::var("GIT_CONFIG_COUNT")
                .ok()
                .and_then(|c| c.trim().parse().ok())
                .unwrap_or(0);
            for root in &self.foreign_owned {
                vars.push((
                    format!("GIT_CONFIG_KEY_{count}"),
                    "safe.directory".to_owned(),
                ));
                vars.push((
                    format!("GIT_CONFIG_VALUE_{count}"),
                    root.display().to_string().replace('\\', "/"),
                ));
                count += 1;
            }
            vars.push(("GIT_CONFIG_COUNT".to_owned(), count.to_string()));
        }
        vars
    }

    /// What the spawn needs, owned, so it can run on a blocking thread while
    /// `self` (leases, temporary directory) stays with the executor.
    pub(super) fn launch_parts(&self) -> LaunchParts {
        LaunchParts {
            environment: self.environment(),
            user: self.user.clone(),
            temp_dir: self.temp.path().to_path_buf(),
        }
    }
}

/// See [`Prepared::launch_parts`].
pub(crate) struct LaunchParts {
    environment: Vec<(String, String)>,
    user: String,
    temp_dir: PathBuf,
}

/// Start `plan` through the launcher inside `job` (the execution's
/// containment job, as a raw handle value). Blocking.
pub(crate) fn spawn(parts: LaunchParts, plan: &ExecPlan, job: usize) -> SpawnOutcome {
    let job = HANDLE(job as *mut c_void);
    let started = (|| -> io::Result<(OwnedHandle, PrivateDesktop)> {
        windows_spawn::restrict_ui(job)?;
        windows_token::ensure_winsta_grant(&parts.user)?;
        let token = windows_token::sandbox_token(&parts.user)?;
        let desktop = PrivateDesktop::create(&parts.user)?;
        Ok((token, desktop))
    })();
    let (token, mut desktop) = match started {
        Ok(v) => v,
        Err(e) => return SpawnOutcome::NotStarted(e),
    };
    let env = windows_spawn::environment_block(&parts.environment);
    let launcher_cwd = plan
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| parts.temp_dir.clone());
    let mut argv = Vec::with_capacity(plan.argv.len() + 1);
    argv.push(plan.program.clone());
    argv.extend(plan.argv.iter().cloned());
    windows_spawn::spawn_command(
        windows_spawn::Spawn {
            token: token.raw(),
            job,
            desktop: &mut desktop,
            env_block: &env,
            launcher_cwd: &launcher_cwd,
            user: &parts.user,
        },
        argv,
        plan.cwd.clone(),
    )
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Leases first (they record the last use), the directory afterwards.
        self.leases.clear();
    }
}

/// Worker self-protection (plan 6.5.3, part three): a Medium worker gives its
/// own process a Medium label with NO_WRITE_UP | NO_READ_UP | NO_EXECUTE_UP,
/// so Low processes (sandboxed commands) get no process right, while the
/// user's own Medium tools keep full access. An elevated worker is already out
/// of reach (its DACL does not grant the user). Failure is only logged.
pub(super) fn protect_worker_process() {
    if current_integrity_rid() != Some(0x2000) {
        return;
    }
    if let Err(error) = set_process_label() {
        log::warn!("exec sandbox: the worker process label could not be set: {error}");
    }
}

fn current_integrity_rid() -> Option<u32> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let token = OwnedHandle(token);
    let mut len = 0u32;
    unsafe {
        let _ = GetTokenInformation(token.raw(), TokenIntegrityLevel, None, 0, &mut len);
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        )
    }
    .ok()?;
    let label = unsafe { &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
    let sid = sid_string(label.Label.Sid).ok()?;
    sid.rsplit('-').next()?.parse().ok()
}

fn set_process_label() -> io::Result<()> {
    use windows::Win32::Security::Authorization::SetSecurityInfo;
    let medium = Sid::parse("S-1-16-8192")?;
    let total =
        (std::mem::size_of::<ACL>() + 8 + unsafe { GetLengthSid(medium.psid()) } as usize + 3) & !3;
    let mut buf = vec![0u64; total.div_ceil(8)];
    let acl = buf.as_mut_ptr() as *mut ACL;
    unsafe {
        InitializeAcl(acl, total as u32, ACL_REVISION).map_err(io::Error::other)?;
        // NO_WRITE_UP | NO_READ_UP | NO_EXECUTE_UP
        AddMandatoryAce(acl, ACL_REVISION, ACE_FLAGS(0), 0x7, medium.psid())
            .map_err(io::Error::other)?;
    }
    let code = unsafe {
        SetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(acl as *const ACL),
        )
    };
    if code.0 != 0 {
        return Err(io::Error::from_raw_os_error(code.0 as i32));
    }
    Ok(())
}

/// Revoke every unused label of the current user (the "revoke all sandbox
/// labels" entry points): (revoked, still in use).
pub(crate) fn revoke_all_labels() -> (usize, usize) {
    windows_lease::reap(true)
}

pub(crate) use windows_lease::{set_idle_minutes, worker_app_dir};
