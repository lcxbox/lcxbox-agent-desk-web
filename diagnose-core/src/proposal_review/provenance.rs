//! Provenance of an AI reviewer's reason in a decision receipt.
//!
//! The reason is model text derived from everything the reviewer saw: the
//! proposal, which derives from the proposing model output, and the evidence
//! messages. When all of that is conversation content still labelled in the
//! session, the receipt may carry the reason under every one of those labels.
//! Otherwise the reason is unverifiable and the assistant never sees it.

use crate::{
    approval_review::{ApprovalEvidence, ApprovalReviewCandidate, ApprovalSource},
    chat::{ChatMessage, ChatRole},
    session::PersistedAgentSession,
};
use desk_agent_protocol::data_lineage::{DataEnvelope, MAX_LINEAGE_ITEMS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What an AI reviewer saw, frozen from the settled candidate. It is stored
/// with the decision so the reason can be traced when it is shown later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewProvenance {
    /// False when the reviewer saw text from outside the conversation whose
    /// source the candidate did not record.
    pub verifiable: bool,
    /// The evidence messages the reviewer read, by event id and the digest
    /// of their text. A message rewritten after the review no longer
    /// vouches for what the reviewer read.
    pub evidence: Vec<ReviewedEvidence>,
    /// Stored text the reviewer read outside the conversation, traced to the
    /// source recorded when it was written.
    pub stored: Vec<ReviewedStoredText>,
}

/// One evidence message an AI reviewer read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedEvidence {
    pub event_id: String,
    pub text_sha256: String,
}

impl ReviewedEvidence {
    pub fn of(evidence: &ApprovalEvidence) -> Self {
        Self {
            event_id: evidence.event_id.clone(),
            text_sha256: text_digest(&evidence.text),
        }
    }
}

fn text_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Stored text an AI reviewer read, whose root joins the reason's parents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewedStoredText {
    /// The goal text, with the proof recorded when it was written.
    Goal {
        goal_id: String,
        text: String,
        source: crate::model_content::StoredTextSource,
    },
    /// A directory path the device resolved for a directory request.
    Directory { request_id: String, path: String },
    /// Text derived from a stored personal skill version.
    Skill {
        base: crate::approval_review::ReviewSkillBase,
        text: String,
    },
}

/// The purpose naming a skill version's root in a reviewed reason.
const REVIEWED_SKILL_PURPOSE: &str = "approval-review";

impl ReviewedStoredText {
    /// The root bound to `destinations`, or `None` when it cannot be built.
    fn root(
        &self,
        destinations: &[desk_agent_protocol::data_lineage::DestinationIdentity],
    ) -> Option<crate::model_content::StoredRoot> {
        use crate::model_content::StoredSpec;
        match self {
            Self::Goal {
                goal_id,
                text,
                source,
            } => StoredSpec::proven(
                crate::goal_projection::KIND,
                goal_id,
                crate::goal_projection::GOAL_TEXT_FIELD,
                text,
                Some(source.clone()),
            )
            .root(destinations.to_vec())
            .ok(),
            Self::Directory { request_id, path } => {
                StoredSpec::device_reported("directory", request_id, "path", path)
                    .root(destinations.to_vec())
                    .ok()
            }
            Self::Skill { base, text } => {
                let [destination] = destinations else {
                    return None;
                };
                crate::skills::SkillRoot::reviewed_base(base)
                    .stored_root(REVIEWED_SKILL_PURPOSE, text, destination)
                    .ok()
            }
        }
    }
}

impl ReviewProvenance {
    pub fn from_candidate(candidate: &ApprovalReviewCandidate) -> Self {
        let action =
            serde_json::from_str::<serde_json::Value>(&candidate.action_json).unwrap_or_default();
        let mut stored = Vec::new();
        let mut verifiable = true;
        match &candidate.source {
            // An update shows text merged from its base version, and a
            // removal shows that version itself.
            ApprovalSource::SkillProposal { .. } => {
                let proposal = &action["skill_proposal"];
                if !proposal["base_version"].is_null()
                    || matches!(proposal["kind"].as_str(), Some("disable" | "delete"))
                {
                    match &candidate.context.skill_base {
                        Some(base) => stored.push(ReviewedStoredText::Skill {
                            base: base.clone(),
                            text: proposal["skill_md"].as_str().unwrap_or_default().into(),
                        }),
                        None => verifiable = false,
                    }
                }
            }
            // A path other than the requested one was resolved by the device.
            ApprovalSource::DirectoryProposal { request_id } => {
                let proposal = &action["directory_proposal"];
                if proposal["canonical_path"] != proposal["requested_path"] {
                    match proposal["canonical_path"].as_str() {
                        Some(path) => stored.push(ReviewedStoredText::Directory {
                            request_id: request_id.clone(),
                            path: path.into(),
                        }),
                        None => verifiable = false,
                    }
                }
            }
            _ => {}
        }
        if let Some(text) = &candidate.context.goal_text {
            match (&candidate.goal_id, &candidate.context.goal_text_source) {
                (Some(goal_id), Some(source)) => stored.push(ReviewedStoredText::Goal {
                    goal_id: goal_id.clone(),
                    text: text.clone(),
                    source: source.clone(),
                }),
                _ => verifiable = false,
            }
        }
        Self {
            verifiable,
            evidence: candidate
                .context
                .evidence
                .iter()
                .map(ReviewedEvidence::of)
                .collect(),
            stored,
        }
    }
}

/// Every parent of a reviewer's reason: the labels of the proposing output and
/// the evidence, and the roots of the stored text the reviewer read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonParents {
    pub labels: Vec<DataEnvelope>,
    pub roots: Vec<crate::model_content::StoredRoot>,
}

impl ReasonParents {
    /// `text` built from all of these parents.
    pub fn content(&self, text: &str) -> crate::model_content::ModelContent {
        use crate::model_content::{ModelContent, ServerText, Sourced, Stored};
        let builder = ModelContent::builder().server(&ServerText::generated(text));
        let builder = self.labels.iter().fold(builder, |builder, label| {
            builder.sourced(&Sourced::new(String::new(), label.clone()))
        });
        self.roots
            .iter()
            .fold(builder, |builder, root| {
                builder.stored(&Stored::new(String::new(), root.clone()))
            })
            .build()
    }
}

/// The label of the model output that made the paused proposal call.
pub fn proposing_output(session: &PersistedAgentSession) -> Option<&DataEnvelope> {
    let pause = session.decision_pause.as_ref()?;
    session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(pause.turn_id.as_str())
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.id == pause.call_id)
        })
        .and_then(|message| message.data_envelope.as_ref())
}

/// The label of the model output that made permission request `request_id`,
/// located through the proof recorded with the request's input; `None` when
/// the request has no such proof or the output is no longer in the session.
pub fn requesting_output<'a>(
    session: &'a PersistedAgentSession,
    request_id: &str,
) -> Option<&'a DataEnvelope> {
    let request = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == request_id)?;
    let crate::model_content::StoredOrigin::ModelOutput { model_source, .. } =
        &request.input_source.as_deref()?.origin
    else {
        return None;
    };
    session
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.as_ref())
        .find(|label| label.envelope_id == model_source.output_envelope_id)
}

/// Replaces a registered single-parent status label by its parent, repeatedly.
/// Such a label is server text whose policy is its parent's, and a projected
/// pending result leaves the model input, so its parent stands in for it.
pub(crate) fn collapse_status_label(
    session: &PersistedAgentSession,
    label: &DataEnvelope,
) -> Option<DataEnvelope> {
    let mut current = label.clone();
    for _ in 0..=session.conversation.len() {
        if !crate::ai_assistant::source_registry::is_registered_status_control(&current) {
            return Some(current);
        }
        let parent = current.provenance.source_envelope_ids.first()?;
        current = find_label(session, parent)?.clone();
    }
    None
}

fn find_label<'a>(session: &'a PersistedAgentSession, id: &str) -> Option<&'a DataEnvelope> {
    session
        .authorization_requirement()
        .into_iter()
        .chain(session.conversation.iter())
        .filter_map(|message: &ChatMessage| message.data_envelope.as_ref())
        .find(|label| label.envelope_id == id)
}

/// Every parent of the reviewer's reason, or `None` when the reason cannot be
/// verified: outside content, missing or rewritten evidence, too many parents,
/// or parents sharing no destination of the proposing output.
pub fn review_reason_parents(
    session: &PersistedAgentSession,
    proposing: &DataEnvelope,
    provenance: &ReviewProvenance,
) -> Option<ReasonParents> {
    if !provenance.verifiable {
        return None;
    }
    let mut parents = BTreeMap::new();
    parents.insert(proposing.envelope_id.clone(), proposing.clone());
    for evidence in &provenance.evidence {
        let message = crate::approval_review::review_evidence_message(session, &evidence.event_id)?;
        if text_digest(&message.text) != evidence.text_sha256 {
            return None;
        }
        let label = collapse_status_label(session, message.data_envelope.as_ref()?)?;
        parents.insert(label.envelope_id.clone(), label);
    }
    // Stored text the reviewer read is bound to the proposing output's model.
    let roots = provenance
        .stored
        .iter()
        .map(|text| text.root(&proposing.allowed_destinations))
        .collect::<Option<Vec<_>>>()?;
    if parents.len() + roots.len() > MAX_LINEAGE_ITEMS {
        return None;
    }
    let mut shared: BTreeSet<_> = proposing.allowed_destinations.iter().collect();
    for parent in parents.values() {
        let allowed: BTreeSet<_> = parent.allowed_destinations.iter().collect();
        shared = shared.intersection(&allowed).copied().collect();
    }
    if shared.is_empty() || shared.len() != proposing.allowed_destinations.len() {
        return None;
    }
    // The proposing output comes first, as the receipt's issuing parent.
    let mut labels = vec![proposing.clone()];
    labels.extend(
        parents
            .into_values()
            .filter(|parent| parent.envelope_id != proposing.envelope_id),
    );
    Some(ReasonParents { labels, roots })
}

/// An AI reviewer's verdict reason as a decision receipt needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiDecisionReason {
    pub reason: String,
    pub reason_code: String,
    pub provenance: ReviewProvenance,
}

impl AiDecisionReason {
    pub fn from_review(
        review: &crate::approval_review::ApprovalReviewDecision,
        candidate: &ApprovalReviewCandidate,
    ) -> Self {
        Self {
            reason: review.reason.clone(),
            reason_code: review.reason_code.clone(),
            provenance: ReviewProvenance::from_candidate(candidate),
        }
    }
}

/// What the executing model is told when the AI reviewer denied one exact
/// action that `proposing` called for. The reviewer's own reason is shown
/// when every parent of its provenance is still labelled in the session, and
/// the content then derives from all of them; otherwise the closed reason
/// code and its fixed guidance stand in for it.
pub fn action_denial_content(
    session: &PersistedAgentSession,
    proposing: Option<&DataEnvelope>,
    ai: &AiDecisionReason,
) -> crate::model_content::ModelContent {
    let code = crate::approval_review::ApprovalReasonCode::parse(&ai.reason_code);
    let parents =
        proposing.and_then(|proposing| review_reason_parents(session, proposing, &ai.provenance));
    let Some(parents) = parents else {
        return crate::approval_review::action_denial_content(code);
    };
    let (name, guidance) = crate::approval_review::reason_code_guidance(code);
    let reason = parents.content(&ai.reason);
    crate::model_content::ModelContent::builder()
        .text("not executed: independent AI review denied this exact action. reason_code=")
        .text(name)
        .text(". ")
        .text(guidance)
        .text(" Reviewer reason: ")
        .content(&reason)
        .build()
}

/// A decision receipt's label and the stored roots its reason cites, which
/// the receipt message carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptLabel {
    pub envelope: Option<DataEnvelope>,
    pub stored_roots: Vec<crate::model_content::StoredRoot>,
}

impl ReceiptLabel {
    /// Labels `message` with this label and its roots.
    pub fn apply(self, message: &mut ChatMessage) {
        message.set_label_and_roots(self.envelope, self.stored_roots);
    }
}

/// The label of a decision receipt. An AI reason keeps every parent of its
/// provenance, or takes a data label that never registers when it cannot be
/// verified; owner and system decisions carry only fixed text under the
/// proposing output. `proposing` is the output that made this proposal's
/// call, when the caller located it; `fallback` is the parent used otherwise.
pub fn decision_receipt_envelope(
    session: &PersistedAgentSession,
    proposing: Option<&DataEnvelope>,
    ai: Option<&AiDecisionReason>,
    fallback: Option<&DataEnvelope>,
    call_id: &str,
    content: &str,
    source: crate::model_message_labels::ServerResultSource,
) -> Result<ReceiptLabel, desk_agent_protocol::AgentError> {
    use crate::model_content::{ModelContent, Unverified};
    use crate::model_message_labels::composed_envelope;
    let only = |envelope: Option<DataEnvelope>| ReceiptLabel {
        envelope,
        stored_roots: Vec::new(),
    };
    // A reason no parent can vouch for is repeated unverified.
    let unverified = |parent: Option<&DataEnvelope>| {
        composed_envelope(
            &ModelContent::builder()
                .unverified(&Unverified::new(content))
                .build(),
            parent,
            call_id,
            source,
            false,
        )
        .map(only)
    };
    let parent = proposing
        .cloned()
        .or_else(|| fallback.and_then(|label| collapse_status_label(session, label)));
    match ai {
        Some(ai) => {
            let parents = proposing
                .and_then(|proposing| review_reason_parents(session, proposing, &ai.provenance));
            match parents {
                Some(parents) if source.reviewed() => {
                    let reason = parents.content(content);
                    match composed_envelope(&reason, parents.labels.first(), call_id, source, false)
                    {
                        Ok(envelope) => Ok(ReceiptLabel {
                            envelope,
                            stored_roots: reason.stored_roots(),
                        }),
                        Err(_) => unverified(parent.as_ref()),
                    }
                }
                _ => unverified(parent.as_ref()),
            }
        }
        // Without an AI reason the receipt holds the server's decision only.
        None => composed_envelope(
            &ModelContent::server_text(content),
            parent.as_ref(),
            call_id,
            source,
            false,
        )
        .map(only),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        approval_review::ApprovalEvidenceTrust,
        model_message_labels::{ServerResultSource, model_bound_user_message},
        proposal_review::{DecisionKind, DecisionPause},
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::DestinationIdentity};

    pub(crate) fn destination(model: &str) -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: model.into(),
            profile_revision: 1,
        }
    }

    fn label(id: &str, model: &str) -> DataEnvelope {
        model_bound_user_message(id.into(), id.into(), destination(model))
            .unwrap()
            .data_envelope
            .unwrap()
    }

    /// owner message, proposing call, its pending result, a device output.
    pub(crate) fn paused_session() -> PersistedAgentSession {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "actor",
            "device",
            1,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-14T00:00:00Z",
        );
        let mut owner = ChatMessage::text("owner", ChatRole::User, "owner");
        owner.data_envelope = Some(label("owner", "m"));
        let mut call = ChatMessage::assistant_tool_calls(
            "assistant",
            "",
            vec![crate::chat::ToolCallRef {
                id: "call".into(),
                name: "propose_skill".into(),
                arguments_json: "{}".into(),
            }],
        );
        call.turn_id = Some("turn".into());
        call.data_envelope = Some(label("proposing", "m"));
        let mut pending = ChatMessage::tool_result("pending", "call", "pending");
        pending.data_envelope = crate::model_message_labels::composed_envelope(
            &crate::model_content::ModelContent::server_text("pending"),
            call.data_envelope.as_ref(),
            "call",
            ServerResultSource::SkillProposal,
            false,
        )
        .unwrap();
        let mut device = ChatMessage::tool_result("device", "read", "device output");
        device.data_envelope = crate::model_message_labels::named_envelope(
            &crate::model_content::ModelContent::builder()
                .unverified(&crate::model_content::Unverified::new("device output"))
                .build(),
            owner.data_envelope.as_ref(),
            "read",
            "read",
            false,
        )
        .unwrap();
        session.conversation = vec![owner, call, pending, device];
        session.decision_pause = Some(DecisionPause {
            kind: DecisionKind::Skill,
            id: "p1".into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            input_revision: session.input_revision,
            requested_at_unix_ms: 1,
        });
        session
    }

    pub(crate) fn provenance(session: &PersistedAgentSession, ids: &[&str]) -> ReviewProvenance {
        ReviewProvenance {
            verifiable: true,
            evidence: ids
                .iter()
                .map(|id| {
                    let message = session
                        .conversation
                        .iter()
                        .find(|message| message.message_id == *id)
                        .unwrap();
                    ReviewedEvidence::of(&ApprovalEvidence {
                        event_id: (*id).into(),
                        trust: ApprovalEvidenceTrust::UntrustedContent,
                        text: message.text.clone(),
                    })
                })
                .collect(),
            stored: Vec::new(),
        }
    }

    fn ids(parents: &[DataEnvelope]) -> BTreeSet<String> {
        parents
            .iter()
            .map(|label| label.envelope_id.clone())
            .collect()
    }

    #[test]
    fn parents_are_the_proposing_output_and_evidence_with_status_labels_collapsed() {
        let session = paused_session();
        let proposing = proposing_output(&session).unwrap().clone();
        assert_eq!(proposing.envelope_id, "user-message-proposing");
        let parents = review_reason_parents(
            &session,
            &proposing,
            &provenance(&session, &["owner", "pending", "device"]),
        )
        .unwrap();
        // The pending result collapses to the proposing output it came from.
        let device = session.conversation[3].data_envelope.clone().unwrap();
        assert_eq!(
            ids(&parents.labels),
            BTreeSet::from([
                "user-message-owner".to_owned(),
                proposing.envelope_id.clone(),
                device.envelope_id,
            ])
        );
    }

    #[test]
    fn unverifiable_missing_or_rewritten_evidence_has_no_parents() {
        let session = paused_session();
        let proposing = proposing_output(&session).unwrap().clone();
        let mut unverifiable = provenance(&session, &["owner"]);
        unverifiable.verifiable = false;
        assert!(review_reason_parents(&session, &proposing, &unverifiable).is_none());
        let mut missing = provenance(&session, &["owner"]);
        missing.evidence[0].event_id = "gone".into();
        assert!(review_reason_parents(&session, &proposing, &missing).is_none());
        // A message rewritten after the review no longer vouches for it.
        let mut rewritten = provenance(&session, &["device"]);
        rewritten.evidence[0].text_sha256 = text_digest("what the reviewer read");
        assert!(review_reason_parents(&session, &proposing, &rewritten).is_none());
        let mut unlabelled = session.clone();
        unlabelled.conversation[3].data_envelope = None;
        assert!(
            review_reason_parents(
                &unlabelled,
                &proposing,
                &provenance(&unlabelled, &["device"])
            )
            .is_none()
        );
    }

    #[test]
    fn evidence_from_another_model_falls_back_to_unverifiable() {
        let mut session = paused_session();
        // The owner switched models within the gateway after this message.
        session.conversation[0].data_envelope = Some(label("owner", "other"));
        let proposing = proposing_output(&session).unwrap().clone();
        assert!(
            review_reason_parents(&session, &proposing, &provenance(&session, &["owner"]))
                .is_none()
        );
    }

    #[test]
    fn an_action_denial_shows_the_reviewer_reason_only_when_its_provenance_is_verified() {
        use crate::model_message_labels::{COMPOSED_RESULT_PREFIX, composed_envelope};
        let session = paused_session();
        let proposing = proposing_output(&session).unwrap().clone();
        let ai = |verifiable| AiDecisionReason {
            reason: "The owner only asked to read the page.".into(),
            reason_code: "exceeds_task".into(),
            provenance: ReviewProvenance {
                verifiable,
                ..provenance(&session, &["owner", "device"])
            },
        };
        let label = |content: &crate::model_content::ModelContent| {
            composed_envelope(
                content,
                Some(&proposing),
                "call",
                ServerResultSource::ExecutionStatus,
                false,
            )
            .unwrap()
            .unwrap()
        };
        // Verified: the reviewer's words, under the proposing output and every
        // piece of evidence the reviewer read.
        let verified = action_denial_content(&session, Some(&proposing), &ai(true));
        assert!(
            verified
                .text()
                .contains("Reviewer reason: The owner only asked to read the page.")
        );
        assert!(verified.text().contains("reason_code=exceeds_task"));
        assert!(!verified.is_unverified());
        let verified = label(&verified);
        assert!(verified.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
        assert_eq!(
            verified.provenance.source_envelope_ids[0],
            proposing.envelope_id
        );
        assert_eq!(
            ids(&session
                .conversation
                .iter()
                .filter_map(|message| message.data_envelope.clone())
                .filter(|label| verified
                    .provenance
                    .source_envelope_ids
                    .contains(&label.envelope_id))
                .collect::<Vec<_>>()),
            BTreeSet::from([
                "user-message-owner".to_owned(),
                proposing.envelope_id.clone(),
                session.conversation[3]
                    .data_envelope
                    .clone()
                    .unwrap()
                    .envelope_id,
            ])
        );
        // Unverifiable, or without the proposing output: only the code and
        // its guidance, as server text under the proposing output.
        for fallback in [
            action_denial_content(&session, Some(&proposing), &ai(false)),
            action_denial_content(&session, None, &ai(true)),
        ] {
            assert!(!fallback.text().contains("read the page"));
            assert!(fallback.text().contains("reason_code=exceeds_task"));
            assert!(fallback.sources().is_server_only());
            assert_eq!(
                label(&fallback).provenance.source_envelope_ids,
                std::slice::from_ref(&proposing.envelope_id)
            );
        }
    }

    #[test]
    fn receipts_use_the_review_shape_only_for_a_verified_ai_reason() {
        use crate::model_message_labels::COMPOSED_RESULT_PREFIX;
        let session = paused_session();
        let proposing = proposing_output(&session);
        let ai = |verifiable| AiDecisionReason {
            reason: "The owner asked for it.".into(),
            reason_code: "within_scope".into(),
            provenance: ReviewProvenance {
                verifiable,
                ..provenance(&session, &["owner"])
            },
        };
        let shape = |ai: Option<&AiDecisionReason>| {
            decision_receipt_envelope(
                &session,
                proposing,
                ai,
                None,
                "receipt",
                "{}",
                ServerResultSource::SkillProposalDecision,
            )
            .unwrap()
            .envelope
            .unwrap()
        };
        assert!(
            shape(Some(&ai(true)))
                .envelope_id
                .starts_with(COMPOSED_RESULT_PREFIX)
        );
        assert!(
            shape(Some(&ai(false)))
                .envelope_id
                .starts_with("unverified-result-")
        );
        let owner = shape(None);
        assert!(owner.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
        assert_eq!(
            owner.provenance.source_envelope_ids,
            [proposing.unwrap().envelope_id.clone()]
        );
    }

    #[test]
    fn stored_text_the_reviewer_read_is_traced_only_with_its_recorded_source() {
        use crate::approval_review::ReviewSkillBase;
        let base = ReviewSkillBase {
            skill_id: "skill-1".into(),
            name: "disk-usage".into(),
            version: 2,
            ai_authored: true,
        };
        let mut candidate = crate::approval_review::tests::candidate();
        let skill = ApprovalSource::SkillProposal {
            proposal_id: "p".into(),
        };
        let directory = ApprovalSource::DirectoryProposal {
            request_id: "d".into(),
        };
        let cases = [
            (
                skill.clone(),
                serde_json::json!({"skill_proposal":{"kind":"create","base_version":null,"skill_md":"new"}}),
                None,
                true,
                0,
            ),
            (
                skill.clone(),
                serde_json::json!({"skill_proposal":{"kind":"update","base_version":2,"skill_md":"merged"}}),
                None,
                false,
                0,
            ),
            (
                skill.clone(),
                serde_json::json!({"skill_proposal":{"kind":"update","base_version":2,"skill_md":"merged"}}),
                Some(base.clone()),
                true,
                1,
            ),
            (
                skill.clone(),
                serde_json::json!({"skill_proposal":{"kind":"disable","base_version":null,"skill_md":"stored"}}),
                Some(base.clone()),
                true,
                1,
            ),
            (
                directory.clone(),
                serde_json::json!({"directory_proposal":{"requested_path":"a","canonical_path":"a"}}),
                None,
                true,
                0,
            ),
            (
                directory.clone(),
                serde_json::json!({"directory_proposal":{"requested_path":"a","canonical_path":"/real/a"}}),
                None,
                true,
                1,
            ),
        ];
        for (source, action, skill_base, verifiable, stored) in cases {
            candidate.source = source;
            candidate.action_json = action.to_string();
            candidate.context.goal_text = None;
            candidate.context.skill_base = skill_base;
            let provenance = ReviewProvenance::from_candidate(&candidate);
            assert_eq!(provenance.verifiable, verifiable, "{action}");
            assert_eq!(provenance.stored.len(), stored, "{action}");
        }
        // The resolved directory is device-reported metadata of the request.
        assert_eq!(
            ReviewProvenance::from_candidate(&candidate).stored[0],
            ReviewedStoredText::Directory {
                request_id: "d".into(),
                path: "/real/a".into(),
            }
        );
        // Goal text needs the proof recorded when it was written.
        candidate.context.goal_text = Some("goal".into());
        candidate.goal_id = Some("goal-1".into());
        candidate.context.goal_text_source = None;
        assert!(!ReviewProvenance::from_candidate(&candidate).verifiable);
        candidate.context.goal_text_source =
            Some(crate::model_content::StoredTextSource::owner("actor", 1));
        let provenance = ReviewProvenance::from_candidate(&candidate);
        assert!(provenance.verifiable);
        assert!(provenance.stored.iter().any(|text| matches!(
            text,
            ReviewedStoredText::Goal { goal_id, .. } if goal_id == "goal-1"
        )));
        // The snapshot is stored with the decision and read back unchanged.
        let stored = serde_json::to_string(&provenance).unwrap();
        assert_eq!(
            serde_json::from_str::<ReviewProvenance>(&stored).unwrap(),
            provenance
        );
    }

    #[test]
    fn a_reason_that_read_stored_text_cites_its_root_in_denials_and_receipts() {
        use crate::model_message_labels::{COMPOSED_RESULT_PREFIX, composed_envelope};
        let session = paused_session();
        let proposing = proposing_output(&session).unwrap().clone();
        let goal = crate::model_content::StoredSpec::proven(
            crate::goal_projection::KIND,
            "goal-1",
            crate::goal_projection::GOAL_TEXT_FIELD,
            "Keep the disk tidy",
            Some(crate::model_content::StoredTextSource::owner("actor", 1)),
        );
        let ai = AiDecisionReason {
            reason: "The goal is only about the disk.".into(),
            reason_code: "exceeds_task".into(),
            provenance: ReviewProvenance {
                stored: vec![ReviewedStoredText::Goal {
                    goal_id: "goal-1".into(),
                    text: "Keep the disk tidy".into(),
                    source: crate::model_content::StoredTextSource::owner("actor", 1),
                }],
                ..provenance(&session, &["owner"])
            },
        };
        let parents = review_reason_parents(&session, &proposing, &ai.provenance).unwrap();
        let [root] = parents.roots.as_slice() else {
            panic!("one stored root");
        };
        assert_eq!(
            root,
            &goal.root(proposing.allowed_destinations.clone()).unwrap()
        );
        // The denial shows the reason and cites the root.
        let denial = action_denial_content(&session, Some(&proposing), &ai);
        assert!(denial.text().contains("The goal is only about the disk."));
        assert_eq!(denial.stored_roots(), vec![root.clone()]);
        let label = composed_envelope(
            &denial,
            Some(&proposing),
            "call",
            ServerResultSource::ExecutionStatus,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(label.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
        assert!(
            label
                .provenance
                .source_envelope_ids
                .contains(&root.label.envelope_id)
        );
        // A decision receipt carries the root with its label.
        let receipt = decision_receipt_envelope(
            &session,
            Some(&proposing),
            Some(&ai),
            None,
            "receipt",
            "{}",
            ServerResultSource::SkillProposalDecision,
        )
        .unwrap();
        assert_eq!(receipt.stored_roots, vec![root.clone()]);
        assert!(
            receipt
                .envelope
                .unwrap()
                .provenance
                .source_envelope_ids
                .contains(&root.label.envelope_id)
        );
    }
}
