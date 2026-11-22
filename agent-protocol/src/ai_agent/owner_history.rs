//! Owner history browsing is independent of model retrieval and execution state.
use super::{
    history::{HistorySort, HistoryTimeRange},
    history_window::AgentHistoryFragment,
    source::{AgentSourceKind, validate_source_id},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

fn default_limit() -> u32 {
    10
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistorySearchRequest {
    #[serde(default)]
    pub queries: Vec<String>,
    pub device_ref: Option<String>,
    pub conversation_id: Option<String>,
    pub time_range: Option<HistoryTimeRange>,
    #[serde(default)]
    pub source_kinds: Vec<AgentSourceKind>,
    #[serde(default)]
    pub sort: HistorySort,
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}
impl AgentHistorySearchRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::history::validate_queries(&self.queries, true)?;
        if self.limit == 0
            || self.limit > 20
            || self.source_kinds.len() > 4
            || self.source_kinds.contains(&AgentSourceKind::PreviousMemory)
            || self.cursor.as_ref().is_some_and(|value| value.is_empty())
        {
            return Err("invalid owner history search");
        }
        for id in [&self.device_ref, &self.conversation_id]
            .into_iter()
            .flatten()
        {
            validate_source_id(id)?;
        }
        if let Some(range) = &self.time_range {
            range.validate()?;
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AgentRetrievalLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        super::history::validate_queries_with_limits(&self.queries, true, limits)?;
        if self.cursor.as_ref().is_some_and(|cursor| {
            cursor.len() as u64 > u64::from(limits.episode.query_state_bytes).div_ceil(3) * 4 + 4
        }) {
            return Err("owner history cursor exceeds query state budget");
        }
        if let Some(range) = &self.time_range {
            range.validate_with_limits(limits)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistorySearchResult {
    pub fragments: Vec<AgentHistoryFragment>,
    pub next_cursor: Option<String>,
    pub observed_through_generation: u64,
    pub partial: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryConversationsRequest {
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryConversation {
    pub conversation_id: String,
    pub source: super::source::AgentSourceResolution,
    pub execution_available: bool,
    pub active: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHistoryConversations {
    pub conversations: Vec<AgentHistoryConversation>,
    pub next_cursor: Option<String>,
    pub partial: bool,
}
