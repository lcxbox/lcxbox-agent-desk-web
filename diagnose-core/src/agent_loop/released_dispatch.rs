//! Run the call an approval resume released, before the model is called again.
//!
//! The model issued this exact call once. Its approval decides whether it runs;
//! the model never has to repeat it. Every host authorization and dispatch check
//! still applies, and a call the approval no longer covers is closed, never
//! proposed again.
use super::*;

/// Closed result when the approval no longer covers the released call.
const NOT_COVERED: &str = "not executed: the approval no longer covers this call (it expired, was used up, or its target changed); call it again if still needed";

/// Dispatch the released call, record its only result and clear the release.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    exposed: &[&RegisteredTool],
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let Some(released) = session.released_permission_call.clone() else {
        return Ok(None);
    };
    let outcome = match issued(session, &released.tool_call_id) {
        Some((index, call, parent)) if !answered_after(session, index, &call.id) => {
            let context = permission_proposal::ProposalContext {
                remaining: &[],
                parent_envelope: parent.as_ref(),
                ..context
            };
            match dispatch(
                deps, session, turn_id, exposed, &released, &call, context, mint, hinted, sink,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    // Nothing reads the release once dispatch has returned. Clear
                    // it first, so settlement never takes a call that may have
                    // run for one that did not.
                    session.released_permission_call = None;
                    if !answered_after(session, index, &call.id) {
                        record_failure(session, exposed, &call, context, mint, sink, &error)?;
                    }
                    return Err(error);
                }
            }
        }
        _ => None,
    };
    session.released_permission_call = None;
    deps.session_seam.save(session).await?;
    Ok(outcome)
}

/// The latest assistant message issuing `tool_call_id`: its index, the call and
/// the message's envelope. Providers may reuse call ids across turns, so only
/// results recorded after this message answer it.
fn issued(
    session: &crate::session::PersistedAgentSession,
    tool_call_id: &str,
) -> Option<(usize, crate::chat::ToolCall, Option<DataEnvelope>)> {
    session
        .conversation
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (message.role == ChatRole::Assistant)
                .then(|| {
                    message
                        .tool_calls
                        .iter()
                        .find(|call| call.id == tool_call_id)
                        .map(|call| {
                            (
                                index,
                                crate::chat::ToolCall {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments_json: call.arguments_json.clone(),
                                },
                                message.data_envelope.clone(),
                            )
                        })
                })
                .flatten()
        })
}

fn answered_after(
    session: &crate::session::PersistedAgentSession,
    index: usize,
    tool_call_id: &str,
) -> bool {
    session.conversation[index + 1..]
        .iter()
        .any(|message| message.tool_call_id.as_deref() == Some(tool_call_id))
}

/// The only result of a released call whose run ended in an error. A write may
/// already have taken effect, so it is reported as an unknown outcome to check
/// before retrying, never as a failure inviting a repeat; a read reports its
/// error. A call no longer exposed was closed (or failed to be) by `dispatch`.
fn record_failure<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    exposed: &[&RegisteredTool],
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    sink: &mut dyn TurnSink,
    error: &AgentError,
) -> Result<(), AgentError> {
    let Some(tool) = exposed.iter().find(|tool| tool.name() == call.name) else {
        return Ok(());
    };
    if matches!(tool.effect, ToolEffect::Mutating) {
        append_internal_tool_result(
            session,
            context.parent_envelope,
            mint(),
            &call.id,
            &crate::model_content::ModelContent::constant(crate::session::RECOVER_OUTCOME_UNKNOWN),
            ServerResultSource::ExecutionOutcomeUnknown,
        )?;
    } else {
        append_tool_error(
            session,
            context.parent_envelope,
            mint(),
            call,
            &crate::model_content::ToolError::host(error),
        )?;
    }
    finish_tool(session, &call.id, false, sink);
    Ok(())
}

/// Closed result when the turn failed before the released call ran.
const NOT_STARTED: &str =
    "not executed: the turn ended before this approved call ran; call it again if still needed";

/// Settle a release whose turn failed before `run` started the call: the call
/// gets its only result, and the release no longer holds the input slot. `run`
/// clears the release on every path that started the call.
pub(super) fn close_unstarted(
    session: &mut crate::session::PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    let Some(released) = session.released_permission_call.take() else {
        return Ok(());
    };
    let Some((index, call, parent)) = issued(session, &released.tool_call_id) else {
        return Ok(());
    };
    if answered_after(session, index, &call.id) {
        return Ok(());
    }
    // Settlement has no turn message counter; this id is unique per call.
    let message_id = crate::native_ids::stable(
        crate::native_ids::Kind::Message,
        &("released-not-started", call.id.as_str()),
    );
    append_internal_tool_result(
        session,
        parent.as_ref(),
        message_id,
        &call.id,
        &crate::model_content::ModelContent::constant(NOT_STARTED),
        ServerResultSource::PermissionResumeNotExecuted,
    )?;
    finish_tool(session, &call.id, false, sink);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    exposed: &[&RegisteredTool],
    released: &crate::session::AwaitingPermission,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if let Some(refusal) = refusal(session, released) {
        close(session, call, context, mint, sink, refusal)?;
        return Ok(None);
    }
    let Some(tool) = exposed
        .iter()
        .find(|tool| tool.name() == call.name)
        .copied()
    else {
        close(session, call, context, mint, sink, NOT_COVERED)?;
        return Ok(None);
    };
    let resolved = match crate::ui_model_ids::resolve_session_call_with_context(
        call,
        session,
        if crate::ui_model_ids::needs_resolution(&call.name) {
            current_unix_ms(deps.clock)?
        } else {
            0
        },
        deps.tools.original_read_context(),
    ) {
        Ok(resolved) => resolved,
        Err(_) => {
            close(session, call, context, mint, sink, NOT_COVERED)?;
            return Ok(None);
        }
    };
    let observation = crate::model_observability::tool::ToolObservation::default();
    match tool.effect {
        ToolEffect::ReadOnly | ToolEffect::ExternalRead | ToolEffect::ActionPreview => {
            crate::future::boxed(|| {
                read_call::run(
                    deps,
                    session,
                    &resolved,
                    read_call::ReadCall {
                        turn_id,
                        tool,
                        selected_device_read: crate::request_devices::is_selected_read_tool(
                            tool, session,
                        ),
                        selected_browser: crate::request_devices::is_selected_action_tool(
                            tool, session,
                        ),
                        replay: true,
                    },
                    context,
                    mint,
                    hinted,
                    &observation,
                    sink,
                )
            })
            .await
        }
        ToolEffect::Mutating => {
            let mut halted = None;
            let mut grant_required = None;
            let outcome = crate::future::boxed(|| {
                run_mutating(
                    deps,
                    session,
                    turn_id,
                    &resolved,
                    &[],
                    mint,
                    &mut halted,
                    &mut grant_required,
                    sink,
                    &observation,
                )
            })
            .await?;
            if grant_required.is_some() {
                close(session, call, context, mint, sink, NOT_COVERED)?;
            }
            Ok(outcome)
        }
        _ => {
            close(session, call, context, mint, sink, NOT_COVERED)?;
            Ok(None)
        }
    }
}

/// The recorded refusal of this call's approval item, if any.
fn refusal(
    session: &crate::session::PersistedAgentSession,
    released: &crate::session::AwaitingPermission,
) -> Option<&'static str> {
    let item = session
        .permission_decisions
        .iter()
        .find(|decision| decision.request_id == released.request_id)
        .and_then(|decision| {
            decision
                .items
                .iter()
                .find(|item| item.item_id == released.tool_call_id)
        });
    match item {
        Some(item) if item.approved => None,
        Some(item) if item.actor == crate::dynamic_run::PermissionDecisionActor::Owner => {
            Some("not executed: the owner refused this call")
        }
        Some(item) if item.actor == crate::dynamic_run::PermissionDecisionActor::AiApproval => {
            Some(
                "not executed: AI review refused this call; see the approval record for the reason",
            )
        }
        _ => Some("not executed: this call was not approved"),
    }
}

/// A released call that cannot run under its approval is closed, not proposed.
pub(super) fn close_unauthorized<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    close(session, call, context, mint, sink, NOT_COVERED)
}

fn close<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    sink: &mut dyn TurnSink,
    text: &'static str,
) -> Result<(), AgentError> {
    append_internal_tool_result(
        session,
        context.parent_envelope,
        mint(),
        &call.id,
        &crate::model_content::ModelContent::constant(text),
        ServerResultSource::PermissionResumeNotExecuted,
    )?;
    finish_tool(session, &call.id, false, sink);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ToolCallRef};
    use crate::seam::NullTurnSink;

    fn session_with_call(name: &str) -> crate::session::PersistedAgentSession {
        let mut session = crate::session::PersistedAgentSession::new(
            "run",
            "actor",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-17T00:00:00Z",
        );
        session.conversation.push(ChatMessage::assistant_tool_calls(
            "assistant",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: name.into(),
                arguments_json: "{}".into(),
            }],
        ));
        session
    }

    fn results(session: &crate::session::PersistedAgentSession) -> Vec<&str> {
        session
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some("call"))
            .map(|message| message.text.as_str())
            .collect()
    }

    fn tool(name: &str, effect: ToolEffect) -> RegisteredTool {
        RegisteredTool {
            spec: crate::chat::ToolSpec {
                name: name.into(),
                description: String::new(),
                parameters_schema: serde_json::json!({}),
            },
            required_device_capability: None,
            effect,
        }
    }

    fn context() -> permission_proposal::ProposalContext<'static> {
        permission_proposal::ProposalContext {
            authority: None,
            request_devices: &[],
            selected_reads: &[],
            step_inventory: None,
            permission_candidates: &[],
            remaining: &[],
            parent_envelope: None,
        }
    }

    fn error() -> AgentError {
        AgentError {
            kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
            message: "the read result could not be recorded".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }
    }

    #[test]
    fn a_release_that_never_ran_is_closed_once_and_frees_the_slot() {
        let mut session = session_with_call("read_text_file");
        session.pause_for_permission("call", "request");
        session.released_permission_call = session.awaiting_permission.take();
        close_unstarted(&mut session, &mut NullTurnSink).unwrap();
        assert!(session.released_permission_call.is_none());
        assert_eq!(results(&session), [NOT_STARTED]);
        assert!(session.unclosed_tool_call_ids().is_empty());
        // A call that already has its result is not answered again.
        session.released_permission_call = Some(crate::session::AwaitingPermission {
            tool_call_id: "call".into(),
            request_id: "request".into(),
        });
        close_unstarted(&mut session, &mut NullTurnSink).unwrap();
        assert!(session.released_permission_call.is_none());
        assert_eq!(results(&session).len(), 1);
    }

    #[test]
    fn a_failed_released_read_reports_its_error_and_a_write_an_unknown_outcome() {
        let call = crate::chat::ToolCall {
            id: "call".into(),
            name: "read_text_file".into(),
            arguments_json: "{}".into(),
        };
        let mut ids = 0;
        let mut mint = || {
            ids += 1;
            format!("m{ids}")
        };

        let mut read = session_with_call("read_text_file");
        let exposed = tool("read_text_file", ToolEffect::ReadOnly);
        record_failure(
            &mut read,
            &[&exposed],
            &call,
            context(),
            &mut mint,
            &mut NullTurnSink,
            &error(),
        )
        .unwrap();
        let [text] = results(&read)[..] else {
            panic!("one result")
        };
        assert!(
            text.contains("the read result could not be recorded"),
            "{text}"
        );

        // A write may have taken effect: never report it as a plain failure.
        let mut write = session_with_call("read_text_file");
        let exposed = tool("read_text_file", ToolEffect::Mutating);
        record_failure(
            &mut write,
            &[&exposed],
            &call,
            context(),
            &mut mint,
            &mut NullTurnSink,
            &error(),
        )
        .unwrap();
        assert_eq!(results(&write), [crate::session::RECOVER_OUTCOME_UNKNOWN]);

        // A call no longer exposed was handled by dispatch itself.
        let mut gone = session_with_call("read_text_file");
        record_failure(
            &mut gone,
            &[],
            &call,
            context(),
            &mut mint,
            &mut NullTurnSink,
            &error(),
        )
        .unwrap();
        assert!(results(&gone).is_empty());
    }
}
