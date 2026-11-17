//! Fresh command policy for authenticated single-account assistant requests.

use crate::config::connection::DatabaseConnection;
use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::{AgentError, AgentErrorKind, RiskLevel};
use desk_diagnose_core::command_confirmation::{CommandPolicyContext, DeviceExecSandbox};
use desk_diagnose_core::file_scope::TrustedDirectoryGrants;
use desk_diagnose_core::session::PersistedAgentSession;
use desk_signal_facade::model::connection::SharedConnectionMap;

/// The owner's current command policy on the device behind
/// `target_connection_id`. Its directory approvals come from `session`, the
/// authoritative session the command belongs to, as of `now_unix_ms`.
pub(crate) async fn current(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    actor_id: &str,
    session: &PersistedAgentSession,
    now_unix_ms: u64,
) -> Result<CommandPolicyContext, AgentError> {
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "current owner command policy is unavailable".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    if actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || !crate::ai_assistant_gate::global_ai_assistant_gate().is_enabled()
    {
        return Err(denied());
    }
    let map = connections.read().await;
    let target = map.get(target_connection_id).ok_or_else(denied)?;
    if target.auth_context.auth_kind != desk_signal_facade::model::auth_context::AuthKind::TokenAuth
        || target.auth_context.remote_desk_type
            != desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
    {
        return Err(denied());
    }
    let device = target
        .client_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(denied)?;
    let shells = target.model.exec_capabilities.available_exec_shells.clone();
    let exec_pty = target.model.exec_capabilities.exec_pty;
    let exec_pty_elevation = exec_pty && target.model.exec_capabilities.exec_pty_elevation;
    let max_runtime_ms = target
        .model
        .exec_capabilities
        .max_ai_command_runtime_ms
        .unwrap_or(desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS);
    drop(map);
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
        .get_fresh(target_connection_id, chrono::Utc::now())
        .ok_or_else(denied)?;
    let mode = crate::model_provider::load(db)
        .await
        .map_err(|_| denied())?
        .execution_mode;
    Ok(CommandPolicyContext {
        directory_grants: TrustedDirectoryGrants::from_session(session, &device, now_unix_ms),
        actor_id: actor_id.into(),
        target_device_id: device,
        target_session_id: format!(
            "{}:{}",
            target_connection_id, readiness.readiness.interactive_session_incarnation
        ),
        policy_revision: desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: mode,
        max_risk: RiskLevel::Critical,
        available_shells: shells,
        max_runtime_ms,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: format!("oss-owner:{}", readiness.readiness.local_ceiling_revision),
        exec_pty,
        exec_pty_elevation,
        exec_sandbox: DeviceExecSandbox::from_readiness(&readiness.readiness),
    })
}

/// The stored session of a subject, or `None` when the conversation has not
/// been stored yet.
pub(crate) async fn stored_session(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    device_id: &str,
) -> Result<Option<Box<PersistedAgentSession>>, AgentError> {
    use crate::entity::agent_session;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let unavailable = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "current owner command policy is unavailable".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(actor_id))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(db)
        .await
        .map_err(|_| unavailable())?
        .map(|row| {
            PersistedAgentSession::decode_json(&row.state_json)
                .map(Box::new)
                .map_err(|_| unavailable())
        })
        .transpose()
}

#[cfg(test)]
pub(crate) mod tests;
