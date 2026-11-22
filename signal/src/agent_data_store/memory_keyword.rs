//! Native source authorization for common fact keyword retrieval.

use super::{
    AgentStoreError,
    memory_view::{MemoryReadAuthorization, NativeMemoryReader},
};
use desk_agent_data_store::memory_keyword as shared;
pub(crate) use desk_agent_data_store::memory_keyword::MemoryCandidates;
use desk_agent_protocol::ai_agent::memory::MemorySearchRequest;
use sea_orm::ConnectionTrait;
pub(crate) async fn relevance_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    request: &MemorySearchRequest,
) -> Result<MemoryCandidates, AgentStoreError> {
    shared::relevance_on::<C, NativeMemoryReader>(db, context, request).await
}
