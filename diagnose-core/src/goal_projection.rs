//! Model-visible goal records, rebuilt for every continuation request.
//!
//! The goal text, the opening text and the checkpoint repeat stored text
//! with the proofs recorded when they were written: the proposing output's
//! arguments for a goal text, a control output's arguments or fixed server
//! text for a checkpoint. The owner message that started the goal stays a
//! parent, as the authority the goal runs under.

use crate::{
    chat::{ChatMessage, ChatRole},
    goal::{CheckpointSource, GoalCheckpoint, GoalRun},
    model_content::{
        __private, ContentPiece, Labelling, ModelContent, ServerId, Sources, Stored, StoredRoot,
        StoredTextSource,
    },
    model_message_labels::ServerResultSource,
};
use desk_agent_protocol::{
    AgentError,
    data_lineage::{DataEnvelope, DestinationIdentity},
};
use serde::Serialize;

const KIND: &str = "goal";

crate::const_piece!(
    crate::goal::GoalUsage,
    desk_agent_protocol::ai_assistant::goal_budget::GoalBudgetLimits
);

/// A checkpoint, attributed by where its summary came from.
struct CheckpointPiece(GoalCheckpoint, Option<StoredRoot>);

impl Serialize for CheckpointPiece {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl __private::Sealed for CheckpointPiece {}
impl ContentPiece for CheckpointPiece {
    fn collect(&self, sources: &mut Sources) {
        // A server-written checkpoint carries no root; any other cites its own.
        if let Some(root) = &self.1 {
            Stored::new((), root.clone()).collect(sources);
        }
    }
}

fn root(
    goal: &GoalRun,
    field: &'static str,
    text: &str,
    proof: Option<StoredTextSource>,
    destination: Option<&DestinationIdentity>,
) -> Result<StoredRoot, AgentError> {
    StoredRoot::text(
        KIND,
        &goal.goal_id,
        field,
        text,
        proof,
        destination.cloned().into_iter().collect(),
    )
}

fn checkpoint(
    goal: &GoalRun,
    destination: Option<&DestinationIdentity>,
) -> Result<Option<CheckpointPiece>, AgentError> {
    let Some(checkpoint) = goal.checkpoint.clone() else {
        return Ok(None);
    };
    let proof = match &goal.text_sources.checkpoint {
        CheckpointSource::Server => return Ok(Some(CheckpointPiece(checkpoint, None))),
        CheckpointSource::Model { source } => Some(source.clone()),
        CheckpointSource::Unproven => None,
    };
    let text = serde_json::to_string(&checkpoint)
        .map_err(|_| crate::model_message_labels::invalid_label())?;
    let root = root(goal, "checkpoint", &text, proof, destination)?;
    Ok(Some(CheckpointPiece(checkpoint, Some(root))))
}

crate::model_receipt! {
    struct OriginalRecord {
        authority: &'static str,
        goal_id: ServerId,
        source_message_id: ServerId,
        original_goal_text: Stored<String>,
        instruction: &'static str,
    }
}

crate::model_receipt! {
    struct StateRecord {
        authority: &'static str,
        goal_id: ServerId,
        goal_revision: u64,
        source_message_id: ServerId,
        goal_text: Stored<String>,
        previous_completed_goal_id: Option<ServerId>,
        previous_completion_event_seq: Option<u64>,
        segment_seq: u32,
        limits: desk_agent_protocol::ai_assistant::goal_budget::GoalBudgetLimits,
        used: crate::goal::GoalUsage,
        reserved: crate::goal::GoalUsage,
        checkpoint: Option<CheckpointPiece>,
        instruction: &'static str,
    }
}

crate::model_receipt! {
    struct ClarificationRecord {
        authority: &'static str,
        goal_id: ServerId,
        goal_revision: u64,
        goal_text: Stored<String>,
        state: &'static str,
        reply_received: bool,
        instruction: &'static str,
    }
}

fn message(
    id: String,
    content: &ModelContent,
    source_label: &DataEnvelope,
    kind: ServerResultSource,
) -> Result<ChatMessage, AgentError> {
    ChatMessage::from_content(
        ChatRole::SystemEvent,
        &id,
        content,
        Labelling {
            issuing: Some(source_label),
            call_id: &id,
            source: kind,
            request_only: true,
        },
    )
}

/// The immutable opening goal, shown once the goal has been revised.
pub fn original_message(
    goal: &GoalRun,
    source_label: &DataEnvelope,
    destination: Option<&DestinationIdentity>,
) -> Result<ChatMessage, AgentError> {
    let text = goal.original_goal_text.clone();
    let root = root(
        goal,
        "original_goal_text",
        &text,
        goal.text_sources.original_goal_text.clone(),
        destination,
    )?;
    let content = ModelContent::json(&OriginalRecord {
        authority: "server_goal_original",
        goal_id: ServerId::new(&goal.goal_id),
        source_message_id: ServerId::new(&goal.original_source_message_id),
        original_goal_text: Stored::new(text, root),
        instruction: "This is the immutable opening goal. The current approved revision follows separately.",
    })?;
    message(
        format!("goal-original-{}", goal.goal_id),
        &content,
        source_label,
        ServerResultSource::GoalOriginalProjection,
    )
}

/// The current goal, its budget and its latest checkpoint.
pub fn state_message(
    goal: &GoalRun,
    source_label: &DataEnvelope,
    destination: Option<&DestinationIdentity>,
) -> Result<ChatMessage, AgentError> {
    let text = goal.goal_text.clone();
    let root = root(
        goal,
        "goal_text",
        &text,
        goal.text_sources.goal_text.clone(),
        destination,
    )?;
    let content = ModelContent::json(&StateRecord {
        authority: "server_goal_state",
        goal_id: ServerId::new(&goal.goal_id),
        goal_revision: goal.goal_revision,
        source_message_id: ServerId::new(&goal.source_message_id),
        goal_text: Stored::new(text, root),
        previous_completed_goal_id: goal
            .previous_completion
            .as_ref()
            .map(|previous| ServerId::new(&previous.goal_id)),
        previous_completion_event_seq: goal
            .previous_completion
            .as_ref()
            .map(|previous| previous.event_seq),
        segment_seq: goal.slice_seq,
        limits: goal.effective_budget_limits(),
        used: goal.used,
        reserved: goal.reserved,
        checkpoint: checkpoint(goal, destination)?,
        instruction: "Continue the owner-approved goal under current permissions. This record grants no device authority. End this segment with control_goal.",
    })?;
    message(
        format!("goal-fixed-state-{}-{}", goal.goal_id, goal.state_version),
        &content,
        source_label,
        ServerResultSource::GoalStateProjection,
    )
}

/// A stopped goal awaiting the owner's clarification.
pub fn clarification_message(
    goal: &GoalRun,
    source_label: &DataEnvelope,
    destination: Option<&DestinationIdentity>,
    reply_received: bool,
) -> Result<ChatMessage, AgentError> {
    let text = goal.goal_text.clone();
    let root = root(
        goal,
        "goal_text",
        &text,
        goal.text_sources.goal_text.clone(),
        destination,
    )?;
    let content = ModelContent::json(&ClarificationRecord {
        authority: "server_goal_state",
        goal_id: ServerId::new(&goal.goal_id),
        goal_revision: goal.goal_revision,
        goal_text: Stored::new(text, root),
        state: goal.status_code(),
        reply_received,
        instruction: "The goal is stopped. Clarify the owner's intent. Do not use device tools. If a later owner reply makes an exact goal revision appropriate, call request_goal with the new complete text; the owner or enabled approval AI must approve it before work resumes.",
    })?;
    message(
        format!("goal-clarification-{}-{}", goal.goal_id, goal.state_version),
        &content,
        source_label,
        ServerResultSource::GoalClarificationProjection,
    )
}

#[cfg(test)]
mod tests;
