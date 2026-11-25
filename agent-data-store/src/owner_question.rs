//! Indexed deadlines and first-writer-wins settlement inside a host transaction.

use crate::{
    decision_resume,
    entity::agent_owner_question as wait,
    schedule::{
        ScheduleStoreError,
        session::{ScheduleHostStores, SessionGuard, SessionPatch, SessionRow, timestamp},
    },
};
use desk_agent_protocol::ai_agent::owner_question::{
    OwnerPrompt, OwnerQuestionAction, OwnerQuestionResult,
};
use desk_diagnose_core::{
    model_message_labels::model_bound_user_message, owner_question::insert_result,
    proposal_review::DecisionKind, session::PersistedAgentSession,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::{Expr, OnConflict},
};

const OPEN: &str = "open";

pub async fn publish_on<C: ConnectionTrait>(
    txn: &C,
    session: &PersistedAgentSession,
) -> Result<(), DbErr> {
    let Some(question) = session.owner_question.as_deref().filter(|w| {
        w.open
            && session
                .decision_pause
                .as_ref()
                .is_some_and(|p| p.is(DecisionKind::OwnerQuestion, &w.view.wait_id))
    }) else {
        return Ok(());
    };
    wait::Entity::insert(wait::ActiveModel {
        wait_id: Set(question.view.wait_id.clone()),
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        deadline_ms: Set(question.view.deadline_unix_ms.map(|n| n as i64)),
        status: Set(OPEN.into()),
        client_request_id: Set(None),
    })
    .on_conflict(
        OnConflict::column(wait::Column::WaitId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(txn)
    .await?;
    Ok(())
}

pub async fn due_on(
    db: &impl ConnectionTrait,
    now: i64,
    limit: u64,
) -> Result<Vec<wait::Model>, DbErr> {
    due_after_on(db, now, limit, None).await
}

pub async fn due_after_on(
    db: &impl ConnectionTrait,
    now: i64,
    limit: u64,
    cursor: Option<&(i64, String)>,
) -> Result<Vec<wait::Model>, DbErr> {
    let mut query = wait::Entity::find();
    if let Some((ms, id)) = cursor {
        query = query.filter(
            sea_orm::Condition::any()
                .add(wait::Column::DeadlineMs.gt(*ms))
                .add(
                    sea_orm::Condition::all()
                        .add(wait::Column::DeadlineMs.eq(*ms))
                        .add(wait::Column::WaitId.gt(id)),
                ),
        );
    }
    query
        .filter(wait::Column::Status.eq(OPEN))
        .filter(wait::Column::DeadlineMs.lte(now))
        .order_by_asc(wait::Column::DeadlineMs)
        .order_by_asc(wait::Column::WaitId)
        .limit(limit)
        .all(db)
        .await
}

pub async fn withdraw_on(db: &impl ConnectionTrait, id: &str) -> Result<(), DbErr> {
    wait::Entity::update_many()
        .col_expr(wait::Column::Status, Expr::value("withdrawn"))
        .filter(wait::Column::WaitId.eq(id))
        .filter(wait::Column::Status.eq(OPEN))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn settle_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    row: &SessionRow<H::Row>,
    id: &str,
    submission: Option<(&str, &OwnerQuestionAction)>,
    now: i64,
) -> Result<String, ScheduleStoreError> {
    use ScheduleStoreError::{Conflict, Invalid};
    let stored = wait::Entity::find_by_id(id)
        .filter(wait::Column::ActorId.eq(&row.actor_id))
        .filter(wait::Column::ConversationId.eq(&row.conversation_id))
        .one(txn)
        .await?
        .ok_or(Conflict)?;
    if stored.status != OPEN {
        if stored.status == "timed_out"
            || stored
                .client_request_id
                .as_deref()
                .zip(submission)
                .is_some_and(|(a, (b, _))| a == b)
        {
            return Ok(stored.status);
        }
        return Err(Conflict);
    }
    let mut session =
        PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| Invalid)?;
    if !decision_resume::paused_on(&session, DecisionKind::OwnerQuestion, id)
        || session.main_stopped
        || row.lease_deadline_ms.is_some()
    {
        return Err(Conflict);
    }
    let question = session
        .owner_question
        .as_deref()
        .filter(|w| w.view.wait_id == id && w.open)
        .ok_or(Conflict)?;
    let expired = stored.deadline_ms.is_some_and(|d| now >= d);
    let client_id = if expired {
        None
    } else {
        Some(submission.ok_or(Conflict)?.0.to_owned())
    };
    let action = submission.filter(|_| !expired).map(|(_, a)| a);
    let issuing = session
        .conversation
        .iter()
        .find(|m| m.message_id == question.issuing_message_id)
        .ok_or(Conflict)?;
    let label_owner = |text: String| {
        let destination = issuing
            .data_envelope
            .as_ref()
            .and_then(|e| {
                e.allowed_destinations.iter().find(|d| {
                    matches!(
                        d,
                        desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
                    )
                })
            })
            .ok_or(Invalid)?;
        model_bound_user_message(format!("owner-response-{id}"), text, destination.clone())
            .map_err(|_| Invalid)
    };
    let status = match &question.view.prompt {
        OwnerPrompt::Questions { .. } => {
            let result = match action {
                Some(a) => question.resolve(a.clone()).map_err(|_| Invalid)?,
                None => OwnerQuestionResult::timed_out(),
            };
            let (status, label) = match &result {
                OwnerQuestionResult::Answered { .. } => (
                    "answered",
                    label_owner(serde_json::to_string(&result).map_err(|_| Invalid)?)?
                        .data_envelope,
                ),
                OwnerQuestionResult::Declined => ("declined", None),
                OwnerQuestionResult::TimedOut { .. } => ("timed_out", None),
            };
            insert_result(&mut session, &result, label.as_ref()).map_err(|_| Invalid)?;
            status
        }
        OwnerPrompt::FollowUp { .. } => match action {
            Some(OwnerQuestionAction::Reply { text }) if question.accepts_reply(text) => {
                let message = label_owner(text.clone())?;
                if question.goal.is_some() {
                    session.conversation.push(message);
                } else {
                    let owner = message.data_envelope.ok_or(Invalid)?;
                    let content = desk_diagnose_core::owner_question::owner_content(
                        &session,
                        id,
                        text.clone(),
                        owner.allowed_destinations,
                    )
                    .map_err(|_| Invalid)?;
                    let reply = question_event(issuing, id, &content)?;
                    session.conversation.push(reply);
                }
                "replied"
            }
            None => {
                let content =
                    desk_diagnose_core::owner_question::timeout_content().map_err(|_| Invalid)?;
                let event = question_event(issuing, id, &content)?;
                session.conversation.push(event);
                "timed_out"
            }
            _ => return Err(Invalid),
        },
    };
    let question = session.owner_question.as_deref_mut().ok_or(Conflict)?;
    question.open = false;
    question.needs_adjustment = status == "declined";
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or(Invalid)?;
    session.version = row.version.checked_add(1).ok_or(Invalid)?;
    session.updated_at = timestamp(now)?.to_rfc3339();
    let json = session.encode_json_for_storage().map_err(|_| Invalid)?;
    let count = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                actor: Some(&row.actor_id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                state_json: Some(&row.state_json),
                lease_deadline_null: true,
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&json),
                version: Some(session.version),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if count != 1 {
        return Err(Conflict);
    }
    let changed = wait::Entity::update_many()
        .col_expr(wait::Column::Status, Expr::value(status))
        .col_expr(wait::Column::ClientRequestId, Expr::value(client_id))
        .filter(wait::Column::WaitId.eq(id))
        .filter(wait::Column::Status.eq(OPEN))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(Conflict);
    }
    decision_resume::insert_on(
        txn,
        &session,
        &row.device_id,
        DecisionKind::OwnerQuestion,
        id,
        now,
    )
    .await?;
    Ok(status.into())
}

fn question_event(
    issuing: &desk_diagnose_core::chat::ChatMessage,
    id: &str,
    content: &desk_diagnose_core::model_content::ModelContent,
) -> Result<desk_diagnose_core::chat::ChatMessage, ScheduleStoreError> {
    use desk_diagnose_core::{
        chat::{ChatMessage, ChatRole},
        model_content::Labelling,
        model_message_labels::ServerResultSource,
    };
    ChatMessage::from_content(
        ChatRole::SystemEvent,
        format!("question-reply-{id}"),
        content,
        Labelling {
            issuing: issuing.data_envelope.as_ref(),
            call_id: id,
            source: ServerResultSource::OwnerQuestion,
            request_only: false,
        },
    )
    .map_err(|_| ScheduleStoreError::Invalid)
}

/// Requeue a claim lost before its first assistant response. Native actions or
/// published model output continue through their own recovery paths.
pub async fn recover_unpublished_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    record: &crate::entity::agent_decision_resume::Model,
    now: i64,
) -> Result<bool, ScheduleStoreError> {
    use crate::schedule::session::{OrgMode, SessionKey};
    host.lock_conversation(txn, &record.conversation_id).await?;
    let Some(row) = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &record.conversation_id,
                actor: Some(&record.actor_id),
                device: Some(&record.device_id),
            },
            OrgMode::PersonalOnly,
            true,
        )
        .await?
    else {
        return Ok(false);
    };
    if row.lease_deadline_ms.is_some_and(|d| d > now) {
        return Ok(true);
    }
    let mut session = PersistedAgentSession::decode_boxed_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let Some(wait) = session
        .owner_question
        .as_mut()
        .filter(|w| w.view.wait_id == record.request_id)
    else {
        return Ok(false);
    };
    let Some(pause) = wait.resumed_from.take() else {
        return Ok(false);
    };
    if session.main_stopped
        || session.decision_pause.is_some()
        || session.input_revision != pause.input_revision
        || session.trigger_origin != desk_diagnose_core::session::TriggerOrigin::ProposalDecision
        || !session.turn_state.is_active()
        || !session.execution_state.states().is_empty()
        || session.conversation.iter().any(|m| {
            m.turn_id == session.current_turn_id
                && m.role == desk_diagnose_core::chat::ChatRole::Assistant
        })
    {
        return Ok(false);
    }
    session.decision_pause = Some(pause);
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        timestamp(now)?.to_rfc3339(),
    );
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let json = session
        .encode_json_for_storage()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let count = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                actor: Some(&row.actor_id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                state_json: Some(&row.state_json),
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&json),
                version: Some(session.version),
                lease_deadline_ms: Some(None),
                turn_state: Some("idle"),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if count != 1
        || !decision_resume::transition_on(
            txn,
            record.id,
            decision_resume::STARTED,
            decision_resume::PENDING,
            now,
        )
        .await?
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(true)
}

pub async fn withdraw_goal_on(
    txn: &impl ConnectionTrait,
    session: &mut PersistedAgentSession,
    goal_id: &str,
    now: i64,
) -> Result<(), DbErr> {
    if session.owner_question.as_ref().is_some_and(|w| {
        w.goal.as_ref().is_some_and(|g| g.goal_id == goal_id)
            && session
                .decision_pause
                .as_ref()
                .is_some_and(|p| p.is(DecisionKind::OwnerQuestion, &w.view.wait_id))
    }) && let Some(pause) = desk_diagnose_core::proposal_review::withdraw_pause(session)
    {
        withdraw_on(txn, &pause.id).await?;
        decision_resume::supersede_conversation_on(txn, &session.conversation_id, now)
            .await
            .map_err(|e| DbErr::Custom(format!("withdraw owner question: {e:?}")))?;
    }
    Ok(())
}
