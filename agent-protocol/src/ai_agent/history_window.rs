//! Owner-only retained fragments; these are never execution-session snapshots.
use super::source::{AgentSourceKind, AgentSourceResolution, validate_source_id};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentHistoryWindowView {
    #[default]
    Context,
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryWindowRequest {
    pub source_id: String,
    pub revision: u64,
    #[serde(default)]
    pub view: AgentHistoryWindowView,
    pub cursor: Option<String>,
}
impl AgentHistoryWindowRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_source_id(&self.source_id)?;
        if self.revision == 0
            || self.revision > i64::MAX as u64
            || self
                .cursor
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 2048)
        {
            return Err("invalid retained history window");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryFragment {
    pub source: AgentSourceResolution,
    pub role: String,
    pub kind: AgentSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryWindow {
    pub conversation_id: String,
    pub anchor_source_id: String,
    pub anchor_revision: u64,
    pub observed_through_generation: u64,
    pub fragments: Vec<AgentHistoryFragment>,
    pub previous_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub partial: bool,
}
