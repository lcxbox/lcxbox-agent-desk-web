//! A decided proposal or directory request whose paused turn the host resumes.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_decision_resume")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `DecisionKind` of the request.
    #[sea_orm(unique_key = "decision_request")]
    pub kind: String,
    #[sea_orm(unique_key = "decision_request")]
    pub request_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub input_revision: i64,
    /// `pending`, `started`, `done` or `superseded`.
    #[sea_orm(indexed)]
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
