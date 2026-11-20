//! Bounded retry of a queued request whose activation ends before native
//! admission. Hosts track one attempt with [`ActivationProgress`] and record a
//! failure against the exact row version their scan read.

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use desk_agent_protocol::{AgentError, ai_assistant::MainRequestFailure};
use sea_orm::{ColumnTrait, ConnectionTrait, QueryFilter, sea_query::Expr};

use super::{QUEUED, fail_payload, pending_row_update, request};
use crate::AssistantStoreError;

/// The fourth failed activation closes the request.
pub const MAX_ACTIVATION_ATTEMPTS: i32 = 4;
const FIRST_BACKOFF_MS: i64 = 5_000;

const PENDING: u8 = 0;
const WAITING: u8 = 1;
const ADMITTED: u8 = 2;

/// Progress of one first activation of a queued row, shared within the attempt
/// only. Until native admission every browser frame for the request is withheld.
#[derive(Default)]
pub struct ActivationProgress {
    state: AtomicU8,
    source_rejected: AtomicBool,
    error_code: Mutex<Option<i32>>,
    cancellation: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    claim: OnceLock<i64>,
}

impl ActivationProgress {
    /// Native admission accepted the input, or handed it to a Worker.
    pub fn mark_admitted(&self) {
        self.state.fetch_max(ADMITTED, Ordering::AcqRel);
    }

    /// Admission's recheck rejected the subscription source. The next scan's
    /// own source check defers a paused source or closes a revoked one.
    pub fn mark_source_rejected(&self) {
        self.source_rejected.store(true, Ordering::Release);
    }

    /// Admission found the conversation busy; the row stays queued unchanged.
    pub fn mark_waiting(&self) {
        self.state.fetch_max(WAITING, Ordering::AcqRel);
    }

    pub fn is_admitted(&self) -> bool {
        self.state.load(Ordering::Acquire) == ADMITTED
    }

    /// Busy and failure frames before admission are withheld as well.
    pub fn suppresses_stream(&self) -> bool {
        !self.is_admitted()
    }

    /// Keep the error code of a preparation exit that does not return its error.
    pub fn note_error(&self, error: &AgentError) {
        if let Some(code) = error.error_code {
            *self
                .error_code
                .lock()
                .expect("activation progress poisoned") = Some(code);
        }
    }

    pub fn noted_error_code(&self) -> Option<i32> {
        *self
            .error_code
            .lock()
            .expect("activation progress poisoned")
    }

    /// Bind the call ownership this attempt acquired, through its own
    /// cancellation token. The first binding wins.
    pub fn attach_cancellation(&self, cancelled: impl Fn() -> bool + Send + Sync + 'static) {
        let _ = self.cancellation.set(Box::new(cancelled));
    }

    /// Only an attempt that owns the request's call can fail it. An error
    /// before that is retried on the next scan without being recorded, since
    /// another instance may own a preparation in progress.
    pub fn owns_call(&self) -> bool {
        self.cancellation.get().is_some()
    }

    /// Keep the row's fencing token this attempt took with
    /// [`claim_activation`]. The first value wins.
    pub fn set_claim(&self, claim: i64) {
        let _ = self.claim.set(claim);
    }

    pub fn claim(&self) -> Option<i64> {
        self.claim.get().copied()
    }

    /// An attempt cancelled by its owner is not an activation failure.
    pub fn cancelled(&self) -> bool {
        self.cancellation.get().is_some_and(|cancelled| cancelled())
    }

    /// The finished attempt owned the call and claimed the row, neither
    /// reached admission nor waited, was not refused for its source, and was
    /// not cancelled: it counts as one failed activation.
    pub fn failed(&self) -> bool {
        self.owns_call()
            && self.claim().is_some()
            && self.state.load(Ordering::Acquire) == PENDING
            && !self.source_rejected.load(Ordering::Acquire)
            && !self.cancelled()
    }

    /// The returned error wins over a noted preparation error.
    pub fn error_code(&self, returned: Option<&AgentError>) -> Option<i32> {
        returned
            .and_then(|error| error.error_code)
            .or_else(|| self.noted_error_code())
    }
}

/// Only a queued row without a native revision starts a tracked activation.
pub fn tracks_activation(row: &request::Model) -> bool {
    row.state == QUEUED && row.input_revision.is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationFailureOutcome {
    Deferred {
        retry_at_ms: i64,
    },
    Failed,
    /// Another writer changed the row after the scan; nothing was recorded.
    Stale,
}

/// Take over the scanned row for this attempt after owning the request's call.
/// Returns the new fencing token, or `None` when the row changed or another
/// attempt took it over since the scan; the attempt then ends uncharged. A
/// failure recorded by an earlier attempt after this claim no longer matches.
pub async fn claim_activation(
    db: &impl ConnectionTrait,
    scanned: &request::Model,
) -> Result<Option<i64>, AssistantStoreError> {
    use request::Column;
    if !tracks_activation(scanned) {
        return Ok(None);
    }
    let Some(claim) = scanned.activation_claim.checked_add(1) else {
        return Ok(None);
    };
    let claimed = pending_row_update(scanned)
        .filter(Column::InputRevision.is_null())
        .filter(Column::ActivationClaim.eq(scanned.activation_claim))
        .filter(Column::ActivationAttempts.eq(scanned.activation_attempts))
        .col_expr(Column::ActivationClaim, Expr::value(claim))
        .exec(db)
        .await?
        .rows_affected
        == 1;
    Ok(claimed.then_some(claim))
}

/// Record one failed activation of the scanned row. Any change by another
/// writer since the scan (cancellation, admission, a paused-source deferral, a
/// coordination reservation or settlement, a concurrent failure record, or a
/// newer attempt's claim) makes the record stale.
pub async fn record_activation_failure(
    db: &impl ConnectionTrait,
    scanned: &request::Model,
    claim: i64,
    now_ms: i64,
    error_code: Option<i32>,
) -> Result<ActivationFailureOutcome, AssistantStoreError> {
    use request::Column;
    if !tracks_activation(scanned) {
        return Ok(ActivationFailureOutcome::Stale);
    }
    let Some(attempts) = scanned.activation_attempts.checked_add(1) else {
        return Ok(ActivationFailureOutcome::Stale);
    };
    let update = pending_row_update(scanned)
        .filter(Column::InputRevision.is_null())
        .filter(Column::ActivationClaim.eq(claim))
        .filter(Column::ActivationAttempts.eq(scanned.activation_attempts))
        .filter(Column::CoordinationAttempts.eq(scanned.coordination_attempts))
        .filter(Column::Coordinated.eq(scanned.coordinated))
        .filter(match &scanned.first_dispatch_json {
            Some(route) => Column::FirstDispatchJson.eq(route.as_str()),
            None => Column::FirstDispatchJson.is_null(),
        })
        .filter(match scanned.retry_at_ms {
            Some(retry_at) => Column::RetryAtMs.eq(retry_at),
            None => Column::RetryAtMs.is_null(),
        })
        .col_expr(Column::ActivationAttempts, Expr::value(attempts));
    let (update, outcome) = if attempts < MAX_ACTIVATION_ATTEMPTS {
        let retry_at_ms = now_ms.saturating_add(FIRST_BACKOFF_MS << (attempts - 1));
        (
            update
                .col_expr(Column::ActivationRetryAtMs, Expr::value(retry_at_ms))
                .col_expr(Column::UpdatedAtMs, Expr::value(now_ms)),
            ActivationFailureOutcome::Deferred { retry_at_ms },
        )
    } else {
        (
            fail_payload(update, now_ms, MainRequestFailure::ActivationFailed)
                .col_expr(Column::FailureErrorCode, Expr::value(error_code)),
            ActivationFailureOutcome::Failed,
        )
    };
    Ok(if update.exec(db).await?.rows_affected == 1 {
        outcome
    } else {
        ActivationFailureOutcome::Stale
    })
}
