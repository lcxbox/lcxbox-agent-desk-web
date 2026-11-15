use super::*;
use crate::schedule::test_host::{Call, Recorder};
use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::DestinationIdentity};
use desk_diagnose_core::{
    ai_assistant::source_registry::NativeDerivedInputPolicy,
    chat::ToolCallRef,
    session::{AgentSessionSurface, TriggerOrigin},
    skills::{SkillCatalog, StoredSkillEntry},
};
use sea_orm::{Database, DatabaseConnection, Schema};

const ACTOR: &str = "1";

async fn db_at(url: &str) -> DatabaseConnection {
    let db = Database::connect(url).await.unwrap();
    crate::entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    db
}

async fn db() -> DatabaseConnection {
    db_at("sqlite::memory:").await
}

fn policy() -> desk_agent_protocol::ai_assistant::source::SourceUsePolicy {
    desk_diagnose_core::ai_assistant::source::owner_text_policy(1, false)
}

fn native() -> NativeDerivedInputPolicy {
    NativeDerivedInputPolicy {
        actor_id: ACTOR.into(),
        conversation_id: "chat".into(),
        physical_call_id: "physical".into(),
        output_envelope_id: "output".into(),
        output_digest_sha256: "a".repeat(64),
        output_label_sha256: "b".repeat(64),
        actor_input_seq: 1,
        input_revision: 1,
        source_contract_revision:
            desk_diagnose_core::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
        policy_revision: 1,
        text_policy: Some(policy()),
    }
}

/// An owner turn whose open call proposes `arguments`.
fn session(call_id: &str, arguments: &serde_json::Value) -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "chat",
        ACTOR,
        "device",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-14T00:00:00Z",
    );
    session.surface = AgentSessionSurface::AiAssistant;
    session
        .begin_turn(
            "turn",
            None,
            None,
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-14T00:00:00Z",
        )
        .unwrap();
    session.adopt_trigger(TriggerOrigin::User, "turn");
    session.input_revision = 1;
    session.bind_assistant_input_targets(&[]).unwrap();
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    let user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "user".into(),
        "Keep this as a skill.".into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let mut assistant = ChatMessage::assistant_tool_calls_with_replay(
        "assistant",
        "",
        vec![ToolCallRef {
            id: call_id.into(),
            name: authoring::PROPOSE_TOOL.into(),
            arguments_json: arguments.to_string(),
        }],
        desk_diagnose_core::replay::ReplayDisposition::legacy_unknown(),
    );
    assistant.turn_id = session.current_turn_id.clone();
    assistant.data_envelope = user.data_envelope.clone();
    session.conversation = vec![user, assistant];
    session
}

fn call(call_id: &str, arguments: &serde_json::Value) -> ToolCall {
    ToolCall {
        id: call_id.into(),
        name: authoring::PROPOSE_TOOL.into(),
        arguments_json: arguments.to_string(),
    }
}

fn host(session: PersistedAgentSession, proof: bool) -> Recorder {
    let mut host = Recorder::new(session);
    host.lease_deadline_ms = Some(i64::MAX / 2);
    if proof {
        host.skill_source = Some((policy(), native()));
    }
    host
}

fn args(name: &str, body: &str) -> serde_json::Value {
    serde_json::json!({"name":name,"description":format!("About {name}"),"body":body,
        "reason":"The owner asked to keep it",
        "files":[{"path":"references/notes.md","text":"Reference notes"}]})
}

async fn propose(
    db: &DatabaseConnection,
    call_id: &str,
    arguments: serde_json::Value,
    proof: bool,
) -> Result<(String, PersistedAgentSession), SkillProposalError> {
    let session = session(call_id, &arguments);
    let host = host(session.clone(), proof);
    propose_on(
        &host,
        db,
        &session,
        &call(call_id, &arguments),
        &Default::default(),
    )
    .await
}

fn rejected_with(result: Result<(String, PersistedAgentSession), SkillProposalError>) -> String {
    match result {
        Err(SkillProposalError::Rejected(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn pending_count(db: &DatabaseConnection) -> i64 {
    scope::Entity::find()
        .filter(scope::Column::ScopeKey.eq(personal_scope(ACTOR)))
        .one(db)
        .await
        .unwrap()
        .map_or(0, |row| row.pending_proposal_count)
}

fn proposal_id(session: &PersistedAgentSession) -> String {
    let receipt = session.conversation.last().unwrap();
    serde_json::from_str::<serde_json::Value>(&receipt.text).unwrap()["proposal_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The proposing turn ended paused on its proposal, as the loop leaves it.
fn paused(waiting: &PersistedAgentSession) -> PersistedAgentSession {
    let mut session = waiting.clone();
    let call_id = session
        .conversation
        .last()
        .unwrap()
        .tool_call_id
        .clone()
        .unwrap();
    let pause =
        desk_diagnose_core::proposal_review::awaiting_proposal(&session, &call_id, "turn", 1)
            .unwrap();
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        "2026-11-14T00:00:01Z",
    );
    session.decision_pause = Some(pause);
    session
}

#[tokio::test]
async fn a_proposal_is_saved_with_its_proof_and_waits_in_its_turn() {
    let db = db().await;
    let (receipt, next) = propose(&db, "call-1", args("disk-report", "Run df -h."), true)
        .await
        .unwrap();
    assert_eq!(receipt, "skill-proposal:call-1");
    let id = proposal_id(&next);
    let stored = next.conversation.last().unwrap();
    assert_eq!(stored.message_id, receipt);
    let value: serde_json::Value = serde_json::from_str(&stored.text).unwrap();
    assert_eq!(value["proposal_id"], id.as_str());
    assert_eq!(value["awaiting_decision"], true);
    let row = proposal::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.kind.as_str()),
        ("pending", "create")
    );
    let proof: AuthoredSkillSource = serde_json::from_str(&row.source_policy_json).unwrap();
    proof.validate(ACTOR).unwrap();
    assert_eq!(pending_count(&db).await, 1);
    let projected = review_proposal_on(&db, &next, &id).await.unwrap().unwrap();
    assert!(projected.skill_md.contains("Run df -h."));
    assert_eq!(projected.files.len(), 1);
    // The reviewer and the owner see the same content.
    let views = list_on(&db, ACTOR, None).await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].status, SkillProposalStatus::Pending);
}

#[tokio::test]
async fn a_proposal_without_a_retainable_proof_is_refused() {
    let db = db().await;
    assert_eq!(
        rejected_with(propose(&db, "call-1", args("a-skill", "x"), false).await),
        authoring::NO_PROOF
    );
    let arguments = args("a-skill", "x");
    let session = session("call-2", &arguments);
    let mut host = host(session.clone(), true);
    let mut no_copy = policy();
    no_copy.no_copy = true;
    no_copy.no_export = true;
    no_copy.derived_visible_text_allowed = false;
    host.skill_source = Some((no_copy, native()));
    let result = propose_on(
        &host,
        &db,
        &session,
        &call("call-2", &arguments),
        &Default::default(),
    )
    .await;
    assert_eq!(rejected_with(result), authoring::NOT_RETAINABLE);
    assert!(proposal::Entity::find().one(&db).await.unwrap().is_none());
}

#[tokio::test]
async fn import_rules_names_and_size_apply_to_proposals() {
    let db = db().await;
    let mut credential = args("creds", "Use the token.");
    credential["files"] = serde_json::json!([{"path":"scripts/run.sh","text":"export TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789"}]);
    assert!(rejected_with(propose(&db, "c1", credential, true).await).contains("credential"));
    assert!(
        rejected_with(propose(&db, "c2", args("issue-analysis", "x"), true).await)
            .contains("reserved")
    );
    let huge = args("huge", &"\"\\".repeat(20 * 1024));
    assert_eq!(
        rejected_with(propose(&db, "c3", huge, true).await),
        authoring::TOO_LARGE
    );
    let scope = personal_scope(ACTOR);
    let package = parse_input(
        &SkillPackageInput {
            zip_base64: None,
            skill_md: Some("---\nname: notes\ndescription: Notes\n---\nv1".into()),
            files: vec![],
        },
        None,
    )
    .unwrap()
    .package;
    create_on(
        &db,
        &scope,
        &package,
        &VersionSource::manual(SkillOrigin::Imported, None),
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        rejected_with(propose(&db, "c4", args("notes", "v2"), true).await),
        authoring::NAME_TAKEN
    );
    let mut stale = args("notes", "v2");
    stale["base"] = serde_json::json!({"skill_ref":"personal/notes","version":2});
    assert_eq!(
        rejected_with(propose(&db, "c5", stale, true).await),
        authoring::BASE_CHANGED
    );
    assert_eq!(pending_count(&db).await, 0);
}

async fn decide(
    db: &DatabaseConnection,
    waiting: &PersistedAgentSession,
    approve: bool,
    decider: SkillDecider,
) -> (proposal::Model, Recorder) {
    let host = host(paused(waiting), true);
    let row = decide_on(
        &host,
        db,
        ACTOR,
        &proposal_id(waiting),
        Some(1),
        approve,
        &decider,
    )
    .await
    .unwrap();
    (row, host)
}

#[tokio::test]
async fn an_approved_proposal_becomes_an_ai_authored_version_and_tells_the_turn() {
    let db = db().await;
    let (_, waiting) = propose(&db, "call-1", args("disk-report", "Run df -h."), true)
        .await
        .unwrap();
    let (row, host) = decide(
        &db,
        &waiting,
        true,
        ai_decider("Matches the owner's request"),
    )
    .await;
    assert_eq!(row.status, "approved");
    assert_eq!(row.decision_source.as_deref(), Some("ai_review"));
    assert_eq!(pending_count(&db).await, 0);
    let skill = find_by_name_on(&db, &personal_scope(ACTOR), "disk-report")
        .await
        .unwrap()
        .unwrap();
    let version = version_on(&db, &skill.skill_id, 1).await.unwrap().unwrap();
    assert_eq!(version.origin, "ai_authored");
    assert_eq!(
        version.proposal_id.as_deref(),
        Some(row.proposal_id.as_str())
    );
    assert_eq!(
        version.source_policy_json.as_deref(),
        Some(row.source_policy_json.as_str())
    );
    // The waiting turn receives exactly one decision receipt.
    assert!(host.calls().contains(&Call::Lock("chat".into())));
    let written = host.written_states.lock().unwrap().clone();
    assert_eq!(written.len(), 1);
    let turn = PersistedAgentSession::decode_json(&written[0]).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_str(&turn.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["event"], SKILL_DECIDED_EVENT);
    assert_eq!(receipt["status"], "approved");
    assert_eq!(receipt["decided_by"], "approval_ai");
    assert_eq!(turn.version, waiting.version + 1);
    // The projection restores the stored proof of this version only.
    let catalog = service::host_catalog_on(
        &db,
        &service::visible_scopes(ACTOR, true),
        &[(skill.skill_id.clone(), 1)],
        vec![],
    )
    .await
    .unwrap();
    let authored = catalog.loaded[0].authored.unwrap();
    assert_eq!(
        authored.decision_source,
        desk_agent_protocol::ai_assistant::skill::SkillDecisionSource::AiReview
    );
    let mut label = desk_diagnose_core::model_message_labels::model_bound_user_message(
        format!("skill-authored-{}-v1", skill.skill_id),
        "body".into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
    .unwrap();
    label.provenance.source_provider_id = "ai-assistant-skill".into();
    label.provenance.source_tool_name = "authored".into();
    assert_eq!(
        authored_policy_on(&db, ACTOR, &label).await.unwrap(),
        Some(
            AuthoredSkillSource::new(ACTOR, policy(), native())
                .unwrap()
                .policy
        )
    );
    assert_eq!(authored_policy_on(&db, "2", &label).await.unwrap(), None);
    // A manual edit is owner content again and restores no AI policy.
    update_from_manual(&db, "disk-report", &skill.skill_id, skill.revision as u64).await;
    label.provenance.source_object_id = Some(format!("skill-authored-{}-v2", skill.skill_id));
    assert_eq!(authored_policy_on(&db, ACTOR, &label).await.unwrap(), None);
}

async fn update_from_manual(db: &DatabaseConnection, name: &str, skill_id: &str, revision: u64) {
    let package = parse_input(
        &SkillPackageInput {
            zip_base64: None,
            skill_md: Some(format!(
                "---\nname: {name}\ndescription: Edited\n---\nby hand"
            )),
            files: vec![],
        },
        Some(name),
    )
    .unwrap()
    .package;
    let summary = update_on(
        db,
        &personal_scope(ACTOR),
        skill_id,
        revision,
        None,
        Some((&package, &VersionSource::manual(SkillOrigin::Edited, None))),
        2,
    )
    .await
    .unwrap();
    let version = version_on(db, skill_id, summary.current_version as i64)
        .await
        .unwrap()
        .unwrap();
    assert!(version.source_policy_json.is_none());
}

#[tokio::test]
async fn updating_a_loaded_skill_keeps_the_old_version_for_this_input() {
    let db = db().await;
    let (_, created) = propose(&db, "call-1", args("notes", "v1"), true)
        .await
        .unwrap();
    decide(&db, &created, true, SkillDecider::Owner).await;
    let skill = find_by_name_on(&db, &personal_scope(ACTOR), "notes")
        .await
        .unwrap()
        .unwrap();
    let mut update = args("notes", "v2 instructions");
    update["base"] = serde_json::json!({"skill_ref":"personal/notes","version":1});
    let mut turn = session("call-2", &update);
    // The current input already loaded version 1.
    let catalog = SkillCatalog {
        stored: vec![StoredSkillEntry {
            skill_id: skill.skill_id.clone(),
            scope: desk_agent_protocol::ai_assistant::skill::SkillScope::Personal,
            name: "notes".into(),
            description: "About notes".into(),
            version: 1,
            origin: SkillOrigin::AiAuthored,
            size_bytes: 20,
            resources: vec![],
        }],
        loaded: vec![],
        disabled_bundled: vec![],
    };
    turn.skills
        .apply(
            1,
            &ToolCall {
                id: "load".into(),
                name: desk_diagnose_core::skills::LOAD_TOOL.into(),
                arguments_json: r#"{"skill_ref":"notes"}"#.into(),
            },
            &catalog,
        )
        .unwrap();
    let host = host(turn.clone(), true);
    let (_, waiting) = propose_on(
        &host,
        &db,
        &turn,
        &call("call-2", &update),
        &Default::default(),
    )
    .await
    .unwrap();
    let id = proposal_id(&waiting);
    let review = review_proposal_on(&db, &waiting, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.base_version, Some(1));
    assert!(!review.changes.is_empty());
    let (row, host) = decide(&db, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(row.status, "approved");
    let turn = PersistedAgentSession::decode_json(&host.written_states.lock().unwrap()[0]).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_str(&turn.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["decided_by"], "owner");
    assert_eq!(receipt["version"], 2);
    assert!(
        receipt["note"]
            .as_str()
            .unwrap()
            .contains("keeps using the loaded version 1; new owner messages use version 2")
    );
    assert_eq!(turn.skills.loaded_stored(1), [(skill.skill_id.clone(), 1)]);
}

#[tokio::test]
async fn a_moved_base_conflicts_and_every_final_state_releases_its_slot() {
    let db = db().await;
    let (_, created) = propose(&db, "call-1", args("notes", "v1"), true)
        .await
        .unwrap();
    decide(&db, &created, true, SkillDecider::Owner).await;
    let skill = find_by_name_on(&db, &personal_scope(ACTOR), "notes")
        .await
        .unwrap()
        .unwrap();
    let mut update = args("notes", "v2");
    update["base"] = serde_json::json!({"skill_ref":"personal/notes","version":1});
    let (_, waiting) = propose(&db, "call-2", update, true).await.unwrap();
    assert_eq!(pending_count(&db).await, 1);
    // The owner edits the skill before the decision.
    update_from_manual(&db, "notes", &skill.skill_id, skill.revision as u64).await;
    let (row, _) = decide(&db, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(row.status, "conflicted");
    assert_eq!(pending_count(&db).await, 0);
    let (_, rejected) = propose(&db, "call-3", args("other", "x"), true)
        .await
        .unwrap();
    let (row, _) = decide(&db, &rejected, false, ai_decider("Relays a web page")).await;
    assert_eq!(row.status, "rejected");
    // Only the reviewer's closed code is kept on the proposal.
    assert_eq!(row.decision_reason, None);
    assert_eq!(row.decision_reason_code.as_deref(), Some("within_scope"));
    assert_eq!(pending_count(&db).await, 0);
    assert!(
        find_by_name_on(&db, &personal_scope(ACTOR), "other")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn at_most_eight_proposals_wait_and_withdrawn_ones_free_their_slots() {
    let db = db().await;
    for index in 0..MAX_PENDING_PROPOSALS {
        propose(
            &db,
            &format!("call-{index}"),
            args(&format!("skill-{index}"), "x"),
            true,
        )
        .await
        .unwrap();
    }
    assert!(
        rejected_with(propose(&db, "call-9", args("skill-9", "x"), true).await)
            .contains("undecided skill proposals")
    );
    // Stopping the conversation withdraws its pending proposals.
    assert!(withdraw_on(&db, ACTOR, "chat", 5).await.unwrap());
    assert!(!withdraw_on(&db, ACTOR, "chat", 6).await.unwrap());
    assert_eq!(pending_count(&db).await, 0);
    assert_eq!(
        proposal::Entity::find()
            .filter(proposal::Column::Status.eq("withdrawn"))
            .all(&db)
            .await
            .unwrap()
            .len(),
        MAX_PENDING_PROPOSALS as usize
    );
    propose(&db, "call-10", args("skill-10", "x"), true)
        .await
        .unwrap();
    assert_eq!(pending_count(&db).await, 1);
}

#[tokio::test]
async fn a_decision_waits_until_the_proposing_turn_has_paused() {
    let db = db().await;
    let (_, waiting) = propose(&db, "call-1", args("disk-report", "x"), true)
        .await
        .unwrap();
    // The turn is still running: deciding now would strand the pause.
    let running = host(waiting.clone(), true);
    assert!(
        decide_on(
            &running,
            &db,
            ACTOR,
            &proposal_id(&waiting),
            Some(1),
            true,
            &SkillDecider::Owner,
        )
        .await
        .is_err()
    );
    assert_eq!(
        proposal::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    let (row, host) = decide(&db, &waiting, false, SkillDecider::Owner).await;
    assert_eq!(row.status, "rejected");
    assert_eq!(host.written_states.lock().unwrap().len(), 1);
    let resume = crate::entity::agent_decision_resume::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            resume.kind.as_str(),
            resume.request_id.as_str(),
            resume.state.as_str()
        ),
        ("skill", row.proposal_id.as_str(), "pending")
    );
}

#[tokio::test]
async fn a_proposal_saved_by_one_instance_is_decided_by_another() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("shared.db").display()
    );
    let first = db_at(&url).await;
    let second = Database::connect(&url).await.unwrap();
    let (_, waiting) = propose(&first, "call-1", args("shared", "x"), true)
        .await
        .unwrap();
    let (row, _) = decide(&second, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(row.status, "approved");
    assert!(
        find_by_name_on(&first, &personal_scope(ACTOR), "shared")
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        decide_on(
            &host(waiting.clone(), true),
            &first,
            ACTOR,
            &proposal_id(&waiting),
            Some(1),
            true,
            &SkillDecider::Owner,
        )
        .await,
        Err(SkillProposalError::Store(ScheduleStoreError::Conflict))
    ));
}

fn removal_args(name: &str, version: u64, action: &str) -> serde_json::Value {
    serde_json::json!({"skill_ref":format!("personal/{name}"),"version":version,
        "action":action,"reason":"The owner no longer needs it"})
}

async fn propose_removal(
    db: &DatabaseConnection,
    call_id: &str,
    arguments: serde_json::Value,
    proof: bool,
) -> Result<(String, PersistedAgentSession), SkillProposalError> {
    let mut session = session(call_id, &arguments);
    session.conversation[1].tool_calls[0].name = authoring::REMOVE_TOOL.into();
    let host = host(session.clone(), proof);
    let call = ToolCall {
        id: call_id.into(),
        name: authoring::REMOVE_TOOL.into(),
        arguments_json: arguments.to_string(),
    };
    propose_on(&host, db, &session, &call, &Default::default()).await
}

/// An approved AI-authored skill named `name` at version 1.
async fn authored(db: &DatabaseConnection, call_id: &str, name: &str) -> skill::Model {
    let (_, waiting) = propose(db, call_id, args(name, "Run df -h."), true)
        .await
        .unwrap();
    decide(db, &waiting, true, SkillDecider::Owner).await;
    current_skill(db, name).await.unwrap()
}

async fn current_skill(db: &DatabaseConnection, name: &str) -> Option<skill::Model> {
    find_by_name_on(db, &personal_scope(ACTOR), name)
        .await
        .unwrap()
}

fn decision_receipt(host: &Recorder) -> serde_json::Value {
    let written = host.written_states.lock().unwrap().clone();
    let turn = PersistedAgentSession::decode_json(written.last().unwrap()).unwrap();
    serde_json::from_str(&turn.conversation.last().unwrap().text).unwrap()
}

#[tokio::test]
async fn an_assistant_written_skill_is_disabled_then_deleted_after_approval() {
    let db = db().await;
    let skill = authored(&db, "call-1", "disk-report").await;
    let (_, waiting) = propose_removal(
        &db,
        "call-2",
        removal_args("disk-report", 1, "disable"),
        true,
    )
    .await
    .unwrap();
    let submitted: serde_json::Value =
        serde_json::from_str(&waiting.conversation.last().unwrap().text).unwrap();
    assert_eq!(submitted["kind"], "disable");
    assert!(
        submitted["message"]
            .as_str()
            .unwrap()
            .contains("disabled or deleted")
    );
    // The decider sees the version it acts on, not a change.
    let views = list_on(&db, ACTOR, None).await.unwrap();
    assert_eq!(views[0].kind, SkillProposalKind::Disable);
    assert!(views[0].skill_md.contains("Run df -h."));
    assert!(views[0].diff.is_empty());
    assert_eq!(
        views[0].base_skill_id.as_deref(),
        Some(skill.skill_id.as_str())
    );
    let review = review_proposal_on(&db, &paused(&waiting), &proposal_id(&waiting))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.kind, SkillProposalKind::Disable);
    let (row, host) = decide(&db, &waiting, true, ai_decider("The owner asked")).await;
    assert_eq!(row.status, "approved");
    assert_eq!(pending_count(&db).await, 0);
    let disabled = current_skill(&db, "disk-report").await.unwrap();
    assert!(!disabled.enabled);
    assert_eq!(disabled.current_version, 1);
    let receipt = decision_receipt(&host);
    assert_eq!(receipt["kind"], "disable");
    assert!(receipt["version"].is_null() && receipt["note"].is_null());
    assert_eq!(
        rejected_with(
            propose_removal(
                &db,
                "call-3",
                removal_args("disk-report", 1, "disable"),
                true
            )
            .await
        ),
        authoring::ALREADY_DISABLED
    );
    // A disabled assistant-written skill can still be deleted.
    let (_, waiting) = propose_removal(
        &db,
        "call-4",
        removal_args("disk-report", 1, "delete"),
        true,
    )
    .await
    .unwrap();
    let (row, host) = decide(&db, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(row.status, "approved");
    assert!(current_skill(&db, "disk-report").await.is_none());
    assert!(version_on(&db, &skill.skill_id, 1).await.unwrap().is_none());
    assert_eq!(decision_receipt(&host)["kind"], "delete");
}

#[tokio::test]
async fn only_assistant_written_versions_can_be_removed() {
    let db = db().await;
    assert_eq!(
        rejected_with(
            propose_removal(&db, "call-0", removal_args("missing", 1, "disable"), true).await
        ),
        authoring::BASE_CHANGED
    );
    // An imported skill updated by the assistant: disabling looks at the
    // current version, deleting at every retained version.
    let package = parse_input(
        &SkillPackageInput {
            zip_base64: None,
            skill_md: Some("---\nname: imported\ndescription: Imported\n---\nby owner".into()),
            files: vec![],
        },
        Some("imported"),
    )
    .unwrap()
    .package;
    create_on(
        &db,
        &personal_scope(ACTOR),
        &package,
        &VersionSource::manual(SkillOrigin::Imported, None),
        1,
    )
    .await
    .unwrap();
    let mut update = args("imported", "Assistant version");
    update["base"] = serde_json::json!({"skill_ref":"personal/imported","version":1});
    let (_, waiting) = propose(&db, "call-1", update, true).await.unwrap();
    decide(&db, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(
        rejected_with(
            propose_removal(&db, "call-2", removal_args("imported", 2, "delete"), true).await
        ),
        authoring::NOT_AI_AUTHORED
    );
    assert!(
        propose_removal(&db, "call-3", removal_args("imported", 2, "disable"), true)
            .await
            .is_ok()
    );
    // An owner edit makes the current version owner content again.
    let skill = authored(&db, "call-4", "edited").await;
    update_from_manual(&db, "edited", &skill.skill_id, skill.revision as u64).await;
    for action in ["disable", "delete"] {
        assert_eq!(
            rejected_with(
                propose_removal(&db, "call-5", removal_args("edited", 2, action), true).await
            ),
            authoring::NOT_AI_AUTHORED
        );
    }
    assert_eq!(
        rejected_with(
            propose_removal(&db, "call-6", removal_args("edited", 1, "disable"), true).await
        ),
        authoring::BASE_CHANGED
    );
    // A removal stores no model text, so it needs no retention proof: the
    // model's reason is dropped and no source is kept.
    authored(&db, "call-7", "unproven").await;
    let (_, waiting) = propose_removal(&db, "call-8", removal_args("unproven", 1, "delete"), false)
        .await
        .unwrap();
    let row = proposal::Entity::find()
        .filter(proposal::Column::ProposalId.eq(proposal_id(&waiting)))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.reason, "");
    assert_eq!(row.source_policy_json, "null");
    // An AI decision keeps only the reviewer's closed code; its receipt in
    // the conversation carries the full reason.
    let (decided, recorder) = decide(&db, &waiting, true, ai_decider("The owner named it.")).await;
    assert_eq!(decided.decision_reason, None);
    assert_eq!(
        decided.decision_reason_code.as_deref(),
        Some("within_scope")
    );
    assert!(current_skill(&db, "unproven").await.is_none());
    let written = recorder.written_states.lock().unwrap().clone();
    let turn = PersistedAgentSession::decode_json(written.last().unwrap()).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_str(&turn.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["reason"], "The owner named it.");
    assert_eq!(receipt["reason_code"], "within_scope");
    // A conversation that may not keep owner data cannot propose removals.
    authored(&db, "call-9", "temporary").await;
    let mut session = session("call-10", &removal_args("temporary", 1, "disable"));
    session.conversation[1].tool_calls[0].name = authoring::REMOVE_TOOL.into();
    let mut ineligible = host(session.clone(), true);
    ineligible.data_eligible = false;
    let call = ToolCall {
        id: "call-10".into(),
        name: authoring::REMOVE_TOOL.into(),
        arguments_json: removal_args("temporary", 1, "disable").to_string(),
    };
    assert_eq!(
        rejected_with(propose_on(&ineligible, &db, &session, &call, &Default::default()).await),
        authoring::UNAVAILABLE
    );
}

#[tokio::test]
async fn a_changed_skill_conflicts_its_removal_and_a_rejection_keeps_it() {
    let db = db().await;
    let skill = authored(&db, "call-1", "disk-report").await;
    let (_, rejected) = propose_removal(
        &db,
        "call-2",
        removal_args("disk-report", 1, "delete"),
        true,
    )
    .await
    .unwrap();
    let (row, _) = decide(&db, &rejected, false, SkillDecider::Owner).await;
    assert_eq!(row.status, "rejected");
    assert!(current_skill(&db, "disk-report").await.unwrap().enabled);
    let (_, waiting) = propose_removal(
        &db,
        "call-3",
        removal_args("disk-report", 1, "delete"),
        true,
    )
    .await
    .unwrap();
    update_from_manual(&db, "disk-report", &skill.skill_id, skill.revision as u64).await;
    let (row, host) = decide(&db, &waiting, true, SkillDecider::Owner).await;
    assert_eq!(row.status, "conflicted");
    assert!(
        row.decision_reason
            .as_deref()
            .unwrap()
            .contains("no longer assistant-written")
    );
    assert_eq!(
        current_skill(&db, "disk-report")
            .await
            .unwrap()
            .current_version,
        2
    );
    assert_eq!(decision_receipt(&host)["status"], "conflicted");
    assert_eq!(pending_count(&db).await, 0);
}

/// An AI decider whose reason has no verified provenance.
fn ai_decider(reason: &str) -> SkillDecider {
    SkillDecider::Ai(
        desk_diagnose_core::proposal_review::provenance::AiDecisionReason {
            reason: reason.into(),
            reason_code: "within_scope".into(),
            provenance: desk_diagnose_core::proposal_review::provenance::ReviewProvenance {
                verifiable: false,
                evidence: Vec::new(),
            },
        },
    )
}
