//! Recoverable deadlines and receipts for native conversation prompts.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_owner_question")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub wait_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub deadline_ms: Option<i64>,
    pub status: String,
    pub client_request_id: Option<String>,
}

impl ActiveModelBehavior for ActiveModel {}
