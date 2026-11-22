//! A model can expand an exact retained evidence handle it actually observed.
//! The owner resolution route supplies formatting only after model authorization.

use super::{
    AgentStoreError,
    conversation_search::{now_ms, validate_final_time},
    memory_view::{self, MemoryReadAuthorization},
    retrieval_native::NativeAgentDataFence,
    retrieval_result::{self, RetrievalResultInput},
    unsigned,
};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use crate::entity::{agent_memory as memory, agent_memory_source as evidence};
use desk_agent_protocol::ai_agent::{
    history::AgentDeviceScope,
    memory::MemoryUsePurpose,
    source::{AgentSourceResolution, HistoryDependency, SourceAvailability, SourceReadPurpose},
};
use desk_diagnose_core::{
    ai_agent::{
        data_access::agent_scope,
        data_control::CurrentMemoryUse,
        memory_access::usable_memory_scope,
        retrieval_response::ToolReplyBudget,
        seam::{AgentDataCall, AgentDataRead, AgentDataReadUse, AgentReadRoot},
    },
    native_ids::{self, Kind},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};
use std::collections::BTreeMap;

pub(crate) async fn read(
    db: &DatabaseConnection,
    context: AgentDataCall<'_>,
    source_id: &str,
) -> Result<AgentDataRead<AgentSourceResolution>, AgentStoreError> {
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| read_on(&txn, context, source_id)).await {
        Ok(result) => {
            txn.commit().await?;
            Ok(result)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn read_on(
    db: &DatabaseTransaction,
    context: AgentDataCall<'_>,
    source_id: &str,
) -> Result<AgentDataRead<AgentSourceResolution>, AgentStoreError> {
    let now = now_ms()?;
    let context = AgentDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence =
        desk_diagnose_core::future::boxed(|| super::retrieval_native::prepare_on(db, context, now))
            .await?;
    if let Some(result) = desk_diagnose_core::future::boxed(|| {
        retrieval_result::replay_on::<_, AgentSourceResolution>(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            SourceReadPurpose::MemoryEvidence,
            now,
        )
    })
    .await?
    {
        ToolReplyBudget::new(
            &context.tool_call.id,
            fence.original.maximum_response_bytes(),
        )
        .map_err(AgentStoreError::Invalid)?
        .encode(&result.result)
        .map_err(AgentStoreError::Invalid)?;
        let deadline = fence.finish_on(now_ms()?).await?;
        validate_final_time(&result, deadline, now_ms()?)?;
        return Ok(result);
    }
    let selected =
        desk_diagnose_core::future::boxed(|| source_on(db, &fence, source_id, now)).await?;
    let result = super::owner_source::memory_source_on(
        db,
        &context.session.actor_id,
        source_id,
        &fence.policy,
        now,
    )
    .await?;
    if result.source_id != selected.source.source_id()
        || result.revision != unsigned(selected.source.memory_revision)?
        || result.content.as_deref() != Some(selected.source.body.as_str())
        || result.availability == SourceAvailability::Unavailable
        || result.recorded_at_unix_ms != Some(unsigned(selected.source.observed_at_ms)?)
        || result.statement_timezone.as_deref() != Some(selected.source.statement_timezone.as_str())
    {
        return Err(AgentStoreError::Inaccessible);
    }
    ToolReplyBudget::new(
        &context.tool_call.id,
        fence.original.maximum_response_bytes(),
    )
    .map_err(AgentStoreError::Invalid)?
    .encode(&result)
    .map_err(AgentStoreError::Invalid)?;
    let source = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(&context.session.actor_id))
        .filter(evidence::Column::Id.eq(desk_agent_data_store::source_rows::memory(source_id)))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)?;
    if source != selected.source {
        return Err(AgentStoreError::Inaccessible);
    }
    let prepared_at = now_ms()?;
    let pending = desk_diagnose_core::future::boxed(|| {
        retrieval_result::prepare_on(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            RetrievalResultInput {
                result,
                purpose: SourceReadPurpose::MemoryEvidence,
                selected: std::slice::from_ref(&selected.usage),
            },
            prepared_at,
        )
    })
    .await?;
    let deadline = fence.finish_on(now_ms()?).await?;
    let stored_at = now_ms()?;
    let result = desk_diagnose_core::future::boxed(|| {
        retrieval_result::store_on(pending, &fence.policy, stored_at)
    })
    .await?;
    validate_final_time(&result, deadline, now_ms()?)?;
    Ok(result)
}

struct SelectedSource {
    source: evidence::Model,
    usage: AgentDataReadUse,
}

async fn source_on(
    db: &DatabaseTransaction,
    fence: &NativeAgentDataFence<'_>,
    source_id: &str,
    now: u64,
) -> Result<SelectedSource, AgentStoreError> {
    // Knowing another source's dependency/decision ID never proves that this
    // exact evidence handle/version was included in the actual producer input.
    let observed = fence
        .observed_version(source_id)
        .ok_or(AgentStoreError::SourceUnavailable)?;
    let actor = &fence.context.session.actor_id;
    let source = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(actor))
        .filter(evidence::Column::Id.eq(desk_agent_data_store::source_rows::memory(source_id)))
        .one(db)
        .await?
        .ok_or(AgentStoreError::SourceUnavailable)?;
    if unsigned(source.memory_revision)? != observed.revision {
        return Err(AgentStoreError::SourceUnavailable);
    }
    let data = fence
        .context
        .session
        .agent_data
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?;
    let scope =
        usable_memory_scope(fence.context.session, now).ok_or(AgentStoreError::Inaccessible)?;
    let devices = memory_view::retrieval_devices_on(db, fence, now).await?;
    let current_scope = agent_scope(fence.context.session).map_err(AgentStoreError::Invalid)?;
    let authority = MemoryReadAuthorization {
        actor_id: actor,
        current_device_ref: &data.source_device_ref,
        agent_scope: &current_scope,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        requested_devices: &devices,
        observation_generation: fence.access.memory_state_generation,
        now_unix_ms: now,
    };
    memory_view::validate_context_on(db, &authority).await?;
    let fact = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&source.memory_id))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)?;
    if !fact.device_ref.is_empty()
        && (!devices.contains(&fact.device_ref)
            || (scope.device_rule == AgentDeviceScope::CurrentDevice
                && !scope.current_device_refs().contains(&fact.device_ref)))
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let reference = CurrentMemoryUse {
        memory_id: source.memory_id.clone(),
        revision: unsigned(source.memory_revision)?,
        purpose: MemoryUsePurpose::HistoricalReference,
    };
    let (all, _, until) = super::model_sources::memory_sources_on(
        db,
        actor,
        &data.source_device_ref,
        &reference,
        now,
    )
    .await?;
    if !all.iter().any(|row| row == &source) {
        return Err(AgentStoreError::Inaccessible);
    }
    let (own, parents, source_until) = super::model_sources::evidence_on(
        db,
        actor,
        &source,
        false,
        fence.context.destination,
        now,
    )
    .await?;
    let mut dependencies = BTreeMap::new();
    for dependency in std::iter::once(own).chain(parents) {
        let key = (dependency.source_id.clone(), dependency.source_revision);
        if let Some(old) = dependencies.insert(key, dependency.clone())
            && old != dependency
        {
            return Err(AgentStoreError::Inaccessible);
        }
    }
    let expiry = [until, source_until]
        .into_iter()
        .flatten()
        .chain(
            dependencies
                .values()
                .filter_map(|source: &HistoryDependency| source.expires_at_unix_ms),
        )
        .min();
    let mut usage = AgentDataReadUse {
        projection_id: native_ids::stable(
            Kind::AgentReadProjection,
            &(
                "memory-evidence-candidate-v1",
                actor,
                fence.context.logical_call_id,
                source_id,
                observed.revision,
            ),
        ),
        source_envelope_id: "agent-memory-evidence".into(),
        purpose: SourceReadPurpose::MemoryEvidence,
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        scope: current_scope,
        policy_revision: fence.access.policy_revision,
        control_generation: fence.access.control_generation,
        memory_state_generation: fence.access.memory_state_generation,
        roots: vec![AgentReadRoot {
            source_id: source_id.into(),
            source_revision: observed.revision,
        }],
        decision_roots: Vec::new(),
        history_dependencies: dependencies.into_values().collect(),
        memories: vec![reference],
        expires_at_unix_ms: expiry,
    };
    usage.expires_at_unix_ms = super::model_sources::validate_session_sources_on(
        db,
        fence.context.session,
        &usage,
        &fence.access,
        fence.context.destination,
        &fence.policy,
        now,
        None,
    )
    .await?;
    Ok(SelectedSource { source, usage })
}
