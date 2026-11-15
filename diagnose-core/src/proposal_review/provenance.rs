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
use std::collections::{BTreeMap, BTreeSet};

/// What an AI reviewer saw, frozen from the settled candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewProvenance {
    /// False when the reviewer saw content from outside the conversation:
    /// a stored skill version, a device-resolved directory, or goal text.
    pub verifiable: bool,
    /// The evidence as the reviewer saw it. Its text is compared with the
    /// session when the receipt is written, so a message rewritten after the
    /// review never vouches for what the reviewer read.
    pub evidence: Vec<ApprovalEvidence>,
}

impl ReviewProvenance {
    pub fn from_candidate(candidate: &ApprovalReviewCandidate) -> Self {
        let action =
            serde_json::from_str::<serde_json::Value>(&candidate.action_json).unwrap_or_default();
        let outside = match candidate.source {
            ApprovalSource::SkillProposal { .. } => {
                let proposal = &action["skill_proposal"];
                !proposal["base_version"].is_null()
                    || matches!(proposal["kind"].as_str(), Some("disable" | "delete"))
            }
            ApprovalSource::DirectoryProposal { .. } => {
                let proposal = &action["directory_proposal"];
                proposal["canonical_path"] != proposal["requested_path"]
            }
            _ => false,
        };
        Self {
            verifiable: !outside && candidate.context.goal_text.is_none(),
            evidence: candidate.context.evidence.clone(),
        }
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
) -> Option<Vec<DataEnvelope>> {
    if !provenance.verifiable {
        return None;
    }
    let mut parents = BTreeMap::new();
    parents.insert(proposing.envelope_id.clone(), proposing.clone());
    for evidence in &provenance.evidence {
        let message = crate::approval_review::review_evidence_message(session, &evidence.event_id)?;
        if message.text != evidence.text {
            return None;
        }
        let label = collapse_status_label(session, message.data_envelope.as_ref()?)?;
        parents.insert(label.envelope_id.clone(), label);
    }
    if parents.len() > MAX_LINEAGE_ITEMS {
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
    let mut ordered = vec![proposing.clone()];
    ordered.extend(
        parents
            .into_values()
            .filter(|parent| parent.envelope_id != proposing.envelope_id),
    );
    Some(ordered)
}

/// An AI reviewer's verdict reason as a decision receipt needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
) -> Result<Option<DataEnvelope>, desk_agent_protocol::AgentError> {
    use crate::model_content::{ModelContent, Unverified};
    use crate::model_message_labels::{composed_envelope, server_review_envelope};
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
    };
    let parent = proposing
        .cloned()
        .or_else(|| fallback.and_then(|label| collapse_status_label(session, label)));
    match ai {
        Some(ai) => {
            let parents = proposing
                .and_then(|proposing| review_reason_parents(session, proposing, &ai.provenance));
            match parents {
                Some(parents) => server_review_envelope(
                    &parents.iter().collect::<Vec<_>>(),
                    call_id,
                    content,
                    source,
                )
                .map(Some)
                .or_else(|_| unverified(parent.as_ref())),
                None => unverified(parent.as_ref()),
            }
        }
        // Without an AI reason the receipt holds the server's decision only.
        None => composed_envelope(
            &ModelContent::server_text(content),
            parent.as_ref(),
            call_id,
            source,
            false,
        ),
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
                    ApprovalEvidence {
                        event_id: (*id).into(),
                        trust: ApprovalEvidenceTrust::UntrustedContent,
                        text: message.text.clone(),
                    }
                })
                .collect(),
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
            ids(&parents),
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
        rewritten.evidence[0].text = "what the reviewer read".into();
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
    fn candidates_with_outside_content_are_unverifiable() {
        let mut candidate = crate::approval_review::tests::candidate();
        let cases = [
            (
                ApprovalSource::SkillProposal {
                    proposal_id: "p".into(),
                },
                serde_json::json!({"skill_proposal":{"kind":"create","base_version":null}}),
                true,
            ),
            (
                ApprovalSource::SkillProposal {
                    proposal_id: "p".into(),
                },
                serde_json::json!({"skill_proposal":{"kind":"update","base_version":2}}),
                false,
            ),
            (
                ApprovalSource::SkillProposal {
                    proposal_id: "p".into(),
                },
                serde_json::json!({"skill_proposal":{"kind":"disable","base_version":null}}),
                false,
            ),
            (
                ApprovalSource::DirectoryProposal {
                    request_id: "d".into(),
                },
                serde_json::json!({"directory_proposal":{"requested_path":"a","canonical_path":"a"}}),
                true,
            ),
            (
                ApprovalSource::DirectoryProposal {
                    request_id: "d".into(),
                },
                serde_json::json!({"directory_proposal":{"requested_path":"a","canonical_path":"/real/a"}}),
                false,
            ),
        ];
        for (source, action, verifiable) in cases {
            candidate.source = source;
            candidate.action_json = action.to_string();
            candidate.context.goal_text = None;
            assert_eq!(
                ReviewProvenance::from_candidate(&candidate).verifiable,
                verifiable,
                "{action}"
            );
        }
        candidate.context.goal_text = Some("goal".into());
        assert!(!ReviewProvenance::from_candidate(&candidate).verifiable);
    }
}
