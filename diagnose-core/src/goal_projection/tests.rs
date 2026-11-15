use super::*;
use crate::{
    ai_assistant::source_registry::{
        NativeInputPolicyRegistration, RegisteredSourceContract, SOURCE_CONTRACT_REVISION,
    },
    goal::{GoalLimits, GoalModelBinding, GoalOpening},
    model_message_labels::model_bound_user_message,
};

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

fn goal() -> GoalRun {
    GoalRun::new(
        "goal".into(),
        "run".into(),
        "owner".into(),
        crate::dynamic_run::AssistantInputTarget::fixed("device".into()),
        "device".into(),
        "Finish the report".into(),
        "message".into(),
        GoalOpening::OwnerRequest,
        GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        1_000,
        GoalLimits::default(),
    )
    .unwrap()
}

fn owner_message() -> DataEnvelope {
    model_bound_user_message("message".into(), "Finish the report".into(), destination())
        .unwrap()
        .data_envelope
        .unwrap()
}

/// Whether an answer derived from `message` with its source owner message
/// may be retained.
fn retained(message: &ChatMessage) -> bool {
    let owner = owner_message();
    let label = message.data_envelope.clone().unwrap();
    let inputs = vec![owner, label];
    let audit = crate::sink_authorizer::SinkProjectionAudit {
        destination: destination(),
        envelope_ids: inputs
            .iter()
            .map(|input| input.envelope_id.clone())
            .collect(),
        total_bytes: 64,
    };
    let snapshot = crate::model_egress::NativeModelInputSnapshot::capture_with_roots(
        &audit,
        &inputs,
        &[],
        &message.stored_roots,
    )
    .unwrap();
    let registration =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert_eq!(
        registration.leaves[1].as_ref().map(|leaf| leaf.contract),
        Some(RegisteredSourceContract::NativeControl)
    );
    registration
        .complete_leaf_text_policy(&snapshot)
        .unwrap()
        .is_some_and(|policy| policy.history_retention_allowed)
}

#[test]
fn goal_text_counts_with_its_recorded_proof() {
    let mut goal = goal();
    let unproven = state_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert_eq!(unproven.stored_roots.len(), 1, "the goal text");
    assert!(
        !retained(&unproven),
        "an unproven goal text is never retained"
    );
    goal.text_sources.goal_text = Some(StoredTextSource::owner("owner", 1));
    let proven = state_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert!(proven.text.contains("Finish the report"));
    assert!(retained(&proven));
}

#[test]
fn a_checkpoint_counts_by_where_its_summary_came_from() {
    let mut goal = goal();
    goal.text_sources.goal_text = Some(StoredTextSource::owner("owner", 1));
    goal.checkpoint = Some(GoalCheckpoint {
        segment_seq: 1,
        event_seq: 2,
        summary: "Drafted the outline".into(),
        evidence_ids: Vec::new(),
        protected_attachment_ids: Vec::new(),
    });
    // Server-written text adds no root.
    goal.text_sources.checkpoint = CheckpointSource::Server;
    let server = state_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert_eq!(server.stored_roots.len(), 1);
    assert!(retained(&server));
    // A control output's text without a proof is never retained.
    goal.text_sources.checkpoint = CheckpointSource::Unproven;
    let unproven = state_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert_eq!(unproven.stored_roots.len(), 2);
    assert!(!retained(&unproven));
    goal.text_sources.checkpoint = CheckpointSource::Model {
        source: StoredTextSource::owner("owner", 1),
    };
    let proven = state_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert!(proven.text.contains("Drafted the outline"));
    assert!(retained(&proven));
}

#[test]
fn original_and_clarification_records_cite_their_text() {
    let mut goal = goal();
    goal.text_sources.original_goal_text = Some(StoredTextSource::owner("owner", 1));
    goal.text_sources.goal_text = Some(StoredTextSource::owner("owner", 1));
    let original = original_message(&goal, &owner_message(), Some(&destination())).unwrap();
    assert!(original.text.contains("server_goal_original"));
    assert!(retained(&original));
    let clarification =
        clarification_message(&goal, &owner_message(), Some(&destination()), false).unwrap();
    assert!(clarification.text.contains("\"reply_received\":false"));
    assert!(retained(&clarification));
}
