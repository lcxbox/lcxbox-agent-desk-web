//! Model-only projection of a completed review onto its original tool call.
//! Persisted draft and decision receipts remain immutable audit records.
use crate::{
    chat::{ChatMessage, ChatRole},
    dynamic_run::RUN_CONTROL_PROVIDER_ID,
};
use desk_agent_protocol::{AgentError, data_lineage::DataEnvelope};
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
            let event = labelled(message, ChatRole::SystemEvent, "schedule_activation");
            if !event && !labelled(message, ChatRole::Tool, "schedule_proposal") {
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

pub fn project(messages: &mut [ChatMessage]) -> Result<(), AgentError> {
    for index in 0..messages.len() {
        let draft = &messages[index];
        if !labelled(draft, ChatRole::Tool, "schedule_proposal") {
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
                if !labelled(message, ChatRole::SystemEvent, "schedule_activation") {
                    return None;
                }
                let value = serde_json::from_str::<Value>(&message.text).ok()?;
                let approved =
                    value["event"] == "scheduled_task_activated" && value["state"] == "active";
                let rejected =
                    value["event"] == "scheduled_task_rejected" && value["state"] == "deleted";
                (value["schedule_id"].as_str() == Some(id) && (approved || rejected))
                    .then_some((at, value))
            });
        let Some((receipt, mut value)) = decision else {
            continue;
        };
        let approved = value["event"] == "scheduled_task_activated";
        value["kind"] = Value::String("fresh_task".into());
        value["awaiting_decision"] = Value::Bool(false);
        value["review_complete"] = Value::Bool(true);
        value["message"] = Value::String(if approved {
            "This proposal has been approved and scheduling is enabled. Report success using the returned execution time; do not ask for approval again."
        } else {
            "This proposal was rejected. It is not scheduled. Report the rejection; do not ask for confirmation again or recreate it without a new user request."
        }.into());
        label_decided(messages, index, receipt, value.to_string())?;
    }
    Ok(())
}

/// Replaces the pending draft at `draft` with the decided `text` for one
/// model request and relabels both records for it. The projection removes
/// the draft's own label from the model input, so the result and the decision
/// receipt both derive from the draft's parent, which stays an input of the
/// same request. Labels never gain a destination or extend retention: each
/// takes the draft parent's, which the stored draft and receipt inherit.
pub(crate) fn label_decided(
    messages: &mut [ChatMessage],
    draft: usize,
    receipt: usize,
    text: String,
) -> Result<(), AgentError> {
    use crate::model_message_labels::{
        ServerResultSource, invalid_label, server_projection_envelope,
    };
    let parent = messages[draft].data_envelope.as_ref().and_then(|label| {
        let [id] = label.provenance.source_envelope_ids.as_slice() else {
            return None;
        };
        messages[..draft].iter().rev().find_map(|message| {
            message
                .data_envelope
                .as_ref()
                .filter(|p| &p.envelope_id == id)
        })
    });
    let parent = parent.or(messages[draft].data_envelope.as_ref()).cloned();
    let decision = &messages[receipt];
    let source = decision
        .data_envelope
        .as_ref()
        .and_then(|label| ServerResultSource::parse(&label.provenance.source_tool_name))
        .ok_or_else(invalid_label)?;
    let receipt_label = server_projection_envelope(
        parent.as_ref(),
        &decision.message_id,
        &decision.text,
        source,
    )?;
    let call_id = messages[draft]
        .tool_call_id
        .clone()
        .ok_or_else(invalid_label)?;
    let draft_label = server_projection_envelope(parent.as_ref(), &call_id, &text, source)?;
    messages[receipt].data_envelope = receipt_label;
    messages[draft].text = text;
    messages[draft].data_envelope = draft_label;
    Ok(())
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
            draft.data_envelope.as_ref(),
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
            project(&mut projected).unwrap();
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
            project(&mut projected).unwrap();
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
            project(&mut messages).unwrap();
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
