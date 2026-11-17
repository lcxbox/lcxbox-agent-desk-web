// The file reference store is process-wide; each test holds its lock for the
// whole command, including the awaits of its single-threaded runtime.
#![allow(clippy::await_holding_lock)]
use super::*;
use crate::worker::agent::file_reference_store::{
    command_changes, file_store_test_lock, text_mutation::RecoveryContext,
};
use desk_agent_protocol::computer_use::FilePatchAction;
use desk_agent_protocol::exec::{
    CommandChangeRestore, CommandChangeRestoreOutcome, ExecSandboxMode, ExecShellKind,
    ExecSnapshotRoot,
};
use desk_agent_protocol::{AgentErrorKind, ExecOutput, RiskLevel};
use desk_file_recovery::{ChangeState, ScanLimits, Scope, Vault};
use desk_ipc_protocol::message::{ExecSpawnReport, FileRecoveryExecutionContext};
use std::path::Path;
use std::time::Duration;

struct Fixture {
    data: tempfile::TempDir,
    /// Below `/tmp`, outside the macOS user temporary directory that a
    /// sandboxed command may write.
    work: tempfile::TempDir,
    outside: tempfile::TempDir,
    snapshots: ExecSnapshots,
    /// The worker's sandbox, with the data directory as its private
    /// directory, as in a running worker.
    sandbox: Arc<ExecSandbox>,
}

fn scratch_in_tmp(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .unwrap()
}

impl Fixture {
    fn new() -> Self {
        Self::with(ScanLimits::default(), |_| ProtectedRoots::default())
    }

    fn with(limits: ScanLimits, protected: impl FnOnce(&Path) -> ProtectedRoots) -> Self {
        let data = tempfile::tempdir().unwrap();
        let work = scratch_in_tmp("exec-snapshot-work");
        let outside = scratch_in_tmp("exec-snapshot-outside");
        let protected = protected(work.path());
        let enabled =
            vault::Enabled::for_test(data.path().to_path_buf(), protected.clone(), limits).unwrap();
        let sandbox = ExecSandbox::start(
            Some(data.path().to_path_buf()),
            protected,
            vec![data.path().to_path_buf()],
        );
        Self {
            data,
            work,
            outside,
            snapshots: ExecSnapshots(Availability::Enabled(Arc::new(enabled))),
            sandbox: Arc::new(sandbox),
        }
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.work.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.work.path().join(path)).ok()
    }

    fn scope() -> Scope {
        Scope {
            authority: "test".into(),
            device: "device".into(),
            owner: "owner".into(),
            os_user: crate::file_recovery_service::platform_user::current().unwrap(),
        }
    }

    fn context(&self, operation: &str) -> RecoveryContext {
        RecoveryContext {
            _test_data: None,
            execution_epoch: 0,
            data_root: self.data.path().to_path_buf(),
            quota: None,
            scope: Self::scope(),
            conversation_id: "conversation".into(),
            operation_id: operation.into(),
            generation: operation.into(),
        }
    }

    fn sandboxed_plan(&self, script: &str) -> ExecPlan {
        ExecPlan {
            sandbox_mode: ExecSandboxMode::Sandboxed,
            ..self.plan(script, 10_000)
        }
    }

    fn plan(&self, script: &str, timeout_ms: u32) -> ExecPlan {
        let cwd = self.work.path().to_str().unwrap().to_owned();
        ExecPlan {
            exec_request_id: desk_agent_protocol::exec::ExecRequestId("exec".into()),
            execution_generation: format!("gen-{}", uuid::Uuid::new_v4()),
            program: "sh".into(),
            argv: vec!["-c".into(), script.into()],
            cwd: Some(cwd.clone()),
            io_mode: desk_agent_protocol::exec::ExecIoMode::NonInteractive,
            shell: ExecShellKind::Native,
            risk: RiskLevel::Low,
            execution_basis: desk_agent_protocol::exec::ExecExecutionBasis::Template,
            template_id: "test".into(),
            approval_id: desk_agent_protocol::exec::ApprovalId("approval".into()),
            authorization: Default::default(),
            fingerprint: "fp".into(),
            timeout_ms,
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
            containment: Default::default(),
            snapshot_roots: vec![ExecSnapshotRoot::Cwd { path: cwd }],
            sandbox_mode: desk_agent_protocol::exec::ExecSandboxMode::Unrestricted,
        }
    }

    async fn run(
        &self,
        plan: &ExecPlan,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> ExecOutput {
        self.run_with(plan, Some(binding()), cancel).await.0
    }

    /// Run through the worker's snapshot and sandbox path; also returns the
    /// spawn report.
    async fn run_with(
        &self,
        plan: &ExecPlan,
        binding: Option<ExecFileRecoveryBinding>,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> (ExecOutput, ExecSpawnReport) {
        let report = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&report);
        let outcome = self
            .snapshots
            .run(plan, binding, &self.sandbox, |started, launch| {
                crate::worker::exec::execute_plan_cancellable(
                    plan,
                    launch,
                    move |report| {
                        if matches!(report, ExecSpawnReport::Started { .. }) {
                            started.store(true, Ordering::Release);
                        }
                        *sink.lock().unwrap() = Some(report);
                    },
                    cancel,
                )
            })
            .await;
        let report = report.lock().unwrap().take().expect("a spawn report");
        match outcome {
            AgentOutcome::Ok(OperationOutput::Exec(output)) => (output, report),
            other => panic!("expected exec output, got {other:?}"),
        }
    }

    fn restore(
        &self,
        report: &ExecFileChanges,
        operation: &str,
    ) -> desk_agent_protocol::exec::CommandChangesRestoreOutput {
        let (action, target) = restore_action(report);
        let output =
            command_changes::restore(&target, &action, &self.context(operation), || Ok(()))
                .unwrap();
        let FilePatchAction::RestoreCommandChanges { root, entries, .. } = &action else {
            unreachable!()
        };
        output.validate_for(root, entries).unwrap();
        output
    }
}

fn binding() -> ExecFileRecoveryBinding {
    ExecFileRecoveryBinding {
        recovery: FileRecoveryExecutionContext {
            execution_epoch: 0,
            os_user: crate::file_recovery_service::platform_user::current().unwrap(),
            authority: "test".into(),
            conversation_id: "conversation".into(),
        },
        device: "device".into(),
        owner: "owner".into(),
    }
}

fn restore_action(
    report: &ExecFileChanges,
) -> (
    FilePatchAction,
    desk_agent_protocol::computer_use::ObjectRef,
) {
    let root = &report.roots[0];
    (
        FilePatchAction::RestoreCommandChanges {
            recovery_id: report.recovery_id.clone().unwrap(),
            root: root.root.clone(),
            entries: report
                .protected_entries(0)
                .map(CommandChangeRestore::from_change)
                .collect(),
        },
        root.directory.clone(),
    )
}

fn listed(report: &ExecFileChanges) -> Vec<(&str, ExecFileChangeKind, bool)> {
    report
        .changes
        .iter()
        .map(|change| (change.path.as_str(), change.kind, change.protected))
        .collect()
}

fn changes_of(output: &ExecOutput, plan: &ExecPlan) -> ExecFileChanges {
    output.validate_file_changes(&plan.snapshot_roots).unwrap();
    output.file_changes.clone().expect("a change report")
}

#[tokio::test]
async fn a_formatting_command_reports_added_modified_and_deleted_files() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    fixture.write("b.txt", "b 1\n");
    fixture.write("sub/dir/c.txt", "c 1\n");
    fixture.write("keep.txt", "keep\n");
    let plan = fixture.plan(
        "printf 'a 2\\n' > a.txt && rm b.txt && rm -r sub && printf 'd 2\\n' > d.txt",
        10_000,
    );
    let output = fixture.run(&plan, None).await;
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    let report = changes_of(&output, &plan);
    assert_eq!(report.snapshot, ExecSnapshotOutcome::Taken);
    assert!(!report.truncated);
    assert_eq!(
        listed(&report),
        vec![
            ("a.txt", ExecFileChangeKind::Modified, true),
            ("b.txt", ExecFileChangeKind::Deleted, true),
            ("d.txt", ExecFileChangeKind::Added, true),
            ("sub/dir/c.txt", ExecFileChangeKind::Deleted, true),
        ]
    );
    // The working directory is reported with a durable reference a restore
    // can target, and the snapshot window covers the command.
    assert_eq!(report.roots[0].root, plan.snapshot_roots[0]);
    assert!(report.started_at_unix_ms <= report.finished_at_unix_ms);
}

#[tokio::test]
async fn excluded_and_ignored_paths_are_not_reported() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write(".gitignore", "*.log\n");
    fixture.write("app.log", "old\n");
    fixture.write("node_modules/pkg/index.js", "old\n");
    fixture.write("src/main.rs", "fn main() {}\n");
    let plan = fixture.plan(
        "printf 'new\\n' > app.log && printf 'new\\n' > node_modules/pkg/index.js \
         && printf '*.log\\n*.tmp\\n' > .gitignore && printf 'x\\n' > src/new.tmp",
        10_000,
    );
    let output = fixture.run(&plan, None).await;
    let report = changes_of(&output, &plan);
    // The rules are frozen before the run: the edited `.gitignore` is a
    // change itself, and the new `*.tmp` rule does not hide `src/new.tmp`.
    assert_eq!(
        listed(&report),
        vec![
            (".gitignore", ExecFileChangeKind::Modified, true),
            ("src/new.tmp", ExecFileChangeKind::Added, true),
        ]
    );
}

#[tokio::test]
async fn timed_out_and_cancelled_commands_still_get_the_final_scan() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    let plan = fixture.plan("printf 'a 2\\n' > a.txt; sleep 20", 1_000);
    let output = fixture.run(&plan, None).await;
    assert_eq!(
        output.failure.as_ref().unwrap().kind,
        AgentErrorKind::Timeout
    );
    assert_eq!(
        listed(&changes_of(&output, &plan)),
        vec![("a.txt", ExecFileChangeKind::Modified, true)]
    );

    let plan = fixture.plan("printf 'a 3\\n' > a.txt; sleep 20", 30_000);
    let cancel = crate::worker::exec::ExecCancel::new();
    let stop = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        stop.cancel();
    });
    let output = fixture.run(&plan, Some(cancel.subscribe())).await;
    canceller.await.unwrap();
    assert_eq!(
        output.failure.as_ref().unwrap().kind,
        AgentErrorKind::Cancelled
    );
    assert_eq!(
        listed(&changes_of(&output, &plan)),
        vec![("a.txt", ExecFileChangeKind::Modified, true)]
    );
}

#[tokio::test]
async fn a_command_that_never_started_reports_nothing_and_aborts_its_snapshot() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    let mut plan = fixture.plan("", 10_000);
    plan.program = "lcxl-definitely-not-a-real-binary".into();
    plan.argv.clear();
    let output = fixture.run(&plan, None).await;
    assert!(!output.started);
    assert_eq!(output.file_changes, None);
    let vault = Vault::open(fixture.data.path()).unwrap();
    let records = vault
        .lock()
        .unwrap()
        .list(&Fixture::scope(), Some("conversation"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].change, ChangeState::Aborted);
}

#[tokio::test]
async fn a_crashed_executor_leaves_an_unknown_outcome_that_cannot_be_restored() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    let plan = fixture.plan("true", 10_000);
    let Availability::Enabled(enabled) = &fixture.snapshots.0 else {
        unreachable!()
    };
    // The worker dies after the pre-run scan: neither finish nor abandon
    // runs, and its executor lock goes away with it.
    let pending = enabled.prepare(&plan, Some(binding()), None).unwrap();
    let recovery_id = pending.recovery_id().to_owned();
    drop(pending);
    let Fixture {
        data,
        work: _work,
        outside: _outside,
        snapshots,
        sandbox: _sandbox,
    } = fixture;
    drop(snapshots);
    let vault = Vault::open(data.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    locked.recover_interrupted(now_ms()).unwrap();
    let scope = Fixture::scope();
    assert_eq!(
        locked.record(&scope, &recovery_id).unwrap().change,
        ChangeState::OutcomeUnknown
    );
    let refused = locked
        .restore_targets(&scope, "conversation", &recovery_id, None, now_ms())
        .unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn limits_and_protected_roots_skip_the_snapshot_but_not_the_command() {
    let _store = file_store_test_lock();
    let fixture = Fixture::with(
        ScanLimits {
            max_files: 1,
            max_duration: Duration::from_secs(10),
        },
        |_| ProtectedRoots::default(),
    );
    fixture.write("a.txt", "a\n");
    fixture.write("b.txt", "b\n");
    let plan = fixture.plan("printf 'c\\n' > c.txt", 10_000);
    let output = fixture.run(&plan, None).await;
    let report = changes_of(&output, &plan);
    assert_eq!(
        report.snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::FileLimit)
    );
    assert_eq!(fixture.read("c.txt").as_deref(), Some("c\n"));

    // A root that holds product data is refused after canonicalization.
    let fixture = Fixture::with(ScanLimits::default(), |work| {
        ProtectedRoots::new(None, Vec::new(), [work.join("data")])
    });
    let plan = fixture.plan("printf 'c\\n' > c.txt", 10_000);
    let output = fixture.run(&plan, None).await;
    assert_eq!(
        changes_of(&output, &plan).snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::ProtectedRoot)
    );
    assert_eq!(fixture.read("c.txt").as_deref(), Some("c\n"));
}

#[tokio::test]
async fn system_identity_and_root_pty_take_no_snapshot() {
    let fixture = Fixture::new();
    let snapshots = ExecSnapshots(Availability::SystemIdentity);
    let plan = fixture.plan("printf 'x\\n' > x.txt", 10_000);
    let outcome = snapshots
        .run(&plan, Some(binding()), &fixture.sandbox, |_, launch| {
            crate::worker::exec::execute_plan_cancellable(&plan, launch, |_| {}, None)
        })
        .await;
    let AgentOutcome::Ok(OperationOutput::Exec(output)) = outcome else {
        panic!("expected exec output")
    };
    assert_eq!(
        changes_of(&output, &plan).snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::SystemIdentity)
    );
    assert_eq!(fixture.read("x.txt").as_deref(), Some("x\n"));

    let root_pty = root_pty_report(&plan, now_ms()).unwrap();
    root_pty.validate_for(&plan.snapshot_roots).unwrap();
    assert_eq!(
        root_pty.snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::SystemIdentity)
    );
    let mut without_roots = plan;
    without_roots.snapshot_roots.clear();
    assert_eq!(root_pty_report(&without_roots, now_ms()), None);
}

#[tokio::test]
async fn a_plan_without_roots_or_binding_is_handled_without_storage() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    let mut plan = fixture.plan("true", 10_000);
    plan.snapshot_roots.clear();
    assert_eq!(fixture.run(&plan, None).await.file_changes, None);

    let plan = fixture.plan("true", 10_000);
    let (output, _) = fixture.run_with(&plan, None, None).await;
    assert_eq!(
        changes_of(&output, &plan).snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::StorageUnavailable)
    );
}

#[tokio::test]
async fn restore_recreates_deleted_parents_and_can_itself_be_undone() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    fixture.write("b.txt", "b 1\n");
    fixture.write("sub/dir/c.txt", "c 1\n");
    let plan = fixture.plan(
        "printf 'a 2\\n' > a.txt && rm b.txt && rm -r sub && printf 'd 2\\n' > d.txt",
        10_000,
    );
    let command = changes_of(&fixture.run(&plan, None).await, &plan);

    let restored = fixture.restore(&command, "restore-1");
    assert!(
        restored
            .results
            .iter()
            .all(|result| result.outcome == CommandChangeRestoreOutcome::Restored),
        "{restored:?}"
    );
    assert!(restored.verified());
    assert_eq!(fixture.read("a.txt").as_deref(), Some("a 1\n"));
    assert_eq!(fixture.read("b.txt").as_deref(), Some("b 1\n"));
    assert_eq!(fixture.read("sub/dir/c.txt").as_deref(), Some("c 1\n"));
    assert_eq!(fixture.read("d.txt"), None);
    assert_eq!(
        listed(&restored.file_changes),
        vec![
            ("a.txt", ExecFileChangeKind::Modified, true),
            ("b.txt", ExecFileChangeKind::Added, true),
            ("d.txt", ExecFileChangeKind::Deleted, true),
            ("sub/dir/c.txt", ExecFileChangeKind::Added, true),
        ]
    );

    // Undo the restore: the command's result comes back.
    let undone = fixture.restore(&restored.file_changes, "restore-2");
    assert!(undone.verified());
    assert_eq!(fixture.read("a.txt").as_deref(), Some("a 2\n"));
    assert_eq!(fixture.read("b.txt"), None);
    assert_eq!(fixture.read("sub/dir/c.txt"), None);
    assert_eq!(fixture.read("d.txt").as_deref(), Some("d 2\n"));
}

#[tokio::test]
async fn restore_skips_files_changed_after_the_command_as_conflicts() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    fixture.write("b.txt", "b 1\n");
    let plan = fixture.plan("printf 'a 2\\n' > a.txt && printf 'b 2\\n' > b.txt", 10_000);
    let command = changes_of(&fixture.run(&plan, None).await, &plan);
    fixture.write("b.txt", "edited by the user\n");

    let restored = fixture.restore(&command, "restore-1");
    let outcomes: Vec<_> = restored
        .results
        .iter()
        .map(|result| (result.path.as_str(), result.outcome))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ("a.txt", CommandChangeRestoreOutcome::Restored),
            ("b.txt", CommandChangeRestoreOutcome::Conflict),
        ]
    );
    assert_eq!(fixture.read("a.txt").as_deref(), Some("a 1\n"));
    assert_eq!(
        fixture.read("b.txt").as_deref(),
        Some("edited by the user\n")
    );
    assert_eq!(
        listed(&restored.file_changes),
        vec![("a.txt", ExecFileChangeKind::Modified, true)]
    );

    // Nothing left to restore: every file conflicts, no record is written.
    let again = fixture.restore(&command, "restore-2");
    assert_eq!(
        again.file_changes.snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::StorageUnavailable)
    );
    assert!(
        again
            .results
            .iter()
            .all(|result| result.outcome == CommandChangeRestoreOutcome::Conflict)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restore_never_writes_through_a_link_planted_in_the_path() {
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fixture.write("sub/c.txt", "c 1\n");
    let plan = fixture.plan("rm -r sub", 10_000);
    let command = changes_of(&fixture.run(&plan, None).await, &plan);
    std::os::unix::fs::symlink(outside.path(), fixture.work.path().join("sub")).unwrap();

    let restored = fixture.restore(&command, "restore-1");
    assert_ne!(
        restored.results[0].outcome,
        CommandChangeRestoreOutcome::Restored
    );
    assert!(!outside.path().join("c.txt").exists());
}

#[test]
fn the_report_is_bounded_and_marks_truncation() {
    let roots = vec![ExecFileChangeRoot {
        root: ExecSnapshotRoot::Cwd {
            path: "/work".into(),
        },
        directory: desk_agent_protocol::computer_use::ObjectRef {
            token: "token".into(),
            snapshot_id: "snapshot".into(),
            object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
            expires_at: String::new(),
        },
    }];
    let sha = "a".repeat(64);
    let change = |index: u64, path: String| desk_file_recovery::FileChange {
        index,
        root: 0,
        path,
        kind: desk_file_recovery::ChangeKind::Added,
        before_sha256: None,
        after_sha256: Some(sha.clone()),
        size: 1,
        protected: true,
    };
    let many: Vec<_> = (0..5_000)
        .map(|index| change(index, format!("src/generated/module_{index:05}.rs")))
        .collect();
    let report = taken("b".repeat(64), 1, 2, roots.clone(), many);
    assert!(report.truncated);
    assert!(!report.changes.is_empty());
    assert!(serde_json::to_vec(&report).unwrap().len() <= MAX_EXEC_FILE_CHANGES_BYTES);
    report.validate_for(&[roots[0].root.clone()]).unwrap();

    // A path the protocol cannot carry is dropped, never sent.
    let report = taken(
        "b".repeat(64),
        1,
        2,
        roots.clone(),
        vec![change(0, "ok.txt".into()), change(1, "bad\\name".into())],
    );
    assert!(report.truncated);
    assert_eq!(report.changes.len(), 1);
    report.validate_for(&[roots[0].root.clone()]).unwrap();

    let report = taken(
        "b".repeat(64),
        1,
        2,
        roots.clone(),
        vec![change(0, "ok.txt".into())],
    );
    assert!(!report.truncated);
}

/// Whether this host can run the command sandbox; Linux CI kernels below
/// Landlock ABI 3 and root runners skip the sandbox behaviour tests.
fn sandbox_available() -> bool {
    use desk_agent_protocol::exec::ExecSandboxSupport;
    match crate::worker::exec_sandbox::sandbox_support() {
        ExecSandboxSupport::Supported => true,
        other => {
            assert!(
                !cfg!(target_os = "macos") || unsafe { libc::geteuid() } == 0,
                "sandbox-exec must work on macOS: {other:?}"
            );
            eprintln!("command sandbox unavailable here, skipping: {other:?}");
            false
        }
    }
}

fn stdout_of(output: &ExecOutput) -> &str {
    match &output.streams {
        desk_agent_protocol::ExecOutputStreams::Split { stdout, .. } => stdout,
        other => panic!("expected split streams, got {other:?}"),
    }
}

fn assert_refused(output: &ExecOutput, report: &ExecSpawnReport, needle: &str) {
    assert!(!output.started, "{output:?}");
    assert_eq!(output.file_changes, None);
    let failure = output.failure.as_ref().expect("a refusal");
    assert_eq!(failure.kind, AgentErrorKind::UnsupportedCapability);
    assert!(failure.message.contains(needle), "{failure:?}");
    assert!(
        matches!(report, ExecSpawnReport::Failed { reason } if reason.contains(needle)),
        "{report:?}"
    );
}

#[tokio::test]
async fn a_sandboxed_command_writes_its_root_and_temp_only_and_is_snapshotted() {
    if !sandbox_available() {
        return;
    }
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    fixture.write("a.txt", "a 1\n");
    let outside = fixture.outside.path().join("escape.txt");
    let script = format!(
        "printf 'a 2\\n' > a.txt && printf 'n\\n' > new.txt && \
         (printf x > '{}' 2>/dev/null && echo escaped || echo denied) && \
         printf t > \"$TMPDIR/t\" && echo \"tmp=$TMPDIR\"",
        outside.display()
    );
    let plan = fixture.sandboxed_plan(&script);
    let (output, report) = fixture.run_with(&plan, Some(binding()), None).await;
    assert!(matches!(report, ExecSpawnReport::Started { .. }));
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("denied") && !stdout.contains("escaped"),
        "{stdout}"
    );
    assert!(!outside.exists(), "a write outside the root must fail");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("a 2\n"));
    assert_eq!(fixture.read("new.txt").as_deref(), Some("n\n"));

    // The temporary directory is private to this execution, below the
    // worker's data directory, and removed afterwards.
    let temp = stdout
        .lines()
        .find_map(|line| line.strip_prefix("tmp="))
        .map(std::path::PathBuf::from)
        .expect("TMPDIR is set");
    let private = std::fs::canonicalize(fixture.data.path()).unwrap();
    assert!(temp.starts_with(private.join("exec-tmp")), "{temp:?}");
    assert!(!temp.exists(), "the temporary directory is removed");

    // The snapshot covers exactly the write root.
    let changes = changes_of(&output, &plan);
    assert_eq!(changes.snapshot, ExecSnapshotOutcome::Taken);
    assert_eq!(changes.roots.len(), 1);
    assert_eq!(changes.roots[0].root, plan.snapshot_roots[0]);
    assert_eq!(
        listed(&changes),
        vec![
            ("a.txt", ExecFileChangeKind::Modified, true),
            ("new.txt", ExecFileChangeKind::Added, true),
        ]
    );
}

#[tokio::test]
async fn a_snapshot_problem_does_not_lift_the_sandbox() {
    if !sandbox_available() {
        return;
    }
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    let outside = fixture.outside.path().join("escape.txt");
    let plan = fixture.sandboxed_plan(&format!(
        "printf 'n\\n' > new.txt; printf x > '{}'",
        outside.display()
    ));
    // Without a recovery binding the snapshot is skipped; the command still
    // runs in the sandbox.
    let (output, _) = fixture.run_with(&plan, None, None).await;
    assert!(output.started);
    assert_ne!(output.exit_code, Some(0), "{output:?}");
    assert!(!outside.exists());
    assert_eq!(fixture.read("new.txt").as_deref(), Some("n\n"));
    assert_eq!(
        changes_of(&output, &plan).snapshot,
        ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::StorageUnavailable)
    );
}

#[tokio::test]
async fn forbidden_or_unopenable_write_roots_refuse_the_command() {
    if !sandbox_available() {
        return;
    }
    let _store = file_store_test_lock();
    // A write root holding product data.
    let fixture = Fixture::with(ScanLimits::default(), |work| {
        ProtectedRoots::new(None, Vec::new(), [work.join("data")])
    });
    let plan = fixture.sandboxed_plan("printf 'c\\n' > c.txt");
    let (output, report) = fixture.run_with(&plan, Some(binding()), None).await;
    assert_refused(&output, &report, "cannot be a writable directory");
    assert_eq!(fixture.read("c.txt"), None);

    // A working directory that does not exist.
    let fixture = Fixture::new();
    let mut plan = fixture.sandboxed_plan("printf 'c\\n' > c.txt");
    let missing = fixture.work.path().join("missing");
    plan.cwd = Some(missing.to_str().unwrap().to_owned());
    plan.snapshot_roots = vec![ExecSnapshotRoot::Cwd {
        path: missing.to_str().unwrap().to_owned(),
    }];
    let (output, report) = fixture.run_with(&plan, Some(binding()), None).await;
    assert_refused(&output, &report, "write root is unavailable");

    // A granted directory replaced after it was granted.
    let fixture = Fixture::new();
    let granted = fixture.work.path().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let reference = crate::worker::agent::file_reference_store::issue(&granted).unwrap();
    std::fs::rename(&granted, fixture.work.path().join("moved")).unwrap();
    std::fs::create_dir(&granted).unwrap();
    let mut plan = fixture.sandboxed_plan("printf 'c\\n' > granted/c.txt");
    plan.snapshot_roots = vec![ExecSnapshotRoot::Grant {
        directory_request_id: "grant".into(),
        directory: reference,
    }];
    let (output, report) = fixture.run_with(&plan, Some(binding()), None).await;
    assert_refused(&output, &report, "write root is unavailable");
    assert!(!granted.join("c.txt").exists());
}

#[tokio::test]
async fn a_worker_without_sandbox_support_refuses_sandboxed_plans() {
    let _store = file_store_test_lock();
    let mut fixture = Fixture::new();
    fixture.sandbox = Arc::new(ExecSandbox::start(
        None,
        ProtectedRoots::default(),
        Vec::new(),
    ));
    let plan = fixture.sandboxed_plan("printf 'c\\n' > c.txt");
    let (output, report) = fixture.run_with(&plan, Some(binding()), None).await;
    assert_refused(&output, &report, "cannot sandbox commands");
    assert_eq!(fixture.read("c.txt"), None);

    // An unrestricted plan on the same worker runs as before.
    let plan = fixture.plan("printf 'c\\n' > c.txt", 10_000);
    let output = fixture.run(&plan, None).await;
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    assert_eq!(fixture.read("c.txt").as_deref(), Some("c\n"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_setuid_program_refused_in_the_sandbox_carries_the_hint() {
    if !sandbox_available() {
        return;
    }
    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    let mut plan = fixture.sandboxed_plan("/bin/ps -p $$");
    plan.snapshot_roots.clear();
    let (output, _) = fixture.run_with(&plan, Some(binding()), None).await;
    assert_eq!(output.exit_code, Some(126), "{output:?}");
    assert!(
        output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.domain == "exec_sandbox"
                && diagnostic.message.contains("sandbox=false")),
        "{output:?}"
    );

    // The same failure outside the sandbox carries no hint.
    let mut plan = fixture.plan("exit 126", 10_000);
    plan.snapshot_roots.clear();
    let output = fixture.run(&plan, None).await;
    assert!(
        output
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.domain != "exec_sandbox")
    );
}

/// On macOS a sandboxed command cannot read the content of this product's
/// configuration and data directories (credentials such as the
/// `config.toml` tokens), while its own temporary directory below the
/// worker's data directory stays usable. The real worker passes every
/// default configuration directory.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn product_configuration_and_data_are_unreadable_in_the_sandbox() {
    if !sandbox_available() {
        return;
    }
    let defaults = crate::worker::exec_sandbox::read_protected_directories(Vec::new());
    let user = desk_utils::host_data_paths::HostDataPaths::resolve(
        desk_utils::host_data_paths::HostDataScope::User,
        None,
    )
    .unwrap();
    assert!(defaults.contains(&user.config_file().parent().unwrap().to_path_buf()));
    assert!(defaults.contains(&user.data_root().to_path_buf()));

    let _store = file_store_test_lock();
    let fixture = Fixture::new();
    let secret = fixture.data.path().join("config.toml");
    std::fs::write(&secret, "tauri_ipc_token = \"secret\"\n").unwrap();
    let mut plan = fixture.sandboxed_plan(&format!(
        "(cat '{}' 2>/dev/null && echo leaked || echo unreadable) && \
         printf t > \"$TMPDIR/t\" && cat \"$TMPDIR/t\"",
        secret.display()
    ));
    plan.snapshot_roots.clear();
    let output = fixture.run(&plan, None).await;
    let stdout = stdout_of(&output);
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    assert!(stdout.contains("unreadable"), "{stdout}");
    assert!(
        !stdout.contains("secret") && !stdout.contains("leaked"),
        "{stdout}"
    );
    assert!(stdout.ends_with('t') || stdout.ends_with("t\n"), "{stdout}");
}
