use super::*;
use crate::chat::{ChatMessage, ToolCallRef};
use crate::session::{TriggerOrigin, TurnState};
use crate::subagent::{DelegatedTaskBinding, DelegationSource};
use desk_agent_protocol::{AgentScope, ExecutionMode};

const NOW: u64 = 1792049770485;

fn scope() -> AgentScope {
    AgentScope {
        granted: vec![],
        mode: ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    }
}

fn main_session(origin: TriggerOrigin) -> PersistedAgentSession {
    let mut session = crate::ai_assistant::test_support::personal_session("owner", "run", "device");
    session.input_revision = 1;
    session.turn_state = TurnState::Running;
    session.adopt_trigger(origin, "turn");
    session
}

fn child_session(source: DelegationSource) -> PersistedAgentSession {
    let mut child = PersistedAgentSession::new_subagent(
        "child",
        "owner",
        "device",
        1,
        scope(),
        DelegatedTaskBinding {
            targets: vec![crate::dynamic_run::AssistantInputTarget::fixed(
                "device".into(),
            )],
            root_conversation_id: "root".into(),
            group_id: "group".into(),
            task_id: "task".into(),
            source,
            objective: "Investigate".into(),
            acceptance_criteria: vec!["Evidence".into()],
            input_revision: 1,
            control_revision: 1,
            source_epoch: 1,
            deadline_ms: 10_000,
        },
        "2026-09-30T00:00:00Z",
    )
    .unwrap();
    child.surface = crate::session::AgentSessionSurface::AiAssistant;
    child
        .begin_turn("turn", None, None, 1, scope(), "2026-09-30T00:00:00Z")
        .unwrap();
    child.adopt_trigger(TriggerOrigin::DelegatedTask, "turn");
    child
}

fn exposed(session: &PersistedAgentSession) -> bool {
    crate::registry::lookup_for_session(&registry(), PROPOSE_TOOL, session).is_some()
}

#[test]
fn only_owner_directed_turns_can_propose_a_skill() {
    assert!(exposed(&main_session(TriggerOrigin::User)));
    for origin in [
        TriggerOrigin::PermissionDecision,
        TriggerOrigin::WorkContinuation,
        TriggerOrigin::GoalContinuation,
        TriggerOrigin::ScheduledTask,
        TriggerOrigin::ScheduledContinuation,
        TriggerOrigin::IssueSubscription,
        TriggerOrigin::SubAgentCompletion,
    ] {
        let session = main_session(origin);
        assert!(!exposed(&session), "{origin:?}");
        let call = call(json!({"name":"a","description":"b","body":"c","reason":"d"}));
        assert!(
            parse(&session, &call, &Default::default()).is_err(),
            "{origin:?}"
        );
    }
    for source in [
        DelegationSource::UserInput { input_revision: 1 },
        DelegationSource::WorkbenchOwner {
            workbench_id: "bench".into(),
            input_revision: 1,
        },
    ] {
        assert!(exposed(&child_session(source.clone())), "{source:?}");
    }
    for source in [
        DelegationSource::Goal {
            goal_id: "goal".into(),
        },
        DelegationSource::ScheduledOccurrence {
            schedule_id: "schedule".into(),
            occurrence_id: "occurrence".into(),
        },
    ] {
        assert!(!exposed(&child_session(source.clone())), "{source:?}");
    }
}

fn call(arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "call-private-id".into(),
        name: PROPOSE_TOOL.into(),
        arguments_json: arguments.to_string(),
    }
}

#[test]
fn parse_rejects_platform_targets_and_turn_changes() {
    let session = main_session(TriggerOrigin::User);
    let ok = parse(
        &session,
        &call(json!({"name":"disk-report","description":"Report disks","body":"Run df","reason":"Owner asked to keep it",
            "files":[{"path":"scripts/df.sh","text":"df -h"}],"base":{"skill_ref":"personal/disk-report","version":2}})),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(ok.base_version, Some(2));
    assert_eq!(ok.files.len(), 1);
    for base in ["platform/disk-report", "bundled/issue-analysis"] {
        let error = parse(
            &session,
            &call(
                json!({"name":"disk-report","description":"x","body":"y","reason":"z",
                "base":{"skill_ref":base,"version":1}}),
            ),
            &Default::default(),
        )
        .unwrap_err();
        assert!(error.contains("platform and bundled"), "{error}");
    }
    assert!(
        parse(
            &session,
            &call(
                json!({"name":"disk-report","description":"x","body":"y","reason":"z",
                "base":{"skill_ref":"personal/other","version":1}})
            ),
            &Default::default(),
        )
        .is_err()
    );
    assert!(
        parse(
            &session,
            &call(json!({"name":"a","description":"x","body":"y","reason":" "})),
            &Default::default(),
        )
        .is_err()
    );
    let mut idle = session.clone();
    idle.turn_state = TurnState::Idle;
    assert!(
        parse(
            &idle,
            &call(json!({"name":"a","description":"x","body":"y","reason":"z"})),
            &Default::default()
        )
        .is_err()
    );
}

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "model-private-id".into(),
        connection_revision: 1,
        model_id: "model-private-name".into(),
        profile_revision: 1,
    }
}

fn proposal(body: &str) -> SkillProposal {
    SkillProposal {
        proposal_id: "p1".into(),
        revision: 1,
        owner_id: "owner".into(),
        conversation_id: "run".into(),
        input_revision: 1,
        created_at_unix_ms: NOW,
        name: "disk-report".into(),
        base_version: None,
        skill_md: format!("---\nname: disk-report\ndescription: Report disks\n---\n{body}"),
        files: vec![],
        changes: vec![],
        reason: "The owner asked to keep this".into(),
    }
}

/// A turn whose proposal receipt waits for the reviewer.
fn waiting_session() -> PersistedAgentSession {
    waiting_session_on("device")
}

/// `device` is empty for a personal conversation bound to no device.
fn waiting_session_on(device: &str) -> PersistedAgentSession {
    let mut session =
        PersistedAgentSession::new("run", "owner", device, 1, scope(), "2026-10-15T07:36:10Z");
    session.adopt_client_metadata(
        Some("client"),
        crate::session::AgentSessionSurface::AiAssistant,
    );
    session.input_revision = 1;
    session.control_revision = 1;
    session.latest_input_seq = 1;
    session.current_turn_id = Some("turn-private-id".into());
    session.turn_state = TurnState::Running;
    session.lease_token = 1;
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "Keep this disk report procedure as a skill.".into(),
        destination(),
    )
    .unwrap();
    let mut assistant = ChatMessage::assistant_tool_calls(
        "assistant",
        "I propose a skill",
        vec![ToolCallRef {
            id: "call-private-id".into(),
            name: PROPOSE_TOOL.into(),
            arguments_json: json!({"name":"disk-report"}).to_string(),
        }],
    );
    assistant.turn_id = session.current_turn_id.clone();
    let bytes = crate::model_egress::message_content_bytes(&assistant).unwrap();
    assistant.data_envelope = Some(
        crate::subagent::projection::envelope(
            "assistant",
            &String::from_utf8(bytes).unwrap(),
            "test",
            &[user.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    let text = json!({"proposal_id":"p1","status":"pending","awaiting_decision":true}).to_string();
    let mut receipt =
        ChatMessage::tool_result("skill-proposal:call-private-id", "call-private-id", &text);
    receipt.turn_id = session.current_turn_id.clone();
    receipt.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        assistant.data_envelope.as_ref(),
        "call-private-id",
        &text,
        "skill_proposal",
    )
    .unwrap();
    session.conversation = vec![user, assistant, receipt];
    // The proposing turn ended paused on the proposal.
    session.turn_state = TurnState::Idle;
    session.decision_pause = Some(crate::proposal_review::DecisionPause {
        kind: crate::proposal_review::DecisionKind::Skill,
        id: "p1".into(),
        turn_id: "turn-private-id".into(),
        call_id: "call-private-id".into(),
        input_revision: session.input_revision,
        requested_at_unix_ms: 0,
    });
    session
}

fn delegation() -> ApprovalDelegation {
    delegation_on("device")
}

fn delegation_on(device: &str) -> ApprovalDelegation {
    ApprovalDelegation::new_usage_only(
        "delegation-private-id".into(),
        "run".into(),
        "owner".into(),
        device.into(),
        "switch-private-id".into(),
        NOW,
    )
    .unwrap()
}

#[test]
fn a_waiting_skill_proposal_is_reviewed_with_its_full_content() {
    let session = waiting_session();
    let proposal = proposal("Run df -h and summarize the result.");
    let candidate = candidate(&session, &proposal, &delegation(), NOW).unwrap();
    authorize(
        &candidate,
        &session,
        &proposal,
        &delegation(),
        &destination(),
        NOW,
    )
    .unwrap();
    let view = crate::approval_review::ReviewModelView::from_candidate(&candidate).unwrap();
    let json = serde_json::to_string(&view).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["source_kind"], "skill_proposal");
    let action: serde_json::Value =
        serde_json::from_str(value["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["operation"], "create_personal_skill");
    assert_eq!(action["skill_ref"], "personal/disk-report");
    assert!(
        action["skill_md"]
            .as_str()
            .unwrap()
            .contains("Run df -h and summarize the result.")
    );
    assert_eq!(action["third_party_source_kinds"], json!([]));
    assert_eq!(action["device_actions_authorized"], false);
    for secret in [
        "delegation-private-id",
        "call-private-id",
        "turn-private-id",
        "model-private-name",
    ] {
        assert!(!json.contains(secret), "private binding leaked: {secret}");
    }
    assert!(APPROVAL_REVIEW_SYSTEM_PROMPT.contains("For skill_proposal"));
    assert!(APPROVAL_REVIEW_SYSTEM_PROMPT.contains("relayed from a web page"));
}

#[test]
fn a_proposal_from_a_conversation_without_a_device_is_reviewed() {
    let session = waiting_session_on("");
    let delegation = delegation_on("");
    let proposal = proposal("Run df -h and summarize the result.");
    let candidate = candidate(&session, &proposal, &delegation, NOW).unwrap();
    assert!(candidate.device_id.is_empty());
    let mut scoped = candidate.clone();
    scoped.input_kind = crate::approval_review::ApprovalInputKind::Scope;
    assert_eq!(
        scoped.validate(),
        Err(ApprovalReviewError::InvalidCandidate)
    );
    authorize(
        &candidate,
        &session,
        &proposal,
        &delegation,
        &destination(),
        NOW,
    )
    .unwrap();
}

#[test]
fn only_the_paused_proposing_conversation_can_be_reviewed() {
    let session = waiting_session();
    let base = proposal("Run df -h.");
    for case in 0..7 {
        let mut session = session.clone();
        let mut proposal = base.clone();
        match case {
            0 => session.decision_pause = None,
            1 => session.turn_state = TurnState::Running,
            2 => session.input_revision += 1,
            3 => proposal.conversation_id = "other".into(),
            4 => proposal.owner_id = "someone-else".into(),
            5 => {
                session.decision_pause.as_mut().unwrap().kind =
                    crate::proposal_review::DecisionKind::Schedule
            }
            6 => session.conversation.pop().map(|_| ()).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            candidate(&session, &proposal, &delegation(), NOW).is_err(),
            "case {case}"
        );
    }
    // The AI review window closes; the owner decides afterwards.
    assert!(
        candidate(
            &session,
            &base,
            &delegation(),
            base.review_expires_at_unix_ms()
        )
        .is_err()
    );
}

#[test]
fn the_serialized_review_action_bounds_a_proposal() {
    let session = waiting_session();
    let fitting = proposal(&"a".repeat(40 * 1024));
    assert!(fits_review(&session, &fitting, "call-private-id"));
    assert!(action(&session, &fitting).is_ok());
    // Quotes and backslashes double when escaped; raw bytes understate the size.
    let escaped = proposal(&"\"\\".repeat(20 * 1024));
    assert!(escaped.skill_md.len() < MAX_APPROVAL_ACTION_BYTES);
    assert!(!fits_review(&session, &escaped, "call-private-id"));
    assert!(action(&session, &escaped).is_err());
}

fn native_source(policy: Option<SourceUsePolicy>) -> NativeDerivedInputPolicy {
    NativeDerivedInputPolicy {
        actor_id: "owner".into(),
        conversation_id: "run".into(),
        physical_call_id: "call".into(),
        output_envelope_id: "output".into(),
        output_digest_sha256: "a".repeat(64),
        output_label_sha256: "b".repeat(64),
        actor_input_seq: 1,
        input_revision: 1,
        source_contract_revision: crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
        policy_revision: 1,
        text_policy: policy,
    }
}

#[test]
fn only_retainable_content_can_be_authored() {
    let retained = crate::ai_assistant::source::owner_text_policy(1, false);
    let source = AuthoredSkillSource::new(
        "owner",
        retained.clone(),
        native_source(Some(retained.clone())),
    )
    .unwrap();
    assert!(!source.policy.independent_fact_allowed);
    source.validate("owner").unwrap();
    assert!(source.validate("someone-else").is_err());
    let mut no_copy = retained.clone();
    no_copy.no_copy = true;
    no_copy.derived_visible_text_allowed = false;
    no_copy.no_export = true;
    assert!(AuthoredSkillSource::new("owner", no_copy, native_source(None)).is_err());
    let mut media = retained.clone();
    media.class = desk_agent_protocol::ai_assistant::source::SourcePolicyClass::Media;
    assert!(AuthoredSkillSource::new("owner", media, native_source(None)).is_err());
    let mut transient = retained;
    transient.history_retention_allowed = false;
    assert_eq!(
        AuthoredSkillSource::new("owner", transient, native_source(None)).unwrap_err(),
        NOT_RETAINABLE
    );
}

#[test]
fn a_decision_is_projected_onto_the_original_call_for_the_model() {
    let mut session = waiting_session();
    let text = json!({"event":crate::proposal_review::SKILL_DECIDED_EVENT,"proposal_id":"p1",
        "status":"approved","version":2,"note":"This request keeps using the loaded version 1; new owner messages use version 2."})
    .to_string();
    let mut receipt = ChatMessage::system_event("decision", &text);
    receipt.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        session.conversation.last().unwrap().data_envelope.as_ref(),
        "decision",
        &text,
        "skill_proposal_decision",
    )
    .unwrap();
    session.conversation.push(receipt);
    let mut messages = session.conversation.clone();
    project_review_results(&mut messages).unwrap();
    let projected: serde_json::Value = serde_json::from_str(&messages[2].text).unwrap();
    assert_eq!(projected["review_complete"], true);
    assert_eq!(projected["awaiting_decision"], false);
    assert!(projected["note"].as_str().unwrap().contains("version 2"));
    assert_eq!(
        messages[2]
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_tool_name,
        "skill_review_result"
    );
    // The stored receipt is unchanged.
    assert!(
        session.conversation[2]
            .text
            .contains("\"awaiting_decision\":true")
    );
}
