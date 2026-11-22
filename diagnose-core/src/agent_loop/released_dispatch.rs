//! Continue the model step an approval resume released, before the model is
//! called again.
//!
//! The model issued these exact calls once. The released call runs first, if
//! its approval covers it; the step's later calls then run in order through
//! the normal per-call processing. A call requested up front runs under its
//! own decision. Every host authorization and dispatch check still applies,
//! and a call an approval no longer covers is closed, never proposed again.
use super::phases::LoopState;
use super::*;

/// Closed result when the approval no longer covers the released call.
const NOT_COVERED: &str = "not executed: the approval no longer covers this call (it expired, was used up, or its target changed); call it again if still needed";
/// Closed result of a call whose request ended without a decision.
const EXPIRED_OR_WITHDRAWN: &str = "not executed: the approval request expired or was withdrawn before a decision; call it again if still needed";
/// Closed result of the step's calls once its issuing data has expired.
const ENVELOPE_EXPIRED: &str = "not executed: the approval wait outlasted this step's data lifetime; call it again if still needed";

/// Turn-local inputs of the resume iteration that the continued step needs.
pub(super) struct Continuation<'s, 'a> {
    pub exposed: &'s Vec<&'a RegisteredTool>,
    pub raw_provider_exposed: &'s Vec<RegisteredTool>,
    pub disclosure_enabled: bool,
    pub authority: &'s Option<crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub request_devices: &'s Vec<crate::request_devices::RequestDevice>,
    pub selected_reads: &'s Vec<(RegisteredTool, bool)>,
    pub step_inventory: &'s Option<Vec<crate::capability_availability::CapabilityAvailability>>,
    pub permission_candidates: &'s [RegisteredTool],
    pub pinned_context: &'s crate::model_context::PinnedContextPolicy,
    pub skill_catalog: &'s crate::skills::SkillCatalog,
    pub planning_goal: &'s Option<Box<crate::goal::GoalRun>>,
}

/// The calls of the agent message at `issuing`, in call order.
fn issued_calls(
    session: &crate::session::PersistedAgentSession,
    issuing: usize,
) -> Vec<crate::chat::ToolCall> {
    session.conversation[issuing]
        .tool_calls
        .iter()
        .map(|call| crate::chat::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .collect()
}

/// Record `id` as the marker call being dispatched, or clear it.
pub(super) fn set_dispatching(
    session: &mut crate::session::PersistedAgentSession,
    id: Option<&str>,
) {
    if let Some(released) = session.released_permission_call.as_mut() {
        released.dispatching = id.map(str::to_owned);
    }
}

/// Run the released call, then the rest of its step, and clear the release.
pub(super) async fn run<M: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    continuation: Continuation<'_, '_>,
    state: &mut LoopState<M>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let Some(released) = session.released_permission_call.clone() else {
        return Ok(None);
    };
    let Some(issuing) = session.marker_issuing_index(&released) else {
        session.released_permission_call = None;
        deps.session_seam.save(session).await?;
        return Ok(None);
    };
    let calls = issued_calls(session, issuing);
    let parent = session.conversation[issuing].data_envelope.clone();
    let issuing_message_id = session.conversation[issuing].message_id.clone();
    // Results bind to the issuing message's data: once it has expired, none of
    // the step's calls can run.
    if parent.as_ref().is_some_and(|envelope| {
        current_unix_ms(deps.clock)
            .is_ok_and(|now| crate::model_egress::envelope_expires_by(envelope, now))
    }) {
        for id in session.close_released_unstarted(ENVELOPE_EXPIRED, ENVELOPE_EXPIRED)? {
            finish_tool(session, &id, false, sink);
        }
        session.released_permission_call = None;
        deps.session_seam.save(session).await?;
        return Ok(None);
    }
    let pending = calls
        .iter()
        .filter(|call| released.pending_call_ids.contains(&call.id))
        .cloned()
        .collect::<Vec<_>>();
    let Continuation {
        exposed,
        raw_provider_exposed,
        disclosure_enabled,
        authority,
        request_devices,
        selected_reads,
        step_inventory,
        permission_candidates,
        pinned_context,
        skill_catalog,
        planning_goal,
    } = continuation;
    let context = permission_proposal::ProposalContext {
        authority: authority.as_ref(),
        request_devices,
        selected_reads,
        step_inventory: step_inventory.as_deref(),
        permission_candidates,
        remaining: &pending,
        parent_envelope: parent.as_ref(),
        issuing_message_id: &issuing_message_id,
        planning: None,
    };
    let mut halted = None;
    if let Some(call) = calls.iter().find(|call| call.id == released.tool_call_id)
        && !session.answered_after(issuing, &call.id)
    {
        // Saved before the call can run, so settlement and recovery never
        // close a call that may have run as one that did not.
        set_dispatching(session, Some(&call.id));
        deps.session_seam.save(session).await?;
        match dispatch(
            deps,
            session,
            turn_id,
            exposed,
            &released.request_id,
            call,
            context,
            &mut state.mint,
            &mut state.exact_mismatch_hinted,
            sink,
        )
        .await
        {
            Ok((outcome, halt)) => {
                set_dispatching(session, None);
                if outcome.is_some() {
                    deps.session_seam.save(session).await?;
                    return Ok(outcome);
                }
                halted = halt;
            }
            Err(error) => {
                if !session.answered_after(issuing, &call.id) {
                    record_failure(
                        session,
                        exposed,
                        call,
                        context,
                        &mut state.mint,
                        sink,
                        &error,
                    )?;
                }
                set_dispatching(session, None);
                return Err(error);
            }
        }
    }
    let start = calls
        .iter()
        .position(|call| pending.iter().any(|pending| pending.id == call.id))
        .unwrap_or(calls.len());
    let owned_exposed = exposed
        .iter()
        .map(|tool| (*tool).clone())
        .collect::<Vec<_>>();
    let model_tool_definitions = exposed
        .iter()
        .map(|tool| {
            let mut spec = tool.spec.clone();
            crate::ui_model_ids::project_tool(&mut spec);
            (spec.name.clone(), spec)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let request_message_ids = HashSet::new();
    let tool_batch = crate::model_observability::tool::ToolBatch::default();
    let mut attachment_read_budget = 0;
    let outcome = crate::future::boxed(|| {
        step_calls::run(
            deps,
            session,
            turn_id,
            step_calls::StepCalls {
                calls: &calls,
                parent_envelope: parent.as_ref(),
                response_message_id: &issuing_message_id,
                request_message_ids: &request_message_ids,
                exposed,
                owned_exposed: &owned_exposed,
                raw_provider_exposed,
                model_tool_definitions: &model_tool_definitions,
                disclosure_enabled,
                authority,
                request_devices,
                selected_reads,
                step_inventory,
                permission_candidates,
                pinned_context,
                skill_catalog,
                planning_goal,
                tool_batch: &tool_batch,
                continuation: true,
            },
            start,
            halted,
            &mut attachment_read_budget,
            state,
            sink,
        )
    })
    .await?;
    if outcome.is_some() {
        return Ok(outcome);
    }
    session.released_permission_call = None;
    deps.session_seam.save(session).await?;
    Ok(None)
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

/// Settle a released step whose turn ended before its calls ran: the
/// released call and each pending call get their only result, except the
/// call being dispatched, which its origin's recovery reconciles. The marker
/// stays until the turn finishes.
pub(super) fn close_unstarted(
    session: &mut crate::session::PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    for id in session.close_released_unstarted(NOT_STARTED, crate::session::RELEASED_TURN_ENDED)? {
        finish_tool(session, &id, false, sink);
    }
    Ok(())
}

/// Dispatch a call of a released step under the decision of `request_id`.
/// Returns the outcome ending the turn, if any, and the note halting the
/// rest of the step when a command failed at execution.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    exposed: &[&RegisteredTool],
    request_id: &str,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    sink: &mut dyn TurnSink,
) -> Result<
    (
        Option<LoopOutcome>,
        Option<crate::model_content::ModelContent>,
    ),
    AgentError,
> {
    if let Some(refusal) = refusal(session, request_id, &call.id) {
        close(session, call, context, mint, sink, refusal)?;
        return Ok((None, None));
    }
    let Some(tool) = exposed
        .iter()
        .find(|tool| tool.name() == call.name)
        .copied()
    else {
        close(session, call, context, mint, sink, NOT_COVERED)?;
        return Ok((None, None));
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
            return Ok((None, None));
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
            .map(|outcome| (outcome, None))
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
                    context.remaining,
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
            Ok((outcome, halted))
        }
        _ => {
            close(session, call, context, mint, sink, NOT_COVERED)?;
            Ok((None, None))
        }
    }
}

/// The recorded refusal of call `call_id`'s item in request `request_id`, if
/// any. A refusal skips only this call; the step's other calls still run.
fn refusal(
    session: &crate::session::PersistedAgentSession,
    request_id: &str,
    call_id: &str,
) -> Option<&'static str> {
    // A call waiting on an equivalent request of an earlier call reads that
    // request's own item: a direct call's request lists it first.
    let item_id = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == request_id)
        .filter(|request| !request.items.iter().any(|item| item.item_id == call_id))
        .and_then(|request| request.items.first())
        .map_or(call_id, |item| item.item_id.as_str());
    let item = session
        .permission_decisions
        .iter()
        .find(|decision| decision.request_id == request_id)
        .and_then(|decision| decision.items.iter().find(|item| item.item_id == item_id));
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
        Some(_) => Some("not executed: this call was not approved"),
        None if session.permission_requests.iter().any(|request| {
            request.request_id == request_id
                && matches!(
                    request.state,
                    crate::dynamic_run::PermissionRequestState::Withdrawn
                        | crate::dynamic_run::PermissionRequestState::NeedsRevalidation
                        | crate::dynamic_run::PermissionRequestState::Replaced
                )
        }) =>
        {
            Some(EXPIRED_OR_WITHDRAWN)
        }
        None => Some("not executed: this call was not approved"),
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
        session.conversation.push(ChatMessage::agent_tool_calls(
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
            issuing_message_id: "",
            planning: None,
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
        session.pause_for_permission(crate::session::AwaitingPermission {
            tool_call_id: "call".into(),
            request_id: "request".into(),
            ..Default::default()
        });
        session.released_permission_call = session.awaiting_permission.take();
        close_unstarted(&mut session, &mut NullTurnSink).unwrap();
        // The marker stays for recovery until the turn finishes.
        assert!(session.released_permission_call.is_some());
        assert_eq!(results(&session), [NOT_STARTED]);
        assert!(session.unclosed_tool_call_ids().is_empty());
        // A call that already has its result is not answered again.
        close_unstarted(&mut session, &mut NullTurnSink).unwrap();
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
