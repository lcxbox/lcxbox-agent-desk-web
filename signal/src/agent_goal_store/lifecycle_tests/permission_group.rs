//! A goal waiting on a step's approval group wakes once the whole group is
//! decided, whichever member is decided last.
use super::*;
use desk_diagnose_core::dynamic_run::{
    PermissionDecisionActor, PermissionDecisionProjection, PermissionItemDisposition,
    PermissionRequestState,
};

fn grouped_request(
    request_id: &str,
    order: u16,
) -> desk_diagnose_core::dynamic_run::PermissionRequest {
    serde_json::from_value(serde_json::json!({
        "schema_version": desk_diagnose_core::dynamic_run::PERMISSION_REQUEST_SCHEMA_VERSION,
        "request_id": request_id, "input_revision": 1, "state": "pending",
        "created_at": Utc::now().to_rfc3339(), "items": [{
            "item_id": format!("call-{request_id}"), "provider_id": "desktop.session",
            "tool_name": "inspect_desktop_session", "expected_effect": "read_device",
            "resource_scope": ["target:current_device"], "operation_scope": ["observe"],
            "suggested_ttl_seconds": 120, "suggested_max_uses": 1, "reason": "Inspect"
        }],
        "group": { "group_id": "group-1", "order": order, "size": 2 }
    }))
    .unwrap()
}

async fn session_of(
    db: &DatabaseConnection,
    cid: &str,
) -> (agent_session::Model, PersistedAgentSession) {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(cid))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    (row, session)
}

async fn write(
    db: &DatabaseConnection,
    row: &agent_session::Model,
    session: &PersistedAgentSession,
) {
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::Id.eq(row.id))
        .exec(db)
        .await
        .unwrap();
}

/// Record an owner decision of `request_id` in the session projection and
/// add its pending resume row, as the decision transaction does.
async fn decide(db: &DatabaseConnection, cid: &str, request_id: &str) {
    let (row, mut session) = session_of(db, cid).await;
    let request = session
        .permission_requests
        .iter_mut()
        .find(|request| request.request_id == request_id)
        .unwrap();
    request.state = PermissionRequestState::Approved;
    let item_id = request.items[0].item_id.clone();
    session
        .permission_decisions
        .push(PermissionDecisionProjection {
            request_id: request_id.into(),
            input_revision: 1,
            actor: PermissionDecisionActor::Owner,
            items: vec![PermissionItemDisposition {
                item_id,
                approved: true,
                actor: PermissionDecisionActor::Owner,
            }],
        });
    write(db, &row, &session).await;
    resume_row::ActiveModel {
        permission_id: Set(format!("{cid}:{request_id}")),
        decision_event_id: Set(format!("decided-{request_id}")),
        run_id: Set(cid.into()),
        request_id: Set(request_id.into()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        input_revision: Set(1),
        state: Set("pending".into()),
        turn_id: Set(None),
        version: Set(1),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn resume_state(db: &DatabaseConnection, cid: &str, request_id: &str) -> String {
    resume_row::Entity::find()
        .filter(resume_row::Column::RunId.eq(cid))
        .filter(resume_row::Column::RequestId.eq(request_id))
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .state
}

#[tokio::test]
async fn goal_wake_waits_for_the_whole_group_in_both_decision_orders() {
    for (first, last) in [("request-1", "request-2"), ("request-2", "request-1")] {
        let db = db().await;
        let cid = format!("group-wake-{first}");
        seed_session(&db, &cid).await;
        let (row, mut session) = session_of(&db, &cid).await;
        session
            .permission_requests
            .push(grouped_request("request-1", 0));
        session
            .permission_requests
            .push(grouped_request("request-2", 1));
        write(&db, &row, &session).await;
        // The goal waits on the step's first request, the held call's.
        let mut waiting = goal(&cid, Utc::now().timestamp_millis() as u64);
        waiting.state = GoalState::Waiting(GoalWaitReason::Approval);
        waiting.status_reason = Some("request-1".into());
        insert(&db, &waiting).await;

        decide(&db, &cid, first).await;
        let wake = wake_for_permission_decision(&db, &cid, "1", "device-1", first, Utc::now())
            .await
            .unwrap();
        assert_eq!(wake, GoalPermissionWake::Held, "{first} decided first");
        assert_eq!(resume_state(&db, &cid, first).await, "pending");

        decide(&db, &cid, last).await;
        let wake = wake_for_permission_decision(&db, &cid, "1", "device-1", last, Utc::now())
            .await
            .unwrap();
        assert_eq!(wake, GoalPermissionWake::Queued, "{last} decided last");
        // The woken goal handles the whole group: no member resumes alone.
        assert_eq!(resume_state(&db, &cid, "request-1").await, "goal_handled");
        assert_eq!(resume_state(&db, &cid, "request-2").await, "goal_handled");
    }
}
