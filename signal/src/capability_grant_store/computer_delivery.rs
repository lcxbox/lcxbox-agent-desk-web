//! Original receipt delivery after the foreground owner has released its lease.

use super::computer_background::{bound, deadlines};
use super::computer_binding::original_on;
use super::computer_completion::{OriginalResult, terminal_result};
use super::*;
use crate::agent_session_store::EventAppend;
use desk_diagnose_core::{
    chat::ChatRole,
    session::{AgentSessionSurface, ExecutionState, PendingAutoTrigger, RecoveryVerdict, WorkKind},
};
use sea_orm::{QueryOrder, QuerySelect};

fn invalid() -> DbErr {
    DbErr::Custom("invalid original completion destination".into())
}

fn refresh_control_label(session: &mut PersistedAgentSession, call: &str) -> Result<(), DbErr> {
    let parent = session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|candidate| candidate.id == call)
        })
        .and_then(|message| message.data_envelope.clone())
        .ok_or_else(invalid)?;
    let indexes: Vec<_> = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == ChatRole::Tool && message.tool_call_id.as_deref() == Some(call)
        })
        .map(|(index, _)| index)
        .collect();
    if indexes.len() != 1 {
        return Err(invalid());
    }
    let message = &mut session.conversation[indexes[0]];
    // Only a recovery placeholder is server text; anything else stays
    // unverified, so a real output left here never registers as a control.
    use desk_diagnose_core::{
        model_content::{Labelling, ModelContent, Unverified},
        model_message_labels::ServerResultSource,
    };
    let (content, source) = match desk_diagnose_core::session::recovery_placeholder(&message.text) {
        Some(text) => (
            ModelContent::constant(text),
            ServerResultSource::ExecutionPlaceholder,
        ),
        None => (
            ModelContent::builder()
                .unverified(&Unverified::new(message.text.clone()))
                .build(),
            ServerResultSource::ExecutionStatus,
        ),
    };
    *message = message
        .clone()
        .with_content(
            &content,
            Labelling {
                issuing: Some(&parent),
                call_id: call,
                source,
                request_only: false,
            },
        )
        .map_err(|_| invalid())?;
    Ok(())
}

pub(super) fn validate_destination(
    session: &PersistedAgentSession,
    original: &OriginalResult,
) -> Result<bool, DbErr> {
    let action = &original.receipt.action;
    let call = &original.original_call_id;
    if session.surface != AgentSessionSurface::AiAgent
        || session
            .execution_state
            .tasks()
            .into_iter()
            .any(|current| current.execution_id == action.execution_id && current != action)
    {
        return Err(invalid());
    }
    let mut results = session
        .conversation
        .iter()
        .filter(|message| message.message_id == original.work.completion_event_id);
    if let Some(existing) = results.next() {
        let recovered_image = session.trigger_origin
            == desk_diagnose_core::session::TriggerOrigin::ScheduledTask
            && original.output.image_data_url.is_some()
            && existing.image_data_url.is_none()
            && existing.text
                == desk_diagnose_core::image_input::recovered_result_text(
                    &original.output.content,
                    original.output.image_data_url.as_deref(),
                )
                .map_err(|_| invalid())?;
        if results.next().is_some()
            || (!recovered_image
                && (!desk_diagnose_core::conversation_attachment::delivery::matches_original(
                    existing,
                    &original.output.content,
                    Some(&original.receipt.envelope),
                ) || existing.image_data_url != original.output.image_data_url))
            || (existing.raw_result.is_none()
                && existing.data_envelope.as_ref() != Some(&original.receipt.envelope))
            || existing.tool_call_id.as_deref() != Some(call)
            || !matches!(existing.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            || existing
                .background_task_id
                .as_ref()
                .is_some_and(|id| id != &action.action_request_id)
        {
            return Err(invalid());
        }
        return Ok(true);
    }
    let proposals: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| message.tool_calls.iter())
        .filter(|candidate| candidate.id == *call)
        .collect();
    if proposals.len() != 1 {
        return Err(invalid());
    }
    let execution = session
        .execution_state
        .execution(&action.execution_id)
        .unwrap_or_default();
    if let ExecutionState::OutcomeUnknown {
        action: current,
        placeholder_message_id,
        ..
    } = &execution
        && current == action
    {
        let anchors: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| message.message_id == *placeholder_message_id)
            .collect();
        if anchors.len() != 1
            || anchors[0].tool_call_id.as_deref() != Some(call)
            || !matches!(anchors[0].role, ChatRole::Tool | ChatRole::UntrustedOutput)
            || anchors[0]
                .background_task_id
                .as_ref()
                .is_some_and(|id| id != &action.action_request_id)
        {
            return Err(invalid());
        }
    }
    Ok(false)
}

impl SignalCapabilityGrantStore {
    /// Persist the exact original bytes and their follow-up intent in one CAS.
    /// Busy or invalid destinations leave delivery pending for a later scan.
    pub(crate) async fn deliver_computer_result(
        &self,
        generation: &str,
    ) -> Result<EventAppend, DbErr> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity).await?;
        let result = async {
            let computer_original = original_on(&txn, generation).await?;
            let (outbox, work, payload) = computer_original.as_ref();
            let binding = bound(outbox, work, payload)?;
            let now = desk_agent_data_store::datetime::utc_now();
            let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| invalid())?;
            if now_ms < deadlines(outbox, &binding)?.0 {
                return Ok(EventAppend::Busy);
            }
            let original = terminal_result(outbox, work.clone(), payload)?.ok_or_else(invalid)?;
            let Some(row) = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&original.work.conversation_id))
                .one(&txn)
                .await?
            else {
                return Err(invalid());
            };
            let mut session =
                PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
            if row.actor_id != original.work.actor_id
                || row.device_id != original.work.target_device_id
                || session.actor_id != row.actor_id
                || session.device_id != row.device_id
                || session.conversation_id != row.conversation_id
            {
                return Err(invalid());
            }
            if session.turn_state.is_active() {
                return Ok(EventAppend::Busy);
            }
            session.version = row.version;
            let already = validate_destination(&session, &original)?;
            // The scheduler alone restores task receipts. A publisher can only
            // acknowledge exact bytes already present, without a model follow-up.
            if session.trigger_origin == desk_diagnose_core::session::TriggerOrigin::ScheduledTask {
                return Ok(if already && session.pending_auto_triggers.is_empty() {
                    EventAppend::AlreadyPresent
                } else {
                    EventAppend::Busy
                });
            }
            if !session.unclosed_tool_call_ids().is_empty() {
                return Ok(EventAppend::Busy);
            }
            let action = &original.receipt.action;
            let appended = session.apply_completion_with_envelope(
                &original.work.completion_event_id,
                generation,
                &original.original_call_id,
                &action.action_request_id,
                &original.output.content,
                Some(original.receipt.envelope.clone()),
                original.output.format,
                now.to_rfc3339(),
            );
            if !appended {
                if session.execution_state.contains(action) {
                    session.execution_state.remove(action);
                } else {
                    return Ok(EventAppend::AlreadyPresent);
                }
            }
            if !already
                && binding.origin.turn_fence.input_revision == session.input_revision
                && binding
                    .execution
                    .as_ref()
                    .is_some_and(|execution| execution.chain_id == session.chain_id)
            {
                session.add_pending_auto_trigger(PendingAutoTrigger {
                    work_id: action.work_id,
                    kind: WorkKind::ComputerAction,
                    execution_id: generation.into(),
                    tool_call_id: original.original_call_id,
                    event_id: original.work.completion_event_id,
                    chain_id: session.chain_id.clone(),
                    resolution_org_id: None,
                    since: now.to_rfc3339(),
                });
            }
            session.version = row.version.checked_add(1).ok_or_else(invalid)?;
            let changed = agent_session::Entity::update_many()
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .col_expr(
                    agent_session::Column::StateJson,
                    Expr::value(session.encode_json_for_storage().map_err(|_| invalid())?),
                )
                .col_expr(agent_session::Column::Version, Expr::value(session.version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            Ok(EventAppend::Appended)
        }
        .await;
        match result {
            Ok(outcome) => {
                txn.commit().await?;
                Ok(outcome)
            }
            Err(error) => {
                txn.rollback().await?;
                Err(error)
            }
        }
    }

    /// Recover only a correlated Computer Action. The existing legacy command
    /// recovery remains responsible when this returns false.
    pub(crate) async fn recover_computer_session(
        &self,
        session: &mut PersistedAgentSession,
        now: &str,
    ) -> Result<bool, DbErr> {
        if session.surface != AgentSessionSurface::AiAgent {
            return Ok(false);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity).await?;
        let result = async {
            let rows = agent_action_item::Entity::find()
                .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
                .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
                .filter(agent_action_item::Column::IsSideEffecting.eq(true))
                .order_by_desc(agent_action_item::Column::Id)
                .all(&txn)
                .await?;
            let unclosed = session.unclosed_tool_call_ids();
            for row in rows {
                let matches_open = session.current_turn_id.as_deref() == Some(&row.turn_id)
                    && unclosed.iter().any(|call| {
                        desk_diagnose_core::native_ids::background_action(
                            &session.conversation_id,
                            &row.turn_id,
                            call,
                        ) == row.action_request_id
                    });
                let matches_current = session.execution_state.tasks().into_iter().any(|action| {
                    action.work_id == row.id && action.kind == WorkKind::ComputerAction
                });
                if !matches_open && !matches_current {
                    continue;
                }
                if row.actor_id != session.actor_id || row.target_device_id != session.device_id {
                    return Err(invalid());
                }
                let outbox = agent_capability_dispatch_outbox::Entity::find()
                    .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
                    .one(&txn)
                    .await?;
                let Some(outbox) = outbox else {
                    continue;
                };
                if !matches!(
                    outbox.state.as_str(),
                    DISPATCH_OUTBOX_SENDING
                        | DISPATCH_OUTBOX_OUTCOME_UNKNOWN
                        | DISPATCH_OUTBOX_COMPLETED
                ) {
                    continue;
                }
                if outbox.computer_binding_json.is_none() {
                    session.recover_session(RecoveryVerdict::InterruptedUnknown, now);
                    return Ok(true);
                }
                let computer_original = original_on(&txn, &outbox.dispatch_id).await?;
                let (outbox, work, payload) = computer_original.as_ref();
                let binding = bound(outbox, work, payload)?;
                let action = desk_diagnose_core::session::ActionIdentity::new(
                    work.id,
                    &payload.call_id,
                    &payload.dispatch_id,
                    WorkKind::ComputerAction,
                );
                if matches_current && !session.execution_state.contains(&action) {
                    return Err(invalid());
                }
                if matches_open {
                    session.recover_session(
                        RecoveryVerdict::OutcomeUnknown {
                            tool_call_id: binding.origin.tool_call_id.clone(),
                            action,
                        },
                        now,
                    );
                } else {
                    let execution = session.execution_state.clone();
                    session.recover_session(RecoveryVerdict::NotExecuted, now);
                    session.execution_state = execution;
                    session.mark_execution_unknown(
                        &payload.dispatch_id,
                        &binding.origin.tool_call_id,
                        now,
                    );
                }
                for call in &unclosed {
                    refresh_control_label(session, call)?;
                }
                if !matches_open {
                    refresh_control_label(session, &binding.origin.tool_call_id)?;
                }
                // Recovery records uncertainty only. Original completion delivery
                // validates and applies the native receipt in its own transaction.
                return Ok(true);
            }
            Ok(false)
        }
        .await;
        txn.rollback().await?;
        result
    }

    pub(crate) async fn publish_computer_results_once(&self) -> Result<(), DbErr> {
        desk_diagnose_core::future::boxed(|| async {
            self.reconcile_computer_backgrounds_once().await
        })
        .await?;
        // Bound this scan to its starting high-water mark. Invalid or busy rows
        // in one page must not permanently starve all later original receipts.
        let upper = desk_diagnose_core::future::boxed(|| async {
            agent_action_item::Entity::find()
                .select_only()
                .column(agent_action_item::Column::Id)
                .order_by_desc(agent_action_item::Column::Id)
                .limit(1)
                .into_tuple::<i64>()
                .one(&self.db)
                .await
        })
        .await?
        .unwrap_or(0);
        let mut cursor = i64::MIN;
        loop {
            let rows = desk_diagnose_core::future::boxed(|| async {
                agent_action_item::Entity::find()
                    .filter(agent_action_item::Column::Id.gt(cursor))
                    .filter(agent_action_item::Column::Id.lte(upper))
                    .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
                    .filter(
                        agent_action_item::Column::CompletionDeliveryState
                            .is_in(["pending", "published"]),
                    )
                    .filter(agent_action_item::Column::ResultSchemaVersion.eq(2))
                    .filter(
                        agent_action_item::Column::Status
                            .is_in([CAPABILITY_WORK_SUCCEEDED, CAPABILITY_WORK_FAILED]),
                    )
                    .order_by_asc(agent_action_item::Column::Id)
                    .limit(128)
                    .all(&self.db)
                    .await
            })
            .await?;
            let Some(last) = rows.last() else {
                break;
            };
            cursor = last.id;
            let sessions =
                crate::agent_session_store::SignalAgentSessionStore::new(self.db.clone());
            let legacy =
                crate::agent_background_task_store::SignalBackgroundTaskStore::new(self.db.clone());
            for row in rows {
                let result = async {
                    let outbox = desk_diagnose_core::future::boxed(|| async {
                        agent_capability_dispatch_outbox::Entity::find()
                            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
                            .one(&self.db)
                            .await
                    })
                    .await?
                    .ok_or_else(invalid)?;
                    if let Some(session) = desk_diagnose_core::future::boxed(|| async {
                        agent_session::Entity::find()
                            .filter(agent_session::Column::ConversationId.eq(&row.conversation_id))
                            .one(&self.db)
                            .await
                    })
                    .await?
                    {
                        desk_diagnose_core::future::boxed(|| async {
                            sessions
                                .settle_lapsed_session(
                                    &session,
                                    desk_agent_data_store::datetime::utc_now(),
                                )
                                .await
                        })
                        .await
                        .map_err(|_| invalid())?;
                    }
                    let delivered = if row.completion_delivery_state == "published" {
                        EventAppend::AlreadyPresent
                    } else {
                        desk_diagnose_core::future::boxed(|| async {
                            self.deliver_computer_result(&outbox.dispatch_id).await
                        })
                        .await?
                    };
                    if matches!(
                        delivered,
                        EventAppend::Appended | EventAppend::AlreadyPresent
                    ) {
                        desk_diagnose_core::future::boxed(|| async {
                            agent_action_item::Entity::update_many()
                                .col_expr(
                                    agent_action_item::Column::CompletionDeliveryState,
                                    Expr::value("published"),
                                )
                                .filter(agent_action_item::Column::Id.eq(row.id))
                                .filter(
                                    agent_action_item::Column::CompletionDeliveryState
                                        .eq("pending"),
                                )
                                .exec(&self.db)
                                .await
                        })
                        .await?;
                        if desk_diagnose_core::future::boxed(|| async {
                            legacy
                                .follow_up_completion(
                                    &sessions,
                                    &row,
                                    &desk_utils::datetime::rfc3339(jiff::Timestamp::now()),
                                    WorkKind::ComputerAction,
                                )
                                .await
                        })
                        .await?
                        {
                            desk_diagnose_core::future::boxed(|| async {
                                self.consume_computer_result(
                                    &row.completion_event_id,
                                    &row.conversation_id,
                                    &row.actor_id,
                                    &row.target_device_id,
                                )
                                .await
                            })
                            .await?;
                        }
                    }
                    Ok::<(), DbErr>(())
                }
                .await;
                if result.is_err() {
                    log::warn!(
                        "[computer-action] original result remains pending after delivery failure"
                    );
                }
            }
        }
        Ok(())
    }

    async fn reconcile_computer_backgrounds_once(&self) -> Result<(), DbErr> {
        let outboxes = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::ComputerBackgroundJson.is_not_null())
            .filter(
                agent_capability_dispatch_outbox::Column::State
                    .is_in([DISPATCH_OUTBOX_SENDING, DISPATCH_OUTBOX_OUTCOME_UNKNOWN]),
            )
            .order_by_asc(agent_capability_dispatch_outbox::Column::Id)
            .all(&self.db)
            .await?;
        for outbox in outboxes {
            let result = async {
                let txn = self.db.begin().await?;
                let original = original_on(&txn, &outbox.dispatch_id).await;
                txn.rollback().await?;
                let computer_original = original?;
                let (outbox, work, payload) = computer_original.as_ref();
                let binding = bound(outbox, work, payload)?;
                let now = u64::try_from(jiff::Timestamp::now().as_millisecond())
                    .map_err(|_| invalid())?;
                if work.status == CAPABILITY_WORK_DISPATCHING
                    && now >= deadlines(outbox, &binding)?.1
                {
                    self.mark_dispatch_outcome_unknown(
                        &outbox.dispatch_id,
                        &payload.call_id,
                        payload.generation,
                        now,
                    )
                    .await?;
                }
                self.project_computer_unknown(&outbox.dispatch_id).await
            }
            .await;
            if result.is_err() {
                log::warn!("[computer-action] background uncertainty projection deferred");
            }
        }
        Ok(())
    }

    async fn project_computer_unknown(&self, generation: &str) -> Result<(), DbErr> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity).await?;
        let result = async {
            let computer_original = original_on(&txn, generation).await?;
            let (outbox, work, payload) = computer_original.as_ref();
            if work.status != CAPABILITY_WORK_OUTCOME_UNKNOWN {
                return Ok(());
            }
            let binding = bound(outbox, work, payload)?;
            let Some(row) = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
                .one(&txn).await? else { return Ok(()); };
            let mut session = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| invalid())?;
            if row.actor_id != work.actor_id || row.device_id != work.target_device_id
                || session.actor_id != work.actor_id || session.device_id != work.target_device_id
                || session.conversation_id != work.conversation_id
            {
                return Err(invalid());
            }
            if session.turn_state.is_active() { return Ok(()); }
            let identity = desk_diagnose_core::session::ActionIdentity::new(
                work.id, &payload.call_id, generation, WorkKind::ComputerAction
            );
            if !matches!(session.execution_state.execution(&identity.execution_id), Some(ExecutionState::Executing { action }) if action == identity) {
                return Ok(());
            }
            let now = desk_agent_data_store::datetime::utc_now();
            if !session.mark_execution_unknown(generation, &binding.origin.tool_call_id, now.to_rfc3339()) {
                return Err(invalid());
            }
            refresh_control_label(&mut session, &binding.origin.tool_call_id)?;
            session.version = row.version.checked_add(1).ok_or_else(invalid)?;
            let changed = agent_session::Entity::update_many()
                .filter(agent_session::Column::Id.eq(row.id))
                .filter(agent_session::Column::Version.eq(row.version))
                .col_expr(agent_session::Column::StateJson,
                    Expr::value(session.encode_json_for_storage().map_err(|_| invalid())?))
                .col_expr(agent_session::Column::Version, Expr::value(session.version))
                .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
                .exec(&txn).await?;
            if changed.rows_affected != 1 { return Err(invalid()); }
            Ok(())
        }.await;
        match result {
            Ok(()) => txn.commit().await,
            Err(error) => {
                txn.rollback().await?;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_recovery_placeholder_is_relabelled_as_a_control() {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "1",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: Vec::new(),
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-14T00:00:00Z",
        );
        let mut call = desk_diagnose_core::chat::ChatMessage::agent_tool_calls(
            "assistant",
            "",
            vec![desk_diagnose_core::chat::ToolCallRef {
                id: "call".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        );
        call.data_envelope = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "owner".into(),
            "run it".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope;
        session.conversation.push(call);
        session.recover_session(RecoveryVerdict::NotExecuted, "2026-11-14T00:00:01Z");
        refresh_control_label(&mut session, "call").unwrap();
        let label = |session: &PersistedAgentSession| {
            session.conversation[1]
                .data_envelope
                .clone()
                .unwrap()
                .envelope_id
        };
        assert!(label(&session).starts_with("composed-result-"));
        // A real output left on the call keeps a data label.
        session.conversation[1].text = "device output".into();
        refresh_control_label(&mut session, "call").unwrap();
        assert!(label(&session).starts_with("unverified-result-"));
    }
}
