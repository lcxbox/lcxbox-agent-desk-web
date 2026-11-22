use super::super::tests::{draft_input, grant, policy_with};
use super::super::*;
use desk_agent_protocol::exec::{
    ExecIdentity, ExecSandboxAttention, ExecSandboxImplicitWritable, ExecSandboxSupport,
    ExecSandboxUnsupportedReason,
};

const STANDARD: ExecIdentity = ExecIdentity {
    elevated: false,
    system: false,
    root: false,
};
const ROOT: ExecIdentity = ExecIdentity {
    elevated: false,
    system: false,
    root: true,
};
const ADMIN: ExecIdentity = ExecIdentity {
    elevated: true,
    system: false,
    root: false,
};
const UNSUPPORTED: ExecSandboxSupport =
    ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::NotImplemented);

fn device(
    policy: &mut CommandPolicyContext,
    support: ExecSandboxSupport,
    identity: ExecIdentity,
    os: &str,
) {
    policy.exec_sandbox = DeviceExecSandbox::for_test(support, identity, os);
}

fn policy_on(
    grants: &[&str],
    support: ExecSandboxSupport,
    identity: ExecIdentity,
    os: &str,
) -> CommandPolicyContext {
    let mut policy = policy_with(grants);
    device(&mut policy, support, identity, os);
    policy
}

fn mutating(cwd: Option<&str>, sandbox: Option<bool>) -> String {
    draft_input(
        "bash",
        "make fmt",
        cwd,
        CommandIoMode::NonInteractive,
        sandbox,
    )
}

fn readonly(sandbox: Option<bool>) -> String {
    draft_input(
        "powershell",
        "Get-Service -Name Spooler",
        None,
        CommandIoMode::NonInteractive,
        sandbox,
    )
}

fn elevated(sandbox: Option<bool>) -> String {
    draft_input(
        "bash",
        "sudo docker restart web1",
        None,
        CommandIoMode::Pty,
        sandbox,
    )
}

fn resolved(confirmation: &CommandConfirmation) -> (ExecSandboxMode, ExecSandboxAttention) {
    assert_eq!(confirmation.plan.sandbox_mode, confirmation.sandbox.mode);
    assert_eq!(
        confirmation.validation_input.sandbox_mode,
        confirmation.sandbox.mode
    );
    (confirmation.sandbox.mode, confirmation.sandbox.attention)
}

const SANDBOXED: (ExecSandboxMode, ExecSandboxAttention) =
    (ExecSandboxMode::Sandboxed, ExecSandboxAttention::Sandboxed);
const PROMINENT: (ExecSandboxMode, ExecSandboxAttention) = (
    ExecSandboxMode::Unrestricted,
    ExecSandboxAttention::Prominent,
);
const NEUTRAL: (ExecSandboxMode, ExecSandboxAttention) =
    (ExecSandboxMode::Unrestricted, ExecSandboxAttention::Neutral);

#[test]
fn omitted_mutating_with_roots_is_sandboxed_on_a_supported_device() {
    // Approved directories and a cwd are both write roots.
    for (grants, cwd) in [(&["r1"][..], None), (&[][..], Some("/repo"))] {
        let confirmation = policy_on(grants, ExecSandboxSupport::Supported, STANDARD, "linux")
            .prepare(&mutating(cwd, None), 1)
            .unwrap();
        assert_eq!(resolved(&confirmation), SANDBOXED);
        assert_eq!(confirmation.plan.snapshot_roots.len(), 1);
        assert_eq!(
            confirmation.sandbox.implicit_writable,
            [
                ExecSandboxImplicitWritable::ExecTempDir,
                ExecSandboxImplicitWritable::DevShm
            ]
        );
        assert_eq!(confirmation.proposal.sandbox, None);
        assert!(!confirmation.unsandboxed());
    }
}

#[test]
fn omitted_mutating_with_roots_is_unrestricted_and_prominent_without_support() {
    for (support, identity) in [
        (UNSUPPORTED, STANDARD),
        (
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
            STANDARD,
        ),
        // A privileged identity counts as unsupported whatever was reported.
        (ExecSandboxSupport::Supported, ROOT),
        (
            ExecSandboxSupport::Supported,
            ExecIdentity {
                elevated: true,
                system: true,
                root: false,
            },
        ),
    ] {
        let confirmation = policy_on(&["r1"], support, identity, "linux")
            .prepare(&mutating(None, None), 1)
            .unwrap();
        assert_eq!(
            resolved(&confirmation),
            PROMINENT,
            "{support:?} {identity:?}"
        );
        assert!(confirmation.unsandboxed());
        assert!(confirmation.sandbox.implicit_writable.is_empty());
        // Phase-two protection still applies.
        assert_eq!(confirmation.plan.snapshot_roots.len(), 1);
        assert_eq!(
            confirmation.sandbox.displayed_identity(),
            identity.is_notable().then_some(identity)
        );
    }
}

#[test]
fn omitted_mutating_without_roots_and_readonly_templates_stay_neutral() {
    for support in [ExecSandboxSupport::Supported, UNSUPPORTED] {
        let policy = policy_on(&[], support, STANDARD, "linux");
        let rootless = policy.prepare(&mutating(None, None), 1).unwrap();
        assert_eq!(resolved(&rootless), NEUTRAL);
        assert!(rootless.plan.snapshot_roots.is_empty());
        // A read-only template has no write roots even with approvals.
        let read = policy_on(&["r1"], support, STANDARD, "linux")
            .prepare(&readonly(None), 1)
            .unwrap();
        assert_eq!(resolved(&read), NEUTRAL);
        assert!(read.plan.snapshot_roots.is_empty());
        assert!(!read.unsandboxed() && !rootless.unsandboxed());
    }
}

#[test]
fn neutral_unrestricted_cards_still_show_a_privileged_or_admin_identity() {
    for identity in [ROOT, ADMIN] {
        let confirmation = policy_on(&[], UNSUPPORTED, identity, "windows")
            .prepare(&mutating(None, None), 1)
            .unwrap();
        assert_eq!(resolved(&confirmation), NEUTRAL);
        assert_eq!(confirmation.sandbox.displayed_identity(), Some(identity));
    }
    // An administrator running a sandboxed command is not called out.
    let sandboxed = policy_on(&["r1"], ExecSandboxSupport::Supported, ADMIN, "windows")
        .prepare(&mutating(None, None), 1)
        .unwrap();
    assert_eq!(resolved(&sandboxed), SANDBOXED);
    assert_eq!(sandboxed.sandbox.displayed_identity(), None);
    assert_eq!(
        sandboxed.sandbox.implicit_writable,
        [
            ExecSandboxImplicitWritable::ExecTempDir,
            ExecSandboxImplicitWritable::LowIntegrityLocations,
            ExecSandboxImplicitWritable::OtherSandboxRoots
        ]
    );
}

#[test]
fn too_many_grants_without_cwd_are_rejected_whatever_the_sandbox_choice() {
    let many: Vec<String> = (0..=desk_agent_protocol::exec::MAX_EXEC_SNAPSHOT_ROOTS)
        .map(|i| format!("r{i:03}"))
        .collect();
    let many = many.iter().map(String::as_str).collect::<Vec<_>>();
    for support in [ExecSandboxSupport::Supported, UNSUPPORTED] {
        let policy = policy_on(&many, support, STANDARD, "linux");
        for sandbox in [None, Some(true), Some(false)] {
            let error = policy.prepare(&mutating(None, sandbox), 1).unwrap_err();
            assert!(error.message.contains("set cwd"), "{}", error.message);
            policy.prepare(&mutating(Some("/repo"), sandbox), 1).ok();
        }
        // A read-only template has no roots and is not affected.
        policy.prepare(&readonly(None), 1).unwrap();
    }
}

#[test]
fn interactive_elevation_runs_unrestricted_and_refuses_explicit_sandbox() {
    for support in [ExecSandboxSupport::Supported, UNSUPPORTED] {
        let policy = policy_on(&["r1"], support, STANDARD, "linux");
        for sandbox in [None, Some(false)] {
            let confirmation = policy.prepare(&elevated(sandbox), 1).unwrap();
            assert!(confirmation.is_elevated());
            assert_eq!(resolved(&confirmation), NEUTRAL);
            assert!(confirmation.plan.snapshot_roots.is_empty());
        }
        let error = policy.prepare(&elevated(Some(true)), 1).unwrap_err();
        assert!(
            error
                .message
                .contains("cannot elevate inside the command sandbox"),
            "{}",
            error.message
        );
    }
}

#[test]
fn explicit_true_is_sandboxed_with_roots_or_temp_only() {
    let policy = policy_on(&["r1"], ExecSandboxSupport::Supported, STANDARD, "macos");
    let rooted = policy.prepare(&mutating(None, Some(true)), 1).unwrap();
    assert_eq!(resolved(&rooted), SANDBOXED);
    assert_eq!(rooted.plan.snapshot_roots.len(), 1);
    assert_eq!(rooted.proposal.sandbox, Some(true));
    assert_eq!(
        rooted.sandbox.implicit_writable,
        [
            ExecSandboxImplicitWritable::ExecTempDir,
            ExecSandboxImplicitWritable::UserTempDir
        ]
    );
    // No write roots: only the temporary locations are writable.
    let rootless = policy_on(&[], ExecSandboxSupport::Supported, STANDARD, "macos")
        .prepare(&mutating(None, Some(true)), 1)
        .unwrap();
    assert_eq!(resolved(&rootless), SANDBOXED);
    assert!(rootless.plan.snapshot_roots.is_empty() && rootless.snapshot.is_none());
    let read = policy.prepare(&readonly(Some(true)), 1).unwrap();
    assert_eq!(resolved(&read), SANDBOXED);
    assert!(read.plan.snapshot_roots.is_empty());
    assert_eq!(read.proposal.sandbox, Some(true));
}

#[test]
fn explicit_true_on_an_unsupported_or_privileged_device_is_rejected_with_the_reason() {
    for (support, identity, os, expected) in [
        (UNSUPPORTED, STANDARD, "windows", "not available on Windows"),
        (
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
            STANDARD,
            "linux",
            "Landlock ABI 3",
        ),
        (
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
            STANDARD,
            "macos",
            "sandbox-exec",
        ),
        (ExecSandboxSupport::Supported, ROOT, "linux", "run as root"),
        (
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity),
            ExecIdentity {
                elevated: true,
                system: true,
                root: false,
            },
            "windows",
            "run as SYSTEM",
        ),
    ] {
        let policy = policy_on(&["r1"], support, identity, os);
        for command in [mutating(None, Some(true)), readonly(Some(true))] {
            let error = policy.prepare(&command, 1).unwrap_err();
            assert!(error.message.contains(expected), "{}", error.message);
            assert!(
                error.message.contains("sandbox to false"),
                "{}",
                error.message
            );
            assert!(error.safe_for_model);
        }
    }
}

#[test]
fn explicit_false_is_unrestricted_and_prominent_only_for_mutating_commands() {
    for support in [ExecSandboxSupport::Supported, UNSUPPORTED] {
        for grants in [&["r1"][..], &[][..]] {
            let policy = policy_on(grants, support, STANDARD, "linux");
            let confirmation = policy.prepare(&mutating(None, Some(false)), 1).unwrap();
            assert_eq!(resolved(&confirmation), PROMINENT);
            assert_eq!(confirmation.proposal.sandbox, Some(false));
            assert_eq!(confirmation.plan.snapshot_roots.len(), grants.len());
            let read = policy.prepare(&readonly(Some(false)), 1).unwrap();
            assert_eq!(resolved(&read), NEUTRAL);
            assert!(!read.unsandboxed());
        }
    }
}

#[test]
fn omitted_and_explicit_choices_with_the_same_resolution_seal_the_same_mode() {
    let supported = policy_on(&["r1"], ExecSandboxSupport::Supported, STANDARD, "linux");
    let omitted = supported.prepare(&mutating(None, None), 1).unwrap();
    let explicit = supported.prepare(&mutating(None, Some(true)), 1).unwrap();
    assert_eq!(omitted.plan, explicit.plan);
    assert_eq!(omitted.sandbox, explicit.sandbox);
    // The literal choice is still part of the approval binding.
    assert_ne!(
        omitted.canonical_input_digest_sha256,
        explicit.canonical_input_digest_sha256
    );
    let unsupported = policy_on(&["r1"], UNSUPPORTED, STANDARD, "linux");
    let omitted = unsupported.prepare(&mutating(None, None), 1).unwrap();
    let explicit = unsupported
        .prepare(&mutating(None, Some(false)), 1)
        .unwrap();
    assert_eq!(omitted.plan, explicit.plan);
    assert_eq!(omitted.sandbox, explicit.sandbox);
}

#[test]
fn sandbox_mode_is_fingerprinted_and_reproduced_by_the_edge() {
    let canonical = mutating(None, None);
    let sandboxed = policy_on(&["r1"], ExecSandboxSupport::Supported, STANDARD, "linux")
        .prepare(&canonical, 1)
        .unwrap();
    let unrestricted = policy_on(&["r1"], UNSUPPORTED, STANDARD, "linux")
        .prepare(&canonical, 1)
        .unwrap();
    assert_eq!(
        sandboxed.plan.snapshot_roots,
        unrestricted.plan.snapshot_roots
    );
    assert_ne!(sandboxed.plan.fingerprint, unrestricted.plan.fingerprint);
    let rebuilt = crate::exec_classify::classify_command_with_policy(
        &sandboxed.validation_input,
        &[],
        desk_agent_protocol::exec_policy::builtin_blocklist(),
        ExecAdmissionPolicy::OwnerInteractive,
    );
    assert_eq!(rebuilt.draft.as_ref(), Some(&sandboxed.plan));
    // Tampering with the frozen mode or notice breaks the confirmation.
    let mut downgraded = sandboxed.clone();
    downgraded.plan.sandbox_mode = ExecSandboxMode::Unrestricted;
    downgraded.validation_input.sandbox_mode = ExecSandboxMode::Unrestricted;
    assert!(downgraded.validate(&canonical).is_err());
    let mut relabeled = unrestricted.clone();
    relabeled.sandbox.attention = ExecSandboxAttention::Neutral;
    relabeled.validate(&canonical).unwrap();
    assert_ne!(
        relabeled.resource_scope().unwrap(),
        unrestricted.resource_scope().unwrap()
    );
    let mut inconsistent = unrestricted.clone();
    inconsistent.sandbox.attention = ExecSandboxAttention::Sandboxed;
    assert!(inconsistent.validate(&canonical).is_err());
    let json = serde_json::to_string(&sandboxed).unwrap();
    assert_eq!(
        serde_json::from_str::<CommandConfirmation>(&json).unwrap(),
        sandboxed
    );
}

#[test]
fn revalidation_fails_when_grants_support_or_identity_change() {
    let canonical = mutating(None, None);
    let before = policy_on(&["r1"], ExecSandboxSupport::Supported, STANDARD, "linux");
    let approved = before.prepare(&canonical, 1).unwrap();
    before.revalidate(&approved, &canonical, 1).unwrap();
    let changes: [fn(&mut CommandPolicyContext); 5] = [
        // A new, revoked or narrowed grant set.
        |policy| {
            policy.directory_grants = crate::file_scope::TrustedDirectoryGrants::for_test(
                "device-1",
                vec![grant("r1"), grant("r2")],
            )
        },
        |policy| {
            policy.directory_grants =
                crate::file_scope::TrustedDirectoryGrants::for_test("device-1", vec![])
        },
        |policy| device(policy, UNSUPPORTED, STANDARD, "linux"),
        // Only the identity changes (same incarnation in this fixture).
        |policy| device(policy, ExecSandboxSupport::Supported, ADMIN, "linux"),
        // A new worker incarnation.
        |policy| policy.target_session_id.push_str(":worker-2"),
    ];
    for change in changes {
        let mut after = before.clone();
        change(&mut after);
        assert!(after.revalidate(&approved, &canonical, 1).is_err());
    }
    // An identity change that leaves the mode unchanged still changes the
    // frozen notice and needs a new approval.
    let read = readonly(None);
    let approved_read = before.prepare(&read, 1).unwrap();
    let mut admin = before.clone();
    device(&mut admin, ExecSandboxSupport::Supported, ADMIN, "linux");
    assert_eq!(
        admin.prepare(&read, 1).unwrap().plan,
        approved_read.plan,
        "same plan"
    );
    assert!(admin.revalidate(&approved_read, &read, 1).is_err());
}

#[test]
fn sandboxed_paths_get_advisory_host_checks() {
    let linux = policy_on(&[], ExecSandboxSupport::Supported, STANDARD, "linux");
    // A relative cwd is rejected only when the command would be sandboxed.
    let error = linux.prepare(&mutating(Some("repo"), None), 1).unwrap_err();
    assert!(error.message.contains("absolute"), "{}", error.message);
    linux
        .prepare(&mutating(Some("repo"), Some(false)), 1)
        .unwrap();
    let mut home = linux.clone();
    home.exec_sandbox = home.exec_sandbox.clone().with_user_home("/home/alice");
    for protected in [
        "/",
        "/etc",
        "/usr/local/lib",
        "/home",
        "/home/alice",
        "/home/alice/.ssh",
        "/home/alice/.config",
        "/home/alice/project/../.ssh/keys",
    ] {
        let error = home
            .prepare(&mutating(Some(protected), None), 1)
            .unwrap_err();
        assert!(
            error
                .message
                .contains("cannot be a writable sandbox directory"),
            "{protected}: {}",
            error.message
        );
        // The refusal never suggests bypassing the sandbox for this directory.
        assert!(
            !error.message.contains("set sandbox to false"),
            "{protected}: {}",
            error.message
        );
        // Without the sandbox the worker only skips the snapshot.
        home.prepare(&mutating(Some(protected), Some(false)), 1)
            .unwrap();
    }
    home.prepare(&mutating(Some("/home/alice/project"), None), 1)
        .unwrap();
    // Approved directories are checked by their canonical path.
    let mut granted = policy_on(&["r1"], ExecSandboxSupport::Supported, STANDARD, "linux");
    granted.directory_grants = crate::file_scope::TrustedDirectoryGrants::for_test(
        "device-1",
        vec![crate::file_scope::TrustedDirectoryGrant {
            canonical_path: "/etc/nginx".into(),
            ..grant("r1")
        }],
    );
    assert!(granted.prepare(&mutating(None, None), 1).is_err());
    // Windows paths are case-insensitive and drive roots are refused.
    let windows = policy_on(&[], ExecSandboxSupport::Supported, STANDARD, "windows");
    for protected in [r"C:\", r"c:\windows\system32", r"C:\Program Files\App"] {
        assert!(
            windows
                .prepare(&mutating(Some(protected), None), 1)
                .is_err(),
            "{protected}"
        );
    }
    windows
        .prepare(&mutating(Some(r"D:\work\repo"), None), 1)
        .unwrap();
    assert!(
        windows
            .prepare(&mutating(Some("/work/repo"), None), 1)
            .is_err()
    );
}

#[test]
fn the_sandbox_argument_is_a_strict_boolean_kept_literally_by_canonicalization() {
    let schema = crate::ai_agent::ai_agent_provider_registry()
        .registered_tools()
        .into_iter()
        .find(|tool| tool.name() == COMMAND_TOOL)
        .unwrap()
        .spec
        .parameters_schema;
    let sandbox = &schema["properties"]["sandbox"];
    assert_eq!(sandbox["type"], "boolean");
    assert!(sandbox.get("default").is_none());
    assert!(sandbox.get("x-canonical-omit-default").is_none());
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        desk_agent_protocol::exec::COMMAND_DRAFT_SCHEMA_VERSION
    );
    // The model's input with its fixed protocol version filled in.
    let input = |sandbox: Option<serde_json::Value>| {
        let mut value = serde_json::json!({"shell":"bash","command":"make fmt","timeout_ms":1000});
        if let Some(sandbox) = sandbox {
            value["sandbox"] = sandbox;
        }
        crate::model_input::fill_versions(COMMAND_TOOL, &mut value);
        value
    };
    let canonical = |value: serde_json::Value| {
        crate::permission_tools::canonical_tool_permission_input_json(COMMAND_TOOL, value).unwrap()
    };
    let forms = [None, Some(true), Some(false)].map(|sandbox| {
        let value = input(sandbox.map(serde_json::Value::Bool));
        crate::model_input::validate_format(COMMAND_TOOL, &value).unwrap();
        let canonical = canonical(value);
        let proposal: CommandDraft = serde_json::from_str(&canonical).unwrap();
        assert_eq!(proposal.sandbox, sandbox);
        canonical
    });
    assert_ne!(forms[0], forms[1]);
    assert_ne!(forms[0], forms[2]);
    assert_ne!(forms[1], forms[2]);
    // The explicit choice survives prepare into the frozen proposal.
    let policy = policy_with(&["r1"]);
    assert_eq!(
        policy.prepare(&forms[1], 1).unwrap().proposal.sandbox,
        Some(true)
    );
    // null is not an omission.
    let error =
        crate::model_input::validate_format(COMMAND_TOOL, &input(Some(serde_json::Value::Null)))
            .unwrap_err();
    assert!(error.contains("omit an optional field"), "{error}");
    assert!(
        policy
            .prepare(&input(Some(serde_json::Value::Null)).to_string(), 1)
            .is_err()
    );
}

#[test]
fn readiness_feeds_the_device_sandbox_used_by_command_policy() {
    use desk_agent_protocol::computer_use::{COMPUTER_USE_SCHEMA_VERSION, ComputerUseReadiness};
    let readiness = ComputerUseReadiness {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        revision: 1,
        observed_at: "2026-11-16T00:00:00Z".into(),
        expires_at: "2026-11-16T00:01:00Z".into(),
        server_api_version: 1,
        os: "linux".into(),
        interactive_user_home: Some("/home/alice".into()),
        interactive_session_incarnation: "worker-1".into(),
        local_ceiling_revision: 1,
        unattended_command_execution: false,
        exec_sandbox: ExecSandboxSupport::Supported,
        exec_identity: ROOT,
        capabilities: vec![],
        context_references: vec![],
    };
    let device = DeviceExecSandbox::from_readiness(&readiness);
    assert_eq!(device.identity(), ROOT);
    assert_eq!(
        device.effective_support(),
        ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity)
    );
    let mut policy = policy_with(&[]);
    policy.exec_sandbox = DeviceExecSandbox::from_readiness(&ComputerUseReadiness {
        exec_identity: STANDARD,
        ..readiness
    });
    // The readiness home feeds the advisory protected-root check.
    assert!(
        policy
            .prepare(&mutating(Some("/home/alice"), None), 1)
            .is_err()
    );
    policy
        .prepare(&mutating(Some("/home/alice/repo"), None), 1)
        .unwrap();
}
