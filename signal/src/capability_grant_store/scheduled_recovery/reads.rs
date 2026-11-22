//! Interrupted reads have unavailable results, not proof of non-execution.
use super::*;
use crate::entity::agent_exec_task;
use desk_agent_protocol::capability_provider::CapabilityEffect;

pub(super) async fn close_untracked_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    matched: &mut BTreeSet<String>,
) -> Result<(), DbErr> {
    let registry = desk_diagnose_core::ai_agent::ai_agent_provider_registry();
    let turn = session.current_turn_id.clone().ok_or_else(invalid)?;
    // A continued step's calls keep the message of the turn that issued them.
    // Exactly the released step's own calls of that message are collected.
    let released = session
        .released_step_calls()
        .map(|(issuing, ids)| (issuing.to_owned(), ids));
    let calls: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| {
            let turn_match = message.turn_id.as_deref() == Some(turn.as_str());
            let step_ids = released
                .as_ref()
                .filter(|(issuing, _)| *issuing == message.message_id)
                .map(|(_, ids)| ids);
            message
                .tool_calls
                .iter()
                .filter(move |call| {
                    turn_match || step_ids.is_some_and(|ids| ids.contains(&call.id))
                })
                .map(move |call| {
                    (
                        message.data_envelope.clone(),
                        call.clone(),
                        step_ids.is_some() && !turn_match,
                    )
                })
        })
        .filter(|(_, call, _)| !matched.contains(&call.id))
        .collect();
    let open: BTreeSet<_> = session.unclosed_tool_call_ids().into_iter().collect();
    for (parent, call, step_call) in calls {
        // A step call that already has its result was written by the core.
        if step_call && !open.contains(&call.id) {
            if !matched.insert(call.id) {
                return Err(invalid());
            }
            continue;
        }
        if session.agent_role.binding().is_some() && !open.contains(&call.id) {
            if !matched.insert(call.id) {
                return Err(invalid());
            }
            continue;
        }
        let capability = registry.capability_for_tool(&call.name);
        let internal = session.agent_role.binding().is_some()
            && matches!(
                call.name.as_str(),
                "request_permissions"
                    | "list_capabilities"
                    | "describe_tools"
                    | "list_scheduled_tasks"
            );
        let readable = capability.is_some_and(|capability| {
            matches!(
                capability.wire.effect,
                CapabilityEffect::ReadDevice
                    | CapabilityEffect::ReadFile
                    | CapabilityEffect::ReadExternal
                    | CapabilityEffect::CaptureScreen
            )
        });
        // Children may close an unavailable mutation result only as unknown
        // evidence. Completion facts retain it as incomplete; no replay occurs.
        let unavailable_mutation = session.agent_role.binding().is_some()
            && capability.is_some_and(|capability| capability.wire.effect.is_side_effecting());
        if (!internal && !readable && !unavailable_mutation)
            || session
                .conversation
                .iter()
                .flat_map(|message| &message.tool_calls)
                .filter(|other| other.id == call.id)
                .count()
                != 1
            || !matched.insert(call.id.clone())
        {
            return Err(invalid());
        }
        if agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(&session.conversation_id))
            .filter(agent_exec_task::Column::ToolCallId.eq(&call.id))
            .one(txn)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        if !open.contains(&call.id) {
            continue;
        }
        let id = stable_id(
            "scheduled-read-unavailable",
            &format!(
                "{}:{turn}:{}:{}",
                session.conversation_id, session.lease_token, call.id
            ),
        );
        if session
            .conversation
            .iter()
            .any(|message| message.message_id == id)
        {
            return Err(invalid());
        }
        let text: &'static str = "The executor stopped before a durable read result was available. The result is unavailable; this does not establish whether the read completed. Do not automatically retry this interrupted turn.";
        let mut result = ChatMessage::tool_result(id, &call.id, text);
        result.turn_id = Some(turn.clone());
        result = result
            .with_content(
                &desk_diagnose_core::model_content::ModelContent::constant(text),
                desk_diagnose_core::model_content::Labelling {
                    issuing: parent.as_ref(),
                    call_id: &call.id,
                    source: if unavailable_mutation {
                    desk_diagnose_core::model_message_labels::ServerResultSource::ChildMutationResultUnavailable
                } else {
                    desk_diagnose_core::model_message_labels::ServerResultSource::ScheduledReadUnavailable
                },
                    request_only: false,
                },
            ).map_err(|_| invalid())?;
        session.conversation.push(result);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::chat::ToolCallRef;

    /// A continued step's issuing message keeps the turn that issued it. Its
    /// earlier calls already have results and are not collected; only the
    /// released step's own calls are, and only an unanswered one is closed.
    #[tokio::test]
    async fn a_released_step_collects_only_its_own_calls() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let mut session = PersistedAgentSession::new(
            "run-1",
            "1",
            "device-1",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-22T00:00:00Z",
        );
        let call = |id: &str, name: &str| ToolCallRef {
            id: id.into(),
            name: name.into(),
            arguments_json: "{}".into(),
        };
        let mut step = ChatMessage::agent_tool_calls_with_replay(
            "step",
            "",
            vec![
                call("write", "create_text_file"),
                call("held", "read_text_file"),
                call("next", "read_text_file"),
            ],
            desk_diagnose_core::replay::ReplayDisposition::legacy_unknown(),
        );
        step.turn_id = Some("turn-1".into());
        session.conversation.push(step);
        for (id, call) in [("write-result", "write"), ("held-result", "held")] {
            let mut result = ChatMessage::tool_result(id, call, "done");
            result.turn_id = Some("turn-1".into());
            session.conversation.push(result);
        }
        session.current_turn_id = Some("turn-2".into());
        session.released_permission_call =
            Some(Box::new(desk_diagnose_core::session::AwaitingPermission {
                tool_call_id: "held".into(),
                request_id: "request-1".into(),
                issuing_message_id: "step".into(),
                pending_call_ids: vec!["next".into()],
                dispatching: Some("next".into()),
                ..Default::default()
            }));
        let txn = crate::db::begin_write(&db, crate::entity::agent_session::Entity)
            .await
            .unwrap();
        let mut matched = BTreeSet::new();
        close_untracked_on(&txn, &mut session, &mut matched)
            .await
            .unwrap();
        assert_eq!(
            matched,
            BTreeSet::from(["held".to_owned(), "next".to_owned()])
        );
        assert!(session.unclosed_tool_call_ids().is_empty());
    }
}
