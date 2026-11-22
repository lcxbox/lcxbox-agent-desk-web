//! Launcher of a Windows sandboxed command (plan 6.5.3, part one).
//!
//! The worker creates this executable (`current_exe`) with the sandbox token,
//! suspended and inside the execution's job, without inheritable handles, as
//! `<exe> __exec-sandbox-launcher <pipe>`. Both binaries (the server and the
//! desktop app) dispatch here before argument parsing, logging, settings or
//! the single-instance lock.
//!
//! The launcher opens the worker's private control pipe (not inheritable, so
//! the target never holds it), reads one request frame, and either starts the
//! target with `std::process::Command` — command-line quoting, batch-file rules
//! and program lookup are std's, exactly as for an unrestricted command — or
//! runs the start-up self-test. It answers with one reply frame and then passes
//! the target's full 32-bit exit code through.

use serde::{Deserialize, Serialize};

/// The mode argument (argv[1]).
pub const MODE: &str = "__exec-sandbox-launcher";

/// Helper mode revoking the current user's unused sandbox labels, for an
/// uninstaller hook (`NSIS_HOOK_PREUNINSTALL`); prints the counts.
pub const REVOKE_MODE: &str = "__exec-sandbox-revoke-all";

/// Exit code of a launcher that did not start a target.
pub const NOT_STARTED: i32 = 0x7F;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Start `argv`; `stdout`/`stderr` are handle values the worker duplicated
    /// into this process.
    Run {
        argv: Vec<String>,
        cwd: Option<String>,
        stdout: u64,
        stderr: u64,
    },
    /// Start-up self-test: write `inside` (must succeed) and `outside` (must
    /// fail).
    SelfTest { inside: String, outside: String },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Started {
        pid: u32,
    },
    /// The target was not started; only this frame means "not started".
    Failed {
        error: String,
        os_code: Option<i32>,
    },
    SelfTest {
        inside: bool,
        outside: bool,
    },
}

/// Largest request frame accepted, in bytes.
pub const MAX_FRAME: usize = 1 << 20;

/// Run the launcher when this process was started as one; returns its exit
/// code. `None` when the arguments do not name the launcher mode.
#[cfg(windows)]
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let mode = args.next();
    if mode.as_deref() == Some(std::ffi::OsStr::new(REVOKE_MODE)) {
        let (revoked, in_use) = revoke_current_user_labels();
        println!("revoked {revoked} sandbox label(s); {in_use} still in use");
        return Some(0);
    }
    if mode.as_deref() != Some(std::ffi::OsStr::new(MODE)) {
        return None;
    }
    let pipe = args.next()?.into_string().ok()?;
    if args.next().is_some() {
        return Some(NOT_STARTED);
    }
    Some(imp::run(&pipe))
}

/// Revoke every sandbox label of the current user that no running command
/// uses (plan 6.5.3, part six). Returns `(revoked, still in use)`; labels in
/// use are revoked by their worker once the lease runs out.
#[cfg(windows)]
pub fn revoke_current_user_labels() -> (usize, usize) {
    crate::worker::exec_sandbox::revoke_all_labels()
}

/// Run the launcher on `pipe` (unit tests start the test binary as the
/// launcher, see `worker::exec_sandbox::tests_windows`).
#[cfg(windows)]
pub fn run_with_pipe(pipe: &str) -> i32 {
    imp::run(pipe)
}

#[cfg(windows)]
mod imp {
    use super::{MAX_FRAME, NOT_STARTED, Reply, Request};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub(super) fn run(pipe: &str) -> i32 {
        // `File::open` does not create inheritable handles.
        let path = format!(r"\\.\pipe\{pipe}");
        let Ok(control) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            return NOT_STARTED;
        };
        let Ok(reader) = control.try_clone() else {
            return NOT_STARTED;
        };
        let mut line = String::new();
        if BufReader::new(reader.take(MAX_FRAME as u64))
            .read_line(&mut line)
            .is_err()
        {
            return NOT_STARTED;
        }
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            return NOT_STARTED;
        };
        let mut control = control;
        let mut reply = |reply: &Reply| {
            let _ = writeln!(
                control,
                "{}",
                serde_json::to_string(reply).unwrap_or_default()
            );
            let _ = control.flush();
        };
        match request {
            Request::SelfTest { inside, outside } => {
                let write = |path: &str| std::fs::write(path, b"lcxbox").is_ok();
                let inside = write(&inside);
                let outside = write(&outside);
                reply(&Reply::SelfTest { inside, outside });
                0
            }
            Request::Run {
                argv,
                cwd,
                stdout,
                stderr,
            } => {
                // Take ownership of the duplicated handles first, so they
                // are closed on every path.
                let stdout = unsafe { OwnedHandle::from_raw_handle(stdout as RawHandle) };
                let stderr = unsafe { OwnedHandle::from_raw_handle(stderr as RawHandle) };
                let Some((program, args)) = argv.split_first() else {
                    reply(&Reply::Failed {
                        error: "empty argv".into(),
                        os_code: None,
                    });
                    return NOT_STARTED;
                };
                let mut command = std::process::Command::new(program);
                command
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::from(stdout))
                    .stderr(std::process::Stdio::from(stderr))
                    // Console programs get a hidden console of their own
                    // instead of a window on the private desktop.
                    .creation_flags(CREATE_NO_WINDOW);
                if let Some(cwd) = cwd {
                    command.current_dir(cwd);
                }
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        reply(&Reply::Failed {
                            error: error.to_string(),
                            os_code: error.raw_os_error(),
                        });
                        return NOT_STARTED;
                    }
                };
                reply(&Reply::Started { pid: child.id() });
                drop(control);
                match child.wait() {
                    // The full 32-bit exit code, NTSTATUS values included.
                    Ok(status) => status.code().unwrap_or(1),
                    Err(_) => 1,
                }
            }
        }
    }
}
