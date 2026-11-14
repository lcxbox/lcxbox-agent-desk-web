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
