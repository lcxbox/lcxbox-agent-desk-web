use super::*;
use crate::exec::{ExecContainmentSnapshot, ExecIoMode};
use crate::exec_policy::{ExecLimits, fingerprint_with_io_mode};

fn directory(token: &str) -> ObjectRef {
    ObjectRef {
        token: token.into(),
        snapshot_id: "snap".into(),
        object_kind: ObjectKind::Directory,
        expires_at: String::new(),
    }
}

fn grant(id: &str) -> ExecSnapshotRoot {
    ExecSnapshotRoot::Grant {
        directory_request_id: id.into(),
        directory: directory(&format!("dir-{id}")),
    }
}

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn change(path: &str, kind: ExecFileChangeKind) -> ExecFileChange {
    let (before, after) = match kind {
        ExecFileChangeKind::Added => (None, Some(sha('b'))),
        ExecFileChangeKind::Modified => (Some(sha('a')), Some(sha('b'))),
        ExecFileChangeKind::Deleted => (Some(sha('a')), None),
    };
    ExecFileChange {
        path: path.into(),
        root: 0,
        kind,
        before_sha256: before,
        after_sha256: after,
        size_bytes: 10,
        protected: true,
    }
}

fn report(roots: &[ExecSnapshotRoot]) -> ExecFileChanges {
    ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Taken,
        recovery_id: Some(sha('c')),
        started_at_unix_ms: 1_000,
        finished_at_unix_ms: 2_000,
        roots: roots
            .iter()
            .map(|root| ExecFileChangeRoot {
                root: root.clone(),
                directory: match root {
                    ExecSnapshotRoot::Grant { directory, .. } => directory.clone(),
                    ExecSnapshotRoot::Cwd { .. } => directory("cwd-ref"),
                },
            })
            .collect(),
        changes: vec![
            change("src/a.rs", ExecFileChangeKind::Modified),
            change("new.txt", ExecFileChangeKind::Added),
            change("gone.txt", ExecFileChangeKind::Deleted),
        ],
        truncated: false,
    }
}

#[test]
fn roots_validate_shape_bounds_and_duplicates() {
    ExecSnapshotRoot::validate_all(&[
        ExecSnapshotRoot::Cwd {
            path: "/repo".into(),
        },
        grant("r1"),
    ])
    .unwrap();
    assert!(
        ExecSnapshotRoot::Cwd { path: " ".into() }
            .validate()
            .is_err()
    );
    let mut file_ref = grant("r1");
    if let ExecSnapshotRoot::Grant { directory, .. } = &mut file_ref {
        directory.object_kind = ObjectKind::File;
    }
    assert!(file_ref.validate().is_err());
    assert!(ExecSnapshotRoot::validate_all(&[grant("r1"), grant("r1")]).is_err());
    let many: Vec<_> = (0..=MAX_EXEC_SNAPSHOT_ROOTS)
        .map(|i| grant(&format!("r{i}")))
        .collect();
    assert!(ExecSnapshotRoot::validate_all(&many).is_err());
    assert!(ExecSnapshotRoot::validate_all(&many[..MAX_EXEC_SNAPSHOT_ROOTS]).is_ok());
}

#[test]
fn roots_participate_in_the_fingerprint() {
    let limits = ExecLimits {
        timeout_ms: 1_000,
        max_stdout_bytes: 10,
        max_stderr_bytes: 10,
    };
    let print = |roots: &[ExecSnapshotRoot]| {
        fingerprint_with_io_mode(
            "bash",
            &["-lc".into(), "make".into()],
            Some("/repo"),
            &limits,
            &ExecContainmentSnapshot::default(),
            ExecIoMode::NonInteractive,
            roots,
        )
    };
    let none = print(&[]);
    let cwd = print(&[ExecSnapshotRoot::Cwd {
        path: "/repo".into(),
    }]);
    let grants = print(&[grant("r1")]);
    let other_grant = print(&[grant("r2")]);
    let both = print(&[grant("r1"), grant("r2")]);
    let swapped = print(&[grant("r2"), grant("r1")]);
    let all = [&none, &cwd, &grants, &other_grant, &both, &swapped];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
    // A rootless plan keeps the fingerprint of the plain helper.
    assert_eq!(
        none,
        crate::exec_policy::fingerprint(
            "bash",
            &["-lc".into(), "make".into()],
            Some("/repo"),
            &limits,
            &ExecContainmentSnapshot::default(),
        )
    );
}

#[test]
fn file_change_report_is_checked_against_the_sealed_roots() {
    let roots = [grant("r1")];
    let valid = report(&roots);
    valid.validate_for(&roots).unwrap();
    // Wrong or reordered roots.
    assert!(valid.validate_for(&[grant("r2")]).is_err());
    assert!(valid.validate_for(&[]).is_err());
    let mut wrong_dir = valid.clone();
    wrong_dir.roots[0].directory = directory("elsewhere");
    assert!(wrong_dir.validate_for(&roots).is_err());
    // A Cwd root takes any device-issued directory reference.
    let cwd = [ExecSnapshotRoot::Cwd {
        path: "/repo".into(),
    }];
    report(&cwd).validate_for(&cwd).unwrap();
    // Paths stay inside their root.
    for path in ["/abs", "../up", "a/../b", "a\\b", "a//b", ""] {
        let mut bad = valid.clone();
        bad.changes[0].path = path.into();
        assert!(bad.validate_for(&roots).is_err(), "{path}");
    }
    // Root index, digest shape and kind/digest consistency.
    let mut bad = valid.clone();
    bad.changes[0].root = 1;
    assert!(bad.validate_for(&roots).is_err());
    let mut bad = valid.clone();
    bad.changes[1].before_sha256 = Some(sha('a'));
    assert!(bad.validate_for(&roots).is_err());
    let mut bad = valid.clone();
    bad.changes[2].before_sha256 = None;
    assert!(bad.validate_for(&roots).is_err());
    bad.changes[2].protected = false;
    bad.validate_for(&roots).unwrap();
    let mut bad = valid.clone();
    bad.changes[0].after_sha256 = Some("x".into());
    assert!(bad.validate_for(&roots).is_err());
    let mut dup = valid.clone();
    dup.changes.push(dup.changes[0].clone());
    assert!(dup.validate_for(&roots).is_err());
    // A skipped snapshot carries no record, roots or changes.
    let skipped = ExecFileChanges {
        snapshot: ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::FileLimit),
        recovery_id: None,
        roots: vec![],
        changes: vec![],
        ..valid.clone()
    };
    skipped.validate_for(&roots).unwrap();
    let mut bad = skipped;
    bad.changes = valid.changes.clone();
    assert!(bad.validate_for(&roots).is_err());
    let mut time = valid.clone();
    time.finished_at_unix_ms = 999;
    assert!(time.validate_for(&roots).is_err());
}

#[test]
fn file_change_report_is_bounded_to_32_kib() {
    let roots = [ExecSnapshotRoot::Cwd {
        path: "/repo".into(),
    }];
    let mut big = report(&roots);
    big.changes = (0..400)
        .map(|i| {
            change(
                &format!("dir/file-{i:04}.txt"),
                ExecFileChangeKind::Modified,
            )
        })
        .collect();
    assert!(serde_json::to_vec(&big).unwrap().len() > MAX_EXEC_FILE_CHANGES_BYTES);
    assert!(big.validate_for(&roots).is_err());
    big.changes.truncate(100);
    big.truncated = true;
    big.validate_for(&roots).unwrap();
}

#[test]
fn exec_output_reports_changes_only_for_rooted_plans() {
    let roots = [grant("r1")];
    let mut output = crate::ExecOutput {
        started: true,
        exit_code: Some(0),
        termination_signal: None,
        failure: None,
        diagnostics: vec![],
        streams: crate::ExecOutputStreams::Split {
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        },
        duration_ms: 1,
        redactions: vec![],
        file_changes: None,
    };
    output.validate_file_changes(&[]).unwrap();
    output.validate_file_changes(&roots).unwrap();
    output.file_changes = Some(report(&roots));
    output.validate_file_changes(&roots).unwrap();
    assert!(output.validate_file_changes(&[]).is_err());
    // JSON and wincode both carry the report.
    let json = serde_json::to_string(&output).unwrap();
    assert_eq!(
        serde_json::from_str::<crate::ExecOutput>(&json).unwrap(),
        output
    );
    let config = wincode::config::Configuration::<
        true,
        { wincode::config::PREALLOCATION_SIZE_LIMIT_DISABLED },
    >::new();
    let bytes = wincode::config::serialize(&output, config).unwrap();
    let back: crate::ExecOutput = wincode::config::deserialize(&bytes, config).unwrap();
    assert_eq!(back, output);
}

fn entry(change: &ExecFileChange) -> CommandChangeRestore {
    CommandChangeRestore::from_change(change)
}

#[test]
fn restore_action_requires_restorable_entries() {
    let roots = [grant("r1")];
    let changes = report(&roots).changes;
    let entries: Vec<_> = changes.iter().map(entry).collect();
    validate_command_restore(&sha('c'), &roots[0], &entries).unwrap();
    assert!(validate_command_restore("short", &roots[0], &entries).is_err());
    assert!(validate_command_restore(&sha('c'), &roots[0], &[]).is_err());
    let mut dup = entries.clone();
    dup.push(entries[0].clone());
    assert!(validate_command_restore(&sha('c'), &roots[0], &dup).is_err());
    let mut unhashed = entries.clone();
    unhashed[0].before_sha256 = None;
    assert!(validate_command_restore(&sha('c'), &roots[0], &unhashed).is_err());
    let action = crate::computer_use::FilePatchAction::RestoreCommandChanges {
        recovery_id: sha('c'),
        root: roots[0].clone(),
        entries: entries.clone(),
    };
    action.validate_command_restore().unwrap();
    assert_eq!(
        crate::computer_use::ComputerActionKind::File(action).required_capability(),
        crate::Capability::FilePatchConfirmed
    );
}

#[test]
fn restore_receipt_matches_the_action_and_reports_inverse_changes() {
    let roots = [grant("r1")];
    let changes = report(&roots).changes;
    let entries: Vec<_> = changes.iter().map(entry).collect();
    let inverse = |entry: &CommandChangeRestore| ExecFileChange {
        path: entry.path.clone(),
        root: 0,
        kind: entry.inverse_kind(),
        before_sha256: entry.after_sha256.clone(),
        after_sha256: entry.before_sha256.clone(),
        size_bytes: 1,
        protected: true,
    };
    let output = CommandChangesRestoreOutput {
        results: vec![
            CommandChangeRestoreResult {
                path: entries[0].path.clone(),
                outcome: CommandChangeRestoreOutcome::Restored,
            },
            CommandChangeRestoreResult {
                path: entries[1].path.clone(),
                outcome: CommandChangeRestoreOutcome::Conflict,
            },
            CommandChangeRestoreResult {
                path: entries[2].path.clone(),
                outcome: CommandChangeRestoreOutcome::Restored,
            },
        ],
        file_changes: ExecFileChanges {
            changes: vec![inverse(&entries[0]), inverse(&entries[2])],
            ..report(&roots)
        },
    };
    output.validate_for(&roots[0], &entries).unwrap();
    assert!(output.verified());
    assert_eq!(
        output.file_changes.changes[1].kind,
        ExecFileChangeKind::Added
    );
    // A conflicting file must not be reported as changed.
    let mut bad = output.clone();
    bad.file_changes.changes.push(inverse(&entries[1]));
    assert!(bad.validate_for(&roots[0], &entries).is_err());
    // Results follow the action order.
    let mut bad = output.clone();
    bad.results.swap(0, 1);
    assert!(bad.validate_for(&roots[0], &entries).is_err());
    // Nothing changed: no record.
    let untouched = CommandChangesRestoreOutput {
        results: entries
            .iter()
            .map(|entry| CommandChangeRestoreResult {
                path: entry.path.clone(),
                outcome: CommandChangeRestoreOutcome::Conflict,
            })
            .collect(),
        file_changes: ExecFileChanges {
            snapshot: ExecSnapshotOutcome::Skipped(ExecSnapshotSkipReason::StorageUnavailable),
            recovery_id: None,
            roots: vec![],
            changes: vec![],
            ..report(&roots)
        },
    };
    untouched.validate_for(&roots[0], &entries).unwrap();
    let mut unknown = output;
    unknown.results[1].outcome = CommandChangeRestoreOutcome::OutcomeUnknown;
    unknown.validate_for(&roots[0], &entries).unwrap();
    assert!(!unknown.verified());
}

#[test]
fn notice_lists_fixed_limits_and_gaps() {
    let notice = ExecSnapshotNotice::new(vec![ExecSnapshotRootLabel {
        kind: ExecSnapshotRootKind::Grant,
        path: "/repo".into(),
    }]);
    assert_eq!(notice.max_files, 20_000);
    assert_eq!(notice.scan_time_limit_ms, 10_000);
    assert_eq!(notice.max_file_bytes, 16 * 1024 * 1024);
    assert!(notice.excluded_names.iter().any(|name| name == ".git"));
    assert!(notice.unprotected.contains(&ExecSnapshotGap::OutsideRoots));
    assert_eq!(EXEC_SNAPSHOT_TIME_BUDGET_MS, 20_000);
}
