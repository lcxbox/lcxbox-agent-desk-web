//! The tool phase that waits for the AI reviewer or the owner to decide a
//! proposal saved by the current call. No transaction is held between polls;
//! the decision receipt written by the deciding transaction is the only
//! snapshot change the wait adopts.

use super::*;
use crate::proposal_review::{AI_REVIEW_WINDOW_MS, ProposalKind, SKILL_AWAITING_OWNER_EVENT};

pub(super) async fn run(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    call_id: &str,
    observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    let Some(reference) = session.pending_proposal_review.clone() else {
        return Ok(());
    };
    // Persist the finished-tool mark so the decision receipt is appended to
    // this exact snapshot.
    deps.session_seam.save(session).await?;
    // A schedule proposal waits until a decision; a skill proposal returns to
    // the model once the AI review window closes and stays with the owner.
    let deadline = match reference.kind {
        ProposalKind::Schedule => None,
        ProposalKind::Skill => {
            Some(current_unix_ms(deps.clock)?.saturating_add(AI_REVIEW_WINDOW_MS))
        }
    };
    let decided = loop {
        ensure_lease_healthy(deps).await?;
        if deps
            .session_seam
            .poll_proposal_review(session, &reference, observation.clone())
            .await?
        {
            break true;
        }
        if deadline
            .is_some_and(|deadline| current_unix_ms(deps.clock).is_ok_and(|now| now >= deadline))
        {
            break false;
        }
    };
    ensure_lease_healthy(deps).await?;
    session.pending_proposal_review = None;
    if !decided {
        let id = format!(
            "{}{}:awaiting-owner",
            reference.kind.receipt_prefix(),
            reference.id
        );
        let text = serde_json::json!({
            "event": SKILL_AWAITING_OWNER_EVENT,
            "proposal_id": reference.id,
            "status": "pending",
            "instruction": "The AI reviewer did not decide this proposal in time. The owner decides it later in the application; a later decision does not resume this request. Continue with other work and do not claim the skill was saved.",
        })
        .to_string();
        let parent = session
            .conversation
            .iter()
            .rev()
            .find_map(|message| message.data_envelope.as_ref());
        let mut message = ChatMessage::system_event(&id, &text);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            parent,
            &id,
            &text,
            "skill_proposal",
        )?;
        message.turn_id = session.current_turn_id.clone();
        session.conversation.push(message);
    }
    deps.session_seam.save(session).await?;
    if let Some(receipt) = session.conversation.last() {
        sink.on_tool_finished(call_id, true, &receipt.text, None);
    }
    Ok(())
}
