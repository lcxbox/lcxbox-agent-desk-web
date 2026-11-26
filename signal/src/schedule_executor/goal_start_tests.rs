//! A queued goal whose segment cannot start is paused, deferred or left to the
//! goal's newer state, and is never picked again by the next one-second scan.
use super::*;
use crate::entity::agent_goal_run;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::goal::{
    GOAL_CONTINUATION_FAILED, GoalPauseReason, GoalRun, GoalStartFailure, GoalState,
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Set};

async fn executor() -> SignalScheduleExecutor {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    SignalScheduleExecutor::new(
        db,
        web::Data::new(SharedConnectionMap::new()),
        Arc::new(AiAgentGate::new(
            desk_agent_protocol::ai_agent::AiAgentSettings {
                enabled: true,
                revision: 1,
            },
        )),
    )
}

/// A queued goal on an idle conversation, as the candidate scan reads it.
async fn queued(db: &DatabaseConnection, conversation_id: &str) -> GoalRun {
    let now = desk_agent_data_store::datetime::utc_now();
    let mut session = PersistedAgentSession::new(
        conversation_id,
        "1",
        "device-1",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        now.to_rfc3339(),
    );
    session.surface = desk_diagnose_core::session::AgentSessionSurface::AiAgent;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    agent_session::ActiveModel {
        conversation_id: Set(conversation_id.into()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let goal = GoalRun::new(
        format!("goal-{conversation_id}"),
        conversation_id.into(),
        "1".into(),
        desk_diagnose_core::dynamic_run::AgentInputTarget::fixed("device-1".into()),
        "device-1".into(),
        "Finish the report".into(),
        "message-1".into(),
        desk_diagnose_core::goal::GoalOpening::OwnerRequest,
        desk_diagnose_core::goal::GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        now.timestamp_millis() as u64 - 1_000,
        desk_diagnose_core::goal::GoalLimits::default(),
    )
    .unwrap();
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    goal
}

async fn stored(db: &DatabaseConnection, goal: &GoalRun) -> GoalRun {
    let row = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::GoalId.eq(&goal.goal_id))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    crate::agent_goal_store::decode(&row).unwrap()
}

async fn runnable(db: &DatabaseConnection, at_unix_ms: u64) -> Vec<String> {
    crate::agent_goal_store::continuation_candidates(db, at_unix_ms, BATCH_SIZE, None)
        .await
        .unwrap()
        .into_iter()
        .map(|goal| goal.goal_id)
        .collect()
}

fn now_ms() -> u64 {
    jiff::Timestamp::now().as_millisecond() as u64
}

fn error(kind: AgentErrorKind, message: &str) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

async fn settle(executor: &SignalScheduleExecutor, goal: &GoalRun, error: &AgentError) {
    executor
        .settle_start_failure(goal, GoalStartFailure::classify(error), &error.message)
        .await;
}

#[actix_web::test]
async fn deterministic_start_failure_pauses_the_goal_for_its_owner() {
    let executor = executor().await;
    let db = executor.db.clone();
    let goal = queued(&db, "deterministic").await;
    assert_eq!(
        runnable(&db, now_ms()).await,
        std::slice::from_ref(&goal.goal_id)
    );
    let failure = desk_diagnose_core::subagent::invalid(
        "delegation creation does not contain authentic owner input",
    );
    settle(&executor, &goal, &failure).await;
    let paused = stored(&db, &goal).await;
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    assert_eq!(
        paused.status_reason.as_deref(),
        Some(GOAL_CONTINUATION_FAILED)
    );
    assert!(runnable(&db, now_ms() + 3_600_000).await.is_empty());
    // The same stale failure observed again changes nothing.
    settle(&executor, &goal, &failure).await;
    assert_eq!(stored(&db, &goal).await, paused);
}

#[actix_web::test]
async fn transient_start_failure_backs_off_and_is_retried_when_due() {
    let executor = executor().await;
    let db = executor.db.clone();
    let goal = queued(&db, "transient").await;
    settle(
        &executor,
        &goal,
        &error(AgentErrorKind::TransportError, "storage is unavailable"),
    )
    .await;
    let deferred = stored(&db, &goal).await;
    assert_eq!(deferred.state, GoalState::Queued);
    assert_eq!(deferred.state_version, goal.state_version);
    let due = deferred.next_attempt_unix_ms.unwrap();
    assert!(due > deferred.updated_at_unix_ms);
    assert!(runnable(&db, due - 1).await.is_empty());
    assert_eq!(
        runnable(&db, due).await,
        std::slice::from_ref(&goal.goal_id)
    );
    // A second failure of the deferred goal waits longer.
    settle(
        &executor,
        &deferred,
        &error(AgentErrorKind::Internal, "session snapshot is invalid"),
    )
    .await;
    let again = stored(&db, &goal).await;
    assert!(
        again.next_attempt_unix_ms.unwrap() - again.updated_at_unix_ms
            > due - deferred.updated_at_unix_ms
    );
    // The goal-row write needs no session snapshot.
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            sea_orm::sea_query::Expr::value("{"),
        )
        .exec(&db)
        .await
        .unwrap();
    settle(
        &executor,
        &again,
        &error(
            AgentErrorKind::InvalidInput,
            "deterministic but unrecordable",
        ),
    )
    .await;
    let after_decode_failure = stored(&db, &goal).await;
    assert_eq!(after_decode_failure.state, GoalState::Queued);
    assert!(after_decode_failure.next_attempt_unix_ms > again.next_attempt_unix_ms);
}

#[actix_web::test]
async fn an_older_failure_does_not_overwrite_a_newer_goal_state() {
    let executor = executor().await;
    let db = executor.db.clone();
    let observed = queued(&db, "stale").await;
    // Another node already deferred the goal from the same observation.
    settle(
        &executor,
        &observed,
        &error(AgentErrorKind::TransportError, "first node"),
    )
    .await;
    let newer = stored(&db, &observed).await;
    for failure in [
        error(AgentErrorKind::InvalidInput, "older deterministic failure"),
        error(AgentErrorKind::TransportError, "older transient failure"),
        error(AgentErrorKind::ModelRejected, "older rejection"),
    ] {
        settle(&executor, &observed, &failure).await;
        assert_eq!(stored(&db, &observed).await, newer);
    }
    // Another node claimed a segment of the goal it observed.
    let observed_claim = queued(&db, "claimed").await;
    let mut running = observed_claim.clone();
    running.claim_slice(now_ms()).unwrap();
    let txn = crate::db::begin_write(&db, agent_session::Entity)
        .await
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(
            &txn,
            &running,
            observed_claim.state_version,
            observed_claim.lease_epoch,
            Some("other-node"),
            Some(now_ms() + 60_000),
        )
        .await
        .unwrap()
    );
    txn.commit().await.unwrap();
    for failure in [
        error(AgentErrorKind::InvalidInput, "older deterministic failure"),
        error(AgentErrorKind::TransportError, "older transient failure"),
    ] {
        settle(&executor, &observed_claim, &failure).await;
        assert_eq!(stored(&db, &observed_claim).await, running);
    }
    // The owner paused it: an old failure cannot defer or re-pause it.
    let paused = crate::agent_goal_store::apply_owner_action(
        &db,
        &newer.conversation_id,
        "1",
        "device-1",
        &newer.goal_id,
        newer.state_version,
        desk_diagnose_core::goal::GoalOwnerAction::Pause,
        desk_agent_data_store::datetime::utc_now(),
    )
    .await
    .unwrap()
    .unwrap();
    for failure in [
        error(AgentErrorKind::InvalidInput, "older deterministic failure"),
        error(AgentErrorKind::TransportError, "older transient failure"),
    ] {
        settle(&executor, &newer, &failure).await;
        assert_eq!(stored(&db, &observed).await, paused);
    }
}

#[actix_web::test]
async fn model_rejection_still_blocks_the_goal() {
    let executor = executor().await;
    let db = executor.db.clone();
    let goal = queued(&db, "rejected").await;
    settle(
        &executor,
        &goal,
        &error(AgentErrorKind::ModelRejected, "model rejected the request"),
    )
    .await;
    let blocked = stored(&db, &goal).await;
    assert_eq!(blocked.state, GoalState::Blocked);
    assert_eq!(
        blocked.status_reason.as_deref(),
        Some("model_request_rejected")
    );
    assert!(runnable(&db, now_ms() + 3_600_000).await.is_empty());
}

#[actix_web::test]
async fn an_unwritable_database_paces_the_goal_on_this_node() {
    let executor = executor().await;
    let db = executor.db.clone();
    let goal = queued(&db, "unwritable").await;
    db.execute_unprepared("DROP TABLE agent_goal_run")
        .await
        .unwrap();
    settle(
        &executor,
        &goal,
        &error(AgentErrorKind::TransportError, "database is unavailable"),
    )
    .await;
    assert!(
        !executor
            .goal_backoff
            .is_ready(&goal.goal_id, std::time::Instant::now())
    );
    assert!(!executor.process_goal(goal).await);
}
