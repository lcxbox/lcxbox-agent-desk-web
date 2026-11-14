//! A model-authored new skill or new version, waiting for a decision.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_skill_proposal")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "proposal_id")]
    pub proposal_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub input_revision: i64,
    /// `create`, `update`, `disable` or `delete`.
    pub kind: String,
    pub name: String,
    pub base_skill_id: Option<String>,
    pub base_version: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub content_json: String,
    #[sea_orm(column_type = "Text")]
    pub source_policy_json: String,
    #[sea_orm(column_type = "Text")]
    pub reason: String,
    /// `pending`, `approved`, `rejected`, `withdrawn` or `conflicted`.
    #[sea_orm(indexed)]
    pub status: String,
    pub decision_source: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub decision_reason: Option<String>,
    pub created_at_ms: i64,
    pub decided_at_ms: Option<i64>,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
