//! The approval path's command policy comes from the worker's current
//! readiness: an owner or AI decision on a command approved under another
//! sandbox support, execution identity or worker incarnation is refused.
use super::*;
use crate::command_policy::tests::{STANDARD_USER, delegated, mark_ready_with, owner_host};
use desk_agent_protocol::exec::{
    ExecIdentity, ExecSandboxMode, ExecSandboxSupport, ExecSandboxUnsupportedReason,
};
use desk_diagnose_core::session::PersistedAgentSession;

const DEVICE: &str = "review-device";
/// The input revision the stored session's commands were requested at.
const INPUT_REVISION: u64 = 1;
const PLAN_CHANGED: &str = "the command plan, target or policy changed; request permission again";

async fn store_session(db: &DatabaseConnection) -> PersistedAgentSession {
    use crate::entity::agent_session;
    use sea_orm::{ActiveModelTrait, Set};
    let mut session = PersistedAgentSession::new(
        "review-conversation",
        SINGLE_ACCOUNT_USER_ID.to_string(),
        DEVICE,
        desk_diagnose_core::agent_policy::PERSONAL_AGENT_POLICY_REVISION,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-17T00:00:00Z",
    );
    session.delegated_directories = vec![delegated(DEVICE, "a")];
    let now = desk_agent_data_store::datetime::utc_now();
    agent_session::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    session
}

#[actix_web::test]
async fn a_review_refuses_a_command_approved_under_another_readiness() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_agent_gate::enable_test_host();
    let session = store_session(&db).await;
    let connection_id = format!("review-host-{}", uuid::Uuid::new_v4());
    let connections = SharedConnectionMap::new_oss();
    connections.write().await.insert(
        connection_id.clone(),
        owner_host(&connection_id, DEVICE, &["bash"], Some(60_000)).await,
    );
    let subject = PendingPermissionReview {
        conversation_id: session.conversation_id.clone(),
        owner_id: session.actor_id.clone(),
        device_id: DEVICE.into(),
        target_device_id: DEVICE.into(),
        request_id: "review-request".into(),
        kind: PendingRequestKind::Permission,
    };
    let ready = |incarnation: &str, support, identity| {
        mark_ready_with(&connection_id, incarnation, &[], support, identity)
    };
    let policy = || async {
        current_device_context(&db, &connections, &subject, jiff::Timestamp::now())
            .await
            .expect("review device context")
            .registry
            .command_policy()
            .expect("review command policy")
            .clone()
    };
    // A sandboxed command and one that runs unrestricted by request.
    let inputs = [
        serde_json::json!({"schema_version":2, "shell":"bash", "command":"touch notes.txt",
            "timeout_ms":1000}),
        serde_json::json!({"schema_version":2, "shell":"bash", "command":"touch notes.txt",
            "sandbox":false, "timeout_ms":1000}),
    ]
    .map(|input| {
        desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            "exec_command",
            input,
        )
        .unwrap()
    });
    ready("worker-1", ExecSandboxSupport::Supported, STANDARD_USER);
    let approved_under = policy().await;
    let confirmations = inputs
        .each_ref()
        .map(|input| approved_under.prepare(input, INPUT_REVISION).unwrap());
    assert_eq!(
        confirmations.each_ref().map(|c| c.plan.sandbox_mode),
        [ExecSandboxMode::Sandboxed, ExecSandboxMode::Unrestricted]
    );

    let changes = [
        (
            "worker-1",
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::NotImplemented),
            STANDARD_USER,
        ),
        (
            "worker-1",
            ExecSandboxSupport::Supported,
            ExecIdentity {
                elevated: true,
                ..STANDARD_USER
            },
        ),
        ("worker-2", ExecSandboxSupport::Supported, STANDARD_USER),
    ];
    for (incarnation, support, identity) in changes {
        // The readiness the commands were approved under keeps them current.
        ready("worker-1", ExecSandboxSupport::Supported, STANDARD_USER);
        let unchanged = policy().await;
        for (input, confirmation) in inputs.iter().zip(&confirmations) {
            unchanged
                .revalidate(confirmation, input, INPUT_REVISION)
                .unwrap();
        }
        ready(incarnation, support, identity);
        let changed = policy().await;
        for (input, confirmation) in inputs.iter().zip(&confirmations) {
            assert_eq!(
                changed
                    .revalidate(confirmation, input, INPUT_REVISION)
                    .unwrap_err()
                    .message,
                PLAN_CHANGED,
                "{incarnation} {support:?} {identity:?}"
            );
        }
    }
}
