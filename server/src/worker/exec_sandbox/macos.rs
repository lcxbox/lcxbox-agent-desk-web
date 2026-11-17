//! Seatbelt sandbox on macOS through `/usr/bin/sandbox-exec`.
//!
//! The profile allows everything except file writes, then allows writes below
//! the write roots, the execution's temporary directory and the user
//! temporary directory (`DARWIN_USER_TEMP_DIR`, without this product's
//! `lcxl-`/`lcxl_` entries), plus the data of a few character devices and
//! `/dev/fd/N`. Every path reaches the profile as a `-D` parameter, never as
//! profile text, so directory names cannot inject rules; parameters are real
//! paths taken from opened handles because Seatbelt matches resolved paths.
//!
//! The profile also refuses renaming a write root itself, the
//! read-only-descriptor fcntls `F_MAKECOMPRESSED`/`F_TRANSFEREXTENTS`,
//! `TIOCSTI`, job creation, LaunchServices opens, Apple Events, preference
//! writes, and reading the content of this product's configuration and data
//! directories (their metadata stays readable so that the execution's
//! temporary directory below the data directory can be reached by path).
//! `sandbox-exec` replaces itself with the command, so the pid and process
//! group are the command's.
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::{
    ExecSandboxSupport, ExecSandboxUnsupportedReason, PerExecTemp, SandboxError, SandboxRoot,
};

pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `TIOCSTI` (`0x80017472`); profiles take decimal numbers only.
const TIOCSTI: u64 = 2_147_578_994;

/// The resolved path of an opened file (`F_GETPATH`).
pub(super) fn real_path(handle: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    if unsafe {
        libc::fcntl(
            handle.as_raw_fd(),
            libc::F_GETPATH,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let length = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
    buffer.truncate(length);
    let path = PathBuf::from(OsString::from_vec(buffer));
    if !path.is_absolute() {
        return Err(io::Error::other(
            "the opened directory has no absolute path",
        ));
    }
    Ok(path)
}

/// The real path of the user temporary directory, if the system names one.
pub(super) fn user_temp_dir() -> Option<PathBuf> {
    let length = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0u8; length];
    if unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            length,
        )
    } == 0
    {
        return None;
    }
    buffer.truncate(buffer.iter().position(|byte| *byte == 0).unwrap_or(length));
    std::fs::canonicalize(OsStr::from_bytes(&buffer)).ok()
}

/// The profile of one execution: the policy text names parameters only.
pub(super) struct Prepared {
    profile: String,
    parameters: Vec<(String, PathBuf)>,
}

impl Prepared {
    pub(super) fn build(
        roots: &[SandboxRoot],
        temp: &PerExecTemp,
        pty_slave: Option<&Path>,
        read_protected: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let read_protected = read_protected
            .iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .collect::<Vec<_>>();
        Ok(Self::assemble(
            roots.iter().map(|root| root.path.clone()).collect(),
            temp.path().to_path_buf(),
            user_temp_dir(),
            pty_slave.map(Path::to_path_buf),
            read_protected,
        ))
    }

    fn assemble(
        roots: Vec<PathBuf>,
        temp: PathBuf,
        user_temp: Option<PathBuf>,
        pty_slave: Option<PathBuf>,
        read_protected: Vec<PathBuf>,
    ) -> Self {
        let mut parameters = vec![("TMP".to_owned(), temp)];
        let roots: Vec<String> = roots
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let name = format!("ROOT{index}");
                parameters.push((name.clone(), path));
                name
            })
            .collect();
        let protected: Vec<String> = read_protected
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let name = format!("PRIVATE{index}");
                parameters.push((name.clone(), path));
                name
            })
            .collect();
        let user_temp = user_temp.map(|path| parameters.push(("USER_TMP".to_owned(), path)));
        let pty = pty_slave.map(|path| parameters.push(("PTY".to_owned(), path)));

        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        profile.push_str("(allow file-write*\n  (subpath (param \"TMP\"))");
        if user_temp.is_some() {
            profile.push_str("\n  (subpath (param \"USER_TMP\"))");
        }
        for root in &roots {
            let _ = write!(profile, "\n  (subpath (param \"{root}\"))");
        }
        profile.push_str(")\n");
        if user_temp.is_some() {
            profile.push_str(concat!(
                "(deny file-write*\n",
                "  (prefix (string-append (param \"USER_TMP\") \"/lcxl-\"))\n",
                "  (prefix (string-append (param \"USER_TMP\") \"/lcxl_\")))\n",
            ));
        }
        profile.push_str("(allow file-write-data");
        for device in ["/dev/null", "/dev/zero", "/dev/tty", "/dev/dtracehelper"] {
            let _ = write!(
                profile,
                "\n  (require-all (literal \"{device}\") (vnode-type CHARACTER-DEVICE))"
            );
        }
        profile.push_str("\n  (regex #\"^/dev/fd/[0-9]+$\")");
        if pty.is_some() {
            profile.push_str(
                "\n  (require-all (literal (param \"PTY\")) (vnode-type CHARACTER-DEVICE))",
            );
        }
        profile.push_str(")\n");
        profile.push_str(
            "(deny file-write-unlink\n  (require-all (literal (param \"TMP\")) (vnode-type DIRECTORY))",
        );
        for root in &roots {
            let _ = write!(
                profile,
                "\n  (require-all (literal (param \"{root}\")) (vnode-type DIRECTORY))"
            );
        }
        profile.push_str(")\n");
        if !protected.is_empty() {
            profile.push_str("(deny file-read-data");
            for name in &protected {
                let _ = write!(profile, "\n  (subpath (param \"{name}\"))");
            }
            // The re-allowance must name the same operation: a deny of
            // `file-read-data` outranks an allow of `file-read*`.
            profile.push_str(")\n(allow file-read-data (subpath (param \"TMP\")))\n");
        }
        let _ = write!(
            profile,
            concat!(
                "(deny system-fcntl (fcntl-command 80 110))\n",
                "(deny file-ioctl (ioctl-command {tiocsti}))\n",
                "(deny job-creation)\n",
                "(deny lsopen)\n",
                "(deny appleevent-send)\n",
                "(deny user-preference-write)\n",
            ),
            tiocsti = TIOCSTI
        );
        Self {
            profile,
            parameters,
        }
    }

    #[cfg(test)]
    pub(super) fn profile(&self) -> &str {
        &self.profile
    }

    /// `sandbox-exec -p <profile> -D NAME=<path>… -- <program> <args>…`.
    pub(super) fn command_line(&self, program: &str, args: &[String]) -> Vec<OsString> {
        let mut line = vec![
            OsString::from(SANDBOX_EXEC),
            OsString::from("-p"),
            OsString::from(&self.profile),
        ];
        for (name, path) in &self.parameters {
            let mut definition = OsString::from(format!("{name}="));
            definition.push(path.as_os_str());
            line.push(OsString::from("-D"));
            line.push(definition);
        }
        line.push(OsString::from("--"));
        line.extend(super::command_line(program, args));
        line
    }

    pub(super) fn pipe_command(&self, program: &str, args: &[String]) -> tokio::process::Command {
        let line = self.command_line(program, args);
        let mut command = tokio::process::Command::new(&line[0]);
        command.args(&line[1..]);
        command
    }
}

/// Supported when `sandbox-exec` exists and runs `/usr/bin/true` under the
/// full profile with every kind of parameter.
pub(super) fn probe() -> ExecSandboxSupport {
    match run_probe() {
        Ok(true) => ExecSandboxSupport::Supported,
        Ok(false) | Err(_) => {
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported)
        }
    }
}

fn run_probe() -> io::Result<bool> {
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Ok(false);
    }
    let scratch = tempfile::Builder::new().prefix("sandbox-probe").tempdir()?;
    let base = std::fs::canonicalize(scratch.path())?;
    for directory in ["root", "tmp", "private"] {
        std::fs::create_dir(base.join(directory))?;
    }
    let prepared = Prepared::assemble(
        vec![base.join("root")],
        base.join("tmp"),
        user_temp_dir(),
        None,
        vec![base.join("private")],
    );
    let line = prepared.command_line("/usr/bin/true", &[]);
    let status = std::process::Command::new(&line[0])
        .args(&line[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

/// Whether a failed sandboxed command looks like a program Seatbelt refused
/// to run: exec of a setuid or otherwise restricted executable (`ps`, `top`,
/// `su`, `sudo`, `at`, `crontab`) fails with status 126 and a permission
/// error.
pub(super) fn refused_program(exit_code: Option<i32>, stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    exit_code == Some(126)
        && (stderr.contains("operation not permitted") || stderr.contains("permission denied"))
}
