//! Proposals that wait in the proposing turn for the AI reviewer or the owner,
//! dispatched by kind to the schedule and skill stores.
use crate::schedule::{ScheduleStoreError, session::ScheduleHostStores};
use crate::skill_store::proposals::SkillProposalError;
use desk_diagnose_core::{
    proposal_review::{ProposalKind, ProposalReviewRef, ReviewedProposal},
    session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;

impl From<SkillProposalError> for ScheduleStoreError {
    fn from(error: SkillProposalError) -> Self {
        match error {
            SkillProposalError::Store(error) => error,
            // A proposal that can no longer be decided as reviewed.
            SkillProposalError::Rejected(_) => Self::Conflict,
        }
    }
}

/// The proposal waiting in `session`'s turn, or `None` once it was decided
/// or belongs to another conversation.
pub async fn review_proposal_on<C: ConnectionTrait>(
    db: &C,
    session: &PersistedAgentSession,
    reference: &ProposalReviewRef,
) -> Result<Option<ReviewedProposal>, ScheduleStoreError> {
    Ok(match reference.kind {
        ProposalKind::Schedule => crate::schedule::review_proposal_on(db, session, &reference.id)
            .await?
            .map(ReviewedProposal::Schedule),
        ProposalKind::Skill => {
            crate::skill_store::proposals::review_proposal_on(db, session, &reference.id)
                .await?
                .map(ReviewedProposal::Skill)
        }
    })
}

/// Apply the reviewer's verdict to the exact reviewed revision in the
/// caller's review-settlement transaction.
pub async fn decide_by_ai_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: &str,
    proposal: &ReviewedProposal,
    approve: bool,
    reason: &str,
) -> Result<(), ScheduleStoreError> {
    match proposal {
        ReviewedProposal::Schedule(proposal) => {
            crate::schedule::decide_by_ai_on(host, txn, owner, proposal, approve, reason).await
        }
        ReviewedProposal::Skill(proposal) => {
            if proposal.owner_id != owner {
                return Err(ScheduleStoreError::Conflict);
            }
            crate::skill_store::proposals::decide_by_ai_on(host, txn, proposal, approve, reason)
                .await
                .map_err(Into::into)
        }
    }
}
