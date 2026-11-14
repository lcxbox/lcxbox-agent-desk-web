//! Requests a turn paused on: schedule and skill proposals and interactive
//! directory requests. The deciding transaction appends the decision event to
//! the paused session and records a resume; the host's decision-resume
//! executor claims the record and resumes the original requirement with
//! `TriggerOrigin::ProposalDecision`. A record leaves `pending` exactly once.

use crate::entity::agent_decision_resume as resume;
use crate::schedule::{
    ScheduleStoreError,
    session::{ScheduleHostStores, SessionGuard, SessionPatch, SessionRow, timestamp},
};
use desk_diagnose_core::{
    chat::ChatMessage,
    proposal_review::{DecisionKind, current_pause},
    session::PersistedAgentSession,
};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, sea_query::Expr,
};

pub const PENDING: &str = "pending";
pub const STARTED: &str = "started";
pub const DONE: &str = "done";
pub const SUPERSEDED: &str = "superseded";

/// Whether `session` ended its turn paused on exactly this request in its
/// current input. Decisions are written and AI review runs only then.
pub fn paused_on(session: &PersistedAgentSession, kind: DecisionKind, id: &str) -> bool {
    !session.turn_state.is_active() && current_pause(session, kind, id).is_some()
}

/// Appends `event` to the paused `session` under its row version and records
/// the resume of the request it paused on, in the deciding transaction. The
/// caller checked [`paused_on`].
pub async fn record_decision_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    row: &SessionRow<H::Row>,
    mut session: PersistedAgentSession,
    event: ChatMessage,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    let pause = session
        .decision_pause
        .clone()
        .ok_or(ScheduleStoreError::Conflict)?;
    session.conversation.push(event);
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.updated_at = timestamp(now_ms)?.to_rfc3339();
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
                updated_at_ms: Some(now_ms),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    insert_on(txn, &session, &row.device_id, pause.kind, &pause.id, now_ms).await
}

/// Records the resume of `session`'s paused turn, in the transaction that
/// wrote the decision into it.
pub async fn insert_on<C: ConnectionTrait>(
    txn: &C,
    session: &PersistedAgentSession,
    device_id: &str,
    kind: DecisionKind,
    request_id: &str,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    resume::Entity::insert(resume::ActiveModel {
        kind: Set(kind.as_str().into()),
        request_id: Set(request_id.into()),
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(device_id.into()),
        input_revision: Set(
            i64::try_from(session.input_revision).map_err(|_| ScheduleStoreError::Invalid)?
        ),
        state: Set(PENDING.into()),
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
        ..Default::default()
    })
    .exec_without_returning(txn)
    .await?;
    Ok(())
}

/// Pending records after `after_id`, oldest first.
pub async fn pending_on<C: ConnectionTrait>(
    db: &C,
    after_id: i64,
    limit: u64,
) -> Result<Vec<resume::Model>, ScheduleStoreError> {
    Ok(resume::Entity::find()
        .filter(resume::Column::State.eq(PENDING))
        .filter(resume::Column::Id.gt(after_id))
        .order_by_asc(resume::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Started records, for settling those whose resumed turn ended or was lost.
pub async fn started_on<C: ConnectionTrait>(
    db: &C,
    limit: u64,
) -> Result<Vec<resume::Model>, ScheduleStoreError> {
    Ok(resume::Entity::find()
        .filter(resume::Column::State.eq(STARTED))
        .order_by_asc(resume::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Moves one record from `from` to `to`; `false` when another instance or
/// transaction already moved it.
pub async fn transition_on<C: ConnectionTrait>(
    txn: &C,
    id: i64,
    from: &str,
    to: &str,
    now_ms: i64,
) -> Result<bool, ScheduleStoreError> {
    let changed = resume::Entity::update_many()
        .col_expr(resume::Column::State, Expr::value(to))
        .col_expr(resume::Column::UpdatedAtMs, Expr::value(now_ms))
        .filter(resume::Column::Id.eq(id))
        .filter(resume::Column::State.eq(from))
        .exec(txn)
        .await?;
    Ok(changed.rows_affected == 1)
}

/// Ends every record of `conversation_id` not yet claimed, when its pause was
/// withdrawn by a stop or a resumed queue.
pub async fn supersede_conversation_on<C: ConnectionTrait>(
    txn: &C,
    conversation_id: &str,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    resume::Entity::update_many()
        .col_expr(resume::Column::State, Expr::value(SUPERSEDED))
        .col_expr(resume::Column::UpdatedAtMs, Expr::value(now_ms))
        .filter(resume::Column::ConversationId.eq(conversation_id))
        .filter(resume::Column::State.eq(PENDING))
        .exec(txn)
        .await?;
    Ok(())
}

/// The record a pending resume claim refers to, when still claimable.
pub async fn find_on<C: ConnectionTrait>(
    db: &C,
    kind: DecisionKind,
    request_id: &str,
) -> Result<Option<resume::Model>, ScheduleStoreError> {
    Ok(resume::Entity::find()
        .filter(resume::Column::Kind.eq(kind.as_str()))
        .filter(resume::Column::RequestId.eq(request_id))
        .one(db)
        .await?)
}

/// Withdraws the request `pause` names when the owner stopped its conversation
/// or resumed its queue: a pending skill proposal becomes `withdrawn`, a
/// pending fresh-task proposal is deleted without a receipt, and unclaimed
/// resumes of the conversation end. It writes only these rows; the caller
/// owns the session write, made after locking the session as `decide_on` does.
pub async fn withdraw_on<C: ConnectionTrait>(
    txn: &C,
    session: &PersistedAgentSession,
    pause: &desk_diagnose_core::proposal_review::DecisionPause,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    use crate::entity::agent_schedule as schedule;
    match pause.kind {
        DecisionKind::Skill => {
            crate::skill_store::proposals::withdraw_on(
                txn,
                &session.actor_id,
                &session.conversation_id,
                now_ms,
            )
            .await?;
        }
        DecisionKind::Schedule => {
            schedule::Entity::update_many()
                .col_expr(schedule::Column::Status, Expr::value("deleted"))
                .col_expr(
                    schedule::Column::NextRunAt,
                    Expr::value(Option::<i64>::None),
                )
                .col_expr(
                    schedule::Column::Revision,
                    sea_orm::sea_query::ExprTrait::add(Expr::col(schedule::Column::Revision), 1),
                )
                .col_expr(schedule::Column::UpdatedAt, Expr::value(now_ms))
                .filter(schedule::Column::ScheduleId.eq(&pause.id))
                .filter(schedule::Column::Status.eq("pending_review"))
                .filter(
                    schedule::Column::ProposalConversationId.eq(session.conversation_id.as_str()),
                )
                .exec(txn)
                .await?;
        }
        // The session write rejects the pending directory request.
        DecisionKind::Directory => {}
    }
    supersede_conversation_on(txn, &session.conversation_id, now_ms).await
}

#[cfg(test)]
mod tests;
