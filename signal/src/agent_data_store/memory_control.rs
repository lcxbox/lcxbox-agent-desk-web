//! Native owner transactions and model group closure adapters.

use super::{AgentStoreError, ensure_runtime_on};
pub(crate) use desk_agent_data_store::memory_control::*;
use desk_agent_protocol::ai_agent::memory::{
    AgentDataControlReceipt, MemoryClearRequest, MemoryDeleteRequest,
};
use sea_orm::TransactionTrait;
pub(crate) mod native;

pub(crate) async fn forget_memory(
    db: &crate::config::connection::DatabaseConnection,
    request: &MemoryDeleteRequest,
    now: u64,
) -> Result<AgentDataControlReceipt, AgentStoreError> {
    request.validate().map_err(AgentStoreError::Invalid)?;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let result = delete_on(&txn, &actor, request, now).await?;
    txn.commit().await?;
    Ok(result)
}

pub(crate) async fn clear_memories(
    db: &crate::config::connection::DatabaseConnection,
    request: &MemoryClearRequest,
    now: u64,
) -> Result<AgentDataControlReceipt, AgentStoreError> {
    request.validate().map_err(AgentStoreError::Invalid)?;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let result = clear_on(&txn, &actor, request, now).await?;
    txn.commit().await?;
    Ok(result)
}
