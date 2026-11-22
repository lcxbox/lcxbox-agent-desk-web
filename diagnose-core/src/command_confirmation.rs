//! Server-authored command review and immutable execution binding.

use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::command_blocklist::BlocklistRule;
use desk_agent_protocol::command_template::SyncedCommandTemplate;
use desk_agent_protocol::exec::{
    COMMAND_PLAN_SCHEMA_VERSION, CommandDraft, CommandIoMode, ExecDecision, ExecEffect,
    ExecPlanDraft, ExecSandboxMode, ExecSandboxNotice, ExecSnapshotNotice, ExecSnapshotRoot,
    ExecSnapshotRootKind, ExecSnapshotRootLabel, MAX_EXEC_SNAPSHOT_ROOTS,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ExecInput, ExecTarget, ExecutionMode, RiskLevel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod sandbox;
pub use sandbox::{DeviceExecSandbox, identity_name};

pub const COMMAND_TOOL: &str = crate::exec_tools::EXEC_TOOL_NAME;

/// Trusted, per-request policy. Never deserialize this from model arguments.
#[derive(Debug, Clone)]
pub struct CommandPolicyContext {
    pub actor_id: String,
    pub target_device_id: String,
    /// The selected host connection/session identity, not the controller connection.
    pub target_session_id: String,
    pub policy_revision: i64,
    pub admission_policy: ExecAdmissionPolicy,
    pub execution_mode: ExecutionMode,
    pub max_risk: RiskLevel,
    pub available_shells: Vec<String>,
    pub max_runtime_ms: u32,
    pub operator_templates: Vec<SyncedCommandTemplate>,
    pub effective_blocklist: Vec<BlocklistRule>,
    /// Stable policy axes; heartbeat timestamps are deliberately excluded.
    pub policy_version: String,
    /// The target host reports that interactive (PTY) execution is enabled.
    pub exec_pty: bool,
    /// The target host reports that interactive elevation (`sudo`/`doas`
    /// inside a PTY) is enabled.
    pub exec_pty_elevation: bool,
    /// The session's current directory approvals on the target device, from
    /// [`crate::file_scope::TrustedDirectoryGrants::from_session`]. A caller
    /// without an authoritative session cannot build a policy.
    pub directory_grants: crate::file_scope::TrustedDirectoryGrants,
    /// The target worker's sandbox support and execution identity, from
    /// [`DeviceExecSandbox::from_readiness`] on its current readiness.
    pub exec_sandbox: DeviceExecSandbox,
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_policy() -> CommandPolicyContext {
    CommandPolicyContext {
        actor_id: "1".into(),
        target_device_id: "device-1".into(),
        target_session_id: "host:session-1".into(),
        policy_revision: 1,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: ExecutionMode::ConfirmEachAction,
        max_risk: RiskLevel::Critical,
        available_shells: vec!["bash".into(), "powershell".into()],
        max_runtime_ms: 10_000,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: "test:1".into(),
        exec_pty: true,
        exec_pty_elevation: true,
        directory_grants: crate::file_scope::TrustedDirectoryGrants::for_test("device-1", vec![]),
        exec_sandbox: DeviceExecSandbox::for_test(
            desk_agent_protocol::exec::ExecSandboxSupport::Supported,
            desk_agent_protocol::exec::ExecIdentity {
                elevated: false,
                system: false,
                root: false,
            },
            "linux",
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use desk_agent_protocol::exec::ExecIoMode;
    fn input(command: &str) -> String {
        input_with(command, CommandIoMode::NonInteractive)
    }

    pub(super) fn input_with(command: &str, io_mode: CommandIoMode) -> String {
        draft_input("bash", command, None, io_mode, None)
    }

    pub(super) fn shell_input(shell: &str, command: &str, cwd: Option<&str>) -> String {
        draft_input(shell, command, cwd, CommandIoMode::NonInteractive, None)
    }

    pub(super) fn draft_input(
        shell: &str,
        command: &str,
        cwd: Option<&str>,
        io_mode: CommandIoMode,
        sandbox: Option<bool>,
    ) -> String {
        serde_json::to_string(&CommandDraft {
            schema_version: desk_agent_protocol::exec::COMMAND_DRAFT_SCHEMA_VERSION,
            shell: shell.into(),
            command: command.into(),
            cwd: cwd.map(Into::into),
            timeout_ms: 30_000,
            io_mode,
            sandbox,
        })
        .unwrap()
    }

    pub(super) fn grant(id: &str) -> crate::file_scope::TrustedDirectoryGrant {
        crate::file_scope::TrustedDirectoryGrant {
            directory_request_id: id.into(),
            canonical_path: format!("/work/{id}"),
            directory: desk_agent_protocol::computer_use::ObjectRef {
                token: format!("ref-{id}"),
                snapshot_id: "gen".into(),
                object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
                expires_at: String::new(),
            },
        }
    }

    pub(super) fn policy_with(grants: &[&str]) -> CommandPolicyContext {
        let mut policy = test_policy();
        policy.directory_grants = crate::file_scope::TrustedDirectoryGrants::for_test(
            "device-1",
            grants.iter().map(|id| grant(id)).collect(),
        );
        policy
    }

    #[test]
    fn only_mutating_commands_get_snapshot_roots() {
        let policy = policy_with(&["r1"]);
        // A read-only template has no roots, even with approved directories.
        let read = shell_input("powershell", "Get-Service -Name Spooler", None);
        let sealed = policy.prepare(&read, 1).unwrap();
        assert!(sealed.plan.snapshot_roots.is_empty());
        assert!(sealed.validation_input.snapshot_roots.is_empty());
        assert!(sealed.snapshot.is_none());
        // A mutating template and an off-template command are snapshotted.
        for (shell, command) in [
            ("powershell", "Restart-Service -Name Spooler"),
            ("bash", "make fmt"),
        ] {
            let sealed = policy
                .prepare(&shell_input(shell, command, None), 1)
                .unwrap();
            assert_eq!(sealed.plan.snapshot_roots.len(), 1, "{command}");
            assert_eq!(sealed.snapshot.as_ref().unwrap().roots[0].path, "/work/r1");
        }
        // Without a cwd or approved directory nothing is snapshotted, and an
        // omitted sandbox keeps the command unrestricted with a neutral mark.
        let none = test_policy()
            .prepare(&shell_input("bash", "make fmt", None), 1)
            .unwrap();
        assert!(none.plan.snapshot_roots.is_empty() && none.snapshot.is_none());
        assert_eq!(none.plan.sandbox_mode, ExecSandboxMode::Unrestricted);
        assert!(!none.unsandboxed());
        // Elevated PTY commands run as root and are never snapshotted.
        let elevated = policy
            .prepare(
                &input_with("sudo docker restart web1", CommandIoMode::Pty),
                1,
            )
            .unwrap();
        assert!(elevated.is_elevated());
        assert!(elevated.plan.snapshot_roots.is_empty());
        // The same template without elevation is snapshotted.
        let plain = policy
            .prepare(&shell_input("bash", "docker restart web1", None), 1)
            .unwrap();
        assert_eq!(plain.plan.snapshot_roots.len(), 1);
    }

    #[test]
    fn cwd_is_the_single_root_otherwise_every_current_grant() {
        let policy = policy_with(&["r2", "r1"]);
        let with_cwd = policy
            .prepare(&shell_input("bash", "make fmt", Some("/repo")), 1)
            .unwrap();
        assert_eq!(
            with_cwd.plan.snapshot_roots,
            vec![ExecSnapshotRoot::Cwd {
                path: "/repo".into()
            }]
        );
        let notice = with_cwd.snapshot.as_ref().unwrap();
        assert_eq!(notice.roots[0].kind, ExecSnapshotRootKind::Cwd);
        let without = policy
            .prepare(&shell_input("bash", "make fmt", None), 1)
            .unwrap();
        // Grants come in their trusted order (the deriving function sorts).
        assert_eq!(
            without.plan.snapshot_roots,
            vec![
                ExecSnapshotRoot::Grant {
                    directory_request_id: "r2".into(),
                    directory: grant("r2").directory,
                },
                ExecSnapshotRoot::Grant {
                    directory_request_id: "r1".into(),
                    directory: grant("r1").directory,
                },
            ]
        );
        assert_eq!(
            without.validation_input.snapshot_roots,
            without.plan.snapshot_roots
        );
        // Too many approvals for one snapshot: the command must name a cwd.
        let many: Vec<String> = (0..=MAX_EXEC_SNAPSHOT_ROOTS)
            .map(|i| format!("r{i:03}"))
            .collect();
        let crowded = policy_with(&many.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(
            crowded
                .prepare(&shell_input("bash", "make fmt", None), 1)
                .is_err()
        );
        assert!(
            crowded
                .prepare(&shell_input("bash", "make fmt", Some("/repo")), 1)
                .is_ok()
        );
    }

    #[test]
    fn snapshot_roots_are_fingerprinted_and_reproduced_from_the_input() {
        let canonical = shell_input("bash", "make fmt", None);
        let one = policy_with(&["r1"]).prepare(&canonical, 1).unwrap();
        let other = policy_with(&["r2"]).prepare(&canonical, 1).unwrap();
        assert_ne!(one.plan.fingerprint, other.plan.fingerprint);
        assert_ne!(
            one.resource_scope().unwrap(),
            other.resource_scope().unwrap()
        );
        // The edge re-classifies the validation input into the identical draft.
        let rebuilt = crate::exec_classify::classify_command_with_policy(
            &one.validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            ExecAdmissionPolicy::OwnerInteractive,
        );
        assert_eq!(rebuilt.draft.as_ref(), Some(&one.plan));
        let sealed = desk_agent_protocol::exec::ExecPlan::from_draft(
            desk_agent_protocol::exec::ExecRequestId("exec".into()),
            "gen",
            desk_agent_protocol::exec::ApprovalId("approval".into()),
            one.plan.clone(),
        );
        assert!(sealed.matches_draft(&one.plan));
        assert!(!sealed.matches_draft(&other.plan));
        // A tampered root breaks the frozen confirmation.
        let mut tampered = one.clone();
        tampered.plan.snapshot_roots = other.plan.snapshot_roots.clone();
        tampered.validation_input.snapshot_roots = other.plan.snapshot_roots.clone();
        assert!(tampered.validate(&canonical).is_err());
        let mut dropped = one.clone();
        dropped.snapshot = None;
        assert!(dropped.validate(&canonical).is_err());
        // The persisted confirmation round-trips.
        let json = serde_json::to_string(&one).unwrap();
        assert_eq!(
            serde_json::from_str::<CommandConfirmation>(&json).unwrap(),
            one
        );
    }

    #[test]
    fn grant_changes_invalidate_only_snapshotted_approvals() {
        let mutating = shell_input("bash", "make fmt", None);
        let readonly = shell_input("powershell", "Get-Service -Name Spooler", None);
        let in_cwd = shell_input("bash", "make fmt", Some("/repo"));
        let before = policy_with(&["r1"]);
        let approved =
            [&mutating, &readonly, &in_cwd].map(|canonical| before.prepare(canonical, 1).unwrap());
        for after in [policy_with(&["r1", "r2"]), policy_with(&[])] {
            assert!(after.revalidate(&approved[0], &mutating, 1).is_err());
            after.revalidate(&approved[1], &readonly, 1).unwrap();
            after.revalidate(&approved[2], &in_cwd, 1).unwrap();
        }
        before.revalidate(&approved[0], &mutating, 1).unwrap();
    }

    #[test]
    fn grants_must_belong_to_the_target_device() {
        let mut policy = policy_with(&["r1"]);
        policy.target_device_id = "device-2".into();
        assert!(policy.prepare(&input("df -h"), 1).is_err());
    }

    #[test]
    fn interactive_commands_seal_a_pty_plan_only_when_the_host_allows_it() {
        let canonical = input_with("top", CommandIoMode::Pty);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(confirmation.plan.io_mode.is_pty());
        assert!(confirmation.is_interactive());
        assert!(!confirmation.is_elevated());
        test_policy()
            .revalidate(&confirmation, &canonical, 1)
            .unwrap();
        // The PTY mode is part of the plan fingerprint and of the proposal.
        let mut downgraded = confirmation.clone();
        downgraded.plan.io_mode = ExecIoMode::NonInteractive;
        assert!(downgraded.validate(&canonical).is_err());
        let mut policy = test_policy();
        policy.exec_pty = false;
        assert!(policy.prepare(&canonical, 1).is_err());
        assert!(policy.revalidate(&confirmation, &canonical, 1).is_err());
        // Non-interactive commands never need the PTY capability.
        assert!(policy.prepare(&input("df -h"), 1).is_ok());
    }

    #[test]
    fn command_target_separates_the_connection_from_the_worker_incarnation() {
        let mut policy = test_policy();
        policy.target_session_id = "device-connection:desktop:worker".into();
        let sealed = policy.prepare(&input("df -h"), 1).unwrap();
        assert_eq!(sealed.target_connection_id(), "device-connection");
        assert_eq!(sealed.target_session_id, "device-connection:desktop:worker");
    }

    #[test]
    fn interactive_elevation_requires_the_elevation_capability() {
        let canonical = input_with("sudo docker ps", CommandIoMode::Pty);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(confirmation.is_elevated());
        let mut policy = test_policy();
        policy.exec_pty_elevation = false;
        assert!(policy.prepare(&canonical, 1).is_err());
        // Elevation outside a PTY stays blocked.
        assert!(test_policy().prepare(&input("sudo df -h"), 1).is_err());
    }

    #[test]
    fn interactive_execution_ids_are_stable_per_permission_item() {
        let first = interactive_exec_request_id("run", "request", "digest");
        assert_eq!(
            first,
            interactive_exec_request_id("run", "request", "digest")
        );
        assert!(crate::native_ids::is(
            crate::native_ids::Kind::InteractiveExecRequest,
            &first
        ));
        assert!(is_permission_bound_exec_request_id(&first));
        assert!(!is_permission_bound_exec_request_id("exec_pty_invalid"));
        assert!(first.len() <= desk_agent_protocol::exec_pty::MAX_PTY_STREAM_ID_BYTES);
        for other in [
            interactive_exec_request_id("run2", "request", "digest"),
            interactive_exec_request_id("run", "request2", "digest"),
            interactive_exec_request_id("run", "request", "digest2"),
        ] {
            assert_ne!(first, other);
        }
    }

    #[test]
    fn freeform_preserves_script_and_freezes_effective_limits_before_approval() {
        let script = "du -d 1 '/tmp/a b' | sort -nr\nprintf done";
        let canonical = input(script);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert_eq!(confirmation.plan.argv, vec!["-lc", script]);
        assert_eq!(confirmation.plan.risk, RiskLevel::Critical);
        assert_eq!(confirmation.plan.timeout_ms, 10_000);
        assert_eq!(confirmation.plan.io_mode, ExecIoMode::NonInteractive);
        test_policy()
            .revalidate(&confirmation, &canonical, 1)
            .unwrap();
    }

    #[test]
    fn missing_authority_blocked_commands_and_insufficient_risk_never_produce_plans() {
        let mut policy = test_policy();
        policy.admission_policy = ExecAdmissionPolicy::TemplateOnly;
        assert!(policy.prepare(&input("df -h"), 1).is_err());
        policy = test_policy();
        policy.max_risk = RiskLevel::High;
        assert!(policy.prepare(&input("df -h"), 1).is_err());
        for mode in [ExecutionMode::ReadOnly, ExecutionMode::SuggestOnly] {
            policy = test_policy();
            policy.execution_mode = mode;
            assert!(policy.prepare(&input("df -h"), 1).is_err());
        }
        assert!(test_policy().prepare(&input("sudo df -h"), 1).is_err());
        assert!(test_policy().prepare(&input("df\0-h"), 1).is_err());
    }

    #[test]
    fn input_target_policy_limit_and_plan_changes_invalidate_original_confirmation() {
        let canonical = input("df -h");
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(
            test_policy()
                .revalidate(&confirmation, &input("df -H"), 1)
                .is_err()
        );
        assert!(
            test_policy()
                .revalidate(&confirmation, &canonical, 2)
                .is_err()
        );
        for mutate in [
            |p: &mut CommandPolicyContext| p.target_session_id.push('2'),
            |p: &mut CommandPolicyContext| p.policy_version.push('2'),
            |p: &mut CommandPolicyContext| p.max_runtime_ms = 5_000,
            |p: &mut CommandPolicyContext| p.available_shells.clear(),
        ] {
            let mut policy = test_policy();
            mutate(&mut policy);
            assert!(policy.revalidate(&confirmation, &canonical, 1).is_err());
        }
        let mut changed = confirmation.clone();
        changed.plan.argv.push("extra".into());
        assert_ne!(
            changed.resource_scope().unwrap(),
            confirmation.resource_scope().unwrap()
        );
        assert!(test_policy().revalidate(&changed, &canonical, 1).is_err());
    }
}

/// Persisted on the permission item before it is shown to the owner. The grant
/// scope contains this whole snapshot's digest in addition to the input digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfirmation {
    pub schema_version: u16,
    pub actor_id: String,
    pub target_device_id: String,
    pub target_session_id: String,
    pub policy_revision: i64,
    pub input_revision: u64,
    pub policy_version: String,
    pub admission_policy: ExecAdmissionPolicy,
    pub proposal: CommandDraft,
    pub validation_input: ExecInput,
    pub plan: ExecPlanDraft,
    pub canonical_input_digest_sha256: String,
    /// What the owner is shown about the plan's snapshot roots; present iff
    /// the plan has roots.
    pub snapshot: Option<Box<ExecSnapshotNotice>>,
    /// How the command runs, the device facts that decided it and the
    /// execution identity; the card and the AI review read it from here.
    pub sandbox: ExecSandboxNotice,
}

/// A classified, executable plan and its effect.
struct AdmittedPlan {
    effect: ExecEffect,
    plan: ExecPlanDraft,
}

impl AdmittedPlan {
    fn requires_root_pty_containment(&self) -> bool {
        self.plan.requires_root_pty_containment()
    }
}

fn denied(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

impl CommandPolicyContext {
    pub fn prepare(
        &self,
        canonical: &str,
        input_revision: u64,
    ) -> Result<CommandConfirmation, AgentError> {
        if self.actor_id.is_empty()
            || self.target_device_id.is_empty()
            || self.target_session_id.is_empty()
            || self.policy_version.is_empty()
            || self.policy_revision < 1
            || input_revision == 0
            || !self.execution_mode.allows_confirmed_execution()
            || self.directory_grants.device_id() != self.target_device_id
        {
            return Err(denied(
                "confirmed command execution is unavailable under current policy",
            ));
        }
        let proposal: CommandDraft = serde_json::from_str(canonical).map_err(|e| {
            denied(crate::model_input::describe_error(
                COMMAND_TOOL,
                &format!("Invalid exact command input: {e}"),
            ))
        })?;
        proposal.validate().map_err(|e| {
            denied(crate::model_input::describe_error(
                COMMAND_TOOL,
                &format!("Invalid exact command input: {e}"),
            ))
        })?;
        if proposal.io_mode == CommandIoMode::Pty && !self.exec_pty {
            return Err(denied(
                "interactive (PTY) execution is not enabled on this device",
            ));
        }
        let shell = crate::exec_tools::canonical_exec_shell(&proposal.shell)
            .ok_or_else(|| denied("exact command shell is not supported"))?;
        if !crate::exec_tools::exec_shell_is_available(shell, &self.available_shells) {
            return Err(denied("exact command shell is unavailable on this device"));
        }
        let mut input = ExecInput {
            target: ExecTarget::Shell {
                shell: shell.into(),
            },
            command: proposal.command.clone(),
            cwd: proposal.cwd.clone(),
            io_mode: proposal.io_mode.exec_io_mode(),
            timeout_ms: proposal.timeout_ms,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
            snapshot_roots: Vec::new(),
            sandbox_mode: ExecSandboxMode::Unrestricted,
        };
        crate::exec_tools::apply_exec_runtime_ceiling(&mut input, self.max_runtime_ms);
        let classified = self.admitted_plan(&input)?;
        let elevated = classified.requires_root_pty_containment();
        let mutating = classified.effect == ExecEffect::Mutating;
        // A mutating command's write roots: snapshotted, and the only
        // writable directories when it runs sandboxed. An interactive
        // elevation runs as root in the daemon and has none.
        let (roots, labels) = if mutating && !elevated {
            self.snapshot_roots(input.cwd.as_deref())?
        } else {
            (Vec::new(), Vec::new())
        };
        let sandbox = sandbox::resolve(
            &self.exec_sandbox,
            proposal.sandbox,
            mutating,
            elevated,
            !roots.is_empty(),
        )?;
        if sandbox.mode == ExecSandboxMode::Sandboxed {
            sandbox::check_sandboxed_paths(&self.exec_sandbox, input.cwd.as_deref(), &labels)?;
        }
        // The classifier seals the roots and the mode into the draft, so the
        // edge reproduces them from the input.
        let snapshot = (!roots.is_empty()).then(|| Box::new(ExecSnapshotNotice::new(labels)));
        let plan = if roots.is_empty() && sandbox.mode == ExecSandboxMode::Unrestricted {
            classified.plan
        } else {
            input.snapshot_roots = roots;
            input.sandbox_mode = sandbox.mode;
            self.admitted_plan(&input)?.plan
        };
        if plan.risk > self.max_risk {
            return Err(denied("command exceeds the current risk policy"));
        }
        if plan.requires_root_pty_containment() && !self.exec_pty_elevation {
            return Err(denied(
                "interactive elevation is not enabled on this device",
            ));
        }
        let confirmation = CommandConfirmation {
            schema_version: COMMAND_PLAN_SCHEMA_VERSION,
            actor_id: self.actor_id.clone(),
            target_device_id: self.target_device_id.clone(),
            target_session_id: self.target_session_id.clone(),
            policy_revision: self.policy_revision,
            input_revision,
            policy_version: self.policy_version.clone(),
            admission_policy: self.admission_policy,
            proposal,
            validation_input: input,
            plan,
            canonical_input_digest_sha256: format!("{:x}", Sha256::digest(canonical.as_bytes())),
            snapshot,
            sandbox,
        };
        confirmation.validate(canonical)?;
        Ok(confirmation)
    }

    /// Classify `input` under this policy and require an executable plan.
    fn admitted_plan(&self, input: &ExecInput) -> Result<AdmittedPlan, AgentError> {
        let classified = crate::exec_classify::classify_command_with_policy(
            input,
            &self.operator_templates,
            &self.effective_blocklist,
            self.admission_policy,
        );
        if classified.classification.decision != ExecDecision::ConfirmRequired {
            return Err(denied(format!(
                "command rejected by current policy: {}",
                classified.classification.impact
            )));
        }
        Ok(AdmittedPlan {
            effect: classified
                .classification
                .effect
                .unwrap_or(ExecEffect::Mutating),
            plan: classified
                .draft
                .ok_or_else(|| denied("command has no executable plan"))?,
        })
    }

    /// Snapshot roots of a mutating command: its `cwd`, or else every
    /// directory the session currently has approved on the target device.
    fn snapshot_roots(
        &self,
        cwd: Option<&str>,
    ) -> Result<(Vec<ExecSnapshotRoot>, Vec<ExecSnapshotRootLabel>), AgentError> {
        if let Some(path) = cwd {
            return Ok((
                vec![ExecSnapshotRoot::Cwd { path: path.into() }],
                vec![ExecSnapshotRootLabel {
                    kind: ExecSnapshotRootKind::Cwd,
                    path: path.into(),
                }],
            ));
        }
        let grants = self.directory_grants.grants();
        if grants.len() > MAX_EXEC_SNAPSHOT_ROOTS {
            return Err(denied(format!(
                "this conversation has more than {MAX_EXEC_SNAPSHOT_ROOTS} approved directories on the device; set cwd for a command that changes files"
            )));
        }
        Ok(grants
            .iter()
            .map(|grant| {
                (
                    ExecSnapshotRoot::Grant {
                        directory_request_id: grant.directory_request_id.clone(),
                        directory: grant.directory.clone(),
                    },
                    ExecSnapshotRootLabel {
                        kind: ExecSnapshotRootKind::Grant,
                        path: grant.canonical_path.clone(),
                    },
                )
            })
            .unzip())
    }

    pub fn revalidate(
        &self,
        confirmation: &CommandConfirmation,
        canonical: &str,
        input_revision: u64,
    ) -> Result<(), AgentError> {
        if self.prepare(canonical, input_revision)? != *confirmation {
            return Err(denied(
                "the command plan, target or policy changed; request permission again",
            ));
        }
        Ok(())
    }
}

/// Whether an execution id was derived from an owner-approved PTY permission.
/// Such an execution binds the carrier consumed by that approval, whichever
/// client connection later runs the approved turn.
pub fn is_permission_bound_exec_request_id(exec_request_id: &str) -> bool {
    crate::native_ids::is(
        crate::native_ids::Kind::InteractiveExecRequest,
        exec_request_id,
    )
}

/// Stable execution id of an interactive command, derived from the permission
/// item that approves it. The owner's terminal carrier is prepared for this id
/// while the permission is still pending, and the approved execution reuses it,
/// so the host's stream-opened report matches the carrier it was bound to.
pub fn interactive_exec_request_id(
    run_id: &str,
    permission_request_id: &str,
    canonical_input_digest_sha256: &str,
) -> String {
    crate::native_ids::stable(
        crate::native_ids::Kind::InteractiveExecRequest,
        &(run_id, permission_request_id, canonical_input_digest_sha256),
    )
}

/// The interactive command a permission decision approves, and the carrier
/// binding it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveApproval {
    pub exec_request_id: String,
    pub target_connection_id: String,
}

/// How a decision may use the client's `carrierId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierUsageError {
    /// Approving an interactive command needs the owner's ready carrier.
    CarrierRequired,
    /// A carrier was supplied for a decision that approves no interactive
    /// command (a denial, or an ordinary request).
    CarrierNotAllowed,
}

/// Validates the carrier of one pending permission decision. Returns the
/// interactive approval that must consume the carrier, or `None` when the
/// decision involves no carrier. A request that is no longer pending (a
/// replay of a committed decision) never consumes one.
pub fn interactive_approval(
    run_id: &str,
    request: &crate::dynamic_run::PermissionRequest,
    decisions: &[crate::dynamic_run::PermissionDecisionItem],
    carrier_id: Option<&str>,
) -> Result<Option<InteractiveApproval>, CarrierUsageError> {
    use crate::dynamic_run::{PermissionItemDecision, PermissionRequestState};
    let carrier_id = carrier_id.filter(|value| !value.is_empty());
    let approved = request
        .items
        .iter()
        .filter_map(|item| {
            let confirmation = item
                .command_confirmation
                .as_ref()
                .filter(|confirmation| confirmation.is_interactive())?;
            decisions
                .iter()
                .any(|decision| {
                    decision.item_id == item.item_id
                        && matches!(decision.decision, PermissionItemDecision::Approve { .. })
                })
                .then(|| InteractiveApproval {
                    exec_request_id: interactive_exec_request_id(
                        run_id,
                        &request.request_id,
                        &confirmation.canonical_input_digest_sha256,
                    ),
                    target_connection_id: confirmation.target_connection_id().to_owned(),
                })
        })
        .next();
    match (approved, carrier_id) {
        (None, Some(_)) => Err(CarrierUsageError::CarrierNotAllowed),
        (None, None) => Ok(None),
        (Some(_), _) if request.state != PermissionRequestState::Pending => Ok(None),
        (Some(_), None) => Err(CarrierUsageError::CarrierRequired),
        (Some(approval), Some(_)) => Ok(Some(approval)),
    }
}

impl CommandConfirmation {
    pub fn target_connection_id(&self) -> &str {
        // Signaling connection IDs are UUIDs. Worker incarnation IDs can
        // themselves contain separators.
        self.target_session_id
            .split_once(':')
            .map_or(self.target_session_id.as_str(), |(connection, _)| {
                connection
            })
    }

    pub fn is_interactive(&self) -> bool {
        self.plan.io_mode.is_pty()
    }

    pub fn is_elevated(&self) -> bool {
        self.plan.requires_root_pty_containment()
    }

    /// A mutating command that runs outside the sandbox where it could have
    /// been sandboxed or was explicitly taken out of it. Derived from the
    /// frozen notice; the AI review receives it as `unsandboxed`.
    pub fn unsandboxed(&self) -> bool {
        self.sandbox.unsandboxed()
    }

    /// The approved confirmation for this exact call together with the id of
    /// the permission request that approved it.
    pub fn approved_request_for_call<'a>(
        session: &'a crate::session::PersistedAgentSession,
        target_device_id: &str,
        canonical: &str,
    ) -> Result<(&'a str, &'a Self), AgentError> {
        use crate::dynamic_run::PermissionRequestState;
        if !session
            .agent_role
            .allows_effect(crate::registry::ToolEffect::Mutating)
        {
            return Err(denied(
                "the coordination session cannot execute device commands",
            ));
        }
        if !session.admits_agent_device(target_device_id) {
            return Err(denied("command target is outside the admitted request"));
        }
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        session
            .permission_requests
            .iter()
            .rev()
            .filter(|request| {
                request.input_revision == session.input_revision
                    && matches!(
                        request.state,
                        PermissionRequestState::Approved
                            | PermissionRequestState::PartiallyApproved
                    )
            })
            .flat_map(|request| {
                request
                    .items
                    .iter()
                    .map(move |item| (request.request_id.as_str(), item))
            })
            .filter(|(_, item)| {
                item.tool_name == COMMAND_TOOL
                    && item.canonical_input_digest_sha256.as_deref() == Some(digest.as_str())
            })
            .filter_map(|(request_id, item)| {
                item.command_confirmation
                    .as_ref()
                    .map(|confirmation| (request_id, confirmation))
            })
            .find(|(_, confirmation)| {
                confirmation.actor_id == session.actor_id
                    && confirmation.target_device_id == target_device_id
                    && confirmation.input_revision == session.input_revision
                    && confirmation.policy_revision == session.policy_revision
                    && confirmation.validate(canonical).is_ok()
            })
            .ok_or_else(|| denied("command has no approved exact plan; request permission first"))
    }

    /// Execution id for this approved call: the permission-derived id for an
    /// interactive command, `None` otherwise.
    pub fn interactive_request_id(&self, run_id: &str, permission_id: &str) -> Option<String> {
        self.is_interactive().then(|| {
            interactive_exec_request_id(run_id, permission_id, &self.canonical_input_digest_sha256)
        })
    }

    pub fn validate(&self, canonical: &str) -> Result<(), AgentError> {
        if self.schema_version != COMMAND_PLAN_SCHEMA_VERSION
            || self.actor_id.is_empty()
            || self.target_session_id.is_empty()
            || self.policy_version.is_empty()
            || self.plan.io_mode != self.proposal.io_mode.exec_io_mode()
            || self.plan.snapshot_roots != self.validation_input.snapshot_roots
            || self.plan.sandbox_mode != self.validation_input.sandbox_mode
            || self.sandbox.mode != self.plan.sandbox_mode
            || !self.sandbox.is_consistent()
            || (self.sandbox.mode == ExecSandboxMode::Sandboxed && self.is_elevated())
            || self.snapshot.as_ref().map(|notice| notice.roots.len())
                != (!self.plan.snapshot_roots.is_empty()).then_some(self.plan.snapshot_roots.len())
            || serde_json::from_str::<CommandDraft>(canonical)
                .ok()
                .as_ref()
                != Some(&self.proposal)
            || format!("{:x}", Sha256::digest(canonical.as_bytes()))
                != self.canonical_input_digest_sha256
        {
            return Err(denied("invalid command confirmation"));
        }
        self.proposal
            .validate()
            .map_err(|_| denied("invalid command proposal"))?;
        let limits = desk_agent_protocol::exec_policy::ExecLimits {
            timeout_ms: self.plan.timeout_ms,
            max_stdout_bytes: self.plan.max_stdout_bytes,
            max_stderr_bytes: self.plan.max_stderr_bytes,
        };
        if self.plan.fingerprint
            != desk_agent_protocol::exec_policy::fingerprint_with_io_mode(
                &self.plan.program,
                &self.plan.argv,
                self.plan.cwd.as_deref(),
                &limits,
                &self.plan.containment,
                self.plan.io_mode,
                &self.plan.snapshot_roots,
                self.plan.sandbox_mode,
            )
        {
            return Err(denied("command plan fingerprint mismatch"));
        }
        desk_agent_protocol::exec::CanonicalCommandIdentity {
            schema_version: COMMAND_PLAN_SCHEMA_VERSION,
            target_device_id: self.target_device_id.clone(),
            policy_revision: self.policy_revision,
            input_revision: self.input_revision,
            plan: self.plan.clone(),
            canonical_input_digest_sha256: self.canonical_input_digest_sha256.clone(),
        }
        .validate()
        .map_err(|_| denied("invalid frozen command plan"))?;
        if self.plan.execution_basis
            == desk_agent_protocol::exec::ExecExecutionBasis::OwnerBlocklistOnly
            && self.admission_policy != ExecAdmissionPolicy::OwnerInteractive
        {
            return Err(denied("freeform command requires owner-interactive policy"));
        }
        Ok(())
    }

    pub fn resource_scope(&self) -> Result<Vec<String>, AgentError> {
        let bytes = serde_json::to_vec(self).map_err(|_| denied("invalid command confirmation"))?;
        let mut scope = crate::capability_grant::exact_command_resource_scope(
            &self.canonical_input_digest_sha256,
        );
        scope.push(format!("command_plan:sha256:{:x}", Sha256::digest(bytes)));
        Ok(scope)
    }
}
