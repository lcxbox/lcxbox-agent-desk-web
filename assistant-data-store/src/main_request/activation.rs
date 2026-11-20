//! Bounded retry of a queued request whose activation ends before native
//! admission. Hosts track one attempt with [`ActivationProgress`] and record a
//! failure against the exact row version their scan read.

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU8, Ordering},
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
    error_code: Mutex<Option<i32>>,
    cancellation: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
}

impl ActivationProgress {
    /// Native admission accepted the input, or handed it to a Worker.
    pub fn mark_admitted(&self) {
        self.state.fetch_max(ADMITTED, Ordering::AcqRel);
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

    /// An attempt cancelled by its owner is not an activation failure.
    pub fn cancelled(&self) -> bool {
        self.cancellation.get().is_some_and(|cancelled| cancelled())
    }

    /// The finished attempt owned the call, neither reached admission nor
    /// waited, and was not
    /// cancelled: it counts as one failed activation.
    pub fn failed(&self) -> bool {
        self.owns_call() && self.state.load(Ordering::Acquire) == PENDING && !self.cancelled()
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
    /// The subscription was paused after the executor checked it; the request
    /// was deferred like any paused source.
    Paused,
    /// The subscription no longer authorizes the request; the next scan
    /// closes it as an unavailable source.
    SourceUnavailable,
}

/// Settle a failed activation. A subscribed request first rechecks its
/// source: admission rejects a source paused or revoked after the executor's
/// own check, and that is not an activation failure.
pub async fn settle_failed_activation(
    db: &impl ConnectionTrait,
    scanned: &request::Model,
    now_ms: i64,
    error_code: Option<i32>,
) -> Result<ActivationFailureOutcome, AssistantStoreError> {
    if let Ok(frozen) = super::FrozenRequestInput::decode(scanned)
        && frozen.issue_source.is_some()
    {
        match frozen
            .issue_queue_authority(db, &scanned.actor_id, now_ms)
            .await?
        {
            super::IssueQueueAuthority::Ready => {}
            super::IssueQueueAuthority::Paused => {
                super::defer_paused_source(db, scanned, now_ms).await?;
                return Ok(ActivationFailureOutcome::Paused);
            }
            super::IssueQueueAuthority::Unavailable => {
                return Ok(ActivationFailureOutcome::SourceUnavailable);
            }
        }
    }
    record_activation_failure(db, scanned, now_ms, error_code).await
}

/// Record one failed activation of the scanned row. Any change by another
/// writer since the scan (cancellation, admission, a paused-source deferral, a
/// coordination reservation or settlement, or a concurrent failure record)
/// makes the record stale.
pub async fn record_activation_failure(
    db: &impl ConnectionTrait,
    scanned: &request::Model,
    now_ms: i64,
    error_code: Option<i32>,
) -> Result<ActivationFailureOutcome, AssistantStoreError> {
    use request::Column;
    if !tracks_activation(scanned) {
        return Ok(ActivationFailureOutcome::Stale);
    }
    let attempts = scanned.activation_attempts.saturating_add(1);
    let update = pending_row_update(scanned)
        .filter(Column::InputRevision.is_null())
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
