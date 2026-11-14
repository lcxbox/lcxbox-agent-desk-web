//! AI review of a skill proposal decides only the exact waiting proposal, and
//! the owner decides once the reviewer cannot.
use super::schedule_approval::{pause_turn, reviewed_kind, saved, turn_fixture};
use super::*;
use crate::agent_approval_store as approval;
use desk_assistant_data_store::{entity::agent_skill_proposal as proposal, skill_store};
use desk_diagnose_core::{
    approval_review::{ApprovalVerdict, PendingRequestKind},
    chat::{ChatMessage, ToolCall},
    proposal_review::{DecisionKind, DecisionPause},
};
use sea_orm::{ActiveModelTrait, Set};

fn proof() -> String {
    let policy = desk_diagnose_core::ai_assistant::source::owner_text_policy(1, false);
    serde_json::to_string(
        &desk_diagnose_core::skills::authoring::AuthoredSkillSource::new(
            "1",
            policy.clone(),
            desk_diagnose_core::ai_assistant::source_registry::NativeDerivedInputPolicy {
                actor_id: "1".into(),
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
                text_policy: Some(policy),
            },
        )
        .unwrap(),
    )
    .unwrap()
}

/// A turn that saved a skill proposal and waits for its decision. The row and
/// receipt match what `propose_on` stores; the native output proof is fixed.
async fn skill_fixture() -> (DatabaseConnection, PersistedAgentSession, String, u64) {
    let call = ToolCall {
        id: "skill-call".into(),
        name: desk_diagnose_core::skills::authoring::PROPOSE_TOOL.into(),
        arguments_json: serde_json::json!({"name":"morning-check","description":"Morning checks",
            "body":"Prepare a read-only status report.","reason":"The owner asked to keep it"})
        .to_string(),
    };
    let (db, mut session, now) = turn_fixture(true, &call).await;
    let now_ms = now.timestamp_millis();
    let id = "proposal1".to_owned();
    proposal::ActiveModel {
        proposal_id: Set(id.clone()),
        actor_id: Set("1".into()),
        conversation_id: Set(session.conversation_id.clone()),
        input_revision: Set(session.input_revision as i64),
        kind: Set("create".into()),
        name: Set("morning-check".into()),
        base_skill_id: Set(None),
        base_version: Set(None),
        content_json: Set(serde_json::json!({"skillMd":
            "---\nname: morning-check\ndescription: Morning checks\n---\nPrepare a read-only status report."})
            .to_string()),
        source_policy_json: Set(proof()),
        reason: Set("The owner asked to keep it".into()),
        status: Set("pending".into()),
        decision_source: Set(None),
        decision_reason: Set(None),
        created_at_ms: Set(now_ms),
        decided_at_ms: Set(None),
        revision: Set(1),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    // The saved proposal holds one pending slot of the owner's scope.
    desk_assistant_data_store::entity::agent_skill_scope::ActiveModel {
        scope_key: Set(crate::assistant_skills::personal_scope()),
        skill_count: Set(0),
        enabled_count: Set(0),
        pending_proposal_count: Set(1),
        revision: Set(1),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let text = serde_json::json!({"proposal_id":id,"status":"pending","awaiting_decision":true})
        .to_string();
    let mut receipt = ChatMessage::tool_result("skill-proposal:skill-call", "skill-call", &text);
    receipt.turn_id = session.current_turn_id.clone();
    receipt.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            session.conversation.last().unwrap().data_envelope.as_ref(),
            "skill-call",
            &text,
            "skill_proposal",
        )
        .unwrap();
    session.conversation.push(receipt);
    let pause = DecisionPause {
        kind: DecisionKind::Skill,
        id: id.clone(),
        turn_id: session.current_turn_id.clone().unwrap(),
        call_id: "skill-call".into(),
        input_revision: session.input_revision,
        requested_at_unix_ms: now_ms as u64,
    };
    let session = pause_turn(&db, session, pause).await;
    (db, session, id, now_ms as u64)
}

#[actix_web::test]
async fn ai_approval_saves_the_waiting_skill_and_answers_its_turn() {
    let (db, session, id, now) = skill_fixture().await;
    let status = reviewed_kind(
        &db,
        &session,
        &id,
        PendingRequestKind::Skill,
        ApprovalVerdict::Approve,
        now,
    )
    .await;
    assert_eq!(status, "approved");
    let skill = skill_store::find_by_name_on(
        &db,
        &crate::assistant_skills::personal_scope(),
        "morning-check",
    )
    .await
    .unwrap()
    .unwrap();
    let version = skill_store::version_on(&db, &skill.skill_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(version.origin, "ai_authored");
    assert_eq!(version.proposal_id.as_deref(), Some(id.as_str()));
    let review = crate::entity::agent_approval_review::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.source_kind, "skill_proposal");
    assert_eq!(review.source_id, id);
    let current = saved(&db, &session.conversation_id).await;
    assert_eq!(current.conversation.len(), session.conversation.len() + 1);
    let receipt: serde_json::Value =
        serde_json::from_str(&current.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["event"], "skill_proposal_decided");
    assert_eq!(receipt["status"], "approved");
    assert_eq!(receipt["decided_by"], "approval_ai");
    // The decision resumes the paused turn exactly once.
    let resume = desk_assistant_data_store::entity::agent_decision_resume::Entity::find()
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
        ("skill", id.as_str(), "pending")
    );
    // The decided proposal is no longer offered for review or to the owner.
    assert!(
        !approval::pending_permission_reviews(&db, 0, 64, now + 2)
            .await
            .unwrap()
            .0
            .iter()
            .any(|item| item.kind == PendingRequestKind::Skill)
    );
    assert!(
        crate::assistant_skills::pending_proposals(&db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[actix_web::test]
async fn ai_denial_rejects_the_exact_skill_proposal() {
    let (db, session, id, now) = skill_fixture().await;
    let status = reviewed_kind(
        &db,
        &session,
        &id,
        PendingRequestKind::Skill,
        ApprovalVerdict::Deny,
        now,
    )
    .await;
    assert_eq!(status, "denied");
    let row = proposal::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(row.status, "rejected");
    assert_eq!(row.decision_source.as_deref(), Some("ai_review"));
    assert!(
        skill_store::find_by_name_on(
            &db,
            &crate::assistant_skills::personal_scope(),
            "morning-check"
        )
        .await
        .unwrap()
        .is_none()
    );
    let receipt: serde_json::Value = serde_json::from_str(
        &saved(&db, &session.conversation_id)
            .await
            .conversation
            .last()
            .unwrap()
            .text,
    )
    .unwrap();
    assert_eq!(receipt["status"], "rejected");
}

#[actix_web::test]
async fn the_owner_decides_a_pending_skill_proposal_and_the_reviewer_has_nothing_left() {
    let (db, session, id, now) = skill_fixture().await;
    let pending = crate::assistant_skills::pending_proposals(&db)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].proposal_id, id);
    assert!(pending[0].skill_md.contains("read-only status report"));
    let stale = desk_agent_protocol::ai_assistant::skill::SkillProposalDecision {
        proposal_id: id.clone(),
        approve: true,
        expected_revision: 7,
    };
    assert!(matches!(
        crate::assistant_skills::decide_proposal(&db, &stale).await,
        Err(crate::assistant_skills::ProposalDecisionError::Conflict)
    ));
    let view = crate::assistant_skills::decide_proposal(
        &db,
        &desk_agent_protocol::ai_assistant::skill::SkillProposalDecision {
            proposal_id: id.clone(),
            approve: true,
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        view.status,
        desk_agent_protocol::ai_assistant::skill::SkillProposalStatus::Approved
    );
    assert_eq!(
        view.decision_source,
        Some(desk_agent_protocol::ai_assistant::skill::SkillDecisionSource::Owner)
    );
    let current = saved(&db, &session.conversation_id).await;
    let receipt: serde_json::Value =
        serde_json::from_str(&current.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["decided_by"], "owner");
    assert!(
        approval::prepare_permission_review_batch(
            &db,
            &session.conversation_id,
            "1",
            &session.device_id,
            &id,
            PendingRequestKind::Skill,
            &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            0,
            now,
        )
        .await
        .is_err()
    );
    // The conversation snapshot lists the decided proposal for its card.
    let cards = crate::assistant_skills::conversation_proposals(&db, "1", &session.conversation_id)
        .await
        .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].status,
        desk_agent_protocol::ai_assistant::skill::SkillProposalStatus::Approved
    );
}

#[actix_web::test]
async fn stopping_the_paused_conversation_withdraws_its_skill_proposal() {
    let (db, session, id, _) = skill_fixture().await;
    let control = desk_agent_protocol::ai_assistant::subagent::AiAssistantStopControl {
        client_request_id: "stop-paused".into(),
        expected_input_revision: session.input_revision,
        expected_control_revision: session.control_revision,
        subagent_choice: None,
    };
    SubAgentStore::new(db.clone())
        .stop_for_owner(
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
            &control,
        )
        .await
        .unwrap();
    let stopped = saved(&db, &session.conversation_id).await;
    assert!(stopped.main_stopped);
    assert!(stopped.decision_pause.is_none());
    let row = proposal::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(
        (row.proposal_id.as_str(), row.status.as_str()),
        (id.as_str(), "withdrawn")
    );
    // The withdrawn proposal frees its pending slot.
    let scope = desk_assistant_data_store::entity::agent_skill_scope::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scope.pending_proposal_count, 0);
}
