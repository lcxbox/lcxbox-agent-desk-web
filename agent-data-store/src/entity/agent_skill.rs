//! A platform or personal skill; content lives in immutable version rows.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_skill")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "skill_id")]
    pub skill_id: String,
    /// `platform` or `personal:<actor_id>`.
    #[sea_orm(unique_key = "scope_name")]
    pub scope_key: String,
    #[sea_orm(unique_key = "scope_name")]
    pub name: String,
    pub enabled: bool,
    pub current_version: i64,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
