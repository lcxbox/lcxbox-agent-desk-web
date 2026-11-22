//! Model-only projection of decided proposals onto their pending results.
//!
//! Each decided proposal is a pair: the pending tool result the proposing
//! call left, and the server's decision receipt. The projection replaces the
//! pending body with the decision for one request, and decides whether the AI
//! reviewer's reason may stay visible: only when every parent of its review
//! label is an input of this same request. Otherwise both messages show the
//! decision without the reason, as server text under the proposing output.
//! Stored records stay unchanged.

use crate::{
    approval_review::ApprovalReasonCode,
    chat::ChatMessage,
    model_message_labels::{COMPOSED_RESULT_PREFIX, ServerResultSource, invalid_label},
};
use desk_agent_protocol::{AgentError, data_lineage::DataEnvelope};
use serde_json::Value;

/// One decided proposal: its pending result, its receipt, and how the pending
/// result reads once decided, from the receipt's body.
pub(crate) struct DecidedPair {
    pub draft: usize,
    pub receipt: usize,
    pub finish: fn(&mut Value),
}

/// Projects every decided skill, schedule and directory proposal, oldest
/// receipt first, so a receipt whose reason drew on an earlier receipt sees
/// that receipt as this request shows it.
pub fn project_decisions(messages: &mut [ChatMessage]) -> Result<(), AgentError> {
    let mut pairs = crate::schedule::review_result::decided_pairs(messages);
    pairs.extend(crate::skills::authoring::decided_pairs(messages));
    pairs.extend(crate::directory_tools::decided_pairs(messages));
    pairs.sort_by_key(|pair| pair.receipt);
    for pair in pairs {
        project_decided(messages, &pair)?;
    }
    Ok(())
}

const REASON_NOTE: &str = "The reviewer's reason is shown to the owner.";

/// The decision without the reviewer's free text. A denial keeps its closed
/// reason code and the server's guidance for it.
fn without_reason(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.remove("reason");
    let code = object
        .remove("reason_code")
        .and_then(|code| code.as_str().and_then(ApprovalReasonCode::parse))
        .filter(|code| *code != ApprovalReasonCode::WithinScope);
    if let Some(code) = code {
        object.insert("reason_code".into(), code.as_str().into());
        object.insert("reason_guidance".into(), code.guidance().into());
    }
    object.insert("reason_note".into(), REASON_NOTE.into());
}

fn label_in<'a>(messages: &'a [ChatMessage], id: &str) -> Option<&'a DataEnvelope> {
    messages
        .iter()
        .filter_map(|message| message.data_envelope.as_ref())
        .find(|label| label.envelope_id == id)
}

fn project_decided(messages: &mut [ChatMessage], pair: &DecidedPair) -> Result<(), AgentError> {
    let Ok(mut value) = serde_json::from_str::<Value>(&messages[pair.receipt].text) else {
        return Ok(());
    };
    let call_id = messages[pair.draft]
        .tool_call_id
        .clone()
        .ok_or_else(invalid_label)?;
    let Some(label) = messages[pair.receipt].data_envelope.clone() else {
        // An unlabelled conversation keeps unlabelled projections.
        (pair.finish)(&mut value);
        messages[pair.draft].replace_content(value.to_string(), None);
        return Ok(());
    };
    let source = ServerResultSource::parse(&label.provenance.source_tool_name)
        .filter(|source| source.reviewed())
        .ok_or_else(invalid_label)?;
    let earlier = &messages[..pair.receipt];
    // The proposing output: the pending result's parent, else the receipt's.
    let proposing = messages[pair.draft]
        .data_envelope
        .as_ref()
        .and_then(|draft| draft.provenance.source_envelope_ids.first())
        .or_else(|| label.provenance.source_envelope_ids.first())
        .and_then(|id| label_in(&messages[..pair.draft], id))
        .cloned();
    // A registered receipt keeps its reason when every parent it was
    // decided from is an input of this request.
    if label.envelope_id.starts_with(COMPOSED_RESULT_PREFIX) {
        // A parent is a label of this request, or a root of stored text the
        // receipt itself carries.
        let roots = &messages[pair.receipt].stored_roots;
        let mut parents = crate::proposal_review::provenance::ReasonParents {
            labels: Vec::new(),
            roots: Vec::new(),
        };
        let found = label.provenance.source_envelope_ids.iter().all(|id| {
            if let Some(found) = label_in(earlier, id) {
                parents.labels.push(found.clone());
                true
            } else if let Some(root) = roots.iter().find(|root| root.label.envelope_id == *id) {
                parents.roots.push(root.clone());
                true
            } else {
                false
            }
        });
        if found && !parents.labels.is_empty() {
            (pair.finish)(&mut value);
            let reason = parents.content(&value.to_string());
            let issuing = parents.labels[0].clone();
            messages[pair.draft] = messages[pair.draft].clone().with_content(
                &reason,
                crate::model_content::Labelling {
                    issuing: Some(&issuing),
                    call_id: &call_id,
                    source,
                    request_only: false,
                },
            )?;
            return Ok(());
        }
    }
    // An unverifiable reason, or one whose parents this request lacks.
    without_reason(&mut value);
    let receipt_text = value.to_string();
    let receipt_id = messages[pair.receipt].message_id.clone();
    let server = |text: String, call_id: &str, message: &ChatMessage| {
        message.clone().with_content(
            &crate::model_content::ModelContent::server_text(text),
            crate::model_content::Labelling {
                issuing: proposing.as_ref(),
                call_id,
                source,
                request_only: true,
            },
        )
    };
    messages[pair.receipt] = server(receipt_text, &receipt_id, &messages[pair.receipt])?;
    (pair.finish)(&mut value);
    messages[pair.draft] = server(value.to_string(), &call_id, &messages[pair.draft])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::ChatRole,
        model_message_labels::COMPOSED_RESULT_PREFIX,
        proposal_review::provenance::{
            AiDecisionReason, decision_receipt_envelope, proposing_output,
            tests::{paused_session, provenance},
        },
    };

    const DENIED: &str = "The owner did not name this skill.";

    /// The paused session decided by AI with `reason` and its receipt label.
    fn decided(verifiable: bool, evidence: &[&str]) -> Vec<ChatMessage> {
        let mut session = paused_session();
        session.conversation[2].text = serde_json::json!({"proposal_id":"p1",
            "status":"pending","awaiting_decision":true})
        .to_string();
        let pending = session.conversation[2].clone();
        session.conversation[2].data_envelope = crate::model_message_labels::composed_envelope(
            &crate::model_content::ModelContent::server_text(&pending.text),
            proposing_output(&session),
            "call",
            ServerResultSource::SkillProposal,
            false,
        )
        .unwrap();
        let ai = AiDecisionReason {
            reason: DENIED.into(),
            reason_code: "owner_not_requested".into(),
            provenance: crate::proposal_review::provenance::ReviewProvenance {
                verifiable,
                ..provenance(&session, evidence)
            },
        };
        let text = serde_json::json!({"event":crate::proposal_review::SKILL_DECIDED_EVENT,
            "proposal_id":"p1","status":"rejected","kind":"create","reason":DENIED,
            "reason_code":"owner_not_requested"})
        .to_string();
        let mut receipt = ChatMessage::system_event("receipt", &text);
        decision_receipt_envelope(
            &session,
            proposing_output(&session),
            Some(&ai),
            None,
            "receipt",
            &text,
            ServerResultSource::SkillProposalDecision,
        )
        .unwrap()
        .apply(&mut receipt);
        session.conversation.push(receipt);
        session.conversation
    }

    fn reason_visible(message: &ChatMessage) -> bool {
        message.text.contains(DENIED)
    }

    #[test]
    fn a_verified_reason_stays_with_every_parent_present() {
        let mut messages = decided(true, &["owner", "device"]);
        project_decisions(&mut messages).unwrap();
        let (draft, receipt) = (&messages[2], &messages[4]);
        assert!(reason_visible(draft) && reason_visible(receipt));
        for label in [draft, receipt].map(|message| message.data_envelope.as_ref().unwrap()) {
            assert!(label.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
            assert_eq!(label.provenance.source_envelope_ids.len(), 3);
        }
        assert!(draft.text.contains("\"awaiting_decision\":false"));
    }

    #[test]
    fn a_missing_parent_or_unverifiable_reason_leaves_only_the_code() {
        // Trimmed evidence: the owner message is not in this request.
        let mut trimmed = decided(true, &["owner", "device"]);
        trimmed.remove(0);
        let mut unverifiable = decided(false, &["owner"]);
        assert!(
            unverifiable[4]
                .data_envelope
                .as_ref()
                .unwrap()
                .envelope_id
                .starts_with("unverified-result-")
        );
        for (messages, draft, receipt) in [(&mut trimmed, 1, 3), (&mut unverifiable, 2, 4)] {
            project_decisions(messages).unwrap();
            for message in [&messages[draft], &messages[receipt]] {
                assert!(!reason_visible(message), "{}", message.text);
                let value: Value = serde_json::from_str(&message.text).unwrap();
                assert_eq!(value["reason_code"], "owner_not_requested");
                assert_eq!(
                    value["reason_guidance"],
                    ApprovalReasonCode::OwnerNotRequested.guidance()
                );
                assert_eq!(value["reason_note"], REASON_NOTE);
                let label = message.data_envelope.as_ref().unwrap();
                assert!(label.envelope_id.starts_with(COMPOSED_RESULT_PREFIX));
                assert_eq!(
                    label.provenance.source_envelope_ids,
                    ["user-message-proposing"]
                );
            }
        }
    }

    #[test]
    fn an_approval_without_a_verified_reason_shows_no_code() {
        let mut value = serde_json::json!({"status":"approved","reason":"ok",
            "reason_code":"within_scope"});
        without_reason(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"status":"approved","reason_note":REASON_NOTE})
        );
    }

    #[test]
    fn a_rewritten_receipt_takes_later_receipts_citing_it_along() {
        let mut messages = decided(true, &["owner", "device"]);
        // A later decision whose reviewer saw the first receipt.
        let first = messages[4].data_envelope.clone().unwrap();
        let proposing = messages[1].data_envelope.clone().unwrap();
        let mut call = messages[1].clone();
        call.message_id = "agent-2".into();
        call.tool_calls[0].id = "call-2".into();
        let mut pending = messages[2].clone();
        pending.message_id = "pending-2".into();
        pending.tool_call_id = Some("call-2".into());
        pending.text = pending.text.replace("p1", "p2");
        let text = messages[4].text.replace("p1", "p2");
        let mut receipt = ChatMessage::system_event("receipt-2", &text);
        receipt.data_envelope = Some(
            crate::model_message_labels::server_review_envelope(
                &[&proposing, &first],
                "receipt-2",
                &text,
                ServerResultSource::SkillProposalDecision,
            )
            .unwrap(),
        );
        messages.extend([call, pending, receipt]);
        // The first receipt's owner evidence is not in this request.
        messages.remove(0);
        project_decisions(&mut messages).unwrap();
        assert!(!reason_visible(&messages[3]));
        assert!(!reason_visible(&messages[6]), "{}", messages[6].text);
        assert_eq!(messages[6].role, ChatRole::SystemEvent);
    }
}
