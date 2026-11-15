//! Model-only projection of a completed review onto its original tool call.
//! Persisted draft and decision receipts remain immutable audit records.
use crate::{
    chat::{ChatMessage, ChatRole},
    dynamic_run::RUN_CONTROL_PROVIDER_ID,
    model_message_labels::ServerResultSource,
};
use desk_agent_protocol::data_lineage::DataEnvelope;
use serde_json::Value;

pub(crate) fn labelled(message: &ChatMessage, role: ChatRole, source: &str) -> bool {
    message.role == role
        && message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_provider_id == RUN_CONTROL_PROVIDER_ID
                && label.provenance.source_tool_name == source
        })
}

/// Label of the server record that enabled a conversation timer in its source
/// conversation: the activation receipt written for an owner creation or
/// decision, or the proposal result of a timer the AI enabled in its own turn.
/// The scheduled input derives its label from this record.
pub fn activation_envelope<'a>(
    messages: &'a [ChatMessage],
    schedule_id: &str,
) -> Option<&'a DataEnvelope> {
    messages
        .iter()
        .rev()
        .find(|message| {
            let event = labelled(
                message,
                ChatRole::SystemEvent,
                ServerResultSource::ScheduleActivation.as_str(),
            );
            if !event
                && !labelled(
                    message,
                    ChatRole::Tool,
                    ServerResultSource::ScheduleProposal.as_str(),
                )
            {
                return false;
            }
            serde_json::from_str::<Value>(&message.text).is_ok_and(|value| {
                value["schedule_id"].as_str() == Some(schedule_id)
                    && if event {
                        value["event"] == "scheduled_task_activated"
                    } else {
                        value["kind"] == "conversation_resume" && value["state"] == "active"
                    }
            })
        })
        .and_then(|message| message.data_envelope.as_ref())
}

/// Decided fresh-task proposals: each pending result with its decision.
pub(crate) fn decided_pairs(
    messages: &[ChatMessage],
) -> Vec<crate::proposal_review::projection::DecidedPair> {
    let mut pairs = Vec::new();
    for index in 0..messages.len() {
        let draft = &messages[index];
        if !labelled(
            draft,
            ChatRole::Tool,
            ServerResultSource::ScheduleProposal.as_str(),
        ) {
            continue;
        }
        let Ok(proposal) = serde_json::from_str::<Value>(&draft.text) else {
            continue;
        };
        if proposal["state"] != "pending_review" || proposal["kind"] != "fresh_task" {
            continue;
        }
        let Some(id) = proposal["schedule_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if draft.tool_call_id.is_none() {
            continue;
        }
        let decision = messages
            .iter()
            .enumerate()
            .skip(index + 1)
            .find_map(|(at, message)| {
                if !labelled(
                    message,
                    ChatRole::SystemEvent,
                    ServerResultSource::ScheduleActivation.as_str(),
                ) {
                    return None;
                }
                let value = serde_json::from_str::<Value>(&message.text).ok()?;
                let approved =
                    value["event"] == "scheduled_task_activated" && value["state"] == "active";
                let rejected =
                    value["event"] == "scheduled_task_rejected" && value["state"] == "deleted";
                (value["schedule_id"].as_str() == Some(id) && (approved || rejected)).then_some(at)
            });
        if let Some(receipt) = decision {
            pairs.push(crate::proposal_review::projection::DecidedPair {
                draft: index,
                receipt,
                finish: finish_decided,
            });
        }
    }
    pairs
}

/// How a decided fresh-task proposal reads in place of its pending result.
fn finish_decided(value: &mut Value) {
    let approved = value["event"] == "scheduled_task_activated";
    value["kind"] = Value::String("fresh_task".into());
    value["awaiting_decision"] = Value::Bool(false);
    value["review_complete"] = Value::Bool(true);
    value["message"] = Value::String(if approved {
        "This proposal has been approved and scheduling is enabled. Report success using the returned execution time; do not ask for approval again."
    } else {
        "This proposal was rejected. It is not scheduled. Report the rejection; do not ask for confirmation again or recreate it without a new user request."
    }.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_message_labels::{model_bound_user_message, server_result_envelope};
    use desk_agent_protocol::data_lineage::DestinationIdentity;
    fn records(approved: bool) -> Vec<ChatMessage> {
        let user = model_bound_user_message(
            "u".into(),
            "timer".into(),
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "m".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let mut draft = ChatMessage::tool_result(
            "draft",
            "call",
            r#"{"schedule_id":"timer","kind":"fresh_task","state":"pending_review","awaiting_decision":true}"#,
        );
        draft.data_envelope = server_result_envelope(
            user.data_envelope.as_ref(),
            "call",
            &draft.text,
            crate::model_message_labels::ServerResultSource::ScheduleProposal,
        )
        .unwrap();
        let text = serde_json::json!({"schedule_id":"timer", "event":if approved {"scheduled_task_activated"} else {"scheduled_task_rejected"}, "decided_by":"approval_ai", "state":if approved {"active"} else {"deleted"}, "next_run_at_utc_ms":12345}).to_string();
        let mut receipt = ChatMessage::system_event("receipt", &text);
        receipt.data_envelope = server_result_envelope(
            user.data_envelope.as_ref(),
            "receipt",
            &text,
            crate::model_message_labels::ServerResultSource::ScheduleActivation,
        )
        .unwrap();
        vec![user, draft, receipt]
    }
    #[test]
    fn projects_final_decision_onto_tool_without_modifying_original_records() {
        for approved in [true, false] {
            let original = records(approved);
            let mut projected = original.clone();
            crate::proposal_review::projection::project_decisions(&mut projected).unwrap();
            let result: Value = serde_json::from_str(&projected[1].text).unwrap();
            assert_eq!(result["state"], if approved { "active" } else { "deleted" });
            assert_eq!(result["awaiting_decision"], false);
            assert_eq!(result["decided_by"], "approval_ai");
            assert_eq!(result["next_run_at_utc_ms"], 12345);
            assert_eq!(projected[0], original[0]);
            assert_eq!(projected[1].tool_call_id, original[1].tool_call_id);
            assert_eq!(projected[2].text, original[2].text);
            assert_eq!(
                serde_json::from_str::<Value>(&original[1].text).unwrap()["state"],
                "pending_review"
            );
            // Both labels derive from the draft's parent, never gaining a
            // destination or retention over the stored records.
            let draft = original[1].data_envelope.as_ref().unwrap();
            for label in projected[1..]
                .iter()
                .map(|m| m.data_envelope.as_ref().unwrap())
            {
                assert_eq!(
                    label.provenance.source_envelope_ids,
                    draft.provenance.source_envelope_ids
                );
                assert_eq!(label.allowed_destinations, draft.allowed_destinations);
                assert_eq!(label.retention, draft.retention);
            }
            let once = projected.clone();
            crate::proposal_review::projection::project_decisions(&mut projected).unwrap();
            assert_eq!(projected, once);
        }
    }
    #[test]
    fn unrelated_untrusted_or_missing_decisions_leave_draft_unchanged() {
        for case in 0..4 {
            let mut messages = records(true);
            match case {
                0 => {
                    messages.pop();
                }
                1 => messages[2].role = ChatRole::User,
                2 => messages[2].data_envelope = None,
                _ => messages[2].text = messages[2].text.replace("timer", "other"),
            }
            let original = messages.clone();
            crate::proposal_review::projection::project_decisions(&mut messages).unwrap();
            assert_eq!(messages, original);
        }
    }
    #[test]
    fn activation_envelope_accepts_a_receipt_or_an_enabled_ai_timer_only() {
        let receipt = records(true).pop().unwrap();
        assert_eq!(
            activation_envelope(std::slice::from_ref(&receipt), "timer"),
            receipt.data_envelope.as_ref()
        );
        let mut timer = records(true).remove(1);
        timer.text =
            r#"{"schedule_id":"timer","kind":"conversation_resume","state":"active"}"#.into();
        assert_eq!(
            activation_envelope(std::slice::from_ref(&timer), "timer"),
            timer.data_envelope.as_ref()
        );
        let mut rejected = receipt.clone();
        rejected.text = rejected
            .text
            .replace("scheduled_task_activated", "scheduled_task_rejected");
        let pending = records(true).remove(1);
        let mut unlabelled = timer.clone();
        unlabelled.data_envelope = None;
        let mut user = timer.clone();
        user.role = ChatRole::User;
        for message in [rejected, pending, unlabelled, user] {
            assert_eq!(activation_envelope(&[message], "timer"), None);
        }
        assert_eq!(activation_envelope(&[timer], "other"), None);
    }
}
