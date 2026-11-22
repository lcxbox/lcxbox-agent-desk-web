//! Memory reads and mutations retain their original native gates and projections.

use super::*;
use crate::agent_data_store as store;
use desk_agent_protocol::{
    ai_agent::{
        memory::{
            AgentDataControlReceipt, MemoryHistoryRequest, MemoryHistoryResult,
            MemorySearchRequest, MemorySearchResult, MemoryWriteRequest, MemoryWriteResult,
        },
        source::AgentSourceResolution,
    },
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::ai_agent::seam::{
    AgentDataCall, AgentDataRead, CurrentMemoryBlock, MemoryMutationSeam, MemorySeam,
};

#[async_trait(?Send)]
impl MemorySeam for SignalAgentSessionStore {
    async fn current_memories(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        preparation_id: &str,
        maximum_bytes: usize,
        _now_unix_ms: u64,
    ) -> Result<CurrentMemoryBlock, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            store::prepare_current_memory(
                &self.db,
                session,
                destination,
                preparation_id,
                maximum_bytes,
            )
        })
        .await
        .map_err(store::AgentStoreError::into_agent_error)
    }

    async fn search_memories(
        &self,
        context: AgentDataCall<'_>,
        request: &MemorySearchRequest,
    ) -> Result<AgentDataRead<MemorySearchResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| store::search_memories(&self.db, context, request))
            .await
            .map_err(store::AgentStoreError::into_agent_error)
    }

    async fn read_memory_history(
        &self,
        context: AgentDataCall<'_>,
        request: &MemoryHistoryRequest,
    ) -> Result<AgentDataRead<MemoryHistoryResult>, AgentError> {
        desk_diagnose_core::future::send_phase(|| {
            store::read_memory_history(&self.db, context, request)
        })
        .await
        .map_err(store::AgentStoreError::into_agent_error)
    }

    async fn read_memory_source(
        &self,
        context: AgentDataCall<'_>,
        source_id: &str,
    ) -> Result<AgentDataRead<AgentSourceResolution>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            store::read_memory_source(&self.db, context, source_id)
        })
        .await
        .map_err(store::AgentStoreError::into_agent_error)
    }
}

#[async_trait(?Send)]
impl MemoryMutationSeam for SignalAgentSessionStore {
    async fn manage_memory(
        &self,
        context: AgentDataCall<'_>,
        request: &MemoryWriteRequest,
    ) -> Result<AgentDataRead<MemoryWriteResult>, AgentError> {
        desk_diagnose_core::future::send_phase(|| store::write_memory(&self.db, context, request))
            .await
            .map_err(store::AgentStoreError::into_agent_error)
    }

    async fn forget_group(
        &self,
        session: &mut PersistedAgentSession,
        call: &desk_diagnose_core::ai_agent::seam::MemoryForgetCall<'_>,
    ) -> Result<AgentDataControlReceipt, AgentError> {
        let (receipt, closed) = desk_diagnose_core::future::boxed(|| {
            store::forget_native_memory(&self.db, session, call)
        })
        .await
        .map_err(store::AgentStoreError::into_agent_error)?;
        if let Some(closed) = closed {
            *session = *closed;
        }
        Ok(receipt)
    }
}
