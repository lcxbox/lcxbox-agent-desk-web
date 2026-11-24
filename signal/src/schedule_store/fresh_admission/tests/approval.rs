//! Owner approval of a held fresh-task call through the real decision and claim paths.
use super::*;
use crate::agent_session_store::{
    PermissionDecisionSubject, PermissionGrantIssuanceContext, SignalAgentSessionStore,
};
use desk_agent_protocol::capability_provider::ProductSurface;
use desk_diagnose_core::{
    capability_availability::CapabilityAvailability,
    chat::{ChatMessage, ToolCallRef},
    dynamic_run::{
        AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, PermissionDecisionItem,
        PermissionItemDecision, PermissionRequestState, PermissionRequestedEvent,
    },
    seam::SessionSeam,
    session::TurnState,
};

const HELD_CALL: &str = "held-call";
const TOOL: &str = desk_diagnose_core::command_confirmation::COMMAND_TOOL;

#[actix_web::test]
async fn approved_fresh_task_claim_releases_the_held_call_for_the_server_to_run() {
    approve_held_command(false).await;
}

/// The device's command policy changed after the request (for example its
/// worker restarted under a new session): the owner's approval cannot be
/// granted, so it is recorded as a system refusal the model sees instead of
/// failing and leaving the card pending.
#[actix_web::test]
async fn approving_a_command_whose_policy_changed_records_a_system_refusal() {
    approve_held_command(true).await;
}

async fn approve_held_command(policy_changed: bool) {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let (store, task) = enabled_fresh_task_on(db.clone(), None, 1, None).await;
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "fresh-approval")
        .await
        .unwrap();
    let connections = SharedConnectionMap::new();
    connections
        .write()
        .await
        .insert("node".into(), peer("node", &task.target_device_id).await);
    let gate = AiAgentGate::new(AiAgentSettings {
        revision: 1,
        enabled: true,
    });
    let claim = || FreshTaskClaim {
        owner: 1,
        run_id: &queued.run_id,
        node_id: "scheduler",
        lease_seconds: 90,
        policy_revision: 1,
        scope: AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        },
    };
    let mut session = store
        .claim_fresh_task(&connections, &gate, claim())
        .await
        .unwrap()
        .session;

    // The loop held the one call the request covers, as the task pause does.
    let arguments = desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        TOOL,
        serde_json::json!({"schema_version":2,"shell":"bash","command":"pwd","timeout_ms":10000}),
    )
    .unwrap();
    session.conversation.push(
        ChatMessage::agent_tool_calls(
            "held-proposal",
            "",
            vec![ToolCallRef {
                id: HELD_CALL.into(),
                name: TOOL.into(),
                arguments_json: arguments.clone(),
            }],
        )
        .with_turn_id(session.current_turn_id.clone().unwrap()),
    );
    // The production edge supplies the device command policy to the registry.
    let mut policy = desk_diagnose_core::command_confirmation::test_policy();
    policy.actor_id = session.actor_id.clone();
    policy.target_device_id = session.device_id.clone();
    policy.policy_revision = session.policy_revision;
    policy.directory_grants = desk_diagnose_core::file_scope::TrustedDirectoryGrants::from_session(
        &session,
        &session.device_id,
        jiff::Timestamp::now().as_millisecond() as u64,
    );
    let registry =
        desk_diagnose_core::ai_agent::ai_agent_provider_registry().with_command_policy(policy);
    let created_at = desk_utils::datetime::rfc3339(jiff::Timestamp::now());
    let request = desk_diagnose_core::permission_tools::build_permission_request(
        &desk_diagnose_core::chat::ToolCall {
            id: HELD_CALL.into(),
            name: desk_diagnose_core::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({ "items": [{
                "item_id": HELD_CALL, "tool_name": TOOL,
                "exact_input": serde_json::from_str::<serde_json::Value>(&arguments).unwrap(),
                "suggested_ttl_seconds": 120, "suggested_max_uses": 1,
                "reason": "This scheduled operation needs approval before it runs."
            }] })
            .to_string(),
        },
        &registry,
        "task-permission-held".into(),
        session.input_revision,
        created_at.clone(),
    )
    .unwrap();
    session.add_permission_request(request.clone()).unwrap();
    session.last_event_seq += 1;
    let event = PermissionRequestedEvent {
        approval_mode_revision: None,
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "task-permission-requested".into(),
            run_id: session.conversation_id.clone(),
            event_seq: session.last_event_seq,
            input_revision: session.input_revision,
            kind: AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at,
        },
        request: request.clone(),
    };
    let sessions = SignalAgentSessionStore::new(db.clone());
    session.pause_for_permission(desk_diagnose_core::session::AwaitingPermission {
        tool_call_id: HELD_CALL.into(),
        request_id: request.request_id.clone(),
        ..Default::default()
    });
    sessions
        .save_permission_request(&mut session, &event)
        .await
        .unwrap();
    session.finish_turn(
        TurnState::Idle,
        desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
    );
    session.handled_input_seq = session.latest_input_seq;
    sessions.save(&mut session).await.unwrap();
    let run = || async {
        agent_schedule_run::Entity::find_by_id(queued.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
    };
    store
        .await_fresh_task_permission(
            crate::schedule_store::FreshTaskLease {
                owner: 1,
                run_id: &queued.run_id,
                node_id: "scheduler",
                run_epoch: run().await.lease_epoch,
                session_token: session.lease_token,
            },
            &request.request_id,
        )
        .await
        .unwrap();
    let waiting = run().await;
    assert_eq!(waiting.status, "awaiting_permission");
    assert_eq!(
        waiting.result_ref.as_deref(),
        Some("permission:task-permission-held")
    );

    // The owner approves through the decision path the host serves.
    let registry = if policy_changed {
        let mut changed = desk_diagnose_core::command_confirmation::test_policy();
        changed.actor_id = session.actor_id.clone();
        changed.target_device_id = session.device_id.clone();
        changed.policy_revision = session.policy_revision;
        changed.target_session_id = "restarted-worker".into();
        changed.directory_grants =
            desk_diagnose_core::file_scope::TrustedDirectoryGrants::from_session(
                &session,
                &session.device_id,
                jiff::Timestamp::now().as_millisecond() as u64,
            );
        registry.with_command_policy(changed)
    } else {
        registry
    };
    let capability = registry.capability_for_tool(TOOL).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let inventory = [CapabilityAvailability {
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: TOOL.into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let item = &request.items[0];
    assert_eq!(item.suggested_max_uses, 1);
    let decided = sessions
        .decide_permission_request_with_expected_request(
            PermissionDecisionSubject {
                conversation_id: &queued.run_id,
                actor_id: "1",
                device_id: &task.target_device_id,
            },
            &request.request_id,
            vec![PermissionDecisionItem {
                item_id: HELD_CALL.into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: item.resource_scope.clone(),
                    operation_scope: item.operation_scope.clone(),
                    export_destinations: item.export_destinations.clone(),
                    ttl_seconds: 120,
                    max_uses: 1,
                },
            }],
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: jiff::Timestamp::now().as_millisecond() as u64,
                implicit_fresh_object_refs: &[],
            },
            &desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
            None,
            None,
        )
        .await
        .unwrap();
    if policy_changed {
        assert_eq!(decided.state, PermissionRequestState::Denied);
        let event = sessions
            .permission_decision_event(
                PermissionDecisionSubject {
                    conversation_id: &queued.run_id,
                    actor_id: "1",
                    device_id: &task.target_device_id,
                },
                &request.request_id,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &event.decision_source,
            desk_diagnose_core::dynamic_run::PermissionDecisionSource::SystemDecision {
                reason_code,
                ..
            } if reason_code
                == desk_diagnose_core::permission_grant::StaleApprovalTarget::STATE_CHANGED_CODE
        ));
        // Nothing was granted for the refused call.
        assert!(
            event
                .items
                .iter()
                .all(|item| item.decision == PermissionItemDecision::Deny)
        );
        return;
    }
    assert_eq!(decided.state, PermissionRequestState::Approved);

    let resumed = store
        .claim_fresh_task(&connections, &gate, claim())
        .await
        .unwrap()
        .session;
    assert!(resumed.awaiting_permission.is_none());
    assert_eq!(
        resumed
            .released_permission_call
            .as_ref()
            .map(|released| released.tool_call_id.as_str()),
        Some(HELD_CALL)
    );
    // The approved call was not closed: the server runs it in the resumed turn.
    assert!(
        !resumed
            .conversation
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some(HELD_CALL))
    );
    assert_eq!(resumed.turn_state, TurnState::Running);
    // The claim committed the released call with the session it returned.
    let stored = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&queued.run_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&stored.state_json).unwrap(),
        resumed
    );
    let running = run().await;
    assert_eq!(running.status, "running");
    assert_eq!(running.lease_epoch, waiting.lease_epoch + 1);
}
