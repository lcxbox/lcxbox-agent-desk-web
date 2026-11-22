//! The per-call loop of one model step, shared by the model step and by the
//! continuation of a step that paused for an approval decision.
use super::phases::{LoopState, ToolPhase};
use super::*;

/// Turn-local inputs of one model step's calls.
pub(super) struct StepCalls<'s, 'a> {
    /// The step's calls, in call order.
    pub calls: &'s [crate::chat::ToolCall],
    /// The envelope of the agent message that issued the calls.
    pub parent_envelope: Option<&'s DataEnvelope>,
    /// The agent message that issued the calls.
    pub response_message_id: &'s String,
    /// The model request messages; empty in a continuation.
    pub request_message_ids: &'s HashSet<String>,
    pub exposed: &'s Vec<&'a RegisteredTool>,
    pub owned_exposed: &'s Vec<RegisteredTool>,
    pub raw_provider_exposed: &'s Vec<RegisteredTool>,
    pub model_tool_definitions: &'s std::collections::BTreeMap<String, crate::chat::ToolSpec>,
    pub disclosure_enabled: bool,
    pub authority: &'s Option<crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub request_devices: &'s Vec<crate::request_devices::RequestDevice>,
    pub selected_reads: &'s Vec<(RegisteredTool, bool)>,
    pub step_inventory: &'s Option<Vec<crate::capability_availability::CapabilityAvailability>>,
    pub permission_candidates: &'s [RegisteredTool],
    pub pinned_context: &'s crate::model_context::PinnedContextPolicy,
    pub skill_catalog: &'s crate::skills::SkillCatalog,
    pub planning_goal: &'s Option<Box<crate::goal::GoalRun>>,
    pub tool_batch: &'s crate::model_observability::tool::ToolBatch,
    /// The calls continue a step paused for an approval decision.
    pub continuation: bool,
}

/// Process `step.calls[start..]` in order. Returns an outcome that ends the
/// turn, or `None` when every call of the step has its result.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run<M: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    turn_id: &str,
    step: StepCalls<'_, '_>,
    start: usize,
    halted: Option<crate::model_content::ModelContent>,
    attachment_read_budget: &mut usize,
    state: &mut LoopState<M>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let StepCalls {
        calls,
        parent_envelope,
        response_message_id,
        request_message_ids,
        exposed,
        owned_exposed,
        raw_provider_exposed,
        model_tool_definitions,
        disclosure_enabled,
        authority,
        request_devices,
        selected_reads,
        step_inventory,
        permission_candidates,
        pinned_context,
        skill_catalog,
        planning_goal,
        tool_batch,
        continuation,
    } = step;
    // Mutating tools in one step run serially; once one fails, times out, or
    // goes to an unknown outcome at execution, the rest of the step's calls
    // are not executed. `halted` holds the skip note.
    let mut halted = halted;
    let planning = permission_group::Planning {
        calls,
        exposed,
        owned_exposed,
        model_tool_definitions,
        disclosure_enabled,
        tool_batch,
    };
    for (call_index, call) in calls.iter().enumerate().skip(start) {
        // A continued call runs only while it is still pending; a call
        // with a result is never run again.
        if continuation && !continuation_pending(session, &call.id) {
            continue;
        }
        if child_source_paused(deps, session).await? {
            for skipped in &calls[call_index..] {
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    (state.mint)(),
                    &skipped.id,
                    &crate::model_content::ModelContent::constant(
                        "not executed: the delegated task source is paused",
                    ),
                    ServerResultSource::DelegationSourcePaused,
                )?;
            }
            return Ok(Some(LoopOutcome::DelegationSourcePaused));
        }
        if let Some(note) = &halted {
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                note,
                ServerResultSource::HaltedToolCall,
            )?;
            continue;
        }

        if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
            append_superseded_tool_results(
                session,
                &calls[call_index..],
                parent_envelope,
                &mut (state.mint),
            )?;
            return Ok(Some(LoopOutcome::Superseded {
                previous_input_revision: session.input_revision,
                current_input_revision,
            }));
        }

        if let Err(error) = ensure_lease_healthy(deps).await {
            for skipped in &calls[call_index..] {
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    (state.mint)(),
                    &skipped.id,
                    &crate::model_content::ModelContent::constant(
                        "tool was not run because the turn lease is no longer healthy",
                    ),
                    ServerResultSource::LeaseLostToolCall,
                )?;
            }
            deps.session_seam.save(session).await?;
            return Err(error);
        }

        // A call whose approval was requested up front runs under its own
        // decision, like the released call; a refusal skips only it.
        if continuation
            && let Some(request_id) = session
                .released_permission_call
                .as_ref()
                .and_then(|released| released.pre_requested_for(&call.id))
                .map(str::to_owned)
        {
            released_dispatch::set_dispatching(session, Some(&call.id));
            deps.session_seam.save(session).await?;
            let (outcome, halt) = crate::future::boxed(|| {
                released_dispatch::dispatch(
                    deps,
                    session,
                    turn_id,
                    exposed,
                    &request_id,
                    call,
                    permission_proposal::ProposalContext {
                        authority: authority.as_ref(),
                        request_devices,
                        selected_reads,
                        step_inventory: step_inventory.as_deref(),
                        permission_candidates,
                        remaining: &calls[call_index + 1..],
                        parent_envelope,
                        issuing_message_id: response_message_id,
                        planning: None,
                    },
                    &mut state.mint,
                    &mut state.exact_mismatch_hinted,
                    sink,
                )
            })
            .await?;
            released_dispatch::set_dispatching(session, None);
            if let Some(outcome) = outcome {
                return Ok(Some(outcome));
            }
            if halt.is_some() {
                halted = halt;
            }
            continue;
        }

        // Same-tool repeat circuit breaker.
        let count = state.same_tool.entry(call.name.clone()).or_insert(0);
        *count += 1;
        if *count > deps.max_same_tool_per_turn {
            // Keep the persisted conversation valid for a follow-up:
            // every tool call in the agent message must have a
            // corresponding result, including calls skipped by this
            // circuit breaker.
            for skipped in &calls[call_index..] {
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    (state.mint)(),
                    &skipped.id,
                    &call_text(
                        skipped,
                        parent_envelope,
                        format!(
                            "tool `{}` was not run because the per-turn repeat limit was reached",
                            skipped.name
                        ),
                    ),
                    ServerResultSource::RepeatLimitToolCall,
                )?;
            }
            deps.session_seam.save(session).await?;
            return Ok(Some(LoopOutcome::CircuitBreak(
                CircuitBreakReason::SameToolRepeat,
            )));
        }

        if crate::visual_evidence::blocks_targeting(session, &call.name) {
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                &crate::model_content::ModelContent::constant(
                    "Screenshot and targeting cannot run in the same agent batch. Read the screenshot result, then submit the action in the next model step; no additional screenshot is required.",
                ),
                ServerResultSource::FreshVisualVerificationRequired,
            )?;
            continue;
        }

        if !exposed
            .iter()
            .any(|advertised| advertised.name() == call.name)
        {
            tool_batch.reject(
                call_index,
                crate::model_observability::Stage::Exposure,
                if deps.registry.iter().any(|tool| tool.name() == call.name) {
                    crate::model_observability::InputIssue::UnexposedTool
                } else {
                    crate::model_observability::InputIssue::UnknownTool
                },
                None,
            );
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                &call_text(
                    call,
                    parent_envelope,
                    format!(
                        "tool_not_advertised: tool `{}` is not advertised in this request. This does not by itself mean permission was denied. Load it with describe_tools when describe_tools is advertised; calling a loaded tool asks for approval when needed. Currently advertised tools (includes built-in conversation tools; only Provider names from the capability index can be loaded): {}",
                        call.name,
                        exposed
                            .iter()
                            .map(|tool| tool.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                ServerResultSource::UnloadedToolCall,
            )?;
            continue;
        }

        // A call naming a tool not exposed under the current scope/state
        // becomes an error tool-result so the conversation stays
        // well-formed and the model can adjust.
        let Some(tool) = lookup_for_session(owned_exposed, &call.name, session) else {
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                &call_text(
                    call,
                    parent_envelope,
                    if crate::permission_tools::latest_tool_request_denied(
                        &session.permission_requests,
                        &session.permission_decisions,
                        &call.name,
                        session.input_revision,
                    ) {
                        format!(
                            "permission_denied: the latest permission request for `{}` was denied, so this call was not dispatched. Do not retry or rename the same denied action. If the independent AI reviewer denied it, use its recorded reason to plan a materially different, narrower action and request that action separately; if the owner denied it, wait for a new owner instruction before requesting it again.",
                            call.name
                        )
                    } else {
                        let reason = deps
                            .registry
                            .iter()
                            .find(|tool| tool.name() == call.name)
                            .and_then(|tool| {
                                crate::registry::exposure_block_reason(
                                    tool,
                                    &session.scope_snapshot,
                                    &session.execution_state,
                                    session.trigger_origin,
                                )
                            })
                            .unwrap_or("tool_not_in_runtime_registry");
                        format!(
                            "{reason}: `{}` is not currently callable. Its visible definition does not grant permission. No action was dispatched; use the current authorization/readiness state to resolve the blocker.",
                            call.name
                        )
                    },
                ),
                ServerResultSource::UnavailableToolCall,
            )?;
            continue;
        };
        if continuation && !continuable(tool.effect) {
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                &crate::model_content::ModelContent::constant(NOT_CONTINUABLE),
                ServerResultSource::PermissionResumeNotExecuted,
            )?;
            continue;
        }
        tool_batch.stage(
            call_index,
            crate::model_observability::Stage::Exposure,
            crate::model_observability::StageOutcome::Passed,
        );
        if session.surface == AgentSessionSurface::AiAgent {
            let validation = model_tool_definitions.get(&call.name).map(|spec| {
                serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                    .map_err(|error| crate::model_input::FormatError {
                        message: crate::model_input::describe_error_with_schema(
                            &call.name,
                            &spec.parameters_schema,
                            &format!("invalid JSON: {error}"),
                        ),
                        issue: crate::model_observability::InputIssue::InvalidJson,
                        schema_path: None,
                    })
                    .and_then(|mut input| {
                        // Resolution verifies and drops an implied device selector.
                        if crate::request_devices::implies_target(&call.name, session)
                            && let Some(input) = input.as_object_mut()
                        {
                            input.remove("target_ref");
                        }
                        crate::model_input::validate_format_reported(
                            &call.name,
                            &spec.parameters_schema,
                            &input,
                        )?;
                        if call.name == "request_permissions"
                            && let Some(registry) = deps.provider_registry
                            && let Some(items) = input["items"].as_array()
                        {
                            crate::model_input::validate_permission_items(items, |name| {
                                registry.capability_for_tool(name).map(|capability| {
                                    let mut target = capability.tool_spec.clone();
                                    crate::ui_model_ids::project_tool(&mut target);
                                    target.parameters_schema
                                })
                            })?;
                        }
                        Ok(())
                    })
            });
            let schema_observed = validation.is_some();
            if let Some(Err(error)) = validation {
                tool_batch.reject(
                    call_index,
                    if error.issue == crate::model_observability::InputIssue::InvalidJson {
                        crate::model_observability::Stage::Json
                    } else {
                        crate::model_observability::Stage::Schema
                    },
                    error.issue,
                    error.schema_path.as_deref(),
                );
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    (state.mint)(),
                    &call.id,
                    &call_text(call, parent_envelope, error.message),
                    ServerResultSource::InvalidToolArguments,
                )?;
                deps.session_seam.save(session).await?;
                finish_tool(session, &call.id, false, sink);
                continue;
            }
            tool_batch.stage(
                call_index,
                crate::model_observability::Stage::Schema,
                if schema_observed {
                    crate::model_observability::StageOutcome::Passed
                } else {
                    crate::model_observability::StageOutcome::NotApplicable
                },
            );
        } else {
            tool_batch.stage(
                call_index,
                crate::model_observability::Stage::Schema,
                crate::model_observability::StageOutcome::NotApplicable,
            );
        }
        session
            .capability_disclosure
            .record_use(std::slice::from_ref(&call.name));
        let resolved_call = match crate::ui_model_ids::resolve_session_call_with_context(
            call,
            session,
            if crate::ui_model_ids::needs_resolution(&call.name) {
                current_unix_ms(deps.clock)?
            } else {
                0
            },
            deps.tools.original_read_context(),
        ) {
            Ok(call) => call,
            Err(error) => {
                tool_batch.reject(
                    call_index,
                    crate::model_observability::Stage::Reference,
                    crate::model_observability::InputIssue::ReferenceUnavailable,
                    None,
                );
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    (state.mint)(),
                    &call.id,
                    &crate::model_content::ToolError::validation(&error)
                        .describe(call, parent_envelope),
                    ServerResultSource::InvalidDesktopId,
                )?;
                deps.session_seam.save(session).await?;
                finish_tool(session, &call.id, false, sink);
                continue;
            }
        };
        if crate::ui_model_ids::needs_resolution(&call.name) {
            tool_batch.stage(
                call_index,
                crate::model_observability::Stage::Reference,
                crate::model_observability::StageOutcome::Attempted,
            );
        }
        let call = &resolved_call;
        let selected_device_read = crate::request_devices::is_selected_read_tool(tool, session);

        let selected_browser = crate::request_devices::is_selected_action_tool(tool, session);
        let selected_device_command = call.name == crate::command_confirmation::COMMAND_TOOL
            && crate::request_devices::selected_command_turn(session);
        if disclosure_enabled
            && !selected_device_read
            && !selected_browser
            && !selected_device_command
            && let Some(providers) = deps.provider_registry
            && let Some(capability) = providers.capability_for_tool(&call.name)
            && let Some(snapshot) = deps.tools.current_grant_disclosure().await?
        {
            let grants = snapshot.subject_grants(session, providers);
            let active = crate::grant_disclosure::active_tool_names(
                &grants,
                session,
                current_unix_ms(deps.clock)?,
                snapshot.readiness_revision,
            );
            let ready = capability.wire.execution_locality
                == desk_agent_protocol::capability_provider::ExecutionLocality::Central
                || tool.has_device_capability(&snapshot.ready_capabilities);
            let policy_read = tool.effect == ToolEffect::ReadOnly
                && (crate::capability_risk::classify_provider_descriptor_floor(
                    capability.wire.effect,
                    &capability.wire.data_policy,
                ) == desk_agent_protocol::capability_grant::CapabilityRiskTier::R0
                    || tool.has_device_capability(&snapshot.policy_read_capabilities));
            let ungranted = ready
                && session.trigger_origin.uses_call_time_approval()
                && !active.contains(&call.name)
                && !policy_read;
            // A direct call without any grant for this tool becomes
            // a call-time proposal; the host still judges calls
            // that have some grant against their exact input.
            if ungranted && (tool.effect == ToolEffect::ReadOnly || session.allows_new_mutation()) {
                let proposal = crate::future::phase(|| async {
                    permission_proposal::propose_call(
                        deps,
                        session,
                        call,
                        permission_proposal::ProposalContext {
                            authority: authority.as_ref(),
                            request_devices,
                            selected_reads,
                            step_inventory: step_inventory.as_deref(),
                            permission_candidates,
                            remaining: &calls[call_index + 1..],
                            parent_envelope,
                            issuing_message_id: response_message_id,
                            planning: Some(&planning),
                        },
                        &mut state.mint,
                        &tool_batch.input(call_index),
                        sink,
                    )
                    .await
                });
                if let Some(outcome) = proposal.await? {
                    return Ok(Some(outcome));
                }
                continue;
            }
            let reason = if !ready {
                Some("capability_not_ready")
            } else if ungranted {
                Some(crate::grant_disclosure::inactive_grant_reason(
                    &grants,
                    &call.name,
                    current_unix_ms(deps.clock)?,
                    snapshot.readiness_revision,
                ))
            } else {
                None
            };
            if let Some(reason) = reason {
                if ready {
                    tool_batch.permission(
                        call_index,
                        crate::model_observability::PermissionOutcome::PolicyRejected,
                    );
                }
                append_internal_tool_result(
                        session,
                        parent_envelope,
                        (state.mint)(),
                        &call.id,
                        &crate::model_content::ModelContent::builder()
                            .text(reason)
                            .text(": this invocation is not currently eligible. Read the latest authorization and readiness state; no action was dispatched. Never retry the same denied action.")
                            .build(),
                        ServerResultSource::InactiveGrantToolCall,
                    )?;
                continue;
            }
        }

        // Some mutation inputs name evidence produced earlier in
        // this durable run. Resolve those references before any
        // approval/reservation work so the model cannot turn a
        // fabricated or cross-run Web source into trusted report
        // content. The resulting action still carries only the
        // bounded title/URL projection, never raw search HTML.
        if let Err(validation_error) = resolve_word_report_web_source_envelope(session, call) {
            tool_batch.reject(
                call_index,
                crate::model_observability::Stage::Preflight,
                crate::model_observability::InputIssue::Semantic,
                None,
            );
            append_internal_tool_result(
                session,
                parent_envelope,
                (state.mint)(),
                &call.id,
                &crate::model_content::ModelContent::constant(validation_error),
                ServerResultSource::InvalidServerBoundWebEvidence,
            )?;
            continue;
        }

        let tool_observation = tool_batch.input(call_index);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            deps.tools
                .observe_tool_input(call, tool_observation.clone());
        }));
        // Saved before the call can run: the host's concrete review accepts
        // the continued call through it, and settlement never closes it as
        // not started.
        if continuation {
            released_dispatch::set_dispatching(session, Some(&call.id));
            deps.session_seam.save(session).await?;
        }
        let action: ToolPhase<'_> = match tool.effect {
            ToolEffect::ExternalWrite => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let result =
                        crate::future::boxed(|| issue_reply::run(deps, session, call)).await;
                    let parent = parent_envelope;
                    let ok = match result {
                        Ok(receipt) => {
                            let content = serde_json::to_string(&receipt).map_err(|_| {
                                crate::subagent::invalid("Invalid publication receipt")
                            })?;
                            if call.name == crate::issue_reply::PREPARE_TOOL_NAME {
                                append_internal_tool_result(
                                    session,
                                    parent,
                                    (state.mint)(),
                                    &call.id,
                                    // Ids, states and reason codes of
                                    // the server's reply record.
                                    &crate::model_content::ModelContent::server_text(content),
                                    ServerResultSource::PrepareIssueReply,
                                )?;
                            } else {
                                append_data_tool_result(
                                    session,
                                    parent,
                                    (state.mint)(),
                                    &call.id,
                                    content,
                                    &call.name,
                                )?;
                            }
                            true
                        }
                        // Drafting runs a model whose upstream
                        // error text is unverified.
                        Err(error) if error.safe_for_model => {
                            append_tool_error(
                                session,
                                parent,
                                (state.mint)(),
                                call,
                                &crate::model_content::ToolError::host(&error),
                            )?;
                            false
                        }
                        Err(error) => return Err(error),
                    };
                    deps.session_seam.save(session).await?;
                    finish_tool(session, &call.id, ok, sink);
                    Ok(None)
                })
            }
            ToolEffect::MemoryQuery | ToolEffect::MemoryWrite | ToolEffect::AgentHistory => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let result_id = (state.mint)();
                    let result = crate::future::boxed(|| {
                        agent_data::dispatch(
                            deps,
                            session,
                            call,
                            response_message_id,
                            result_id.clone(),
                            parent_envelope,
                        )
                    })
                    .await;
                    let (message, ok, replayed) = match result {
                        Ok(result) => (*result.message, result.ok, result.replayed),
                        // Argument checks, or store errors whose
                        // model-safe forms are fixed text
                        // (`AgentStoreError::into_agent_error`).
                        Err(error) if error.safe_for_model => {
                            let message = crate::model_content::ToolError::validation(&error)
                                .result_message(result_id, call, parent_envelope)?;
                            (message, false, false)
                        }
                        Err(error) => return Err(error),
                    };
                    if !replayed {
                        session.conversation.push(message);
                    }
                    deps.session_seam.save(session).await?;
                    finish_tool(session, &call.id, ok, sink);
                    Ok(None)
                })
            }
            ToolEffect::SubAgentPlanning
            | ToolEffect::SubAgentControl
            | ToolEffect::SubAgentQuery
            | ToolEffect::SubAgentWait
            | ToolEffect::SubAgentReport => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let result_message_id = (state.mint)();
                    let result = match crate::subagent::tools::parse_observed(
                        session,
                        call,
                        &tool_observation,
                    ) {
                        Ok(operation) if !operation.requires_single_call() || calls.len() == 1 => {
                            tool_observation.stage(
                                crate::model_observability::Stage::Preflight,
                                crate::model_observability::StageOutcome::Attempted,
                            );
                            match deps.session_seam.subagents(session) {
                                Some(seam) => {
                                    seam.execute(
                                        session,
                                        call,
                                        operation,
                                        &result_message_id,
                                        &tool_observation,
                                    )
                                    .await
                                }
                                None => Err(crate::subagent::invalid(
                                    "durable delegation is unavailable",
                                )),
                            }
                        }
                        Ok(_) => {
                            tool_batch.reject(
                                call_index,
                                crate::model_observability::Stage::Preflight,
                                crate::model_observability::InputIssue::Semantic,
                                None,
                            );
                            Err(crate::subagent::invalid(
                                "delegation waits, questions and terminal reports must be called alone",
                            ))
                        }
                        Err(error) => Err(error),
                    };
                    if result.is_ok() {
                        tool_observation.stage(
                            crate::model_observability::Stage::Preflight,
                            crate::model_observability::StageOutcome::Passed,
                        );
                    }
                    match result {
                        Ok(receipt) => {
                            let payload_text = receipt.payload.to_string();
                            let committed = session
                                .conversation
                                .iter()
                                .find(|message| message.message_id == receipt.result_message_id)
                                .is_some_and(|message| {
                                    message.role == ChatRole::Tool
                                        && message.tool_call_id.as_deref() == Some(call.id.as_str())
                                        && message.data_envelope.is_some()
                                        && message.text == payload_text
                                });
                            if !committed {
                                return Err(crate::subagent::invalid(
                                    "delegation tool receipt was not durably committed",
                                ));
                            }
                            finish_tool(session, &call.id, true, sink);
                            if tool.effect == ToolEffect::SubAgentReport
                                && session.turn_state.is_settled()
                            {
                                return Ok(Some(LoopOutcome::SubAgentYielded));
                            }
                            if let Some(wait) = &session.subagent_wait {
                                return Ok(Some(LoopOutcome::SubAgentsWaiting {
                                    wait_id: wait.wait_id.clone(),
                                }));
                            }
                        }
                        // Delegation errors are argument checks or
                        // fixed host text (`delegation_storage` maps
                        // every storage error to a constant).
                        Err(error) => {
                            append_tool_error(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                call,
                                &crate::model_content::ToolError::validation(&error),
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                        }
                    }

                    Ok(None)
                })
            }
            ToolEffect::ReadOnly | ToolEffect::ExternalRead | ToolEffect::ActionPreview => {
                crate::future::phase(|| async {
                    read_call::run(
                        deps,
                        session,
                        call,
                        read_call::ReadCall {
                            turn_id,
                            tool,
                            selected_device_read,
                            selected_browser,
                            replay: false,
                        },
                        permission_proposal::ProposalContext {
                            authority: authority.as_ref(),
                            request_devices,
                            selected_reads,
                            step_inventory: step_inventory.as_deref(),
                            permission_candidates,
                            remaining: &calls[call_index + 1..],
                            parent_envelope,
                            issuing_message_id: response_message_id,
                            planning: Some(&planning),
                        },
                        &mut state.mint,
                        &mut state.exact_mismatch_hinted,
                        &tool_observation,
                        sink,
                    )
                    .await
                })
            }
            ToolEffect::Mutating => crate::future::phase(|| async {
                let mut grant_required = None;
                if let Some(outcome) = crate::future::boxed(|| {
                    run_mutating(
                        deps,
                        session,
                        turn_id,
                        call,
                        &calls[call_index + 1..],
                        &mut (state.mint),
                        &mut halted,
                        &mut grant_required,
                        sink,
                        &tool_observation,
                    )
                })
                .await?
                {
                    return Ok(Some(outcome));
                }
                if let Some(input_mismatch) = grant_required {
                    return permission_proposal::grant_required(
                        deps,
                        session,
                        call,
                        input_mismatch,
                        &mut state.exact_mismatch_hinted,
                        permission_proposal::ProposalContext {
                            authority: authority.as_ref(),
                            request_devices,
                            selected_reads,
                            step_inventory: step_inventory.as_deref(),
                            permission_candidates,
                            remaining: &calls[call_index + 1..],
                            parent_envelope,
                            issuing_message_id: response_message_id,
                            planning: Some(&planning),
                        },
                        &mut state.mint,
                        &tool_observation,
                        sink,
                    )
                    .await;
                }

                Ok(None)
            }),
            ToolEffect::WaitTask => crate::future::phase(|| async {
                if let Some(outcome) = crate::future::boxed(|| {
                    run_wait(
                        deps,
                        session,
                        call,
                        &calls[call_index + 1..],
                        &mut (state.mint),
                        &mut halted,
                        sink,
                        &tool_observation,
                    )
                })
                .await?
                {
                    return Ok(Some(outcome));
                }

                Ok(None)
            }),
            ToolEffect::GoalControl => crate::future::phase(|| async {
                sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                match crate::goal_tools::parse(call) {
                    Ok(proposal) => {
                        if proposal.decision == crate::goal_tools::GoalDecision::Complete
                            && crate::goal_continuation::completion_pending(session, &call.id)
                        {
                            append_internal_tool_result(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                &call.id,
                                &crate::model_content::ModelContent::constant(
                                    "goal_dependencies_pending: background work, a permission decision, or another tool result is still pending. Use continue for independent work or wait for native background results. Do not claim completion.",
                                ),
                                ServerResultSource::ControlGoal,
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                            return Ok(None);
                        }
                        if proposal.decision == crate::goal_tools::GoalDecision::Complete
                            && let Some(seam) = deps.session_seam.subagents(session)
                            && !seam.required_children_complete(session).await?
                        {
                            tool_observation.reject(
                                crate::model_observability::Stage::Preflight,
                                crate::model_observability::InputIssue::Precondition,
                            );
                            append_internal_tool_result(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                &call.id,
                                &crate::model_content::ModelContent::constant(
                                    "required_subagents_incomplete: read_subagent_reports for each required request, page through to its current terminal report and assess it against the goal. Finished means the child returned, not that the goal was achieved. Use wait_subagent_reports for unfinished dependencies. A failed or cancelled required dependency cannot be claimed as complete.",
                                ),
                                ServerResultSource::ControlGoal,
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                            return Ok(None);
                        }
                        let claimed_goal = deps.session_seam.load_claimed_goal(session).await?;
                        let sources =
                            if proposal.decision == crate::goal_tools::GoalDecision::Complete {
                                goal_control::completion_sources(
                                    session,
                                    &claimed_goal,
                                    request_message_ids,
                                    response_message_id,
                                    deps.registry,
                                )
                                .inspect_err(|error| {
                                    tool_observation.input_error(error);
                                })?
                            } else {
                                Vec::new()
                            };
                        let control = if proposal.decision == crate::goal_tools::GoalDecision::Wait
                        {
                            let Some(wait) = deps
                                .session_seam
                                .prepare_goal_work_wait(session, &call.id)
                                .await?
                            else {
                                append_internal_tool_result(
                                    session,
                                    parent_envelope,
                                    (state.mint)(),
                                    &call.id,
                                    &crate::model_content::ModelContent::constant(
                                        "no_goal_background_tasks: no normal background task of this goal is available to wait for. Inspect saved results and choose continue, ask_user, complete, or blocked. Use wait_subagent_reports for child dependencies.",
                                    ),
                                    ServerResultSource::ControlGoal,
                                )?;
                                deps.session_seam.save(session).await?;
                                finish_tool(session, &call.id, false, sink);
                                return Ok(None);
                            };
                            crate::goal::GoalControl::WaitForWork {
                                wait,
                                message: proposal.message,
                            }
                        } else {
                            proposal.into_control(sources).inspect_err(|error| {
                                tool_observation.input_error(error);
                            })?
                        };
                        tool_observation.stage(
                            crate::model_observability::Stage::Preflight,
                            crate::model_observability::StageOutcome::Passed,
                        );
                        append_internal_tool_result(
                            session,
                            parent_envelope,
                            (state.mint)(),
                            &call.id,
                            &crate::model_content::ModelContent::json(&GoalSettlementReceipt {
                                status: "accepted_for_atomic_settlement",
                                goal_id: crate::model_content::ServerId::new(&claimed_goal.goal_id),
                            })?,
                            ServerResultSource::ControlGoal,
                        )?;
                        let summary = match &control {
                            crate::goal::GoalControl::WaitForWork { message, .. } => message,
                            crate::goal::GoalControl::Continue { progress, .. } => progress,
                            crate::goal::GoalControl::Wait { reference_id, .. } => reference_id,
                            crate::goal::GoalControl::Complete { summary, .. } => summary,
                            crate::goal::GoalControl::Blocked { reason } => reason,
                        };
                        // The visible summary is the model's own
                        // argument to this call.
                        let issuing = parent_envelope;
                        let content = crate::model_content::ModelContent::builder()
                            .arg(&crate::model_content::ModelArg::of_call(
                                call,
                                issuing,
                                summary.clone(),
                            ))
                            .build();
                        let message = ChatMessage::from_content(
                            ChatRole::Assistant,
                            (state.mint)(),
                            &content,
                            crate::model_content::Labelling {
                                issuing,
                                call_id: &call.id,
                                source:
                                    crate::model_message_labels::ServerResultSource::ControlGoal,
                                request_only: false,
                            },
                        )?
                        .with_turn_id(session.current_turn_id.clone().unwrap_or_default());
                        session.conversation.push(message);
                        if matches!(
                            control,
                            crate::goal::GoalControl::Complete { .. }
                                | crate::goal::GoalControl::Blocked { .. }
                        ) {
                            stage_subagent_observations(
                                session,
                                request_message_ids,
                                response_message_id,
                                true,
                            )?;
                        }
                        return Ok(Some(LoopOutcome::GoalControlled {
                            control,
                            call_id: call.id.clone(),
                        }));
                    }
                    Err(error) => {
                        tool_observation.input_error(&error);
                        // Only argument parsing fails here; host
                        // errors propagate.
                        append_tool_error(
                            session,
                            parent_envelope,
                            (state.mint)(),
                            call,
                            &crate::model_content::ToolError::validation(&error),
                        )?;
                        deps.session_seam.save(session).await?;
                        finish_tool(session, &call.id, false, sink);
                    }
                }

                Ok(None)
            }),
            ToolEffect::RunProjection => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let updated_at = (deps.clock)();
                    let step_id = stable_lineage_id("status-step", &call.id);
                    let current_revision = session
                        .task_status_projection
                        .as_ref()
                        .map_or(0, |projection| projection.revision);
                    let projection = crate::task_status_tools::build_task_status_projection(
                        call,
                        current_revision,
                        updated_at.clone(),
                        step_id,
                    );
                    tool_observation.preflight_result(&projection);
                    match projection {
                        Ok(projection) => {
                            let content =
                                crate::model_content::ModelContent::json(&TaskStatusReceipt {
                                    status: "updated",
                                    revision: projection.revision,
                                    item_count: projection.items.len(),
                                })?;
                            let result_envelope = crate::model_message_labels::composed_envelope(
                                &content,
                                parent_envelope,
                                &call.id,
                                crate::model_message_labels::ServerResultSource::UpdateTaskStatus,
                                false,
                            )?;
                            let event_seq =
                                session.last_event_seq.checked_add(1).ok_or_else(|| {
                                    AgentError {
                                        kind: AgentErrorKind::Internal,
                                        message: "agent run event sequence exhausted".into(),
                                        retryable: false,
                                        safe_for_model: false,
                                        error_code: None,
                                    }
                                })?;
                            let event = crate::dynamic_run::TaskStatusUpdatedEvent {
                                event: crate::dynamic_run::AgentRunEvent {
                                    schema_version:
                                        crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
                                    event_id: stable_lineage_id(
                                        "status-event",
                                        &(&session.conversation_id, event_seq, &call.id),
                                    ),
                                    run_id: session.conversation_id.clone(),
                                    event_seq,
                                    input_revision: session.input_revision,
                                    kind: crate::dynamic_run::AgentRunEventKind::TaskStatusUpdated,
                                    correlation_id: Some(call.id.clone()),
                                    source_envelope_ids: parent_envelope
                                        .map(|envelope| vec![envelope.envelope_id.clone()])
                                        .unwrap_or_default(),
                                    result_envelope_ids: result_envelope
                                        .as_ref()
                                        .map(|envelope| vec![envelope.envelope_id.clone()])
                                        .unwrap_or_default(),
                                    created_at: updated_at,
                                },
                                projection: projection.clone(),
                            };
                            event.validate().map_err(|error| AgentError {
                                kind: AgentErrorKind::Internal,
                                message: format!("invalid task-status update event: {error}"),
                                retryable: false,
                                safe_for_model: false,
                                error_code: None,
                            })?;
                            session.focus_epoch.bind_task_status();
                            session.task_status_projection = Some(projection);
                            session.last_event_seq = event_seq;
                            let message = ChatMessage::tool_result_from(
                                (state.mint)(),
                                &call.id,
                                &content,
                                parent_envelope,
                                crate::model_message_labels::ServerResultSource::UpdateTaskStatus,
                            )?;
                            debug_assert_eq!(message.data_envelope, result_envelope);
                            session.conversation.push(message);
                            deps.session_seam
                                .save_task_status_update(session, &event)
                                .await?;
                            finish_tool(session, &call.id, true, sink);
                        }
                        // The task list is built from the call alone.
                        Err(error) => {
                            append_tool_error(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                call,
                                &crate::model_content::ToolError::validation(&error),
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                        }
                    }

                    Ok(None)
                })
            }
            ToolEffect::GoalOpenPlanning => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let prepared = (|| -> Result<_, AgentError> {
                        if session.surface != AgentSessionSurface::AiAgent
                            || session.trigger_origin != TriggerOrigin::User
                        {
                            return Err(crate::goal_tools::unavailable());
                        }
                        let proposal =
                            crate::goal_tools::parse_open(call).inspect_err(|error| {
                                tool_observation.input_error(error);
                            })?;
                        let user = crate::permission_resume::latest_user_requirement(
                            &session.conversation,
                        )
                        .ok_or_else(|| {
                            crate::goal_tools::invalid(
                                "a current owner request is required to propose a goal",
                            )
                        })?;
                        let policy = deps
                            .model
                            .model_egress_policy()?
                            .ok_or_else(crate::goal_tools::unavailable)?;
                        let model_binding =
                            crate::goal::GoalModelBinding::from_destination(&policy.destination)
                                .map_err(|_| {
                                    crate::goal_tools::invalid(
                                        "current model cannot be bound to a goal",
                                    )
                                })?;
                        let created_at = (deps.clock)();
                        let now_unix_ms = u64::try_from(
                            chrono::DateTime::parse_from_rfc3339(&created_at)
                                .map_err(|_| crate::goal_tools::invalid("invalid goal clock"))?
                                .timestamp_millis(),
                        )
                        .map_err(|_| crate::goal_tools::invalid("invalid goal clock"))?;
                        let event_seq = session.last_event_seq.checked_add(1).ok_or_else(|| {
                            crate::goal_tools::invalid("goal event sequence exhausted")
                        })?;
                        let request_id = stable_lineage_id(
                            "goal-open-request",
                            &(
                                &session.conversation_id,
                                session.input_revision,
                                event_seq,
                                &call.id,
                            ),
                        );
                        let mut request = if let Some(goal) = planning_goal.as_deref() {
                            if proposal.previous_completed_goal_id.is_some() {
                                tool_observation.reject(
                                    crate::model_observability::Stage::Preflight,
                                    crate::model_observability::InputIssue::Combination,
                                );
                                return Err(crate::goal_tools::invalid(
                                    "a revision cannot reference a completed goal",
                                ));
                            }
                            crate::goal::GoalOpenRequest::new_revision(
                                request_id,
                                goal,
                                user.message_id.clone(),
                                session.input_revision,
                                proposal.goal_text,
                                model_binding,
                                now_unix_ms,
                            )
                        } else {
                            crate::goal::GoalOpenRequest::new(
                                request_id,
                                session.conversation_id.clone(),
                                session.actor_id.clone(),
                                session
                                    .single_agent_request_target()
                                    .map_err(crate::goal_tools::invalid)?
                                    .clone(),
                                session.device_id.clone(),
                                user.message_id.clone(),
                                session.input_revision,
                                proposal.goal_text,
                                crate::goal::GoalLimits::default(),
                                model_binding,
                                now_unix_ms,
                            )
                        }
                        .map_err(|_| {
                            crate::goal_tools::invalid(
                                "goal proposal requires a later owner reply and a stopped goal",
                            )
                        })?;
                        request.previous_completed_goal_id = proposal.previous_completed_goal_id;
                        request.validate().map_err(|_| {
                            crate::goal_tools::invalid("invalid previous completed goal ID")
                        })?;
                        let event = crate::goal::GoalOpenRequestEvent::new(
                            &request,
                            crate::dynamic_run::AgentRunEventKind::GoalOpenRequested,
                            event_seq,
                            created_at,
                        )
                        .map_err(|_| crate::goal_tools::invalid("invalid goal proposal event"))?;
                        Ok(event)
                    })();
                    match prepared {
                        Ok(event) => {
                            tool_observation.stage(
                                crate::model_observability::Stage::Preflight,
                                crate::model_observability::StageOutcome::Passed,
                            );
                            let mut before = session.clone_boxed();
                            // The goal text is the call's own argument.
                            let content = crate::model_content::ModelContent::json(
                                &GoalOpenPendingReceipt {
                                    status: "pending_approval",
                                    request_id: crate::model_content::ServerId::new(
                                        &event.request.request_id,
                                    ),
                                    goal_text: crate::model_content::ModelArg::of_call(
                                        call,
                                        parent_envelope,
                                        event.request.goal_text.clone(),
                                    ),
                                    previous_completed_goal_id: event
                                        .request
                                        .previous_completed_goal_id
                                        .as_deref()
                                        .map(crate::model_content::ServerId::new),
                                    executed: false,
                                },
                            )?;
                            append_internal_tool_result(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                &call.id,
                                &content,
                                ServerResultSource::GoalOpenPending,
                            )?;
                            session.last_event_seq = event.event.event_seq;
                            match deps
                                .session_seam
                                .save_goal_open_request(session, &event)
                                .await
                            {
                                Ok(()) => {
                                    if let Some(alias)=crate::model_observability::ObservationAlias::goal_open_request(
                        &session.conversation_id,&event.request.request_id,
                    ) { tool_observation.bind(alias); }
                                    tool_observation.permission(
                                        crate::model_observability::PermissionOutcome::Waiting,
                                    );
                                    finish_tool(session, &call.id, true, sink);
                                    return Ok(Some(LoopOutcome::GoalOpenRequested {
                                        request_id: event.request.request_id,
                                    }));
                                }
                                Err(error) => {
                                    std::mem::swap(session, &mut before);
                                    append_internal_tool_result(
                                        session,
                                        parent_envelope,
                                        (state.mint)(),
                                        &call.id,
                                        &crate::model_content::ToolError::validation(&error)
                                            .describe(call, parent_envelope),
                                        ServerResultSource::GoalOpenFailed,
                                    )?;
                                    deps.session_seam.save(session).await?;
                                    finish_tool(session, &call.id, false, sink);
                                }
                            }
                        }
                        Err(error) => {
                            append_internal_tool_result(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                &call.id,
                                &crate::model_content::ToolError::validation(&error)
                                    .describe(call, parent_envelope),
                                ServerResultSource::GoalOpenFailed,
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                        }
                    }

                    Ok(None)
                })
            }
            ToolEffect::SchedulePlanning
            | ToolEffect::ScheduleQuery
            | ToolEffect::SkillAuthoring => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    // Persist the model call before a separate transaction
                    // compares the exact version and lease and records its result.
                    deps.session_seam.save(session).await?;
                    let skill = tool.effect == ToolEffect::SkillAuthoring;
                    let result = if skill {
                        deps.session_seam
                            .propose_skill(session, call, tool_observation.clone())
                            .await
                    } else {
                        deps.session_seam
                            .manage_schedule_tool(session, call, tool_observation.clone())
                            .await
                    };
                    if let Err(error) = result {
                        append_internal_tool_result(
                            session,
                            parent_envelope,
                            (state.mint)(),
                            &call.id,
                            &crate::model_content::ToolError::validation(&error)
                                .describe(call, parent_envelope),
                            if skill {
                                ServerResultSource::SkillProposalFailed
                            } else {
                                ServerResultSource::ScheduleProposalFailed
                            },
                        )?;
                        deps.session_seam.save(session).await?;
                        finish_tool(session, &call.id, false, sink);
                    } else {
                        // The proposal transaction already stored a labelled
                        // result. Do not append a second success message.
                        finish_tool(session, &call.id, true, sink);
                        if let Some(pause) = crate::proposal_review::awaiting_proposal(
                            session,
                            &call.id,
                            turn_id,
                            current_unix_ms(deps.clock)?,
                        ) {
                            // The turn ends here; the decision resumes it.
                            tool_observation
                                .permission(crate::model_observability::PermissionOutcome::Waiting);
                            append_unstarted_tool_results(
                                session,
                                &calls[call_index + 1..],
                                parent_envelope,
                                &mut (state.mint),
                                "not executed: this step paused for a decision on an earlier call; call it again after that decision if still needed",
                                ServerResultSource::ProposalPauseToolCall,
                            )?;
                            deps.session_seam.save(session).await?;
                            sink.on_permission_requested(&pause.id, 1);
                            return Ok(Some(LoopOutcome::DecisionRequested { pause }));
                        }
                    }

                    Ok(None)
                })
            }
            ToolEffect::DirectoryPlanning => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let resolved = match crate::directory_tools::parse_for_request(
                        call,
                        session.uses_request_device_tools(),
                    ) {
                        Ok((Some(reference), input)) => deps
                            .tools
                            .resolve_request_device_directory(session, &reference, &input.path)
                            .await
                            .map(|(device, resolved)| (device, input, resolved))
                            // The device resolves the path; its
                            // errors are unverified text.
                            .map_err(|error| (error, true)),
                        Ok((None, input)) => deps
                            .tools
                            .resolve_directory_candidate(&input.path)
                            .await
                            .map(|resolved| (session.device_id.clone(), input, resolved))
                            .map_err(|error| (error, true)),
                        // Parsing reads only the call.
                        Err(error) => Err((error, false)),
                    };
                    tool_observation.preflight_result(
                        &resolved
                            .as_ref()
                            .map(|_| ())
                            .map_err(|(error, _)| error.clone()),
                    );
                    let (device, input, resolved) = match resolved {
                        Ok(value) => value,
                        Err((error, from_device)) => {
                            let error = if from_device {
                                crate::model_content::ToolError::host(&error)
                            } else {
                                crate::model_content::ToolError::validation(&error)
                            };
                            append_tool_error(
                                session,
                                parent_envelope,
                                (state.mint)(),
                                call,
                                &error,
                            )?;
                            deps.session_seam.save(session).await?;
                            finish_tool(session, &call.id, false, sink);
                            return Ok(None);
                        }
                    };
                    let request_id = stable_lineage_id(
                        "directory-proposal",
                        &(&session.conversation_id, session.input_revision, &call.id),
                    );
                    // Save the exact held state before the proposal transaction.
                    // Resolution metadata is for the owner's UI, not model egress.
                    deps.session_seam.save(session).await?;
                    let proposal_result = deps
                        .session_seam
                        .propose_directory(
                            session,
                            &device,
                            crate::directory_tools::proposal(request_id.clone(), input, resolved),
                            current_unix_ms(deps.clock)?,
                        )
                        .await;
                    if let Err(error) = proposal_result {
                        append_internal_tool_result(
                            session,
                            parent_envelope,
                            (state.mint)(),
                            &call.id,
                            &crate::model_content::ToolError::validation(&error)
                                .describe(call, parent_envelope),
                            ServerResultSource::DirectoryProposalFailed,
                        )?;
                        deps.session_seam.save(session).await?;
                        finish_tool(session, &call.id, false, sink);
                        return Ok(None);
                    }
                    if let Some(alias) =
                        crate::model_observability::ObservationAlias::directory_request(
                            &session.conversation_id,
                            &request_id,
                        )
                    {
                        tool_observation.bind(alias);
                    }
                    tool_observation
                        .permission(crate::model_observability::PermissionOutcome::Waiting);
                    append_internal_tool_result(
                        session,
                        parent_envelope,
                        (state.mint)(),
                        &call.id,
                        &crate::directory_tools::pending_result(&request_id)?,
                        ServerResultSource::DirectoryProposalPending,
                    )?;
                    append_unstarted_tool_results(
                        session,
                        &calls[call_index + 1..],
                        parent_envelope,
                        &mut (state.mint),
                        "not executed: this step paused for a decision on an earlier call; call it again after that decision if still needed",
                        ServerResultSource::DirectoryPauseToolCall,
                    )?;
                    deps.session_seam.save(session).await?;
                    finish_tool(session, &call.id, true, sink);
                    sink.on_permission_requested(&request_id, 1);
                    // A scheduled run pauses for its durable approval
                    // flow; every other turn ends for a decision that
                    // resumes the requirement.
                    if session.trigger_origin == crate::session::TriggerOrigin::ScheduledTask {
                        return Ok(Some(LoopOutcome::PermissionRequested { request_id }));
                    }
                    Ok(Some(LoopOutcome::DecisionRequested {
                        pause: crate::proposal_review::DecisionPause {
                            kind: crate::proposal_review::DecisionKind::Directory,
                            id: request_id,
                            turn_id: turn_id.to_owned(),
                            call_id: call.id.clone(),
                            input_revision: session.input_revision,
                            requested_at_unix_ms: current_unix_ms(deps.clock)?,
                        },
                    }))
                })
            }
            ToolEffect::PermissionPlanning => crate::future::phase(|| async {
                permission_proposal::propose(
                    deps,
                    session,
                    call,
                    call,
                    permission_proposal::ProposalContext {
                        authority: authority.as_ref(),
                        request_devices,
                        selected_reads,
                        step_inventory: step_inventory.as_deref(),
                        permission_candidates,
                        remaining: &calls[call_index + 1..],
                        parent_envelope,
                        issuing_message_id: response_message_id,
                        planning: Some(&planning),
                    },
                    &mut state.mint,
                    &tool_observation,
                    sink,
                )
                .await
            }),
            ToolEffect::SkillLoading => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let result = session
                        .skills
                        .apply(session.input_revision, call, skill_catalog);
                    tool_observation.preflight_result(&result);
                    let ok = result.is_ok();
                    let parent = parent_envelope;
                    match result {
                        Ok((text, root)) => {
                            let destination = deps
                                .model
                                .model_egress_policy()?
                                .map(|policy| policy.destination);
                            let content =
                                load_skill_content(call, parent, text, root, destination.as_ref())?;
                            let message = ChatMessage::tool_result_from(
                                (state.mint)(),
                                &call.id,
                                &content,
                                parent,
                                ServerResultSource::LoadSkill,
                            )?;
                            session.conversation.push(message);
                        }
                        // Skill loading reads only the call and the frozen catalogue.
                        Err(error) => append_tool_error(
                            session,
                            parent,
                            (state.mint)(),
                            call,
                            &crate::model_content::ToolError::validation(&error),
                        )?,
                    }
                    deps.session_seam.save(session).await?;
                    finish_tool(session, &call.id, ok, sink);
                    Ok(None)
                })
            }
            ToolEffect::CapabilityDiscovery => {
                crate::future::phase(|| async {
                    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                    let providers = deps.provider_registry.ok_or_else(|| AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "capability discovery has no Provider registry".into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    })?;
                    let inventory = step_inventory.as_deref().ok_or_else(|| AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "capability discovery has no live inventory".into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    })?;
                    let result = crate::capability_disclosure::apply_load_call(
                        call,
                        &mut session.capability_disclosure,
                        session.input_revision,
                        &crate::capability_disclosure::CapabilityLoadContext {
                            registry: providers,
                            inventory,
                            max_context_bytes: pinned_context.max_context_bytes,
                            callable_tools: raw_provider_exposed,
                            permission_candidates,
                        },
                    );
                    tool_observation.preflight_result(&result);
                    let ok = result.is_ok();
                    let parent = parent_envelope;
                    match result {
                        // Loaded schemas come from the compiled registry.
                        Ok(content) => session.conversation.push(ChatMessage::tool_result_from(
                            (state.mint)(),
                            &call.id,
                            &crate::model_content::ModelContent::server_text(content),
                            parent,
                            ServerResultSource::DescribeTools,
                        )?),
                        // Loading reads only the call and the server's tool registry.
                        Err(error) => append_tool_error(
                            session,
                            parent,
                            (state.mint)(),
                            call,
                            &crate::model_content::ToolError::validation(&error),
                        )?,
                    }
                    deps.session_seam.save(session).await?;
                    finish_tool(session, &call.id, ok, sink);

                    Ok(None)
                })
            }
            ToolEffect::ConversationHistory => crate::future::phase(|| async {
                sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                let result = crate::future::boxed(|| {
                    retrieval_accounting::dispatch_legacy(
                        deps,
                        session,
                        call,
                        response_message_id,
                        (state.mint)(),
                        parent_envelope,
                        attachment_read_budget,
                        &tool_observation,
                    )
                })
                .await?;
                let ok = result.ok;
                if !result.replayed {
                    session.conversation.push(*result.message);
                }
                if call.name == crate::conversation_attachment::READ_ATTACHMENT_TOOL {
                    retain_latest_session_image(session)?;
                }
                deps.session_seam.save(session).await?;
                finish_tool(session, &call.id, ok, sink);
                Ok(None)
            }),
        };
        let outcome = action.await?;
        if let Some(outcome) = outcome {
            return Ok(Some(outcome));
        }
        if continuation {
            released_dispatch::set_dispatching(session, None);
        }
    }
    Ok(None)
}

/// Result of a call that cannot run after an approval pause.
const NOT_CONTINUABLE: &str =
    "not executed: this call cannot run after an approval pause; call it again if still needed";

/// Whether a call of this effect may run in a continued step. The excluded
/// effects bind their state to the turn that runs them (decision pauses,
/// proposals, goals, child tasks) or need the model request's size; they
/// would bind the resume turn instead of the issuing one.
pub(super) fn continuable(effect: ToolEffect) -> bool {
    match effect {
        ToolEffect::ReadOnly
        | ToolEffect::ActionPreview
        | ToolEffect::ExternalRead
        | ToolEffect::Mutating
        | ToolEffect::WaitTask
        | ToolEffect::RunProjection
        | ToolEffect::PermissionPlanning
        | ToolEffect::CapabilityDiscovery
        | ToolEffect::MemoryQuery
        | ToolEffect::MemoryWrite
        | ToolEffect::AgentHistory
        | ToolEffect::SkillLoading => true,
        ToolEffect::SchedulePlanning
        | ToolEffect::ScheduleQuery
        | ToolEffect::SkillAuthoring
        | ToolEffect::DirectoryPlanning
        | ToolEffect::GoalOpenPlanning
        | ToolEffect::GoalControl
        | ToolEffect::SubAgentPlanning
        | ToolEffect::SubAgentControl
        | ToolEffect::SubAgentQuery
        | ToolEffect::SubAgentWait
        | ToolEffect::SubAgentReport
        | ToolEffect::ConversationHistory
        | ToolEffect::ExternalWrite => false,
    }
}

/// Prune answered ids from the released step's pending calls and tell
/// whether `call_id` is still pending.
fn continuation_pending(session: &mut PersistedAgentSession, call_id: &str) -> bool {
    let Some(released) = session.released_permission_call.as_ref() else {
        return false;
    };
    let Some(issuing) = session.marker_issuing_index(released) else {
        return false;
    };
    let pending = released
        .pending_call_ids
        .iter()
        .filter(|id| !session.answered_after(issuing, id))
        .cloned()
        .collect::<Vec<_>>();
    let still_pending = pending.iter().any(|id| id == call_id);
    if let Some(released) = session.released_permission_call.as_mut() {
        released.pending_call_ids = pending;
    }
    still_pending
}

/// A `load_skill` receipt. A stored skill's receipt lists its package files,
/// so it carries that version's root; a bundled skill's receipt holds only
/// compiled text and the call's own arguments.
fn load_skill_content(
    call: &crate::chat::ToolCall,
    issuing: Option<&DataEnvelope>,
    text: String,
    root: Option<crate::skills::SkillRoot>,
    destination: Option<&desk_agent_protocol::data_lineage::DestinationIdentity>,
) -> Result<crate::model_content::ModelContent, AgentError> {
    use crate::model_content::{ModelArg, ModelContent, Stored, Unverified};
    Ok(match (root, destination) {
        (None, _) => ModelContent::builder()
            .arg(&ModelArg::of_call(call, issuing, text))
            .build(),
        (Some(root), Some(destination)) => {
            let root = root.stored_root("receipt", &text, destination)?;
            ModelContent::builder()
                .stored(&Stored::new(text, root))
                .build()
        }
        // Without a model destination no root can be bound.
        (Some(_), None) => ModelContent::builder()
            .unverified(&Unverified::new(text))
            .build(),
    })
}

crate::model_receipt! {
    struct TaskStatusReceipt {
        status: &'static str,
        revision: u64,
        item_count: usize,
    }
}

crate::model_receipt! {
    struct GoalSettlementReceipt {
        status: &'static str,
        goal_id: crate::model_content::ServerId,
    }
}

crate::model_receipt! {
    struct GoalOpenPendingReceipt {
        status: &'static str,
        request_id: crate::model_content::ServerId,
        goal_text: crate::model_content::ModelArg<String>,
        previous_completed_goal_id: Option<crate::model_content::ServerId>,
        executed: bool,
    }
}
