//! Workbench identity and native coordinator construction shared by both hosts.

pub mod tasks;

use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    ai_agent::workbench::{AiAgentWorkbenchView, CreateAiAgentWorkbench},
};
use desk_diagnose_core::{
    agent_policy::PERSONAL_AGENT_POLICY_REVISION,
    conversation_key::{
        AgentConversationScope, derive_agent_conversation_key, is_valid_client_conversation_id,
    },
    session::{AgentSessionSurface, PersistedAgentSession},
};

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

use crate::{
    AgentStoreError, device_roster, entity::agent_main_request as request,
    main_request::decode_workbench_coordinator,
};

pub fn identity(
    actor: &str,
    body: CreateAiAgentWorkbench,
) -> Result<AiAgentWorkbenchView, AgentStoreError> {
    let conversation_id = format!("workbench-{}", body.creation_id);
    let name = body.name.trim();
    if body.creation_id.is_empty()
        || !is_valid_client_conversation_id(&conversation_id)
        || name.is_empty()
        || name.chars().count() > 80
        || name.chars().any(char::is_control)
    {
        return Err(AgentStoreError::Invalid(
            "Invalid workbench name or creation id",
        ));
    }
    Ok(AiAgentWorkbenchView {
        workbench_id: derive_agent_conversation_key(
            actor,
            AgentConversationScope::Personal,
            &conversation_id,
        ),
        conversation_id,
        name: name.into(),
    })
}

#[inline(never)]
pub fn initial_state(
    actor: &str,
    view: &AiAgentWorkbenchView,
    now: &str,
) -> Result<(String, i64, i64), AgentStoreError> {
    let mut root = PersistedAgentSession::new_workbench_coordinator(
        actor,
        &view.workbench_id,
        None,
        PERSONAL_AGENT_POLICY_REVISION,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        now,
    )
    .map_err(AgentStoreError::Invalid)?;
    root.adopt_client_metadata(Some(&view.conversation_id), AgentSessionSurface::AiAgent);
    let state_json = root
        .encode_json_for_storage()
        .map_err(|_| AgentStoreError::Invalid("Invalid coordinator state"))?;
    Ok((state_json, root.version, root.lease_token as i64))
}

/// Only initialize a fresh queue identity. A previously submitted request must
/// keep its original execution role even if its native session has not started.
pub async fn initialize_roster_on(
    txn: &impl ConnectionTrait,
    actor: &str,
    workbench_id: &str,
) -> Result<(), AgentStoreError> {
    if request::Entity::find()
        .filter(request::Column::ActorId.eq(actor))
        .filter(request::Column::ConversationId.eq(workbench_id))
        .one(txn)
        .await?
        .is_some()
    {
        return Err(AgentStoreError::Inaccessible);
    }
    device_roster::replace(txn, actor, workbench_id, 0, vec![]).await?;
    Ok(())
}

pub fn view(
    actor: &str,
    workbench_id: &str,
    state_json: &str,
    name: Option<String>,
    version: i64,
    lease_token: i64,
) -> Option<AiAgentWorkbenchView> {
    let root = decode_workbench_coordinator(state_json, actor, workbench_id).ok()?;
    if root.surface != AgentSessionSurface::AiAgent
        || !root.device_id.is_empty()
        || root.version != version
        || i64::try_from(root.lease_token).ok() != Some(lease_token)
    {
        return None;
    }
    let conversation_id = root.client_conversation_id?;
    if !is_valid_client_conversation_id(&conversation_id)
        || derive_agent_conversation_key(actor, AgentConversationScope::Personal, &conversation_id)
            != workbench_id
    {
        return None;
    }
    Some(AiAgentWorkbenchView {
        workbench_id: workbench_id.into(),
        conversation_id,
        name: name.filter(|name| !name.is_empty())?,
    })
}
