//! Authenticated link identities used by source suppression during forgetting.

use crate::entity::agent_memory_source as evidence;
use crate::{AgentStoreError, unsigned};
use desk_agent_protocol::ai_agent::source::SourceRefRole;
use desk_diagnose_core::ai_agent::{
    memory_control::KnownMemorySource, memory_evidence::MemoryEvidenceRetentionProof,
};

pub fn forgetting_sources(
    row: &evidence::Model,
) -> Result<Vec<KnownMemorySource>, AgentStoreError> {
    let known: Vec<KnownMemorySource> =
        serde_json::from_str(&row.known_sources_json).map_err(|_| AgentStoreError::Inaccessible)?;
    let forget: Vec<KnownMemorySource> = serde_json::from_str(&row.forget_sources_json)
        .map_err(|_| AgentStoreError::Inaccessible)?;
    let proof: MemoryEvidenceRetentionProof = serde_json::from_str(&row.retention_proof_json)
        .map_err(|_| AgentStoreError::Inaccessible)?;
    if proof.actor_id != row.actor_id
        || proof.memory_id != row.memory_id
        || proof.memory_revision != unsigned(row.memory_revision)?
        || proof.origin_id != row.origin_id
        || row.retained_bytes <= 0
        || proof.recorded_at_unix_ms != unsigned(row.observed_at_ms)?
        || proof.statement_timezone != row.statement_timezone
        || !matches!(
            (row.role.as_str(), proof.role),
            ("evidence", SourceRefRole::Evidence)
                | ("background", SourceRefRole::Background)
                | ("previous_memory", SourceRefRole::PreviousMemory)
        )
    {
        return Err(AgentStoreError::Inaccessible);
    }
    proof
        .forgetting_sources(&known, &forget)
        .map_err(AgentStoreError::Invalid)
}
