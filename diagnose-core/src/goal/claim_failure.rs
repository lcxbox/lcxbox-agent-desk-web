//! What a host does with a queued goal whose next segment could not start.
//!
//! The goal stays authoritative in storage. A failed start either leaves the
//! goal for the model wait or block it already has, defers the next attempt
//! for a fault that can clear by itself, or pauses the goal for its owner when
//! retrying the same input cannot succeed. Hosts apply every outcome as a
//! write conditioned on the goal they observed, so a stale failure is a no-op.

use super::{GoalError, GoalPauseReason, GoalRun, GoalState};
use desk_agent_protocol::{AgentError, AgentErrorKind};

/// Owner-visible reason of a goal paused because its segment cannot start.
pub const GOAL_CONTINUATION_FAILED: &str = "continuation_failed";

const CLAIM_RETRY_BASE_MS: u64 = 5_000;
const CLAIM_RETRY_MAX_MS: u64 = 10 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStartFailure {
    /// The attempt observed a goal or session that has since changed: another
    /// node claimed it or its owner acted. Nothing is written.
    Stale,
    /// The model provider is temporarily unavailable: the existing model wait.
    ModelWait,
    /// The model provider rejected the request: the existing block.
    ModelBlocked,
    /// Transport, storage or database faults, including a session snapshot
    /// that cannot be decoded: retry later with backoff.
    Transient,
    /// The same input cannot start: pause for the owner.
    Deterministic,
}

impl GoalStartFailure {
    pub fn classify(error: &AgentError) -> Self {
        match error.kind {
            AgentErrorKind::ModelUnavailable if error.retryable => Self::ModelWait,
            AgentErrorKind::ModelRejected => Self::ModelBlocked,
            AgentErrorKind::InvalidInput
            | AgentErrorKind::PermissionDenied
            | AgentErrorKind::ApprovalRequired
            | AgentErrorKind::RiskBlocked
            | AgentErrorKind::UnsupportedCapability
            | AgentErrorKind::UnsupportedPlatform
            | AgentErrorKind::ContentBlocked
            | AgentErrorKind::OutputLimitExceeded
            | AgentErrorKind::AttachmentCapacity => Self::Deterministic,
            AgentErrorKind::TargetOffline
            | AgentErrorKind::SessionUnavailable
            | AgentErrorKind::HostAtCapacity
            | AgentErrorKind::Timeout
            | AgentErrorKind::Cancelled
            | AgentErrorKind::RedactionFailed
            | AgentErrorKind::TransportError
            | AgentErrorKind::ModelUnavailable
            | AgentErrorKind::ContentSafetyUnavailable
            | AgentErrorKind::Internal => Self::Transient,
        }
    }
}

impl GoalRun {
    /// Whether `self` is still exactly the queued goal a failed start observed.
    pub fn is_observed_queued(&self, observed: &GoalRun, now_unix_ms: u64) -> bool {
        self.state == GoalState::Queued
            && now_unix_ms < self.deadline_unix_ms
            && self.goal_id == observed.goal_id
            && self.owner_id == observed.owner_id
            && self.device_id == observed.device_id
            && self.input_revision == observed.input_revision
            && self.goal_revision == observed.goal_revision
            && self.state_version == observed.state_version
            && self.lease_epoch == observed.lease_epoch
            && self.updated_at_unix_ms == observed.updated_at_unix_ms
    }

    /// Defer the next start of a queued goal. Each consecutive deferral doubles
    /// the previous delay up to a cap; a successful claim clears it. Only the
    /// scheduling time changes, so the owner's goal version stays current.
    pub fn defer_start(&mut self, now_unix_ms: u64) -> Result<u64, GoalError> {
        if self.state != GoalState::Queued
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        let previous = self
            .next_attempt_unix_ms
            .map_or(0, |next| next.saturating_sub(self.updated_at_unix_ms));
        let delay = previous
            .saturating_mul(2)
            .clamp(CLAIM_RETRY_BASE_MS, CLAIM_RETRY_MAX_MS);
        let next = now_unix_ms.saturating_add(delay).min(self.deadline_unix_ms);
        self.next_attempt_unix_ms = Some(next);
        self.updated_at_unix_ms = now_unix_ms;
        self.validate()?;
        Ok(next)
    }

    /// Pause a queued goal whose segment cannot start for its current input.
    /// The owner retries or cancels it with the existing goal controls.
    pub fn pause_for_start_failure(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Queued {
            return Err(GoalError::InvalidState);
        }
        self.pause_settled(GoalPauseReason::Recovery, now_unix_ms)?;
        self.status_reason = Some(GOAL_CONTINUATION_FAILED.into());
        self.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(kind: AgentErrorKind, retryable: bool) -> AgentError {
        AgentError {
            kind,
            message: "fixture".into(),
            retryable,
            safe_for_model: false,
            error_code: None,
        }
    }

    #[test]
    fn classification_keeps_model_handling_and_separates_retryable_faults() {
        use GoalStartFailure::*;
        assert_eq!(
            GoalStartFailure::classify(&error(AgentErrorKind::ModelUnavailable, true)),
            ModelWait
        );
        assert_eq!(
            GoalStartFailure::classify(&error(AgentErrorKind::ModelRejected, false)),
            ModelBlocked
        );
        assert_eq!(
            GoalStartFailure::classify(&crate::subagent::invalid(
                "delegation creation does not contain authentic owner input"
            )),
            Deterministic
        );
        for kind in [
            AgentErrorKind::TransportError,
            AgentErrorKind::Internal,
            AgentErrorKind::ModelUnavailable,
        ] {
            assert_eq!(GoalStartFailure::classify(&error(kind, false)), Transient);
        }
    }

    #[test]
    fn deferral_doubles_to_a_cap_within_the_deadline_and_pause_is_owner_visible() {
        let mut goal = super::super::tests::goal();
        let start = goal.updated_at_unix_ms;
        let version = goal.state_version;
        let first = goal.defer_start(start).unwrap();
        assert_eq!(first, start + CLAIM_RETRY_BASE_MS);
        let second = goal.defer_start(start + 1).unwrap();
        assert_eq!(second, start + 1 + 2 * CLAIM_RETRY_BASE_MS);
        for _ in 0..20 {
            goal.defer_start(goal.updated_at_unix_ms).unwrap();
        }
        assert_eq!(
            goal.next_attempt_unix_ms,
            Some((goal.updated_at_unix_ms + CLAIM_RETRY_MAX_MS).min(goal.deadline_unix_ms))
        );
        assert_eq!(goal.state_version, version);
        assert_eq!(goal.state, GoalState::Queued);
        let observed = goal.clone();
        assert!(goal.is_observed_queued(&observed, goal.updated_at_unix_ms));
        goal.pause_for_start_failure(goal.updated_at_unix_ms)
            .unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Recovery));
        assert_eq!(
            goal.status_reason.as_deref(),
            Some(GOAL_CONTINUATION_FAILED)
        );
        assert_eq!(goal.next_attempt_unix_ms, None);
        assert!(!goal.is_observed_queued(&observed, goal.updated_at_unix_ms));
        assert!(goal.defer_start(goal.updated_at_unix_ms).is_err());
        assert!(
            goal.pause_for_start_failure(goal.updated_at_unix_ms)
                .is_err()
        );
    }
}
