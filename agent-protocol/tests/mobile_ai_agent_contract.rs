use desk_agent_protocol::{
    agent_event::{AgentEvent, AgentEventKind},
    ai_agent::{
        AiAgentAsk, AiAgentContextUpdate, AiAgentContextUpdated, AiAgentObjectContextUpdate,
        skill::{
            SkillDecisionSource, SkillDiffOp, SkillProposalDecision, SkillProposalKind,
            SkillProposalStatus, SkillProposalView,
        },
    },
    capability_provider::CapabilityInventorySnapshot,
};
use serde_json::Value;

fn contract() -> Value {
    serde_json::from_str(include_str!("fixtures/mobile_ai_agent/contract-v1.json"))
        .expect("mobile AI Agent fixture must be valid JSON")
}

#[test]
fn canonical_mobile_contract_decodes_with_the_shared_protocol() {
    let root = contract();

    let minimal: AiAgentAsk =
        serde_json::from_value(root["asks"]["minimal"].clone()).expect("minimal ask");
    let full: AiAgentAsk = serde_json::from_value(root["asks"]["full"].clone()).expect("full ask");
    minimal.validate().expect("minimal ask is valid");
    full.validate().expect("full ask is valid");
    assert!(minimal.selected_issues.is_empty());
    assert_eq!(full.selected_issues[0].connection_ref, "forge");
    assert_eq!(full.selected_issues[0].issue_number, 4);
    assert_eq!(
        serde_json::to_value(&full).unwrap()["selected_issues"],
        root["asks"]["full"]["selected_issues"]
    );

    let events: Vec<AgentEvent> =
        serde_json::from_value(root["events"].clone()).expect("agent events");
    assert_eq!(events.len(), 10);
    assert!(
        events
            .iter()
            .any(|event| event.kind == AgentEventKind::Answer)
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == AgentEventKind::PermissionRequired)
    );

    let inventory: CapabilityInventorySnapshot =
        serde_json::from_value(root["inventory"].clone()).expect("inventory");
    inventory
        .validate()
        .expect("inventory is internally consistent");
    assert!(inventory.entries[0].context_selectable);

    let update: AiAgentContextUpdate =
        serde_json::from_value(root["context_update"].clone()).expect("context update");
    update.validate().expect("context update is valid");
    let ack: AiAgentContextUpdated =
        serde_json::from_value(root["context_updated"].clone()).expect("context ack");
    assert_eq!(ack.client_request_id, update.client_request_id);

    let object_update: AiAgentObjectContextUpdate =
        serde_json::from_value(root["object_context_update"].clone()).expect("object update");
    object_update.validate().expect("object update is valid");
    assert!(object_update.conversation_target.is_selectable());
    let mut missing_target = root["object_context_update"].clone();
    missing_target
        .as_object_mut()
        .unwrap()
        .remove("conversation_target");
    assert!(serde_json::from_value::<AiAgentObjectContextUpdate>(missing_target).is_err());
}

#[test]
fn canonical_feature_profile_and_mutations_keep_fail_closed_fields_explicit() {
    let root = contract();
    assert_eq!(root["snapshot"]["sessionId"], "session-1");
    assert_eq!(
        root["snapshot"]["fileScope"]["directories"][0]["deviceRef"],
        "device-1"
    );
    assert_eq!(root["snapshot"]["controlRevision"], 1);
    assert_eq!(root["snapshot"]["mainStopped"], false);
    assert_eq!(root["snapshot"]["subagents"]["attention_count"], 0);
    assert_eq!(
        root["snapshot"]["subagents"]["active_tasks"],
        serde_json::json!([])
    );
    assert_eq!(root["snapshot"]["capabilityGrants"][0]["inputRevision"], 3);
    assert_eq!(
        root["snapshot"]["permissionRequests"][0]["items"][0].get("externalSendConfirmation"),
        Some(&Value::Null),
    );
    let profiles = &root["feature_profiles"];
    assert_eq!(profiles["oss"]["ai_agent"]["schema_version"], 1);
    assert_eq!(profiles["oss"]["ai_agent"]["turn_stream"], true);
    assert_eq!(
        profiles["manager_inventory_only"]["ai_agent"]["turn_stream"],
        false
    );
    assert!(profiles["legacy"].get("ai_agent").is_none());

    let mutations = &root["mutations"];
    assert_eq!(mutations["permission_deny"]["items"][0]["decision"], "deny");
    assert!(
        mutations["permission_deny"]["items"][0]
            .get("resourceScope")
            .is_none()
    );
    assert_eq!(
        mutations["background_cancel"]["requestId"],
        "cancel-request-1"
    );
}

#[test]
fn canonical_skill_proposals_and_decision_match_the_shared_protocol() {
    let root = contract();
    let proposals: Vec<SkillProposalView> =
        serde_json::from_value(root["snapshot"]["skillProposals"].clone())
            .expect("snapshot skill proposals");
    assert_eq!(proposals.len(), 2);
    let pending = &proposals[0];
    assert_eq!(pending.kind, SkillProposalKind::Update);
    assert_eq!(pending.status, SkillProposalStatus::Pending);
    assert!(pending.awaiting_owner);
    assert_eq!(pending.decision_source, None);
    assert_eq!(pending.base_version, Some(2));
    assert_eq!(pending.diff[0].hunks[0].lines[1].op, SkillDiffOp::Remove);
    let decided = &proposals[1];
    assert_eq!(decided.status, SkillProposalStatus::Approved);
    assert!(!decided.awaiting_owner);
    assert_eq!(decided.decision_source, Some(SkillDecisionSource::AiReview));
    assert_eq!(
        serde_json::to_value(&proposals).unwrap(),
        root["snapshot"]["skillProposals"]
    );

    let decision: SkillProposalDecision =
        serde_json::from_value(root["mutations"]["skill_proposal_decision"].clone())
            .expect("skill proposal decision");
    assert_eq!(decision.proposal_id, pending.proposal_id);
    assert_eq!(decision.expected_revision, pending.revision);
    assert!(decision.approve);
}
