//! Counters of one skill scope, locked while skills or proposals change.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_skill_scope")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "scope_key")]
    pub scope_key: String,
    pub skill_count: i64,
    pub enabled_count: i64,
    pub pending_proposal_count: i64,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
