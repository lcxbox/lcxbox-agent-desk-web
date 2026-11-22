//! The same one-level delegation API shape for OSS Signal and Manager.

pub use desk_agent_protocol::ai_agent::subagent::{
    AiAgentStopControl, AiAgentStopResult, AiAgentSubAgentControl, AiAgentSubAgentEvent,
    AiAgentSubAgentPage, AiAgentSubAgentResult, AiAgentSubAgentSummary,
};
use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAgentSubAgentsQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAgentSubAgentQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub task_id: String,
    pub before_report_seq: Option<u64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAgentSubAgentControlBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub control: AiAgentSubAgentControl,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAgentStopBody {
    /// Omit for an owner conversation without a device target.
    pub connection: Option<String>,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub control: AiAgentStopControl,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AiAgentDirectoryControlAction {
    SelectDirectory {
        path: String,
        purpose: String,
        expected_revision: u64,
    },
    DecideDirectory {
        directory_request_id: String,
        expected_revision: u64,
        approve: bool,
    },
    RevokeDirectory {
        directory_request_id: String,
        expected_revision: u64,
    },
}

impl From<AiAgentDirectoryControlAction>
    for desk_agent_protocol::ai_agent::AiAgentObjectContextOperation
{
    fn from(value: AiAgentDirectoryControlAction) -> Self {
        match value {
            AiAgentDirectoryControlAction::SelectDirectory {
                path,
                purpose,
                expected_revision,
            } => Self::SelectDirectory {
                path,
                purpose,
                expected_revision,
            },
            AiAgentDirectoryControlAction::DecideDirectory {
                directory_request_id,
                expected_revision,
                approve,
            } => Self::DecideDirectory {
                directory_request_id,
                expected_revision,
                approve,
            },
            AiAgentDirectoryControlAction::RevokeDirectory {
                directory_request_id,
                expected_revision,
            } => Self::RevokeDirectory {
                directory_request_id,
                expected_revision,
            },
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAgentDirectoryControlBody {
    pub connection: String,
    pub session: String,
    pub client_request_id: String,
    pub operation: AiAgentDirectoryControlAction,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAgentSubAgentReadBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub task_id: String,
    /// Marks only UI notifications at or before the revision displayed by the owner.
    pub through_state_revision: u64,
}

/// Idempotency is the immutable native request/generation pair, not the latest turn.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAgentCommandCancelBody {
    pub connection: String,
    pub session: String,
    pub exec_request_id: String,
    pub execution_generation: String,
}
