//! Shared provenance labels for user messages and selected Provider read results.

use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    provider_registry::ProviderRegistry,
    seam::ToolRunOutput,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub mod sources;
pub use sources::ServerResultSource;

pub const SOURCE_RESULT_PROJECTION: &str = "source-result";

pub fn model_bound_user_message(
    message_id: String,
    text: String,
    destination: DestinationIdentity,
) -> Result<ChatMessage, AgentError> {
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("user-message-{message_id}"),
        content: ContentRef::Message {
            message_id: message_id.clone(),
            size_bytes: text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "ai-assistant-user".into(),
            source_tool_name: crate::model_message_labels::sources::USER_MESSAGE.into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
        sensitivity: Sensitivity::UserContent,
        // Pressing Send explicitly targets the currently resolved model. A
        // different gateway/model revision produces a different identity and
        // cannot reuse this allowance.
        allowed_destinations: vec![destination],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        },
    };
    envelope.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to label AI Assistant input: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let mut message = ChatMessage::text(message_id, ChatRole::User, text);
    message.data_envelope = Some(envelope);
    Ok(message)
}

pub struct ReadResultLabel {
    pub envelope_id: String,
    pub observation_id: String,
    pub source_object_id: Option<String>,
    pub observed_at_unix_ms: u64,
}

pub fn read_result_envelope(
    registry: &ProviderRegistry,
    call: &ToolCall,
    output: &ToolRunOutput,
    label: ReadResultLabel,
) -> Result<DataEnvelope, AgentError> {
    let capability = registry
        .capability_for_tool(&call.name)
        .ok_or_else(invalid_label)?;
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .ok_or_else(invalid_label)?;
    let payload = crate::model_egress::message_payload_bytes(
        &output.content,
        output.image_data_url.as_deref(),
    )
    .map_err(|_| invalid_label())?;
    if payload.is_empty() || label.observed_at_unix_ms == 0 {
        return Err(invalid_label());
    }
    let digest_sha256 = format!("{:x}", Sha256::digest(&payload));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: label.envelope_id,
        content: ContentRef::ImmutableBlob {
            blob_id: format!("native-read-snapshot-{}", label.observation_id),
            sha256: digest_sha256.clone(),
            size_bytes: payload.len() as u64,
            media_type: "application/octet-stream".into(),
        },
        provenance: DataProvenance {
            source_provider_id: provider.wire.provider_id.clone(),
            source_tool_name: call.name.clone(),
            source_object_id: label.source_object_id,
            source_envelope_ids: Vec::new(),
        },
        sensitivity: match capability.wire.capability_id.as_str() {
            crate::ai_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID
            | crate::ai_assistant::WEB_RESEARCH_SEARCH_CAPABILITY_ID => Sensitivity::Public,
            crate::ai_assistant::DESKTOP_SESSION_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_INFO_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_NETWORK_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_SERVICE_CAPABILITY_ID
            | crate::ai_assistant::SYSTEM_CONTAINER_CAPABILITY_ID => Sensitivity::UserContent,
            _ => Sensitivity::Sensitive,
        },
        // Reading never implicitly grants export to any model or other sink.
        allowed_destinations: Vec::new(),
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(envelope)
}

pub(crate) fn invalid_label() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "Provider result cannot be labeled safely".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

/// Label a decision receipt, or its request projection, whose body carries
/// an AI reviewer's reason. The first parent is the proposing model output;
/// the others are every evidence the reviewer saw, so the reason keeps all of
/// their constraints and registers only with every parent in the same input.
pub fn server_review_envelope(
    parents: &[&DataEnvelope],
    call_id: &str,
    content: &str,
    source: ServerResultSource,
) -> Result<DataEnvelope, AgentError> {
    let Some((proposing, evidence)) = parents.split_first() else {
        return Err(invalid_label());
    };
    if !source.reviewed() {
        return Err(invalid_label());
    }
    let content = evidence
        .iter()
        .fold(
            crate::model_content::ModelContent::builder()
                .server(&crate::model_content::ServerText::generated(content)),
            |builder, label| {
                builder.sourced(&crate::model_content::Sourced::new(
                    String::new(),
                    (*label).clone(),
                ))
            },
        )
        .build();
    composed_envelope(&content, Some(proposing), call_id, source, false)?.ok_or_else(invalid_label)
}

pub(crate) const COMPOSED_RESULT_PREFIX: &str = "composed-result-";
pub(crate) const COMPOSED_CONTENT_PREFIX: &str = "composed-content-";
pub(crate) const COMPOSED_RESULT_PROJECTION: &str = "composed-result";
pub(crate) const UNVERIFIED_RESULT_PREFIX: &str = "unverified-result-";
const UNVERIFIED_CONTENT_PREFIX: &str = "unverified-content-";
const UNVERIFIED_RESULT_PROJECTION: &str = "unverified-result";

/// Label model-visible `content` from its pieces' sources.
///
/// The parents are the issuing model output (when given) followed by every
/// in-request label the content repeats, in id order; the issuing output stays
/// first so readers can find it. Sensitivity, retention and destinations are
/// the strictest of all parents. Content holding unverified text gets a shape
/// the registry never accepts; any other content gets the composed shape,
/// which registers only with every parent present in the same input.
///
/// A persisted result (`request_only == false`) binds the source, call, parents
/// and body digest in its id; a request-only projection is content-addressed.
/// Without any parent the content stays unlabelled, as in unlabelled sessions.
/// Every argument piece must come from `call_id` issued by `issuing`.
pub fn composed_envelope(
    content: &crate::model_content::ModelContent,
    issuing: Option<&DataEnvelope>,
    call_id: &str,
    source: ServerResultSource,
    request_only: bool,
) -> Result<Option<DataEnvelope>, AgentError> {
    compose_named(content, issuing, call_id, source.as_str(), request_only)
}

/// Label `content` under a tool's own name, as results that history export
/// and argument checks find by the tool that produced them. Server-built
/// content registers only under a registered source name; unverified content
/// never does.
pub fn named_envelope(
    content: &crate::model_content::ModelContent,
    issuing: Option<&DataEnvelope>,
    call_id: &str,
    name: &str,
    request_only: bool,
) -> Result<Option<DataEnvelope>, AgentError> {
    compose_named(content, issuing, call_id, name, request_only)
}

/// Which destinations a composed label allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destinations {
    /// Those every parent allows.
    Shared,
    /// Those the issuing parent allows; see [`reference_projection_envelope`].
    Issuing,
}

fn compose_named(
    content: &crate::model_content::ModelContent,
    issuing: Option<&DataEnvelope>,
    call_id: &str,
    name: &str,
    request_only: bool,
) -> Result<Option<DataEnvelope>, AgentError> {
    compose_with(
        content,
        issuing,
        call_id,
        name,
        request_only,
        Destinations::Shared,
    )
}

/// Label a request-only projection that repeats references from earlier
/// results, issued under `issuing`. Those results are bound to the model only
/// when they are exported, so the label keeps the issuing parent's
/// destinations; every parent's sensitivity and retention still apply. The
/// registry admits it only when each parent is in the request and allows the
/// destination; otherwise it stays unregistered.
pub(crate) fn reference_projection_envelope(
    content: &crate::model_content::ModelContent,
    issuing: &DataEnvelope,
    call_id: &str,
    source: ServerResultSource,
) -> Result<Option<DataEnvelope>, AgentError> {
    compose_with(
        content,
        Some(issuing),
        call_id,
        source.as_str(),
        true,
        Destinations::Issuing,
    )
}

fn compose_with(
    content: &crate::model_content::ModelContent,
    issuing: Option<&DataEnvelope>,
    call_id: &str,
    name: &str,
    request_only: bool,
    destinations: Destinations,
) -> Result<Option<DataEnvelope>, AgentError> {
    let sources = content.sources();
    // Every root must be bound to a destination before labelling.
    if !sources.unbound.is_empty() {
        return Err(invalid_label());
    }
    let issuing_id = issuing.map(|label| label.envelope_id.as_str());
    if sources
        .args
        .iter()
        .any(|(call, output)| call != call_id || output.as_deref() != issuing_id)
    {
        return Err(invalid_label());
    }
    let mut parents = Vec::with_capacity(sources.parents.len() + 1);
    if let Some(issuing) = issuing {
        parents.push(issuing);
    }
    parents.extend(
        sources
            .parents
            .values()
            .chain(sources.stored.values().map(|root| &root.label))
            .filter(|label| Some(label.envelope_id.as_str()) != issuing_id),
    );
    if parents.is_empty() {
        return Ok(None);
    }
    for parent in &parents {
        parent.validate().map_err(|_| invalid_label())?;
    }
    let MergedInputs {
        sensitivity,
        retention,
        allowed,
        ..
    } = merge_inputs(&parents)?;
    let allowed = match (destinations, issuing) {
        (Destinations::Issuing, Some(issuing)) => issuing
            .allowed_destinations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        _ => allowed,
    };
    let source_envelope_ids = parents
        .iter()
        .map(|parent| parent.envelope_id.clone())
        .collect::<Vec<_>>();
    let text = content.text();
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    let (envelope_prefix, blob_prefix, projection) = if content.is_unverified() {
        (
            UNVERIFIED_RESULT_PREFIX,
            UNVERIFIED_CONTENT_PREFIX,
            UNVERIFIED_RESULT_PROJECTION,
        )
    } else {
        (
            COMPOSED_RESULT_PREFIX,
            COMPOSED_CONTENT_PREFIX,
            COMPOSED_RESULT_PROJECTION,
        )
    };
    let identity =
        serde_json::to_vec(&(name, call_id, &source_envelope_ids, &digest, request_only))
            .map_err(|_| invalid_label())?;
    let content_ref = if request_only {
        ContentRef::ImmutableBlob {
            blob_id: format!("{blob_prefix}{:x}", Sha256::digest(digest.as_bytes())),
            sha256: digest,
            size_bytes: text.len() as u64,
            media_type: "text/plain".into(),
        }
    } else {
        ContentRef::Projection {
            projection: projection.into(),
            size_bytes: text.len() as u64,
            media_type: "text/plain".into(),
        }
    };
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("{envelope_prefix}{:x}", Sha256::digest(identity)),
        content: content_ref,
        provenance: DataProvenance {
            source_provider_id: crate::dynamic_run::RUN_CONTROL_PROVIDER_ID.into(),
            source_tool_name: name.into(),
            source_object_id: None,
            source_envelope_ids,
        },
        sensitivity,
        allowed_destinations: allowed.into_iter().collect(),
        retention,
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(Some(envelope))
}

/// Replace the body of the result at `index` with a server-authored `text`.
/// The new label derives from the nearest earlier assistant message that made
/// call `call_id`: call ids can repeat across turns, and the replaced body's
/// own label leaves the model input with it. An unlabelled call leaves the
/// result unlabelled; a labelled result without its call is an error. The id
/// binds the parent, call, source and body, so two results of one call never
/// share a source and body. On error the message is left unchanged.
pub(crate) fn rewrite_result(
    messages: &mut [ChatMessage],
    index: usize,
    call_id: &str,
    content: &crate::model_content::ModelContent,
    source: ServerResultSource,
) -> Result<(), AgentError> {
    let issuing = messages
        .get(..index)
        .ok_or_else(invalid_label)?
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|call| call.id == call_id)
        });
    let parent = match issuing {
        Some(message) => message.data_envelope.clone(),
        None if messages[index].data_envelope.is_none() => None,
        None => return Err(invalid_label()),
    };
    let message = &mut messages[index];
    *message = message.clone().with_content(
        content,
        crate::model_content::Labelling {
            issuing: parent.as_ref(),
            call_id,
            source,
            request_only: false,
        },
    )?;
    Ok(())
}

/// A history page is a deterministic projection of several older message
/// envelopes plus the model's current tool call. Its authority is the strict
/// intersection of every input destination, its sensitivity is the maximum,
/// and operation-reference deadlines remain in its lineage metadata. A model
/// switch still requires matching disclosure authority; elapsed time does not
/// invalidate the historical page for model recollection.
pub fn conversation_history_result_envelope(
    parent: Option<&DataEnvelope>,
    source_messages: &[ChatMessage],
    call_id: &str,
    content: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    let mut inputs = Vec::new();
    if let Some(parent) = parent {
        inputs.push(parent);
    }
    inputs.extend(
        source_messages
            .iter()
            .filter_map(|message| message.data_envelope.as_ref()),
    );
    source_result_envelope(
        &inputs,
        call_id,
        content,
        crate::dynamic_run::RUN_CONTROL_PROVIDER_ID,
        crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME,
        None,
    )
}

pub fn assistant_data_result_envelope(
    parent: Option<&DataEnvelope>,
    sources: &[DataEnvelope],
    call_id: &str,
    content: &str,
    tool_name: &str,
    episode_id: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    if let [snapshot] = sources
        && snapshot.provenance.source_provider_id == "assistant-conversation-snapshot"
    {
        return Ok(Some(snapshot.clone()));
    }
    let mut inputs = Vec::with_capacity(sources.len() + 1);
    if let Some(parent) = parent {
        inputs.push(parent);
    }
    inputs.extend(sources);
    source_result_envelope(
        &inputs,
        call_id,
        content,
        "assistant-data-result",
        tool_name,
        Some(episode_id),
    )
}

fn source_result_envelope(
    inputs: &[&DataEnvelope],
    call_id: &str,
    content: &str,
    provider: &str,
    tool_name: &str,
    object: Option<&str>,
) -> Result<Option<DataEnvelope>, AgentError> {
    source_result_identity(
        inputs,
        call_id,
        content.len() as u64,
        provider,
        tool_name,
        object,
    )
}

/// Reconstruct an original tool-result label from immutable projection metadata,
/// without loading or retaining the old result body. This grants no source use.
pub(crate) fn retained_assistant_data_result(
    parent: &DataEnvelope,
    source: &DataEnvelope,
    call_id: &str,
    tool_name: &str,
    episode_id: &str,
) -> Result<Option<DataEnvelope>, AgentError> {
    source.validate().map_err(|_| invalid_label())?;
    source_result_identity(
        &[parent, source],
        call_id,
        source.content.size_bytes(),
        "assistant-data-result",
        tool_name,
        Some(episode_id),
    )
}

/// The combined constraints of several inputs: their sorted distinct ids, the
/// highest sensitivity, the most restrictive retention (counting ephemeral
/// observation deadlines) and the destinations every input allows.
struct MergedInputs<'a> {
    by_id: std::collections::BTreeMap<&'a str, &'a DataEnvelope>,
    source_envelope_ids: Vec<String>,
    sensitivity: Sensitivity,
    retention: RetentionBoundary,
    allowed: BTreeSet<DestinationIdentity>,
}

fn merge_inputs<'a>(inputs: &[&'a DataEnvelope]) -> Result<MergedInputs<'a>, AgentError> {
    let mut by_id = std::collections::BTreeMap::new();
    for input in inputs {
        if let Some(previous) = by_id.insert(input.envelope_id.as_str(), *input)
            && previous != *input
        {
            return Err(invalid_label());
        }
    }
    let mut source_envelope_ids = inputs
        .iter()
        .map(|input| input.envelope_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if source_envelope_ids.len() > desk_agent_protocol::data_lineage::MAX_LINEAGE_ITEMS {
        return Err(invalid_label());
    }
    source_envelope_ids.sort();
    let sensitivity = inputs
        .iter()
        .map(|input| input.sensitivity)
        .max()
        .unwrap_or(Sensitivity::Sensitive);
    let effective_retention = |input: &DataEnvelope| {
        if let ContentRef::EphemeralObservation {
            expires_at_unix_ms, ..
        } = input.content
        {
            input.retention.most_restrictive(RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: true,
            })
        } else {
            input.retention
        }
    };
    let retention = inputs
        .iter()
        .skip(1)
        .fold(effective_retention(inputs[0]), |current, input| {
            current.most_restrictive(effective_retention(input))
        });
    let mut allowed = inputs[0]
        .allowed_destinations
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for input in inputs.iter().skip(1) {
        let destinations = input
            .allowed_destinations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        allowed = allowed.intersection(&destinations).cloned().collect();
    }
    Ok(MergedInputs {
        by_id,
        source_envelope_ids,
        sensitivity,
        retention,
        allowed,
    })
}

fn source_result_identity(
    inputs: &[&DataEnvelope],
    call_id: &str,
    size_bytes: u64,
    provider: &str,
    tool_name: &str,
    object: Option<&str>,
) -> Result<Option<DataEnvelope>, AgentError> {
    if inputs.is_empty() {
        return Ok(None);
    }
    for input in inputs {
        input.validate().map_err(|_| invalid_label())?;
    }
    let MergedInputs {
        by_id,
        source_envelope_ids,
        sensitivity,
        retention,
        allowed,
    } = merge_inputs(inputs)?;
    let identity = serde_json::to_vec(&(
        provider,
        tool_name,
        object,
        call_id,
        by_id.values().collect::<Vec<_>>(),
    ))
    .map_err(|_| invalid_label())?;
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("source-result-{:x}", Sha256::digest(identity)),
        content: ContentRef::Projection {
            projection: SOURCE_RESULT_PROJECTION.into(),
            size_bytes,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: provider.into(),
            source_tool_name: tool_name.into(),
            source_object_id: object.map(str::to_owned),
            source_envelope_ids,
        },
        sensitivity,
        allowed_destinations: allowed.into_iter().collect(),
        retention,
    };
    envelope.validate().map_err(|_| invalid_label())?;
    Ok(Some(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> DataEnvelope {
        model_bound_user_message(
            id.into(),
            "source text".into(),
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope
        .unwrap()
    }

    #[test]
    fn status_and_request_labels_name_one_body_per_id() {
        let parent = source("owner");
        // Host recovery can relabel one call's status under the same parent and tool.
        let running = crate::model_message_labels::named_envelope(
            &crate::model_content::ModelContent::builder()
                .unverified(&crate::model_content::Unverified::new("running"))
                .build(),
            Some(&parent),
            "call",
            "status",
            false,
        )
        .unwrap()
        .unwrap();
        let unknown = crate::model_message_labels::named_envelope(
            &crate::model_content::ModelContent::builder()
                .unverified(&crate::model_content::Unverified::new("unknown"))
                .build(),
            Some(&parent),
            "call",
            "status",
            false,
        )
        .unwrap()
        .unwrap();
        assert_ne!(running.envelope_id, unknown.envelope_id);
        assert!(matches!(running.content, ContentRef::Projection { .. }));
        assert_eq!(
            running,
            crate::model_message_labels::named_envelope(
                &crate::model_content::ModelContent::builder()
                    .unverified(&crate::model_content::Unverified::new("running"))
                    .build(),
                Some(&parent),
                "call",
                "status",
                false
            )
            .unwrap()
            .unwrap()
        );
        // Request projections reuse one call id while their body changes per request.
        let projection = |body: &str| {
            crate::model_message_labels::composed_envelope(
                &crate::model_content::ModelContent::server_text(body),
                Some(&parent),
                "marker",
                ServerResultSource::RuntimeContext,
                true,
            )
            .unwrap()
            .unwrap()
        };
        let (first, second) = (projection("one"), projection("two"));
        assert_ne!(first.envelope_id, second.envelope_id);
        assert!(first.content_sha256().is_some());
    }

    #[test]
    fn server_and_data_labels_have_distinct_shapes_whatever_the_name() {
        let parent = source("owner");
        let name = ServerResultSource::ManageMemories;
        let server = crate::model_message_labels::composed_envelope(
            &crate::model_content::ModelContent::server_text("ok"),
            Some(&parent),
            "call",
            name,
            false,
        )
        .unwrap()
        .unwrap();
        let data = crate::model_message_labels::named_envelope(
            &crate::model_content::ModelContent::builder()
                .unverified(&crate::model_content::Unverified::new("ok"))
                .build(),
            Some(&parent),
            "call",
            name.as_str(),
            false,
        )
        .unwrap()
        .unwrap();
        assert!(server.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
        assert!(data.envelope_id.starts_with("unverified-result-"));
        assert_eq!(server.provenance, data.provenance);
        let ContentRef::ImmutableBlob { blob_id, .. } =
            crate::model_message_labels::named_envelope(
                &crate::model_content::ModelContent::builder()
                    .unverified(&crate::model_content::Unverified::new("ok"))
                    .build(),
                Some(&parent),
                "call",
                name.as_str(),
                true,
            )
            .unwrap()
            .unwrap()
            .content
        else {
            panic!("request projections are content addressed");
        };
        assert!(blob_id.starts_with("unverified-content-"));
        // A receipt keeps the producing tool's name; only a registered source
        // name lets the registry accept server text under it.
        let receipt = |tool: &str| {
            crate::model_message_labels::named_envelope(
                &crate::model_content::ModelContent::server_text("ok"),
                Some(&parent),
                "call",
                tool,
                false,
            )
            .unwrap()
            .unwrap()
            .envelope_id
        };
        assert_eq!(receipt(name.as_str()), server.envelope_id);
        assert_ne!(receipt("search_memories"), server.envelope_id);
        assert!(ServerResultSource::parse("search_memories").is_none());
    }

    #[test]
    fn rewrites_derive_from_the_nearest_call_and_name_one_body_per_id() {
        let call = |id: &str, label: &str| {
            let mut message = ChatMessage::assistant_tool_calls(
                "assistant",
                "",
                vec![crate::chat::ToolCallRef {
                    id: id.into(),
                    name: "exec_command".into(),
                    arguments_json: "{}".into(),
                }],
            );
            message.data_envelope = Some(source(label));
            message
        };
        let mut result = ChatMessage::tool_result("result", "call", "original");
        result.data_envelope = Some(source("provider"));
        // The call id repeats across turns; the nearest earlier call is the parent.
        let mut messages = vec![
            call("call", "old"),
            call("call", "issuing"),
            result,
            call("call", "later"),
        ];
        let issuing = messages[1].data_envelope.clone().unwrap().envelope_id;
        let mut seen = std::collections::BTreeMap::new();
        for (text, source) in [
            ("unknown", ServerResultSource::ExecutionOutcomeUnknown),
            ("disposed", ServerResultSource::ManualOutcomeDisposition),
            ("unknown", ServerResultSource::ExecutionOutcomeUnknown),
        ] {
            rewrite_result(
                &mut messages,
                2,
                "call",
                &crate::model_content::ModelContent::constant(text),
                source,
            )
            .unwrap();
            let label = messages[2].data_envelope.clone().unwrap();
            assert_eq!(messages[2].text, text);
            assert_eq!(
                label.provenance.source_envelope_ids,
                std::slice::from_ref(&issuing)
            );
            assert!(label.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
            // One id never names two bodies.
            let body = seen.entry(label.envelope_id).or_insert(text);
            assert_eq!(*body, text);
        }
        assert_eq!(seen.len(), 2);
        // Without its issuing call the message is left unchanged.
        let mut orphan = vec![messages[2].clone()];
        let before = orphan.clone();
        assert!(
            rewrite_result(
                &mut orphan,
                0,
                "call",
                &crate::model_content::ModelContent::constant("other"),
                ServerResultSource::ExecutionOutcomeUnknown
            )
            .is_err()
        );
        assert_eq!(orphan, before);
    }

    #[test]
    fn source_result_identity_binds_purpose_episode_and_complete_parent_proofs() {
        let a = source("a");
        let b = source("b");
        let result = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        let reordered = assistant_data_result_envelope(
            None,
            &[b.clone(), a.clone(), a.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(result, reordered);
        let other_tool = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_conversations",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        let other_episode = assistant_data_result_envelope(
            None,
            &[a.clone(), b.clone()],
            "call",
            "{}",
            "search_memories",
            "episode-2",
        )
        .unwrap()
        .unwrap();
        assert_ne!(result.envelope_id, other_tool.envelope_id);
        assert_ne!(result.envelope_id, other_episode.envelope_id);
        let mut restricted = a.clone();
        restricted.allowed_destinations.clear();
        let changed = assistant_data_result_envelope(
            None,
            &[restricted.clone(), b],
            "call",
            "{}",
            "search_memories",
            "episode-1",
        )
        .unwrap()
        .unwrap();
        assert_ne!(result.envelope_id, changed.envelope_id);
        assert!(changed.allowed_destinations.is_empty());
        assert!(
            assistant_data_result_envelope(
                None,
                &[a, restricted],
                "call",
                "{}",
                "search_memories",
                "episode-1"
            )
            .is_err()
        );
    }

    #[test]
    fn original_assistant_result_identity_survives_body_cleanup_and_binds_all_original_metadata() {
        let parent = source("producer");
        let read = source("projection");
        let original = assistant_data_result_envelope(
            Some(&parent),
            std::slice::from_ref(&read),
            "read-call",
            "source text",
            "search_memories",
            "original-episode",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            retained_assistant_data_result(
                &parent,
                &read,
                "read-call",
                "search_memories",
                "original-episode"
            )
            .unwrap(),
            Some(original.clone())
        );
        for case in 0..7 {
            let mut producer = parent.clone();
            let mut projection = read.clone();
            let mut call = "read-call";
            let mut tool = "search_memories";
            let mut episode = "original-episode";
            match case {
                0 => producer.allowed_destinations.clear(),
                1 => producer.retention.expires_at_unix_ms = Some(5000),
                2 => projection.provenance.source_object_id = Some("other-object".into()),
                3 => call = "other-call",
                4 => tool = "search_conversations",
                5 => episode = "other-episode",
                _ => {
                    if let ContentRef::ImmutableBlob { size_bytes, .. }
                    | ContentRef::Projection { size_bytes, .. }
                    | ContentRef::Message { size_bytes, .. } = &mut projection.content
                    {
                        *size_bytes += 1;
                    }
                }
            }
            assert_ne!(
                retained_assistant_data_result(&producer, &projection, call, tool, episode)
                    .unwrap(),
                Some(original.clone()),
                "case {case}"
            );
        }
    }

    #[test]
    fn history_and_memory_results_preserve_ephemeral_content_deadlines() {
        let mut input = source("ephemeral");
        input.content = ContentRef::EphemeralObservation {
            observation_id: "obs".into(),
            size_bytes: 11,
            expires_at_unix_ms: 100,
        };
        input.retention = RetentionBoundary {
            expires_at_unix_ms: Some(500),
            delete_with_run: false,
        };
        let result = assistant_data_result_envelope(
            None,
            std::slice::from_ref(&input),
            "call",
            "{}",
            "search_memories",
            "episode",
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.retention.expires_at_unix_ms, Some(100));
        assert!(result.retention.delete_with_run);
        let mut message = ChatMessage::text("message", ChatRole::User, "source text");
        message.data_envelope = Some(input);
        let history = conversation_history_result_envelope(None, &[message], "call", "{}")
            .unwrap()
            .unwrap();
        assert_eq!(history.retention, result.retention);
        assert_ne!(history.envelope_id, result.envelope_id);
    }

    #[test]
    fn internal_result_never_renews_ephemeral_parent_or_mints_missing_authority() {
        assert!(
            crate::model_message_labels::composed_envelope(
                &crate::model_content::ModelContent::server_text("unavailable"),
                None,
                "call",
                ServerResultSource::SupersedeToolCall,
                false
            )
            .unwrap()
            .is_none()
        );
        let mut parent = model_bound_user_message(
            "source".into(),
            "input".into(),
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
        parent.content = ContentRef::EphemeralObservation {
            observation_id: "observation".into(),
            size_bytes: 5,
            expires_at_unix_ms: 100,
        };
        let result = crate::model_message_labels::composed_envelope(
            &crate::model_content::ModelContent::server_text("unavailable"),
            Some(&parent),
            "call",
            ServerResultSource::SupersedeToolCall,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.retention.expires_at_unix_ms, Some(100));
        assert!(result.retention.delete_with_run);
        assert_eq!(result.allowed_destinations, parent.allowed_destinations);
        assert_eq!(
            result.provenance.source_envelope_ids,
            vec![parent.envelope_id.clone()]
        );
        parent.envelope_id.clear();
        assert!(
            crate::model_message_labels::composed_envelope(
                &crate::model_content::ModelContent::server_text("unavailable"),
                Some(&parent),
                "call",
                ServerResultSource::SupersedeToolCall,
                false
            )
            .is_err()
        );
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments_json: "{}".into(),
        }
    }
    fn label() -> ReadResultLabel {
        ReadResultLabel {
            envelope_id: "result-1".into(),
            observation_id: "observation-1".into(),
            source_object_id: Some("device-1:call-1".into()),
            observed_at_unix_ms: 100,
        }
    }

    #[test]
    fn read_label_is_bounded_and_never_grants_export() {
        let output = ToolRunOutput {
            format: crate::seam::ToolOutputFormat::Text,
            content: "desktop metadata".into(),
            image_data_url: None,
            document_preview: None,
        };
        let envelope = read_result_envelope(
            &crate::ai_assistant::ai_assistant_provider_registry(),
            &call("inspect_desktop_session"),
            &output,
            label(),
        )
        .unwrap();
        assert!(envelope.allowed_destinations.is_empty());
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.retention.expires_at_unix_ms, None);
        assert!(envelope.retention.delete_with_run);
        assert_eq!(
            envelope.content_sha256(),
            Some(format!("{:x}", Sha256::digest(output.content.as_bytes())).as_str())
        );
    }

    #[test]
    fn unknown_tool_and_empty_result_do_not_get_labels() {
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        assert!(
            read_result_envelope(
                &registry,
                &call("invented-tool"),
                &ToolRunOutput {
                    format: crate::seam::ToolOutputFormat::Text,
                    content: "data".into(),
                    image_data_url: None,
                    document_preview: None,
                },
                label()
            )
            .is_err()
        );
        assert!(
            read_result_envelope(
                &registry,
                &call("inspect_desktop_session"),
                &ToolRunOutput::default(),
                label()
            )
            .is_err()
        );
    }

    #[test]
    fn user_label_targets_only_the_resolved_model() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 2,
        };
        let user = model_bound_user_message(
            "user-1".into(),
            "private question".into(),
            destination.clone(),
        )
        .unwrap();
        let envelope = user.data_envelope.unwrap();
        assert_eq!(envelope.allowed_destinations, vec![destination]);
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.provenance.source_tool_name, "send-message");
    }

    #[test]
    fn history_result_intersects_destinations_and_keeps_restrictive_retention() {
        fn destination(model_id: &str) -> DestinationIdentity {
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: model_id.into(),
                profile_revision: 1,
            }
        }

        let model_a = destination("model-a");
        let model_b = destination("model-b");
        let mut first =
            model_bound_user_message("first".into(), "first question".into(), model_a.clone())
                .unwrap();
        first.data_envelope.as_mut().unwrap().allowed_destinations = vec![model_a.clone(), model_b];
        first.data_envelope.as_mut().unwrap().retention = RetentionBoundary {
            expires_at_unix_ms: Some(500),
            delete_with_run: false,
        };
        let mut second =
            model_bound_user_message("second".into(), "second question".into(), model_a.clone())
                .unwrap();
        second.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Sensitive;
        second.data_envelope.as_mut().unwrap().retention = RetentionBoundary {
            expires_at_unix_ms: Some(300),
            delete_with_run: true,
        };

        let envelope = conversation_history_result_envelope(
            None,
            &[first, second],
            "history-call",
            r#"{"messages":[]}"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(envelope.allowed_destinations, vec![model_a]);
        assert_eq!(envelope.sensitivity, Sensitivity::Sensitive);
        assert_eq!(envelope.retention.expires_at_unix_ms, Some(300));
        assert!(envelope.retention.delete_with_run);
        assert_eq!(envelope.provenance.source_envelope_ids.len(), 2);
    }
}
