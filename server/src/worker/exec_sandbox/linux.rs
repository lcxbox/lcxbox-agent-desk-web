//! Landlock sandbox on Linux.
//!
//! The ruleset handles exactly the write rights of ABI 3
//! (`AccessFs::from_write(ABI::V3)`, including `REFER` and `TRUNCATE`) as a
//! hard requirement, whatever newer ABI the kernel offers; reads and execution
//! are not handled and so stay unrestricted. It grants every handled right
//! below the write roots, the execution's temporary directory and `/dev/shm`,
//! and `WRITE_FILE | TRUNCATE` on `/dev/null`, `/dev/tty` and the PTY slave of
//! the execution. Directory rules use the already opened, identity-checked
//! handles, so a root renamed afterwards keeps its rule and a new directory of
//! the same name gets none.
//!
//! The worker builds the ruleset before forking; a restriction step then only
//! issues `prctl(PR_SET_NO_NEW_PRIVS)` and `landlock_restrict_self`, both
//! async-signal-safe raw system calls:
//! - the pipe executor runs that step in the forked child (`pre_exec`);
//! - the PTY executor (portable-pty has no pre-exec hook) runs it on a
//!   dedicated thread that then spawns the command and exits, so only that
//!   thread and the processes it creates are restricted.
//!
//! Landlock does not restrict metadata changes (chmod, chown, utimes, xattr).
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use landlock::{
    ABI, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr,
};

use super::{
    ExecSandboxSupport, ExecSandboxUnsupportedReason, PerExecTemp, SandboxError, SandboxRoot,
};

/// `LANDLOCK_CREATE_RULESET_VERSION`.
const CREATE_RULESET_VERSION: libc::c_uint = 1;
/// The lowest ABI that also restricts truncation.
pub(super) const MINIMUM_ABI: i64 = 3;

/// The resolved path of an opened file (`/proc/self/fd/N`).
pub(super) fn real_path(handle: &File) -> io::Result<PathBuf> {
    let path = std::fs::read_link(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
    if !path.is_absolute() {
        return Err(io::Error::other(
            "the opened directory has no absolute path",
        ));
    }
    Ok(path)
}

/// The Landlock ABI of the running kernel, or the errno of the probe
/// (`ENOSYS` without Landlock, `EOPNOTSUPP` when it is not enabled, `EPERM`
/// under a seccomp filter).
pub(super) fn abi_version() -> Result<i64, i32> {
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if version < 0 {
        return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(version)
}

pub(super) fn support_for(abi: Result<i64, i32>) -> ExecSandboxSupport {
    match abi {
        Ok(version) if version >= MINIMUM_ABI => ExecSandboxSupport::Supported,
        _ => ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
    }
}

pub(super) fn probe() -> ExecSandboxSupport {
    support_for(abi_version())
}

fn restriction_error(cause: impl std::fmt::Display) -> SandboxError {
    SandboxError::Restriction(cause.to_string())
}

/// Every handled right: write roots, the temporary directory, `/dev/shm`.
fn directory_access() -> BitFlags<AccessFs> {
    AccessFs::from_write(ABI::V3)
}

/// The rights a single device file may carry.
fn device_access() -> BitFlags<AccessFs> {
    AccessFs::WriteFile | AccessFs::Truncate
}

/// A built ruleset, waiting to be applied by a restriction step.
pub(super) struct Prepared {
    ruleset: OwnedFd,
}

impl Prepared {
    pub(super) fn build(
        roots: &[SandboxRoot],
        temp: &PerExecTemp,
        pty_slave: Option<&Path>,
        _read_protected: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(directory_access())
            .and_then(Ruleset::create)
            .map_err(restriction_error)?;
        for root in roots {
            ruleset = ruleset
                .add_rule(PathBeneath::new(&root.handle, directory_access()))
                .map_err(restriction_error)?;
        }
        ruleset = ruleset
            .add_rule(PathBeneath::new(temp.handle(), directory_access()))
            .map_err(restriction_error)?;
        if std::fs::metadata("/dev/shm").is_ok_and(|metadata| metadata.is_dir()) {
            let shm = PathFd::new("/dev/shm").map_err(restriction_error)?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(shm, directory_access()))
                .map_err(restriction_error)?;
        }
        let devices = [Path::new("/dev/null"), Path::new("/dev/tty")];
        for device in devices.into_iter().chain(pty_slave) {
            let file = PathFd::new(device).map_err(restriction_error)?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(file, device_access()))
                .map_err(restriction_error)?;
        }
        let ruleset: Option<OwnedFd> = ruleset.into();
        let ruleset = ruleset.ok_or_else(|| {
            SandboxError::Restriction("the kernel created no Landlock ruleset".to_owned())
        })?;
        Ok(Self { ruleset })
    }

    pub(super) fn pipe_command(&self, program: &str, args: &[String]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        let ruleset = self.ruleset.as_raw_fd();
        // SAFETY: the hook only issues raw system calls and builds an
        // `io::Error` from errno, neither of which allocates. The ruleset
        // descriptor is close-on-exec and stays open while `self` lives,
        // which the caller guarantees until the spawn returned.
        unsafe {
            command.pre_exec(move || restrict_current_thread(ruleset));
        }
        command
    }

    pub(super) fn spawn_restricted<T: Send>(
        &self,
        spawn: impl FnOnce() -> T + Send,
    ) -> io::Result<T> {
        let ruleset = self.ruleset.as_raw_fd();
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("exec-sandbox-spawn".to_owned())
                .spawn_scoped(scope, move || {
                    restrict_current_thread(ruleset)?;
                    Ok(spawn())
                })?
                .join()
                .map_err(|_| io::Error::other("the sandbox spawn thread panicked"))?
        })
    }
}

/// Restrict the calling thread and everything it creates afterwards.
/// Async-signal-safe.
fn restrict_current_thread(ruleset: RawFd) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether a failed sandboxed command looks like a setuid program refused by
/// `no_new_privs`: sudo names the flag (older releases report that its
/// effective uid is not 0), pkexec that it must be setuid root.
pub(super) fn refused_program(_exit_code: Option<i32>, stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    [
        "no new privileges",
        "effective uid is not 0",
        "must be setuid root",
    ]
    .iter()
    .any(|message| stderr.contains(message))
}
