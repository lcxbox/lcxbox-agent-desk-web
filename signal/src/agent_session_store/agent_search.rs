//! Personal-agent retrieval delegates to the original native call gates.
//! A port advertises host support; it never substitutes for source authorization.

use super::*;
use desk_agent_protocol::ai_agent::history::{
    ConversationReadRequest, ConversationReadResult, ConversationSearchRequest,
    ConversationSearchResult,
};
use desk_diagnose_core::ai_agent::seam::{AgentDataCall, AgentDataRead, ConversationSearchSeam};

#[async_trait(?Send)]
impl ConversationSearchSeam for SignalAgentSessionStore {
    async fn search_conversations(
        &self,
        context: AgentDataCall<'_>,
        request: &ConversationSearchRequest,
    ) -> Result<AgentDataRead<ConversationSearchResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            crate::agent_data_store::search_conversations(&self.db, context, request)
        })
        .await
        .map_err(crate::agent_data_store::AgentStoreError::into_agent_error)
    }

    async fn read_conversation(
        &self,
        context: AgentDataCall<'_>,
        request: &ConversationReadRequest,
    ) -> Result<AgentDataRead<ConversationReadResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            crate::agent_data_store::read_conversation(&self.db, context, request)
        })
        .await
        .map_err(crate::agent_data_store::AgentStoreError::into_agent_error)
    }
}
