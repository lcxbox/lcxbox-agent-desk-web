//! Single-account agent settings remain in the authoritative config document.

use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::ai_agent::policy::{PolicyAssessment, PolicyIssue, PolicyView};
use desk_agent_protocol::ai_agent::user_instructions::{
    AgentDataPolicy, AgentDataPolicyUpdate, AgentDataSettings, AgentDataSettingsUpdate,
    UserInstructions, UserInstructionsUpdate,
};
use desk_diagnose_core::ai_agent::policy as policy_rules;
use sea_orm::DbErr;

#[derive(Debug)]
pub enum WriteError {
    Conflict { current_revision: u64 },
    Invalid,
    PolicyInvalid(PolicyAssessment),
    Backend(DbErr),
}
impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

pub async fn settings<C: ConfigConnection>(db: &C) -> AgentDataSettings {
    db.config_read().await.agent_data_settings.clone()
}
pub async fn instructions<C: ConfigConnection>(db: &C) -> UserInstructions {
    db.config_read().await.agent_user_instructions.clone()
}
pub async fn policy<C: ConfigConnection>(db: &C) -> AgentDataPolicy {
    db.config_read().await.agent_data_policy.clone()
}

pub async fn update_instructions(
    db: &DatabaseConnection,
    request: &UserInstructionsUpdate,
) -> Result<UserInstructions, WriteError> {
    if desk_diagnose_core::ai_agent::settings::contains_credentials(&request.content)
        .map_err(|_| WriteError::Invalid)?
    {
        return Err(WriteError::Invalid);
    }
    db.config_context()
        .update(|config| {
            request
                .validate_with_limits(&config.agent_data_policy.limits.memory)
                .map_err(|_| WriteError::Invalid)?;
            if config.agent_user_instructions.revision != request.expected_revision {
                return Err(WriteError::Conflict {
                    current_revision: config.agent_user_instructions.revision,
                });
            }
            let next = UserInstructions {
                revision: request
                    .expected_revision
                    .checked_add(1)
                    .ok_or(WriteError::Invalid)?,
                content: request.content.clone(),
            };
            config.agent_user_instructions = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Invalid)
}

pub async fn update_settings(
    db: &DatabaseConnection,
    request: &AgentDataSettingsUpdate,
) -> Result<AgentDataSettings, WriteError> {
    db.config_context()
        .update(|config| {
            if config.agent_data_settings.revision != request.expected_revision {
                return Err(WriteError::Conflict {
                    current_revision: config.agent_data_settings.revision,
                });
            }
            let next = AgentDataSettings {
                revision: request
                    .expected_revision
                    .checked_add(1)
                    .ok_or(WriteError::Invalid)?,
                automatic_memory_enabled: request.automatic_memory_enabled,
                model_history_search_enabled: request.model_history_search_enabled,
                explanation_version: request.explanation_version,
            };
            desk_diagnose_core::ai_agent::settings::validate_settings(&next)
                .map_err(|_| WriteError::Invalid)?;

            config.agent_data_settings = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Invalid)
}

pub async fn update_policy(
    db: &DatabaseConnection,
    request: &AgentDataPolicyUpdate,
) -> Result<PolicyView, WriteError> {
    let assessment = policy_rules::assess_update(request);
    if assessment.has_errors() {
        return Err(WriteError::PolicyInvalid(assessment));
    }
    let saved = db
        .config_context()
        .update(|config| {
            if config.agent_data_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict {
                    current_revision: config.agent_data_policy.revision,
                });
            }
            let mut next = request.policy.clone();
            next.revision = request
                .expected_revision
                .checked_add(1)
                .ok_or(WriteError::Invalid)?;
            config.agent_data_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Invalid)?;
    log_warnings(&assessment.issues);
    Ok(PolicyView {
        policy: saved,
        issues: assessment.issues,
    })
}

pub(crate) fn log_warnings(issues: &[PolicyIssue]) {
    for issue in issues {
        log::warn!(
            "Agent policy {}: {}",
            issue.fields.join(", "),
            issue.message
        );
    }
}
