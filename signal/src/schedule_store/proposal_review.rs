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
    reason: &str,
) -> Result<(), ScheduleStoreError> {
    desk_assistant_data_store::proposal_review::decide_by_ai_on(
        &super::session_host::SignalScheduleHost,
        txn,
        owner,
        proposal,
        approve,
        reason,
    )
    .await
}
