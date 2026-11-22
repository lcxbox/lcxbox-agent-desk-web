//! Resumes conversations whose turn paused on a proposal or directory request
//! once the AI reviewer or the owner decided it, and tells the owner which
//! pending requests need their decision now.

use crate::config::connection::DatabaseConnection;
use crate::entity::{agent_approval_review, agent_session};
use desk_agent_data_store::{decision_resume, entity::agent_decision_resume};
use desk_agent_protocol::ai_agent::{
    AiAgentAsk, approval_mode::AgentApprovalMode, skill::SkillProposalView,
};
use desk_diagnose_core::{
    approval_review::{APPROVAL_REVIEW_STATUS_EXPIRED, APPROVAL_REVIEW_STATUS_UNAVAILABLE},
    proposal_review::{DecisionKind, OwnerDecisionFacts, awaiting_owner},
    session::PersistedAgentSession,
};
use desk_signal_facade::model::connection::SharedConnectionMap;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};
use std::time::Duration;

const BATCH_SIZE: u64 = 32;

fn review_source_kind(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Schedule => "schedule_proposal",
        DecisionKind::Skill => "skill_proposal",
        DecisionKind::Directory => "directory_proposal",
    }
}

/// Whether the owner must decide this pending request of `session` now.
pub async fn awaiting_owner_on(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    kind: DecisionKind,
    request_id: &str,
    requested_at_unix_ms: u64,
) -> Result<bool, DbErr> {
    let manual = crate::agent_approval_store::preference_for_session_on(db, session)
        .await?
        .is_some_and(|preference| preference.mode == AgentApprovalMode::Manual);
    let reviewer_available = crate::ai_agent_gate::global_ai_agent_gate().is_enabled()
        && crate::approval_model_provider::load(db)
            .await
            .is_ok_and(|config| config.destination_identity().is_ok());
    let review_ended_without_verdict = agent_approval_review::Entity::find()
        .filter(agent_approval_review::Column::SourceKind.eq(review_source_kind(kind)))
        .filter(agent_approval_review::Column::SourceId.eq(request_id))
        .filter(agent_approval_review::Column::Status.is_in([
            APPROVAL_REVIEW_STATUS_UNAVAILABLE,
            APPROVAL_REVIEW_STATUS_EXPIRED,
        ]))
        .one(db)
        .await?
        .is_some();
    Ok(awaiting_owner(OwnerDecisionFacts {
        manual_approval: manual,
        reviewer_available,
        review_ended_without_verdict,
        requested_at_unix_ms,
        now_unix_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
    }))
}

/// The snapshot view of the request `conversation_id` paused on.
pub async fn pause_view_on(
    db: &DatabaseConnection,
    conversation_id: &str,
    actor_id: &str,
    pause: &desk_diagnose_core::proposal_review::DecisionPause,
) -> Result<desk_signal_facade::controller::ai_agent_session::DecisionPauseDto, DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(actor_id))
        .one(db)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("paused conversation".into()))?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let awaiting = awaiting_owner_on(
        db,
        &session,
        pause.kind,
        &pause.id,
        pause.requested_at_unix_ms,
    )
    .await?;
    Ok(desk_signal_facade::controller::ai_agent_session::DecisionPauseDto::new(pause, awaiting))
}

/// Fills `awaiting_owner` of the pending proposals among `views`.
pub async fn mark_skill_views_on(
    db: &DatabaseConnection,
    views: &mut [SkillProposalView],
) -> Result<(), DbErr> {
    use desk_agent_protocol::ai_agent::skill::SkillProposalStatus;
    for view in views
        .iter_mut()
        .filter(|view| view.status == SkillProposalStatus::Pending)
    {
        let Some(row) = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&view.conversation_id))
            .one(db)
            .await?
        else {
            view.awaiting_owner = true;
            continue;
        };
        let Ok(session) = PersistedAgentSession::decode_json(&row.state_json) else {
            continue;
        };
        let requested_at = session
            .decision_pause
            .as_ref()
            .filter(|pause| pause.is(DecisionKind::Skill, &view.proposal_id))
            .map_or(view.created_at_ms.max(0) as u64, |pause| {
                pause.requested_at_unix_ms
            });
        view.awaiting_owner = awaiting_owner_on(
            db,
            &session,
            DecisionKind::Skill,
            &view.proposal_id,
            requested_at,
        )
        .await?;
    }
    Ok(())
}

/// Withdraws the pause of `before` in the stop or queue-resume transaction
/// that writes `after`, the session without its pause.
pub async fn withdraw_on(
    txn: &impl ConnectionTrait,
    before: &PersistedAgentSession,
    after: &PersistedAgentSession,
) -> Result<(), DbErr> {
    let Some(pause) = before.decision_pause.as_ref() else {
        return Ok(());
    };
    if after.decision_pause.is_some() {
        return Ok(());
    }
    decision_resume::withdraw_on(txn, before, pause, chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|error| DbErr::Custom(format!("withdraw paused request: {error:?}")))
}

/// Resumes decided requests whose conversation still pauses on them.
#[derive(Clone)]
pub struct DecisionResumeExecutor {
    db: DatabaseConnection,
    connections: actix_web::web::Data<SharedConnectionMap>,
    ai_agent_gate: std::sync::Arc<crate::ai_agent_gate::AiAgentGate>,
}

impl DecisionResumeExecutor {
    pub fn new(
        db: DatabaseConnection,
        connections: actix_web::web::Data<SharedConnectionMap>,
        ai_agent_gate: std::sync::Arc<crate::ai_agent_gate::AiAgentGate>,
    ) -> Self {
        Self {
            db,
            connections,
            ai_agent_gate,
        }
    }

    pub async fn run(self) {
        let mut cursor = 0;
        loop {
            match desk_diagnose_core::future::boxed(|| self.scan_once(cursor)).await {
                Ok(next) => cursor = next.unwrap_or(0),
                Err(error) => {
                    cursor = 0;
                    log::warn!("[decision-resume] durable scan unavailable: {error}");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// One pass over pending records after `after_id`, then a settlement of
    /// started records whose resumed turn ended or was lost.
    pub async fn scan_once(&self, after_id: i64) -> Result<Option<i64>, DbErr> {
        let records = decision_resume::pending_on(&self.db, after_id, BATCH_SIZE)
            .await
            .map_err(store_error)?;
        let next = (records.len() == BATCH_SIZE as usize)
            .then(|| records.last().map(|record| record.id))
            .flatten();
        for record in records {
            if let Err(error) =
                desk_diagnose_core::future::boxed(|| self.process(record.clone())).await
            {
                log::warn!(
                    "[decision-resume] resume deferred: kind={}, error={error}",
                    record.kind
                );
            }
        }
        self.settle_started().await?;
        Ok(next)
    }

    async fn process(&self, record: agent_decision_resume::Model) -> Result<(), DbErr> {
        if !self.ai_agent_gate.is_enabled()
            || record.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        {
            return Ok(());
        }
        let Some(kind) = DecisionKind::parse(&record.kind) else {
            return self.finish(&record, decision_resume::SUPERSEDED).await;
        };
        let Some(row) = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&record.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&record.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&record.device_id))
            .one(&self.db)
            .await?
        else {
            return self.finish(&record, decision_resume::SUPERSEDED).await;
        };
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        let Ok(input_revision) = u64::try_from(record.input_revision) else {
            return self.finish(&record, decision_resume::SUPERSEDED).await;
        };
        // A pause withdrawn or replaced by another input is never resumed.
        if session.input_revision != input_revision
            || desk_diagnose_core::proposal_review::current_pause(
                &session,
                kind,
                &record.request_id,
            )
            .is_none()
        {
            return self.finish(&record, decision_resume::SUPERSEDED).await;
        }
        if session.turn_state.is_active() || session.main_stopped {
            return Ok(());
        }
        // A goal segment is woken; its coordinator resumes the goal.
        match crate::agent_goal_store::wake_for_decision(
            &self.db,
            &session,
            &record,
            chrono::Utc::now(),
        )
        .await?
        {
            desk_diagnose_core::goal::GoalPermissionWake::NoGoal => {}
            desk_diagnose_core::goal::GoalPermissionWake::Queued
            | desk_diagnose_core::goal::GoalPermissionWake::Held => return Ok(()),
        }
        let child_creation = crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .child_creation_context(&session)
            .await
            .map_err(|_| DbErr::Custom("delegated decision source changed".into()))?;
        let Some(question) = child_creation
            .as_ref()
            .map(|creation| creation.source.owner_requirement.text.clone())
            .or_else(|| {
                desk_diagnose_core::permission_resume::latest_user_requirement(
                    &session.conversation,
                )
                .map(|message| message.text.clone())
            })
        else {
            return self.finish(&record, decision_resume::SUPERSEDED).await;
        };
        let locale = child_creation
            .as_ref()
            .and_then(|creation| creation.response_locale.clone());
        let resume_id = format!("decision-resume:{}", record.id);
        crate::ai_agent_orchestrator::resume_after_decision(
            self.connections.clone(),
            self.db.clone(),
            resume_id.clone(),
            String::new(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            session.device_id.clone(),
            session.conversation_id.clone(),
            kind,
            record.request_id.clone(),
            input_revision,
            AiAgentAsk {
                temporary_conversation: None,
                question,
                locale,
                client_message_id: resume_id,
                conversation_id: session.client_conversation_id.clone(),
                ..Default::default()
            },
        )
        .await;
        Ok(())
    }

    /// A started record is done once its resumed turn is no longer running.
    async fn settle_started(&self) -> Result<(), DbErr> {
        let now = chrono::Utc::now();
        for record in decision_resume::started_on(&self.db, BATCH_SIZE)
            .await
            .map_err(store_error)?
        {
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&record.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&record.actor_id))
                .one(&self.db)
                .await?;
            let running = row.as_ref().is_some_and(|row| {
                PersistedAgentSession::decode_json(&row.state_json)
                    .is_ok_and(|session| session.turn_state.is_active())
                    && row.lease_deadline.is_some_and(|deadline| deadline > now)
            });
            if !running {
                self.finish(&record, decision_resume::DONE).await?;
            }
        }
        Ok(())
    }

    async fn finish(&self, record: &agent_decision_resume::Model, to: &str) -> Result<(), DbErr> {
        let txn = crate::db::begin_write(&self.db, agent_decision_resume::Entity).await?;
        decision_resume::transition_on(
            &txn,
            record.id,
            &record.state,
            to,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(store_error)?;
        txn.commit().await
    }
}

fn store_error(error: desk_agent_data_store::schedule::ScheduleStoreError) -> DbErr {
    match error {
        desk_agent_data_store::schedule::ScheduleStoreError::Backend(error) => error,
        other => DbErr::Custom(format!("{other:?}")),
    }
}
