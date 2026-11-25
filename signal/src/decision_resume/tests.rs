use super::*;
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    ai_agent::owner_question::{OwnerAnswer, OwnerQuestionAction, OwnerQuestionResult},
};
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall},
    owner_question,
    session::{AgentSessionSurface, TriggerOrigin},
};
use sea_orm::{ActiveModelTrait, PaginatorTrait, Set};

#[tokio::test]
async fn owner_question_deadline_and_late_answer_commit_one_timeout_and_resume() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let mut session = PersistedAgentSession::new(
        "goal-conversation",
        "1",
        "host",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-14T00:00:00Z",
    );
    session.adopt_client_metadata(Some("client-goal"), AgentSessionSurface::AiAgent);
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session
        .begin_goal_segment("goal", "owner-goal", 1, 1, 1)
        .unwrap();
    session.trigger_origin = TriggerOrigin::GoalContinuation;
    session.current_turn_id = Some("goal-turn".into());
    let call = ToolCall {
        id: "goal-call".into(),
        name: owner_question::TOOL.into(),
        arguments_json: r#"{"questions":[{"question":"Which timezone?"}]}"#.into(),
    };
    session.conversation.push(
        ChatMessage::agent_tool_calls("issuer", "", vec![call.to_ref()]).with_turn_id("goal-turn"),
    );
    let wait_id = "goal-wait";
    session.decision_pause = Some(
        owner_question::publish(
            &mut session,
            &call,
            "issuer",
            wait_id.into(),
            1,
            Some(60_000),
            owner_question::parse(&call).unwrap(),
        )
        .unwrap(),
    );
    let now = desk_agent_data_store::datetime::utc_now();
    agent_session::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        conversation_schema_version: Set(Some(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        ))),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(0),
        lease_deadline: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    desk_agent_data_store::owner_question::publish_on(&db, &session)
        .await
        .unwrap();
    use desk_agent_data_store::owner_question::due_on;
    assert!(due_on(&db, 60_000, 1).await.unwrap().is_empty());
    assert_eq!(due_on(&db, 60_001, 1).await.unwrap()[0].wait_id, wait_id);
    let late = OwnerQuestionAction::Answer {
        answers: vec![OwnerAnswer::Text {
            value: "discard this late answer".into(),
        }],
    };
    let settle = |action| settle_question(&db, "1", "goal-conversation", wait_id, action);
    let (scanner, answer) = tokio::join!(settle(None), settle(Some(("late", &late))));
    assert_eq!(scanner.unwrap(), "timed_out");
    assert_eq!(answer.unwrap(), "timed_out");
    assert_eq!(settle(None).await.unwrap(), "timed_out");
    assert!(due_on(&db, i64::MAX, 1).await.unwrap().is_empty());
    let row = agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.version, session.version + 1);
    assert!(!row.state_json.contains("discard this late answer"));
    let saved = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(saved.conversation.len(), session.conversation.len() + 1);
    let result = saved.conversation.last().unwrap();
    assert_eq!(result.tool_call_id.as_deref(), Some("goal-call"));
    assert_eq!(result.turn_id.as_deref(), Some("goal-turn"));
    assert_eq!(
        serde_json::from_str::<OwnerQuestionResult>(&result.text).unwrap(),
        OwnerQuestionResult::timed_out()
    );
    assert_eq!(
        agent_decision_resume::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        1
    );
    let resume = agent_decision_resume::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resume.state, decision_resume::PENDING);
}

#[tokio::test]
async fn owner_question_deadline_scan_moves_past_unsettled_and_tied_deadlines() {
    use desk_agent_data_store::{
        entity::agent_owner_question as wait, owner_question::due_after_on,
    };
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    for (id, deadline) in [("b", 100), ("c", 101), ("a", 100)] {
        wait::ActiveModel {
            wait_id: Set(id.into()),
            conversation_id: Set(format!("conversation-{id}")),
            actor_id: Set("1".into()),
            deadline_ms: Set(Some(deadline)),
            status: Set("open".into()),
            client_request_id: Set(None),
        }
        .insert(&db)
        .await
        .unwrap();
    }
    let first = due_after_on(&db, 100, 1, None).await.unwrap();
    assert_eq!(first[0].wait_id, "a");
    // An inaccessible first row can remain open across scans.
    assert!(
        settle_question(&db, "1", "conversation-a", "a", None)
            .await
            .is_err()
    );
    let after_first = Some((100, first[0].wait_id.clone()));
    let next = due_after_on(&db, 100, 2, after_first.as_ref())
        .await
        .unwrap();
    assert_eq!(
        next.iter().map(|w| w.wait_id.as_str()).collect::<Vec<_>>(),
        ["b"]
    );
    let after_second = Some((100, next[0].wait_id.clone()));
    assert!(
        due_after_on(&db, 100, 2, after_second.as_ref())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        due_after_on(&db, 101, 2, after_second.as_ref())
            .await
            .unwrap()[0]
            .wait_id,
        "c"
    );
}
