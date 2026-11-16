//! Ephemeral server state, separate from stable instructions and durable history.
use crate::chat::{ChatMessage, ChatRole};

pub const MESSAGE_ID: &str = "assistant-runtime-context";

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

/// Assistant state stays at the request tail; diagnostic-only prompts retain
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
    let Some(bound) = runtime.exact_projection.take() else {
        let label = crate::model_message_labels::composed_envelope(
            &content,
            parent,
            MESSAGE_ID,
            crate::model_message_labels::ServerResultSource::RuntimeContext,
            true,
        )?;
        runtime.set_label_and_roots(label, content.stored_roots());
        return Ok(());
    };
    // Projected exact inputs may hold message bodies: the label keeps every
    // parent's policy and is bounded to their model and expiry.
    let label = crate::model_message_labels::composed_envelope(
        &content,
        parent,
        MESSAGE_ID,
        crate::model_message_labels::ServerResultSource::ExactAuthorizationProjection,
        true,
    )?;
    match label {
        Some(mut label) => {
            label.sensitivity = label
                .sensitivity
                .max(desk_agent_protocol::data_lineage::Sensitivity::Sensitive);
            label.retention = label.retention.most_restrictive(
                desk_agent_protocol::data_lineage::RetentionBoundary {
                    expires_at_unix_ms: Some(bound.expires_at_unix_ms),
                    delete_with_run: true,
                },
            );
            if !label.allowed_destinations.iter().any(|destination| {
                crate::model_egress::same_recipient(destination, &bound.destination)
            }) {
                return Err(projection_denied());
            }
            label.allowed_destinations = vec![bound.destination.clone()];
            label.validate().map_err(|_| projection_denied())?;
            runtime.set_label_and_roots(Some(label), content.stored_roots());
        }
        // Without any parent the projection keeps its own unregistered label.
        None => {
            *runtime = crate::permission_resume::bind_exact_authorization_system_message(
                std::mem::replace(runtime, ChatMessage::system_event(MESSAGE_ID, "")),
                bound.destination,
                bound.expires_at_unix_ms,
            )?;
        }
    }
    Ok(())
}

fn projection_denied() -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
        message: "Current authorization cannot be bound to the selected model".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
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
    fn static_instructions_ignore_locale_and_authorization() {
        let mut first = crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
            Some("zh-CN"),
            &crate::model_content::ModelContent::server_text("grant A"),
        );
        let mut second = crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
            Some("en-US"),
            &crate::model_content::ModelContent::server_text("grant B"),
        );
        assert_eq!(first.text, second.text);
        assert_ne!(super::take(&mut first).text, super::take(&mut second).text);
        assert!(first.runtime_context.is_none());
        assert!(!serde_json::to_string(&second).unwrap().contains("grant B"));
    }
    #[test]
    fn an_exact_projection_is_labelled_once_from_its_content_within_its_bounds() {
        use crate::ai_assistant::source_registry::{
            NativeInputPolicyRegistration, RegisteredSourceContract, SOURCE_CONTRACT_REVISION,
        };
        use crate::{prompt::ResponseFormatSpec, seam::ModelRequest};
        use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        // The host records the bounds without labelling anything.
        let mut system = crate::permission_resume::restrict_exact_projection(
            crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
                None,
                &crate::model_content::ModelContent::server_text("exact private input"),
            ),
            destination.clone(),
            100_000,
        )
        .unwrap();
        assert!(system.data_envelope.is_none());
        let mut runtime = super::take(&mut system);
        assert!(runtime.data_envelope.is_none());
        // Later appends join before the single label.
        super::append(
            &mut runtime,
            &crate::model_content::ModelContent::constant("\nCurrent time"),
        );
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Send the reply".into(),
            destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let mut labelled = runtime.clone();
        super::label(&mut labelled, Some(&owner), Some(&destination)).unwrap();
        let label = labelled.data_envelope.clone().unwrap();
        assert_eq!(
            label.provenance.source_tool_name,
            crate::model_message_labels::ServerResultSource::ExactAuthorizationProjection.as_str()
        );
        assert_eq!(label.retention.expires_at_unix_ms, Some(100_000));
        assert!(label.retention.delete_with_run);
        assert!(label.sensitivity >= Sensitivity::Sensitive);
        assert_eq!(label.allowed_destinations, vec![destination.clone()]);
        // It registers under its parent instead of making the request unknown.
        let inputs = vec![owner.clone(), label];
        let audit = crate::sink_authorizer::SinkProjectionAudit {
            destination: destination.clone(),
            envelope_ids: inputs
                .iter()
                .map(|input| input.envelope_id.clone())
                .collect(),
            total_bytes: 32,
        };
        let snapshot =
            crate::model_egress::NativeModelInputSnapshot::capture(&audit, &inputs).unwrap();
        let registration =
            NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
        assert_eq!(
            registration.leaves[1].as_ref().map(|leaf| leaf.contract),
            Some(RegisteredSourceContract::NativeControl)
        );
        let policy = crate::model_egress::ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "test".into(),
            now_unix_ms: 1,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        };
        policy
            .authorize_request(ModelRequest::text_only(
                vec![system.clone(), labelled],
                ResponseFormatSpec::None,
            ))
            .unwrap();
        // A parent from before a connection change of the gateway family is
        // bound to the current model; another family is denied.
        let mut old = owner.clone();
        if let Some(DestinationIdentity::Model {
            connection_revision,
            ..
        }) = old.allowed_destinations.first_mut()
        {
            *connection_revision += 1;
        }
        let mut followed = runtime.clone();
        super::label(&mut followed, Some(&old), Some(&destination)).unwrap();
        assert_eq!(
            followed.data_envelope.unwrap().allowed_destinations,
            vec![destination.clone()]
        );
        let mut other_family = owner.clone();
        if let Some(DestinationIdentity::Model { connection_id, .. }) =
            other_family.allowed_destinations.first_mut()
        {
            *connection_id = "approval:1".into();
        }
        let mut denied = runtime.clone();
        assert!(super::label(&mut denied, Some(&other_family), Some(&destination)).is_err());
        // Without any parent the projection keeps its own unregistered label.
        let mut orphan = runtime;
        super::label(&mut orphan, None, Some(&destination)).unwrap();
        let label = orphan.data_envelope.as_ref().unwrap();
        assert_eq!(
            label.provenance.source_tool_name,
            crate::model_message_labels::sources::CAPABILITY_AUTHORIZATION
        );
        assert_eq!(label.retention.expires_at_unix_ms, Some(100_000));
    }

    /// The runtime context's label follows everything appended to it: cited
    /// stored text joins as roots bound to the model destination.
    #[test]
    fn the_label_cites_appended_stored_text() {
        use crate::ai_assistant::source_registry::{
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
            let mut system = crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
                Some("zh-CN"),
                &grants,
            );
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
