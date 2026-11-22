//! Native source authorization for the common keyword search engine.

use super::{AgentStoreError, unsigned};
use crate::entity::{agent_history as history, agent_origin as origin};
use desk_agent_data_store::keyword_search::{self as shared, HistorySourceReader};
pub(crate) use desk_agent_data_store::keyword_search::{
    HistorySearchAuthorization, KeywordCandidates, ReadableHistorySource,
};
use desk_agent_protocol::ai_agent::{
    history::ConversationSearchRequest,
    source::{HistoryDependency, SourceReadPurpose, SourceUsePolicy},
};
use desk_diagnose_core::{
    ai_agent::seam::{AgentDataReadUse, AgentReadRoot},
    native_ids::{self, Kind},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::BTreeSet;
#[cfg(test)]
use std::time::Duration;
struct NativeHistoryReader;
#[async_trait::async_trait]
impl<C: ConnectionTrait + crate::config::ConfigConnection> HistorySourceReader<C>
    for NativeHistoryReader
{
    async fn validate_context_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
    ) -> Result<(), AgentStoreError> {
        validate_context_on(db, context).await
    }
    async fn readable_history_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
        source_id: &str,
        revision: u64,
    ) -> Result<Box<ReadableHistorySource>, AgentStoreError> {
        readable_history_on(db, context, source_id, revision).await
    }
}
pub(crate) async fn relevance_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
) -> Result<KeywordCandidates, AgentStoreError> {
    shared::relevance_on::<C, NativeHistoryReader>(db, context, request, owner_devices).await
}
#[cfg(test)]
pub(super) async fn scan_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
    time_budget: Duration,
) -> Result<KeywordCandidates, AgentStoreError> {
    shared::scan_on::<C, NativeHistoryReader>(db, context, request, owner_devices, time_budget)
        .await
}

/// No model output label is created here. This reusable read builds the complete
/// actual dependency closure and passes the existing model-purpose source gate.
pub(crate) async fn readable_history_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    source_id: &str,
    revision: u64,
) -> Result<Box<ReadableHistorySource>, AgentStoreError> {
    validate_context_on(db, context).await?;
    let row = history::Entity::find()
        .filter(history::Column::ActorId.eq(context.actor_id))
        .filter(history::Column::Id.eq(desk_agent_data_store::source_rows::history(source_id)))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)?;
    if unsigned(row.revision)? != revision
        || row.admitted_generation <= 0
        || unsigned(row.admitted_generation)? > context.corpus_generation
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let original = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(context.actor_id))
        .filter(origin::Column::Id.eq(row.origin_id))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)?;
    if !super::source_lifecycle::origin_readable_on(db, context.actor_id, &original).await? {
        return Err(AgentStoreError::Inaccessible);
    }
    let policy: SourceUsePolicy =
        serde_json::from_str(&original.policy_json).map_err(|_| AgentStoreError::Inaccessible)?;
    let own = HistoryDependency {
        source_id: row.source_id().clone(),
        source_revision: revision,
        origin_id: row.origin_id,
        origin_deletion_generation: unsigned(original.deletion_generation)?,
        policy_revision: policy.policy_revision,
        expires_at_unix_ms: Some(unsigned(row.expires_at_ms)?),
    };
    let mut dependencies = super::model_sources::decode_dependencies(&row.dependencies_json)?;
    if dependencies.iter().any(|source| {
        source.source_id == own.source_id && source.source_revision == own.source_revision
    }) {
        return Err(AgentStoreError::Inaccessible);
    }
    dependencies.push(own);
    let expiry = dependencies
        .iter()
        .filter_map(|source| source.expires_at_unix_ms)
        .min();
    let mut usage = AgentDataReadUse {
        decision_roots: Vec::new(),
        projection_id: native_ids::stable(
            Kind::AgentReadProjection,
            &(
                "history-candidate-v1",
                context.actor_id,
                &context.scope.root_conversation_id,
                &context.scope.retrieval_episode_id,
                source_id,
                revision,
            ),
        ),
        source_envelope_id: "agent-history-candidate".into(),
        purpose: SourceReadPurpose::ModelHistory,
        retrieval_episode_id: context.scope.retrieval_episode_id.clone(),
        scope: Box::new(context.agent_scope.clone()),
        policy_revision: context.access.policy_revision,
        control_generation: context.access.control_generation,
        memory_state_generation: context.access.memory_state_generation,
        roots: vec![AgentReadRoot {
            source_id: source_id.into(),
            source_revision: revision,
        }],
        history_dependencies: dependencies,
        memories: Vec::new(),
        expires_at_unix_ms: expiry,
    };
    let expiry = super::model_sources::validate_on(
        db,
        context.actor_id,
        context.current_device_ref,
        &usage,
        context.access,
        Some(context.scope),
        context.destination,
        context.platform,
        context.now_unix_ms,
    )
    .await?;
    usage.expires_at_unix_ms = expiry;
    Ok(Box::new(ReadableHistorySource { row, usage }))
}

pub(super) async fn validate_context_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
) -> Result<(), AgentStoreError> {
    context
        .access
        .validate()
        .map_err(AgentStoreError::Invalid)?;
    context
        .platform
        .validate()
        .map_err(AgentStoreError::Invalid)?;
    context.scope.validate().map_err(AgentStoreError::Invalid)?;
    if context.actor_id != context.scope.actor_id
        || context.current_device_ref != context.scope.current_device_ref
        || !context.access.settings.model_history_search_enabled
        || context.access.policy_revision != context.platform.revision
    {
        return Err(AgentStoreError::Inaccessible);
    }
    if context.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AgentStoreError::Inaccessible);
    }
    let runtime = super::reconcile_settings_on(db, context.actor_id).await?;
    let config = db.config_read().await;
    if unsigned(runtime.memory_state_generation)? != context.access.memory_state_generation
        || unsigned(runtime.control_generation)? != context.access.control_generation
        || unsigned(runtime.history_data_generation)? < context.corpus_generation
        || config.agent_data_settings != context.access.settings
    {
        return Err(AgentStoreError::Inaccessible);
    }
    Ok(())
}
