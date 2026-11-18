//! Windows command sandbox through the real executor (plan 6.5, section 11
//! "worker（阶段三 Windows）"). The test binary doubles as the launcher: see
//! [`launcher_entry`].
use super::*;
use crate::worker::exec::execute_plan_cancellable;
use desk_agent_protocol::exec::{ExecSandboxMode, ExecShellKind};
use desk_agent_protocol::{
    AgentErrorKind, AgentOutcome, ExecOutput, ExecOutputStreams, OperationOutput,
};
use std::sync::Mutex;

/// The launcher mode of the test binary: `windows_spawn` starts it with the
/// pipe name as an extra (never matching) test filter.
#[test]
fn launcher_entry() {
    if let Some(pipe) = std::env::args().find(|a| a.starts_with("lcxl-exec-sandbox-")) {
        std::process::exit(crate::exec_sandbox_launcher::run_with_pipe(&pipe));
    }
}

/// Labelling is one-at-a-time per process; the tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn available() -> bool {
    match sandbox_support() {
        ExecSandboxSupport::Supported => true,
        ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity) => {
            eprintln!("running as SYSTEM: the sandbox is unsupported, skipping");
            false
        }
        other => panic!("the Windows self-test must pass on a supported host: {other:?}"),
    }
}

struct Fixture {
    _scratch: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    sandbox: Arc<ExecSandbox>,
}

fn fixture() -> Fixture {
    let scratch = tempfile::Builder::new()
        .prefix("exec-sandbox-win")
        .tempdir()
        .unwrap();
    let base = scratch.path().to_path_buf();
    let private = base.join("private");
    let root = base.join("root");
    let outside = base.join("outside");
    for d in [&private, &root, &outside] {
        std::fs::create_dir(d).unwrap();
    }
    std::fs::write(outside.join("existing.txt"), "keep").unwrap();
    let sandbox = Arc::new(ExecSandbox::with_support(
        sandbox_support(),
        private.clone(),
        ProtectedRoots::new(None, Vec::new(), [private.clone()]),
        Vec::new(),
    ));
    Fixture {
        _scratch: scratch,
        root,
        outside,
        sandbox,
    }
}

fn open_dir(path: &Path) -> File {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0200_0000)
        .open(path)
        .unwrap()
}

fn plan(program: &str, argv: &[&str], cwd: Option<&Path>) -> ExecPlan {
    ExecPlan {
        exec_request_id: desk_agent_protocol::exec::ExecRequestId("exec_t".into()),
        execution_generation: format!("gen_t_{}", uuid::Uuid::new_v4()),
        program: program.into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: cwd.map(|p| p.display().to_string()),
        io_mode: desk_agent_protocol::exec::ExecIoMode::NonInteractive,
        shell: ExecShellKind::Native,
        risk: desk_agent_protocol::RiskLevel::Low,
        execution_basis: desk_agent_protocol::exec::ExecExecutionBasis::Template,
        template_id: "test".into(),
        approval_id: desk_agent_protocol::exec::ApprovalId("appr_t".into()),
        authorization: Default::default(),
        fingerprint: "fp".into(),
        timeout_ms: 60_000,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 64 * 1024,
        containment: Default::default(),
        snapshot_roots: Vec::new(),
        sandbox_mode: ExecSandboxMode::Sandboxed,
    }
}

fn run(fixture: &Fixture, plan: &ExecPlan) -> ExecOutput {
    let handle = open_dir(&fixture.root);
    let prepared = fixture
        .sandbox
        .prepare_handles(&[&handle], None)
        .expect("prepare");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    match runtime.block_on(execute_plan_cancellable(
        plan,
        ExecLaunch::Sandboxed(prepared),
        |_| {},
        None,
    )) {
        AgentOutcome::Ok(OperationOutput::Exec(output)) => output,
        other => panic!("expected exec output, got {other:?}"),
    }
}

fn stdout(output: &ExecOutput) -> &str {
    match &output.streams {
        ExecOutputStreams::Split { stdout, .. } => stdout,
        other => panic!("unexpected streams {other:?}"),
    }
}

#[test]
fn the_self_test_reports_its_cause() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if execution_identity().is_privileged() {
        return;
    }
    platform::self_test().expect("Windows sandbox self-test");
}

#[test]
fn the_label_store_opens_after_the_self_test_in_a_fresh_profile() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let local = tempfile::tempdir().unwrap();
    if !execution_identity().is_privileged() {
        platform::self_test_in(local.path()).expect("Windows sandbox self-test");
    }
    windows_lease::open_store_in(local.path()).expect("the store after the self-test");
    // An application directory created with an inherited DACL is refused,
    // so nothing but the private-directory code may create it.
    let plain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(plain.path().join("lcxl-remote-desk-worker").join("probe")).unwrap();
    assert!(windows_lease::open_store_in(plain.path()).is_err());
}

#[test]
fn the_self_test_passes_on_this_host() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _ = available();
}

#[test]
fn writes_land_in_the_root_and_temp_but_not_outside() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let inside = f.root.join("inside.txt");
    let outside = f.outside.join("outside.txt");
    let existing = f.outside.join("existing.txt");
    // No embedded quotes: std escapes them as \" which cmd does not parse.
    // The temporary paths contain no spaces.
    let script = format!(
        "echo x> {} & echo x> {} & del {} & echo x> %TEMP%\\t.txt && echo temp-ok & echo done",
        inside.display(),
        outside.display(),
        existing.display()
    );
    let output = run(&f, &plan("cmd.exe", &["/d", "/c", &script], Some(&f.root)));
    assert!(output.started);
    assert!(stdout(&output).contains("done"), "{output:?}");
    assert!(stdout(&output).contains("temp-ok"), "{output:?}");
    assert!(inside.exists(), "a root write succeeds");
    assert!(!outside.exists(), "a write outside the root fails");
    assert!(existing.exists(), "a delete outside the root fails");
    assert!(
        windows_label::has_root_label(&f.root),
        "the root keeps its label while the lease runs"
    );
}

#[test]
fn a_missing_program_is_reported_as_not_started() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let output = run(
        &f,
        &plan("lcxl-exec-sandbox-no-such-program.exe", &[], None),
    );
    assert!(!output.started, "{output:?}");
}

#[test]
fn the_full_exit_code_passes_through() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let output = run(
        &f,
        &plan("cmd.exe", &["/d", "/c", "exit -1073741515"], None),
    );
    assert!(output.started);
    assert_eq!(output.exit_code, Some(-1_073_741_515));
}

#[test]
fn the_command_line_matches_an_unrestricted_spawn() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let bat = f.root.join("echo.bat");
    std::fs::write(&bat, "@echo [%*]\r\n").unwrap();
    let args = ["a&b", "x y", "q\"uote", "back\\slash\\"];
    let output = run(&f, &plan(&bat.display().to_string(), &args, Some(&f.root)));
    let direct = std::process::Command::new(&bat)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        stdout(&output).trim(),
        String::from_utf8_lossy(&direct.stdout).trim()
    );
}

#[test]
fn a_root_with_hard_links_is_refused() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let target = f.outside.join("linked.txt");
    std::fs::write(&target, "outside").unwrap();
    std::fs::hard_link(&target, f.root.join("link.txt")).unwrap();
    let handle = open_dir(&f.root);
    match f.sandbox.prepare_handles(&[&handle], None) {
        Err(SandboxError::Restriction(reason)) => assert!(reason.contains("hard link"), "{reason}"),
        other => panic!("expected a refusal, got {:?}", other.err()),
    }
    assert!(
        !windows_label::is_low_labelled(&target),
        "the outside name never gets the label"
    );
}

#[test]
fn revoking_all_removes_unused_labels() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let output = run(
        &f,
        &plan("cmd.exe", &["/d", "/c", "echo hi"], Some(&f.root)),
    );
    assert!(output.started);
    assert!(windows_label::has_root_label(&f.root));
    revoke_all_labels();
    assert!(!windows_label::has_root_label(&f.root));
}

#[test]
fn a_sandboxed_command_cannot_open_the_worker_process() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    platform::protect_worker_process();
    let f = fixture();
    let script = format!(
        "try {{ [void][System.Diagnostics.Process]::GetProcessById({}).Handle; 'opened' }} catch {{ 'denied' }}",
        std::process::id()
    );
    let output = run(
        &f,
        &plan(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            None,
        ),
    );
    assert!(stdout(&output).contains("denied"), "{output:?}");
}

fn encoded(script: &str) -> String {
    use base64::Engine;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// The job reclaims a detached grandchild of a sandboxed command on timeout,
/// as on the unrestricted path.
#[test]
fn a_timed_out_sandboxed_command_takes_its_descendants_with_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    let f = fixture();
    let marker = f.root.join("descendant-alive");
    let lp = marker.display().to_string().replace('\'', "''");
    let child = encoded(&format!(
        "while ($true) {{ Set-Content -LiteralPath '{lp}' -Value x; Start-Sleep -Milliseconds 50 }}"
    ));
    let script = encoded(&format!(
        "Start-Process -WindowStyle Hidden powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{child}'; Start-Sleep -Seconds 60"
    ));
    let mut p = plan(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-EncodedCommand", &script],
        Some(&f.root),
    );
    p.timeout_ms = 8_000;
    let output = run(&f, &p);
    assert!(
        output
            .failure
            .as_ref()
            .is_some_and(|e| e.kind == AgentErrorKind::Timeout),
        "{output:?}"
    );
    assert!(
        marker.exists(),
        "the grandchild never ran, so the test proves nothing"
    );
    // TerminateJobObject returns before the processes are gone; a write
    // already in flight may still land, so let it settle before clearing.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = std::fs::remove_file(&marker);
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    assert!(
        !marker.exists(),
        "a descendant outlived the reclaimed command"
    );
}

/// A Low integrity process cannot open the worker's threads for termination
/// or suspension (plan 6.5.3, part four).
#[test]
fn a_sandboxed_command_cannot_open_the_worker_threads() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    platform::protect_worker_process();
    let f = fixture();
    // OpenThread(THREAD_TERMINATE | THREAD_SUSPEND_RESUME) on every thread.
    let script = encoded(&format!(
        "Add-Type -Name T -Namespace W -MemberDefinition '[DllImport(\"kernel32.dll\")] public static extern System.IntPtr OpenThread(uint a, bool i, uint id);'; \
         $opened = 0; foreach ($t in [System.Diagnostics.Process]::GetProcessById({pid}).Threads) {{ if ([W.T]::OpenThread(0x0003, $false, [uint32]$t.Id) -ne [System.IntPtr]::Zero) {{ $opened++ }} }}; \
         \"opened=$opened\"",
        pid = std::process::id()
    ));
    let output = run(
        &f,
        &plan(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-EncodedCommand", &script],
            None,
        ),
    );
    let text = stdout(&output);
    // Either the thread list is unreadable (the process cannot be opened at
    // all) or no thread opens.
    assert!(
        text.contains("opened=0") || !text.contains("opened="),
        "{output:?}"
    );
}

/// A process of the same user at Medium integrity can still end the worker
/// (only Low processes are kept out).
#[test]
fn a_medium_process_of_the_user_can_still_end_a_protected_process() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !available() {
        return;
    }
    // Protect a child the way the worker protects itself, then end it from
    // this (Medium) process.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "worker::exec_sandbox::tests_windows::protected_child_entry",
            "--exact",
            "--ignored",
            "--nocapture",
            "-q",
        ])
        .env("LCXL_PROTECTED_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait for the child to report that it applied the label (libtest prints
    // its own header first).
    let reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let protected = std::io::BufRead::lines(reader)
        .map_while(Result::ok)
        .any(|line| line.contains("protected"));
    assert!(protected, "the child never reported its protection");
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &child.id().to_string()])
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
    let _ = child.wait();
}

/// Child side of [`a_medium_process_of_the_user_can_still_end_a_protected_process`].
#[test]
#[ignore = "started by a_medium_process_of_the_user_can_still_end_a_protected_process"]
fn protected_child_entry() {
    if std::env::var_os("LCXL_PROTECTED_CHILD").is_none() {
        return;
    }
    platform::protect_worker_process();
    println!("protected");
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[test]
fn a_root_still_being_prepared_is_a_retryable_refusal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime.block_on(execute_plan_cancellable(
        &plan("cmd.exe", &["/c", "echo"], None),
        ExecLaunch::Refused(SandboxError::Preparing),
        |_| {},
        None,
    ));
    let AgentOutcome::Ok(OperationOutput::Exec(output)) = outcome else {
        panic!("expected exec output");
    };
    assert!(!output.started);
    let failure = output.failure.expect("failure");
    assert_eq!(failure.kind, AgentErrorKind::HostAtCapacity);
    assert!(failure.retryable);
    assert!(failure.message.contains("sandbox=false"));
    assert_eq!(output.diagnostics[0].domain, "exec_sandbox_preparing");
}
