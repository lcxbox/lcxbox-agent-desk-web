use super::*;
use crate::{
    chat::{ChatRole, ToolCall},
    model_message_labels::{
        COMPOSED_RESULT_PREFIX, ServerResultSource, UNVERIFIED_RESULT_PREFIX, composed_envelope,
        model_bound_user_message,
    },
};
use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};

fn destination(revision: u64) -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: revision,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

fn label(id: &str, revision: u64) -> DataEnvelope {
    model_bound_user_message(id.into(), "text".into(), destination(revision))
        .unwrap()
        .data_envelope
        .unwrap()
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    }
}

crate::model_receipt! {
    struct Receipt {
        status: &'static str,
        id: ServerId,
        name: Option<ModelArg<String>>,
        page: Vec<Sourced<String>>,
    }
}

#[test]
fn constants_and_arguments_register_under_the_issuing_output() {
    let issuing = label("issuing", 1);
    let receipt = Receipt {
        status: "saved",
        id: ServerId::new("p1"),
        name: Some(ModelArg::of_call(
            &call("c1"),
            Some(&issuing),
            "notes".into(),
        )),
        page: Vec::new(),
    };
    let content = ModelContent::json(&receipt).unwrap();
    assert_eq!(
        content.text(),
        r#"{"status":"saved","id":"p1","name":"notes","page":[]}"#
    );
    let envelope = composed_envelope(
        &content,
        Some(&issuing),
        "c1",
        ServerResultSource::SkillProposal,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(envelope.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
    assert_eq!(
        envelope.provenance.source_envelope_ids,
        ["user-message-issuing"]
    );
    assert_eq!(envelope.allowed_destinations, issuing.allowed_destinations);
}

#[test]
fn arguments_of_another_call_or_output_are_rejected() {
    let issuing = label("issuing", 1);
    let other = label("other", 1);
    let arg = ModelArg::of_call(&call("c1"), Some(&other), "x".to_string());
    let content = ModelContent::builder().arg(&arg).build();
    for (output, call_id) in [(&issuing, "c1"), (&other, "c2")] {
        assert!(
            composed_envelope(
                &content,
                Some(output),
                call_id,
                ServerResultSource::SkillProposal,
                false
            )
            .is_err()
        );
    }
}

#[test]
fn sourced_text_adds_its_label_after_the_issuing_output() {
    let issuing = label("issuing", 1);
    let mut device = label("device", 1);
    device.sensitivity = Sensitivity::Sensitive;
    device.allowed_destinations.push(destination(2));
    let content = ModelContent::builder()
        .text("read: ")
        .sourced(&Sourced::new("disk full".to_string(), device.clone()))
        .build();
    assert_eq!(content.text(), "read: disk full");
    let envelope = composed_envelope(
        &content,
        Some(&issuing),
        "c1",
        ServerResultSource::SkillProposal,
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        envelope.provenance.source_envelope_ids,
        ["user-message-issuing", "user-message-device"]
    );
    assert_eq!(envelope.sensitivity, Sensitivity::Sensitive);
    assert_eq!(envelope.allowed_destinations, [destination(1)]);
}

#[test]
fn unverified_text_gets_the_unregistered_shape() {
    let issuing = label("issuing", 1);
    let content = ModelContent::builder()
        .text("tool error: ")
        .unverified(&Unverified::new("upstream said no"))
        .build();
    let envelope = composed_envelope(
        &content,
        Some(&issuing),
        "c1",
        ServerResultSource::SkillProposal,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(envelope.envelope_id.starts_with(UNVERIFIED_RESULT_PREFIX));
}

#[test]
fn content_without_any_parent_stays_unlabelled() {
    let message = crate::chat::ChatMessage::from_content(
        ChatRole::SystemEvent,
        "m1",
        &ModelContent::constant("notice"),
        Labelling {
            issuing: None,
            call_id: "m1",
            source: ServerResultSource::InputWatermark,
            request_only: true,
        },
    )
    .unwrap();
    assert_eq!(message.text, "notice");
    assert!(message.data_envelope.is_none());
}

#[test]
fn fixed_execution_reasons_stay_server_text_and_host_reasons_stay_unverified() {
    use crate::seam::ExecReason;
    let fixed = ExecReason::Server("disabled by policy").content("not executed: ");
    assert_eq!(fixed.text(), "not executed: disabled by policy");
    assert!(!fixed.is_unverified());
    let host = ExecReason::Unverified("device said no".into()).content("not executed: ");
    assert_eq!(host.text(), "not executed: device said no");
    assert!(host.is_unverified());
}
