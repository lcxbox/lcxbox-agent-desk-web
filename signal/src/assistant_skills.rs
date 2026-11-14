//! Standalone-signal host for platform and personal skills. The single local
//! account is also the platform account, so it manages both scopes.

use crate::config::ConfigConnection;

/// Bundled skills disabled in `config.toml`.
pub async fn disabled_bundled<C: ConfigConnection>(db: &C) -> Vec<String> {
    db.config_read()
        .await
        .assistant_bundled_skills
        .disabled
        .clone()
}

/// The skill catalog of one request. The local account owns both scopes.
pub async fn catalog<C: ConfigConnection>(
    db: &C,
    actor_id: &str,
    request: &desk_diagnose_core::skills::SkillCatalogRequest,
) -> Result<desk_diagnose_core::skills::SkillCatalog, sea_orm::DbErr> {
    use desk_assistant_data_store::skill_store::{SkillStoreError, service};
    service::host_catalog_on(
        db,
        &service::visible_scopes(actor_id, request.include_personal),
        &request.loaded,
        disabled_bundled(db).await,
    )
    .await
    .map_err(|error| match error {
        SkillStoreError::Backend(error) => error,
        _ => sea_orm::DbErr::Custom("skill catalog unavailable".into()),
    })
}

#[derive(Debug)]
pub enum BundledWriteError {
    Conflict,
    Invalid,
    Backend(sea_orm::DbErr),
}

impl From<sea_orm::DbErr> for BundledWriteError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Backend(error)
    }
}

/// Enables or disables one bundled skill in `config.toml`.
pub async fn set_bundled<C: ConfigConnection>(
    db: &C,
    name: &str,
    enabled: bool,
    expected_revision: u64,
) -> Result<desk_agent_protocol::ai_assistant::skill::BundledSkillSettings, BundledWriteError> {
    db.config_context()
        .update(|config| {
            if config.assistant_bundled_skills.revision != expected_revision {
                return Err(BundledWriteError::Conflict);
            }
            let next = desk_assistant_data_store::skill_store::service::toggle_bundled(
                &config.assistant_bundled_skills,
                name,
                enabled,
            )
            .ok_or(BundledWriteError::Invalid)?;
            config.assistant_bundled_skills = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(BundledWriteError::Invalid)
}

/// The scope key of the single local account's personal skills.
pub fn personal_scope() -> String {
    desk_assistant_data_store::skill_store::personal_scope(
        &crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string(),
    )
}

/// Saves a model skill proposal of the local account and its receipt under
/// the proposing turn's fence. A refusal is returned to the model.
pub(crate) async fn propose(
    db: &crate::config::connection::DatabaseConnection,
    session: &mut desk_diagnose_core::session::PersistedAgentSession,
    call: &desk_diagnose_core::chat::ToolCall,
    observation: &desk_diagnose_core::model_observability::tool::ToolObservation,
) -> Result<String, desk_agent_protocol::AgentError> {
    use desk_diagnose_core::skills::authoring;
    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(authoring::unavailable());
    }
    let txn = crate::db::begin_write(
        db,
        desk_assistant_data_store::entity::agent_skill_proposal::Entity,
    )
    .await
    .map_err(|_| authoring::unavailable())?;
    let (receipt, next) = desk_diagnose_core::future::send_phase(|| {
        desk_assistant_data_store::skill_store::proposals::propose_on(
            &crate::schedule_store::SignalScheduleHost,
            &txn,
            session,
            call,
            observation,
        )
    })
    .await
    .map_err(proposal_error)?;
    txn.commit().await.map_err(|_| authoring::unavailable())?;
    *session = next;
    observation.permission(desk_diagnose_core::model_observability::PermissionOutcome::Waiting);
    Ok(receipt)
}

fn proposal_error(
    error: desk_assistant_data_store::skill_store::proposals::SkillProposalError,
) -> desk_agent_protocol::AgentError {
    use desk_assistant_data_store::{
        schedule::ScheduleStoreError, skill_store::proposals::SkillProposalError,
    };
    match error {
        SkillProposalError::Rejected(message) => {
            desk_diagnose_core::skills::authoring::invalid_input(message)
        }
        SkillProposalError::Store(ScheduleStoreError::ToolInput(message)) => {
            desk_diagnose_core::skills::authoring::invalid_input(message)
        }
        SkillProposalError::Store(_) => desk_diagnose_core::skills::authoring::unavailable(),
    }
}

#[derive(Debug)]
pub enum ProposalDecisionError {
    NotFound,
    Conflict,
    Backend(sea_orm::DbErr),
}

/// The owner's decision on one of the local account's pending proposals.
pub async fn decide_proposal(
    db: &crate::config::connection::DatabaseConnection,
    decision: &desk_agent_protocol::ai_assistant::skill::SkillProposalDecision,
) -> Result<desk_agent_protocol::ai_assistant::skill::SkillProposalView, ProposalDecisionError> {
    use desk_assistant_data_store::{
        schedule::ScheduleStoreError,
        skill_store::proposals::{self, SkillDecider, SkillProposalError},
    };
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    let expected =
        i64::try_from(decision.expected_revision).map_err(|_| ProposalDecisionError::Conflict)?;
    let txn = crate::db::begin_write(
        db,
        desk_assistant_data_store::entity::agent_skill_proposal::Entity,
    )
    .await
    .map_err(ProposalDecisionError::Backend)?;
    let decided = proposals::decide_on(
        &crate::schedule_store::SignalScheduleHost,
        &txn,
        &actor,
        &decision.proposal_id,
        Some(expected),
        decision.approve,
        &SkillDecider::Owner,
    )
    .await
    .map_err(|error| match error {
        SkillProposalError::Store(ScheduleStoreError::NotFound) => ProposalDecisionError::NotFound,
        SkillProposalError::Store(ScheduleStoreError::Backend(error)) => {
            ProposalDecisionError::Backend(error)
        }
        _ => ProposalDecisionError::Conflict,
    })?;
    let view = proposals::view_on(&txn, decided)
        .await
        .map_err(|_| ProposalDecisionError::Conflict)?;
    txn.commit().await.map_err(ProposalDecisionError::Backend)?;
    Ok(view)
}

/// Undecided proposals of the local account, each marked when the owner
/// must decide it now.
pub async fn pending_proposals(
    db: &crate::config::connection::DatabaseConnection,
) -> Result<Vec<desk_agent_protocol::ai_assistant::skill::SkillProposalView>, sea_orm::DbErr> {
    use desk_assistant_data_store::skill_store::proposals;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    let mut views = proposals::list_on(db, &actor, None)
        .await
        .map_err(proposal_db_error)?;
    crate::decision_resume::mark_skill_views_on(db, &mut views).await?;
    Ok(views)
}

/// Every proposal of one conversation, for its proposal cards.
pub async fn conversation_proposals(
    db: &crate::config::connection::DatabaseConnection,
    actor_id: &str,
    conversation_id: &str,
) -> Result<Vec<desk_agent_protocol::ai_assistant::skill::SkillProposalView>, sea_orm::DbErr> {
    let mut views = desk_assistant_data_store::skill_store::proposals::list_on(
        db,
        actor_id,
        Some(conversation_id),
    )
    .await
    .map_err(proposal_db_error)?;
    // Decided proposals show their outcome on the proposing tool call; the
    // conversation lists only the proposals still awaiting a decision.
    views.retain(|view| {
        view.status == desk_agent_protocol::ai_assistant::skill::SkillProposalStatus::Pending
    });
    crate::decision_resume::mark_skill_views_on(db, &mut views).await?;
    Ok(views)
}

fn proposal_db_error(
    error: desk_assistant_data_store::skill_store::proposals::SkillProposalError,
) -> sea_orm::DbErr {
    use desk_assistant_data_store::{
        schedule::ScheduleStoreError, skill_store::proposals::SkillProposalError,
    };
    match error {
        SkillProposalError::Store(ScheduleStoreError::Backend(error)) => error,
        _ => sea_orm::DbErr::Custom("skill proposals unavailable".into()),
    }
}
