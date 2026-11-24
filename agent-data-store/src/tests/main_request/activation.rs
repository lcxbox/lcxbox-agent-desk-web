use super::*;
use crate::entity::agent_main_request as request;
use queue::{ActivationFailureOutcome, coordination::Reservation};
use sea_orm::sea_query::Expr;

const CODE: i32 = 5203;

async fn enqueue_at(db: &sea_orm::DatabaseConnection, message: &str, now: i64) -> request::Model {
    let txn = db.begin().await.unwrap();
    let row = queue::enqueue(
        &txn,
        "owner",
        "root",
        &input(message, &format!("Request {message}")),
        None,
        8,
        now,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    row
}

async fn reload(db: &impl ConnectionTrait, row: &request::Model) -> request::Model {
    queue::load(db, "owner", "root", &row.request_id)
        .await
        .unwrap()
        .unwrap()
}

async fn record(
    db: &impl ConnectionTrait,
    scanned: &request::Model,
    now: i64,
) -> ActivationFailureOutcome {
    queue::record_activation_failure(db, scanned, scanned.activation_claim, now, Some(CODE))
        .await
        .unwrap()
}

fn agent_session() -> desk_diagnose_core::session::PersistedAgentSession {
    let mut session = desk_diagnose_core::session::PersistedAgentSession::new(
        "root",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    session.surface = desk_diagnose_core::session::AgentSessionSurface::AiAgent;
    session
}

#[tokio::test]
async fn failed_activations_back_off_then_close_the_request_with_its_error_code() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let row = enqueue_at(&db, "a", now - 60_000).await;
    // An elapsed coordination deferral is a separate schedule and is kept.
    request::Entity::update_many()
        .col_expr(request::Column::RetryAtMs, Expr::value(now - 1))
        .filter(request::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    for (attempt, backoff) in [(1, 5_000), (2, 10_000), (3, 20_000)] {
        let scanned = reload(&db, &row).await;
        let at = now + i64::from(attempt);
        assert_eq!(
            record(&db, &scanned, at).await,
            ActivationFailureOutcome::Deferred {
                retry_at_ms: at + backoff
            }
        );
        let saved = reload(&db, &row).await;
        assert_eq!(saved.state, queue::QUEUED);
        assert_eq!(saved.activation_attempts, attempt);
        assert_eq!(saved.activation_retry_at_ms, Some(at + backoff));
        assert_eq!(saved.retry_at_ms, Some(now - 1));
        assert!(saved.failure_error_code.is_none());
        assert!(queue::recovery_heads(&db, 0, 10).await.unwrap().is_empty());
        assert!(
            queue::first_pending(&db, "owner", "root")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            queue::receipts(&db, "owner", "root", None).await.unwrap()[0].state,
            MainRequestState::Queued
        );
    }
    let scanned = reload(&db, &row).await;
    assert_eq!(
        record(&db, &scanned, now + 4).await,
        ActivationFailureOutcome::Failed
    );
    let failed = reload(&db, &row).await;
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.activation_attempts, 4);
    assert!(failed.retry_at_ms.is_none());
    assert!(failed.activation_retry_at_ms.is_none());
    assert!(failed.pending_input_json.is_none());
    assert_eq!(failed.failure_error_code, Some(CODE));
    assert_eq!(
        record(&db, &failed, now + 5).await,
        ActivationFailureOutcome::Stale
    );
    assert_eq!(
        record(&db, &scanned, now + 5).await,
        ActivationFailureOutcome::Stale
    );
    for receipt in [
        queue::receipts(&db, "owner", "root", None).await.unwrap(),
        queue::receipts(&db, "owner", "root", Some(&row.request_id))
            .await
            .unwrap(),
    ] {
        assert_eq!(receipt.len(), 1);
        assert_eq!(receipt[0].state, MainRequestState::Failed);
        assert_eq!(
            receipt[0].failure,
            Some(MainRequestFailure::ActivationFailed)
        );
        assert_eq!(receipt[0].error_code, Some(CODE));
    }
    let tasks = crate::workbench::tasks::list_on(&db, "owner", "root", None, None)
        .await
        .unwrap();
    assert_eq!(
        tasks.items[0].receipt.failure,
        Some(MainRequestFailure::ActivationFailed)
    );
    assert_eq!(tasks.items[0].receipt.error_code, Some(CODE));
    assert!(queue::recovery_heads(&db, 0, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failure_without_an_error_code_closes_the_request_with_none() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let row = enqueue_at(&db, "a", now).await;
    for attempt in 1..=queue::MAX_ACTIVATION_ATTEMPTS {
        let scanned = reload(&db, &row).await;
        queue::record_activation_failure(
            &db,
            &scanned,
            scanned.activation_claim,
            now + i64::from(attempt),
            None,
        )
        .await
        .unwrap();
    }
    let receipt = queue::receipts(&db, "owner", "root", Some(&row.request_id))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(receipt.failure, Some(MainRequestFailure::ActivationFailed));
    assert!(receipt.error_code.is_none());
}

#[tokio::test]
async fn one_scan_records_once_and_cancellation_or_activation_wins_over_a_stale_scan() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    assert!(matches!(
        record(&db, &a, now).await,
        ActivationFailureOutcome::Deferred { .. }
    ));
    assert_eq!(
        record(&db, &a, now + 1).await,
        ActivationFailureOutcome::Stale
    );
    assert_eq!(reload(&db, &a).await.activation_attempts, 1);

    // A deferred request can still be cancelled.
    let cancelled = queue::cancel_owner_request(&db, "owner", "root", &a.request_id, now + 2)
        .await
        .unwrap();
    assert_eq!(cancelled.state, MainRequestState::Cancelled);

    // A cancellation that commits first leaves an old scan stale.
    let b = enqueue_at(&db, "b", now).await;
    queue::cancel_owner_request(&db, "owner", "root", &b.request_id, now + 3)
        .await
        .unwrap();
    assert_eq!(
        record(&db, &b, now + 4).await,
        ActivationFailureOutcome::Stale
    );
    let b = reload(&db, &b).await;
    assert_eq!(b.state, "cancelled");
    assert_eq!(b.activation_attempts, 0);

    // An activated row is never charged, from an old or a new scan.
    let c = enqueue_at(&db, "c", now).await;
    assert!(queue::activate(&db, &c, 1, now + 5).await.unwrap());
    let activated = reload(&db, &c).await;
    assert_eq!(
        record(&db, &c, now + 6).await,
        ActivationFailureOutcome::Stale
    );
    assert_eq!(
        record(&db, &activated, now + 6).await,
        ActivationFailureOutcome::Stale
    );
    assert_eq!(reload(&db, &c).await, activated);
}

#[tokio::test]
async fn a_deferred_head_lets_a_due_follower_start_after_unclaimed_activations() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    let b = enqueue_at(&db, "b", now).await;
    let c = enqueue_at(&db, "c", now).await;
    assert!(queue::activate(&db, &a, 1, now).await.unwrap());
    record(&db, &b, now).await;
    let heads = queue::recovery_heads(&db, 0, 10).await.unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].request_id, a.request_id, "unclaimed work first");
    let mut session = agent_session();
    session.input_revision = 1;
    assert!(
        queue::mark_claimed(&db, &a.request_id, &session, &a.client_message_id, now)
            .await
            .unwrap()
    );
    assert_eq!(
        queue::recovery_heads(&db, 0, 10).await.unwrap()[0].request_id,
        c.request_id
    );
    assert_eq!(
        queue::first_pending(&db, "owner", "root")
            .await
            .unwrap()
            .unwrap()
            .request_id,
        c.request_id
    );
    // Receipts still list the deferred request.
    assert!(
        queue::receipts(&db, "owner", "root", None)
            .await
            .unwrap()
            .iter()
            .any(|receipt| receipt.request_id == b.request_id
                && receipt.state == MainRequestState::Queued)
    );
}

#[tokio::test]
async fn admission_order_decides_between_a_committed_backoff_and_a_selected_head() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    // A backoff that commits first hides the head from admission.
    let a = enqueue_at(&db, "a", now).await;
    record(&db, &a, now).await;
    assert!(
        queue::first_pending(&db, "owner", "root")
            .await
            .unwrap()
            .is_none()
    );
    queue::cancel_owner_request(&db, "owner", "root", &a.request_id, now)
        .await
        .unwrap();

    // Admission already selected the head; the later backoff does not stop it,
    // and the started request keeps none of the backoff.
    let b = enqueue_at(&db, "b", now).await;
    let selected = queue::first_pending(&db, "owner", "root")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.request_id, b.request_id);
    assert!(matches!(
        record(&db, &b, now).await,
        ActivationFailureOutcome::Deferred { .. }
    ));
    assert!(queue::activate(&db, &selected, 1, now + 1).await.unwrap());
    let started = reload(&db, &b).await;
    assert_eq!(started.state, "activated");
    assert!(started.activation_retry_at_ms.is_none());
    assert_eq!(
        queue::recovery_heads(&db, 0, 10).await.unwrap()[0].request_id,
        b.request_id,
        "the unclaimed activation is scanned at once"
    );

    // A closed request cannot be admitted.
    let c = enqueue_at(&db, "c", now).await;
    for attempt in 1..=queue::MAX_ACTIVATION_ATTEMPTS {
        let scanned = reload(&db, &c).await;
        record(&db, &scanned, now + i64::from(attempt)).await;
    }
    assert!(!queue::activate(&db, &c, 2, now + 9).await.unwrap());
    assert_eq!(reload(&db, &c).await.state, "failed");
}

#[tokio::test]
async fn coordination_and_paused_deferrals_keep_an_activation_backoff_separate() {
    use desk_diagnose_core::chat::{ModelTurn, StopReason};
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    enqueue_at(&db, "b", now).await;
    let mut session = agent_session();
    let txn = db.begin().await.unwrap();
    let (batch, _) =
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], None)
            .await
            .unwrap()
            .batch()
            .unwrap();
    txn.commit().await.unwrap();
    // A failure recorded in the reservation's millisecond keeps the batch's
    // version; its settlement clears only the coordination deferral.
    let reserved = reload(&db, &a).await;
    assert!(matches!(
        record(&db, &reserved, now).await,
        ActivationFailureOutcome::Deferred { .. }
    ));
    let turn = ModelTurn {
        text: serde_json::json!({"decisions":[{"request_id": a.request_id, "route": "native"}]})
            .to_string(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    queue::coordination::settle(&db, &batch, Some(&turn), now + 1, &Default::default())
        .await
        .unwrap();
    let settled = reload(&db, &a).await;
    assert!(settled.coordinated);
    assert!(settled.retry_at_ms.is_none());
    assert_eq!(settled.activation_retry_at_ms, Some(now + 5_000));
    assert!(
        queue::recovery_heads(&db, 0, 10)
            .await
            .unwrap()
            .iter()
            .all(|head| head.id != a.id)
    );

    // A paused-source deferral changes neither the count nor the backoff.
    queue::defer_paused_source(&db, &settled, None, now + 2)
        .await
        .unwrap();
    let paused = reload(&db, &a).await;
    assert_eq!(paused.retry_at_ms, Some(now + 5_002));
    assert_eq!(paused.activation_attempts, 1);
    assert_eq!(paused.activation_retry_at_ms, Some(now + 5_000));
}

#[tokio::test]
async fn an_old_scan_loses_to_a_paused_deferral_a_reservation_and_a_selected_route() {
    use desk_diagnose_core::chat::{ModelTurn, StopReason};
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    // Three failures, then a paused-source deferral, then the old scan's
    // fourth failure: the subscription may still resume its request.
    let a = enqueue_at(&db, "a", now).await;
    for attempt in 1..=3 {
        let scanned = reload(&db, &a).await;
        record(&db, &scanned, now + attempt).await;
    }
    let old = reload(&db, &a).await;
    queue::defer_paused_source(&db, &old, None, now + 4)
        .await
        .unwrap();
    assert_eq!(
        record(&db, &old, now + 5).await,
        ActivationFailureOutcome::Stale
    );
    let paused = reload(&db, &a).await;
    assert_eq!(paused.state, queue::QUEUED);
    assert_eq!(paused.activation_attempts, 3);
    assert!(paused.pending_input_json.is_some());
    queue::cancel_owner_request(&db, "owner", "root", &a.request_id, now)
        .await
        .unwrap();

    // A reservation that commits first keeps its coordination attempt.
    let b = enqueue_at(&db, "b", now).await;
    enqueue_at(&db, "c", now).await;
    let mut workbench =
        desk_diagnose_core::session::PersistedAgentSession::new_workbench_coordinator(
            "owner",
            "root",
            None,
            1,
            agent_session().scope_snapshot,
            &desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
        )
        .unwrap();
    workbench.surface = desk_diagnose_core::session::AgentSessionSurface::AiAgent;
    let txn = db.begin().await.unwrap();
    let (batch, request) =
        queue::coordination::reserve(&txn, &mut workbench, &b, now, Vec::new(), &[], None)
            .await
            .unwrap()
            .batch()
            .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        record(&db, &b, now + 1).await,
        ActivationFailureOutcome::Stale
    );
    let reserved = reload(&db, &b).await;
    assert_eq!(reserved.coordination_attempts, 1);
    assert_eq!(reserved.activation_attempts, 0);

    // The settlement selects a Worker route; a scan from before it is stale,
    // while a scan of the routed row records as usual and closes it at last.
    let mut turn = ModelTurn {
        text: serde_json::json!({"decisions":[{"request_id": b.request_id, "route": "worker", "dispatch": {}}]})
            .to_string(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let policy = desk_diagnose_core::model_egress::ModelEgressPolicy {
        destination: batch.destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "route".into(),
        now_unix_ms: now as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let accepted = policy.authorize_request(request).unwrap();
    turn.provider_meta.data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &accepted.input_envelopes)
            .unwrap(),
    );
    queue::coordination::settle(&db, &batch, Some(&turn), now + 2, &Default::default())
        .await
        .unwrap();
    let routed = reload(&db, &b).await;
    assert!(routed.first_dispatch_json.is_some());
    assert_eq!(
        record(&db, &reserved, now + 3).await,
        ActivationFailureOutcome::Stale
    );
    assert_eq!(reload(&db, &b).await, routed);
    for attempt in 1..=queue::MAX_ACTIVATION_ATTEMPTS {
        let scanned = reload(&db, &b).await;
        assert_ne!(
            record(&db, &scanned, now + 3 + i64::from(attempt)).await,
            ActivationFailureOutcome::Stale
        );
        if attempt < queue::MAX_ACTIVATION_ATTEMPTS {
            assert!(reload(&db, &b).await.first_dispatch_json.is_some());
        }
    }
    assert_eq!(reload(&db, &b).await.state, "failed");
}

#[tokio::test]
async fn an_unreadable_follower_ends_the_batch_and_later_fails_as_its_own_head() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    let b = enqueue_at(&db, "b", now).await;
    request::Entity::update_many()
        .col_expr(request::Column::PendingInputJson, Expr::value("{}"))
        .filter(request::Column::Id.eq(b.id))
        .exec(&db)
        .await
        .unwrap();
    let mut session = agent_session();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], None)
            .await
            .unwrap(),
        Reservation::Unbatched
    ));
    txn.commit().await.unwrap();
    let a = reload(&db, &a).await;
    assert_eq!(a.coordination_attempts, 0);
    assert!(queue::activate(&db, &a, 1, now).await.unwrap());
    session.input_revision = 1;
    assert!(
        queue::mark_claimed(&db, &a.request_id, &session, &a.client_message_id, now)
            .await
            .unwrap()
    );
    let head = queue::recovery_heads(&db, 0, 10).await.unwrap().remove(0);
    assert_eq!(head.id, b.id);
    assert!(FrozenRequestInput::decode(&head).is_err());
    for attempt in 1..=queue::MAX_ACTIVATION_ATTEMPTS {
        let scanned = reload(&db, &b).await;
        record(&db, &scanned, now + i64::from(attempt)).await;
    }
    assert_eq!(reload(&db, &b).await.state, "failed");
}

#[tokio::test]
async fn a_head_that_cannot_be_projected_fails_its_reservation_alone() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let mut broken = input("a", "Request a");
    broken.destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 0,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let txn = db.begin().await.unwrap();
    let a = queue::enqueue(&txn, "owner", "root", &broken, None, 8, now)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    enqueue_at(&db, "b", now).await;
    let mut session = agent_session();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], None).await,
        Err(AgentStoreError::Invalid(_))
    ));
    txn.rollback().await.unwrap();
    assert_eq!(reload(&db, &a).await.coordination_attempts, 0);
}

/// A failure recorded by another instance after the candidates were read
/// changes the batch; the whole reservation rolls back without charging the
/// head, and the next round reserves normally.
#[tokio::test]
async fn a_candidate_changed_after_its_read_rolls_the_reservation_back() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    let b = enqueue_at(&db, "b", now).await;
    // Simulate the concurrent writer between the read and the final update.
    db.execute_unprepared(&format!(
        "CREATE TRIGGER concurrent_activation_failure AFTER UPDATE OF coordination_attempts \
         ON agent_main_request WHEN NEW.id = {} BEGIN UPDATE agent_main_request \
         SET activation_attempts = activation_attempts + 1, activation_retry_at_ms = {} \
         WHERE id = {}; END",
        a.id,
        now + 5_000,
        b.id
    ))
    .await
    .unwrap();
    let mut session = agent_session();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], None)
            .await
            .unwrap(),
        Reservation::Changed
    ));
    txn.rollback().await.unwrap();
    assert!(session.main_coordination.is_none());
    for row in [&a, &b] {
        let saved = reload(&db, row).await;
        assert_eq!(saved.state, queue::QUEUED);
        assert_eq!(saved.coordination_attempts, 0);
        assert_eq!(saved.activation_attempts, 0);
    }
    db.execute_unprepared("DROP TRIGGER concurrent_activation_failure")
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    assert!(
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], None)
            .await
            .unwrap()
            .batch()
            .is_some()
    );
    txn.commit().await.unwrap();
    assert_eq!(reload(&db, &a).await.coordination_attempts, 1);

    // A follower under activation backoff is not read into the batch at all.
    let c = enqueue_at(&db, "c", now).await;
    let d = enqueue_at(&db, "d", now).await;
    record(&db, &d, now).await;
    let mut other = agent_session();
    other.conversation_id = "root".into();
    let txn = db.begin().await.unwrap();
    queue::cancel_owner_request(&txn, "owner", "root", &a.request_id, now)
        .await
        .unwrap();
    queue::cancel_owner_request(&txn, "owner", "root", &b.request_id, now)
        .await
        .unwrap();
    assert!(matches!(
        queue::coordination::reserve(&txn, &mut other, &c, now, Vec::new(), &[], None)
            .await
            .unwrap(),
        Reservation::Unbatched
    ));
    txn.rollback().await.unwrap();
}

#[test]
fn progress_counts_only_an_uncancelled_attempt_that_neither_waited_nor_was_admitted() {
    use desk_agent_protocol::{AgentError, AgentErrorKind};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let error = |code| AgentError {
        kind: AgentErrorKind::Internal,
        message: "failed".into(),
        retryable: false,
        safe_for_model: false,
        error_code: code,
    };
    // Without owning the request's call, nothing is this attempt's failure.
    let pending = queue::ActivationProgress::default();
    assert!(!pending.owns_call() && !pending.failed() && pending.suppresses_stream());
    pending.attach_cancellation(|| false);
    assert!(
        pending.owns_call() && !pending.failed(),
        "the row is not claimed yet"
    );
    pending.set_claim(1);
    assert!(pending.failed());
    pending.note_error(&error(Some(7)));
    pending.note_error(&error(None));
    assert_eq!(pending.error_code(None), Some(7));
    assert_eq!(pending.error_code(Some(&error(Some(9)))), Some(9));
    assert_eq!(pending.error_code(Some(&error(None))), Some(7));

    let waiting = queue::ActivationProgress::default();
    waiting.attach_cancellation(|| false);
    waiting.set_claim(1);
    waiting.mark_waiting();
    assert!(!waiting.failed() && waiting.suppresses_stream());
    waiting.mark_admitted();
    waiting.mark_waiting();
    assert!(waiting.is_admitted() && !waiting.suppresses_stream());

    let cancelled = queue::ActivationProgress::default();
    let flag = Arc::new(AtomicBool::new(false));
    let probe = flag.clone();
    cancelled.attach_cancellation(move || probe.load(Ordering::SeqCst));
    cancelled.set_claim(1);
    assert!(cancelled.failed());
    flag.store(true, Ordering::SeqCst);
    assert!(!cancelled.failed());

    let mut row = request::Model {
        id: 1,
        request_id: "r".into(),
        conversation_id: "root".into(),
        actor_id: "owner".into(),
        client_message_id: "m".into(),
        submission_sha256: String::new(),
        state: queue::QUEUED.into(),
        failure: None,
        pending_input_json: Some("{}".into()),
        reply_connection_id: None,
        coordinated: false,
        first_dispatch_json: None,
        first_worker_request_id: None,
        coordination_attempts: 0,
        retry_at_ms: None,
        activation_attempts: 0,
        activation_retry_at_ms: None,
        failure_error_code: None,
        activation_claim: 0,
        input_revision: None,
        accepted_at_ms: 0,
        updated_at_ms: 0,
    };
    assert!(queue::tracks_activation(&row));
    // An admitted activation awaiting its first claim is not tracked again.
    row.state = "activated".into();
    row.input_revision = Some(1);
    assert!(!queue::tracks_activation(&row));
}

#[tokio::test]
async fn failed_receipts_list_the_latest_failures_by_when_they_failed() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let mut rows = Vec::new();
    for index in 0..10_i64 {
        let row = enqueue_at(&db, &format!("m{index}"), now).await;
        if index > 0 {
            queue::fail_unactivated(
                &db,
                &row,
                None,
                now + index,
                MainRequestFailure::SourceUnavailable,
            )
            .await
            .unwrap();
        }
        rows.push(row);
    }
    // The earliest request is the last to fail.
    queue::fail_unactivated(
        &db,
        &rows[0],
        None,
        now + 100,
        MainRequestFailure::SourceUnavailable,
    )
    .await
    .unwrap();
    let failed: Vec<_> = queue::receipts(&db, "owner", "root", None)
        .await
        .unwrap()
        .into_iter()
        .map(|receipt| receipt.request_id)
        .collect();
    assert_eq!(failed.len(), 8);
    assert_eq!(failed[0], rows[0].request_id);
    assert!(!failed.contains(&rows[1].request_id));
    assert!(!failed.contains(&rows[2].request_id));
}

/// The row's claim fences one attempt's failure record against a newer
/// attempt in either order.
#[tokio::test]
async fn a_claim_fences_failure_records_between_attempts() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let row = enqueue_at(&db, "fenced", now).await;
    // An older attempt claims, then a newer one takes the row over.
    let older = queue::claim_activation(&db, &row).await.unwrap().unwrap();
    let rescanned = reload(&db, &row).await;
    let newer = queue::claim_activation(&db, &rescanned)
        .await
        .unwrap()
        .unwrap();
    assert!(newer > older);
    assert_eq!(
        queue::record_activation_failure(&db, &row, older, now, None)
            .await
            .unwrap(),
        ActivationFailureOutcome::Stale
    );
    let saved = reload(&db, &row).await;
    assert_eq!(saved.activation_attempts, 0);
    assert_eq!(saved.state, queue::QUEUED);
    // The newer attempt's own record still applies.
    assert!(matches!(
        queue::record_activation_failure(&db, &rescanned, newer, now, None)
            .await
            .unwrap(),
        ActivationFailureOutcome::Deferred { .. }
    ));

    // An older attempt that records first leaves a newer claim of the old
    // scan with nothing to take over.
    let other = enqueue_at(&db, "first-wins", now).await;
    let older = queue::claim_activation(&db, &other).await.unwrap().unwrap();
    let claimed = reload(&db, &other).await;
    // A claim leaves the version other writers compare untouched.
    assert_eq!(claimed.updated_at_ms, other.updated_at_ms);
    assert!(matches!(
        queue::record_activation_failure(&db, &claimed, older, now, None)
            .await
            .unwrap(),
        ActivationFailureOutcome::Deferred { .. }
    ));
    assert_eq!(queue::claim_activation(&db, &claimed).await.unwrap(), None);
    assert_eq!(queue::claim_activation(&db, &other).await.unwrap(), None);
}

/// Admission and the dedicated close paths of a first activation act only
/// under the claim the attempt holds.
#[tokio::test]
async fn a_stale_claim_cannot_activate_close_or_defer_the_row() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let row = enqueue_at(&db, "stale-claim", now).await;
    let older = queue::claim_activation(&db, &row).await.unwrap().unwrap();
    let newer = queue::claim_activation(&db, &reload(&db, &row).await)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !queue::activate_with_dispatch(&db, &row, Some(older), 1, now, None)
            .await
            .unwrap()
    );
    queue::fail_unactivated(
        &db,
        &row,
        Some(older),
        now,
        MainRequestFailure::SourceUnavailable,
    )
    .await
    .unwrap();
    queue::defer_paused_source(&db, &row, Some(older), now)
        .await
        .unwrap();
    let untouched = reload(&db, &row).await;
    assert_eq!(untouched.state, queue::QUEUED);
    assert!(untouched.retry_at_ms.is_none());
    assert_eq!(untouched.activation_claim, newer);
    // The current claim still acts.
    assert!(
        queue::activate_with_dispatch(&db, &row, Some(newer), 1, now, None)
            .await
            .unwrap()
    );
    let other = enqueue_at(&db, "current-close", now).await;
    let claim = queue::claim_activation(&db, &other).await.unwrap().unwrap();
    queue::fail_unactivated(
        &db,
        &other,
        Some(claim),
        now,
        MainRequestFailure::SourceUnavailable,
    )
    .await
    .unwrap();
    assert_eq!(reload(&db, &other).await.state, "failed");
}

#[tokio::test]
async fn an_exhausted_claim_or_attempt_counter_takes_nothing() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let row = enqueue_at(&db, "exhausted", now).await;
    request::Entity::update_many()
        .col_expr(request::Column::ActivationClaim, Expr::value(i64::MAX))
        .filter(request::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let saturated = reload(&db, &row).await;
    assert_eq!(
        queue::claim_activation(&db, &saturated).await.unwrap(),
        None
    );
    request::Entity::update_many()
        .col_expr(request::Column::ActivationAttempts, Expr::value(i32::MAX))
        .filter(request::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let saturated = reload(&db, &row).await;
    assert_eq!(
        queue::record_activation_failure(&db, &saturated, i64::MAX, now, None)
            .await
            .unwrap(),
        ActivationFailureOutcome::Stale
    );
    assert_eq!(reload(&db, &row).await, saturated);
}

/// A newer attempt takes the head over before an older one reserves its
/// coordination, or after it: the older attempt neither spends nor settles
/// the head's coordination.
#[tokio::test]
async fn a_stale_claim_cannot_reserve_or_settle_the_heads_coordination() {
    let TestStore { db } = setup().await;
    let now = jiff::Timestamp::now().as_millisecond();
    let a = enqueue_at(&db, "a", now).await;
    let b = enqueue_at(&db, "b", now).await;
    request::Entity::update_many()
        .col_expr(request::Column::CoordinationAttempts, Expr::value(2))
        .filter(request::Column::Id.eq(a.id))
        .exec(&db)
        .await
        .unwrap();
    let a = reload(&db, &a).await;
    let older = queue::claim_activation(&db, &a).await.unwrap().unwrap();
    let newer = queue::claim_activation(&db, &reload(&db, &a).await)
        .await
        .unwrap()
        .unwrap();
    let mut session = agent_session();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], Some(older))
            .await
            .unwrap(),
        Reservation::Changed
    ));
    txn.rollback().await.unwrap();
    assert_eq!(reload(&db, &a).await.coordination_attempts, 2);
    assert_eq!(reload(&db, &b).await.coordination_attempts, 0);

    // The current attempt reserves; a newer claim then takes the head over,
    // and the reserving attempt's settlement leaves the head untouched.
    let txn = db.begin().await.unwrap();
    let (batch, _) =
        queue::coordination::reserve(&txn, &mut session, &a, now, Vec::new(), &[], Some(newer))
            .await
            .unwrap()
            .batch()
            .unwrap();
    txn.commit().await.unwrap();
    let reserved = reload(&db, &a).await;
    assert_eq!(reserved.coordination_attempts, 3);
    queue::claim_activation(&db, &reserved)
        .await
        .unwrap()
        .unwrap();
    queue::coordination::settle(&db, &batch, None, now + 1, &Default::default())
        .await
        .unwrap();
    let head = reload(&db, &a).await;
    assert_eq!(head.state, queue::QUEUED);
    assert!(head.failure.is_none());
    // The follower is not claimed by the attempt and settles as usual.
    assert!(reload(&db, &b).await.retry_at_ms.is_some());
}
