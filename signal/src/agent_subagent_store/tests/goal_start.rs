//! A goal paused because its segment cannot start admits no child request.
use super::*;
use desk_diagnose_core::goal::{GOAL_CONTINUATION_FAILED, GoalOwnerAction, GoalPauseReason};

#[tokio::test]
async fn start_failure_pause_closes_child_admission_until_the_owner_cancels() {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    super::creation::runnable_parent(&db).await;
    super::input_sources::release_parent(&db).await;
    super::input_sources::append_input(&db, true, "start-failure-goal").await;
    let store = SubAgentStore::new(db.clone());
    let task = super::input_sources::spawn_goal_child(&db, &store).await;
    super::input_sources::release_parent(&db).await;
    let goal = super::input_sources::goal(&db).await;
    assert_eq!(goal.state, GoalState::Queued);
    assert!(
        store
            .child_runtime_candidate(&task.task_id)
            .await
            .unwrap()
            .is_some()
    );
    let paused = crate::agent_goal_store::pause_for_start_failure(
        &db,
        &goal,
        desk_agent_data_store::datetime::utc_now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Recovery));
    assert_eq!(
        paused.status_reason.as_deref(),
        Some(GOAL_CONTINUATION_FAILED)
    );
    let frozen = load(&db, &task.task_id).await;
    assert_eq!(frozen.state, SubAgentState::WaitingSource);
    assert!(
        store
            .child_runtime_candidate(&task.task_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!matches!(
        store
            .claim_child(
                &super::creation::claim_params(&frozen),
                &task.task_id,
                frozen.fence(),
                &super::creation::destination(),
            )
            .await,
        Ok(SubAgentClaimOutcome::Claimed(_))
    ));
    let cancelled = super::paused_permission::control(&db, &paused, GoalOwnerAction::Cancel)
        .await
        .unwrap();
    assert_eq!(cancelled.state, GoalState::Cancelled);
    assert_eq!(
        load(&db, &task.task_id).await.state,
        SubAgentState::Cancelled
    );
}
