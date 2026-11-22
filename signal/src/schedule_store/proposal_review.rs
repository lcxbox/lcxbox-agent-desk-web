//! Apply the AI reviewer's verdict on a proposal waiting in its turn.
use super::ScheduleStoreError;
use desk_diagnose_core::proposal_review::ReviewedProposal;

/// Apply the reviewer's verdict to the exact reviewed revision in the
/// caller's review-settlement transaction.
pub(crate) async fn decide_by_ai_on(
    txn: &crate::config::connection::DatabaseTransaction,
    owner: &str,
    proposal: &ReviewedProposal,
    approve: bool,
    ai: &desk_diagnose_core::proposal_review::provenance::AiDecisionReason,
) -> Result<(), ScheduleStoreError> {
    desk_agent_data_store::proposal_review::decide_by_ai_on(
        &super::session_host::SignalScheduleHost,
        txn,
        owner,
        proposal,
        approve,
        ai,
    )
    .await
}
