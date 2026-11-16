//! Commit an AI draft and its original tool-result receipt under one turn fence.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    model_management::{cancel, list, lock_target},
    resume_activation::activate_on,
    session::{
        OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch, SessionRow,
        TouchColumn, timestamp,
    },
};
use desk_diagnose_core::model_message_labels::ServerResultSource;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_observability::{InputIssue, Stage, StageOutcome, tool::ToolObservation},
    proposal_review::{ProposalKind, ProposalReviewRef},
    schedule::management_tools::Action,
    session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;

/// Applies a parsed management action in the caller's transaction after its
/// admission locks. Returns the receipt id and the session to adopt once the
/// caller commits.
pub async fn manage_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    owner: i32,
    session: &PersistedAgentSession,
    call: &ToolCall,
    action: Action,
    observation: &ToolObservation,
) -> Result<(String, PersistedAgentSession), ScheduleStoreError> {
    let target = lock_target(txn, owner, session, &action).await?;
    let (row, parent_index, now) = lock_tool_call_on(host, txn, session, call).await?;
    let parent = session.conversation[parent_index]
        .data_envelope
        .as_ref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let mut awaiting_review = None;
    let (content, source) = match action {
        Action::Create(draft) => {
            desk_diagnose_core::schedule::validate_publication(
                &draft.spec,
                now,
                draft.kind == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume,
            )
            .map_err(|_| {
                observation.reject(Stage::Preflight, InputIssue::Precondition);
                ScheduleStoreError::ToolInput(
                    desk_diagnose_core::schedule::proposal::NO_FUTURE_OCCURRENCE,
                )
            })?;
            if draft.kind == desk_agent_protocol::schedule::ScheduledTaskKind::ConversationResume {
                host.check_resume_retention(txn, &draft.spec, now)
                    .await
                    .inspect_err(|error| {
                        if matches!(error, ScheduleStoreError::ExceedsSessionRetention { .. }) {
                            observation.reject(Stage::Preflight, InputIssue::Precondition);
                        }
                    })?;
            }
            observation.stage(Stage::Preflight, StageOutcome::Passed);
            let mut task = desk_diagnose_core::future::send_phase(|| {
                super::create_task_on(host, txn, owner, &draft, now, Some(session))
            })
            .await?;
            desk_diagnose_core::future::send_phase(|| {
                host.record_prompt_retention(
                    txn,
                    &mut task,
                    session,
                    &session.conversation[parent_index],
                )
            })
            .await?;
            // The proposed title, rule and prompt are the proposing output's
            // text; an output that was not retained leaves them unproven.
            let source = desk_diagnose_core::future::send_phase(|| {
                host.skill_proposal_source(txn, session, &session.conversation[parent_index])
            })
            .await?
            .and_then(|(_, model_source)| {
                desk_diagnose_core::model_content::StoredTextSource::model_output(
                    &session.actor_id,
                    model_source,
                    Vec::new(),
                )
            });
            task = super::field_sources::record_on(
                txn,
                task,
                &super::field_sources::ScheduleFieldSources::all(source),
            )
            .await?;
            let message = if task.kind == "conversation_resume" {
                // A continuation needs no approval: enable it in this turn's
                // transaction without writing to the running session.
                task = activate_on(host, txn, owner, &task, false).await?;
                "The conversation timer is enabled. It grants no tool permissions; its run approves device actions one at a time."
            } else {
                awaiting_review = Some(task.schedule_id.clone());
                "Submitted for a decision. This turn ends now; when the AI reviewer or the owner decides in the application, the server continues this requirement and this result shows the decision. Do not report success before it. No extra chat confirmation is needed. No tool permission is granted; every run approves device actions one at a time."
            };
            // Ids, states and fixed text answering this call.
            let text = serde_json::json!({"schedule_id":task.schedule_id,"kind":task.kind,"state":task.status,
                "next_run_at_utc_ms":task.next_run_at,"awaiting_decision":awaiting_review.is_some(),
                "message":message}).to_string();
            (
                desk_diagnose_core::model_content::ModelContent::builder()
                    .arg(&desk_diagnose_core::model_content::ModelArg::of_call(
                        call,
                        Some(parent),
                        text,
                    ))
                    .build(),
                ServerResultSource::ScheduleProposal,
            )
        }
        Action::List { after, limit } => {
            observation.stage(Stage::Preflight, StageOutcome::Passed);
            (
                list(
                    txn,
                    owner,
                    session,
                    after,
                    limit,
                    now,
                    &parent.allowed_destinations,
                )
                .await?,
                ServerResultSource::ScheduleQuery,
            )
        }
        Action::Cancel {
            expected_revision, ..
        } => (
            cancel(
                txn,
                target,
                expected_revision,
                now,
                observation,
                &parent.allowed_destinations,
            )
            .await?,
            ServerResultSource::ScheduleCancellation,
        ),
    };
    let envelope = desk_diagnose_core::model_message_labels::composed_envelope(
        &content,
        Some(parent),
        &call.id,
        source,
        false,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let receipt_id = format!("{}{}", ProposalKind::Schedule.receipt_prefix(), call.id);
    let next = commit_tool_receipt_on(
        host,
        txn,
        row,
        session,
        ToolReceipt {
            message_id: &receipt_id,
            call_id: &call.id,
            content: content.text().into(),
            envelope,
            stored_roots: content.stored_roots(),
            awaiting: awaiting_review.map(ProposalReviewRef::schedule),
            now,
        },
    )
    .await?;
    Ok((receipt_id, next))
}

/// Locks the session row of a model tool call under its exact version and
/// lease, and checks that the call is the open call of one assistant message.
/// Returns the row, the index of that message and the database clock.
pub(crate) async fn lock_tool_call_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    call: &ToolCall,
) -> Result<(SessionRow<H::Row>, usize, i64), ScheduleStoreError> {
    let locked = host
        .touch_session(
            txn,
            TouchColumn::Version,
            SessionGuard {
                conversation: Some(&session.conversation_id),
                actor: Some(&session.actor_id),
                device: Some(&session.device_id),
                version: Some(session.version),
                lease_token: Some(
                    i64::try_from(session.lease_token).map_err(|_| ScheduleStoreError::Invalid)?,
                ),
                ..SessionGuard::any()
            },
        )
        .await?;
    if locked != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &session.conversation_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let now = authority_now(txn).await?;
    if row.state_json
        != session
            .encode_json_for_storage()
            .map_err(|_| ScheduleStoreError::Invalid)?
        || row.lease_deadline_ms.is_none_or(|deadline| deadline <= now)
        || !row.personal
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let parents: Vec<usize> = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|tool| {
                    tool.id == call.id
                        && tool.name == call.name
                        && tool.arguments_json == call.arguments_json
                })
        })
        .map(|(index, _)| index)
        .collect();
    if parents.len() != 1 || !session.unclosed_tool_call_ids().contains(&call.id) {
        return Err(ScheduleStoreError::Conflict);
    }
    if desk_diagnose_core::model_controls::is_control(&call.name) {
        host.validate_model_control(txn, session, call)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    if session.conversation[parents[0]].data_envelope.is_none() {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok((row, parents[0], now))
}

/// The labelled tool result a proposal or management transaction stores.
pub(crate) struct ToolReceipt<'a> {
    pub message_id: &'a str,
    pub call_id: &'a str,
    pub content: String,
    pub envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    /// Roots of stored text the content repeats.
    pub stored_roots: Vec<desk_diagnose_core::model_content::StoredRoot>,
    /// The proposal this turn now waits for; its review opens first.
    pub awaiting: Option<ProposalReviewRef>,
    pub now: i64,
}

/// Appends the receipt to `session` and writes it under the locked row's
/// version and lease. A waiting proposal opens its default AI review first.
pub(crate) async fn commit_tool_receipt_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    mut row: SessionRow<H::Row>,
    session: &PersistedAgentSession,
    receipt: ToolReceipt<'_>,
) -> Result<PersistedAgentSession, ScheduleStoreError> {
    let now = receipt.now;
    let mut next = session.clone();
    if receipt.awaiting.is_some()
        && host
            .open_for_proposal_review(
                txn,
                &row.raw,
                &mut next,
                u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
            )
            .await?
    {
        // The audit event advanced the row; the receipt follows it.
        row = host
            .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, false)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
    }
    let mut message =
        ChatMessage::tool_result(receipt.message_id, receipt.call_id, receipt.content);
    message.turn_id = next.current_turn_id.clone();
    message.set_label_and_roots(receipt.envelope, receipt.stored_roots);
    next.conversation.push(message);
    next.version = next
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp = timestamp(now)?;
    next.updated_at = timestamp.to_rfc3339();
    let state_json = next
        .encode_json_for_storage()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let changed = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                ..SessionGuard::any()
            },
            SessionPatch {
                version: Some(next.version),
                state_json: Some(&state_json),
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(next)
}
