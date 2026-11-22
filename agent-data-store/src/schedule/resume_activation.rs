//! Enable tasks: owner creation, AI proposal decisions and resumes. Enabling
//! schedules the calendar only and never grants a tool or device permission.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    json,
    session::{
        OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch, TouchColumn, timestamp,
    },
};
use crate::entity::agent_schedule as entity;
use desk_diagnose_core::{
    schedule::{
        SCHEDULE_CALC_VERSION, lifecycle::FailureState, next_after, parse_json,
        validate_publication,
    },
    session::{AgentSessionSurface, ExecutionState, PersistedAgentSession},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// Who decided a pending AI proposal. Both decisions use the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalDecider {
    Owner,
    /// The delegated approval reviewer; its reason reaches the model only
    /// when its provenance can be verified.
    Ai(desk_diagnose_core::proposal_review::provenance::AiDecisionReason),
}

/// `decide_proposal` inside the caller's transaction, for a decision that
/// commits together with its review record.
pub async fn decide_proposal_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: i32,
    schedule_id: &str,
    expected_revision: i64,
    approve: bool,
    decider: &ProposalDecider,
) -> Result<entity::Model, ScheduleStoreError> {
    let task = find_task(txn, owner, schedule_id, expected_revision).await?;
    let Some(conversation) = task.proposal_conversation_id.as_deref() else {
        return Err(ScheduleStoreError::Conflict);
    };
    if task.status != "pending_review" {
        return Err(ScheduleStoreError::Conflict);
    }
    // The decision receipt writes the proposing session; take its
    // conversation lock before the task revision CAS.
    host.lock_conversation(txn, conversation).await?;
    let result = if approve {
        activate_on(host, txn, owner, &task, false).await?
    } else {
        let now = authority_now(txn).await?;
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("deleted".into()),
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        entity::Entity::find_by_id(task.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?
    };
    record_decision_on(host, txn, &result, Some(decider)).await?;
    Ok(result)
}

/// Enable a pending or paused task inside the caller's transaction. A
/// continuation keeps its source requirement locked and must still fit
/// the session retention window. No receipt is written here.
pub async fn activate_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: i32,
    task: &entity::Model,
    resume: bool,
) -> Result<entity::Model, ScheduleStoreError> {
    let continuation = task.kind == "conversation_resume";
    if owner <= 0
        || task.owner_user_id != owner
        || task.status != if resume { "paused" } else { "pending_review" }
        || task.active_run_id.is_some()
        || task.calc_version != SCHEDULE_CALC_VERSION
    {
        return Err(ScheduleStoreError::Conflict);
    }
    super::retrieval_consent(task)?;
    let mut failures: FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if resume {
        failures
            .resume()
            .map_err(|_| ScheduleStoreError::Conflict)?;
    } else if !failures.pause_reasons.is_empty() {
        return Err(ScheduleStoreError::Conflict);
    }
    // Lock order: conversation lock → task row (revision CAS) → session row,
    // the same order deletion and reclaim use.
    if continuation {
        let source = task
            .source_conversation_id
            .as_deref()
            .ok_or(ScheduleStoreError::Invalid)?;
        host.lock_conversation(txn, source).await?;
    }
    let revision = task
        .revision
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let locked = entity::Entity::update_many()
        .set(entity::ActiveModel {
            revision: Set(revision),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    if continuation {
        lock_original_requirement(host, txn, owner, task).await?;
    }
    // PostgreSQL CURRENT_TIMESTAMP is the transaction start time. Use the
    // actual database clock after the locks, so an elapsed deadline rejects.
    let now = authority_now(txn).await?;
    let spec = parse_json(&task.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
    let spec = validate_publication(&spec, now, continuation).map_err(|_| {
        ScheduleStoreError::ToolInput(desk_diagnose_core::schedule::proposal::NO_FUTURE_OCCURRENCE)
    })?;
    if continuation {
        host.check_resume_retention(txn, &spec, now).await?;
    }
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set("active".into()),
            failure_state_json: Set(json(&failures)?),
            spec_json: Set(json(&spec)?),
            next_run_at: Set(next_after(&spec, now).map_err(|_| ScheduleStoreError::Invalid)?),
            recurrence_cursor_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(revision))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    entity::Entity::find_by_id(task.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)
}

pub async fn find_task<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    schedule_id: &str,
    expected_revision: i64,
) -> Result<entity::Model, ScheduleStoreError> {
    let task = entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.revision != expected_revision {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(task)
}

/// Hold the unchanged source input while editing or enabling its continuation.
pub async fn lock_original_requirement<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: i32,
    task: &entity::Model,
) -> Result<(), ScheduleStoreError> {
    let subject = super::continuation_subject(task)?;
    let source = task
        .source_conversation_id
        .as_deref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let required = task
        .requirement_revision
        .filter(|revision| *revision > 0)
        .ok_or(ScheduleStoreError::Invalid)?;
    let actor = owner.to_string();
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: source,
                actor: Some(&actor),
                device: Some(&subject),
            },
            OrgMode::PersonalOnly,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.conversation_id != source
        || session.check_subject(&actor, &subject).is_err()
        || session.check_surface(AgentSessionSurface::AiAgent).is_err()
    {
        return Err(ScheduleStoreError::NotFound);
    }
    if session.input_revision != required as u64
        || matches!(
            session.execution_state,
            ExecutionState::OutcomeUnknown { .. } | ExecutionState::Interrupted { .. }
        )
    {
        return Err(ScheduleStoreError::Conflict);
    }
    // Lock the exact source envelope without advancing its input or version.
    // All input writers update this row. A racing write either wins this CAS
    // or waits until activation commits. Later chat does not cancel approval.
    let locked = host
        .touch_session(
            txn,
            TouchColumn::Version,
            SessionGuard {
                id: Some(row.id),
                conversation: Some(source),
                actor: Some(&actor),
                device: Some(&subject),
                version: Some(row.version),
                lease_token: None,
                state_json: Some(&row.state_json),
                lease_deadline_null: false,
                org: OrgMode::Any,
            },
        )
        .await?;
    if locked != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}

/// Persist the authoritative decision or activation receipt without creating a
/// new user input. A proposal decision goes to its proposing conversation and
/// resumes the turn that paused on it; a continuation's activation goes to its
/// source conversation.
pub async fn record_decision_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    task: &entity::Model,
    decider: Option<&ProposalDecider>,
) -> Result<(), ScheduleStoreError> {
    let now = authority_now(txn).await?;
    let rejected = task.status == "deleted";
    let conversation = match decider {
        Some(_) => task.proposal_conversation_id.as_deref(),
        None => task.source_conversation_id.as_deref(),
    }
    .ok_or(ScheduleStoreError::Invalid)?;
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation,
                actor: None,
                device: None,
            },
            OrgMode::PersonalOnly,
            false,
        )
        .await?;
    let Some(row) = row else {
        // Deleting an orphan proposal must remain possible. The deleted task
        // row is the durable decision even when its transcript no longer exists.
        return if rejected {
            Ok(())
        } else {
            Err(ScheduleStoreError::NotFound)
        };
    };
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    // A proposal's turn paused on it: the decision resumes that requirement.
    let paused = decider.is_some()
        && crate::decision_resume::paused_on(
            &session,
            desk_diagnose_core::proposal_review::DecisionKind::Schedule,
            &task.schedule_id,
        );
    if session.turn_state.is_active() {
        // A proposal's turn pauses only when it ends; deciding before that
        // would leave the pause without a decision, so the caller retries. A
        // creation notice never steals an interactive turn.
        return if rejected && decider.is_none() {
            Ok(())
        } else {
            Err(ScheduleStoreError::Conflict)
        };
    }
    let ai = match decider {
        Some(ProposalDecider::Ai(ai)) => Some(ai),
        _ => None,
    };
    let id = format!("schedule-decision:{}:{}", task.schedule_id, task.revision);
    let text = serde_json::json!({
        "event": if rejected { "scheduled_task_rejected" } else { "scheduled_task_activated" },
        "schedule_id": task.schedule_id, "revision": task.revision, "state": task.status,
        "next_run_at_utc_ms": task.next_run_at,
        "authority": "calendar_only_no_tool_permissions",
        "decided_by": match decider {
            Some(ProposalDecider::Ai(_)) => "approval_ai",
            Some(ProposalDecider::Owner) | None => "owner",
        },
        "reason": ai.map(|ai| &ai.reason),
        "reason_code": ai.map(|ai| &ai.reason_code),
        "instruction": "This server receipt is the final decision. Do not request another chat confirmation. Ordinary chat does not cancel an enabled task. Every run approves its device actions separately."})
    .to_string();
    let last_labelled = session
        .conversation
        .iter()
        .rev()
        .find_map(|message| message.data_envelope.as_ref());
    // A paused turn names the proposing call; an owner decision outside the
    // pause derives from the proposal's pending receipt; a creation or
    // continuation notice keeps the latest label.
    let proposing = if paused {
        desk_diagnose_core::proposal_review::provenance::proposing_output(&session).cloned()
    } else if decider.is_some() {
        pending_proposal_parent(&session, &task.schedule_id)
    } else {
        None
    };
    let mut message = desk_diagnose_core::chat::ChatMessage::system_event(&id, &text);
    desk_diagnose_core::proposal_review::provenance::decision_receipt_envelope(
        &session,
        proposing.as_ref(),
        ai,
        last_labelled,
        &id,
        &text,
        desk_diagnose_core::model_message_labels::ServerResultSource::ScheduleActivation,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?
    .apply(&mut message);
    if paused {
        return crate::decision_resume::record_decision_on(host, txn, &row, session, message, now)
            .await;
    }
    session.conversation.push(message);
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp = timestamp(now)?;
    session.updated_at = timestamp.to_rfc3339();
    let state_json = session
        .encode_json_for_storage()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let changed = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                version: Some(row.version),
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&state_json),
                version: Some(session.version),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// The parent of the pending proposal result for `schedule_id`: the model
/// output that proposed it. Proposal, query and cancellation results share
/// one id prefix, so the label and state identify the proposal.
fn pending_proposal_parent(
    session: &PersistedAgentSession,
    schedule_id: &str,
) -> Option<desk_agent_protocol::data_lineage::DataEnvelope> {
    use desk_diagnose_core::model_message_labels::ServerResultSource;
    let pending = session.conversation.iter().rev().find(|message| {
        message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_tool_name == ServerResultSource::ScheduleProposal.as_str()
        }) && serde_json::from_str::<serde_json::Value>(&message.text).is_ok_and(|value| {
            value["schedule_id"].as_str() == Some(schedule_id) && value["state"] == "pending_review"
        })
    })?;
    let [parent] = pending
        .data_envelope
        .as_ref()?
        .provenance
        .source_envelope_ids
        .as_slice()
    else {
        return None;
    };
    session
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.as_ref())
        .find(|label| &label.envelope_id == parent)
        .cloned()
}
