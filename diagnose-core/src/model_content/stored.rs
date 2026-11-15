//! Stored text a model sees again, with the source proof computed when it
//! was written.
//!
//! A record field such as a schedule title or a goal text is read back into
//! later requests. Its root label carries no parents; the proof embedded next
//! to it says where the text came from: the owner, or a model output whose
//! retention was decided when it was accepted. The proof travels with every
//! message that repeats the text, so later edits of the record never change
//! how an earlier message is judged. Device-reported text, such as a device
//! name, uses its own root whose policy is fixed by its contract.

use super::{__private, ContentPiece, Sources};
use crate::ai_assistant::{
    seam::AssistantReadRoot,
    source::{owner_text_policy, validate_policy},
    source_registry::NativeDerivedInputPolicy,
};
use desk_agent_protocol::{
    AgentError,
    ai_assistant::source::SourceUsePolicy,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const STORED_TEXT_PROVIDER: &str = "assistant-stored-text";
pub const DEVICE_REPORTED_PROVIDER: &str = "assistant-device-reported";
const STORED_TEXT_PREFIX: &str = "stored-text-";
const DEVICE_REPORTED_PREFIX: &str = "device-reported-";
const ROOT_PROJECTION: &str = "stored-root";
/// Device-reported text is short metadata, never a device output channel.
pub const MAX_DEVICE_REPORTED_BYTES: usize = 512;

/// Where stored text came from, decided when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredOrigin {
    /// The owner wrote or edited it.
    Owner,
    /// A model output wrote it; its retention was decided on acceptance.
    ModelOutput {
        model_source: Box<NativeDerivedInputPolicy>,
        #[serde(default)]
        retained_roots: Vec<AssistantReadRoot>,
    },
}

/// The source proof of one stored text field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredTextSource {
    pub actor_id: String,
    pub origin: StoredOrigin,
    pub policy: SourceUsePolicy,
}

impl StoredTextSource {
    pub fn owner(actor_id: &str, policy_revision: u64) -> Self {
        Self {
            actor_id: actor_id.into(),
            origin: StoredOrigin::Owner,
            policy: owner_text_policy(policy_revision, false),
        }
    }

    /// The proof of text a retained model output wrote. An output that was
    /// not retained has no policy and yields no proof.
    pub fn model_output(
        actor_id: &str,
        model_source: NativeDerivedInputPolicy,
        retained_roots: Vec<AssistantReadRoot>,
    ) -> Option<Self> {
        let policy = model_source.text_policy.clone()?;
        let value = Self {
            actor_id: actor_id.into(),
            origin: StoredOrigin::ModelOutput {
                model_source: Box::new(model_source),
                retained_roots,
            },
            policy,
        };
        value.validate(actor_id).ok()?;
        Some(value)
    }

    /// The proof must belong to `actor` and match its origin.
    pub fn validate(&self, actor: &str) -> Result<(), &'static str> {
        validate_policy(&self.policy)?;
        if self.actor_id != actor {
            return Err("stored text proof belongs to another actor");
        }
        match &self.origin {
            StoredOrigin::Owner => {
                if self.policy != owner_text_policy(self.policy.policy_revision, false) {
                    return Err("owner text proof changed its policy");
                }
            }
            StoredOrigin::ModelOutput { model_source, .. } => {
                model_source.validate()?;
                if model_source.actor_id != actor
                    || model_source.text_policy.as_ref() != Some(&self.policy)
                {
                    return Err("model output proof changed its policy");
                }
            }
        }
        Ok(())
    }

    /// The policy a later answer inherits, or `None` when the text may not be
    /// copied into it, including after its hard deadline when `now` is known.
    pub fn usable_policy(&self, actor: &str, now: Option<u64>) -> Option<&SourceUsePolicy> {
        self.validate(actor).ok()?;
        let policy = &self.policy;
        (!policy.no_copy
            && policy.derived_visible_text_allowed
            && !now.is_some_and(|now| {
                policy
                    .content_hard_expires_at_unix_ms
                    .is_some_and(|until| until <= now)
            }))
        .then_some(policy)
    }

    pub fn retained_roots(&self) -> &[AssistantReadRoot] {
        match &self.origin {
            StoredOrigin::Owner => &[],
            StoredOrigin::ModelOutput { retained_roots, .. } => retained_roots,
        }
    }
}

/// The root label of one stored text field, with its proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRoot {
    pub label: DataEnvelope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<StoredTextSource>,
}

impl StoredRoot {
    /// The root of `field` of record `record_id` of `kind`, holding `text`.
    /// Its id addresses the text, proof and destinations, so two revisions or
    /// destinations of one field never share an id. A missing proof keeps the
    /// root unusable for retention.
    pub fn text(
        kind: &'static str,
        record_id: &str,
        field: &'static str,
        text: &str,
        proof: Option<StoredTextSource>,
        destinations: Vec<DestinationIdentity>,
    ) -> Result<Self, AgentError> {
        let identity = serde_json::to_vec(&(
            kind,
            record_id,
            field,
            format!("{:x}", Sha256::digest(text.as_bytes())),
            &proof,
            &destinations,
        ))
        .map_err(|_| crate::model_message_labels::invalid_label())?;
        let expires = proof
            .as_ref()
            .and_then(|proof| proof.policy.content_hard_expires_at_unix_ms);
        let label = root_label(
            format!("{STORED_TEXT_PREFIX}{:x}", Sha256::digest(identity)),
            STORED_TEXT_PROVIDER,
            kind,
            text,
            Sensitivity::UserContent,
            destinations,
            expires,
        )?;
        Ok(Self { label, proof })
    }

    /// The root of a device-reported field, such as a device name or a path
    /// the device resolved. Its policy is fixed by the device-reported
    /// contract, so it needs no proof.
    pub fn device_reported(
        kind: &'static str,
        record_id: &str,
        field: &'static str,
        text: &str,
        destinations: Vec<DestinationIdentity>,
    ) -> Result<Self, AgentError> {
        if text.len() > MAX_DEVICE_REPORTED_BYTES {
            return Err(crate::model_message_labels::invalid_label());
        }
        let identity = serde_json::to_vec(&(
            kind,
            record_id,
            field,
            format!("{:x}", Sha256::digest(text.as_bytes())),
            &destinations,
        ))
        .map_err(|_| crate::model_message_labels::invalid_label())?;
        let label = root_label(
            format!("{DEVICE_REPORTED_PREFIX}{:x}", Sha256::digest(identity)),
            DEVICE_REPORTED_PROVIDER,
            kind,
            text,
            Sensitivity::UserContent,
            destinations,
            None,
        )?;
        Ok(Self { label, proof: None })
    }
}

fn root_label(
    envelope_id: String,
    provider: &str,
    kind: &str,
    text: &str,
    sensitivity: Sensitivity,
    allowed_destinations: Vec<DestinationIdentity>,
    expires_at_unix_ms: Option<u64>,
) -> Result<DataEnvelope, AgentError> {
    let label = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id,
        content: ContentRef::Projection {
            projection: ROOT_PROJECTION.into(),
            size_bytes: text.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: provider.into(),
            source_tool_name: kind.into(),
            source_object_id: None,
            source_envelope_ids: Vec::new(),
        },
        sensitivity,
        allowed_destinations,
        retention: RetentionBoundary {
            expires_at_unix_ms,
            delete_with_run: false,
        },
    };
    label
        .validate()
        .map_err(|_| crate::model_message_labels::invalid_label())?;
    Ok(label)
}

/// Whether `label` is a stored-text root of the shape [`StoredRoot::text`]
/// builds.
pub fn is_stored_text_label(label: &DataEnvelope) -> bool {
    root_shape(label, STORED_TEXT_PROVIDER, STORED_TEXT_PREFIX)
}

/// Whether `label` is a device-reported root of the shape
/// [`StoredRoot::device_reported`] builds.
pub fn is_device_reported_label(label: &DataEnvelope) -> bool {
    root_shape(label, DEVICE_REPORTED_PROVIDER, DEVICE_REPORTED_PREFIX)
        && label.content.size_bytes() <= MAX_DEVICE_REPORTED_BYTES as u64
}

fn root_shape(label: &DataEnvelope, provider: &str, prefix: &str) -> bool {
    label.provenance.source_provider_id == provider
        && label.provenance.source_envelope_ids.is_empty()
        && label.provenance.source_object_id.is_none()
        && label
            .envelope_id
            .strip_prefix(prefix)
            .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && matches!(&label.content, ContentRef::Projection { projection, media_type, .. }
            if projection == ROOT_PROJECTION && media_type == "text/plain")
}

/// Stored text repeated in model-visible content. Its root becomes a parent
/// of the content's label and travels with the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored<T> {
    value: T,
    root: StoredRoot,
}

impl<T> Stored<T> {
    pub fn new(value: T, root: StoredRoot) -> Self {
        Self { value, root }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn root(&self) -> &StoredRoot {
        &self.root
    }
}

impl<T: Serialize> Serialize for Stored<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<T: Serialize> __private::Sealed for Stored<T> {}
impl<T: Serialize> ContentPiece for Stored<T> {
    fn collect(&self, sources: &mut Sources) {
        sources
            .stored
            .entry(self.root.label.envelope_id.clone())
            .or_insert_with(|| self.root.clone());
    }
}
