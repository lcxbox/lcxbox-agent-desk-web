// The file reference store is process-wide; each test holds its lock for the
// whole command, including the awaits of its single-threaded runtime.
#![allow(clippy::await_holding_lock)]
use super::*;
use crate::worker::agent::file_reference_store::{
    command_changes, file_store_test_lock, text_mutation::RecoveryContext,
};
use desk_agent_protocol::computer_use::FilePatchAction;
use desk_agent_protocol::exec::{
    CommandChangeRestore, CommandChangeRestoreOutcome, ExecShellKind, ExecSnapshotRoot,
};
use desk_agent_protocol::{AgentErrorKind, ExecOutput, RiskLevel};
use desk_file_recovery::{ChangeState, ScanLimits, Scope, Vault};
use desk_ipc_protocol::message::{ExecSpawnReport, FileRecoveryExecutionContext};
use std::path::Path;
use std::time::Duration;

struct Fixture {
    data: tempfile::TempDir,
    work: tempfile::TempDir,
    snapshots: ExecSnapshots,
}

impl Fixture {
    fn new() -> Self {
        Self::with(ScanLimits::default(), |_| ProtectedRoots::default())
    }

    fn with(limits: ScanLimits, protected: impl FnOnce(&Path) -> ProtectedRoots) -> Self {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let enabled =
            vault::Enabled::for_test(data.path().to_path_buf(), protected(work.path()), limits)
                .unwrap();
        Self {
            data,
            work,
            snapshots: ExecSnapshots(Availability::Enabled(Arc::new(enabled))),
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
        }
    }

    async fn run(
        &self,
        plan: &ExecPlan,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> ExecOutput {
        let outcome = self
            .snapshots
            .run(plan, Some(binding()), |started| {
                crate::worker::exec::execute_plan_cancellable(
                    plan,
                    move |report| {
                        if matches!(report, ExecSpawnReport::Started { .. }) {
                            started.store(true, Ordering::Release);
                        }
                    },
                    cancel,
                )
            })
            .await;
        match outcome {
            AgentOutcome::Ok(OperationOutput::Exec(output)) => output,
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
    let pending = enabled.prepare(&plan, Some(binding())).unwrap();
    let recovery_id = pending.recovery_id().to_owned();
    drop(pending);
    let Fixture {
        data,
        work: _work,
        snapshots,
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
        .run(&plan, Some(binding()), |_| {
            crate::worker::exec::execute_plan_cancellable(&plan, |_| {}, None)
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
    let outcome = fixture
        .snapshots
        .run(&plan, None, |started| {
            crate::worker::exec::execute_plan_cancellable(
                &plan,
                move |_| started.store(true, Ordering::Release),
                None,
            )
        })
        .await;
    let AgentOutcome::Ok(OperationOutput::Exec(output)) = outcome else {
        panic!("expected exec output")
    };
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
