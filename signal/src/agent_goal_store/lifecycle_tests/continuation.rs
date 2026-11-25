use super::*;
use desk_diagnose_core::session::{ActionIdentity, ExecutionState};

async fn read_session(db: &DatabaseConnection, cid: &str) -> PersistedAgentSession {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(cid))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

async fn store_fixture(db: &DatabaseConnection, session: &PersistedAgentSession) {
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .exec(db)
        .await
        .unwrap();
}

fn params(
    session: &PersistedAgentSession,
    turn: &str,
    now: sea_orm::prelude::DateTimeUtc,
) -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: session.conversation_id.clone(),
        actor_id: session.actor_id.clone(),
        device_id: session.device_id.clone(),
        policy_revision: 1,
        current_pdp_scope: session.scope_snapshot.clone(),
        turn_id: turn.into(),
        request_id: None,
        connection_id: None,
        trigger_origin: TriggerOrigin::GoalContinuation,
        now: now.to_rfc3339(),
    }
}

async fn claimed_fixture(
    db: &DatabaseConnection,
    cid: &str,
    now: sea_orm::prelude::DateTimeUtc,
) -> ClaimedGoalSlice {
    claimed_fixture_on(db, cid, now, None).await
}

async fn claimed_fixture_on(
    db: &DatabaseConnection,
    cid: &str,
    now: sea_orm::prelude::DateTimeUtc,
    model_binding: Option<&desk_diagnose_core::goal::GoalModelBinding>,
) -> ClaimedGoalSlice {
    let goal = goal(cid, now.timestamp_millis() as u64 - 1_000);
    claimed_fixture_for_goal(db, goal, now, model_binding).await
}

async fn claimed_fixture_for_goal(
    db: &DatabaseConnection,
    goal: GoalRun,
    now: sea_orm::prelude::DateTimeUtc,
    model_binding: Option<&desk_diagnose_core::goal::GoalModelBinding>,
) -> ClaimedGoalSlice {
    let cid = &goal.conversation_id;
    seed_session(db, cid).await;
    let mut session = read_session(db, cid).await;
    session.latest_input_seq = 1;
    session.conversation.push(
        desk_diagnose_core::model_message_labels::model_bound_user_message(
            "message-1".into(),
            "Finish the report".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: goal.model_binding.connection_id.clone(),
                connection_revision: goal.model_binding.connection_revision,
                model_id: goal.model_binding.model_id.clone(),
                profile_revision: goal.model_binding.profile_revision.try_into().unwrap(),
            },
        )
        .unwrap(),
    );
    store_fixture(db, &session).await;
    insert(db, &goal).await;
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .unwrap();
    let requirement = session.conversation[0].clone();
    crate::agent_subagent_store::initialize_input_group_on(
        &txn,
        &mut session,
        requirement,
        None,
        Some(&goal),
        now.timestamp_millis(),
        Default::default(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    store_fixture(db, &session).await;
    claim_slice(
        db,
        &params(&session, "random-first-turn", now),
        &goal.goal_id,
        Some(goal.state_version),
        "coordinator-a",
        model_binding,
    )
    .await
    .unwrap()
    .unwrap()
}

async fn deliver_fixture(
    db: &DatabaseConnection,
    action: &ActionIdentity,
    cid: &str,
    now: sea_orm::prelude::DateTimeUtc,
) -> String {
    let mut session = read_session(db, cid).await;
    let original = crate::goal_work::original_on(db, &session, action)
        .await
        .unwrap()
        .unwrap();
    let event_id = original.event_id.unwrap();
    assert!(session.apply_completion(
        &event_id,
        &action.execution_id,
        &format!("call-{}", action.action_request_id),
        &action.action_request_id,
        "verified result",
        now.to_rfc3339()
    ));
    assert!(
        session.add_pending_auto_trigger(desk_diagnose_core::session::PendingAutoTrigger {
            work_id: action.work_id,
            kind: action.kind,
            execution_id: action.execution_id.clone(),
            tool_call_id: format!("call-{}", action.action_request_id),
            event_id: event_id.clone(),
            chain_id: session.chain_id.clone(),
            resolution_org_id: None,
            since: now.to_rfc3339(),
        })
    );
    // Formatting may already have externalized the output before the scheduler wakes.
    for message in &mut session.conversation {
        message.pending_delivery_format = None;
    }
    store_fixture(db, &session).await;
    event_id
}

#[tokio::test]
async fn a_claim_moves_the_goal_to_the_owner_current_model() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let current = desk_diagnose_core::goal::GoalModelBinding {
        connection_id: "gateway".into(),
        connection_revision: 2,
        profile_revision: 3,
        model_id: "new-model".into(),
    };
    let claimed = claimed_fixture_on(&db, "rebind", now, Some(&current)).await;
    assert_eq!(claimed.goal.model_binding, current);
}

#[tokio::test]
async fn native_wait_wakes_on_any_member_and_claims_with_other_work_running() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "wait-any", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    let b = add_work(&db, &claimed.session, "b").await;
    claimed.session.execution_state = ExecutionState::from_states(vec![
        ExecutionState::Executing { action: a.clone() },
        ExecutionState::Executing { action: b.clone() },
    ]);
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wait.members.len(), 2);
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting for results".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    assert_eq!(waiting.state, GoalState::Waiting(GoalWaitReason::Work));
    let deferred = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.state, waiting.state);
    complete_work(&db, &b).await;
    let event = deliver_fixture(&db, &b, "wait-any", now).await;
    let queued = wake_for_work_completion(&db, &deferred, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert_eq!(
        queued.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event.clone()]
    );
    let saved = read_session(&db, "wait-any").await;
    assert_eq!(saved.execution_state.tasks(), vec![&a]);
    assert_eq!(saved.pending_auto_triggers.len(), 1);
    let next = claim_slice(
        &db,
        &params(&saved, "another-turn", now),
        &queued.goal_id,
        Some(queued.state_version),
        "coordinator-b",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(next.goal.state, GoalState::Running);
    assert_eq!(
        next.session.pending_auto_triggers.len(),
        1,
        "claim must not acknowledge model reaction"
    );
    assert_eq!(
        next.goal.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event]
    );
    assert!(
        claim_slice(
            &db,
            &params(&saved, "competing-turn", now),
            &queued.goal_id,
            Some(queued.state_version),
            "coordinator-c",
            None
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        pause_if_unresolved_work(&db, &queued, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::goal_work::prepare_on(&db, &next.goal, &next.session, "wait-again")
            .await
            .unwrap()
            .unwrap()
            .members
            == vec![a]
    );
}

#[tokio::test]
async fn completion_between_wait_preparation_and_settlement_is_not_lost() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "register-race", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    complete_work(&db, &a).await;
    crate::goal_work::validate_wait_on(&db, &claimed.goal, &claimed.session, &wait)
        .await
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    deliver_fixture(&db, &a, "register-race", now).await;
    let mut saved = read_session(&db, "register-race").await;
    saved.pending_auto_triggers.clear();
    store_fixture(&db, &saved).await;
    assert_eq!(
        wake_for_work_completion(&db, &waiting, now)
            .await
            .unwrap()
            .unwrap()
            .state,
        GoalState::Queued
    );
}

#[tokio::test]
async fn normal_work_is_not_recovery_but_mixed_unknown_work_is() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "normal-and-unknown", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let queued = settle_slice(&db, &mut claimed.session, &GoalSegmentEnd::Yield, &[], now)
        .await
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert!(
        pause_if_unresolved_work(&db, &queued, now)
            .await
            .unwrap()
            .is_none()
    );
    let mut session = read_session(&db, "normal-and-unknown").await;
    session.execution_state = ExecutionState::Concurrent {
        tasks: vec![desk_diagnose_core::session::BackgroundExecution {
            action: a,
            unknown: None,
        }],
        interrupted_since: Some(now.to_rfc3339()),
    };
    store_fixture(&db, &session).await;
    let paused = pause_if_unresolved_work(&db, &queued, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    let mut saved = read_session(&db, "normal-and-unknown").await;
    let action = saved.execution_state.tasks()[0].clone();
    saved.execution_state = ExecutionState::Executing { action };
    store_fixture(&db, &saved).await;
    let resumed = apply_owner_action(
        &db,
        "normal-and-unknown",
        &saved.actor_id,
        &saved.device_id,
        &paused.goal_id,
        paused.state_version,
        GoalOwnerAction::Resume,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resumed.state, GoalState::Queued);
}

#[tokio::test]
async fn late_result_keeps_original_goal_owner_after_cancellation_and_new_goal() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "late-owner", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    let cancelled = apply_owner_action(
        &db,
        "late-owner",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &waiting.goal_id,
        waiting.state_version,
        GoalOwnerAction::Cancel,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    let mut new_goal = goal("late-owner", now.timestamp_millis() as u64);
    new_goal.goal_id = "new-goal".into();
    insert(&db, &new_goal).await;
    complete_work(&db, &a).await;
    deliver_fixture(&db, &a, "late-owner", now).await;
    let saved = read_session(&db, "late-owner").await;
    let owner = crate::goal_work::owner_on(&db, &saved, &a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.goal_id, waiting.goal_id);
    assert_eq!(owner.state, GoalState::Cancelled);
    assert!(
        wake_for_work_completion(&db, &cancelled, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        wake_for_work_completion(&db, &new_goal, now)
            .await
            .unwrap()
            .is_none()
    );
    let mut forged = a;
    forged.execution_id = "different-generation".into();
    assert!(
        crate::goal_work::owner_on(&db, &saved, &forged)
            .await
            .is_err()
    );
}

use crate::agent_background_task_store::{BackgroundTaskCreate, SignalBackgroundTaskStore};
use crate::entity::agent_action_item;
use desk_agent_protocol::capability_provider::{
    CapabilityEffect, CapabilityTaskRef, ExecutionPolicy,
};
use desk_diagnose_core::dynamic_run::{
    BACKGROUND_TASK_SCHEMA_VERSION, BackgroundTaskRecord, BackgroundTaskState,
};
use desk_diagnose_core::session::WorkKind;
async fn add_work(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    task: &str,
) -> ActionIdentity {
    let now = desk_utils::datetime::rfc3339(jiff::Timestamp::now());
    let row = SignalBackgroundTaskStore::new(db.clone())
        .create(BackgroundTaskCreate {
            actor_id: session.actor_id.clone(),
            policy_revision: 1,
            record: BackgroundTaskRecord {
                schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
                device_id: session.device_id.clone(),
                task: CapabilityTaskRef {
                    task_id: task.into(),
                    call_id: format!("call-{task}"),
                    run_id: session.conversation_id.clone(),
                    provider_id: "file.workspace".into(),
                    capability_id: "file.report.create".into(),
                    input_revision: 1,
                    generation: 1,
                },
                turn_id: session.current_turn_id.clone().unwrap(),
                tool_name: "create_report".into(),
                canonical_input_digest_sha256: "a".repeat(64),
                effect: CapabilityEffect::WriteArtifact,
                execution_policy: ExecutionPolicy::Adaptive {
                    foreground_budget_ms: 5000,
                },
                supports_cancel: true,
                state: BackgroundTaskState::Running,
                progress_sequence: 0,
                started_at: now.clone(),
                updated_at: now,
                terminal_at: None,
                cancel_request_id: None,
                result_envelope_ids: vec![],
            },
        })
        .await
        .unwrap();
    ActionIdentity::new(
        row.id,
        task,
        format!("{task}:1"),
        WorkKind::CapabilityProvider,
    )
}
async fn complete_work(db: &DatabaseConnection, action: &ActionIdentity) {
    let row = agent_action_item::Entity::find_by_id(action.work_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    SignalBackgroundTaskStore::new(db.clone())
        .apply_completion(
            &desk_agent_protocol::capability_provider::CapabilityCompletionEvent {
                task: record.task,
                sequence: 1,
                completion:
                    desk_agent_protocol::capability_provider::CapabilityCompletionClass::Succeeded,
                result_envelope_ids: vec![format!("result-{}", action.action_request_id)],
            },
            &desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_paused_goal_keeps_receipts_without_waking_planning() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "owner-pause", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    let paused = apply_owner_action(
        &db,
        "owner-pause",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &waiting.goal_id,
        waiting.state_version,
        GoalOwnerAction::Pause,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    complete_work(&db, &a).await;
    let event = deliver_fixture(&db, &a, "owner-pause", now).await;
    assert!(
        wake_for_work_completion(&db, &paused, now)
            .await
            .unwrap()
            .is_none()
    );
    let saved = read_session(&db, "owner-pause").await;
    assert!(
        saved
            .conversation
            .iter()
            .any(|message| message.message_id == event)
    );
    assert_eq!(saved.pending_auto_triggers.len(), 1);
}

#[tokio::test]
async fn ordinary_task_is_tracked_but_cannot_be_adopted_by_a_goal_wait() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "unrelated-work", now).await;
    let mut origin = claimed.session.clone();
    origin.current_turn_id = Some("ordinary-turn-outside-goal".into());
    let a = add_work(&db, &origin, "ordinary").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    assert!(
        crate::goal_work::all_tracked_on(&db, &claimed.session)
            .await
            .unwrap()
    );
    assert!(
        crate::goal_work::owner_on(&db, &claimed.session, &a)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ready_wait_and_unreacted_receipt_survive_a_model_outage() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "model-outage", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    complete_work(&db, &a).await;
    let event = deliver_fixture(&db, &a, "model-outage", now).await;
    let queued = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    let saved = read_session(&db, "model-outage").await;
    let mut next = claim_slice(
        &db,
        &params(&saved, "model-retry-turn", now),
        &queued.goal_id,
        Some(queued.state_version),
        "coordinator-b",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    next.session
        .finish_turn(TurnState::Failed, now.to_rfc3339());
    let model_wait = settle_slice(
        &db,
        &mut next.session,
        &GoalSegmentEnd::WaitForModel {
            retry_after_unix_ms: None,
        },
        &[],
        now,
    )
    .await
    .unwrap();
    assert_eq!(model_wait.state, GoalState::Waiting(GoalWaitReason::Model));
    assert_eq!(
        model_wait.work_wait.as_ref().unwrap().ready_event_ids,
        vec![event]
    );
    assert_eq!(
        read_session(&db, "model-outage")
            .await
            .pending_auto_triggers
            .len(),
        1
    );

    let paused = apply_owner_action(
        &db,
        "model-outage",
        &next.session.actor_id,
        &next.session.device_id,
        &model_wait.goal_id,
        model_wait.state_version,
        GoalOwnerAction::Pause,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    let resumed = apply_owner_action(
        &db,
        "model-outage",
        &next.session.actor_id,
        &next.session.device_id,
        &paused.goal_id,
        paused.state_version,
        GoalOwnerAction::Resume,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        resumed.work_wait.as_ref().unwrap().ready,
        "owner resume must retain unreacted results"
    );
}

#[tokio::test]
async fn an_unknown_native_outcome_does_not_wait_forever_after_metadata_delivery() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "unknown-outcome", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    unknown_work(&db, &a).await;
    let mut saved = read_session(&db, "unknown-outcome").await;
    saved.execution_state = ExecutionState::None;
    store_fixture(&db, &saved).await;
    let paused = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    assert_eq!(
        paused.status_reason.as_deref(),
        Some("reconcile_interrupted_work")
    );
}

async fn unknown_work(db: &DatabaseConnection, action: &ActionIdentity) {
    let row = agent_action_item::Entity::find_by_id(action.work_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    record.state = BackgroundTaskState::OutcomeUnknown;
    record.terminal_at = Some(desk_utils::datetime::rfc3339(jiff::Timestamp::now()));
    record.updated_at = record.terminal_at.clone().unwrap();
    record.validate().unwrap();
    agent_action_item::Entity::update_many()
        .set(agent_action_item::ActiveModel {
            status: Set("background_outcome_unknown".into()),
            payload_json: Set(serde_json::to_string(&record).unwrap()),
            result_json: Set(Some("{}".into())),
            ..Default::default()
        })
        .filter(agent_action_item::Column::Id.eq(action.work_id))
        .exec(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn provider_publication_precedes_model_reaction_and_is_replayable() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "published-not-reacted", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "wait")
        .await
        .unwrap()
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let waiting = settle_slice(
        &db,
        &mut claimed.session,
        &GoalSegmentEnd::Control(GoalControl::WaitForWork {
            wait,
            message: "Waiting".into(),
        }),
        &[],
        now,
    )
    .await
    .unwrap();
    complete_work(&db, &a).await;
    let store = SignalBackgroundTaskStore::new(db.clone());
    store.publish_once().await.unwrap();
    let row = agent_action_item::Entity::find_by_id(a.work_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.completion_delivery_state, "published");
    let first = read_session(&db, "published-not-reacted").await;
    assert_eq!(first.pending_auto_triggers.len(), 1);
    store.publish_once().await.unwrap();
    let second = read_session(&db, "published-not-reacted").await;
    assert_eq!(second.conversation, first.conversation);
    let queued = wake_for_work_completion(&db, &waiting, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.state, GoalState::Queued);
    assert!(queued.work_wait.unwrap().ready);
}

#[tokio::test]
async fn background_progress_can_advance_the_clock_without_losing_a_held_goal_turn() {
    use desk_diagnose_core::seam::SessionSeam;
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "clock-rebase", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .save(&mut claimed.session)
        .await
        .unwrap();
    let row = agent_action_item::Entity::find_by_id(a.work_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let record: BackgroundTaskRecord = serde_json::from_str(&row.payload_json).unwrap();
    SignalBackgroundTaskStore::new(db.clone())
        .apply_progress(
            &desk_agent_protocol::capability_provider::CapabilityProgressEvent {
                task: record.task,
                sequence: 1,
                message_key: Some("report.progress".into()),
                completed_units: None,
                total_units: None,
            },
            &desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
        )
        .await
        .unwrap();
    let previous = claimed.session.last_event_seq;
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .save(&mut claimed.session)
        .await
        .unwrap();
    assert_eq!(claimed.session.last_event_seq, previous + 1);
    let mut current = claimed.session.clone();
    current.version += 1;
    assert!(
        !crate::goal_work::rebase_background_events_on(&db, &current, &mut claimed.session)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn retention_preserves_claim_association_for_work_after_goal_cancellation() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "retained-origin", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    claimed
        .session
        .finish_turn(TurnState::Idle, now.to_rfc3339());
    let queued = settle_slice(&db, &mut claimed.session, &GoalSegmentEnd::Yield, &[], now)
        .await
        .unwrap();
    let cancelled = apply_owner_action(
        &db,
        "retained-origin",
        &claimed.session.actor_id,
        &claimed.session.device_id,
        &queued.goal_id,
        queued.state_version,
        GoalOwnerAction::Cancel,
        now,
    )
    .await
    .unwrap()
    .unwrap();

    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::UpdatedAt,
            Expr::value(
                now - desk_agent_data_store::datetime::database_interval(
                    jiff::SignedDuration::from_hours(960),
                ),
            ),
        )
        .filter(agent_session::Column::ConversationId.eq("retained-origin"))
        .exec(&db)
        .await
        .unwrap();
    crate::usage_retention::cleanup_once(&db, now)
        .await
        .unwrap();

    let saved = read_session(&db, "retained-origin").await;
    let owner = crate::goal_work::owner_on(&db, &saved, &a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.goal_id, cancelled.goal_id);
    assert_eq!(owner.state, GoalState::Cancelled);
    assert!(
        crate::goal_work::claim_for_turn(&db, "retained-origin", "random-first-turn")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_new_wait_can_track_earlier_work_of_the_resumed_goal() {
    let db = db().await;
    let now = desk_agent_data_store::datetime::utc_now();
    let mut claimed = claimed_fixture(&db, "resumed-dependency", now).await;
    let a = add_work(&db, &claimed.session, "a").await;
    claimed.session.execution_state = ExecutionState::Executing { action: a.clone() };
    let old_wait =
        crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "before-stop")
            .await
            .unwrap()
            .unwrap();
    claimed.session.control_revision += 1;
    assert!(!old_wait.matches(&claimed.goal, claimed.session.control_revision));
    let wait = crate::goal_work::prepare_on(&db, &claimed.goal, &claimed.session, "after-resume")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wait.members, vec![a]);
    assert_eq!(wait.control_revision, claimed.session.control_revision);
    crate::goal_work::validate_wait_on(&db, &claimed.goal, &claimed.session, &wait)
        .await
        .unwrap();
}

#[tokio::test]
async fn orphan_goal_uses_current_time_policy_before_deciding_whether_to_continue() {
    for finite in [false, true] {
        let db = db().await;
        let now = desk_agent_data_store::datetime::utc_now();
        let old = now
            - desk_agent_data_store::datetime::database_interval(jiff::SignedDuration::from_hours(
                192,
            ));
        let claimed = claimed_fixture(&db, "time-recovery", old).await;
        // Persist the inverse of today's policy to prove recovery reads authority.
        let mut stale = claimed.goal.clone();
        stale.deadline_unix_ms = if finite {
            i64::MAX as u64
        } else {
            stale.created_at_unix_ms + 7_200_000
        };
        goal_row::Entity::update_many()
            .set(goal_row::ActiveModel {
                state_json: Set(serde_json::to_string(&stale).unwrap()),
                absolute_deadline: Set(stale.deadline_unix_ms as i64),
                ..Default::default()
            })
            .filter(goal_row::Column::GoalId.eq(&stale.goal_id))
            .exec(&db)
            .await
            .unwrap();
        if finite {
            let mut policy = crate::goal_budget_policy::read(&db).await.unwrap();
            let revision = policy.revision;
            policy.limits.deadline_ms = Some(86_400_000);
            crate::goal_budget_policy::update(
                &db,
                &desk_agent_protocol::ai_agent::goal_budget::UpdateGoalBudgetPolicy {
                    expected_revision: revision,
                    limits: policy.limits,
                    owner_question_timeout_ms: policy.owner_question_timeout_ms,
                    device_unavailable_max_ms: policy.device_unavailable_max_ms,
                },
            )
            .await
            .unwrap();
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("time-recovery"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
        assert!(store.settle_lapsed_session(&row, now).await.unwrap());
        let current = goal_row::Entity::find()
            .filter(goal_row::Column::GoalId.eq(&stale.goal_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let current = decode(&current).unwrap();
        assert_eq!(
            current.state,
            if finite {
                GoalState::Failed
            } else {
                GoalState::Queued
            }
        );
        assert!(current.used.slices >= stale.used.slices);
        assert_eq!(current.goal_id, stale.goal_id);
        assert_eq!(current.source_message_id, stale.source_message_id);
        assert!(!store.settle_lapsed_session(&row, now).await.unwrap());
    }
}

#[tokio::test]
async fn owner_question_closes_once_at_its_server_deadline_and_resumes_the_same_goal() {
    use desk_agent_data_store::{
        decision_resume, owner_question as store,
        schedule::session::{OrgMode, SessionKey, SessionRows},
    };
    use desk_agent_protocol::ai_agent::owner_question::{OwnerAnswer, OwnerQuestionAction};
    use desk_diagnose_core::{
        chat::{ChatMessage, ToolCall},
        goal::GoalWaitReason,
        owner_question,
    };

    for (offset, expected) in [(-1, "answered"), (0, "timed_out"), (2_000, "answered")] {
        let db = db().await;
        let now = desk_agent_data_store::datetime::utc_now();
        let cid = "owner-question-boundary";
        let mut claimed = claimed_fixture(&db, cid, now).await;
        let call = ToolCall {
            id: "zone-call".into(),
            name: owner_question::TOOL.into(),
            arguments_json: r#"{"questions":[{"question":"Which time zone?"}]}"#.into(),
        };
        let mut issuing = ChatMessage::agent_tool_calls(
            "asked-zone",
            "Select the report time zone.",
            vec![call.to_ref()],
        );
        issuing.turn_id = claimed.session.current_turn_id.clone();
        issuing.set_label(Some(
            desk_diagnose_core::subagent::projection::envelope(
                "asked-zone",
                &issuing.text,
                "model-output",
                &[claimed.session.conversation[0]
                    .data_envelope
                    .clone()
                    .unwrap()],
            )
            .unwrap(),
        ));
        claimed.session.conversation.push(issuing);
        let pause = owner_question::publish(
            &mut claimed.session,
            &call,
            "asked-zone",
            "wait-zone".into(),
            now.timestamp_millis() as u64,
            Some(60_000),
            owner_question::parse(&call).unwrap(),
        )
        .unwrap();
        claimed.session.decision_pause = Some(pause.clone());
        claimed
            .session
            .finish_turn(TurnState::Idle, now.to_rfc3339());
        let wait = GoalWaitReason::OwnerQuestion;
        let goal = settle_slice(
            &db,
            &mut claimed.session,
            &GoalSegmentEnd::Control(GoalControl::Wait {
                reason: wait,
                reference_id: pause.id.clone(),
            }),
            &[],
            now,
        )
        .await
        .unwrap();
        assert_eq!(goal.state, GoalState::Waiting(wait));
        let deadline = claimed
            .session
            .owner_question
            .as_ref()
            .unwrap()
            .view
            .deadline_unix_ms
            .unwrap() as i64;
        assert!(
            store::due_on(&db, deadline - 1, 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store::due_on(&db, deadline, 1).await.unwrap()[0].wait_id,
            pause.id
        );

        if offset > 0 {
            let before = read_session(&db, cid).await;
            let interact = OwnerQuestionAction::Interact {};
            let response = |key| {
                crate::decision_resume::settle_question(
                    &db,
                    "1",
                    cid,
                    &pause.id,
                    Some((key, &interact)),
                )
            };
            let (first, retry) = tokio::join!(response("first-touch"), response("retry-touch"));
            assert_eq!(first.unwrap(), "interacting");
            assert_eq!(retry.unwrap(), "interacting");
            let editing = read_session(&db, cid).await;
            assert_eq!(editing.version, before.version + 1);
            assert_eq!(editing.conversation, before.conversation);
            assert_eq!(editing.decision_pause, before.decision_pause);
            assert!(editing.owner_question.as_ref().unwrap().open);
            assert!(
                editing
                    .owner_question
                    .as_ref()
                    .unwrap()
                    .view
                    .deadline_unix_ms
                    .is_none()
            );
            assert!(store::due_on(&db, i64::MAX, 1).await.unwrap().is_empty());
            assert!(
                decision_resume::pending_on(&db, 0, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        let txn = crate::db::begin_write(&db, agent_session::Entity)
            .await
            .unwrap();
        let host = crate::schedule_store::SignalScheduleHost;
        let row = host
            .find_session(
                &txn,
                SessionKey::Conversation {
                    conversation: cid,
                    actor: Some("1"),
                    device: Some("device-1"),
                },
                OrgMode::PersonalOnly,
                true,
            )
            .await
            .unwrap()
            .unwrap();
        let action = OwnerQuestionAction::Answer {
            answers: vec![OwnerAnswer::Text {
                value: "UTC".into(),
            }],
        };
        let now_ms = deadline + offset;
        assert_eq!(
            store::settle_on(
                &host,
                &txn,
                &row,
                &pause.id,
                Some(("submit-zone", &action)),
                now_ms
            )
            .await
            .unwrap(),
            expected
        );
        assert_eq!(
            store::settle_on(
                &host,
                &txn,
                &row,
                &pause.id,
                Some(("submit-zone", &action)),
                deadline + 1,
            )
            .await
            .unwrap(),
            expected
        );
        txn.commit().await.unwrap();

        assert!(store::due_on(&db, deadline, 1).await.unwrap().is_empty());
        let resumes = decision_resume::pending_on(&db, 0, 2).await.unwrap();
        assert_eq!(resumes.len(), 1);
        let when = sea_orm::prelude::DateTimeUtc::from_timestamp_millis(now_ms).unwrap();
        let waiting_session = read_session(&db, cid).await;
        assert!(matches!(
            wake_for_decision(&db, &waiting_session, &resumes[0], when)
                .await
                .unwrap(),
            GoalPermissionWake::Queued
        ));
        let session = read_session(&db, cid).await;
        let mut next = claim_slice(
            &db,
            &params(&session, "second-slice", when),
            &goal.goal_id,
            None,
            "second-node",
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(next.goal.goal_id, goal.goal_id);
        assert_eq!(next.goal.slice_seq, goal.slice_seq + 1);
        assert_eq!(next.goal.goal_revision, goal.goal_revision);
        let results: Vec<_> = next
            .session
            .conversation
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("zone-call"))
            .collect();
        assert_eq!(results.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&results[0].text).unwrap();
        assert_eq!(value["status"], expected);
        if expected == "timed_out" {
            assert_eq!(value["reason"], "user may be away");
            assert!(value.get("answers").is_none());
        } else {
            assert_eq!(value["answers"][0]["value"], "UTC");
        }
        if offset > 0 {
            let new_call = ToolCall {
                id: "format-call".into(),
                name: owner_question::TOOL.into(),
                arguments_json: r#"{"questions":[{"question":"Which format?"}]}"#.into(),
            };
            let mut issued =
                ChatMessage::agent_tool_calls("asked-format", "", vec![new_call.to_ref()]);
            issued.turn_id = next.session.current_turn_id.clone();
            next.session.conversation.push(issued);
            let new_wait = owner_question::publish(
                &mut next.session,
                &new_call,
                "asked-format",
                "wait-format".into(),
                now_ms as u64,
                Some(60_000),
                owner_question::parse(&new_call).unwrap(),
            )
            .unwrap();
            next.session.decision_pause = Some(new_wait.clone());
            next.session.finish_turn(TurnState::Idle, when.to_rfc3339());
            settle_slice(
                &db,
                &mut next.session,
                &GoalSegmentEnd::Control(GoalControl::Wait {
                    reason: GoalWaitReason::OwnerQuestion,
                    reference_id: new_wait.id.clone(),
                }),
                &[],
                when,
            )
            .await
            .unwrap();
            let published_at = next
                .session
                .decision_pause
                .as_ref()
                .unwrap()
                .requested_at_unix_ms as i64;
            let due = store::due_on(&db, published_at + 60_000, 2).await.unwrap();
            assert_eq!(due.len(), 1);
            assert_eq!(due[0].wait_id, new_wait.id);
            assert_eq!(due[0].deadline_ms, Some(published_at + 60_000));
        }
    }
}

#[tokio::test]
async fn directed_goal_reply_proposes_a_revision_and_requires_the_owner_decision() {
    use crate::{
        agent_goal_open_store, agent_session_store::SignalAgentSessionStore,
        config::ConfigConnection,
    };
    use desk_agent_protocol::ai_agent::owner_question::OwnerQuestionAction;
    use desk_diagnose_core::{
        chat::{ChatMessage, ChatRole, ToolCall},
        goal::{GoalModelBinding, GoalOpenRequest, GoalOpenRequestEvent, GoalOpenRequestState},
        goal_tools, owner_question,
        proposal_review::DecisionKind,
        registry,
        seam::SessionSeam,
    };

    for decision in [None, Some(false), Some(true)] {
        let db = db().await;
        db.config_context()
            .update::<_, DbErr, _>(|file| {
                file.ai_gateway.base_url = Some("http://127.0.0.1:1".into());
                file.ai_gateway.max_context_bytes = Some(1_048_576);
                file.ai_gateway.model = Some("revision-model".into());
                file.ai_gateway.api_key = Some("fixture-key".into());
                file.ai_gateway.wire_protocol =
                    Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions);
                file.ai_gateway.output_limit_field =
                    desk_diagnose_core::model_profile::OutputLimitField::MaxTokens;
                Ok(Some(()))
            })
            .await
            .unwrap();
        let destination = crate::model_provider::load(&db)
            .await
            .unwrap()
            .destination_identity()
            .unwrap();
        let binding = GoalModelBinding::from_destination(&destination).unwrap();
        let cid = "directed-goal";
        let now = desk_agent_data_store::datetime::utc_now();
        let mut original_goal = goal(cid, now.timestamp_millis() as u64 - 1_000);
        original_goal.model_binding = binding.clone();
        let mut claimed = claimed_fixture_for_goal(&db, original_goal, now, Some(&binding)).await;
        let target = claimed.goal.execution_target.clone();
        claimed
            .session
            .bind_agent_input_targets(std::slice::from_ref(&target))
            .unwrap();
        let mut prompt = ChatMessage::text(
            "adjustment-question",
            ChatRole::Assistant,
            "What should change?",
        )
        .with_turn_id("random-first-turn");
        let source = claimed.session.conversation[0]
            .data_envelope
            .clone()
            .unwrap();
        prompt.set_label(Some(
            desk_diagnose_core::subagent::projection::envelope(
                &prompt.message_id,
                &prompt.text,
                "model-output",
                &[source],
            )
            .unwrap(),
        ));
        claimed.session.conversation.push(prompt);
        let pause = owner_question::publish_follow_up(
            &mut claimed.session,
            "random-first-turn",
            "adjust-report".into(),
            now.timestamp_millis() as u64,
            Some(60_000),
        )
        .unwrap();
        claimed.session.decision_pause = Some(pause.clone());
        claimed
            .session
            .finish_turn(TurnState::Idle, now.to_rfc3339());
        let original = settle_slice(
            &db,
            &mut claimed.session,
            &GoalSegmentEnd::Control(GoalControl::Wait {
                reason: GoalWaitReason::OwnerQuestion,
                reference_id: pause.id.clone(),
            }),
            &[],
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            crate::decision_resume::settle_question(
                &db,
                "1",
                cid,
                &pause.id,
                Some(("typing-adjustment", &OwnerQuestionAction::Interact {}))
            )
            .await
            .unwrap(),
            "interacting"
        );
        assert!(
            desk_agent_data_store::decision_resume::pending_on(&db, 0, 1)
                .await
                .unwrap()
                .is_empty()
        );
        let revised_text = "Finish the report in French";
        let action = OwnerQuestionAction::Reply {
            text: if decision.is_some() {
                revised_text
            } else {
                "Keep the original report"
            }
            .into(),
        };
        assert_eq!(
            crate::decision_resume::settle_question(
                &db,
                "1",
                cid,
                &pause.id,
                Some(("adjust-report-submit", &action))
            )
            .await
            .unwrap(),
            "replied"
        );

        let pending = || desk_agent_data_store::decision_resume::pending_on(&db, 0, 2);
        let session = read_session(&db, cid).await;
        let records = pending().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(session.input_revision, original.input_revision);
        let now = desk_agent_data_store::datetime::utc_now();
        assert!(matches!(
            wake_for_decision(&db, &session, &records[0], now)
                .await
                .unwrap(),
            GoalPermissionWake::Held
        ));
        let session = read_session(&db, cid).await;
        let waiting = load_for_subject(&db, cid, "1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.state, GoalState::Waiting(GoalWaitReason::User));
        assert_eq!(
            waiting.user_reply_floor_revision,
            Some(original.input_revision)
        );
        assert_eq!(waiting.input_revision, original.input_revision + 1);
        assert_eq!(session.input_revision, waiting.input_revision);
        assert_eq!(session.single_agent_request_target().unwrap(), &target);
        let receipt = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::RunId.eq(cid))
            .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.input_revision, waiting.input_revision as i64);
        assert_eq!(
            receipt.correlation_id.as_deref(),
            Some("owner-response-adjust-report")
        );
        let records = pending().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_revision, waiting.input_revision as i64);
        assert!(matches!(
            wake_for_decision(&db, &session, &records[0], now)
                .await
                .unwrap(),
            GoalPermissionWake::NoGoal
        ));

        let store = SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(None, AgentSessionSurface::AiAgent)
            .with_expected_input_revision(session.input_revision)
            .with_decision_resume(DecisionKind::OwnerQuestion, pause.id.clone());
        let mut claim = params(&session, "adjustment-planning", now);
        claim.trigger_origin = TriggerOrigin::ProposalDecision;
        let mut planning = store.claim_turn(claim).await.unwrap();
        assert!(owner_question::planning_reply(&planning));
        assert!(
            registry::lookup_for_session(
                &goal_tools::open_registry(),
                goal_tools::REQUEST_GOAL_TOOL_NAME,
                &planning
            )
            .is_some()
        );
        let Some(approve) = decision else {
            planning.conversation.push(
                ChatMessage::text(
                    "clarified",
                    ChatRole::Assistant,
                    "I will finish the original report.",
                )
                .with_turn_id("adjustment-planning"),
            );
            planning.finish_turn(TurnState::Idle, now.to_rfc3339());
            store.save(&mut planning).await.unwrap();
            let records = desk_agent_data_store::decision_resume::started_on(&db, 2)
                .await
                .unwrap();
            assert_eq!(records.len(), 1);
            let when = desk_agent_data_store::datetime::utc_now();
            assert!(matches!(
                wake_for_decision(&db, &planning, &records[0], when)
                    .await
                    .unwrap(),
                GoalPermissionWake::Queued
            ));
            let session = read_session(&db, cid).await;
            let next = claim_slice(
                &db,
                &params(&session, "continue-original", when),
                &original.goal_id,
                None,
                "second-node",
                None,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(next.goal.goal_text, original.goal_text);
            assert_eq!(next.goal.goal_revision, original.goal_revision);
            assert_eq!(next.goal.source_message_id, original.source_message_id);
            continue;
        };
        let call = ToolCall {
            id: "propose-french".into(),
            name: goal_tools::REQUEST_GOAL_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"goal_text": revised_text}).to_string(),
        };
        let parsed = goal_tools::parse_open(&call).unwrap();
        let source = owner_question::directed_goal_reply(&planning)
            .unwrap()
            .message_id
            .clone();
        let request = GoalOpenRequest::new_revision(
            "revision-of-report".into(),
            &waiting,
            source,
            planning.input_revision,
            parsed.goal_text,
            binding,
            now.timestamp_millis() as u64,
        )
        .unwrap();
        planning.conversation.push(
            ChatMessage::agent_tool_calls("proposed-french", "", vec![call.to_ref()])
                .with_turn_id("adjustment-planning"),
        );
        planning.conversation.push(
            ChatMessage::tool_result(
                "revision-submitted",
                &call.id,
                serde_json::json!({"status":"pending_approval","request_id":request.request_id,"goal_text":revised_text}).to_string(),
            ).with_turn_id("adjustment-planning")
        );
        planning.last_event_seq += 1;
        let event = GoalOpenRequestEvent::new(
            &request,
            AgentRunEventKind::GoalOpenRequested,
            planning.last_event_seq,
            now.to_rfc3339(),
        )
        .unwrap();
        agent_goal_open_store::save_model_request(&db, &mut planning, &event)
            .await
            .unwrap();
        planning.finish_turn(TurnState::Idle, now.to_rfc3339());
        store.save(&mut planning).await.unwrap();
        let (decided, change) = agent_goal_open_store::decide_for_subject(
            &db,
            cid,
            "1",
            "device-1",
            &request.request_id,
            approve,
            desk_agent_data_store::datetime::utc_now(),
        )
        .await
        .unwrap()
        .unwrap();
        let latest = load_for_subject(&db, cid, "1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.goal_id, original.goal_id);
        assert_eq!(
            latest.original_source_message_id,
            original.original_source_message_id
        );
        assert_eq!(latest.execution_target, target);
        if approve {
            assert_eq!(decided.state, GoalOpenRequestState::Approved);
            assert_eq!(change.unwrap(), latest);
            assert_eq!(latest.goal_revision, original.goal_revision + 1);
            assert_eq!(latest.goal_text, revised_text);
            assert_eq!(latest.state, GoalState::Queued);
            let resumed = read_session(&db, cid).await;
            let next = claim_slice(
                &db,
                &params(
                    &resumed,
                    "revised-slice",
                    desk_agent_data_store::datetime::utc_now(),
                ),
                &latest.goal_id,
                Some(latest.state_version),
                "second-node",
                None,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(next.goal.goal_revision, latest.goal_revision);
            assert_eq!(next.goal.source_message_id, "owner-response-adjust-report");
        } else {
            assert_eq!(decided.state, GoalOpenRequestState::Denied);
            assert!(change.is_none());
            assert_eq!(latest.goal_revision, original.goal_revision);
            assert_eq!(latest.goal_text, original.goal_text);
        }
    }
}
