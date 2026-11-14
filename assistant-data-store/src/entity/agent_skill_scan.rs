//! A short-lived AI security scan result shown before an upload is saved.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_skill_scan")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "scan_id")]
    pub scan_id: String,
    pub scope_key: String,
    #[sea_orm(column_type = "Text")]
    pub summary_json: String,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
