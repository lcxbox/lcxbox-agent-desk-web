//! Ephemeral server state, separate from stable instructions and durable history.
use crate::chat::{ChatMessage, ChatRole};

pub const MESSAGE_ID: &str = "agent-runtime-context";

pub fn is_runtime(message: &ChatMessage) -> bool {
    message.role == ChatRole::SystemEvent && message.message_id == MESSAGE_ID
}

/// Return the initial server snapshot for fresh construction on every request.
pub fn take(system: &mut ChatMessage) -> ChatMessage {
    system
        .runtime_context
        .take()
        .map(|value| *value)
        .unwrap_or_else(|| ChatMessage::system_event(MESSAGE_ID, ""))
}

/// Appends `content` to `message`, gathering its sources for the label the
/// message gets once complete.
pub fn append(message: &mut ChatMessage, content: &crate::model_content::ModelContent) {
    message.text.push_str(content.text());
    message.content_sources.merge(content.sources());
}

/// Agent state stays at the request tail; diagnostic-only prompts retain
/// their dedicated system contract.
pub fn append_instruction(system: &mut ChatMessage, content: &crate::model_content::ModelContent) {
    match system.runtime_context.as_deref_mut() {
        Some(runtime) => append(runtime, content),
        None => append(system, content),
    }
}

/// Labels the finished runtime context from everything appended to it, under
/// `parent`, with its stored text bound to `destination`.
pub fn label(
    runtime: &mut ChatMessage,
    parent: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
    destination: Option<&desk_agent_protocol::data_lineage::DestinationIdentity>,
) -> Result<(), desk_agent_protocol::AgentError> {
    let mut content = crate::model_content::ModelContent::from_parts(
        runtime.text.clone(),
        std::mem::take(&mut runtime.content_sources),
    );
    content.bind(&destination.cloned().into_iter().collect::<Vec<_>>())?;
    let label = crate::model_message_labels::composed_envelope(
        &content,
        parent,
        MESSAGE_ID,
        crate::model_message_labels::ServerResultSource::RuntimeContext,
        true,
    )?;
    runtime.set_label_and_roots(label, content.stored_roots());
    Ok(())
}

/// A response locale as a short server-checked tag, or `None` when it is not
/// a plausible BCP-47 tag.
pub fn locale_tag(locale: &str) -> Option<crate::model_content::ServerText> {
    (!locale.is_empty()
        && locale.len() <= 35
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then(|| crate::model_content::ServerText::generated(locale))
}

#[cfg(test)]
mod tests {
    #[test]
    fn static_instructions_ignore_locale_and_runtime_facts() {
        let mut first = crate::ai_agent::build_ai_agent_system_message(Some("zh-CN"));
        super::append_instruction(
            &mut first,
            &crate::model_content::ModelContent::server_text("fact A"),
        );
        let mut second = crate::ai_agent::build_ai_agent_system_message(Some("en-US"));
        super::append_instruction(
            &mut second,
            &crate::model_content::ModelContent::server_text("fact B"),
        );
        assert_eq!(first.text, second.text);
        assert_ne!(super::take(&mut first).text, super::take(&mut second).text);
        assert!(first.runtime_context.is_none());
        assert!(!serde_json::to_string(&second).unwrap().contains("fact B"));
    }
    /// The runtime context's label follows everything appended to it: cited
    /// stored text joins as roots bound to the model destination.
    #[test]
    fn the_label_cites_appended_stored_text() {
        use crate::ai_agent::source_registry::{
            NativeInputPolicyRegistration, RegisteredSourceContract, SOURCE_CONTRACT_REVISION,
        };
        use crate::model_content::{ModelContent, ServerText, StoredSpec, StoredTextSource};
        use desk_agent_protocol::data_lineage::DestinationIdentity;
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "read the log".into(),
            destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let retained = |proof: Option<StoredTextSource>| {
            let grants = ModelContent::builder()
                .server(&ServerText::generated("grants: [log]"))
                .cite(StoredSpec::proven(
                    "permission_request",
                    "request-1",
                    "items",
                    "[log]",
                    proof,
                ))
                .build();
            let devices = ModelContent::builder()
                .server(&ServerText::generated(" device office-mac"))
                .cite(StoredSpec::device_reported(
                    "device",
                    "target-1",
                    "name",
                    "office-mac",
                ))
                .build();
            let mut system = crate::ai_agent::build_ai_agent_system_message(Some("zh-CN"));
            super::append_instruction(&mut system, &grants);
            super::append_instruction(&mut system, &devices);
            let mut runtime = super::take(&mut system);
            super::label(&mut runtime, Some(&owner), Some(&destination)).unwrap();
            assert!(runtime.text.contains("office-mac"));
            assert_eq!(runtime.stored_roots.len(), 2);
            let label = runtime.data_envelope.clone().unwrap();
            let inputs = vec![owner.clone(), label];
            let audit = crate::sink_authorizer::SinkProjectionAudit {
                destination: destination.clone(),
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
                &runtime.stored_roots,
            )
            .unwrap();
            let registration =
                NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1)
                    .unwrap();
            assert_eq!(
                registration.leaves[1].as_ref().map(|leaf| leaf.contract),
                Some(RegisteredSourceContract::NativeControl)
            );
            registration
                .complete_leaf_text_policy(&snapshot)
                .unwrap()
                .is_some_and(|policy| policy.history_retention_allowed)
        };
        assert!(retained(Some(StoredTextSource::owner("owner", 1))));
        assert!(
            !retained(None),
            "an unproven request input is never retained"
        );
    }
}
