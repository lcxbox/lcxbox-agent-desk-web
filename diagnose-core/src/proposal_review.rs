//! Owner-directed proposals that wait in the proposing turn for an AI or
//! owner decision. Schedule and skill proposals share the waiting field, the
//! turn checks and the decision receipt shape; each kind keeps its own store.
use crate::{
    approval_review::ApprovalReviewError,
    chat::ChatRole,
    session::{AgentSessionSurface, PersistedAgentSession},
};
use serde::{Deserialize, Serialize};

/// How long the AI reviewer may decide a proposal; afterwards only the owner can.
pub const AI_REVIEW_WINDOW_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// A fresh scheduled task.
    Schedule,
    /// A new personal skill or a new version of one.
    Skill,
}

/// The proposal awaited by the current turn. It is not a permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalReviewRef {
    pub kind: ProposalKind,
    pub id: String,
}

impl ProposalReviewRef {
    pub fn schedule(id: impl Into<String>) -> Self {
        Self {
            kind: ProposalKind::Schedule,
            id: id.into(),
        }
    }

    pub fn skill(id: impl Into<String>) -> Self {
        Self {
            kind: ProposalKind::Skill,
            id: id.into(),
        }
    }
}

impl ProposalKind {
    /// Message id prefix of the tool-result receipt that saved the proposal.
    pub fn receipt_prefix(self) -> &'static str {
        match self {
            Self::Schedule => "schedule-proposal:",
            Self::Skill => "skill-proposal:",
        }
    }

    /// JSON key naming the proposal in its receipts.
    pub fn id_key(self) -> &'static str {
        match self {
            Self::Schedule => "schedule_id",
            Self::Skill => "proposal_id",
        }
    }

    /// Source kind of the review record.
    pub fn source_kind(self) -> &'static str {
        match self {
            Self::Schedule => "schedule_proposal",
            Self::Skill => "skill_proposal",
        }
    }

    /// Whether the session may propose this kind at all.
    pub fn allowed(self, session: &PersistedAgentSession) -> bool {
        session.surface == AgentSessionSurface::AiAssistant && session.owner_direct_planning()
    }

    /// The decision carried by a decision receipt of this kind: `Some(true)`
    /// for approval, `Some(false)` for any final non-approval.
    pub fn decision(self, receipt: &serde_json::Value, id: &str) -> Option<bool> {
        if receipt[self.id_key()].as_str() != Some(id) {
            return None;
        }
        match (self, receipt["event"].as_str()?) {
            (Self::Schedule, "scheduled_task_activated") => Some(true),
            (Self::Schedule, "scheduled_task_rejected") => Some(false),
            (Self::Skill, SKILL_DECIDED_EVENT) => Some(receipt["status"] == "approved"),
            _ => None,
        }
    }
}

/// Event name of a skill proposal decision receipt.
pub const SKILL_DECIDED_EVENT: &str = "skill_proposal_decided";
/// Event of a skill proposal left to the owner after the AI review window.
pub const SKILL_AWAITING_OWNER_EVENT: &str = "skill_proposal_awaiting_owner";

/// The identity of a proposal waiting in its original turn.
#[derive(Debug, Clone, Copy)]
pub struct WaitingProposal<'a> {
    pub reference: &'a ProposalReviewRef,
    pub owner_id: &'a str,
    pub conversation_id: &'a str,
    pub input_revision: u64,
    pub review_expires_at_unix_ms: u64,
}

/// The proposal must still wait in its original, owner-directed turn.
pub fn pending(
    session: &PersistedAgentSession,
    proposal: WaitingProposal<'_>,
    now: u64,
) -> Result<(), ApprovalReviewError> {
    if !proposal.reference.kind.allowed(session)
        || !session.turn_state.is_active()
        || session.main_stopped
        || session.pending_proposal_review.as_ref() != Some(proposal.reference)
        || proposal.conversation_id != session.conversation_id
        || proposal.owner_id != session.actor_id
        || proposal.input_revision != session.input_revision
        || proposal.review_expires_at_unix_ms <= now
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    proposal_call(session, proposal.reference)?;
    Ok(())
}

/// The proposal's original tool call, found through its receipt in this turn.
pub fn proposal_call<'a>(
    session: &'a PersistedAgentSession,
    reference: &ProposalReviewRef,
) -> Result<&'a str, ApprovalReviewError> {
    session
        .conversation
        .iter()
        .rev()
        .filter(|message| {
            message.role == ChatRole::Tool
                && message.turn_id.is_some()
                && message.turn_id == session.current_turn_id
                && message
                    .message_id
                    .starts_with(reference.kind.receipt_prefix())
        })
        .find(|message| {
            serde_json::from_str::<serde_json::Value>(&message.text)
                .is_ok_and(|value| value[reference.kind.id_key()] == reference.id.as_str())
        })
        .and_then(|message| message.tool_call_id.as_deref())
        .ok_or(ApprovalReviewError::InvalidContext)
}

/// A proposal projected by its store for the AI reviewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewedProposal {
    Schedule(crate::schedule::review::ScheduleProposal),
    Skill(crate::skills::authoring::SkillProposal),
}

impl ReviewedProposal {
    pub fn reference(&self) -> ProposalReviewRef {
        match self {
            Self::Schedule(proposal) => proposal.reference(),
            Self::Skill(proposal) => proposal.reference(),
        }
    }

    pub fn pending(
        &self,
        session: &PersistedAgentSession,
        now: u64,
    ) -> Result<(), ApprovalReviewError> {
        match self {
            Self::Schedule(proposal) => crate::schedule::review::pending(session, proposal, now),
            Self::Skill(proposal) => crate::skills::authoring::pending(session, proposal, now),
        }
    }

    /// The frozen review action of the proposal's original call.
    pub fn action(&self, session: &PersistedAgentSession) -> Result<String, ApprovalReviewError> {
        match self {
            Self::Schedule(proposal) => crate::schedule::review::action(session, proposal),
            Self::Skill(proposal) => crate::skills::authoring::action(session, proposal),
        }
    }

    pub fn candidate(
        &self,
        session: &PersistedAgentSession,
        delegation: &crate::approval_delegation::ApprovalDelegation,
        now: u64,
    ) -> Result<crate::approval_review::ApprovalReviewCandidate, ApprovalReviewError> {
        match self {
            Self::Schedule(proposal) => {
                crate::schedule::review::candidate(session, proposal, delegation, now)
            }
            Self::Skill(proposal) => {
                crate::skills::authoring::candidate(session, proposal, delegation, now)
            }
        }
    }

    pub fn authorize(
        &self,
        candidate: &crate::approval_review::ApprovalReviewCandidate,
        session: &PersistedAgentSession,
        delegation: &crate::approval_delegation::ApprovalDelegation,
        destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
        now: u64,
    ) -> Result<crate::approval_egress::AuthorizedApprovalReview, ApprovalReviewError> {
        match self {
            Self::Schedule(proposal) => crate::schedule::review::authorize(
                candidate,
                session,
                proposal,
                delegation,
                destination,
                now,
            ),
            Self::Skill(proposal) => crate::skills::authoring::authorize(
                candidate,
                session,
                proposal,
                delegation,
                destination,
                now,
            ),
        }
    }

    pub fn validate_current(
        &self,
        candidate: &crate::approval_review::ApprovalReviewCandidate,
        session: &PersistedAgentSession,
        delegation: &crate::approval_delegation::ApprovalDelegation,
        now: u64,
    ) -> Result<(), ApprovalReviewError> {
        match self {
            Self::Schedule(proposal) => crate::schedule::review::validate_current(
                candidate, session, proposal, delegation, now,
            ),
            Self::Skill(proposal) => crate::skills::authoring::validate_current(
                candidate, session, proposal, delegation, now,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_match_only_their_kind_and_id() {
        let schedule = serde_json::json!({"event":"scheduled_task_activated","schedule_id":"a"});
        assert_eq!(ProposalKind::Schedule.decision(&schedule, "a"), Some(true));
        assert_eq!(ProposalKind::Schedule.decision(&schedule, "b"), None);
        assert_eq!(ProposalKind::Skill.decision(&schedule, "a"), None);
        let skill = serde_json::json!({"event":SKILL_DECIDED_EVENT,"proposal_id":"a","status":"conflicted"});
        assert_eq!(ProposalKind::Skill.decision(&skill, "a"), Some(false));
        let approved =
            serde_json::json!({"event":SKILL_DECIDED_EVENT,"proposal_id":"a","status":"approved"});
        assert_eq!(ProposalKind::Skill.decision(&approved, "a"), Some(true));
        assert_eq!(ProposalKind::Schedule.decision(&approved, "a"), None);
    }

    #[test]
    fn the_reference_round_trips_in_session_json() {
        let reference = ProposalReviewRef::skill("p1");
        let text = serde_json::to_string(&reference).unwrap();
        assert_eq!(text, r#"{"kind":"skill","id":"p1"}"#);
        assert_eq!(
            serde_json::from_str::<ProposalReviewRef>(&text).unwrap(),
            reference
        );
    }
}
