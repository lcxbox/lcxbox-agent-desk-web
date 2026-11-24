use super::*;
#[cfg(test)]
use desk_agent_data_store::entity_table::EntityTableExt;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use desk_diagnose_core::file_scope::{DirectoryConsentSource, DirectoryProposal, FileScopeSubject};
use sea_orm::{ConnectionTrait, Schema};

async fn setup() -> SignalAgentSessionStore {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.entity_table(agent_session::Entity),
        schema.entity_table(agent_run_event::Entity),
        schema.entity_table(crate::entity::agent_decision_resume::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
    crate::db::ensure_lifecycle_tables(&db).await;
    scoped(db)
}

fn scoped(db: DatabaseConnection) -> SignalAgentSessionStore {
    SignalAgentSessionStore::new(db)
        .with_client_metadata(Some("client".into()), AgentSessionSurface::AiAgent)
}

fn update() -> FileScopeUpdate {
    FileScopeUpdate {
        subject: FileScopeSubject {
            actor_id: "7".into(),
            device_id: "device".into(),
            conversation_id: "run".into(),
        },
        client_conversation_id: "client".into(),
        client_request_id: "select".into(),
        expected_revision: 0,
        mutation: FileScopeMutation::Select {
            proposal: DirectoryProposal {
                request_id: "select".into(),
                requested_path: "/private/tmp/test".into(),
                canonical_path: "/private/tmp/test".into(),
                purpose: "Create a file".into(),
                source: DirectoryConsentSource::OwnerSelection,
                directory: ObjectRef {
                    token: "opaque".into(),
                    snapshot_id: "generation".into(),
                    object_kind: ObjectKind::Directory,
                    expires_at: "2030-01-01T00:00:00Z".into(),
                },
            },
        },
    }
}

fn now() -> sea_orm::prelude::DateTimeUtc {
    "2026-09-05T00:00:00.123456Z".parse().unwrap()
}

async fn state(store: &SignalAgentSessionStore) -> PersistedAgentSession {
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn selectable_directory_scope_survives_snapshot_and_device_removal() {
    use desk_agent_data_store::device_roster::{personal_selection_key, replace};
    use desk_diagnose_core::conversation_key::{
        AgentConversationScope, derive_agent_conversation_key,
    };
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let store = scoped(db.clone());
    let mut a = update();
    a.subject.conversation_id =
        derive_agent_conversation_key("7", AgentConversationScope::Personal, "client");
    let run = &a.subject.conversation_id;
    let roster_key = personal_selection_key(run);
    let mut b = a.clone();
    b.subject.device_id = "other-device".into();
    b.client_request_id = "select-other".into();
    let FileScopeMutation::Select { proposal } = &mut b.mutation else {
        panic!()
    };
    proposal.request_id.clone_from(&b.client_request_id);
    for device in [&a.subject.device_id, &b.subject.device_id] {
        crate::device_code_store::get_or_create(&db, device)
            .await
            .unwrap();
    }
    assert!(store.update_file_scope(&a, now()).await.is_err());
    replace(
        &db,
        "7",
        &roster_key,
        0,
        vec![a.subject.device_id.clone(), b.subject.device_id.clone()],
    )
    .await
    .unwrap();
    let receipt = store.update_file_scope(&a, now()).await.unwrap();
    b.expected_revision = receipt.scope_revision;
    let receipt_b = store.update_file_scope(&b, now()).await.unwrap();
    replace(&db, "7", &roster_key, 1, vec![b.subject.device_id.clone()])
        .await
        .unwrap();
    let revocation = FileScopeUpdate {
        client_request_id: "revoke-first".into(),
        expected_revision: receipt_b.scope_revision,
        mutation: FileScopeMutation::Revoke {
            directory_request_id: "select".into(),
        },
        ..a.clone()
    };
    let wrong_device = FileScopeUpdate {
        subject: b.subject.clone(),
        ..revocation.clone()
    };
    assert!(store.update_file_scope(&wrong_device, now()).await.is_err());
    store.update_file_scope(&revocation, now()).await.unwrap();
    let reopened = scoped(db);
    let snapshot = reopened
        .read_agent_snapshot_for_subject(run, "7", "")
        .await
        .unwrap()
        .unwrap();
    let records = snapshot.session.file_scope.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].device_id, b.subject.device_id);
    assert_eq!(
        reopened.update_file_scope(&a, now()).await.unwrap(),
        receipt
    );
}

#[tokio::test]
async fn stale_directory_decisions_leave_no_receipt_or_partial_state() {
    for path in [r"C:\用户资料\季度 报告", "/Users/owner/季度 报告"] {
        let store = setup().await;
        let mut proposed = update();
        let FileScopeMutation::Select { mut proposal } = proposed.mutation else {
            panic!()
        };
        proposal.source = DirectoryConsentSource::ModelProposal;
        proposal.requested_path = path.into();
        proposal.canonical_path = path.into();
        proposal.directory.expires_at = (now()
            + desk_agent_data_store::datetime::database_interval(jiff::SignedDuration::from_secs(
                1,
            )))
        .to_rfc3339();
        proposed.mutation = FileScopeMutation::Propose { proposal };
        store.update_file_scope(&proposed, now()).await.unwrap();
        let before = state(&store).await;
        let mut decision = FileScopeUpdate {
            client_request_id: "decide".into(),
            expected_revision: before.file_scope.revision(),
            mutation: FileScopeMutation::Decide {
                directory_request_id: "select".into(),
                approve: true,
            },
            ..update()
        };
        for (revision, time) in [
            (0, now()),
            (
                before.file_scope.revision() + 1,
                now()
                    + desk_agent_data_store::datetime::database_interval(
                        jiff::SignedDuration::from_secs(1),
                    ),
            ),
        ] {
            decision.expected_revision = revision;
            assert!(store.update_file_scope(&decision, time).await.is_err());
            assert_eq!(state(&store).await, before);
            assert!(
                agent_run_event::Entity::find()
                    .filter(agent_run_event::Column::EventId.eq(decision.event_id()))
                    .one(&store.db)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        // A failed approval has no receipt to poison a later explicit rejection.
        decision.expected_revision = before.file_scope.revision();
        decision.mutation = FileScopeMutation::Decide {
            directory_request_id: "select".into(),
            approve: false,
        };
        let expired = now()
            + desk_agent_data_store::datetime::database_interval(jiff::SignedDuration::from_secs(
                2,
            ));
        let receipt = store.update_file_scope(&decision, expired).await.unwrap();
        let rejected = state(&store).await;
        assert_eq!(
            rejected.file_scope.revision(),
            before.file_scope.revision() + 1
        );
        assert_eq!(rejected.scope_snapshot, before.scope_snapshot);
        assert!(
            desk_diagnose_core::file_scope::approved_directories(
                &rejected,
                expired.timestamp_millis() as u64,
            )
            .unwrap()
            .is_empty()
        );
        let reopened = scoped(store.db.clone());
        assert_eq!(
            reopened
                .update_file_scope(&decision, expired)
                .await
                .unwrap(),
            receipt
        );
        assert_eq!(state(&reopened).await, rejected);
    }
}

#[tokio::test]
async fn receipt_and_scope_survive_reopen_and_expired_replay_without_resurrection() {
    for (requested, canonical) in [
        ("/private/tmp/test", "/private/tmp/test"),
        (r"D:\测试 输入", r"\\?\D:\测试 输入"),
        (r"D:\", r"\\?\D:\"),
    ] {
        verify_persisted_directory_replay(requested, canonical).await;
    }
}

async fn verify_persisted_directory_replay(requested: &str, canonical: &str) {
    let store = setup().await;
    let mut selected = update();
    let FileScopeMutation::Select { proposal } = &mut selected.mutation else {
        unreachable!()
    };
    proposal.requested_path = requested.into();
    proposal.canonical_path = canonical.into();
    let first = store.update_file_scope(&selected, now()).await.unwrap();
    assert_eq!(first.scope_revision, 2);
    assert!(state(&store).await.scope_snapshot.granted.is_empty());
    let reopened = scoped(store.db.clone());
    let persisted = state(&reopened).await;
    assert_eq!(
        persisted.file_scope.records()[0].proposal.requested_path,
        requested
    );
    assert_eq!(
        persisted.file_scope.records()[0].proposal.canonical_path,
        canonical
    );
    let mut changed_intent = selected.clone();
    let FileScopeMutation::Select { proposal } = &mut changed_intent.mutation else {
        unreachable!()
    };
    proposal.requested_path = "/different-request".into();
    assert!(
        reopened
            .update_file_scope(&changed_intent, now())
            .await
            .is_err()
    );
    assert_eq!(state(&reopened).await, persisted);
    assert_eq!(
        reopened.update_file_scope(&selected, now()).await.unwrap(),
        first
    );
    let revoke = FileScopeUpdate {
        client_request_id: "revoke".into(),
        expected_revision: 2,
        mutation: FileScopeMutation::Revoke {
            directory_request_id: "select".into(),
        },
        ..selected.clone()
    };
    reopened.update_file_scope(&revoke, now()).await.unwrap();
    assert!(state(&store).await.file_scope.records().is_empty());
    let before = state(&store).await;
    assert_eq!(
        store
            .update_file_scope(&selected, "2040-01-01T00:00:00Z".parse().unwrap())
            .await
            .unwrap(),
        first
    );
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn model_proposal_advances_only_its_held_version_and_never_approves() {
    let store = setup().await;
    store.update_file_scope(&update(), now()).await.unwrap();
    let mut held = state(&store).await;
    held.turn_state = desk_diagnose_core::session::TurnState::Running;
    let request_id = directory_call(&mut held, "model-request");
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(held.encode_json_for_storage().unwrap()),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let FileScopeMutation::Select { mut proposal } = update().mutation else {
        panic!()
    };
    proposal.request_id = request_id.clone();
    proposal.source = DirectoryConsentSource::ModelProposal;
    let stale = held.clone();
    store
        .propose_file_scope_for_turn(&mut held, "device", proposal.clone(), now())
        .await
        .unwrap();
    assert_eq!(held.version, stale.version + 1);
    assert_eq!(held.file_scope, state(&store).await.file_scope);
    assert_eq!(
        held.file_scope.records().last().unwrap().state,
        desk_diagnose_core::file_scope::DirectoryConsentState::Pending
    );
    let mut stale = stale;
    proposal.request_id = "another-model-request".into();
    assert!(
        store
            .propose_file_scope_for_turn(&mut stale, "device", proposal, now())
            .await
            .is_err()
    );
    assert_eq!(held.file_scope, state(&store).await.file_scope);
}

#[tokio::test]
async fn conflicting_and_cross_subject_requests_do_not_change_stored_state() {
    let store = setup().await;
    let selected = update();
    store.update_file_scope(&selected, now()).await.unwrap();
    let before = state(&store).await;
    let mut conflict = selected.clone();
    if let FileScopeMutation::Select { proposal } = &mut conflict.mutation {
        proposal.canonical_path.push_str("-other");
    }
    assert!(store.update_file_scope(&conflict, now()).await.is_err());
    for subject in [
        FileScopeSubject {
            actor_id: "8".into(),
            ..selected.subject.clone()
        },
        FileScopeSubject {
            device_id: "other".into(),
            ..selected.subject.clone()
        },
    ] {
        let foreign = FileScopeUpdate {
            subject,
            ..selected.clone()
        };
        assert!(store.update_file_scope(&foreign, now()).await.is_err());
    }
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn missing_receipt_and_stale_updates_fail_without_partial_writes() {
    let store = setup().await;
    let selected = update();
    store.update_file_scope(&selected, now()).await.unwrap();
    let before = state(&store).await;
    let stale = FileScopeUpdate {
        client_request_id: "remove".into(),
        mutation: FileScopeMutation::Revoke {
            directory_request_id: "select".into(),
        },
        ..selected.clone()
    };
    assert!(store.update_file_scope(&stale, now()).await.is_err());
    agent_run_event::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    assert!(store.update_file_scope(&selected, now()).await.is_err());
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn terminal_history_does_not_exhaust_a_long_lived_conversation() {
    let store = setup().await;
    let mut first = None;
    for index in 0..140 {
        let mut selected = update();
        selected.client_request_id = format!("select-{index}");
        selected.expected_revision = index * 3;
        if let FileScopeMutation::Select { proposal } = &mut selected.mutation {
            proposal.request_id = selected.client_request_id.clone();
        }
        let receipt = store.update_file_scope(&selected, now()).await.unwrap();
        if index == 0 {
            first = Some((selected.clone(), receipt.clone()));
        }
        let revoke = FileScopeUpdate {
            client_request_id: format!("revoke-{index}"),
            expected_revision: receipt.scope_revision,
            mutation: FileScopeMutation::Revoke {
                directory_request_id: selected.client_request_id.clone(),
            },
            ..selected
        };
        store.update_file_scope(&revoke, now()).await.unwrap();
    }
    assert!(state(&store).await.file_scope.records().is_empty());
    let (request, receipt) = first.unwrap();
    assert_eq!(
        store.update_file_scope(&request, now()).await.unwrap(),
        receipt
    );
    assert!(state(&store).await.file_scope.records().is_empty());
}

#[tokio::test]
async fn an_owner_directory_decision_records_one_resume_of_the_paused_turn() {
    for approve in [true, false] {
        let store = setup().await;
        let now = desk_agent_data_store::datetime::utc_now();
        store.update_file_scope(&update(), now).await.unwrap();
        let mut held = state(&store).await;
        held.turn_state = desk_diagnose_core::session::TurnState::Running;
        let request_id = directory_call(&mut held, "waiting-directory");
        let mut cached_result = desk_diagnose_core::chat::ChatMessage::text(
            "attachment-result",
            desk_diagnose_core::chat::ChatRole::Tool,
            "Verified attachment result",
        );
        cached_result.resolved_result = Some(Box::new(cached_result.clone()));
        held.conversation.insert(0, cached_result);
        let deadline = now
            + desk_agent_data_store::datetime::database_interval(jiff::SignedDuration::from_mins(
                5,
            ));
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(held.encode_json_for_storage().unwrap()),
            )
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(deadline)),
            )
            .exec(&store.db)
            .await
            .unwrap();
        let FileScopeMutation::Select { mut proposal } = update().mutation else {
            panic!()
        };
        proposal.request_id = request_id.clone();
        proposal.source = DirectoryConsentSource::ModelProposal;
        store
            .propose_file_scope_for_turn(&mut held, "device", proposal, now)
            .await
            .unwrap();
        // The turn ends paused on the request.
        held.finish_turn(
            desk_diagnose_core::session::TurnState::Idle,
            now.to_rfc3339(),
        );
        held.decision_pause = Some(desk_diagnose_core::proposal_review::DecisionPause {
            kind: desk_diagnose_core::proposal_review::DecisionKind::Directory,
            id: request_id.clone(),
            turn_id: "directory-test-turn".into(),
            call_id: "waiting-directory".into(),
            input_revision: held.input_revision,
            requested_at_unix_ms: now.timestamp_millis() as u64,
        });
        held.version += 1;
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(held.encode_json_for_storage().unwrap()),
            )
            .col_expr(agent_session::Column::Version, Expr::value(held.version))
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Option::<sea_orm::prelude::DateTimeUtc>::None),
            )
            .exec(&store.db)
            .await
            .unwrap();
        let before = state(&store).await;
        let decision = FileScopeUpdate {
            client_request_id: "owner-decision".into(),
            expected_revision: before.file_scope.revision(),
            mutation: FileScopeMutation::Decide {
                directory_request_id: request_id.clone(),
                approve,
            },
            ..update()
        };
        store.update_file_scope(&decision, now).await.unwrap();
        let settled = state(&store).await;
        // The decision event joins the paused conversation; the pause waits
        // for the decision-resume claim.
        assert_eq!(settled.conversation.len(), before.conversation.len() + 1);
        let event: serde_json::Value =
            serde_json::from_str(&settled.conversation.last().unwrap().text).unwrap();
        assert_eq!(
            event["state"],
            if approve { "approved" } else { "rejected" }
        );
        assert_eq!(settled.decision_pause, before.decision_pause);
        assert_eq!(settled.scope_snapshot, before.scope_snapshot);
        // HTTP/signaling retries replay the receipt; there is no second resume.
        store.update_file_scope(&decision, now).await.unwrap();
        assert_eq!(state(&store).await, settled);
        let resumes = desk_agent_data_store::entity::agent_decision_resume::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(resumes.len(), 1);
        assert_eq!(resumes[0].kind, "directory");
        assert_eq!(resumes[0].request_id, request_id);
    }
}

fn directory_call(held: &mut PersistedAgentSession, call: &str) -> String {
    held.current_turn_id = Some("directory-test-turn".into());
    let mut message = desk_diagnose_core::chat::ChatMessage::agent_tool_calls(
        format!("directory-source-{call}"),
        "",
        vec![desk_diagnose_core::chat::ToolCallRef {
            id: call.into(),
            name: "request_directory".into(),
            arguments_json: r#"{"path":"/private/tmp/test","purpose":"test"}"#.into(),
        }],
    );
    message.turn_id = held.current_turn_id.clone();
    held.conversation.push(message);
    desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::DirectoryRequest,
        &(
            "directory-proposal",
            &(&held.conversation_id, held.input_revision, call),
        ),
    )
}

use crate::agent_run_event_store::{
    AppendUserFollowupParams, SignalAgentRunEventStore, StartUserGoal,
};
#[tokio::test]
async fn first_goal_after_directory_confirmation_can_claim_two_segments() {
    run_goal_on_small_stacks(false).await;
}

#[tokio::test]
async fn cancelling_a_policy_paused_goal_releases_only_its_input() {
    run_goal_on_small_stacks(true).await;
}

async fn run_goal_on_small_stacks(legacy: bool) {
    for bytes in [1_048_576, 2_097_152] {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let (done, finished) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("first-goal-stack".into())
            .stack_size(bytes)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(desk_diagnose_core::future::boxed(|| {
                    verify_first_goal(db, legacy)
                }));
                let _ = done.send(());
            })
            .unwrap();
        let _ = finished.await;
        thread.join().unwrap();
    }
}

async fn verify_first_goal(db: DatabaseConnection, legacy: bool) {
    use crate::agent_goal_store as goals;
    use desk_diagnose_core::goal::GoalModelBinding;
    use desk_diagnose_core::{
        goal::GoalSegmentEnd,
        seam::ClaimTurnParams,
        session::{TriggerOrigin, TurnState},
    };
    let input = crate::agent_run_event_store::tests::goal_input(
        "event-goal",
        "owner-goal",
        "Read the report",
    );
    let store = SignalAgentSessionStore::new(db.clone()).with_client_metadata(
        input.client_conversation_id.clone(),
        AgentSessionSurface::AiAgent,
    );
    let now: sea_orm::prelude::DateTimeUtc = input.created_at.parse().unwrap();
    let events = SignalAgentRunEventStore::new(db.clone());
    let mut selection = update();
    selection.subject.actor_id.clone_from(&input.actor_id);
    selection.subject.device_id.clone_from(&input.device_id);
    selection.subject.conversation_id.clone_from(&input.run_id);
    selection.client_conversation_id = input.client_conversation_id.clone().unwrap();
    let selected = store.update_file_scope(&selection, now).await.unwrap();
    let decision = FileScopeUpdate {
        client_request_id: "confirm-directory".into(),
        expected_revision: selected.scope_revision,
        mutation: FileScopeMutation::Decide {
            directory_request_id: selection.client_request_id.clone(),
            approve: true,
        },
        ..selection
    };
    store.update_file_scope(&decision, now).await.unwrap();
    let initial = state(&store).await;
    assert_eq!(initial.policy_revision, 0);
    let start = StartUserGoal {
        goal_id: "directory-goal".into(),
        previous_completed_goal_id: None,
        model_binding: GoalModelBinding::from_destination(
            &input
                .message
                .data_envelope
                .as_ref()
                .unwrap()
                .allowed_destinations[0],
        )
        .unwrap(),
    };
    let mut obsolete = input.clone();
    obsolete.policy_revision = 0;
    assert!(
        events
            .append_user_goal(&obsolete, start.clone())
            .await
            .is_err()
    );
    assert_eq!(state(&store).await, initial);

    events
        .append_user_goal(&input, start.clone())
        .await
        .unwrap();
    let ordinary_replay = || events.append_user_followup(&input);
    assert_eq!(
        ordinary_replay().await.unwrap_err().kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
    if legacy {
        desk_diagnose_core::future::boxed(|| verify_cancel_and_replay(store, input, start, now))
            .await;
        return;
    }
    let mut claim = ClaimTurnParams {
        conversation_id: input.run_id,
        actor_id: input.actor_id,
        device_id: input.device_id,
        policy_revision: input.policy_revision,
        current_pdp_scope: input.current_scope,
        turn_id: "first".into(),
        request_id: None,
        connection_id: None,
        trigger_origin: TriggerOrigin::GoalContinuation,
        now: now.to_rfc3339(),
    };
    let mut first = goals::claim_slice(&db, &claim, &start.goal_id, None, "worker", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.goal.slice_seq, 1);
    assert_eq!(first.session.file_scope, initial.file_scope);
    first.session.finish_turn(TurnState::Idle, now.to_rfc3339());
    let queued = goals::settle_slice(&db, &mut first.session, &GoalSegmentEnd::Yield, &[], now)
        .await
        .unwrap();
    claim.turn_id = "second".into();
    let second = goals::claim_slice(
        &db,
        &claim,
        &start.goal_id,
        Some(queued.state_version),
        "worker",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.goal.slice_seq, 2);
}

async fn verify_cancel_and_replay(
    store: SignalAgentSessionStore,
    input: AppendUserFollowupParams,
    start: StartUserGoal,
    now: sea_orm::prelude::DateTimeUtc,
) {
    use crate::agent_goal_store as goals;
    use desk_diagnose_core::{
        seam::ClaimTurnParams,
        session::{TriggerOrigin, TurnState},
    };
    let db = store.db.clone();
    let events = SignalAgentRunEventStore::new(db.clone());
    let ordinary_replay = || events.append_user_followup(&input);
    let row = agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut stale = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    stale.policy_revision = 0;
    agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(stale.encode_json_for_storage().unwrap()),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let owner = input.actor_id.as_str();
    let goal = goals::load_for_subject(&db, &input.run_id, owner, &input.device_id)
        .await
        .unwrap()
        .unwrap();
    let paused = goals::pause_if_goal_context_invalid(&db, &goal, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        paused.status_reason.as_deref(),
        Some(desk_diagnose_core::goal::GOAL_POLICY_CHANGED)
    );
    assert!(
        goals::pause_if_goal_context_invalid(&db, &goal, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        goals::apply_owner_action(
            &db,
            &input.run_id,
            owner,
            &input.device_id,
            &goal.goal_id,
            paused.state_version,
            desk_diagnose_core::goal::GoalOwnerAction::Resume,
            now
        )
        .await
        .is_err()
    );
    let mut next = crate::agent_run_event_store::tests::goal_input("next", "next", "Next question");
    next.created_at = now.to_rfc3339();
    use desk_agent_data_store::main_request::{OwnerAdmission, OwnerAdmissionResult};
    next.read_context = Some(
        desk_diagnose_core::input_read_context::ReadContextSelection::capture(
            &[],
            &next.current_scope,
        )
        .unwrap(),
    );
    let ask = desk_agent_protocol::ai_agent::AiAgentAsk {
        conversation_id: next.client_conversation_id.clone(),
        client_message_id: next.message.message_id.clone(),
        client_timezone: next.client_timezone.clone(),
        question: next.message.text.clone(),
        ..Default::default()
    };
    let OwnerAdmissionResult::Queued { row, waiting } = events
        .admit_user_input(
            &next,
            None,
            OwnerAdmission::Enqueue {
                ask: &ask,
                limit: 4,
                reply_connection_id: None,
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected queued follow-up");
    };
    assert_eq!(waiting, Some(true));
    let activate = OwnerAdmission::Activate {
        request_id: &row.request_id,
        issue_source: None,
    };
    assert!(matches!(
        events
            .admit_user_input(&next, None, activate)
            .await
            .unwrap(),
        OwnerAdmissionResult::Waiting
    ));
    let cancelled = goals::apply_owner_action(
        &db,
        &input.run_id,
        owner,
        &input.device_id,
        &goal.goal_id,
        paused.state_version,
        desk_diagnose_core::goal::GoalOwnerAction::Cancel,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        cancelled.state,
        desk_diagnose_core::goal::GoalState::Cancelled
    );
    let saved = state(&store).await;
    assert_eq!((saved.handled_input_seq, saved.latest_input_seq), (1, 1));
    assert!(!desk_agent_data_store::main_request::holds_input_slot(
        &saved
    ));
    let replay = || events.append_user_goal(&input, start.clone());
    assert_eq!(
        replay().await.unwrap_err(),
        desk_diagnose_core::goal::cancelled_goal_input_error()
    );
    let new_goal = StartUserGoal {
        goal_id: "conflicting-goal".into(),
        ..start.clone()
    };
    assert_eq!(
        events
            .append_user_goal(&next, new_goal)
            .await
            .unwrap_err()
            .message,
        "This input was already queued as an ordinary request"
    );
    let receipt = events
        .admit_user_input(&next, None, activate)
        .await
        .unwrap()
        .native()
        .unwrap();
    // A different goal must not supersede an activated input before its first claim.
    let racing =
        crate::agent_run_event_store::tests::goal_input("racing", "racing", "Start another goal");
    let busy = events
        .append_user_goal(
            &racing,
            StartUserGoal {
                goal_id: "racing-goal".into(),
                ..start.clone()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        busy.kind,
        desk_agent_protocol::AgentErrorKind::SessionUnavailable
    );
    assert!(busy.retryable);
    let mut held = store
        .clone()
        .with_expected_input_revision(receipt.input_revision)
        .with_queued_input(row.request_id.clone(), next.message.message_id.clone())
        .claim_turn(ClaimTurnParams {
            conversation_id: next.run_id.clone(),
            actor_id: next.actor_id.clone(),
            device_id: next.device_id.clone(),
            policy_revision: next.policy_revision,
            current_pdp_scope: next.current_scope.clone(),
            turn_id: "next-turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: next.created_at.clone(),
        })
        .await
        .unwrap();
    held.conversation.push(
        desk_diagnose_core::chat::ChatMessage::text(
            "answer",
            desk_diagnose_core::chat::ChatRole::Assistant,
            "Only the later request receives this answer",
        )
        .with_turn_id("next-turn"),
    );
    held.handled_input_seq = receipt.input_seq;
    held.finish_turn(TurnState::Idle, now.to_rfc3339());
    store.save(&mut held).await.unwrap();
    let answered = state(&store).await;
    assert_eq!(
        answered.conversation.last().unwrap().text,
        "Only the later request receives this answer"
    );
    assert_eq!(
        replay().await.unwrap_err(),
        desk_diagnose_core::goal::cancelled_goal_input_error()
    );
    assert_eq!(state(&store).await, answered);
    assert_eq!(
        ordinary_replay().await.unwrap_err().kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
}
