use super::*;
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerUseAdapterKind, ComputerUseAdapterRef,
    ComputerUseCapabilityReadiness, ComputerUseReadiness, ObjectKind, ObjectRef,
};
use desk_agent_protocol::exec::{
    ExecIdentity, ExecSandboxAttention, ExecSandboxMode, ExecSandboxSupport,
    ExecSandboxUnsupportedReason, ExecSnapshotRoot,
};
use desk_diagnose_core::file_scope::DelegatedDirectory;
use desk_signal_facade::model::auth_context::AuthContext;
use desk_signal_facade::model::connection::{ConnectionModel, ConnectionState};
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use desk_signal_facade::model::signal::RemoteDeskTypeEnum;
use desk_signal_facade::model::version::VersionInfo;

/// An authenticated OSS host connection for `device`.
pub(crate) async fn owner_host(
    connection_id: &str,
    device: &str,
    shells: &[&str],
    max_ai_command_runtime_ms: Option<u32>,
) -> ConnectionState {
    let request = actix_web::test::TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = <actix_web::web::Payload as actix_web::FromRequest>::from_request(
        &request,
        &mut actix_web::dev::Payload::None,
    )
    .await
    .unwrap();
    let (response, socket, _) = actix_ws::handle(&request, payload).unwrap();
    // Keep the outbound side open so frames sent to this host are accepted.
    std::mem::forget(response);
    let (client_id, model) = SignalingPeer {
        version_info: Some(VersionInfo {
            api_version: 1,
            build_number: 1,
            commit_hash: "fixture".into(),
            debug_build: cfg!(debug_assertions),
        }),
        peer: PeerInfo {
            remote_desk_type: RemoteDeskTypeEnum::Server,
            operation_system: Default::default(),
            display_name: None,
        },
        server: Some(ServerRegistration {
            client_id: device.into(),
            exec_capabilities: HostExecCapabilities {
                available_exec_shells: shells.iter().map(|shell| (*shell).into()).collect(),
                exec_pty: false,
                exec_pty_elevation: false,
                max_ai_command_runtime_ms,
            },
            ai_assistant_settings: desk_agent_protocol::ai_assistant::AiAssistantSettings {
                revision: 1,
                enabled: true,
            },
        }),
    }
    .into_connection_model(connection_id.into(), None);
    ConnectionState {
        lifecycle: Default::default(),
        client_id,
        model: ConnectionModel {
            device_id: None,
            owner_node_id: None,
            ..model
        }
        .into(),
        session: std::sync::Arc::new(tokio::sync::RwLock::new(socket)),
        terminal_connection_ids: Default::default(),
        request_callback_map: Default::default(),
        device_code: None,
        auth_context: AuthContext::token_auth(
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            1,
            RemoteDeskTypeEnum::Server,
        ),
    }
}

/// A standard (non-elevated, non-root) execution identity.
pub(crate) const STANDARD_USER: ExecIdentity = ExecIdentity {
    elevated: false,
    system: false,
    root: false,
};

/// Fresh readiness of `connection_id` for the given device capabilities, on a
/// worker that supports the command sandbox and runs as a standard user.
pub(crate) fn mark_ready(
    connection_id: &str,
    incarnation: &str,
    capabilities: &[desk_agent_protocol::Capability],
) {
    mark_ready_with(
        connection_id,
        incarnation,
        capabilities,
        ExecSandboxSupport::Supported,
        STANDARD_USER,
    );
}

/// Fresh readiness of `connection_id` reporting the given sandbox support
/// and execution identity.
pub(crate) fn mark_ready_with(
    connection_id: &str,
    incarnation: &str,
    capabilities: &[desk_agent_protocol::Capability],
    exec_sandbox: ExecSandboxSupport,
    exec_identity: ExecIdentity,
) {
    let now = chrono::Utc::now();
    let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
    cache.remove_connection(connection_id);
    cache
        .update(
            connection_id,
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: "linux".into(),
                interactive_user_home: None,
                exec_sandbox,
                exec_identity,
                interactive_session_incarnation: incarnation.into(),
                local_ceiling_revision: 1,
                unattended_command_execution: false,
                capabilities: capabilities
                    .iter()
                    .map(|capability| ComputerUseCapabilityReadiness {
                        capability: *capability,
                        adapter: ComputerUseAdapterRef {
                            kind: ComputerUseAdapterKind::Terminal,
                            version: "1".into(),
                        },
                        supported: true,
                        ready: true,
                        reason: None,
                    })
                    .collect(),
                context_references: vec![],
            },
            now,
        )
        .unwrap();
}

/// A main conversation's directory approval delegated to `session`.
pub(crate) fn delegated(device: &str, request_id: &str) -> DelegatedDirectory {
    DelegatedDirectory {
        device_id: device.into(),
        request_id: request_id.into(),
        canonical_path: format!("/work/{request_id}"),
        directory: ObjectRef {
            token: format!("directory-{request_id}"),
            snapshot_id: "generation-1".into(),
            object_kind: ObjectKind::Directory,
            expires_at: String::new(),
        },
    }
}

struct Fixture {
    db: DatabaseConnection,
    connections: SharedConnectionMap,
    connection_id: String,
}

const DEVICE: &str = "policy-device";

async fn fixture() -> Fixture {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let connection_id = format!("policy-host-{}", uuid::Uuid::new_v4());
    let connections = SharedConnectionMap::new_oss();
    connections.write().await.insert(
        connection_id.clone(),
        owner_host(
            &connection_id,
            DEVICE,
            &["bash", "powershell"],
            Some(60_000),
        )
        .await,
    );
    mark_ready(&connection_id, "desktop", &[]);
    Fixture {
        db,
        connections,
        connection_id,
    }
}

fn session(grants: &[&str]) -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "policy-conversation",
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string(),
        DEVICE,
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-16T00:00:00Z",
    );
    session.delegated_directories = grants.iter().map(|id| delegated(DEVICE, id)).collect();
    session
}

impl Fixture {
    async fn policy(&self, session: &PersistedAgentSession) -> CommandPolicyContext {
        current(
            &self.db,
            &self.connections,
            &self.connection_id,
            &crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string(),
            session,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap()
    }
}

fn command(shell: &str, command: &str) -> String {
    desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        "exec_command",
        serde_json::json!({"schema_version":2, "shell":shell, "command":command, "timeout_ms":1000}),
    )
    .unwrap()
}

#[actix_web::test]
async fn current_policy_takes_directory_approvals_from_the_session_for_its_device() {
    let fixture = fixture().await;
    let mut session = session(&["b", "a"]);
    session
        .delegated_directories
        .push(delegated("other-device", "c"));
    let policy = fixture.policy(&session).await;
    assert_eq!(policy.target_device_id, DEVICE);
    assert_eq!(policy.directory_grants.device_id(), DEVICE);
    let ids: Vec<_> = policy
        .directory_grants
        .grants()
        .iter()
        .map(|grant| grant.directory_request_id.as_str())
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[actix_web::test]
async fn a_mutating_command_needs_new_approval_after_any_directory_approval_change() {
    let fixture = fixture().await;
    let approved_with = session(&["a"]);
    let input = command("bash", "touch notes.txt");
    let confirmation = fixture
        .policy(&approved_with)
        .await
        .prepare(&input, 1)
        .unwrap();
    assert_eq!(
        confirmation.plan.snapshot_roots,
        [ExecSnapshotRoot::Grant {
            directory_request_id: "a".into(),
            directory: delegated(DEVICE, "a").directory,
        }]
    );
    fixture
        .policy(&approved_with)
        .await
        .revalidate(&confirmation, &input, 1)
        .unwrap();
    for changed in [session(&["a", "b"]), session(&[])] {
        let error = fixture
            .policy(&changed)
            .await
            .revalidate(&confirmation, &input, 1)
            .unwrap_err();
        assert_eq!(
            error.message,
            "the command plan, target or policy changed; request permission again"
        );
    }
}

#[actix_web::test]
async fn a_read_only_command_keeps_its_approval_across_directory_approval_changes() {
    let fixture = fixture().await;
    let input = command("powershell", "Get-Service -Name Spooler");
    let confirmation = fixture
        .policy(&session(&["a"]))
        .await
        .prepare(&input, 1)
        .unwrap();
    assert!(confirmation.plan.snapshot_roots.is_empty());
    for changed in [session(&["a", "b"]), session(&[])] {
        fixture
            .policy(&changed)
            .await
            .revalidate(&confirmation, &input, 1)
            .unwrap();
    }
}

const PLAN_CHANGED: &str = "the command plan, target or policy changed; request permission again";

fn command_with_sandbox(command: &str, sandbox: bool) -> String {
    desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        "exec_command",
        serde_json::json!({"schema_version":2, "shell":"bash", "command":command,
            "sandbox":sandbox, "timeout_ms":1000}),
    )
    .unwrap()
}

#[actix_web::test]
async fn current_policy_resolves_the_sandbox_from_the_cached_readiness() {
    let fixture = fixture().await;
    let approved_with = session(&["a"]);
    let input = command("bash", "touch notes.txt");
    let sandboxed = fixture
        .policy(&approved_with)
        .await
        .prepare(&input, 1)
        .unwrap();
    assert_eq!(sandboxed.plan.sandbox_mode, ExecSandboxMode::Sandboxed);
    assert_eq!(sandboxed.sandbox.mode, ExecSandboxMode::Sandboxed);
    assert_eq!(
        sandboxed.sandbox.device_support,
        ExecSandboxSupport::Supported
    );

    let kernel = ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported);
    mark_ready_with(
        &fixture.connection_id,
        "desktop",
        &[],
        kernel,
        STANDARD_USER,
    );
    let unrestricted = fixture
        .policy(&approved_with)
        .await
        .prepare(&input, 1)
        .unwrap();
    assert_eq!(
        unrestricted.plan.sandbox_mode,
        ExecSandboxMode::Unrestricted
    );
    assert_eq!(
        unrestricted.sandbox.attention,
        ExecSandboxAttention::Prominent
    );
    assert_eq!(unrestricted.sandbox.device_support, kernel);

    // Root cannot be sandboxed: an explicit request is refused before the card.
    mark_ready_with(
        &fixture.connection_id,
        "desktop",
        &[],
        ExecSandboxSupport::Supported,
        ExecIdentity {
            root: true,
            ..STANDARD_USER
        },
    );
    let error = fixture
        .policy(&approved_with)
        .await
        .prepare(&command_with_sandbox("touch notes.txt", true), 1)
        .unwrap_err();
    assert!(
        error.message.starts_with("sandbox=true cannot be honored"),
        "{}",
        error.message
    );
}

#[actix_web::test]
async fn an_approved_command_needs_new_approval_after_a_sandbox_identity_or_worker_change() {
    let fixture = fixture().await;
    let approved_with = session(&["a"]);
    // A sandboxed mutating command, an explicitly unrestricted one and a
    // read-only template that runs unrestricted.
    let inputs = [
        command("bash", "touch notes.txt"),
        command_with_sandbox("touch notes.txt", false),
        command("powershell", "Get-Service -Name Spooler"),
    ];
    let policy = fixture.policy(&approved_with).await;
    let confirmations: Vec<_> = inputs
        .iter()
        .map(|input| policy.prepare(input, 1).unwrap())
        .collect();
    assert_eq!(
        confirmations
            .iter()
            .map(|confirmation| confirmation.plan.sandbox_mode)
            .collect::<Vec<_>>(),
        [
            ExecSandboxMode::Sandboxed,
            ExecSandboxMode::Unrestricted,
            ExecSandboxMode::Unrestricted,
        ]
    );
    let revalidate = |policy: &CommandPolicyContext| {
        inputs
            .iter()
            .zip(&confirmations)
            .map(|(input, confirmation)| {
                policy
                    .revalidate(confirmation, input, 1)
                    .map_err(|error| error.message)
            })
            .collect::<Vec<_>>()
    };

    // A new readiness report that changes nothing the policy reads keeps
    // every approval current.
    mark_ready(
        &fixture.connection_id,
        "desktop",
        &[desk_agent_protocol::Capability::ShellExecConfirmed],
    );
    assert!(
        revalidate(&fixture.policy(&approved_with).await)
            .iter()
            .all(Result::is_ok)
    );

    let changes = [
        (
            "desktop",
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
            STANDARD_USER,
        ),
        (
            "desktop",
            ExecSandboxSupport::Supported,
            ExecIdentity {
                elevated: true,
                ..STANDARD_USER
            },
        ),
        (
            "desktop-restarted",
            ExecSandboxSupport::Supported,
            STANDARD_USER,
        ),
    ];
    for (incarnation, support, identity) in changes {
        mark_ready_with(&fixture.connection_id, incarnation, &[], support, identity);
        for result in revalidate(&fixture.policy(&approved_with).await) {
            assert_eq!(
                result.unwrap_err(),
                PLAN_CHANGED,
                "{incarnation} {support:?} {identity:?}"
            );
        }
    }
}

#[actix_web::test]
async fn stored_session_reads_the_subject_row_only() {
    use crate::entity::agent_session;
    use sea_orm::{ActiveModelTrait, Set};
    let fixture = fixture().await;
    let stored = session(&["a"]);
    let now = chrono::Utc::now();
    agent_session::ActiveModel {
        conversation_id: Set(stored.conversation_id.clone()),
        actor_id: Set(stored.actor_id.clone()),
        device_id: Set(stored.device_id.clone()),
        state_json: Set(stored.encode_json_for_storage().unwrap()),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&fixture.db)
    .await
    .unwrap();
    let loaded = stored_session(
        &fixture.db,
        &stored.conversation_id,
        &stored.actor_id,
        DEVICE,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(loaded.delegated_directories, stored.delegated_directories);
    assert!(
        stored_session(&fixture.db, &stored.conversation_id, &stored.actor_id, "")
            .await
            .unwrap()
            .is_none()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn frozen_macos_disk_scan_plan_executes_the_original_multiline_script_in_an_isolated_directory() {
    use desk_agent_protocol::authz::ExecAdmissionPolicy;
    use desk_agent_protocol::{ExecutionMode, RiskLevel};
    let fixture = tempfile::Builder::new()
        .prefix("assistant command ")
        .tempdir()
        .unwrap();
    for (name, bytes) in [("small directory", 4096), ("large directory", 131072)] {
        let path = fixture.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("fixture.bin"), vec![42u8; bytes]).unwrap();
    }
    let policy = CommandPolicyContext {
        actor_id: "1".into(),
        target_device_id: "fixture-device".into(),
        target_session_id: "fixture-host:session".into(),
        policy_revision: 1,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: ExecutionMode::ConfirmEachAction,
        max_risk: RiskLevel::Critical,
        available_shells: vec!["bash".into()],
        max_runtime_ms: 10000,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: "fixture-owner-policy".into(),
        exec_pty: false,
        exec_pty_elevation: false,
        directory_grants: TrustedDirectoryGrants::for_test("fixture-device", vec![]),
        exec_sandbox: desk_diagnose_core::command_confirmation::DeviceExecSandbox::for_test(
            desk_agent_protocol::exec::ExecSandboxSupport::Supported,
            desk_agent_protocol::exec::ExecIdentity {
                elevated: false,
                system: false,
                root: false,
            },
            "macos",
        ),
    };
    let script = "du -k -d 1 . | sort -n\ndf -k .";
    let input = serde_json::json!({"schema_version":2, "shell":"bash", "command":script,
        "cwd":fixture.path().to_str().unwrap(), "timeout_ms":10000})
    .to_string();
    let confirmation = policy.prepare(&input, 1).unwrap();
    policy.revalidate(&confirmation, &input, 1).unwrap();
    assert_eq!(confirmation.plan.argv, vec!["-lc", script]);
    assert_eq!(
        confirmation.plan.io_mode,
        desk_agent_protocol::exec::ExecIoMode::NonInteractive
    );
    // This is a real shell/argv smoke test, not a substitute for the remote
    // permission, transport, worker containment, or model end-to-end tests.
    let output = std::process::Command::new(&confirmation.plan.program)
        .args(&confirmation.plan.argv)
        .current_dir(confirmation.plan.cwd.as_ref().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let size = |directory: &str| {
        stdout
            .lines()
            .find_map(|line| {
                let (size, path) = line.split_once('\t')?;
                (path == format!("./{directory}")).then(|| size.parse::<u64>().unwrap())
            })
            .unwrap()
    };
    assert!(size("large directory") > size("small directory"));
    assert!(stdout.contains("Filesystem"));
}
