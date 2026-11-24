//! Direct read admission inside the original physical model-start transaction.

use super::{AgentStoreError, unsigned};
use crate::{
    config::{ConfigConnection, connection::DatabaseTransaction},
    entity::{agent_goal_run, agent_session, model_egress_receipt as native},
};
use desk_agent_protocol::ai_agent::user_instructions::AgentDataPolicy;
use desk_diagnose_core::{
    ai_agent::{
        data_access::{AgentDataToolAccess, retained_context_eligible},
        native_export::{AgentDataExportAuthorization, NativeAgentModelCall},
    },
    seam::ModelRequest,
    session::{PersistedAgentSession, TurnState},
};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};

pub(crate) mod output;

pub(crate) struct PreparedModelData {
    row: agent_session::Model,
    session: Box<PersistedAgentSession>,
    policy: AgentDataPolicy,
}

/// Hold the actual config snapshot and SQLite writer fence through dispatch.
/// The final read below is separate: one transaction is not one read snapshot.
pub(crate) async fn prepare_on(
    db: &DatabaseTransaction,
    request: &ModelRequest,
) -> Result<Option<Box<PreparedModelData>>, AgentStoreError> {
    let Some(manifest) = request.agent_data.as_ref() else {
        desk_diagnose_core::ai_agent::seam::AgentModelDataManifest::validate_absence(request)
            .map_err(AgentStoreError::Invalid)?;
        return Ok(None);
    };
    // Main agents also use delegation reservations for budget accounting.
    // The persisted role gates data access; native dispatch validates the budget.
    if request.context_conversation_id.as_deref() != Some(manifest.conversation_id.as_str()) {
        return Err(AgentStoreError::Inaccessible);
    }
    if manifest.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AgentStoreError::Inaccessible);
    }
    super::ensure_runtime_on(db, &manifest.actor_id).await?;
    let config = db.config_read().await;
    let policy = config.agent_data_policy.clone();
    let row = session_on(db, request).await?;
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AgentStoreError::Invalid("invalid agent source session"))?,
    );
    validate_session(
        &row,
        &session,
        request,
        jiff::Timestamp::now().as_millisecond() as u64,
    )?;
    let data = session
        .agent_data
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?;
    let binding =
        crate::agent_data_store::source_device_ref(&config.metadata.instance, &session.device_id);
    if data.source_device_ref != binding
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let now = jiff::Timestamp::now().as_millisecond();
    crate::issue_connections::check_work_on(db, &session, false, now).await?;
    task_deadline_on(db, &session, now as u64).await?;
    Ok(Some(Box::new(PreparedModelData {
        row,
        session,
        policy,
    })))
}

/// This runs after all original native business writes and before their commit.
/// Any error requires the caller to roll back the whole dispatch transaction.
pub(crate) fn authorize_on<'a>(
    db: &'a DatabaseTransaction,
    prepared: Option<&'a PreparedModelData>,
    request: &'a ModelRequest,
    physical_call_id: &'a str,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<u64>, AgentStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let Some(prepared) = prepared else {
            if request.agent_data.is_some() {
                return Err(AgentStoreError::Inaccessible);
            }
            return Ok(None);
        };
        let manifest = request
            .agent_data
            .as_ref()
            .ok_or(AgentStoreError::Inaccessible)?;
        let row = session_on(db, request).await?;
        if row.state_json != prepared.row.state_json
            || row.version != prepared.row.version
            || row.lease_token != prepared.row.lease_token
        {
            return Err(AgentStoreError::Inaccessible);
        }
        let now = jiff::Timestamp::now().as_millisecond() as u64;
        validate_session(&row, &prepared.session, request, now)?;
        let runtime = super::reconcile_settings_on(db, &manifest.actor_id).await?;
        let config = db.config_read().await;
        let access = AgentDataToolAccess {
            settings: config.agent_data_settings.clone(),
            policy_revision: prepared.policy.revision,
            memory_state_generation: unsigned(runtime.memory_state_generation)?,
            control_generation: unsigned(runtime.control_generation)?,
        };
        access.validate().map_err(AgentStoreError::Invalid)?;
        if access.policy_revision != manifest.policy_revision
            || access.control_generation != manifest.control_generation
            || access.memory_state_generation != manifest.memory_state_generation
        {
            return Err(AgentStoreError::Inaccessible);
        }
        let native = native::Entity::find_by_id(physical_call_id)
            .one(db)
            .await?
            .ok_or(AgentStoreError::Inaccessible)?;
        if native.state != crate::model_egress_store::STATE_DISPATCH_INTENT
            || native.completed_at.is_some()
            || native.context_conversation_id.as_deref() != Some(manifest.conversation_id.as_str())
        {
            return Err(AgentStoreError::Inaccessible);
        }
        let (audit, inputs, input_snapshot) = crate::model_egress_store::receipt_inputs(&native)?;
        desk_diagnose_core::future::boxed(|| {
            desk_agent_data_store::issue_events::supplements::verify_model_material_on(
                db,
                &prepared.session,
                &audit.destination,
                &input_snapshot.envelopes,
                now as i64,
                false,
            )
        })
        .await?;
        let started = unsigned(native.authorized_at.timestamp_millis())?;
        if started > now {
            return Err(AgentStoreError::Inaccessible);
        }
        let mut deadline = unsigned(
            row.lease_deadline
                .ok_or(AgentStoreError::Inaccessible)?
                .timestamp_millis(),
        )?;
        deadline = deadline.min(
            started
                .checked_add(desk_diagnose_core::ai_agent::native_export::MAX_NATIVE_EXPORT_TTL_MS)
                .ok_or(AgentStoreError::Inaccessible)?,
        );
        if let Some(until) = task_deadline_on(db, &prepared.session, now).await? {
            deadline = deadline.min(until);
        }
        if let Some(until) = desk_diagnose_core::future::boxed(|| {
            super::prior_outputs::validate_on(db, &prepared.session, &input_snapshot, now)
        })
        .await?
        {
            deadline = deadline.min(until);
        }
        let mut grants = Vec::with_capacity(manifest.read_projection_ids.len());
        for id in &manifest.read_projection_ids {
            let source = super::projection::load_on(db, &manifest.actor_id, id)
                .await?
                .ok_or(AgentStoreError::Inaccessible)?;
            source
                .proof
                .validate_direct_selection(&prepared.session, request, &inputs)
                .map_err(AgentStoreError::Invalid)?;
            if source.proof.kind
                == desk_diagnose_core::ai_agent::read_projection::ReadProjectionKind::Tool
            {
                let producer = prepared
                    .session
                    .conversation
                    .iter()
                    .find(|message| {
                        Some(message.message_id.as_str())
                            == source.proof.producer_message_id.as_deref()
                    })
                    .ok_or(AgentStoreError::Inaccessible)?;
                let output =
                    desk_diagnose_core::model_egress::model_output_message_envelope(producer)
                        .map_err(|_| AgentStoreError::Inaccessible)?;
                crate::model_egress_store::SignalModelEgressStore::read_context_output_on(
                    db,
                    &manifest.conversation_id,
                    output,
                )
                .await?;
            }
            if let Some(until) = super::model_sources::validate_session_sources_on(
                db,
                &prepared.session,
                &source.proof.usage,
                &access,
                &audit.destination,
                &prepared.policy,
                now,
                None,
            )
            .await?
            {
                deadline = deadline.min(until);
            }
            let call = NativeAgentModelCall {
                physical_call_id: physical_call_id.into(),
                model_export_authorization_id: native.export_authorization_id.clone(),
                destination: audit.destination.clone(),
                authority_expires_at_unix_ms: Some(deadline),
            };
            let grant = AgentDataExportAuthorization::for_projection(
                &prepared.session,
                manifest,
                &source.proof,
                call,
                source.row.first_consumed_at_ms.map(unsigned).transpose()?,
                started,
            )
            .map_err(AgentStoreError::Invalid)?;
            deadline = deadline.min(grant.expires_at_unix_ms);
            grants.push(grant);
        }
        if deadline <= jiff::Timestamp::now().as_millisecond() as u64 {
            return Err(AgentStoreError::Inaccessible);
        }
        let call = NativeAgentModelCall {
            physical_call_id: physical_call_id.into(),
            model_export_authorization_id: native.export_authorization_id.clone(),
            destination: audit.destination.clone(),
            authority_expires_at_unix_ms: Some(deadline),
        };
        let set = desk_diagnose_core::ai_agent::native_export::AgentNativeSourceSet::for_request(
            &prepared.session,
            manifest,
            &call,
            &grants,
            started,
        )
        .map_err(AgentStoreError::Invalid)?;
        desk_diagnose_core::future::boxed(|| {
            super::native_export::record_sealed_call_on(db, &set, &grants, &prepared.policy)
        })
        .await?;
        if deadline <= jiff::Timestamp::now().as_millisecond() as u64 {
            return Err(AgentStoreError::Inaccessible);
        }
        Ok(Some(deadline))
    })
}

async fn session_on(
    db: &DatabaseTransaction,
    request: &ModelRequest,
) -> Result<agent_session::Model, AgentStoreError> {
    let manifest = request
        .agent_data
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?;
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&manifest.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&manifest.actor_id))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)
}

fn validate_session(
    row: &agent_session::Model,
    session: &PersistedAgentSession,
    request: &ModelRequest,
    now: u64,
) -> Result<(), AgentStoreError> {
    if !retained_context_eligible(session)
        || session.turn_state != TurnState::Running
        || row.actor_id != session.actor_id
        || row.device_id != session.device_id
        || row.conversation_id != session.conversation_id
        || row.version != session.version
        || unsigned(row.lease_token)? != session.lease_token
        || row
            .lease_deadline
            .is_none_or(|until| until.timestamp_millis() <= now as i64)
    {
        return Err(AgentStoreError::Inaccessible);
    }
    request
        .agent_data
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?
        .validate_for(session, request)
        .map_err(AgentStoreError::Invalid)
}

pub(super) async fn task_deadline_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    now: u64,
) -> Result<Option<u64>, AgentStoreError> {
    let Some(task_id) = session
        .agent_data
        .as_ref()
        .and_then(|data| data.retrieval_task_id.as_ref())
    else {
        return Ok(None);
    };
    // A child request's owner data lives no longer than the request itself.
    if let Some(binding) = session.agent_role.binding() {
        if binding.task_id != *task_id {
            return Err(AgentStoreError::Inaccessible);
        }
        return u64::try_from(binding.deadline_ms)
            .map(Some)
            .map_err(|_| AgentStoreError::Invalid("invalid child request deadline"));
    }
    if matches!(
        session.trigger_origin,
        desk_diagnose_core::session::TriggerOrigin::ScheduledTask
            | desk_diagnose_core::session::TriggerOrigin::ScheduledContinuation
    ) {
        return desk_diagnose_core::future::send_phase(|| {
            crate::schedule_store::ScheduleStore::agent_retrieval_deadline_on(db, session)
        })
        .await
        .map(Some)
        .map_err(AgentStoreError::Backend);
    }
    let row = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::GoalId.eq(task_id))
        .filter(agent_goal_run::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_goal_run::Column::ActorId.eq(&session.actor_id))
        .filter(agent_goal_run::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or(AgentStoreError::Inaccessible)?;
    let goal = crate::agent_goal_store::decode(&row)?;
    let authority =
        desk_diagnose_core::ai_agent::retrieval_task::goal_authority(session, &goal, now)
            .map_err(AgentStoreError::Invalid)?;
    let deadline = authority
        .native_deadline(row.lease_owner.as_deref(), row.lease_deadline, now)
        .map_err(AgentStoreError::Invalid)?;
    Ok(Some(deadline))
}

pub(crate) fn admission_error(
    error: AgentStoreError,
) -> desk_diagnose_core::schedule::model_admission::ModelAdmissionError<DbErr> {
    use desk_diagnose_core::schedule::model_admission::ModelAdmissionError;
    match error {
        AgentStoreError::Backend(error) => ModelAdmissionError::Backend(error),
        AgentStoreError::Capacity => ModelAdmissionError::Backend(DbErr::Custom(
            "agent source metadata capacity reached".into(),
        )),
        _ => ModelAdmissionError::AgentDataChanged,
    }
}
