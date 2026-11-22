//! Worker side of a Windows sandboxed spawn (plan 6.5.3, part one).
//!
//! The launcher (`crate::exec_sandbox_launcher`, `current_exe`) is created
//! with the sandbox token, `CREATE_SUSPENDED | CREATE_NO_WINDOW`, inside the
//! execution's job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`) and without inheritable
//! handles. It connects to a private, single-instance control pipe whose
//! client pid is checked; only then are the stdout/stderr write ends
//! duplicated into it and the request sent. No child-side handle is ever
//! inheritable in the worker, so concurrent std/tokio spawns cannot leak them.
//!
//! Only a well-formed `failed` reply means the target did not start. A
//! missing, malformed or late reply means the target may have run (it can
//! terminate its launcher), so the execution is reported as started.
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use super::windows_token::{OwnedHandle, PrivateDesktop, wide};
#[cfg(not(test))]
use crate::exec_sandbox_launcher::MODE;
use crate::exec_sandbox_launcher::{MAX_FRAME, Reply, Request};

/// How long the launcher may take to connect and to answer.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

fn win(error: windows::core::Error, what: &str) -> io::Error {
    io::Error::other(format!("{what}: {}", error.message().trim()))
}

/// UI limits of a sandboxed execution's job: handles, clipboard, global atoms,
/// system parameters, display settings, desktops and exiting Windows.
pub(crate) fn restrict_ui(job: HANDLE) -> io::Result<()> {
    let limits = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT(0xFF),
    };
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            &limits as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )
    }
    .map_err(|e| win(e, "SetInformationJobObject(UI restrictions)"))
}

/// An anonymous kill-on-close job with UI limits (the start-up self-test).
pub(crate) fn anonymous_job() -> io::Result<OwnedHandle> {
    let job = OwnedHandle(
        unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| win(e, "CreateJobObjectW"))?,
    );
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|e| win(e, "SetInformationJobObject(kill on close)"))?;
    restrict_ui(job.raw())?;
    Ok(job)
}

/// `KEY=VALUE\0...\0\0`, sorted case-insensitively as Windows expects.
pub(crate) fn environment_block(overrides: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !overrides.iter().any(|(o, _)| o.eq_ignore_ascii_case(k)))
        .collect();
    vars.extend(overrides.iter().cloned());
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

struct AttrList {
    buf: Vec<u64>,
    _jobs: Box<[HANDLE; 1]>,
}

impl AttrList {
    fn with_job(job: HANDLE) -> io::Result<Self> {
        let mut size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        }
        let mut list = Self {
            buf: vec![0u64; size.div_ceil(8).max(1)],
            _jobs: Box::new([job]),
        };
        unsafe {
            InitializeProcThreadAttributeList(Some(list.ptr()), 1, None, &mut size)
                .map_err(|e| win(e, "InitializeProcThreadAttributeList"))?;
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(list._jobs.as_ptr() as *const c_void),
                std::mem::size_of::<HANDLE>(),
                None,
                None,
            )
            .map_err(|e| win(e, "UpdateProcThreadAttribute(JOB_LIST)"))?;
        }
        Ok(list)
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_mut_ptr() as *mut c_void)
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

/// A private, single-instance pipe for the user, labelled Low so the Low
/// launcher can open it for writing.
fn control_pipe(name: &str, user: &str) -> io::Result<OwnedHandle> {
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{user})S:(ML;;NW;;;LW)"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(|e| {
        win(
            e,
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(pipe)",
        )
    })?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let full = wide(&format!(r"\\.\pipe\{name}"));
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(full.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            Some(&sa),
        )
    };
    unsafe {
        let _ =
            windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
    }
    if h.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(h))
}

/// A started launcher connected on its control pipe.
struct Launched {
    process: OwnedHandle,
    control: File,
}

/// Start the launcher and connect it; any failure here happens before a
/// request was sent, so the target certainly did not start.
fn launch(
    token: HANDLE,
    job: HANDLE,
    desktop: &mut PrivateDesktop,
    env_block: &[u16],
    cwd: &Path,
    user: &str,
) -> io::Result<Launched> {
    let exe = std::env::current_exe()?;
    let name = format!("lcxbox-exec-sandbox-{}", uuid::Uuid::new_v4().simple());
    let pipe = control_pipe(&name, user)?;
    let mut attrs = AttrList::with_job(job)?;
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = desktop.startup_ptr();
    si.lpAttributeList = attrs.ptr();
    #[cfg(not(test))]
    let mut command = wide(&format!("\"{}\" {MODE} {name}", exe.display()));
    // A unit-test binary cannot dispatch the launcher mode before libtest;
    // it runs one test that acts as the launcher (the pipe name is passed as
    // a second, never-matching filter).
    #[cfg(test)]
    let mut command = wide(&format!(
        "\"{}\" worker::exec_sandbox::tests_windows::launcher_entry {name} --exact --test-threads=1 -q",
        exe.display()
    ));
    let cwd_w = {
        use std::os::windows::ffi::OsStrExt;
        cwd.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_SUSPENDED
                | CREATE_NO_WINDOW,
            Some(env_block.as_ptr() as *const c_void),
            PCWSTR(cwd_w.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    }
    .map_err(|e| win(e, "CreateProcessAsUserW(launcher)"))?;
    drop(attrs);
    let process = OwnedHandle(pi.hProcess);
    let thread = OwnedHandle(pi.hThread);
    unsafe { ResumeThread(thread.raw()) };
    let kill = |process: &OwnedHandle| unsafe {
        let _ = TerminateProcess(process.raw(), 1);
    };
    // Connect with a bound: on a timeout the launcher is terminated, which
    // also releases the waiting connect.
    let raw = pipe.raw().0 as usize;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ok = match unsafe { ConnectNamedPipe(HANDLE(raw as *mut c_void), None) } {
            Ok(()) => true,
            // ERROR_PIPE_CONNECTED: the client was faster than this call.
            Err(e) => e.code().0 as u32 == 0x8007_0217,
        };
        let _ = tx.send(ok);
    });
    if !rx.recv_timeout(FRAME_TIMEOUT).unwrap_or(false) {
        kill(&process);
        return Err(io::Error::other("the sandbox launcher did not connect"));
    }
    let mut client = 0u32;
    let _ = unsafe { GetNamedPipeClientProcessId(pipe.raw(), &mut client) };
    if client != pi.dwProcessId {
        kill(&process);
        return Err(io::Error::other(
            "an unexpected process connected to the sandbox launcher pipe",
        ));
    }
    let control = unsafe { File::from_raw_handle(pipe.0.0 as RawHandle) };
    std::mem::forget(pipe);
    Ok(Launched { process, control })
}

fn send(control: &mut File, request: &Request) -> io::Result<()> {
    writeln!(
        control,
        "{}",
        serde_json::to_string(request).map_err(io::Error::other)?
    )?;
    control.flush()
}

/// The reply, or `None` when the launcher closed, answered garbage or took
/// longer than [`FRAME_TIMEOUT`].
fn receive(control: File) -> Option<Reply> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut line = String::new();
        let ok = BufReader::new(control.take(MAX_FRAME as u64))
            .read_line(&mut line)
            .is_ok();
        let _ = tx.send(
            ok.then(|| serde_json::from_str::<Reply>(line.trim()).ok())
                .flatten(),
        );
    });
    rx.recv_timeout(FRAME_TIMEOUT).ok().flatten()
}

/// A sandboxed command's launcher process with the read ends of its output.
pub(crate) struct SandboxedChild {
    process: Arc<OwnedHandle>,
    pub(crate) stdout: Option<tokio::fs::File>,
    pub(crate) stderr: Option<tokio::fs::File>,
}

impl SandboxedChild {
    pub(crate) fn start_kill(&mut self) -> io::Result<()> {
        unsafe { TerminateProcess(self.process.raw(), 1) }
            .map_err(|e| win(e, "TerminateProcess(launcher)"))
    }

    /// The launcher's exit status, which is the target's (full 32 bits).
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let process = Arc::clone(&self.process);
        tokio::task::spawn_blocking(move || {
            unsafe { WaitForSingleObject(process.raw(), INFINITE) };
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(process.raw(), &mut code) }
                .map_err(|e| win(e, "GetExitCodeProcess"))?;
            Ok(ExitStatus::from_raw(code))
        })
        .await
        .map_err(io::Error::other)?
    }
}

pub(crate) enum SpawnOutcome {
    /// The target was not started (only a `failed` reply, or a failure
    /// before the request was sent).
    NotStarted(io::Error),
    /// The target started, or may have; `problem` explains a missing or
    /// malformed confirmation.
    Started {
        child: Box<SandboxedChild>,
        problem: Option<String>,
    },
}

pub(crate) struct Spawn<'a> {
    pub token: HANDLE,
    pub job: HANDLE,
    pub desktop: &'a mut PrivateDesktop,
    pub env_block: &'a [u16],
    /// Working directory of the launcher process.
    pub launcher_cwd: &'a Path,
    pub user: &'a str,
}

fn pipe_pair() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    unsafe { CreatePipe(&mut r, &mut w, None, 0) }.map_err(|e| win(e, "CreatePipe"))?;
    Ok((OwnedHandle(r), OwnedHandle(w)))
}

fn duplicate_into(process: &OwnedHandle, handle: &OwnedHandle) -> io::Result<u64> {
    let mut target = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.raw(),
            process.raw(),
            &mut target,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .map_err(|e| win(e, "DuplicateHandle"))?;
    Ok(target.0 as u64)
}

fn async_reader(handle: OwnedHandle) -> tokio::fs::File {
    let file = unsafe { File::from_raw_handle(handle.0.0 as RawHandle) };
    std::mem::forget(handle);
    tokio::fs::File::from_std(file)
}

/// Start `argv` in the sandbox. Blocking.
pub(crate) fn spawn_command(
    spawn: Spawn<'_>,
    argv: Vec<String>,
    cwd: Option<String>,
) -> SpawnOutcome {
    let launched = match launch(
        spawn.token,
        spawn.job,
        spawn.desktop,
        spawn.env_block,
        spawn.launcher_cwd,
        spawn.user,
    ) {
        Ok(l) => l,
        Err(e) => return SpawnOutcome::NotStarted(e),
    };
    let Launched {
        process,
        mut control,
    } = launched;
    let pipes = pipe_pair().and_then(|out| pipe_pair().map(|err| (out, err)));
    let ((out_r, out_w), (err_r, err_w)) = match pipes {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                let _ = TerminateProcess(process.raw(), 1);
            }
            return SpawnOutcome::NotStarted(e);
        }
    };
    let request = duplicate_into(&process, &out_w).and_then(|stdout| {
        duplicate_into(&process, &err_w).map(|stderr| Request::Run {
            argv,
            cwd,
            stdout,
            stderr,
        })
    });
    // Our copies of the write ends go away; EOF then follows the target.
    drop(out_w);
    drop(err_w);
    let sent = request.and_then(|r| send(&mut control, &r));
    if let Err(e) = sent {
        unsafe {
            let _ = TerminateProcess(process.raw(), 1);
        }
        return SpawnOutcome::NotStarted(e);
    }
    let reply = receive(control);
    let child = Box::new(SandboxedChild {
        process: Arc::new(process),
        stdout: Some(async_reader(out_r)),
        stderr: Some(async_reader(err_r)),
    });
    match reply {
        Some(Reply::Started { .. }) => SpawnOutcome::Started {
            child,
            problem: None,
        },
        Some(Reply::Failed { error, os_code }) => SpawnOutcome::NotStarted(match os_code {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::other(error),
        }),
        Some(other) => SpawnOutcome::Started {
            child,
            problem: Some(format!(
                "the sandbox launcher answered unexpectedly: {other:?}"
            )),
        },
        None => SpawnOutcome::Started {
            child,
            problem: Some("the sandbox launcher did not confirm the start".into()),
        },
    }
}

/// Run the start-up self-test through a launcher: (inside written, outside
/// written). Blocking.
pub(crate) fn self_test(
    spawn: Spawn<'_>,
    inside: &Path,
    outside: &Path,
) -> io::Result<(bool, bool)> {
    let Launched {
        process,
        mut control,
    } = launch(
        spawn.token,
        spawn.job,
        spawn.desktop,
        spawn.env_block,
        spawn.launcher_cwd,
        spawn.user,
    )?;
    send(
        &mut control,
        &Request::SelfTest {
            inside: inside.display().to_string(),
            outside: outside.display().to_string(),
        },
    )?;
    let reply = receive(control);
    unsafe { WaitForSingleObject(process.raw(), 5000) };
    match reply {
        Some(Reply::SelfTest { inside, outside }) => Ok((inside, outside)),
        other => Err(io::Error::other(format!(
            "the sandbox launcher self-test did not answer: {other:?}"
        ))),
    }
}
