//! Approval groups: one model step's requests saved together, resumed only
//! once every request is decided, and granted by the settling decision.
use super::*;
use crate::entity::agent_permission_resume as resume;

const GROUP: &str = "group-1";

fn item(device: &str) -> GrantRequestItem {
    GrantRequestItem {
        command_confirmation: None,
        selected_device_target: Some(desk_diagnose_core::request_devices::SelectedDeviceTarget {
            device_id: device.to_owned(),
            target_ref: device.to_owned(),
            connection_id: "host".into(),
            readiness_revision: 1,
        }),
        launch_confirmation: None,
        item_id: "first".into(),
        provider_id: "desktop.session".into(),
        tool_name: "inspect_desktop_session".into(),
        expected_effect: CapabilityEffect::ReadDevice,
        resource_scope: vec![format!("target:{device}")],
        operation_scope: vec!["observe".into()],
        export_destinations: vec![],
        canonical_input_json: None,
        canonical_input_digest_sha256: None,
        suggested_ttl_seconds: 300,
        suggested_max_uses: 2,
        reason: "Inspect selected target".into(),
    }
}

fn member(request_id: &str, order: u16, device: &str) -> PermissionRequest {
    PermissionRequest {
        input_source: None,
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: request_id.into(),
        input_revision: 1,
        state: PermissionRequestState::Pending,
        items: vec![item(device)],
        created_at: Utc::now().to_rfc3339(),
        group: Some(desk_diagnose_core::dynamic_run::PermissionRequestGroup {
            group_id: GROUP.into(),
            order,
            size: 2,
        }),
    }
}

/// Stage two grouped requests in a claimed turn and save them as one group.
async fn seed_group(
    db: DatabaseConnection,
    second_device: &str,
) -> (SignalAgentSessionStore, Result<(), AgentError>) {
    crate::db::initialize_schema(&db).await.unwrap();
    let store = SignalAgentSessionStore::new(db).with_client_metadata(
        Some("client-conversation-1".into()),
        AgentSessionSurface::AiAgent,
    );
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(&followup("input-1", "user-1", "inspect the target"))
        .await
        .unwrap();
    let mut session = store.claim_turn(claim("permission-turn")).await.unwrap();
    let mut events = Vec::new();
    for request in [
        member("permission-1", 0, "device-1"),
        member("permission-2", 1, second_device),
    ] {
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq += 1;
        events.push(PermissionRequestedEvent {
            approval_mode_revision: None,
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: format!("requested-{}", request.request_id),
                run_id: session.conversation_id.clone(),
                event_seq: session.last_event_seq,
                input_revision: 1,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: request.created_at.clone(),
            },
            request,
        });
    }
    let saved = store
        .save_permission_request_group(&mut session, &events)
        .await;
    if saved.is_ok() {
        session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        store.save(&mut session).await.unwrap();
    }
    (store, saved)
}

fn approve() -> Vec<PermissionDecisionItem> {
    vec![PermissionDecisionItem {
        item_id: "first".into(),
        decision: PermissionItemDecision::Approve {
            resource_scope: vec!["target:device-1".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            ttl_seconds: 120,
            max_uses: 1,
        },
    }]
}

async fn decide_member(
    store: &SignalAgentSessionStore,
    request_id: &str,
    decisions: Vec<PermissionDecisionItem>,
    now_unix_ms: u64,
) -> Result<PermissionDecisionOutcome, AgentError> {
    let registry = desk_diagnose_core::ai_agent::ai_agent_provider_registry();
    let inventory = [CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: "desktop.session.inspect".into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    store
        .decide_permission_request_with_expected_request(
            PermissionDecisionSubject {
                conversation_id: "conversation-1",
                actor_id: "1",
                device_id: "device-1",
            },
            request_id,
            decisions,
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms,
                implicit_fresh_object_refs: &[],
            },
            &Utc::now().to_rfc3339(),
            None,
            None,
        )
        .await
}

async fn grants(
    store: &SignalAgentSessionStore,
) -> Vec<desk_agent_protocol::capability_grant::CapabilityGrant> {
    state(store)
        .await
        .2
        .into_iter()
        .map(|row| serde_json::from_str(&row.issued_payload_json).unwrap())
        .collect()
}

async fn row(store: &SignalAgentSessionStore, request_id: &str) -> resume::Model {
    resume::Entity::find()
        .filter(resume::Column::RequestId.eq(request_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
}

async fn claimant(
    store: &SignalAgentSessionStore,
    request_id: &str,
) -> (SignalAgentSessionStore, ClaimTurnParams) {
    let candidate = row(store, request_id).await;
    let session = store
        .pending_permission_resume(&candidate, Utc::now())
        .await
        .unwrap()
        .unwrap();
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
        .list_for_subject("conversation-1", "1", "device-1")
        .await
        .unwrap();
    let claimant = store
        .clone()
        .with_client_metadata(session.client_conversation_id.clone(), session.surface)
        .with_expected_input_revision(session.input_revision)
        .with_permission_resume(candidate.request_id.clone(), session.version, grants);
    let mut params = claim(&candidate.permission_id);
    params.trigger_origin = TriggerOrigin::PermissionDecision;
    params.request_id = None;
    params.connection_id = None;
    (claimant, params)
}

async fn memory() -> DatabaseConnection {
    crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap()
}

#[tokio::test]
async fn the_group_is_published_atomically() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    let (sessions, events, _) = state(&store).await;
    let session = PersistedAgentSession::decode_json(&sessions[0].state_json).unwrap();
    assert_eq!(session.permission_requests.len(), 2);
    assert!(
        session
            .permission_requests
            .iter()
            .all(|request| request.group.as_ref().unwrap().group_id == GROUP)
    );
    let requested = events
        .iter()
        .filter(|event| event.kind == AgentRunEventKind::PermissionRequested.as_str())
        .count();
    assert_eq!(requested, 2);
}

#[tokio::test]
async fn a_member_failing_its_host_check_rejects_the_whole_group_save() {
    let (store, saved) = seed_group(memory().await, "device-other").await;
    let error = saved.unwrap_err();
    assert_eq!(
        desk_diagnose_core::dynamic_run::rejected_group_member(&error),
        Some("permission-2")
    );
    // Nothing of the group was written.
    let (sessions, events, _) = state(&store).await;
    let session = PersistedAgentSession::decode_json(&sessions[0].state_json).unwrap();
    assert!(session.permission_requests.is_empty());
    assert!(
        events
            .iter()
            .all(|event| event.kind != AgentRunEventKind::PermissionRequested.as_str())
    );
}

#[tokio::test]
async fn group_grants_are_minted_only_by_the_settling_decision() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    assert!(
        grants(&store).await.is_empty(),
        "an open group grants nothing"
    );
    decide_member(&store, "permission-2", approve(), 5_000)
        .await
        .unwrap();
    let issued = grants(&store).await;
    assert_eq!(issued.len(), 2, "the settling decision grants every member");
    // a_member_grant_ttl_starts_at_settlement: the first member's time
    // limit starts when the whole group is decided.
    assert!(issued.iter().all(|grant| grant.issued_at_unix_ms == 5_000));
}

#[tokio::test]
async fn the_claim_waits_for_every_member_and_the_last_claim_supersedes_the_others() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    // Not ready: the row stays pending and nothing is consumed.
    let (early, params) = claimant(&store, "permission-1").await;
    assert!(matches!(
        early.claim_turn(params).await,
        Err(ClaimError::Busy)
    ));
    assert_eq!(row(&store, "permission-1").await.state, "pending");
    let deny = vec![PermissionDecisionItem {
        item_id: "first".into(),
        decision: PermissionItemDecision::Deny,
    }];
    decide_member(&store, "permission-2", deny, 2_000)
        .await
        .unwrap();
    let (last, params) = claimant(&store, "permission-2").await;
    let mut session = last.claim_turn(params).await.unwrap();
    assert_eq!(row(&store, "permission-2").await.state, "started");
    assert_eq!(row(&store, "permission-1").await.state, "superseded");
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    // The step answered its input; only the approval rows could still hold
    // the slot.
    session.handled_input_seq = session.latest_input_seq;
    store.save(&mut session).await.unwrap();
    let txn = crate::db::begin_write(&store.db, crate::entity::agent_session::Entity)
        .await
        .unwrap();
    let saved = PersistedAgentSession::decode_json(
        &find(&txn, "conversation-1")
            .await
            .unwrap()
            .unwrap()
            .state_json,
    )
    .unwrap();
    assert!(
        !crate::agent_run_event_store::main_requests::slot_held(&txn, &saved)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_unsettled_group_mints_nothing_after_new_input() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(&followup("input-2", "user-2", "something else"))
        .await
        .unwrap();
    assert!(
        decide_member(&store, "permission-2", approve(), 2_000)
            .await
            .is_err()
    );
    assert!(grants(&store).await.is_empty());
}

fn executor(
    store: &SignalAgentSessionStore,
) -> crate::permission_resume_executor::SignalPermissionResumeExecutor {
    crate::permission_resume_executor::SignalPermissionResumeExecutor::new(
        store.db.clone(),
        web::Data::new(desk_signal_facade::model::connection::SharedConnectionMap::default()),
        std::sync::Arc::new(crate::ai_agent_gate::AiAgentGate::new(
            desk_agent_protocol::ai_agent::AiAgentSettings {
                revision: 1,
                enabled: true,
            },
        )),
    )
}

/// A goal waiting on the step's first request, so a resume that reaches the
/// goal wake is visible as an attempted scan item.
async fn waiting_goal(store: &SignalAgentSessionStore) {
    let mut goal = desk_diagnose_core::goal::GoalRun::new(
        "goal-1".into(),
        "conversation-1".into(),
        "1".into(),
        desk_diagnose_core::dynamic_run::AgentInputTarget::fixed("device-1".into()),
        "device-1".into(),
        "Inspect the target".into(),
        "input-1".into(),
        desk_diagnose_core::goal::GoalOpening::OwnerRequest,
        desk_diagnose_core::goal::GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        Utc::now().timestamp_millis() as u64,
        desk_diagnose_core::goal::GoalLimits::default(),
    )
    .unwrap();
    goal.state = desk_diagnose_core::goal::GoalState::Waiting(
        desk_diagnose_core::goal::GoalWaitReason::Approval,
    );
    goal.status_reason = Some("permission-1".into());
    let txn = crate::db::begin_write(&store.db, crate::entity::agent_session::Entity)
        .await
        .unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn the_executor_makes_no_claim_attempt_while_the_group_is_unsettled() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    waiting_goal(&store).await;
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    let report = executor(&store).scan_once(0).await.unwrap();
    assert_eq!((report.scanned, report.attempted), (1, 0));
    assert_eq!(row(&store, "permission-1").await.state, "pending");
    decide_member(&store, "permission-2", approve(), 2_000)
        .await
        .unwrap();
    // Settled, the resume proceeds: the waiting goal takes the whole group.
    let report = executor(&store).scan_once(0).await.unwrap();
    assert!(report.attempted >= 1, "{report:?}");
    for request_id in ["permission-1", "permission-2"] {
        assert_ne!(
            row(&store, request_id).await.state,
            "pending",
            "{request_id}"
        );
    }
}

/// Record the first member's decision as an AI review bound to the given
/// goal, as the reviewer batch records it.
async fn as_ai_decision(
    store: &SignalAgentSessionStore,
    delegation: &desk_diagnose_core::approval_delegation::ApprovalDelegation,
    goal: Option<(String, u64)>,
) {
    let row = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::CorrelationId.eq("permission-1"))
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::PermissionDecided.as_str()))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut event: desk_diagnose_core::dynamic_run::PermissionDecidedEvent =
        serde_json::from_str(&row.payload_json).unwrap();
    event.decision_source =
        desk_diagnose_core::dynamic_run::PermissionDecisionSource::AutomaticReview {
            delegation_id: delegation.delegation_id.clone(),
            delegation_revision: delegation.revision,
            reviews: vec![
                desk_diagnose_core::dynamic_run::AiPermissionDecisionEvidence {
                    item_id: "first".into(),
                    candidate_id: "candidate-1".into(),
                    candidate_expires_at_unix_ms: 10_000_000,
                    model_config_revision: 1,
                    reason_code: "within_scope".into(),
                    reason: "the owner asked for it".into(),
                    provenance: None,
                },
            ],
            failures: vec![],
            goal_binding: goal,
        };
    event.validate().unwrap();
    agent_run_event::Entity::update_many()
        .col_expr(
            agent_run_event::Column::PayloadJson,
            Expr::value(serde_json::to_string(&event).unwrap()),
        )
        .filter(agent_run_event::Column::Id.eq(row.id))
        .exec(&store.db)
        .await
        .unwrap();
}

async fn current_delegation(
    store: &SignalAgentSessionStore,
) -> desk_diagnose_core::approval_delegation::ApprovalDelegation {
    let session = PersistedAgentSession::decode_json(&state(store).await.0[0].state_json).unwrap();
    let now = Utc::now().timestamp_millis();
    let delegation = desk_diagnose_core::approval_delegation::ApprovalDelegation::new_usage_only(
        "delegation-1".into(),
        session.approval_delegation_conversation_id().into(),
        "1".into(),
        "device-1".into(),
        "owner-switch".into(),
        now as u64,
    )
    .unwrap();
    crate::entity::agent_approval_delegation::ActiveModel {
        delegation_id: Set(delegation.delegation_id.clone()),
        conversation_id: Set(session.approval_delegation_conversation_id().into()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        state_json: Set(serde_json::to_string(&delegation).unwrap()),
        status: Set("active".into()),
        version: Set(delegation.ledger_version as i64),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    delegation
}

#[tokio::test]
async fn ai_member_grant_requires_the_same_delegation_and_goal_at_settlement() {
    // The reviewed goal is gone at settlement: the AI-approved member gets no
    // grant, and the owner-approved member is still granted.
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    let delegation = current_delegation(&store).await;
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    as_ai_decision(&store, &delegation, Some(("goal-gone".into(), 1))).await;
    decide_member(&store, "permission-2", approve(), 2_000)
        .await
        .unwrap();
    let issued = grants(&store).await;
    assert_eq!(issued.len(), 1, "{issued:?}");

    // The delegation the review ran under no longer exists: same result.
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    let mut missing = current_delegation(&store).await;
    missing.delegation_id = "delegation-gone".into();
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    as_ai_decision(&store, &missing, None).await;
    decide_member(&store, "permission-2", approve(), 2_000)
        .await
        .unwrap();
    assert_eq!(grants(&store).await.len(), 1);

    // The same delegation and goal (none) are still current: both members are
    // granted at settlement.
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    let delegation = current_delegation(&store).await;
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    as_ai_decision(&store, &delegation, None).await;
    decide_member(&store, "permission-2", approve(), 2_000)
        .await
        .unwrap();
    assert_eq!(grants(&store).await.len(), 2);
}

#[tokio::test]
async fn concurrent_last_decisions_mint_once() {
    let (store, saved) = seed_group(memory().await, "device-1").await;
    saved.unwrap();
    decide_member(&store, "permission-1", approve(), 1_000)
        .await
        .unwrap();
    // Two deliveries of the settling decision race; the session version
    // serializes them, so exactly one settles the group and mints.
    let (first, second) = tokio::join!(
        decide_member(&store, "permission-2", approve(), 2_000),
        decide_member(&store, "permission-2", approve(), 2_000),
    );
    let newly = [first, second]
        .into_iter()
        .filter(|outcome| outcome.as_ref().is_ok_and(|outcome| outcome.newly_recorded))
        .count();
    assert_eq!(newly, 1, "exactly one delivery records the decision");
    assert_eq!(grants(&store).await.len(), 2, "one grant per member");
}
