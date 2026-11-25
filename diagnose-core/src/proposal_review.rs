//! Requests that pause their turn until an AI or owner decision: schedule and
//! skill proposals and interactive directory requests. The paused turn ends;
//! the decision is written as a receipt and a decision-resume record, and the
//! host resumes the original requirement with `TriggerOrigin::ProposalDecision`.
use crate::{
    approval_review::ApprovalReviewError,
    chat::ChatRole,
    session::{AgentSessionSurface, PersistedAgentSession},
};
use serde::{Deserialize, Serialize};

pub mod projection;
pub mod provenance;

/// How long the AI reviewer may decide a request; afterwards only the owner can.
pub const AI_REVIEW_WINDOW_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// A fresh scheduled task.
    Schedule,
    /// A new personal skill or a new version of one.
    Skill,
}

/// A proposal under review. It is not a permission grant.
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

/// The kind of request a paused turn waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Schedule,
    Skill,
    /// An interactive directory request of the model.
    Directory,
    OwnerQuestion,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Skill => "skill",
            Self::Directory => "directory",
            Self::OwnerQuestion => "owner_question",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "schedule" => Some(Self::Schedule),
            "skill" => Some(Self::Skill),
            "directory" => Some(Self::Directory),
            "owner_question" => Some(Self::OwnerQuestion),
            _ => None,
        }
    }

    pub fn proposal(self) -> Option<ProposalKind> {
        match self {
            Self::Schedule => Some(ProposalKind::Schedule),
            Self::Skill => Some(ProposalKind::Skill),
            Self::Directory | Self::OwnerQuestion => None,
        }
    }
}

impl From<ProposalKind> for DecisionKind {
    fn from(kind: ProposalKind) -> Self {
        match kind {
            ProposalKind::Schedule => Self::Schedule,
            ProposalKind::Skill => Self::Skill,
        }
    }
}

/// The request a finished turn paused on. Set only when the turn ends with
/// `LoopOutcome::DecisionRequested`; cleared by the decision-resume claim, a
/// goal wake or a withdrawal. While set, no other turn may be claimed and new
/// owner input queues behind it. It grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionPause {
    pub kind: DecisionKind,
    pub id: String,
    pub turn_id: String,
    pub call_id: String,
    pub input_revision: u64,
    /// When the turn paused; the AI review window starts here.
    pub requested_at_unix_ms: u64,
}

impl DecisionPause {
    pub fn proposal(&self) -> Option<ProposalReviewRef> {
        Some(ProposalReviewRef {
            kind: self.kind.proposal()?,
            id: self.id.clone(),
        })
    }

    pub fn is(&self, kind: DecisionKind, id: &str) -> bool {
        self.kind == kind && self.id == id
    }
}

/// The pause of `session` when it waits for exactly this request in its
/// current input. Decisions and AI review apply only then.
pub fn current_pause<'a>(
    session: &'a PersistedAgentSession,
    kind: DecisionKind,
    id: &str,
) -> Option<&'a DecisionPause> {
    session
        .decision_pause
        .as_ref()
        .filter(|pause| pause.is(kind, id) && pause.input_revision == session.input_revision)
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
        session.surface == AgentSessionSurface::AiAgent && session.owner_direct_planning()
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

/// What a host knows about one pending request when it shows it to the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OwnerDecisionFacts {
    /// The conversation uses manual approval.
    pub manual_approval: bool,
    /// The approval model is configured and enabled.
    pub reviewer_available: bool,
    /// The AI review of this request ended without approving or denying it.
    pub review_ended_without_verdict: bool,
    pub requested_at_unix_ms: u64,
    pub now_unix_ms: u64,
}

/// Whether the owner must decide a pending request now rather than wait for
/// the AI reviewer.
pub fn awaiting_owner(facts: OwnerDecisionFacts) -> bool {
    facts.manual_approval
        || !facts.reviewer_available
        || facts.review_ended_without_verdict
        || facts
            .requested_at_unix_ms
            .saturating_add(AI_REVIEW_WINDOW_MS)
            <= facts.now_unix_ms
}

/// Withdraws the pause when the owner stops the conversation or resumes its
/// queue, like a pending permission request: a pending directory request is
/// rejected by the system and the turn is not resumed. The host withdraws a
/// paused proposal's own row and its resume record in the same transaction.
pub fn withdraw_pause(session: &mut PersistedAgentSession) -> Option<DecisionPause> {
    if session
        .decision_pause
        .as_ref()
        .is_some_and(|p| p.kind == DecisionKind::OwnerQuestion)
    {
        crate::owner_question::withdraw(session);
    }
    let pause = session.decision_pause.take()?;
    if pause.kind == DecisionKind::Directory {
        session.file_scope.withdraw_pending(
            &pause.id,
            crate::directory_review::DirectoryDecision {
                source: crate::dynamic_run::PermissionDecisionSource::SystemDecision {
                    reason_code: "owner_stopped".into(),
                    reason: "The owner stopped the conversation before the decision.".into(),
                },
                reason: "The owner stopped the conversation before the decision.".into(),
                candidate_id: None,
            },
        );
    }
    Some(pause)
}

/// The pause for a proposal the call `call_id` of turn `turn_id` just saved,
/// read from the receipt its store committed. `None` when the call settled
/// without a decision, such as a conversation timer created directly.
pub fn awaiting_proposal(
    session: &PersistedAgentSession,
    call_id: &str,
    turn_id: &str,
    now_unix_ms: u64,
) -> Option<DecisionPause> {
    let receipt = session.conversation.iter().rev().find(|message| {
        message.role == ChatRole::Tool
            && message.tool_call_id.as_deref() == Some(call_id)
            && message.turn_id.as_deref() == Some(turn_id)
    })?;
    let kind = [ProposalKind::Skill, ProposalKind::Schedule]
        .into_iter()
        .find(|kind| receipt.message_id.starts_with(kind.receipt_prefix()))?;
    let value = serde_json::from_str::<serde_json::Value>(&receipt.text).ok()?;
    if value["awaiting_decision"] != true {
        return None;
    }
    let id = value[kind.id_key()].as_str().filter(|id| !id.is_empty())?;
    Some(DecisionPause {
        kind: kind.into(),
        id: id.to_owned(),
        turn_id: turn_id.to_owned(),
        call_id: call_id.to_owned(),
        input_revision: session.input_revision,
        requested_at_unix_ms: now_unix_ms,
    })
}

/// The identity of a proposal its turn paused on.
#[derive(Debug, Clone, Copy)]
pub struct WaitingProposal<'a> {
    pub reference: &'a ProposalReviewRef,
    pub owner_id: &'a str,
    pub conversation_id: &'a str,
    pub input_revision: u64,
    pub review_expires_at_unix_ms: u64,
}

/// The proposal must still pause its original, owner-directed turn and be
/// within the AI review window.
pub fn pending(
    session: &PersistedAgentSession,
    proposal: WaitingProposal<'_>,
    now: u64,
) -> Result<(), ApprovalReviewError> {
    if !proposal.reference.kind.allowed(session)
        || session.main_stopped
        || session.turn_state.is_active()
        || current_pause(
            session,
            proposal.reference.kind.into(),
            &proposal.reference.id,
        )
        .is_none()
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

/// The proposal's original tool call, found through its receipt in the turn
/// that paused on it.
pub fn proposal_call<'a>(
    session: &'a PersistedAgentSession,
    reference: &ProposalReviewRef,
) -> Result<&'a str, ApprovalReviewError> {
    let pause = current_pause(session, reference.kind.into(), &reference.id)
        .ok_or(ApprovalReviewError::InvalidContext)?;
    session
        .conversation
        .iter()
        .rev()
        .filter(|message| {
            message.role == ChatRole::Tool
                && message.turn_id.as_deref() == Some(pause.turn_id.as_str())
                && message.tool_call_id.as_deref() == Some(pause.call_id.as_str())
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

    fn paused_session(kind: DecisionKind) -> PersistedAgentSession {
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-14T00:00:00Z",
        );
        session.input_revision = 3;
        session.decision_pause = Some(DecisionPause {
            kind,
            id: "request".into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            input_revision: 3,
            requested_at_unix_ms: 1_000,
        });
        session
    }

    #[test]
    fn a_pause_applies_only_to_its_request_in_the_current_input() {
        let mut session = paused_session(DecisionKind::Skill);
        assert!(current_pause(&session, DecisionKind::Skill, "request").is_some());
        assert!(current_pause(&session, DecisionKind::Skill, "other").is_none());
        assert!(current_pause(&session, DecisionKind::Schedule, "request").is_none());
        session.input_revision += 1;
        assert!(current_pause(&session, DecisionKind::Skill, "request").is_none());
    }

    #[test]
    fn withdrawing_a_directory_pause_rejects_the_request_by_the_system() {
        let mut session = paused_session(DecisionKind::Skill);
        assert_eq!(withdraw_pause(&mut session).unwrap().id, "request");
        assert!(session.decision_pause.is_none());
        assert!(withdraw_pause(&mut session).is_none());
        // A directory pause without its record still clears the pause.
        let mut directory = paused_session(DecisionKind::Directory);
        assert!(withdraw_pause(&mut directory).is_some());
        assert!(directory.decision_pause.is_none());
    }

    #[test]
    fn the_owner_decides_when_the_reviewer_cannot() {
        let facts = OwnerDecisionFacts {
            manual_approval: false,
            reviewer_available: true,
            review_ended_without_verdict: false,
            requested_at_unix_ms: 1_000,
            now_unix_ms: 1_000 + AI_REVIEW_WINDOW_MS - 1,
        };
        assert!(!awaiting_owner(facts));
        for owner in [
            OwnerDecisionFacts {
                manual_approval: true,
                ..facts
            },
            OwnerDecisionFacts {
                reviewer_available: false,
                ..facts
            },
            OwnerDecisionFacts {
                review_ended_without_verdict: true,
                ..facts
            },
            OwnerDecisionFacts {
                now_unix_ms: 1_000 + AI_REVIEW_WINDOW_MS,
                ..facts
            },
        ] {
            assert!(awaiting_owner(owner), "{owner:?}");
        }
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
