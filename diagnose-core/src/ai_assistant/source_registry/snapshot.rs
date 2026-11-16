//! Immutable leaf registrations from the exact audited native input snapshot.
//! Missing native parent/reference proofs stay unresolved, never permissive.

use super::*;
use crate::model_egress::{NativeModelInputSnapshot, sink_metadata_sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputLeafPolicy {
    pub contract: RegisteredSourceContract,
    /// Index into `NativeInputPolicyRegistration::policies`.
    pub policy_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputPolicyRegistration {
    pub source_contract_revision: u64,
    pub policy_revision: u64,
    /// Ordered one-to-one with the complete original snapshot. Equal policies
    /// share one entry; actual inputs are never deduplicated.
    pub leaves: Vec<Option<NativeInputLeafPolicy>>,
    /// Distinct policies in first-use order, so a leaf keeps its index across
    /// later calls that only append inputs.
    pub policies: Vec<SourceUsePolicy>,
    /// Ordered one-to-one with the snapshot's stored roots. Stored-text
    /// leaves hold the strictest compiled policy; their embedded proofs apply
    /// only when policies are merged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stored_leaves: Vec<Option<NativeInputLeafPolicy>>,
}

impl NativeInputPolicyRegistration {
    pub fn policy(&self, leaf: &NativeInputLeafPolicy) -> Option<&SourceUsePolicy> {
        self.policies.get(usize::try_from(leaf.policy_index).ok()?)
    }

    pub fn capture(
        snapshot: &NativeModelInputSnapshot,
        source_contract_revision: u64,
        policy_revision: u64,
    ) -> Result<Self, &'static str> {
        if source_contract_revision == 0 || policy_revision == 0 {
            return Err("missing original input policy revision");
        }
        let mut policies: Vec<SourceUsePolicy> = Vec::new();
        let mut leaves = Vec::with_capacity(snapshot.envelopes.len());
        let mut stored_leaves = Vec::with_capacity(snapshot.stored_roots.len());
        for (index, registration) in
            registered_inputs(snapshot, source_contract_revision, policy_revision)?
                .into_iter()
                .enumerate()
        {
            let leaves = if index < snapshot.envelopes.len() {
                &mut leaves
            } else {
                &mut stored_leaves
            };
            leaves.push(match registration {
                Some(entry) => {
                    let index = match policies.iter().position(|policy| *policy == entry.policy) {
                        Some(index) => index,
                        None => {
                            policies.push(entry.policy.clone());
                            policies.len() - 1
                        }
                    };
                    Some(NativeInputLeafPolicy {
                        contract: entry.contract,
                        policy_index: u32::try_from(index)
                            .map_err(|_| "too many native input policies")?,
                    })
                }
                None => None,
            });
        }
        let value = Self {
            source_contract_revision,
            policy_revision,
            leaves,
            policies,
            stored_leaves,
        };
        value.validate_metadata(snapshot, source_contract_revision, policy_revision)?;
        Ok(value)
    }

    /// Structural historical evidence only. This intentionally does not apply
    /// current code to an unavailable original contract revision.
    pub fn validate_metadata(
        &self,
        snapshot: &NativeModelInputSnapshot,
        original_contract_revision: u64,
        original_policy_revision: u64,
    ) -> Result<(), &'static str> {
        if self.source_contract_revision == 0
            || self.policy_revision == 0
            || self.source_contract_revision != original_contract_revision
            || self.policy_revision != original_policy_revision
            || self.leaves.len() != snapshot.envelopes.len()
            || self.stored_leaves.len() != snapshot.stored_roots.len()
        {
            return Err("native policy registration changed its original snapshot");
        }
        // Leaves reference policies in first-use order, which makes the
        // encoding unique: no gaps, no duplicates and no unused entries.
        let mut next = 0u32;
        for entry in self.leaves.iter().chain(&self.stored_leaves).flatten() {
            if entry.policy_index > next {
                return Err("native leaf policy dictionary is not in first-use order");
            }
            if entry.policy_index == next {
                next += 1;
            }
        }
        if usize::try_from(next).ok() != Some(self.policies.len()) {
            return Err("native policy dictionary contains unused policy claims");
        }
        for (index, policy) in self.policies.iter().enumerate() {
            validate_policy(policy)?;
            if policy.policy_revision != self.policy_revision
                || self.policies[..index].contains(policy)
            {
                return Err("native leaf policy dictionary changed");
            }
        }
        sink_metadata_sha256(self)?;
        Ok(())
    }

    pub fn validate_registered_leaves(
        &self,
        snapshot: &NativeModelInputSnapshot,
    ) -> Result<(), &'static str> {
        self.validate_metadata(
            snapshot,
            self.source_contract_revision,
            self.policy_revision,
        )?;
        if self.source_contract_revision != SOURCE_CONTRACT_REVISION {
            return Err("original source contract revision is unavailable");
        }
        for (entry, expected) in
            self.leaves
                .iter()
                .chain(&self.stored_leaves)
                .zip(registered_inputs(
                    snapshot,
                    self.source_contract_revision,
                    self.policy_revision,
                )?)
        {
            match (entry, expected) {
                (None, None) => {}
                // A missing historical control registration grants nothing.
                // Its original label and actual parents remain mandatory.
                (None, Some(expected))
                    if expected.contract == RegisteredSourceContract::NativeControl => {}
                (Some(reference), Some(expected))
                    if reference.contract == expected.contract
                        && self.policy(reference) == Some(&expected.policy) => {}
                _ => return Err("native leaf registration changed its compiled source policy"),
            }
        }
        Ok(())
    }

    /// The policy stored root `index` of `snapshot` gives an answer derived
    /// from it: a device-reported or skill root's compiled policy, or a stored-text
    /// root's embedded proof when it belongs to `actor` and may be copied at
    /// `now`. `None` leaves the answer unretained.
    pub fn stored_root_policy<'a>(
        &'a self,
        snapshot: &'a NativeModelInputSnapshot,
        index: usize,
        actor: &str,
        now: Option<u64>,
    ) -> Option<&'a SourceUsePolicy> {
        let leaf = self.stored_leaves.get(index)?.as_ref()?;
        match leaf.contract {
            // An AI-authored skill's compiled policy stands here; hosts
            // restore its version's stored proof instead.
            RegisteredSourceContract::DeviceReported
            | RegisteredSourceContract::PlatformSkill
            | RegisteredSourceContract::PersonalSkill
            | RegisteredSourceContract::AuthoredSkill => self.policy(leaf),
            RegisteredSourceContract::StoredText => snapshot
                .stored_roots
                .get(index)?
                .proof
                .as_ref()?
                .usable_policy(actor, now),
            _ => None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    /// This is a historical policy result, never a live read/send grant. A
    /// single unresolved actual input prevents claiming a complete derivation.
    /// Stored roots use their embedded proofs under each proof's own actor.
    pub fn complete_leaf_text_policy(
        &self,
        snapshot: &NativeModelInputSnapshot,
    ) -> Result<Option<SourceUsePolicy>, &'static str> {
        self.validate_registered_leaves(snapshot)?;
        if self.leaves.iter().any(Option::is_none) {
            return Ok(None);
        }
        let mut policies = self
            .leaves
            .iter()
            .flatten()
            .filter_map(|leaf| self.policy(leaf))
            .cloned()
            .collect::<Vec<_>>();
        for (index, root) in snapshot.stored_roots.iter().enumerate() {
            let actor = root
                .proof
                .as_ref()
                .map(|proof| proof.actor_id.as_str())
                .unwrap_or_default();
            match self.stored_root_policy(snapshot, index, actor, None) {
                Some(policy) => policies.push(policy.clone()),
                None => return Ok(None),
            }
        }
        super::super::source::derive_visible_text_policy(&policies, self.policy_revision).map(Some)
    }
}

/// Controls and derived reads are not independent roots. Every parent is an actual
/// input in this immutable snapshot, or a stored-text root carried with one,
/// where its own native/read policy remains mandatory. No ancestor labels,
/// bodies or permissions are invented here. Entries follow the snapshot's
/// inputs, then its stored roots.
fn registered_inputs(
    snapshot: &NativeModelInputSnapshot,
    source_contract_revision: u64,
    policy_revision: u64,
) -> Result<Vec<Option<RegisteredSourcePolicy>>, &'static str> {
    let mut sources = snapshot.policy_sources();
    let inputs = sources.len();
    sources.extend(snapshot.stored_roots.iter().map(|root| &root.label));
    if source_contract_revision != SOURCE_CONTRACT_REVISION {
        return Ok(vec![None; sources.len()]);
    }
    let mut entries = sources
        .iter()
        .map(|source| registered_leaf(source, policy_revision, source_contract_revision))
        .collect::<Result<Vec<_>, _>>()?;
    let contracts = sources
        .iter()
        .map(|source| {
            if native_control(source) {
                Some(RegisteredSourceContract::NativeControl)
            } else {
                native_read(source)
            }
        })
        .collect::<Vec<_>>();
    let mut labels = BTreeMap::new();
    let projected = snapshot
        .envelopes
        .iter()
        .map(Some)
        .chain(std::iter::repeat_n(None, sources.len() - inputs));
    for (index, (original, projected)) in sources.iter().zip(projected).enumerate() {
        for label in std::iter::once(*original).chain(projected) {
            if labels
                .insert(label.envelope_id.as_str(), (index, label))
                .is_some_and(|(_, old)| old != label)
            {
                return Err("native source parent has conflicting full labels");
            }
        }
    }
    // Iterative closure keeps cycles unresolved without recursive stack use.
    // A non-control parent still needs its registered leaf, native archive or
    // original read proof in the existing complete retention inspection.
    loop {
        let mut progress = false;
        for (index, source) in sources.iter().enumerate() {
            let Some(contract) = contracts[index].filter(|_| entries[index].is_none()) else {
                continue;
            };
            let parents = source
                .provenance
                .source_envelope_ids
                .iter()
                .map(|id| labels.get(id.as_str()).copied())
                .collect::<Option<Vec<_>>>();
            let Some(parents) = parents else {
                continue;
            };
            if parents.iter().any(|(parent_index, parent)| {
                if contracts[*parent_index].is_some() && entries[*parent_index].is_none() {
                    return true;
                }
                if contract != RegisteredSourceContract::NativeControl {
                    return false;
                }
                // A parent exported in this request reaches the model under its
                // exported label; that label's destinations are the ones it has.
                let delivered = snapshot.envelopes.get(*parent_index).unwrap_or(parent);
                source.sensitivity < parent.sensitivity
                    || source.retention.most_restrictive(parent.retention) != source.retention
                    || source.allowed_destinations.iter().any(|destination| !delivered.allowed_destinations.contains(destination))
                    || matches!(parent.content, ContentRef::EphemeralObservation { expires_at_unix_ms, .. }
                        if !source.retention.delete_with_run || source.retention.expires_at_unix_ms.is_none_or(|until| until > expires_at_unix_ms))
            }) { continue; }
            entries[index] = Some(registration(
                source,
                contract,
                policy_revision,
                source_contract_revision,
            )?);
            progress = true;
        }
        if !progress {
            return Ok(entries);
        }
    }
}

/// Native reads produce fresh content. Their actual parents remain separate
/// inputs whose copy restrictions and hard deadlines constrain the answer.
fn native_read(source: &DataEnvelope) -> Option<RegisteredSourceContract> {
    if source.provenance.source_envelope_ids.is_empty()
        || source
            .provenance
            .source_object_id
            .as_deref()
            .is_some_and(|id| id.starts_with("live-target:sha256:"))
        || !matches!(&source.content, ContentRef::ImmutableBlob { blob_id, sha256, size_bytes, media_type }
            if blob_id.strip_prefix("native-read-snapshot-").is_some_and(|id| !id.is_empty())
                && sha256.len() == 64 && *size_bytes > 0 && media_type == "application/octet-stream")
    {
        return None;
    }
    let contract = classify(source);
    matches!(
        contract,
        RegisteredSourceContract::DeviceObservation
            | RegisteredSourceContract::UiObservation
            | RegisteredSourceContract::MediaObservation
            | RegisteredSourceContract::PublicResearchResult
            | RegisteredSourceContract::ProviderPrivate
            | RegisteredSourceContract::Secret
    )
    .then_some(contract)
}

/// A registered composed label with a single parent: server text governed
/// only by that parent's policy.
pub(crate) fn is_registered_status_control(source: &DataEnvelope) -> bool {
    let blob = match &source.content {
        ContentRef::ImmutableBlob {
            blob_id,
            sha256,
            media_type,
            ..
        } => Some((blob_id, sha256, media_type)),
        _ => None,
    };
    source.provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
        && source.provenance.source_object_id.is_none()
        && crate::model_message_labels::ServerResultSource::parse(
            &source.provenance.source_tool_name,
        )
        .is_some()
        && source.provenance.source_envelope_ids.len() == 1
        && composed_result_shape(source, blob)
}

/// The registered shape of a composed label: a persisted projection, or a
/// request-only content-addressed blob, derived from one or more parents that
/// must all be inputs of the same request. Unverified content uses another
/// shape and never matches.
fn composed_result_shape(source: &DataEnvelope, blob: Option<(&String, &String, &String)>) -> bool {
    use crate::model_message_labels::{
        COMPOSED_CONTENT_PREFIX, COMPOSED_RESULT_PREFIX, COMPOSED_RESULT_PROJECTION,
    };
    use sha2::{Digest, Sha256};
    source
        .envelope_id
        .strip_prefix(COMPOSED_RESULT_PREFIX)
        .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && !source.provenance.source_envelope_ids.is_empty()
        && match (&source.content, blob) {
            (
                ContentRef::Projection {
                    projection,
                    media_type,
                    ..
                },
                _,
            ) => projection == COMPOSED_RESULT_PROJECTION && media_type == "text/plain",
            (_, Some((blob_id, sha256, media_type))) => {
                media_type == "text/plain"
                    && *blob_id
                        == format!(
                            "{COMPOSED_CONTENT_PREFIX}{:x}",
                            Sha256::digest(sha256.as_bytes())
                        )
            }
            _ => false,
        }
}

/// Match the compiled producer's complete label shape, not a model-selected
/// source name. Parent policies above prevent control text laundering data.
#[inline(never)]
fn native_control(source: &DataEnvelope) -> bool {
    if source.provenance.source_envelope_ids.is_empty() {
        return false;
    }
    if source.content.size_bytes() == 0 {
        return false;
    }
    // Persisted status results are projections; request-only status
    // projections and runtime control messages are content-addressed blobs.
    let blob = match &source.content {
        ContentRef::ImmutableBlob {
            blob_id,
            sha256,
            media_type,
            ..
        } => Some((blob_id, sha256, media_type)),
        _ => None,
    };
    let provenance = &source.provenance;
    if provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID {
        if provenance.source_object_id.is_some() {
            return false;
        }
        match provenance.source_tool_name.as_str() {
            name if crate::model_message_labels::ServerResultSource::parse(name).is_some()
                && composed_result_shape(source, blob) =>
            {
                true
            }
            crate::model_message_labels::sources::DELEGATION_STATE => {
                source
                    .envelope_id
                    .strip_prefix("delegation-")
                    .is_some_and(|id| !id.is_empty())
                    && matches!(&source.content, ContentRef::Projection { projection, media_type, .. }
                        if projection == crate::subagent::projection::DELEGATION_PROJECTION
                            && media_type == "application/json")
            }
            _ => false,
        }
    } else if provenance.source_provider_id == "assistant-runtime-control" {
        let Some(id) = provenance.source_object_id.as_deref() else {
            return false;
        };
        let prefix = match provenance.source_tool_name.as_str() {
            crate::model_message_labels::sources::SCHEDULED_CONTINUATION => "scheduled-resume",
            crate::model_message_labels::sources::PERMISSION_DECISION_RESUME => "permission-resume",
            crate::model_message_labels::sources::PROPOSAL_DECISION_RESUME => "decision-resume",
            _ => return false,
        };
        source.envelope_id == format!("{prefix}-message-{id}")
            && provenance.source_envelope_ids.len() == 1
            && matches!(&source.content, ContentRef::Message { message_id, media_type, .. }
                if message_id == id && media_type == "text/plain;charset=utf-8")
    } else {
        false
    }
}
