//! Exact native metadata verification shared by owner and model history reads.

use super::{AgentStoreError, unsigned};
use crate::entity::agent_history as history;
use desk_agent_protocol::ai_agent::source::AgentSourceKind;
use desk_diagnose_core::ai_agent::native_history::{
    NativeHistoryMetadata, NativeHistoryTimeColumns, read_metadata,
};

pub fn read(row: &history::Model) -> Result<Option<NativeHistoryMetadata>, AgentStoreError> {
    let kind = match row.kind.as_str() {
        "input" => AgentSourceKind::Input,
        "agent_message" => AgentSourceKind::AgentMessage,
        "receipt" => AgentSourceKind::Receipt,
        "native_status" => AgentSourceKind::NativeStatus,
        _ => return Err(AgentStoreError::Inaccessible),
    };
    let columns = NativeHistoryTimeColumns {
        at_unix_ms: row.native_action_at_ms.map(unsigned).transpose()?,
        from_unix_ms: row.native_action_from_ms.map(unsigned).transpose()?,
        until_unix_ms: row.native_action_until_ms.map(unsigned).transpose()?,
        basis: row.time_basis.clone(),
    };
    read_metadata(
        kind,
        unsigned(row.recorded_at_ms)?,
        row.state_json.as_deref(),
        &columns,
    )
    .map_err(|_| AgentStoreError::Inaccessible)
}

pub fn zero_input_allowed(row: &history::Model, native: Option<&NativeHistoryMetadata>) -> bool {
    let kind = match row.kind.as_str() {
        "input" => AgentSourceKind::Input,
        "agent_message" => AgentSourceKind::AgentMessage,
        "receipt" => AgentSourceKind::Receipt,
        "native_status" => AgentSourceKind::NativeStatus,
        _ => return false,
    };
    desk_diagnose_core::ai_agent::source::zero_owner_input_allowed(
        kind,
        &row.role,
        &row.conversation_id,
        &row.root_conversation_id,
        native,
    )
}
