//! One immutable version of a skill.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_skill_version")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "skill_version")]
    pub skill_id: String,
    #[sea_orm(unique_key = "skill_version")]
    pub version: i64,
    /// `imported`, `edited` or `ai_authored`.
    pub origin: String,
    /// The approved model proposal of an `ai_authored` version.
    pub proposal_id: Option<String>,
    /// Source policy proof of an `ai_authored` version; manual saves are owner content.
    #[sea_orm(column_type = "Text")]
    pub source_policy_json: Option<String>,
    /// Summary of the AI security scan the uploader saw before saving.
    #[sea_orm(column_type = "Text")]
    pub scan_summary_json: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub description: String,
    #[sea_orm(column_type = "Text")]
    pub compatibility: Option<String>,
    pub license: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub metadata_json: String,
    #[sea_orm(column_type = "Text")]
    pub allowed_tools: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    /// Path, kind, language and size of each file, read by the catalog.
    #[sea_orm(column_type = "Text")]
    pub resource_index_json: String,
    /// File contents, read only when the skill is loaded or edited.
    #[sea_orm(column_type = "Text")]
    pub resources_json: String,
    pub size_bytes: i64,
    pub created_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
