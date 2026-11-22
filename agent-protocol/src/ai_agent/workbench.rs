//! Owner-facing identities for personal Super Agent coordinators.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::MainRequestReceipt;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAiAgentWorkbench {
    /// Stable client-generated id for retrying this creation.
    pub creation_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAgentWorkbenchView {
    pub workbench_id: String,
    pub conversation_id: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAgentWorkbenchTasks {
    pub items: Vec<AiAgentWorkbenchTask>,
    pub next_before: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAgentWorkbenchTask {
    pub receipt: MainRequestReceipt,
    pub execution_session_id: Option<String>,
    pub updated_at_ms: i64,
    pub issue_number: Option<i64>,
    pub worker_states: Vec<String>,
    pub deliveries: Vec<AiAgentWorkbenchDelivery>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAgentWorkbenchDelivery {
    pub artifact_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub updated_at_ms: i64,
    pub comment_id: Option<i64>,
    pub instance: String,
    pub owner: String,
    pub repository: String,
    pub issue_number: u64,
}
