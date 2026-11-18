use super::*;

#[test]
fn queued_receipts_show_only_owner_text() {
    let owner = serde_json::json!({
        "ask": { "question": "Check the disk", "client_message_id": "m1" },
        "device_id": "",
        "targets": [],
    });
    assert_eq!(
        owner_question(&owner.to_string()).unwrap().as_deref(),
        Some("Check the disk")
    );
    let subscription = serde_json::json!({
        "issue_source": { "request_id": "issue-1", "anything": ["kept opaque"] },
        "ask": { "question": "Generated requirement" },
    });
    assert_eq!(owner_question(&subscription.to_string()).unwrap(), None);
    assert!(owner_question("{}").is_err());
}

/// A released call holds the input slot only while its turn runs: once the turn
/// settles, even as failed, queued input may start.
#[test]
fn a_failed_turn_no_longer_holds_the_input_slot_for_its_released_call() {
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session = PersistedAgentSession::new("root", "owner", "", 1, scope, "now");
    session
        .begin_turn(
            "turn-1",
            None,
            None,
            1,
            session.scope_snapshot.clone(),
            "now",
        )
        .unwrap();
    session.released_permission_call = Some(desk_diagnose_core::session::AwaitingPermission {
        tool_call_id: "call-1".into(),
        request_id: "request-1".into(),
    });
    assert!(holds_input_slot(&session));
    session.finish_turn(desk_diagnose_core::session::TurnState::Failed, "now");
    assert!(!holds_input_slot(&session));
}
