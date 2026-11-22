//! Separate preparation, model exchange and tool acceptance polls.
use super::*;
use crate::model_observability::protocol_correction::{
    PendingProtocolCorrection, ProtocolCorrectionCheck, ProtocolCorrectionReason,
};
use std::{future::Future, pin::Pin};

pub(super) struct LoopState<M> {
    pub mint: M,
    pub same_tool: HashMap<String, u32>,
    pub compression_attempted: HashSet<String>,
    pub projection_rebuilds: u8,
    pub pending_protocol_correction: Option<PendingProtocolCorrection>,
    pub completion_protocol_retries: u8,
    pub empty_end_turn_retries: u8,
    pub truncated_turn_retries: u8,
    pub post_tool_permission_protocol_retries: u8,
    pub invalid_tool_argument_retries: u8,
    pub goal_correction_attempted: bool,
    pub required_tool_choice_rejected: bool,
    pub physical_model_attempt: u32,
    /// Tools whose exact-input mismatch was already explained in this turn.
    pub exact_mismatch_hinted: HashSet<String>,
}

enum PhaseResult<F> {
    Ready(F),
    Complete(Option<LoopOutcome>),
}
type ToolPhase<'a> = Pin<Box<dyn Future<Output = Result<Option<LoopOutcome>, AgentError>> + 'a>>;
type ModelPhase<'a> =
    Pin<Box<dyn Future<Output = Result<PhaseResult<ToolPhase<'a>>, AgentError>> + 'a>>;

type ContextPhase<'a> =
    Pin<Box<dyn Future<Output = Result<PhaseResult<ModelPhase<'a>>, AgentError>> + 'a>>;
type PromptPhase<'a> =
    Pin<Box<dyn Future<Output = Result<PhaseResult<RuntimePhase<'a>>, AgentError>> + 'a>>;

type RuntimePhase<'a> =
    Pin<Box<dyn Future<Output = Result<PhaseResult<ContextPhase<'a>>, AgentError>> + 'a>>;

pub(super) async fn run_iteration<M: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    turn_id: &str,
    sink: &mut dyn TurnSink,
    state: &mut LoopState<M>,
) -> Result<Option<LoopOutcome>, AgentError> {
    let prompt =
        match crate::future::boxed(|| prepare_iteration(deps, session, turn_id, sink, state))
            .await?
        {
            PhaseResult::Complete(outcome) => return Ok(outcome),
            PhaseResult::Ready(prompt) => prompt,
        };
    let runtime = match prompt.await? {
        PhaseResult::Complete(outcome) => return Ok(outcome),
        PhaseResult::Ready(runtime) => runtime,
    };
    let context = match runtime.await? {
        PhaseResult::Complete(outcome) => return Ok(outcome),
        PhaseResult::Ready(context) => context,
    };
    let model = match context.await? {
        PhaseResult::Complete(outcome) => return Ok(outcome),
        PhaseResult::Ready(model) => model,
    };
    let tools = match model.await? {
        PhaseResult::Complete(outcome) => return Ok(outcome),
        PhaseResult::Ready(tools) => tools,
    };
    tools.await
}

async fn prepare_iteration<'a, 'd: 'a, M: FnMut() -> String + 'a>(
    deps: &'a LoopDeps<'d>,
    session: &'a mut PersistedAgentSession,
    turn_id: &'a str,
    sink: &'a mut dyn TurnSink,
    state: &'a mut LoopState<M>,
) -> Result<PhaseResult<PromptPhase<'a>>, AgentError> {
    prepare_iteration_phase(deps, session, turn_id, sink, state).await
}

fn prepare_iteration_phase<'a, 'd: 'a, M: FnMut() -> String + 'a>(
    deps: &'a LoopDeps<'d>,
    session: &'a mut PersistedAgentSession,
    turn_id: &'a str,
    sink: &'a mut dyn TurnSink,
    state: &'a mut LoopState<M>,
) -> Pin<Box<dyn Future<Output = Result<PhaseResult<PromptPhase<'a>>, AgentError>> + 'a>> {
    let mut inputs = Some(Box::new((deps, session, turn_id, sink, state)));
    crate::future::phase(|| {
        prepare_iteration_inner(inputs.take().expect("immediate phase factory"))
    })
}

#[allow(clippy::type_complexity)]
async fn prepare_iteration_inner<'a, 'd: 'a, M: FnMut() -> String + 'a>(
    inputs: Box<(
        &'a LoopDeps<'d>,
        &'a mut PersistedAgentSession,
        &'a str,
        &'a mut dyn TurnSink,
        &'a mut LoopState<M>,
    )>,
) -> Result<PhaseResult<PromptPhase<'a>>, AgentError> {
    let (deps, session, turn_id, sink, state) = *inputs;
    if child_source_paused(deps, session).await? {
        return Ok(PhaseResult::Complete(Some(
            LoopOutcome::DelegationSourcePaused,
        )));
    }
    let subagent_projection = match deps.session_seam.subagents(session) {
        Some(seam) if session.surface == AgentSessionSurface::AiAgent => {
            seam.planning_projection(session).await?
        }
        _ => None,
    };
    let child_runtime_label = session
        .agent_role
        .binding()
        .and(subagent_projection.as_ref())
        .and_then(|message| message.data_envelope.clone());
    if subagent_projection.as_ref().is_some_and(|message| {
        message.role != ChatRole::SystemEvent || message.data_envelope.is_none()
    }) {
        return Err(crate::subagent::invalid(
            "delegation projection requires a labelled runtime data message",
        ));
    }
    if session.turn_step_budget_exhausted(deps.max_steps_per_turn)
        || (session.surface == AgentSessionSurface::AiAgent
            && session.focus_epoch.step_budget_exhausted())
    {
        if session.trigger_origin == TriggerOrigin::GoalContinuation
            && state.pending_protocol_correction.is_none()
            && !state.goal_correction_attempted
            && !crate::goal_continuation::unresolved_work(session)
            && !crate::goal_continuation::pending_approval(session)
            && session.subagent_wait.is_none()
        {
            return Ok(PhaseResult::Complete(Some(LoopOutcome::GoalYielded)));
        }
        return Ok(PhaseResult::Complete(Some(LoopOutcome::CircuitBreak(
            CircuitBreakReason::StepBudget,
        ))));
    }
    if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
        return Ok(PhaseResult::Complete(Some(LoopOutcome::Superseded {
            previous_input_revision: session.input_revision,
            current_input_revision,
        })));
    }

    if crate::ai_agent::data_access::retained_context_eligible(session) {
        agent_data::fresh_access(deps, session, current_unix_ms(deps.clock)?).await?;
    }

    let disclosure_enabled = session.surface == AgentSessionSurface::AiAgent
        && deps.provider_registry.is_some()
        && deps.capability_inventory.is_some();
    if disclosure_enabled
        && session.capability_disclosure.focus_input_revision != session.input_revision
    {
        session
            .capability_disclosure
            .reset_for_input(session.input_revision);
    }
    let authority = if disclosure_enabled {
        deps.tools.current_grant_disclosure().await?
    } else {
        None
    };
    let mut step_inventory = deps.capability_inventory.map(|items| items.to_vec());
    if session.agent_role.workbench_id().is_some()
        && let Some((inventory, providers)) = step_inventory.as_mut().zip(deps.provider_registry)
    {
        inventory.retain(|item| {
            providers
                .capability_for_tool(&item.tool_name)
                .is_some_and(|capability| {
                    session
                        .agent_role
                        .allows_effect(ToolEffect::from_capability_effect(capability.wire.effect))
                })
        });
    }
    if session.uses_request_device_tools()
        && let Some((inventory, providers)) = step_inventory.as_mut().zip(deps.provider_registry)
    {
        // Device capabilities are projected from each admitted target below.
        inventory.retain(|item| {
            providers
                .capability_for_tool(&item.tool_name)
                .is_some_and(|capability| {
                    capability.wire.execution_locality
                        == desk_agent_protocol::capability_provider::ExecutionLocality::Central
                })
        });
        session
            .capability_disclosure
            .loaded_tool_names
            .retain(|name| inventory.iter().any(|item| item.tool_name == *name));
    }
    if let Some((snapshot, inventory)) = authority.as_ref().zip(step_inventory.as_mut()) {
        snapshot.narrow_inventory(
            deps.provider_registry.expect("disclosure registry"),
            inventory,
        );
    }
    let authority_grants = authority.as_ref().map(|snapshot| {
        snapshot.subject_grants(
            session,
            deps.provider_registry.expect("disclosure registry"),
        )
    });
    let projection_now = if authority.is_some() {
        current_unix_ms(deps.clock)?
    } else {
        0
    };
    let authority_names =
        authority
            .as_ref()
            .zip(authority_grants.as_ref())
            .map(|(snapshot, grants)| {
                crate::grant_disclosure::active_tool_names(
                    grants,
                    session,
                    projection_now,
                    snapshot.readiness_revision,
                )
            });
    let planning_goal = if session.surface == AgentSessionSurface::AiAgent
        && session.agent_role.is_main()
        && session.trigger_origin == TriggerOrigin::User
    {
        deps.session_seam
            .load_goal_for_planning(session)
            .await?
            .map(Box::new)
    } else {
        None
    };
    let latest_completed_goal = if session.surface == AgentSessionSurface::AiAgent
        && session.agent_role.is_main()
        && session.trigger_origin == TriggerOrigin::User
        && planning_goal.is_none()
    {
        deps.session_seam
            .load_latest_completed_goal(session)
            .await?
            .map(Box::new)
    } else {
        None
    };

    let data_access = if crate::ai_agent::data_access::eligible_origin(session) {
        deps.session_seam
            .agent_data_access(session, current_unix_ms(deps.clock)?)
            .await?
    } else {
        None
    };
    let mut callable_registry = deps
        .registry
        .iter()
        .filter(|tool| {
            !crate::ai_agent::data_tools::is_data_tool(tool.name())
                && !crate::ai_agent::data_tools::is_data_effect(tool.effect)
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(access) = &data_access {
        access.validate().map_err(crate::subagent::invalid)?;
        callable_registry.extend(crate::ai_agent::data_tools::for_session(
            session,
            &access.settings,
            deps.session_seam.memory().is_some(),
            deps.session_seam.memory_mutation().is_some(),
            deps.session_seam.conversation_search().is_some(),
            current_unix_ms(deps.clock)?,
        ));
    }
    if let Some(tool) =
        crate::external_tools::read_tool(&deps.tools.external_issue_access(session).await?)?
    {
        callable_registry.push(tool);
    }
    if let Some(tool) = crate::issue_reply::tool(&deps.tools.issue_reply_access(session).await?)? {
        callable_registry.push(tool);
    }
    let skill_catalog: Box<crate::skills::SkillCatalog> = if session.surface
        == AgentSessionSurface::AiAgent
        && deps.model.command_completion_event_id().is_none()
    {
        session.skills.reset_for_input(session.input_revision);
        let request = crate::skills::catalog_request(session);
        let catalog =
            crate::future::phase(|| deps.session_seam.skill_catalog(session, &request)).await?;
        callable_registry.push(session.skills.tool(session.input_revision, &catalog));
        Box::new(catalog)
    } else {
        Box::default()
    };
    let mut exposed = exposed_for_session(&callable_registry, session);
    if deps.session_seam.subagents(session).is_none() {
        exposed.retain(|tool| crate::subagent::tools::effect(tool.name()).is_none());
    }
    let policy_read_names = if planning_goal.is_some() {
        Vec::new()
    } else if let Some(snapshot) = &authority {
        exposed
            .iter()
            .filter(|tool| {
                tool.effect == ToolEffect::ReadOnly
                    && step_inventory.as_deref().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item.tool_name == tool.name() && item.callable())
                    })
                    && deps
                        .provider_registry
                        .and_then(|r| r.capability_for_tool(tool.name()))
                        .is_some_and(|capability| {
                            crate::capability_risk::classify_provider_descriptor_floor(
                                capability.wire.effect,
                                &capability.wire.data_policy,
                            ) == desk_agent_protocol::capability_grant::CapabilityRiskTier::R0
                                || tool.has_device_capability(&snapshot.policy_read_capabilities)
                        })
            })
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if let Some(names) = &authority_names {
        exposed.retain(|tool| {
            let Some(capability) = deps
                .provider_registry
                .and_then(|r| r.capability_for_tool(tool.name()))
            else {
                return true;
            };
            if capability.wire.execution_locality
                != desk_agent_protocol::capability_provider::ExecutionLocality::Central
                && !authority.as_ref().is_some_and(|snapshot| {
                    tool.has_device_capability(&snapshot.ready_capabilities)
                })
            {
                return false;
            }
            // Frozen tasks derive per-call grants at dispatch. Current
            // readiness still applies; task admission remains authoritative.
            if session.trigger_origin == crate::session::TriggerOrigin::ScheduledTask {
                return true;
            }
            // A loaded tool without a grant stays callable: the call itself
            // becomes the approval request.
            names.contains(tool.name())
                || (session
                    .capability_disclosure
                    .loaded_tool_names
                    .iter()
                    .any(|name| name == tool.name())
                    && (session.trigger_origin.uses_call_time_approval()
                        || policy_read_names.iter().any(|name| name == tool.name())))
        });
    }
    if planning_goal.is_some() {
        // A stopped goal's user turn is only for clarification or a new
        // owner-reviewed goal proposal. It cannot perform device actions.
        exposed.retain(|tool| {
            matches!(
                tool.effect,
                ToolEffect::GoalOpenPlanning
                    | ToolEffect::ConversationHistory
                    | ToolEffect::AgentHistory
                    | ToolEffect::MemoryQuery
                    | ToolEffect::MemoryWrite
                    | ToolEffect::RunProjection
            )
        });
    }
    let goal_control_only = session.trigger_origin == TriggerOrigin::GoalContinuation
        && (state.goal_correction_attempted
            || deps
                .max_steps_per_turn
                .saturating_sub(session.current_turn_steps)
                <= 2
            || crate::focus_epoch::MAX_FOCUS_EPOCH_STEPS
                .saturating_sub(session.focus_epoch.steps_used)
                <= 2);
    if goal_control_only {
        exposed.retain(|tool| tool.effect == ToolEffect::GoalControl);
        if exposed.is_empty() {
            return Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "goal control tool is unavailable for the claimed segment".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            });
        }
    }
    if session.uses_request_device_tools() {
        exposed.retain(|tool| {
            !deps
                .provider_registry
                .and_then(|registry| registry.capability_for_tool(tool.name()))
                .is_some_and(|descriptor| {
                    descriptor.target_binding
                        == crate::provider_registry::TargetBinding::RequestDevice
                })
        });
    }
    // The model can consume the preceding screenshot in this request.
    // Only targeting in the screenshot-producing batch is fenced below.
    let request_devices = if session.uses_request_device_tools()
        && !session.has_admitted_targetless_input()
        && deps.model.command_completion_event_id().is_none()
    {
        crate::future::boxed(|| deps.tools.request_devices(session)).await?
    } else {
        Vec::new()
    };
    let selected_reads = if !goal_control_only
        && planning_goal.is_none()
        && crate::request_devices::accepts_device_reads(session)
    {
        crate::request_devices::read_tools(
            &request_devices,
            session,
            deps.provider_registry
                .unwrap_or_else(|| crate::ai_agent::ai_agent_provider_registry_ref()),
            deps.model.supports_image_input(),
            deps.tools.original_read_context(),
        )
    } else {
        Vec::new()
    };
    if let Some(inventory) = step_inventory.as_mut() {
        for (tool, _) in &selected_reads {
            let providers = deps
                .provider_registry
                .expect("selected reads have a Provider inventory");
            let descriptor = providers
                .capability_for_tool(tool.name())
                .expect("selected read is registered");
            let provider = providers
                .provider_for_capability(&descriptor.wire.capability_id)
                .expect("selected read has a Provider");
            inventory.push(crate::capability_availability::CapabilityAvailability {
                provider_id: provider.wire.provider_id.clone(),
                capability_id: descriptor.wire.capability_id.clone(),
                tool_name: tool.name().into(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: true,
                reason: None,
            });
        }
    }
    // Selected-device tools without a grant stay callable: the host answers
    // a call without a matching grant with a call-time proposal.
    for (tool, callable) in &selected_reads {
        exposed.retain(|candidate| candidate.name() != tool.name());
        if *callable || session.trigger_origin.uses_call_time_approval() {
            exposed.push(tool);
        }
    }
    let selected_directory = (!goal_control_only && planning_goal.is_none())
        .then(|| crate::directory_tools::selected_tool(session, &request_devices))
        .flatten();
    if session.uses_request_device_tools() {
        exposed.retain(|candidate| candidate.name() != crate::directory_tools::REQUEST_DIRECTORY);
    }
    if let Some(tool) = &selected_directory {
        exposed.push(tool);
    }
    let raw_provider_exposed = exposed
        .iter()
        .filter(|tool| {
            deps.provider_registry
                .is_some_and(|registry| registry.capability_for_tool(tool.name()).is_some())
                && (!session.uses_request_device_tools()
                    || step_inventory.as_ref().is_none_or(|inventory| {
                        inventory.iter().any(|item| item.tool_name == tool.name())
                    }))
        })
        .map(|tool| (*tool).clone())
        .collect::<Vec<_>>();
    let inferred_permission_candidates =
        if planning_goal.is_none() && deps.capability_permission_candidates.is_empty() {
            deps.registry
                .iter()
                .filter(|tool| {
                    deps.provider_registry
                        .is_some_and(|registry| registry.capability_for_tool(tool.name()).is_some())
                        && !raw_provider_exposed
                            .iter()
                            .any(|exposed| exposed.name() == tool.name())
                        && step_inventory.as_deref().is_some_and(|inventory| {
                            inventory
                                .iter()
                                .any(|item| item.tool_name == tool.name() && item.callable())
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
    let mut permission_candidates = if planning_goal.is_some() {
        &[][..]
    } else if deps.capability_permission_candidates.is_empty() {
        inferred_permission_candidates.as_slice()
    } else {
        deps.capability_permission_candidates
    }
    .iter()
    .filter(|tool| session.agent_role.allows_effect(tool.effect))
    .cloned()
    .collect::<Vec<_>>();
    permission_candidates.extend(selected_reads.iter().map(|(tool, _)| tool.clone()));
    let tool_requirements =
        crate::model_capability::ModelRequirements::for_registered_tools(exposed.iter().copied());
    let completion_messages = if let Some(event_id) = deps.model.command_completion_event_id() {
        Some(
            project_command_completion(
                deps,
                session,
                event_id,
                Box::new(ModelRequest::text_only(
                    Vec::new(),
                    deps.response_format.clone(),
                )),
            )
            .await?
            .messages,
        )
    } else {
        None
    };
    let request_requirements =
        tool_requirements.union(crate::model_capability::ModelRequirements::for_messages(
            completion_messages
                .as_deref()
                .unwrap_or(&session.model_conversation()),
        ));
    let mut pinned_context = deps.model.context_policy(request_requirements).await?;
    pinned_context.preserve_history = session.surface == AgentSessionSurface::AiAgent;
    let mut definition_registry = callable_registry.clone();
    for (tool, callable) in &selected_reads {
        definition_registry.retain(|candidate| candidate.name() != tool.name());
        if *callable || session.trigger_origin.uses_call_time_approval() {
            definition_registry.push(tool.clone());
        }
    }

    if disclosure_enabled && let Some(providers) = deps.provider_registry {
        for name in &session.capability_disclosure.advertised_tool_names {
            if definition_registry
                .iter()
                .any(|candidate| candidate.name() == name)
            {
                continue;
            }
            if let Some(capability) = providers.capability_for_tool(name)
                && !(session.uses_request_device_tools()
                    && (crate::request_devices::selected_read_descriptor(capability)
                        || crate::request_devices::selected_action_descriptor(capability)))
            {
                definition_registry.push(capability.registered_tool());
            }
        }
    }
    if disclosure_enabled {
        let candidates = definition_registry
                .iter()
                .filter(|tool| {
                    deps.provider_registry
                        .is_some_and(|r| r.capability_for_tool(tool.name()).is_some())
                        && step_inventory.as_deref().is_some_and(|items| {
                            items.iter().any(|item| {
                                item.tool_name == tool.name() && item.compiled && item.enabled
                                && item.reason != Some(desk_agent_protocol::capability_provider::CapabilityBlockedReason::ModelIncompatible)
                            })
                        })
                        && (session.trigger_origin != crate::session::TriggerOrigin::ScheduledTask
                            || exposed
                                .iter()
                                .any(|candidate| candidate.name() == tool.name()))
                })
                .cloned()
                .collect::<Vec<_>>();
        // Only the call an approval released must be defined: the step runs it
        // before calling the model. Every other callable tool is offered within
        // the byte budget, so approving a directory or many grants cannot make
        // the conversation fail; a tool left out is loaded by name.
        let released_tool = session
            .released_permission_call
            .as_ref()
            .and_then(|released| {
                session
                    .conversation
                    .iter()
                    .rev()
                    .flat_map(|message| message.tool_calls.iter())
                    .find(|call| call.id == released.tool_call_id)
                    .map(|call| call.name.clone())
            });
        let required = released_tool.iter().cloned().collect::<Vec<_>>();
        let callable = raw_provider_exposed
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        // Grants only rank callable tools. A frozen task has no conversation
        // grants; its tools are what it was created with.
        let authorized = if session.trigger_origin == crate::session::TriggerOrigin::ScheduledTask {
            callable.clone()
        } else {
            callable
                .iter()
                .filter(|name| {
                    authority_names
                        .as_ref()
                        .is_some_and(|names| names.contains(*name))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let selected = session.capability_disclosure.select_definitions(
                &candidates,
                &required,
                &callable,
                &authorized,
                released_tool.is_none(),
                pinned_context.max_context_bytes,
            ).map_err(|error| AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!("Provider definition byte budget exceeded: {error:?}; narrow the requested tools. Permissions are unchanged."),
                retryable: false, safe_for_model: true, error_code: None,
            })?;
        exposed.retain(|tool| {
            deps.provider_registry
                .is_none_or(|r| r.capability_for_tool(tool.name()).is_none())
        });
        exposed.extend(
            definition_registry
                .iter()
                .filter(|tool| selected.iter().any(|name| name == tool.name())),
        );
        // Definition state participates in the same fenced session contract.
        deps.session_seam.save(session).await?;
    }
    if goal_control_only {
        // Capability disclosure can re-add Provider definitions after the
        // initial filter. The reserved final steps must remain control-only.
        exposed.retain(|tool| tool.effect == ToolEffect::GoalControl);
    }
    if planning_goal.is_some() {
        exposed.retain(|tool| {
            matches!(
                tool.effect,
                ToolEffect::GoalOpenPlanning
                    | ToolEffect::ConversationHistory
                    | ToolEffect::AgentHistory
                    | ToolEffect::MemoryQuery
                    | ToolEffect::MemoryWrite
                    | ToolEffect::RunProjection
            )
        });
    }

    let selected_permission_tool = exposed
        .iter()
        .find(|tool| tool.name() == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME)
        .and_then(|tool| {
            crate::request_devices::device_permission_tool(
                session,
                &request_devices,
                &selected_reads,
                tool,
            )
        });
    if let Some(tool) = &selected_permission_tool {
        exposed.retain(|candidate| candidate.name() != tool.name());
        exposed.push(tool);
    }
    let can_request_selected_command =
        crate::request_devices::can_request_command(session, &request_devices);
    let offered_command_tool = if !goal_control_only && planning_goal.is_none() {
        deps.provider_registry.and_then(|registry| {
            crate::request_devices::command_tool(session, &request_devices, registry)
        })
    } else {
        None
    };
    if let Some((tool, _)) = &offered_command_tool {
        exposed.retain(|candidate| candidate.name() != tool.name());
        exposed.push(tool);
    }
    let require_tool_choice = goal_control_only;
    exposed.sort_by_key(|tool| tool.name());
    let tool_requirements =
        crate::model_capability::ModelRequirements::for_registered_tools(exposed.iter().copied());
    // An approval resume first runs the call it released; the model is called
    // on the next iteration with that call's result and fresh authority.
    if session.released_permission_call.is_some() {
        let outcome = crate::future::boxed(|| {
            released_dispatch::run(
                deps,
                session,
                turn_id,
                &exposed,
                permission_proposal::ProposalContext {
                    authority: authority.as_ref(),
                    request_devices: &request_devices,
                    selected_reads: &selected_reads,
                    step_inventory: step_inventory.as_deref(),
                    permission_candidates: &permission_candidates,
                    remaining: &[],
                    parent_envelope: None,
                },
                &mut state.mint,
                &mut state.exact_mismatch_hinted,
                sink,
            )
        })
        .await?;
        return Ok(PhaseResult::Complete(outcome));
    }
    let specs = exposed
        .iter()
        .map(|tool| tool.spec.clone())
        .collect::<Vec<_>>();
    let owned_permission_candidates = permission_candidates;
    let owned_exposed = exposed.into_iter().cloned().collect::<Vec<_>>();
    let prompt: PromptPhase<'a> = crate::future::phase(|| async move {
        let exposed = owned_exposed.iter().collect::<Vec<_>>();
        let permission_candidates = owned_permission_candidates.as_slice();
        let mut stable_system = Box::new(deps.system_prompt.clone());
        let disclosed_provider_tools = exposed
            .iter()
            .filter(|tool| {
                deps.provider_registry
                    .is_some_and(|registry| registry.capability_for_tool(tool.name()).is_some())
            })
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        if deps.provider_registry.is_some() {
            crate::ai_agent::scope_disclosed_instructions(
                &mut stable_system,
                &disclosed_provider_tools,
            );
        }
        if session.agent_role.binding().is_some()
            && deps.model.command_completion_event_id().is_none()
        {
            let text = format!(
                "{}\n\n{}",
                stable_system.text,
                crate::subagent::report::REPORT_INSTRUCTION
            );
            stable_system.rewrite_instructions(text);
        }
        let split_runtime = session.surface == AgentSessionSurface::AiAgent;
        let mut system_prompt = if split_runtime {
            Box::new(crate::runtime_context::take(&mut stable_system))
        } else {
            stable_system.clone()
        };

        if !policy_read_names.is_empty() {
            // Registry tool names only.
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::server_text(format!(
                    "\nPOLICY-ELIGIBLE READS: {}. These existing read policies can issue authority at invocation. Load missing parameter details and call them directly; sensitive arguments can still require approval, which the call itself requests.",
                    policy_read_names.join(", ")
                )),
            );
        }
        if session.surface == crate::session::AgentSessionSurface::AiAgent {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::server_text(
                    crate::ai_agent::current_time_prompt(current_unix_ms(deps.clock)?),
                ),
            );
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::directory_tools::scope_prompt(session, current_unix_ms(deps.clock)?),
            );
        }
        if deps
            .registry
            .iter()
            .any(|tool| tool.name() == crate::schedule::proposal::REQUEST_SCHEDULE)
        {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::server_text(
                    crate::schedule::proposal::clock_prompt(session, current_unix_ms(deps.clock)?),
                ),
            );
        }
        if deps.model.command_completion_event_id().is_some() {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::constant(
                    crate::command_completion::INTERPRETATION_INSTRUCTION,
                ),
            );
            if state.completion_protocol_retries > 0 {
                crate::runtime_context::append(
                    &mut system_prompt,
                    &crate::model_content::ModelContent::constant(
                        "\nYour previous response was discarded for emitting a tool invocation. Return only a natural-language interpretation; no additional operation has been executed.",
                    ),
                );
            }
        }
        if session.agent_role.workbench_id().is_some() {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::constant(
                    "\n\nWORKBENCH COORDINATION: Use the available reads to understand the work and its selected targets. Delegate work that changes a device or writes to an external system. Use the original request and report references for follow-up.",
                ),
            );
        }
        if let Some(tag) = session
            .response_locale
            .as_deref()
            .and_then(crate::runtime_context::locale_tag)
        {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::builder()
                    .text("\n\nWrite natural-language answers and progress explanations in BCP-47 locale ")
                    .server(&tag)
                    .text(". Tool names, commands, and JSON fields remain unchanged.")
                    .build(),
            );
        }
        let mut disclosure_projection = None;
        if disclosure_enabled {
            let providers = deps.provider_registry.ok_or_else(|| AgentError {
                kind: AgentErrorKind::Internal,
                message: "AI Agent capability disclosure has no Provider registry".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })?;
            let inventory = step_inventory.as_deref().ok_or_else(|| AgentError {
                kind: AgentErrorKind::Internal,
                message: "AI Agent capability disclosure has no live inventory".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })?;
            let projection = crate::capability_disclosure::project_capability_disclosure(
                providers,
                inventory,
                &raw_provider_exposed,
                permission_candidates,
                &[],
                &session.capability_disclosure,
                pinned_context.max_context_bytes,
            )
            .map_err(|error| AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!("capability disclosure projection failed: {error:?}"),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })?;
            let advertised = exposed.iter().map(|tool| tool.name()).collect();
            // The index and details come from the server's registry and
            // readiness state.
            let index = crate::capability_disclosure::capability_name_index_prompt(
                providers,
                inventory,
                &raw_provider_exposed,
                permission_candidates,
                &advertised,
            )
            .map_err(|error| AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!("capability state exceeds budget: {error:?}"),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })?;
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::model_content::ModelContent::server_text(format!("\n\n{index}")),
            );
            if !projection.detail_prompt.is_empty() {
                crate::runtime_context::append(
                    &mut system_prompt,
                    &crate::model_content::ModelContent::server_text(format!(
                        "\n\n{}",
                        projection.detail_prompt
                    )),
                );
            }
            disclosure_projection = Some(projection);
        }
        if !request_devices.is_empty() {
            crate::runtime_context::append(
                &mut system_prompt,
                &crate::request_devices::catalogue(
                    session,
                    &request_devices,
                    &selected_reads,
                    deps.tools.original_read_context(),
                    can_request_selected_command,
                ),
            );
        }
        // AI denial notices quote reasons traced in this session.
        for event in std::mem::take(&mut system_prompt.denial_notices) {
            let notice =
                crate::approval_review::ai_review_denial_notice(&event, session).map_err(|_| {
                    AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "AI approval denial receipt is invalid".into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    }
                })?;
            if let Some(notice) = notice {
                crate::runtime_context::append(&mut system_prompt, &notice.content);
            }
        }
        crate::future::boxed(|| deliver_pending_results(deps, session)).await?;
        if split_runtime && system_prompt.data_envelope.is_none() {
            let parent = child_runtime_label.as_ref().or_else(|| {
                crate::permission_resume::latest_user_requirement(&session.conversation)
                    .and_then(|message| message.data_envelope.as_ref())
            });
            let destination = deps
                .model
                .model_egress_policy()?
                .map(|policy| policy.destination);
            crate::runtime_context::label(&mut system_prompt, parent, destination.as_ref())?;
        }
        let home_projection = match (
            deps.interactive_user_home,
            deps.interactive_user_home_incarnation,
        ) {
            (Some(home), Some(incarnation)) => interactive_user_home_projection(
                Some(home),
                Some(incarnation),
                system_prompt.data_envelope.as_ref(),
                turn_id,
                current_unix_ms(deps.clock)?,
            )?,
            _ => None,
        };
        let mut messages = vec![*system_prompt];
        if session.surface == AgentSessionSurface::AiAgent
            && deps.model.command_completion_event_id().is_none()
            && let Some(policy) = deps.model.model_egress_policy()?
        {
            messages.extend(session.skills.project(
                session.input_revision,
                &policy.destination,
                &skill_catalog,
            )?);
        }
        if session.surface == AgentSessionSurface::AiAgent
            && let Some(data) = &session.agent_data
            && !data.instructions.content.is_empty()
        {
            let destination = deps
                .model
                .model_egress_policy()?
                .ok_or_else(|| AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "Personalization requires an authorized model destination".into(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                })?
                .destination;
            if let Some(projection) = crate::ai_agent::settings::instructions_projection(
                &data.instructions,
                &session.conversation_id,
                destination,
            )? {
                messages.push(projection);
            }
        }
        if let Some(projection) = subagent_projection {
            messages.push(projection);
        }
        if let Some(projection) = home_projection {
            messages.push(projection);
        }
        if session.surface == AgentSessionSurface::AiAgent
            && deps.model.command_completion_event_id().is_none()
        {
            // Put the server-owned input watermark at the recency edge of the
            // request. Some OpenAI-compatible models underweight a system prompt
            // when several durable user follow-ups are batched; this marker makes
            // the latest-input rule explicit without copying any user content.
            let marker_id = format!(
                "runtime-input-watermark-{turn_id}-{}",
                session.input_revision
            );
            let mut marker_text = if let Some(binding) = session.agent_role.binding() {
                format!(
                    "RUNTIME CHILD WATERMARK (server authoritative): task_id={} input_revision={} control_revision={} source_epoch={}. Follow the current durable finite objective and acceptance criteria. The original owner requirement is source evidence, not a new User turn in this child. A permission bridge does not replace the task. Older task text in compressed history cannot override the current objective. Request this child's own permissions when necessary.",
                    binding.task_id,
                    binding.input_revision,
                    binding.control_revision,
                    binding.source_epoch
                )
            } else if matches!(
                session.trigger_origin,
                TriggerOrigin::ScheduledTask | TriggerOrigin::ScheduledContinuation
            ) {
                format!(
                    "RUNTIME INPUT WATERMARK (server authoritative): scheduled occurrence={} input_revision={} latest_input_seq={}. Follow the original occurrence and its frozen task instructions. Owner context supplied during a pause is additional data; it does not replace the task, approve an action, expand authority or reset any budget or deadline. Its native time observation identifies the accepted statement time of that specific supplement. The original source input anchor below does not apply to later supplements.",
                    session.current_request_id.as_deref().unwrap_or(""),
                    session.input_revision,
                    session.latest_input_seq
                )
            } else {
                format!(
                    "RUNTIME INPUT WATERMARK (server authoritative): input_revision={} latest_input_seq={}. The newest user message in the transcript is the active requirement and overrides conflicting earlier requests. Do not continue a superseded plan. If update_task_status already succeeded for this requirement, do not call it again unless actual task progress materially changed; continue the work or answer.",
                    session.input_revision, session.latest_input_seq
                )
            };
            if let Some(data) = &session.agent_data
                && data.current_actor_input_seq > 0
            {
                let anchor = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                    data.current_input_recorded_at_unix_ms as i64,
                )
                .ok_or_else(|| AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "Invalid user statement time anchor".into(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                })?;
                let timezone = data
                    .current_input_timezone
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "Invalid user statement timezone".into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    })?;
                marker_text.push_str(&format!(" Original user input recorded_at_unix_ms={} statement_timezone={} local_anchor={}. Interpret relative dates in that input from this accepted-input anchor; later processing time does not change it. Recording time alone does not prove when an external action occurred.",
                    data.current_input_recorded_at_unix_ms, data.current_input_timezone, anchor.with_timezone(&timezone).to_rfc3339()));
            } else if let Some(data) = &session.agent_data {
                // Only a timezone the server recognizes is repeated, so the
                // marker stays server text.
                let timezone = crate::ai_agent::recognized_timezone(&data.current_input_timezone);
                marker_text.push_str(&format!(" The original task has no accepted owner-input timestamp. Its frozen statement_timezone is {timezone}. Do not invent an original owner input date; native supplement anchors and external action times remain distinct."));
            }
            let latest_user =
                crate::permission_resume::latest_user_requirement(&session.conversation).cloned();
            let parent = child_runtime_label.as_ref().or_else(|| {
                latest_user
                    .as_ref()
                    .and_then(|message| message.data_envelope.as_ref())
            });
            messages.push(runtime_notice(
                &marker_id,
                &crate::model_content::ModelContent::server_text(marker_text),
                parent,
                crate::model_message_labels::ServerResultSource::InputWatermark,
            )?);
            if let Some(artifact_projection) = requested_artifact_registry_projection(
                &session.model_conversation(),
                &format!(
                    "runtime-requested-artifacts-{turn_id}-{}",
                    session.input_revision
                ),
                current_unix_ms(deps.clock)?,
            )? {
                messages.push(artifact_projection);
            }
            if session.observe_references()? {
                deps.session_seam.save(session).await?;
            }
            if let Some(result_projection) = reusable_provider_result_projection(
                &session.model_conversation(),
                &format!(
                    "runtime-reusable-results-{turn_id}-{}",
                    session.input_revision
                ),
                current_unix_ms(deps.clock)?,
                Some(&session.ui_references),
            )? {
                messages.push(result_projection);
            }
            let consecutive_user_inputs = session
                .conversation
                .iter()
                .rposition(|message| message.role == ChatRole::User)
                .map(|position| {
                    session.conversation[..=position]
                        .iter()
                        .rev()
                        .filter(|message| {
                            !crate::permission_resume::is_permission_resume_message(message)
                                && !crate::permission_resume::is_scheduled_resume_message(message)
                        })
                        .take_while(|message| message.role == ChatRole::User)
                        .count()
                })
                .unwrap_or_default();
            if consecutive_user_inputs > 1
                && let Some(latest_user) = latest_user
            {
                // Repeat the exact latest bytes at the recency edge only for a
                // batched follow-up. The repeated bytes need their own derived
                // DataEnvelope: a sink projection intentionally rejects duplicate
                // envelope ids, even when the bytes are identical. Inheriting the
                // parent's sensitivity/destination while recording lineage keeps
                // that fail-closed invariant intact. Older messages remain in
                // context for additive requirements, but cannot outweigh recency.
                let projection_id =
                    format!("runtime-latest-input-{turn_id}-{}", session.input_revision);
                messages.push(latest_user.repeat_as(
                    projection_id,
                    crate::model_message_labels::ServerResultSource::LatestInputProjection,
                )?);
            }
        }
        if state.empty_end_turn_retries > 0 {
            // Some reasoning-capable OpenAI-compatible providers can end a
            // response after emitting only opaque reasoning, especially after
            // a denied tool call. Retry once with a server-owned recency marker
            // instead of persisting an empty agent message. The marker does
            // not grant authority or request a tool; it only asks the model to
            // produce a protocol-visible answer or call on the next response.
            let marker_id = format!(
                "runtime-empty-end-turn-retry-{turn_id}-{}",
                state.empty_end_turn_retries
            );
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous provider response contained reasoning but no agent text or tool call. Continue the same requirement now with either a visible answer or a valid exposed tool call. This notice grants no permission; a call without authorization asks for approval when made.";
            let parent = child_runtime_label.as_ref().or_else(|| {
                crate::permission_resume::latest_user_requirement(&session.conversation)
                    .and_then(|message| message.data_envelope.as_ref())
            });
            messages.push(runtime_notice(
                &marker_id,
                &crate::model_content::ModelContent::constant(marker_text),
                parent,
                crate::model_message_labels::ServerResultSource::EmptyEndTurnRetry,
            )?);
        }
        if state.truncated_turn_retries > 0 {
            // A MaxTokens response can contain partial text or an incomplete
            // tool call. Nothing from that response is persisted or
            // dispatched. Give the provider one bounded chance to continue
            // with a concise, protocol-visible result while preserving the
            // same durable requirement and already-authorized grants.
            let marker_id = format!(
                "runtime-truncated-turn-retry-{turn_id}-{}",
                state.truncated_turn_retries
            );
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous provider response reached its output-token limit and was discarded before any agent text or tool call was committed. Continue the same requirement now. Do not repeat prior reasoning. Produce only the minimum valid exposed tool call(s) needed for the next step, or a concise visible answer if no tool is needed. Re-read current authorized grants and preserve approved exact inputs. This notice grants no permission.";
            let parent = child_runtime_label.as_ref().or_else(|| {
                crate::permission_resume::latest_user_requirement(&session.conversation)
                    .and_then(|message| message.data_envelope.as_ref())
            });
            messages.push(runtime_notice(
                &marker_id,
                &crate::model_content::ModelContent::constant(marker_text),
                parent,
                crate::model_message_labels::ServerResultSource::TruncatedTurnRetry,
            )?);
        }
        if state.post_tool_permission_protocol_retries > 0 {
            // A malformed permission-planning call after a verified/read-back
            // tool result has not been committed or dispatched. Give the model
            // one bounded chance to rebuild only that JSON call from the current
            // server-authored projections. This never replays the preceding tool
            // and does not grant or widen authority.
            let marker_id = format!(
                "runtime-post-tool-permission-protocol-retry-{turn_id}-{}",
                state.post_tool_permission_protocol_retries
            );
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous request_permissions call had invalid JSON arguments and was discarded before it was recorded or executed. Rebuild exactly one valid request_permissions call from the current capability catalog and CURRENT REUSABLE PROVIDER RESULTS. Copy observed short page and element IDs verbatim, keep the exact downstream tool input bounded, and do not repeat the preceding Provider action. This notice grants no authority; normal permission planning and final server validation remain authoritative.";
            // This server-authored notice contains no Provider output bytes.
            // Inherit the delegated projection or owner requirement, keeping
            // the same model destination and retention without fabricating a
            // User message in a child or exporting the preceding receipt.
            let parent = child_runtime_label.as_ref().or_else(|| {
                crate::permission_resume::latest_user_requirement(&session.conversation)
                    .and_then(|message| message.data_envelope.as_ref())
            });
            messages.push(runtime_notice(
                &marker_id,
                &crate::model_content::ModelContent::constant(marker_text),
                parent,
                crate::model_message_labels::ServerResultSource::PostToolPermissionProtocolRetry,
            )?);
        }
        if state.invalid_tool_argument_retries > 0 {
            // The rejected response was discarded before anything was recorded or
            // dispatched. The notice carries no model output and grants nothing.
            let marker_id = format!(
                "runtime-invalid-tool-arguments-retry-{turn_id}-{}",
                state.invalid_tool_argument_retries
            );
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous response had tool call arguments that were not valid JSON and was discarded before it was recorded or executed. Continue the task and send complete, valid JSON arguments. This notice grants no authority.";
            let parent = child_runtime_label.as_ref().or_else(|| {
                crate::permission_resume::latest_user_requirement(&session.conversation)
                    .and_then(|message| message.data_envelope.as_ref())
            });
            messages.push(runtime_notice(
                &marker_id,
                &crate::model_content::ModelContent::constant(marker_text),
                parent,
                crate::model_message_labels::ServerResultSource::InvalidToolArgumentsRetry,
            )?);
        }
        // Reserve the concrete per-step prompt and API tool bytes before the
        // history window is selected. All transient blocks are included; only
        // protocol framing uses the reserve. Final wire encoding is rechecked.
        const REQUEST_FRAMING_RESERVE_BYTES: usize = 0;
        let history_changed = session.observe_references()?;
        let runtime_changed = session.ui_references.observe(&messages)?;
        let sources = session
            .conversation
            .iter()
            .map(|message| message.message_id.clone())
            .collect();
        let protected = session.protected_references();
        let pruned = session.ui_references.retain_sources(&sources, &protected);
        if history_changed || runtime_changed || pruned {
            deps.session_seam.save(session).await?;
        }
        let runtime: RuntimePhase<'a> = crate::future::phase(|| async move {
            let mut overhead_request = Box::new(ModelRequest::text_only(
                messages
                    .iter()
                    .cloned()
                    .chain(split_runtime.then(|| (*stable_system).clone()))
                    .collect(),
                deps.response_format.clone(),
            ));
            overhead_request.tools = specs.clone();
            overhead_request.ui_references = Some(session.ui_references.clone());
            overhead_request.result_locators =
                crate::result_model_ids::locators(&session.model_conversation());
            let overhead_request = crate::ui_model_ids::project_request(&overhead_request);
            let tool_spec_bytes = serde_json::to_vec(&overhead_request.tools)
                .map_err(|_| {
                    model_context_error(
                        crate::model_context::ModelContextError::ContextCostOverflow,
                    )
                })?
                .len();
            let mut system_prompt_bytes = Some(0usize);
            // Skill projections are reported separately from the system
            // prompt and other overhead; the parts still sum to the same total.
            let (mut skill_system_bytes, mut skill_other_bytes) = (0usize, 0usize);
            let message_overhead_bytes = overhead_request
                .messages
                .iter()
                .try_fold(0usize, |total, message| {
                    let cost = crate::trim::model_context_cost(message);
                    let skill = crate::skills::is_projection(message);
                    if message.role == ChatRole::System {
                        if skill {
                            skill_system_bytes = skill_system_bytes.saturating_add(cost);
                        } else {
                            system_prompt_bytes =
                                system_prompt_bytes.and_then(|bytes| bytes.checked_add(cost));
                        }
                    } else if skill {
                        skill_other_bytes = skill_other_bytes.saturating_add(cost);
                    }
                    total.checked_add(cost)
                })
                .ok_or_else(|| {
                    model_context_error(
                        crate::model_context::ModelContextError::ContextCostOverflow,
                    )
                })?;
            let request_overhead_bytes = message_overhead_bytes
                .checked_add(tool_spec_bytes)
                .and_then(|value| value.checked_add(REQUEST_FRAMING_RESERVE_BYTES))
                .ok_or_else(|| {
                    model_context_error(
                        crate::model_context::ModelContextError::ContextCostOverflow,
                    )
                })?;
            let mut history_policy = pinned_context
                .clone()
                .with_request_overhead_bytes(request_overhead_bytes)
                .map_err(model_context_error)?;
            let request_budget = system_prompt_bytes.and_then(|system_prompt_bytes| {
                Some(crate::context_usage::ContextRequestBudget {
                    total_bytes: pinned_context.max_context_bytes,
                    system_prompt_bytes,
                    tool_definitions_bytes: tool_spec_bytes,
                    other_overhead_bytes: message_overhead_bytes
                        .checked_sub(system_prompt_bytes)?
                        .checked_sub(skill_system_bytes)?
                        .checked_sub(skill_other_bytes)?
                        .checked_add(REQUEST_FRAMING_RESERVE_BYTES)?,
                    skill_bytes: skill_system_bytes + skill_other_bytes,
                })
            });
            if let Some(observation) = &mut session.latest_model_context {
                observation.stale |= observation.source_context_key.as_ref()
                    != Some(&pinned_context.source_context_key)
                    || observation.profile_revision != pinned_context.profile_revision;
            }
            if session.observe_references()? {
                deps.session_seam.save(session).await?;
            }
            let result_locators = crate::result_model_ids::locators(&session.model_conversation());
            for source in &mut session.conversation {
                let mut projected = source.clone();
                projected.prepared_context_cost.0 = None;
                crate::ui_model_ids::project_model_message(
                    &mut projected,
                    Some(&session.ui_references),
                    &result_locators,
                    crate::model_profile::ModelUseCase::Agent,
                );
                crate::thinking_context::project_replay(
                    &mut projected,
                    pinned_context.reasoning_contract,
                );
                if pinned_context.anthropic_prefix_binding {
                    crate::thinking_context::project_known_invalid(
                        &mut projected,
                        &session.thinking_prefix,
                    );
                }
                source.prepared_context_cost.0 = Some(crate::trim::PreparedMessageCost {
                    bytes: crate::trim::model_context_cost(&projected),
                    replay_bytes: crate::trim::model_replay_cost(&projected),
                });
            }
            crate::conversation_attachment::model_read::validate_pending_reads(
                deps.session_seam,
                session,
            )
            .await?;
            let runtime_messages = messages;
            let mut messages = Vec::with_capacity(runtime_messages.len() + 1);
            if split_runtime {
                messages.push(*stable_system);
                // Skills and personalization lead the conversation history
                // (`assemble_context_request` skips them), so owner guidance is
                // never read as the newest input at the request tail.
                let (leading, rest): (Vec<_>, Vec<_>) =
                    runtime_messages.into_iter().partition(|message| {
                        crate::skills::is_projection(message)
                            || crate::ai_agent::settings::is_personalization_projection(message)
                    });
                messages.extend(leading);
                messages.extend(rest);
            } else {
                messages.extend(runtime_messages);
            }
            if session.agent_role.issue_source().is_some()
                && deps.model.command_completion_event_id().is_none()
            {
                messages
                    .extend(crate::future::boxed(|| deps.model.issue_supplements(session)).await?);
            }
            let goal_destination = deps
                .model
                .model_egress_policy()?
                .map(|policy| policy.destination);
            if session.trigger_origin == TriggerOrigin::GoalContinuation {
                let goal = deps.session_seam.load_claimed_goal(session).await?;
                // Goal markers show these message ids; they are rebuilt every step.
                session.ui_references.register_message_ids(
                    [
                        goal.original_source_message_id.as_str(),
                        goal.source_message_id.as_str(),
                    ]
                    .into_iter()
                    .chain(
                        goal.checkpoint.iter().flat_map(|checkpoint| {
                            checkpoint.evidence_ids.iter().map(String::as_str)
                        }),
                    ),
                )?;
                if goal.goal_revision > 1 {
                    let original_source = session
                        .conversation
                        .iter()
                        .find(|message| message.message_id == goal.original_source_message_id)
                        .and_then(|message| message.data_envelope.as_ref())
                        .ok_or_else(|| {
                            crate::goal_tools::invalid(
                                "original goal evidence is unavailable for this model request",
                            )
                        })?;
                    messages.push(crate::goal_projection::original_message(
                        &goal,
                        original_source,
                        goal_destination.as_ref(),
                    )?);
                }
                let source = session
                    .conversation
                    .iter()
                    .find(|message| message.message_id == goal.source_message_id)
                    .and_then(|message| message.data_envelope.as_ref())
                    .ok_or_else(|| AgentError {
                        kind: AgentErrorKind::PermissionDenied,
                        message: "goal source evidence is unavailable for this model request"
                            .into(),
                        retryable: false,
                        safe_for_model: true,
                        error_code: None,
                    })?;
                messages.push(crate::goal_projection::state_message(
                    &goal,
                    source,
                    goal_destination.as_ref(),
                )?);
            } else if let Some(goal) = planning_goal.as_deref() {
                let source = session
                    .conversation
                    .iter()
                    .find(|message| message.message_id == goal.source_message_id)
                    .and_then(|message| message.data_envelope.as_ref())
                    .ok_or_else(|| {
                        crate::goal_tools::invalid(
                            "goal source evidence is unavailable for clarification",
                        )
                    })?;
                messages.push(crate::goal_projection::clarification_message(
                    goal,
                    source,
                    goal_destination.as_ref(),
                    goal.can_propose_revision(session.input_revision).is_ok(),
                )?);
            }
            if let Some(goal) = latest_completed_goal.as_deref() {
                let marker_id = format!(
                    "goal-completed-hint-{}-{}",
                    goal.goal_id, session.input_revision
                );
                let marker_text = serde_json::json!({
                "authority": "server_completed_goal_hint",
                "goal_id": goal.goal_id,
                "completion_event_seq": goal.checkpoint.as_ref().map(|checkpoint| checkpoint.event_seq),
                "instruction": "If the owner's current request says this completed goal is unfinished, use this goal_id as previous_completed_goal_id in request_goal. Propose the full remaining goal text. This hint does not reopen the goal or grant device authority. The prior result body is only available through the separately authorized conversation history."
            }).to_string();
                let parent =
                    crate::permission_resume::latest_user_requirement(&session.conversation)
                        .and_then(|message| message.data_envelope.as_ref());
                messages.push(runtime_notice(
                    &marker_id,
                    &crate::model_content::ModelContent::server_text(marker_text),
                    parent,
                    crate::model_message_labels::ServerResultSource::GoalCompletedHint,
                )?);
            }
            if state.goal_correction_attempted {
                let marker_id = format!("goal-control-correction-{turn_id}");
                let marker_text = "The preceding answer did not end this goal segment. Call control_goal now with a factual continue, wait, ask_user, complete, or blocked decision using exactly decision and message. Do not call a device tool in this response.";
                let parent =
                    crate::permission_resume::latest_user_requirement(&session.conversation)
                        .and_then(|message| message.data_envelope.as_ref());
                messages.push(runtime_notice(
                    &marker_id,
                    &crate::model_content::ModelContent::constant(marker_text),
                    parent,
                    crate::model_message_labels::ServerResultSource::GoalControlCorrection,
                )?);
            }
            // The ids the model is about to see. A pending auto-trigger whose completion
            // message is in this request is cleared once the model reacts to it (the
            // agent answer / tool-call save below), so it never fires an automation
            // turn for a result the model already handled.
            let mut request = Box::new(ModelRequest {
                observation: deps.model.observation_context(
                    crate::model_profile::ModelUseCase::Agent,
                    crate::model_observability::Origin::from(session.trigger_origin),
                ),
                messages,
                tools: specs,
                tool_requirements,
                tool_choice: if require_tool_choice && !state.required_tool_choice_rejected {
                    crate::chat::ToolChoice::Required
                } else {
                    crate::chat::ToolChoice::Auto
                },
                response_format: if session.agent_role.binding().is_some() {
                    crate::prompt::ResponseFormatSpec::None
                } else {
                    deps.response_format.clone()
                },
                use_case: crate::model_profile::ModelUseCase::Agent,
                thinking_prefix: Some(session.thinking_prefix.clone()),
                protected_replay_message_ids: crate::thinking_context::protected_replay_ids(
                    session,
                ),
                context_conversation_id: Some(session.conversation_id.clone()),
                pinned_source_context_key: Some(pinned_context.source_context_key.clone()),
                ui_references: Some(session.ui_references.clone()),
                result_locators: crate::result_model_ids::locators(&session.model_conversation()),
                previous_cache_projection: session.cache_projection.clone(),
                delegation_call: None,
                preparation: None,
                agent_data: None,
                compression_identity: None,
                issue_reply: None,
                caller_output_hard_cap: None,
            });
            if session.surface == crate::session::AgentSessionSurface::TerminalAiAgent
                && let Some(context) = &mut request.observation
            {
                context.attribution.surface = crate::model_observability::Surface::Terminal;
            }
            let _observation_preflight =
                crate::model_observability::PreflightObservation::new(request.observation.clone());
            let mut request = if let Some(event_id) = deps.model.command_completion_event_id() {
                project_command_completion(deps, session, event_id, request).await?
            } else {
                request
            };
            let context: ContextPhase<'a> = crate::future::phase(|| async move {
                let permission_candidates = owned_permission_candidates.as_slice();
                let base_preparation = deps.model.prepare_request(&request)?;
                let context_view = match crate::future::boxed(|| {
                    prepare_model_context(
                        deps,
                        session,
                        turn_id,
                        &mut history_policy,
                        &mut state.compression_attempted,
                        &request,
                        authority.as_ref(),
                        sink,
                    )
                })
                .await
                {
                    Ok(view) => view,
                    Err(error) => return Err(error),
                };
                // Compression may have registered aliases for a new checkpoint.
                request.ui_references = Some(session.ui_references.clone());
                if deps.model.command_completion_event_id().is_none() {
                    request = assemble_context_request(&request, &context_view, session)?;
                }
                let current_memory_bytes = crate::future::boxed(|| {
                    agent_data::current::prepare(deps, session, &mut request, turn_id)
                })
                .await?;
                let mut usage_policy = history_policy.clone();
                usage_policy.request_overhead_bytes = usage_policy
                    .request_overhead_bytes
                    .checked_add(current_memory_bytes)
                    .ok_or_else(|| {
                        model_context_error(
                            crate::model_context::ModelContextError::ContextCostOverflow,
                        )
                    })?;
                let preparation = deps.model.prepare_request(&request)?;
                if let Some(prepared) = &preparation {
                    prepared.require_fit()?;
                }
                let assembled_request_cost = preparation.as_ref().map_or_else(
                    || {
                        request
                            .messages
                            .iter()
                            .map(crate::trim::model_context_cost)
                            .sum::<usize>()
                            .saturating_add(history_policy.request_overhead_bytes)
                    },
                    |prepared| prepared.context_bytes,
                );
                let request_message_ids: HashSet<String> = preparation.as_ref().map_or_else(
                    || {
                        request
                            .messages
                            .iter()
                            .map(|message| message.message_id.clone())
                            .collect()
                    },
                    |prepared| prepared.source_message_ids.iter().cloned().collect(),
                );
                if deps.model.command_completion_event_id().is_none() {
                    let projected_history = request
                        .messages
                        .iter()
                        .filter(|message| {
                            request_message_ids.contains(&message.message_id)
                                && (session
                                    .conversation
                                    .iter()
                                    .any(|source| source.message_id == message.message_id)
                                    || message.role == ChatRole::ContextSummary)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    let mut basis = crate::context_usage::ContextUsageBasis::observe(
                        &session.conversation,
                        &projected_history,
                        &usage_policy,
                    );
                    if let Some(prepared) = &preparation {
                        let overhead = usage_policy.request_overhead_bytes;
                        let base = base_preparation.as_ref().unwrap_or(prepared);
                        let tool_bytes = base.tool_definitions_bytes;
                        // Wire bytes have no per-message split; move the measured
                        // skill share out of the system and other parts.
                        let system_bytes =
                            base.system_prompt_bytes.saturating_sub(skill_system_bytes);
                        let other_bytes = overhead
                            .saturating_sub(base.system_prompt_bytes)
                            .saturating_sub(tool_bytes);
                        let other_without_skills = other_bytes.saturating_sub(skill_other_bytes);
                        basis = basis
                            .with_wire_projection(prepared, overhead)
                            .with_request_budget(Some(
                                crate::context_usage::ContextRequestBudget {
                                    total_bytes: pinned_context.max_context_bytes,
                                    system_prompt_bytes: system_bytes,
                                    tool_definitions_bytes: tool_bytes,
                                    other_overhead_bytes: other_without_skills,
                                    skill_bytes: (base.system_prompt_bytes - system_bytes)
                                        + (other_bytes - other_without_skills),
                                },
                            ));
                    } else {
                        basis = basis.with_request_budget(request_budget);
                    }
                    session.context_usage_basis = Some(basis);
                }
                request.preparation = preparation.map(Box::new);
                let catalog_metrics = deps.capability_catalog_metrics.unwrap_or_default();
                deps.model
                    .on_model_request_projected(crate::seam::ModelRequestProjectionMetrics {
                        static_instruction_bytes: request
                            .messages
                            .first()
                            .map_or(0, |message| message.text.len() as u64),
                        runtime_context_bytes: request
                            .messages
                            .iter()
                            .position(crate::runtime_context::is_runtime)
                            .map_or(0, |start| {
                                request.messages[start..]
                                    .iter()
                                    .map(|message| crate::trim::model_context_cost(message) as u64)
                                    .sum()
                            }),
                        definition_revision: session.capability_disclosure.definition_revision,
                        message_count: u64::try_from(request.messages.len()).unwrap_or(u64::MAX),
                        message_json_bytes: u64::try_from(
                            serde_json::to_vec(&request.messages)
                                .expect("model request messages are serializable")
                                .len(),
                        )
                        .unwrap_or(u64::MAX),
                        advertised_tool_count: u64::try_from(request.tools.len())
                            .unwrap_or(u64::MAX),
                        advertised_tool_json_bytes: u64::try_from(
                            serde_json::to_vec(&request.tools)
                                .expect("model request tools are serializable")
                                .len(),
                        )
                        .unwrap_or(u64::MAX),
                        capability_registry_count: catalog_metrics.registry_count,
                        runtime_ready_count: catalog_metrics.runtime_ready_count,
                        permission_candidate_count: u64::try_from(permission_candidates.len())
                            .unwrap_or(u64::MAX),
                        capability_catalog_utf8_bytes: catalog_metrics.catalog_utf8_bytes,
                        capability_index_utf8_bytes: disclosure_projection.as_ref().map_or(
                            0,
                            |projection| {
                                u64::try_from(projection.index_utf8_bytes).unwrap_or(u64::MAX)
                            },
                        ),
                        loaded_capability_detail_utf8_bytes: disclosure_projection.as_ref().map_or(
                            0,
                            |projection| {
                                u64::try_from(projection.detail_utf8_bytes).unwrap_or(u64::MAX)
                            },
                        ),
                        loaded_capability_count: u64::try_from(
                            session.capability_disclosure.loaded_tool_names.len(),
                        )
                        .unwrap_or(u64::MAX),
                        authorized_provider_tool_count: u64::try_from(raw_provider_exposed.len())
                            .unwrap_or(u64::MAX),
                        conversation_message_count: u64::try_from(session.conversation.len())
                            .unwrap_or(u64::MAX),
                        session_snapshot_json_bytes: u64::try_from(
                            serde_json::to_vec(session)
                                .expect("agent session snapshot is serializable")
                                .len(),
                        )
                        .unwrap_or(u64::MAX),
                        context_attachment_count: u64::try_from(session.context_attachments.len())
                            .unwrap_or(u64::MAX),
                        task_status_item_count: session
                            .task_status_projection
                            .as_ref()
                            .map_or(0, |value| {
                                u64::try_from(value.items.len()).unwrap_or(u64::MAX)
                            }),
                        permission_request_count: u64::try_from(session.permission_requests.len())
                            .unwrap_or(u64::MAX),
                        pending_work_trigger_count: u64::try_from(
                            session.pending_auto_triggers.len(),
                        )
                        .unwrap_or(u64::MAX),
                        unresolved_execution_fact_count: u64::from(!matches!(
                            session.execution_state,
                            ExecutionState::None
                        )),
                    });

                if let Some(current_input_revision) = input_revision_advanced(deps, session).await?
                {
                    return Ok(PhaseResult::Complete(Some(LoopOutcome::Superseded {
                        previous_input_revision: session.input_revision,
                        current_input_revision,
                    })));
                }
                ensure_lease_healthy(deps).await?;
                if disclosure_enabled {
                    let latest = deps.tools.current_grant_disclosure().await?;
                    let now = current_unix_ms(deps.clock)?;
                    let expired_during_projection =
                        authority_grants.as_ref().is_some_and(|grants| {
                            grants.iter().any(|grant| {
                                grant.expires_at_unix_ms > projection_now
                                    && grant.expires_at_unix_ms <= now
                            })
                        });
                    if latest != authority || expired_during_projection {
                        if state.projection_rebuilds >= 2 {
                            return Err(AgentError { kind: AgentErrorKind::PermissionDenied,
                        message: "Authorization changed repeatedly while preparing model context; retry with current permissions.".into(),
                        retryable: true, safe_for_model: true, error_code: None });
                        }
                        state.projection_rebuilds += 1;
                        return Ok(PhaseResult::Complete(None));
                    }
                }
                state.projection_rebuilds = 0;
                // Interpretation is published only after protocol and safety validation.

                let model: ModelPhase<'a> = crate::future::phase(|| async move {
                    let completion_only = deps.model.command_completion_event_id().is_some();
                    state.physical_model_attempt =
                        state.physical_model_attempt.checked_add(1).ok_or_else(|| {
                            crate::subagent::invalid("physical model attempt counter exhausted")
                        })?;
                    let identity = format!("model:{turn_id}:{}", state.physical_model_attempt);
                    crate::future::boxed(|| {
                        agent_data::prepare_request(deps, session, &mut request)
                    })
                    .await?;
                    let budget = match call_budget::reserve_model(
                        deps,
                        session,
                        &mut request,
                        &identity,
                        crate::subagent::reservation::DelegationCallKind::Model,
                    )
                    .await
                    {
                        Ok(budget) => budget,
                        Err(error) => return Err(error),
                    };
                    let mut model_controls = crate::model_controls::FrozenModelControls::capture(
                        session,
                        &request
                            .messages
                            .iter()
                            .filter(|message| request_message_ids.contains(&message.message_id))
                            .cloned()
                            .collect::<Vec<_>>(),
                    );
                    let control_export_authorization = deps
                        .model
                        .model_egress_policy()?
                        .map(|policy| policy.export_authorization_id);
                    let model_tool_definitions = request
                        .tools
                        .iter()
                        .cloned()
                        .map(|mut spec| {
                            crate::ui_model_ids::project_tool(&mut spec);
                            (spec.name.clone(), spec)
                        })
                        .collect::<std::collections::BTreeMap<_, _>>();
                    if let Some(context) = &mut request.observation {
                        context.attribution.contract_revision = format!(
                            "{}.{}",
                            crate::model_observability::DEFINITION_VERSION,
                            session.capability_disclosure.definition_revision
                        );
                        if session.surface == AgentSessionSurface::TerminalAiAgent {
                            context.attribution.surface =
                                crate::model_observability::Surface::Terminal;
                        }
                    }
                    let observation = request.observation.clone();
                    let correction_fence = observation.as_ref().and_then(|context| {
                        crate::model_observability::correction::CorrectionFence::new(
                            context, session,
                        )
                    });
                    let mut correction_projection =
                        crate::model_observability::correction::CorrectionProjection::new(
                            observation.clone(),
                            correction_fence.clone(),
                            &request.messages,
                        );
                    let mut protocol_correction = state
                        .pending_protocol_correction
                        .take()
                        .and_then(|pending| {
                            let marker = match pending.reason {
                                ProtocolCorrectionReason::EmptyResponse => Some(format!(
                                    "runtime-empty-end-turn-retry-{turn_id}-{}",
                                    state.empty_end_turn_retries
                                )),
                                ProtocolCorrectionReason::TruncatedOutput => Some(format!(
                                    "runtime-truncated-turn-retry-{turn_id}-{}",
                                    state.truncated_turn_retries
                                )),
                                ProtocolCorrectionReason::PermissionPlanProtocol => Some(format!(
                                    "runtime-post-tool-permission-protocol-retry-{turn_id}-{}",
                                    state.post_tool_permission_protocol_retries
                                )),
                                ProtocolCorrectionReason::InvalidToolArguments => Some(format!(
                                    "runtime-invalid-tool-arguments-retry-{turn_id}-{}",
                                    state.invalid_tool_argument_retries
                                )),
                                ProtocolCorrectionReason::GoalControlMissing => {
                                    Some(format!("goal-control-correction-{turn_id}"))
                                }
                                ProtocolCorrectionReason::CompletionInterpretation
                                | ProtocolCorrectionReason::ToolChoiceFallback => None,
                            };
                            let cue_present = if let Some(marker) = marker {
                                request.messages.iter().any(|message| {
                                    message.role == ChatRole::SystemEvent
                                        && message.message_id == marker
                                })
                            } else {
                                match pending.reason {
                                    ProtocolCorrectionReason::CompletionInterpretation => {
                                        completion_only
                                            && state.completion_protocol_retries > 0
                                            && request
                                                .messages
                                                .iter()
                                                .any(|message| message.role == ChatRole::System)
                                    }
                                    ProtocolCorrectionReason::ToolChoiceFallback => {
                                        require_tool_choice
                                            && state.required_tool_choice_rejected
                                            && matches!(
                                                request.tool_choice,
                                                crate::chat::ToolChoice::Auto
                                            )
                                    }
                                    _ => false,
                                }
                            };
                            pending.project(
                                observation.clone(),
                                correction_fence.clone(),
                                cue_present,
                            )
                        });
                    let dial_result = if completion_only {
                        dial_request(deps.model, request, &mut crate::seam::NullTurnSink).await
                    } else {
                        dial_request(deps.model, request, sink).await
                    };
                    if dial_result.is_ok()
                        && let Some(projection) = &mut correction_projection
                    {
                        projection.model_returned();
                    }
                    if let Some(correction) = &mut protocol_correction {
                        correction.dial_result(dial_result.is_ok());
                    }
                    call_budget::settle_model(deps, session, budget, &dial_result).await?;
                    if require_tool_choice
                        && !state.required_tool_choice_rejected
                        && dial_result
                            .as_ref()
                            .err()
                            .is_some_and(rejects_required_tool_choice)
                    {
                        // Some compatible gateways reject `required`. Keep the single
                        // tool exposed and retry once with normal tool selection;
                        // this failed request has already been conservatively charged.
                        state.required_tool_choice_rejected = true;
                        state.pending_protocol_correction = PendingProtocolCorrection::open(
                            observation.clone(),
                            correction_fence.clone(),
                            ProtocolCorrectionReason::ToolChoiceFallback,
                        );
                        return Ok(PhaseResult::Complete(None));
                    }
                    let turn = dial_result?;
                    model_controls.retain_exported(
                        turn.provider_meta.data_envelope.as_ref(),
                        control_export_authorization.as_deref(),
                    );
                    model_controls.retain_called_targets(&session.ui_references, &turn.tool_calls);
                    let tool_batch = crate::model_observability::tool::ToolBatch::new(
                        observation.clone(),
                        &turn.tool_calls,
                        &model_tool_definitions,
                    );
                    if child_source_paused(deps, session).await? {
                        sink.on_turn_discarded();
                        return Ok(PhaseResult::Complete(Some(
                            LoopOutcome::DelegationSourcePaused,
                        )));
                    }
                    if let Some(current_input_revision) =
                        input_revision_advanced(deps, session).await?
                    {
                        return Ok(PhaseResult::Complete(Some(LoopOutcome::Superseded {
                            previous_input_revision: session.input_revision,
                            current_input_revision,
                        })));
                    }
                    ensure_lease_healthy(deps).await?;
                    session.cache_projection = turn.provider_meta.cache_projection.clone();
                    session.record_step(turn.usage);
                    if !completion_only {
                        session.latest_model_context =
                            turn.provider_meta.context_observation.clone();
                    }
                    if let Some(prefix) = &turn.provider_meta.thinking_prefix {
                        session
                            .thinking_prefix
                            .invalidated
                            .extend(prefix.invalidated.iter().cloned());
                    }
                    if completion_only
                        && (!turn.tool_calls.is_empty()
                            || crate::command_completion::contains_tool_invocation(&turn.text))
                    {
                        if let Some(context) = &observation {
                            context.output(
                                crate::model_observability::OutputOutcome::InvalidStructuredOutput,
                                crate::model_observability::tool::now_ms(),
                            );
                        }
                        if let Some(correction) = &mut protocol_correction {
                            correction.check(ProtocolCorrectionCheck::Rejected);
                        }
                        if state.completion_protocol_retries >= 1 {
                            return Err(crate::command_completion::invalid_interpretation_error());
                        }
                        state.completion_protocol_retries += 1;
                        state.pending_protocol_correction = PendingProtocolCorrection::open(
                            observation.clone(),
                            correction_fence.clone(),
                            ProtocolCorrectionReason::CompletionInterpretation,
                        );
                        return Ok(PhaseResult::Complete(None));
                    }

                    // Reasoning models may spend the entire completion allowance on opaque
                    // reasoning and return neither user-visible text nor a tool call. Treat
                    // that as an actionable configuration failure instead of the generic
                    // retryable truncation: retrying the same budget is deterministic churn.
                    if turn.stop_reason == crate::chat::StopReason::MaxTokens
                        && turn.provider_meta.reasoning_observed
                        && turn.text.trim().is_empty()
                        && turn.tool_calls.is_empty()
                    {
                        if deps.content_safety.is_enforced() {
                            sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                        } else {
                            sink.on_turn_discarded();
                        }
                        if let Some(context) = &observation {
                            context.output(
                                crate::model_observability::OutputOutcome::OutputTruncated,
                                crate::model_observability::tool::now_ms(),
                            );
                        }
                        if let Some(correction) = &mut protocol_correction {
                            correction.check(ProtocolCorrectionCheck::Rejected);
                        }
                        return Err(reasoning_output_budget_error());
                    }

                    if turn.stop_reason == crate::chat::StopReason::EndTurn
                        && turn.text.trim().is_empty()
                        && turn.tool_calls.is_empty()
                    {
                        if deps.content_safety.is_enforced() {
                            sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                        } else {
                            sink.on_turn_discarded();
                        }
                        if let Some(context) = &observation {
                            context.output(
                                crate::model_observability::OutputOutcome::EmptyResponse,
                                crate::model_observability::tool::now_ms(),
                            );
                        }
                        if let Some(correction) = &mut protocol_correction {
                            correction.check(ProtocolCorrectionCheck::Rejected);
                        }
                        if state.empty_end_turn_retries >= 1 {
                            return Err(empty_end_turn_recovery_error());
                        }
                        state.empty_end_turn_retries += 1;
                        state.pending_protocol_correction = PendingProtocolCorrection::open(
                            observation.clone(),
                            correction_fence.clone(),
                            ProtocolCorrectionReason::EmptyResponse,
                        );
                        return Ok(PhaseResult::Complete(None));
                    }

                    let disposition = match classify_model_turn(&turn) {
                        Ok(disposition) => {
                            tool_batch.protocol(None);
                            if let Some(projection) = &mut correction_projection {
                                projection.response(
                                    &tool_batch,
                                    matches!(
                                        disposition,
                                        TurnDisposition::InvokeTools | TurnDisposition::Answer
                                    ),
                                );
                            }
                            disposition
                        }
                        Err(error) => {
                            if let Some(correction) = &mut protocol_correction {
                                correction.check(ProtocolCorrectionCheck::Rejected);
                            }
                            tool_batch.protocol(Some(&error));
                            if let Some(projection) = &mut correction_projection {
                                projection.response(
                                    &tool_batch,
                                    matches!(
                                        error,
                                        crate::chat::ModelTurnError::InvalidToolArguments { .. }
                                    ),
                                );
                            }
                            if let Some(context) = &observation {
                                context.output(
                        if matches!(
                            error,
                            crate::chat::ModelTurnError::InvalidToolArguments { .. }
                        ) {
                            crate::model_observability::OutputOutcome::InvalidStructuredOutput
                        } else {
                            crate::model_observability::OutputOutcome::InvalidProtocol
                        },
                        crate::model_observability::tool::now_ms(),
                    );
                            }
                            if deps.content_safety.is_enforced() {
                                sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                            } else {
                                sink.on_turn_discarded();
                            }
                            let follows_tool_result =
                                session.conversation.last().is_some_and(|message| {
                                    matches!(
                                        message.role,
                                        ChatRole::Tool | ChatRole::UntrustedOutput
                                    )
                                });
                            let malformed_permission_plan =
                                matches!(
                                    error,
                                    crate::chat::ModelTurnError::InvalidToolArguments { .. }
                                ) && turn.tool_calls.iter().any(|call| {
                                    call.name
                                == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
                                });
                            if session.surface == crate::session::AgentSessionSurface::AiAgent
                                && follows_tool_result
                                && malformed_permission_plan
                                && state.post_tool_permission_protocol_retries < 1
                            {
                                state.post_tool_permission_protocol_retries += 1;
                                state.pending_protocol_correction = PendingProtocolCorrection::open(
                                    observation.clone(),
                                    correction_fence.clone(),
                                    ProtocolCorrectionReason::PermissionPlanProtocol,
                                );
                                return Ok(PhaseResult::Complete(None));
                            }
                            if session.surface == crate::session::AgentSessionSurface::AiAgent
                                && matches!(
                                    error,
                                    crate::chat::ModelTurnError::InvalidToolArguments { .. }
                                )
                                && state.invalid_tool_argument_retries < 1
                            {
                                state.invalid_tool_argument_retries += 1;
                                state.pending_protocol_correction = PendingProtocolCorrection::open(
                                    observation.clone(),
                                    correction_fence.clone(),
                                    ProtocolCorrectionReason::InvalidToolArguments,
                                );
                                return Ok(PhaseResult::Complete(None));
                            }
                            return Ok(PhaseResult::Complete(Some(LoopOutcome::ProtocolError(
                                error,
                            ))));
                        }
                    };
                    if matches!(
                        disposition,
                        TurnDisposition::Discard | TurnDisposition::ContextWindowExceeded
                    ) {
                        if deps.content_safety.is_enforced() {
                            sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                        } else {
                            sink.on_turn_discarded();
                        }
                        if let Some(context) = &observation {
                            context.output(
                                if disposition == TurnDisposition::Discard {
                                    crate::model_observability::OutputOutcome::OutputTruncated
                                } else {
                                    crate::model_observability::OutputOutcome::ContextLimit
                                },
                                crate::model_observability::tool::now_ms(),
                            );
                        }
                        if let Some(correction) = &mut protocol_correction {
                            correction.check(ProtocolCorrectionCheck::Rejected);
                        }
                        if disposition == TurnDisposition::Discard
                            && state.truncated_turn_retries < 1
                        {
                            state.truncated_turn_retries += 1;
                            state.pending_protocol_correction = PendingProtocolCorrection::open(
                                observation.clone(),
                                correction_fence.clone(),
                                ProtocolCorrectionReason::TruncatedOutput,
                            );
                            return Ok(PhaseResult::Complete(None));
                        }
                        return Ok(PhaseResult::Complete(Some(match disposition {
                            TurnDisposition::Discard => LoopOutcome::Truncated,
                            TurnDisposition::ContextWindowExceeded => {
                                LoopOutcome::ContextWindowExceeded
                            }
                            _ => unreachable!("only non-actionable dispositions enter this branch"),
                        })));
                    }

                    let safety_decision = match review_model_turn(deps, &turn).await {
                        Ok(decision) => decision,
                        Err(error) => {
                            sink.on_turn_retracted(
                                StreamRetractionReason::SafetyUnavailable,
                                Some(error.clone()),
                            );
                            return Ok(PhaseResult::Complete(Some(
                                LoopOutcome::ContentSafetyUnavailable(error),
                            )));
                        }
                    };
                    ensure_lease_healthy(deps).await?;
                    if safety_decision != ContentSafetyDecision::Allow {
                        if let Some(context) = &observation {
                            context.output(
                                crate::model_observability::OutputOutcome::PolicyRejected,
                                crate::model_observability::tool::now_ms(),
                            );
                        }
                        append_refusal_placeholder(session, &mut (state.mint), safety_decision);

                        session.clear_reacted_auto_triggers(&request_message_ids);

                        deps.session_seam.save(session).await?;
                        sink.on_turn_retracted(
                            retraction_reason(safety_decision),
                            Some(content_blocked_error()),
                        );
                        return Ok(PhaseResult::Complete(Some(LoopOutcome::ContentRejected(
                            safety_decision,
                        ))));
                    }

                    let terminal_answer = session.surface
                        == crate::session::AgentSessionSurface::TerminalAiAgent
                        && disposition == TurnDisposition::Answer;
                    if !terminal_answer && let Some(context) = &observation {
                        context.output(
                            crate::model_observability::OutputOutcome::Accepted,
                            crate::model_observability::tool::now_ms(),
                        );
                    }
                    if let Some(correction) = &mut protocol_correction {
                        let passed = match correction.reason {
                            ProtocolCorrectionReason::GoalControlMissing => {
                                turn.tool_calls.len() == 1
                                    && turn.tool_calls[0].name
                                        == crate::goal_tools::CONTROL_GOAL_TOOL_NAME
                            }
                            ProtocolCorrectionReason::PermissionPlanProtocol => turn
                                .tool_calls
                                .len()
                                == 1
                                && turn.tool_calls[0].name
                                    == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                            _ => true,
                        };
                        correction.check(if passed {
                            ProtocolCorrectionCheck::Passed
                        } else {
                            ProtocolCorrectionCheck::Rejected
                        });
                    }

                    let tools: ToolPhase<'a> = crate::future::phase(|| async move {
                        let exposed = owned_exposed.iter().collect::<Vec<_>>();
                        let permission_candidates = owned_permission_candidates.as_slice();
                        let _observation_preflight = _observation_preflight;
                        let _correction_projection = correction_projection;
                        let _protocol_correction = protocol_correction;
                        let mut observed_sink = tool_batch.observe_sink(sink);
                        let sink: &mut dyn TurnSink = &mut observed_sink;
                        crate::conversation_attachment::model_read::mark_consumed(
                            &mut session.conversation,
                            &request_message_ids,
                        );
                        let mut attachment_read_budget = pinned_context
                            .max_context_bytes
                            .saturating_sub(assembled_request_cost.saturating_add(4096));
                        match disposition {
                            TurnDisposition::Answer => {
                                if let Some(binding) = session.agent_role.binding()
                                    && deps.model.command_completion_event_id().is_none()
                                {
                                    deps.session_seam
                                        .subagents(session)
                                        .ok_or_else(|| {
                                            crate::subagent::invalid(
                                                "durable child answer validation is unavailable",
                                            )
                                        })?
                                        .evaluate_child_answer(
                                            session,
                                            crate::subagent::state::PlanningFence {
                                                input_revision: binding.input_revision,
                                                control_revision: binding.control_revision,
                                                source_epoch: binding.source_epoch,
                                            },
                                            &turn.text,
                                        )
                                        .await?;
                                }
                                let mut message = ChatMessage::text(
                                    (state.mint)(),
                                    crate::chat::ChatRole::Assistant,
                                    turn.text.clone(),
                                )
                                .with_turn_id(session.current_turn_id.clone().unwrap_or_default());
                                if let Some(prefix) = &turn.provider_meta.thinking_prefix {
                                    session.thinking_prefix.bindings.insert(
                                        message.message_id.clone(),
                                        prefix.request_prefix.clone(),
                                    );
                                }
                                message.set_label(turn.provider_meta.data_envelope.clone());
                                message.replay_disposition = turn.provider_meta.replay.clone();
                                message.reasoning = turn
                                    .provider_meta
                                    .display_reasoning
                                    .as_deref()
                                    .and_then(crate::reasoning_display::bounded);
                                if turn
                                    .tool_calls
                                    .iter()
                                    .any(|call| crate::model_controls::is_control(&call.name))
                                {
                                    message.model_controls = Some(Box::new(model_controls.clone()));
                                }
                                let response_message_id = message.message_id.clone();
                                session.conversation.push(message);
                                // The model reacted to this request; drop any pending auto-trigger
                                // whose completion it saw here so it does not also fire a turn.
                                session.clear_reacted_auto_triggers(&request_message_ids);
                                stage_subagent_observations(
                                    session,
                                    &request_message_ids,
                                    &response_message_id,
                                    true,
                                )?;
                                deps.session_seam.save(session).await?;
                                if session.trigger_origin == TriggerOrigin::GoalContinuation
                                    && !state.goal_correction_attempted
                                    && !session.turn_step_budget_exhausted(deps.max_steps_per_turn)
                                    && !session.focus_epoch.step_budget_exhausted()
                                {
                                    state.goal_correction_attempted = true;
                                    state.pending_protocol_correction =
                                        PendingProtocolCorrection::open(
                                            observation.clone(),
                                            correction_fence.clone(),
                                            ProtocolCorrectionReason::GoalControlMissing,
                                        );
                                    sink.on_partial_committed();
                                    return Ok(None);
                                }
                                if terminal_answer && let Some(context) = observation.clone() {
                                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                        || sink.on_answer_observation(context),
                                    ));
                                }
                                sink.on_answer_committed(&turn.text);
                                // Naming uses the current lease after the saved answer is visible.
                                title::generate(deps, session).await;
                                return Ok(Some(LoopOutcome::Answered(turn.text)));
                            }
                            TurnDisposition::Discard => {
                                unreachable!("discard returned before safety review")
                            }
                            TurnDisposition::ContextWindowExceeded => {
                                unreachable!("context-window stop returned before safety review")
                            }
                            TurnDisposition::InvokeTools => {
                                // Record the agent's tool-call message so the conversation
                                // stays well-formed when replayed to the model.
                                let refs = turn.tool_calls.iter().map(|c| c.to_ref()).collect();
                                let replay = turn.provider_meta.replay.clone().expect(
                                    "classify_model_turn requires tool-call replay disposition",
                                );
                                let mut message = ChatMessage::agent_tool_calls_with_replay(
                                    (state.mint)(),
                                    turn.text.clone(),
                                    refs,
                                    replay,
                                )
                                .with_turn_id(session.current_turn_id.clone().unwrap_or_default());
                                if let Some(prefix) = &turn.provider_meta.thinking_prefix {
                                    session.thinking_prefix.bindings.insert(
                                        message.message_id.clone(),
                                        prefix.request_prefix.clone(),
                                    );
                                }
                                message.set_label(turn.provider_meta.data_envelope.clone());
                                message.normalized_inputs =
                                    turn.provider_meta.normalized_inputs.clone();
                                message.reasoning = turn
                                    .provider_meta
                                    .display_reasoning
                                    .as_deref()
                                    .and_then(crate::reasoning_display::bounded);
                                if turn
                                    .tool_calls
                                    .iter()
                                    .any(|call| crate::model_controls::is_control(&call.name))
                                {
                                    message.model_controls = Some(Box::new(model_controls.clone()));
                                }
                                let response_message_id = message.message_id.clone();
                                session.conversation.push(message);

                                stage_subagent_observations(
                                    session,
                                    &request_message_ids,
                                    &response_message_id,
                                    false,
                                )?;

                                // The model reacted to this request (with tool calls); drop any
                                // pending auto-trigger whose completion it saw here. Persisted with
                                // the tool results at the save below.

                                session.clear_reacted_auto_triggers(&request_message_ids);

                                // Persist every agent tool-call and its replay disposition
                                // before any tool lifecycle event or external action.
                                deps.session_seam.save(session).await?;
                                tool_batch.bind_message(&response_message_id);
                                if deps.content_safety.is_enforced() {
                                    sink.on_partial_committed();
                                }

                                let mut tool_budget_error = None;
                                for call in &turn.tool_calls {
                                    if let Err(error) =
                                        call_budget::charge_tool(deps, session, turn_id, call).await
                                    {
                                        tool_budget_error = Some(error);
                                        break;
                                    }
                                }
                                if let Some(error) = tool_budget_error {
                                    for call in &turn.tool_calls {
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            &crate::model_content::ModelContent::constant(
                                                "not executed: call budget admission failed before dispatch",
                                            ),
                                            ServerResultSource::ToolBudgetAdmissionFailed,
                                        )?;
                                    }
                                    deps.session_seam.save(session).await?;
                                    for call in &turn.tool_calls {
                                        finish_tool(session, &call.id, false, sink);
                                    }
                                    return Err(error);
                                }

                                let forget_targets = if session.surface
                                    == AgentSessionSurface::AiAgent
                                    && session.uses_owner_agent_data()
                                {
                                    match crate::ai_agent::data_tools::prepare_memory_batch(
                                        &turn.tool_calls,
                                        &session.ui_references,
                                    ) {
                                        Ok(targets) => targets,
                                        Err(error) => {
                                            for call in &turn.tool_calls {
                                                append_internal_tool_result(
                                                    session,
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    (state.mint)(),
                                                    &call.id,
                                                    &call_text(
                                                        call,
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                        crate::model_content::ToolError::validation(&error)
                                                            .reason()
                                                            .into(),
                                                    ),
                                                    ServerResultSource::MemoryBatchRejected,
                                                )?;
                                            }
                                            deps.session_seam.save(session).await?;
                                            for call in &turn.tool_calls {
                                                finish_tool(session, &call.id, false, sink);
                                            }
                                            return Ok(None);
                                        }
                                    }
                                } else {
                                    None
                                };

                                if let Some(targets) = forget_targets {
                                    let count = state
                                        .same_tool
                                        .entry(crate::ai_agent::data_tools::MANAGE_MEMORIES.into())
                                        .or_insert(0);
                                    let prior_invocations = *count;
                                    *count = count.saturating_add(
                                        u32::try_from(turn.tool_calls.len()).unwrap_or(u32::MAX),
                                    );
                                    let originally_exposed = exposed.iter().any(|tool| {
                                        tool.name() == crate::ai_agent::data_tools::MANAGE_MEMORIES
                                    });
                                    return crate::future::phase(|| {
                                        agent_data::dispatch_forget(
                                            deps,
                                            session,
                                            &turn.tool_calls,
                                            &targets,
                                            &response_message_id,
                                            originally_exposed,
                                            prior_invocations,
                                            sink,
                                        )
                                    })
                                    .await;
                                }

                                if turn.tool_calls.len() != 1
                                    && turn.tool_calls.iter().any(|call| {
                                        call.name == crate::goal_tools::CONTROL_GOAL_TOOL_NAME
                                            || call.name
                                                == crate::goal_tools::REQUEST_GOAL_TOOL_NAME
                                    })
                                {
                                    for call in &turn.tool_calls {
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            &crate::model_content::ModelContent::constant(
                                                "not executed: goal control or goal opening must be the only tool in its model response",
                                            ),
                                            ServerResultSource::GoalControlBatchRejected,
                                        )?;
                                    }
                                    deps.session_seam.save(session).await?;
                                    for call in &turn.tool_calls {
                                        finish_tool(session, &call.id, false, sink);
                                    }
                                    return Ok(None);
                                }

                                // Mutating tools in one turn run serially; once one is rejected,
                                // times out, or goes to an unknown outcome, the rest of the turn's
                                // calls are not executed (§3). `halted` holds the skip note.
                                let mut halted: Option<crate::model_content::ModelContent> = None;
                                for (call_index, call) in turn.tool_calls.iter().enumerate() {
                                    if child_source_paused(deps, session).await? {
                                        for skipped in &turn.tool_calls[call_index..] {
                                            append_internal_tool_result(
                                                session,
                                                turn.provider_meta.data_envelope.as_ref(),
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
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            note,
                                            ServerResultSource::HaltedToolCall,
                                        )?;
                                        continue;
                                    }

                                    if let Some(current_input_revision) =
                                        input_revision_advanced(deps, session).await?
                                    {
                                        append_superseded_tool_results(
                                            session,
                                            &turn.tool_calls[call_index..],
                                            turn.provider_meta.data_envelope.as_ref(),
                                            &mut (state.mint),
                                        )?;
                                        return Ok(Some(LoopOutcome::Superseded {
                                            previous_input_revision: session.input_revision,
                                            current_input_revision,
                                        }));
                                    }

                                    if let Err(error) = ensure_lease_healthy(deps).await {
                                        for skipped in &turn.tool_calls[call_index..] {
                                            append_internal_tool_result(
                                                session,
                                                turn.provider_meta.data_envelope.as_ref(),
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

                                    // Same-tool repeat circuit breaker.
                                    let count =
                                        state.same_tool.entry(call.name.clone()).or_insert(0);
                                    *count += 1;
                                    if *count > deps.max_same_tool_per_turn {
                                        // Keep the persisted conversation valid for a follow-up:
                                        // every tool call in the agent message must have a
                                        // corresponding result, including calls skipped by this
                                        // circuit breaker.
                                        for skipped in &turn.tool_calls[call_index..] {
                                            append_internal_tool_result(
                                                session,
                                                turn.provider_meta.data_envelope.as_ref(),
                                                (state.mint)(),
                                                &skipped.id,
                                                &call_text(
                                                    skipped,
                                                    turn.provider_meta.data_envelope.as_ref(),
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

                                    if crate::visual_evidence::blocks_targeting(session, &call.name)
                                    {
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
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
                                        if deps.registry.iter().any(|tool| tool.name() == call.name)
                                        {
                                            crate::model_observability::InputIssue::UnexposedTool
                                        } else {
                                            crate::model_observability::InputIssue::UnknownTool
                                        },
                                        None,
                                    );
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            &call_text(
                                                call,
                                                turn.provider_meta.data_envelope.as_ref(),
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
                                    let Some(tool) =
                                        lookup_for_session(&owned_exposed, &call.name, session)
                                    else {
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            &call_text(
                                                call,
                                                turn.provider_meta.data_envelope.as_ref(),
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
                                            crate::model_input::validate_permission_items(
                                                items,
                                                |name| {
                                                    registry.capability_for_tool(name).map(|capability| {
                                                        let mut target = capability.tool_spec.clone();
                                                        crate::ui_model_ids::project_tool(&mut target);
                                                        target.parameters_schema
                                                    })
                                                },
                                            )?;
                                        }
                                        Ok(())
                                    })
                            });
                                        let schema_observed = validation.is_some();
                                        if let Some(Err(error)) = validation {
                                            tool_batch.reject(
                                    call_index,
                                    if error.issue
                                        == crate::model_observability::InputIssue::InvalidJson
                                    {
                                        crate::model_observability::Stage::Json
                                    } else {
                                        crate::model_observability::Stage::Schema
                                    },
                                    error.issue,
                                    error.schema_path.as_deref(),
                                );
                                            append_internal_tool_result(
                                                session,
                                                turn.provider_meta.data_envelope.as_ref(),
                                                (state.mint)(),
                                                &call.id,
                                                &call_text(
                                                    call,
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    error.message,
                                                ),
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
                                    let resolved_call =
                                        match crate::ui_model_ids::resolve_session_call_with_context(
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
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    (state.mint)(),
                                                    &call.id,
                                                    &crate::model_content::ToolError::validation(
                                                        &error,
                                                    )
                                                    .describe(
                                                        call,
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                    ),
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
                                    let selected_device_read =
                                        crate::request_devices::is_selected_read_tool(
                                            tool, session,
                                        );

                                    let selected_browser =
                                        crate::request_devices::is_selected_action_tool(
                                            tool, session,
                                        );
                                    let selected_device_command = call.name
                                        == crate::command_confirmation::COMMAND_TOOL
                                        && crate::request_devices::selected_command_turn(session);
                                    if disclosure_enabled
                                        && !selected_device_read
                                        && !selected_browser
                                        && !selected_device_command
                                        && let Some(providers) = deps.provider_registry
                                        && let Some(capability) =
                                            providers.capability_for_tool(&call.name)
                                        && let Some(snapshot) =
                                            deps.tools.current_grant_disclosure().await?
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
                            || tool.has_device_capability(&snapshot
                                .ready_capabilities);
                                        let policy_read = tool.effect == ToolEffect::ReadOnly
                                && (crate::capability_risk::classify_provider_descriptor_floor(
                                    capability.wire.effect,
                                    &capability.wire.data_policy,
                                )
                                    == desk_agent_protocol::capability_grant::CapabilityRiskTier::R0
                                    || tool.has_device_capability(&snapshot
                                        .policy_read_capabilities));
                                        let ungranted = ready
                                            && session.trigger_origin.uses_call_time_approval()
                                            && !active.contains(&call.name)
                                            && !policy_read;
                                        // A direct call without any grant for this tool becomes
                                        // a call-time proposal; the host still judges calls
                                        // that have some grant against their exact input.
                                        if ungranted
                                            && (tool.effect == ToolEffect::ReadOnly
                                                || session.allows_new_mutation())
                                        {
                                            let proposal = crate::future::phase(|| async {
                                                permission_proposal::propose_call(
                                                    deps,
                                                    session,
                                                    call,
                                                    permission_proposal::ProposalContext {
                                                        authority: authority.as_ref(),
                                                        request_devices: &request_devices,
                                                        selected_reads: &selected_reads,
                                                        step_inventory: step_inventory.as_deref(),
                                                        permission_candidates,
                                                        remaining: &turn.tool_calls
                                                            [call_index + 1..],
                                                        parent_envelope: turn
                                                            .provider_meta
                                                            .data_envelope
                                                            .as_ref(),
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
                                                turn.provider_meta.data_envelope.as_ref(),
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
                                    if let Err(validation_error) =
                                        resolve_word_report_web_source_envelope(session, call)
                                    {
                                        tool_batch.reject(
                                            call_index,
                                            crate::model_observability::Stage::Preflight,
                                            crate::model_observability::InputIssue::Semantic,
                                            None,
                                        );
                                        append_internal_tool_result(
                                            session,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            (state.mint)(),
                                            &call.id,
                                            &crate::model_content::ModelContent::constant(
                                                validation_error,
                                            ),
                                            ServerResultSource::InvalidServerBoundWebEvidence,
                                        )?;
                                        continue;
                                    }

                                    let tool_observation = tool_batch.input(call_index);
                                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                        || {
                                            deps.tools
                                                .observe_tool_input(call, tool_observation.clone());
                                        },
                                    ));
                                    let action: ToolPhase<'_> = match tool.effect {
                                        ToolEffect::ExternalWrite => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let result = crate::future::boxed(|| {
                                                    issue_reply::run(deps, session, call)
                                                })
                                                .await;
                                                let parent =
                                                    turn.provider_meta.data_envelope.as_ref();
                                                let ok = match result {
                                                    Ok(receipt) => {
                                                        let content = serde_json::to_string(
                                                            &receipt,
                                                        )
                                                        .map_err(|_| {
                                                            crate::subagent::invalid(
                                                                "Invalid publication receipt",
                                                            )
                                                        })?;
                                                        if call.name
                                                            == crate::issue_reply::PREPARE_TOOL_NAME
                                                        {
                                                            append_internal_tool_result(
                                                                session,
                                                                parent,
                                                                (state.mint)(),
                                                                &call.id,
                                                                // Ids, states and reason codes of
                                                                // the server's reply record.
                                                                &crate::model_content::ModelContent::server_text(
                                                                    content,
                                                                ),
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
                                                            &crate::model_content::ToolError::host(
                                                                &error,
                                                            ),
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
                                        ToolEffect::MemoryQuery
                                        | ToolEffect::MemoryWrite
                                        | ToolEffect::AgentHistory => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let result_id = (state.mint)();
                                                let result = crate::future::boxed(|| {
                                                    agent_data::dispatch(
                                                        deps,
                                                        session,
                                                        call,
                                                        &response_message_id,
                                                        result_id.clone(),
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                    )
                                                })
                                                .await;
                                                let (message, ok, replayed) = match result {
                                                    Ok(result) => (
                                                        *result.message,
                                                        result.ok,
                                                        result.replayed,
                                                    ),
                                                    // Argument checks, or store errors whose
                                                    // model-safe forms are fixed text
                                                    // (`AgentStoreError::into_agent_error`).
                                                    Err(error) if error.safe_for_model => {
                                                        let message = crate::model_content::ToolError::validation(&error)
                                                            .result_message(
                                                                result_id,
                                                                call,
                                                                turn.provider_meta.data_envelope.as_ref(),
                                                            )?;
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
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let result_message_id = (state.mint)();
                                                let result =
                                                    match crate::subagent::tools::parse_observed(
                                                        session,
                                                        call,
                                                        &tool_observation,
                                                    ) {
                                                        Ok(operation)
                                                            if !operation
                                                                .requires_single_call()
                                                                || turn.tool_calls.len() == 1 =>
                                                        {
                                                            tool_observation.stage(
                                            crate::model_observability::Stage::Preflight,
                                            crate::model_observability::StageOutcome::Attempted,
                                        );
                                                            match deps
                                                                .session_seam
                                                                .subagents(session)
                                                            {
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
                                                                None => {
                                                                    Err(crate::subagent::invalid(
                                                                        "durable delegation is unavailable",
                                                                    ))
                                                                }
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
                                                        let payload_text =
                                                            receipt.payload.to_string();
                                                        let committed = session
                                                            .conversation
                                                            .iter()
                                                            .find(|message| {
                                                                message.message_id
                                                                    == receipt.result_message_id
                                                            })
                                                            .is_some_and(|message| {
                                                                message.role == ChatRole::Tool
                                                                    && message
                                                                        .tool_call_id
                                                                        .as_deref()
                                                                        == Some(call.id.as_str())
                                                                    && message
                                                                        .data_envelope
                                                                        .is_some()
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
                                                            return Ok(Some(
                                                                LoopOutcome::SubAgentYielded,
                                                            ));
                                                        }
                                                        if let Some(wait) = &session.subagent_wait {
                                                            return Ok(Some(
                                                                LoopOutcome::SubAgentsWaiting {
                                                                    wait_id: wait.wait_id.clone(),
                                                                },
                                                            ));
                                                        }
                                                    }
                                                    // Delegation errors are argument checks or
                                                    // fixed host text (`delegation_storage` maps
                                                    // every storage error to a constant).
                                                    Err(error) => {
                                                        append_tool_error(
                                                            session,
                                                            turn.provider_meta
                                                                .data_envelope
                                                                .as_ref(),
                                                            (state.mint)(),
                                                            call,
                                                            &crate::model_content::ToolError::validation(
                                                                &error,
                                                            ),
                                                        )?;
                                                        deps.session_seam.save(session).await?;
                                                        finish_tool(session, &call.id, false, sink);
                                                    }
                                                }

                                                Ok(None)
                                            })
                                        }
                                        ToolEffect::ReadOnly
                                        | ToolEffect::ExternalRead
                                        | ToolEffect::ActionPreview => {
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
                                                        request_devices: &request_devices,
                                                        selected_reads: &selected_reads,
                                                        step_inventory: step_inventory.as_deref(),
                                                        permission_candidates,
                                                        remaining: &turn.tool_calls
                                                            [call_index + 1..],
                                                        parent_envelope: turn
                                                            .provider_meta
                                                            .data_envelope
                                                            .as_ref(),
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
                                                    &turn.tool_calls[call_index + 1..],
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
                                                        request_devices: &request_devices,
                                                        selected_reads: &selected_reads,
                                                        step_inventory: step_inventory.as_deref(),
                                                        permission_candidates,
                                                        remaining: &turn.tool_calls
                                                            [call_index + 1..],
                                                        parent_envelope: turn
                                                            .provider_meta
                                                            .data_envelope
                                                            .as_ref(),
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
                                                    &turn.tool_calls[call_index + 1..],
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
                                            sink.on_tool_started(
                                                &call.name,
                                                &call.id,
                                                &call.arguments_json,
                                            );
                                            match crate::goal_tools::parse(call) {
                                                Ok(proposal) => {
                                                    if proposal.decision
                                                    == crate::goal_tools::GoalDecision::Complete
                                                    && crate::goal_continuation::completion_pending(
                                                        session, &call.id,
                                                    )
                                                {
                                                    append_internal_tool_result(session, turn.provider_meta.data_envelope.as_ref(), (state.mint)(), &call.id,
                                            &crate::model_content::ModelContent::constant("goal_dependencies_pending: background work, a permission decision, or another tool result is still pending. Use continue for independent work or wait for native background results. Do not claim completion."),
                                            ServerResultSource::ControlGoal)?;
                                                    deps.session_seam.save(session).await?;
                                                    finish_tool(session, &call.id, false, sink);
                                                    return Ok(None);
                                                }
                                                    if proposal.decision
                                                        == crate::goal_tools::GoalDecision::Complete
                                                        && let Some(seam) =
                                                            deps.session_seam.subagents(session)
                                                        && !seam
                                                            .required_children_complete(session)
                                                            .await?
                                                    {
                                                        tool_observation.reject(
                                            crate::model_observability::Stage::Preflight,
                                            crate::model_observability::InputIssue::Precondition,
                                        );
                                                        append_internal_tool_result(session, turn.provider_meta.data_envelope.as_ref(), (state.mint)(), &call.id,
                                            &crate::model_content::ModelContent::constant("required_subagents_incomplete: read_subagent_reports for each required request, page through to its current terminal report and assess it against the goal. Finished means the child returned, not that the goal was achieved. Use wait_subagent_reports for unfinished dependencies. A failed or cancelled required dependency cannot be claimed as complete."),
                                            ServerResultSource::ControlGoal)?;
                                                        deps.session_seam.save(session).await?;
                                                        finish_tool(session, &call.id, false, sink);
                                                        return Ok(None);
                                                    }
                                                    let claimed_goal = deps
                                                        .session_seam
                                                        .load_claimed_goal(session)
                                                        .await?;
                                                    let sources = if proposal.decision
                                                        == crate::goal_tools::GoalDecision::Complete
                                                    {
                                                        goal_control::completion_sources(
                                                            session,
                                                            &claimed_goal,
                                                            &request_message_ids,
                                                            &response_message_id,
                                                            deps.registry,
                                                        )
                                                        .inspect_err(|error| {
                                                            tool_observation.input_error(error);
                                                        })?
                                                    } else {
                                                        Vec::new()
                                                    };
                                                    let control = if proposal.decision
                                                        == crate::goal_tools::GoalDecision::Wait
                                                    {
                                                        let Some(wait) = deps
                                                            .session_seam
                                                            .prepare_goal_work_wait(
                                                                session, &call.id,
                                                            )
                                                            .await?
                                                        else {
                                                            append_internal_tool_result(session, turn.provider_meta.data_envelope.as_ref(), (state.mint)(), &call.id,
                                                &crate::model_content::ModelContent::constant("no_goal_background_tasks: no normal background task of this goal is available to wait for. Inspect saved results and choose continue, ask_user, complete, or blocked. Use wait_subagent_reports for child dependencies."),
                                                ServerResultSource::ControlGoal)?;
                                                            deps.session_seam.save(session).await?;
                                                            finish_tool(
                                                                session, &call.id, false, sink,
                                                            );
                                                            return Ok(None);
                                                        };
                                                        crate::goal::GoalControl::WaitForWork {
                                                            wait,
                                                            message: proposal.message,
                                                        }
                                                    } else {
                                                        proposal.into_control(sources).inspect_err(
                                                            |error| {
                                                                tool_observation.input_error(error);
                                                            },
                                                        )?
                                                    };
                                                    tool_observation.stage(
                                            crate::model_observability::Stage::Preflight,
                                            crate::model_observability::StageOutcome::Passed,
                                        );
                                                    append_internal_tool_result(
                                                    session,
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    (state.mint)(),
                                                    &call.id,
                                                    &crate::model_content::ModelContent::json(
                                                        &GoalSettlementReceipt {
                                                            status: "accepted_for_atomic_settlement",
                                                            goal_id: crate::model_content::ServerId::new(
                                                                &claimed_goal.goal_id,
                                                            ),
                                                        },
                                                    )?,
                                                    ServerResultSource::ControlGoal,
                                                )?;
                                                    let summary = match &control {
                                                        crate::goal::GoalControl::WaitForWork {
                                                            message,
                                                            ..
                                                        } => message,
                                                        crate::goal::GoalControl::Continue {
                                                            progress,
                                                            ..
                                                        } => progress,
                                                        crate::goal::GoalControl::Wait {
                                                            reference_id,
                                                            ..
                                                        } => reference_id,
                                                        crate::goal::GoalControl::Complete {
                                                            summary,
                                                            ..
                                                        } => summary,
                                                        crate::goal::GoalControl::Blocked {
                                                            reason,
                                                        } => reason,
                                                    };
                                                    // The visible summary is the model's own
                                                    // argument to this call.
                                                    let issuing =
                                                        turn.provider_meta.data_envelope.as_ref();
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
                                                            source: crate::model_message_labels::ServerResultSource::ControlGoal,
                                                            request_only: false,
                                                        },
                                                    )?
                                                    .with_turn_id(
                                                        session
                                                            .current_turn_id
                                                            .clone()
                                                            .unwrap_or_default(),
                                                    );
                                                    session.conversation.push(message);
                                                    if matches!(
                                                        control,
                                                        crate::goal::GoalControl::Complete { .. }
                                                            | crate::goal::GoalControl::Blocked { .. }
                                                    ) {
                                                        stage_subagent_observations(
                                                            session,
                                                            &request_message_ids,
                                                            &response_message_id,
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
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                        (state.mint)(),
                                                        call,
                                                        &crate::model_content::ToolError::validation(
                                                            &error,
                                                        ),
                                                    )?;
                                                    deps.session_seam.save(session).await?;
                                                    finish_tool(session, &call.id, false, sink);
                                                }
                                            }

                                            Ok(None)
                                        }),
                                        ToolEffect::RunProjection => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let updated_at = (deps.clock)();
                                                let step_id =
                                                    stable_lineage_id("status-step", &call.id);
                                                let current_revision = session
                                                    .task_status_projection
                                                    .as_ref()
                                                    .map_or(0, |projection| projection.revision);
                                                let projection =
                                            crate::task_status_tools::build_task_status_projection(
                                                call,
                                                current_revision,
                                                updated_at.clone(),
                                                step_id,
                                            );
                                                tool_observation.preflight_result(&projection);
                                                match projection {
                                                    Ok(projection) => {
                                                        let content = crate::model_content::ModelContent::json(
                                                            &TaskStatusReceipt {
                                                                status: "updated",
                                                                revision: projection.revision,
                                                                item_count: projection.items.len(),
                                                            },
                                                        )?;
                                                        let result_envelope = crate::model_message_labels::composed_envelope(
                                            &content,
                                            turn.provider_meta.data_envelope.as_ref(),
                                            &call.id,
                                            crate::model_message_labels::ServerResultSource::UpdateTaskStatus,
                                            false,
                                        )?;
                                                        let event_seq = session
                                                            .last_event_seq
                                                            .checked_add(1)
                                                            .ok_or_else(|| {
                                                                AgentError {
                                                        kind: AgentErrorKind::Internal,
                                                        message:
                                                            "agent run event sequence exhausted"
                                                                .into(),
                                                        retryable: false,
                                                        safe_for_model: false,
                                                        error_code: None,
                                                    }
                                                            })?;
                                                        let event = crate::dynamic_run::TaskStatusUpdatedEvent {
                                        event: crate::dynamic_run::AgentRunEvent {
                                            schema_version: crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
                                            event_id: stable_lineage_id(
                                                "status-event",
                                                &(&session.conversation_id, event_seq, &call.id),
                                            ),
                                            run_id: session.conversation_id.clone(),
                                            event_seq,
                                            input_revision: session.input_revision,
                                            kind: crate::dynamic_run::AgentRunEventKind::TaskStatusUpdated,
                                            correlation_id: Some(call.id.clone()),
                                            source_envelope_ids: turn
                                                .provider_meta
                                                .data_envelope
                                                .as_ref()
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
                                                    message: format!(
                                                        "invalid task-status update event: {error}"
                                                    ),
                                                    retryable: false,
                                                    safe_for_model: false,
                                                    error_code: None,
                                                })?;
                                                        session.focus_epoch.bind_task_status();
                                                        session.task_status_projection =
                                                            Some(projection);
                                                        session.last_event_seq = event_seq;
                                                        let message = ChatMessage::tool_result_from(
                                                            (state.mint)(),
                                                            &call.id,
                                                            &content,
                                                            turn.provider_meta.data_envelope.as_ref(),
                                                            crate::model_message_labels::ServerResultSource::UpdateTaskStatus,
                                                        )?;
                                                        debug_assert_eq!(
                                                            message.data_envelope,
                                                            result_envelope
                                                        );
                                                        session.conversation.push(message);
                                                        deps.session_seam
                                                            .save_task_status_update(
                                                                session, &event,
                                                            )
                                                            .await?;
                                                        finish_tool(session, &call.id, true, sink);
                                                    }
                                                    // The task list is built from the call alone.
                                                    Err(error) => {
                                                        append_tool_error(
                                                            session,
                                                            turn.provider_meta.data_envelope.as_ref(),
                                                            (state.mint)(),
                                                            call,
                                                            &crate::model_content::ToolError::validation(
                                                                &error,
                                                            ),
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
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let prepared = (|| -> Result<_, AgentError> {
                                                    if session.surface
                                                        != AgentSessionSurface::AiAgent
                                                        || session.trigger_origin
                                                            != TriggerOrigin::User
                                                    {
                                                        return Err(
                                                            crate::goal_tools::unavailable(),
                                                        );
                                                    }
                                                    let proposal =
                                                        crate::goal_tools::parse_open(call)
                                                            .inspect_err(|error| {
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
                                                        .ok_or_else(
                                                            crate::goal_tools::unavailable,
                                                        )?;
                                                    let model_binding =
                                                crate::goal::GoalModelBinding::from_destination(
                                                    &policy.destination,
                                                )
                                                .map_err(|_| {
                                                    crate::goal_tools::invalid(
                                                        "current model cannot be bound to a goal",
                                                    )
                                                })?;
                                                    let created_at = (deps.clock)();
                                                    let now_unix_ms = u64::try_from(
                                                        chrono::DateTime::parse_from_rfc3339(
                                                            &created_at,
                                                        )
                                                        .map_err(|_| {
                                                            crate::goal_tools::invalid(
                                                                "invalid goal clock",
                                                            )
                                                        })?
                                                        .timestamp_millis(),
                                                    )
                                                    .map_err(|_| {
                                                        crate::goal_tools::invalid(
                                                            "invalid goal clock",
                                                        )
                                                    })?;
                                                    let event_seq = session
                                                        .last_event_seq
                                                        .checked_add(1)
                                                        .ok_or_else(|| {
                                                            crate::goal_tools::invalid(
                                                                "goal event sequence exhausted",
                                                            )
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
                                        tool_observation.reject(crate::model_observability::Stage::Preflight,crate::model_observability::InputIssue::Combination);
                                        return Err(crate::goal_tools::invalid("a revision cannot reference a completed goal"));
                                    }
                                    crate::goal::GoalOpenRequest::new_revision(
                                        request_id, goal, user.message_id.clone(),
                                        session.input_revision, proposal.goal_text, model_binding,
                                        now_unix_ms,
                                    )
                                } else {
                                    crate::goal::GoalOpenRequest::new(
                                        request_id,
                                        session.conversation_id.clone(),
                                        session.actor_id.clone(),
                                        session.single_agent_request_target().map_err(crate::goal_tools::invalid)?.clone(),
                                        session.device_id.clone(),
                                        user.message_id.clone(),
                                        session.input_revision,
                                        proposal.goal_text,
                                        crate::goal::GoalLimits::default(),
                                        model_binding,
                                        now_unix_ms,
                                    )
                                }.map_err(|_| crate::goal_tools::invalid("goal proposal requires a later owner reply and a stopped goal"))?;
                                                    request.previous_completed_goal_id =
                                                        proposal.previous_completed_goal_id;
                                                    request.validate().map_err(|_| {
                                                        crate::goal_tools::invalid(
                                                            "invalid previous completed goal ID",
                                                        )
                                                    })?;
                                                    let event = crate::goal::GoalOpenRequestEvent::new(
                                        &request,
                                        crate::dynamic_run::AgentRunEventKind::GoalOpenRequested,
                                        event_seq,
                                        created_at,
                                    )
                                    .map_err(|_| {
                                        crate::goal_tools::invalid("invalid goal proposal event")
                                    })?;
                                                    Ok(event)
                                                })(
                                                );
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
                                                                    turn.provider_meta.data_envelope.as_ref(),
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
                                                            turn.provider_meta
                                                                .data_envelope
                                                                .as_ref(),
                                                            (state.mint)(),
                                                            &call.id,
                                                            &content,
                                                            ServerResultSource::GoalOpenPending,
                                                        )?;
                                                        session.last_event_seq =
                                                            event.event.event_seq;
                                                        match deps
                                                            .session_seam
                                                            .save_goal_open_request(session, &event)
                                                            .await
                                                        {
                                                            Ok(()) => {
                                                                if let Some(alias)=crate::model_observability::ObservationAlias::goal_open_request(
                                                &session.conversation_id,&event.request.request_id,
                                            ) { tool_observation.bind(alias); }
                                                                tool_observation.permission(crate::model_observability::PermissionOutcome::Waiting);
                                                                finish_tool(
                                                                    session, &call.id, true, sink,
                                                                );
                                                                return Ok(Some(
                                                                LoopOutcome::GoalOpenRequested {
                                                                    request_id: event
                                                                        .request
                                                                        .request_id,
                                                                },
                                                            ));
                                                            }
                                                            Err(error) => {
                                                                std::mem::swap(
                                                                    session,
                                                                    &mut before,
                                                                );
                                                                append_internal_tool_result(
                                                                session,
                                                                turn.provider_meta
                                                                    .data_envelope
                                                                    .as_ref(),
                                                                (state.mint)(),
                                                                &call.id,
                                                                &crate::model_content::ToolError::validation(&error)
    .describe(call, turn.provider_meta.data_envelope.as_ref()),
                                                                ServerResultSource::GoalOpenFailed,
                                                            )?;
                                                                deps.session_seam
                                                                    .save(session)
                                                                    .await?;
                                                                finish_tool(
                                                                    session, &call.id, false, sink,
                                                                );
                                                            }
                                                        }
                                                    }
                                                    Err(error) => {
                                                        append_internal_tool_result(
                                                            session,
                                                            turn.provider_meta
                                                                .data_envelope
                                                                .as_ref(),
                                                            (state.mint)(),
                                                            &call.id,
                                                            &crate::model_content::ToolError::validation(&error)
    .describe(call, turn.provider_meta.data_envelope.as_ref()),
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
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                // Persist the model call before a separate transaction
                                                // compares the exact version and lease and records its result.
                                                deps.session_seam.save(session).await?;
                                                let skill =
                                                    tool.effect == ToolEffect::SkillAuthoring;
                                                let result = if skill {
                                                    deps.session_seam
                                                        .propose_skill(
                                                            session,
                                                            call,
                                                            tool_observation.clone(),
                                                        )
                                                        .await
                                                } else {
                                                    deps.session_seam
                                                        .manage_schedule_tool(
                                                            session,
                                                            call,
                                                            tool_observation.clone(),
                                                        )
                                                        .await
                                                };
                                                if let Err(error) = result {
                                                    append_internal_tool_result(
                                                        session,
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                        (state.mint)(),
                                                        &call.id,
                                                        &crate::model_content::ToolError::validation(&error)
    .describe(call, turn.provider_meta.data_envelope.as_ref()),
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
                                                    if let Some(pause) =
                                                        crate::proposal_review::awaiting_proposal(
                                                            session,
                                                            &call.id,
                                                            turn_id,
                                                            current_unix_ms(deps.clock)?,
                                                        )
                                                    {
                                                        // The turn ends here; the decision resumes it.
                                                        tool_observation.permission(
                                                            crate::model_observability::PermissionOutcome::Waiting,
                                                        );
                                                        append_unstarted_tool_results(
                                                            session,
                                                            &turn.tool_calls[call_index + 1..],
                                                            turn.provider_meta
                                                                .data_envelope
                                                                .as_ref(),
                                                            &mut (state.mint),
                                                            "not executed: waiting for the proposal decision",
                                                            ServerResultSource::ProposalPauseToolCall,
                                                        )?;
                                                        deps.session_seam.save(session).await?;
                                                        sink.on_permission_requested(&pause.id, 1);
                                                        return Ok(Some(
                                                            LoopOutcome::DecisionRequested {
                                                                pause,
                                                            },
                                                        ));
                                                    }
                                                }

                                                Ok(None)
                                            })
                                        }
                                        ToolEffect::DirectoryPlanning => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let resolved =
                                                    match crate::directory_tools::parse_for_request(
                                                        call,
                                                        session.uses_request_device_tools(),
                                                    ) {
                                                        Ok((Some(reference), input)) => deps
                                                            .tools
                                                            .resolve_request_device_directory(
                                                                session,
                                                                &reference,
                                                                &input.path,
                                                            )
                                                            .await
                                                            .map(|(device, resolved)| {
                                                                (device, input, resolved)
                                                            })
                                                            // The device resolves the path; its
                                                            // errors are unverified text.
                                                            .map_err(|error| (error, true)),
                                                        Ok((None, input)) => deps
                                                            .tools
                                                            .resolve_directory_candidate(
                                                                &input.path,
                                                            )
                                                            .await
                                                            .map(|resolved| {
                                                                (
                                                                    session.device_id.clone(),
                                                                    input,
                                                                    resolved,
                                                                )
                                                            })
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
                                                            crate::model_content::ToolError::host(
                                                                &error,
                                                            )
                                                        } else {
                                                            crate::model_content::ToolError::validation(&error)
                                                        };
                                                        append_tool_error(
                                                            session,
                                                            turn.provider_meta
                                                                .data_envelope
                                                                .as_ref(),
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
                                                    &(
                                                        &session.conversation_id,
                                                        session.input_revision,
                                                        &call.id,
                                                    ),
                                                );
                                                // Save the exact held state before the proposal transaction.
                                                // Resolution metadata is for the owner's UI, not model egress.
                                                deps.session_seam.save(session).await?;
                                                let proposal_result = deps
                                                    .session_seam
                                                    .propose_directory(
                                                        session,
                                                        &device,
                                                        crate::directory_tools::proposal(
                                                            request_id.clone(),
                                                            input,
                                                            resolved,
                                                        ),
                                                        current_unix_ms(deps.clock)?,
                                                    )
                                                    .await;
                                                if let Err(error) = proposal_result {
                                                    append_internal_tool_result(
                                                        session,
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                        (state.mint)(),
                                                        &call.id,
                                                        &crate::model_content::ToolError::validation(&error)
                                                            .describe(
                                                                call,
                                                                turn.provider_meta.data_envelope.as_ref(),
                                                            ),
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
                                                tool_observation.permission(
                                            crate::model_observability::PermissionOutcome::Waiting,
                                        );
                                                append_internal_tool_result(
                                                    session,
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    (state.mint)(),
                                                    &call.id,
                                                    &crate::directory_tools::pending_result(
                                                        &request_id,
                                                    )?,
                                                    ServerResultSource::DirectoryProposalPending,
                                                )?;
                                                append_unstarted_tool_results(
                                                    session,
                                                    &turn.tool_calls[call_index + 1..],
                                                    turn.provider_meta.data_envelope.as_ref(),
                                                    &mut (state.mint),
                                                    "not executed: waiting for directory confirmation",
                                                    ServerResultSource::DirectoryPauseToolCall,
                                                )?;
                                                deps.session_seam.save(session).await?;
                                                finish_tool(session, &call.id, true, sink);
                                                sink.on_permission_requested(&request_id, 1);
                                                // A scheduled run pauses for its durable approval
                                                // flow; every other turn ends for a decision that
                                                // resumes the requirement.
                                                if session.trigger_origin
                                                    == crate::session::TriggerOrigin::ScheduledTask
                                                {
                                                    return Ok(Some(
                                                        LoopOutcome::PermissionRequested {
                                                            request_id,
                                                        },
                                                    ));
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
                                        ToolEffect::PermissionPlanning => {
                                            crate::future::phase(|| async {
                                                permission_proposal::propose(
                                                    deps,
                                                    session,
                                                    call,
                                                    call,
                                                    permission_proposal::ProposalContext {
                                                        authority: authority.as_ref(),
                                                        request_devices: &request_devices,
                                                        selected_reads: &selected_reads,
                                                        step_inventory: step_inventory.as_deref(),
                                                        permission_candidates,
                                                        remaining: &turn.tool_calls
                                                            [call_index + 1..],
                                                        parent_envelope: turn
                                                            .provider_meta
                                                            .data_envelope
                                                            .as_ref(),
                                                    },
                                                    &mut state.mint,
                                                    &tool_observation,
                                                    sink,
                                                )
                                                .await
                                            })
                                        }
                                        ToolEffect::SkillLoading => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let result = session.skills.apply(
                                                    session.input_revision,
                                                    call,
                                                    &skill_catalog,
                                                );
                                                tool_observation.preflight_result(&result);
                                                let ok = result.is_ok();
                                                let parent =
                                                    turn.provider_meta.data_envelope.as_ref();
                                                match result {
                                                    Ok((text, root)) => {
                                                        let destination = deps
                                                            .model
                                                            .model_egress_policy()?
                                                            .map(|policy| policy.destination);
                                                        let content = load_skill_content(call, parent, text, root, destination.as_ref())?;
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
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let providers =
                                                    deps.provider_registry.ok_or_else(|| {
                                                        AgentError {
                                                kind: AgentErrorKind::Internal,
                                                message:
                                                    "capability discovery has no Provider registry"
                                                        .into(),
                                                retryable: false,
                                                safe_for_model: false,
                                                error_code: None,
                                            }
                                                    })?;
                                                let inventory =
                                                    step_inventory.as_deref().ok_or_else(|| {
                                                        AgentError {
                                                    kind: AgentErrorKind::Internal,
                                                    message:
                                                        "capability discovery has no live inventory"
                                                            .into(),
                                                    retryable: false,
                                                    safe_for_model: false,
                                                    error_code: None,
                                                }
                                                    })?;
                                                let result = crate::capability_disclosure::apply_load_call(
                                            call,
                                            &mut session.capability_disclosure,
                                            session.input_revision,
                                            &crate::capability_disclosure::CapabilityLoadContext {
                                                registry: providers,
                                                inventory,
                                                max_context_bytes: pinned_context.max_context_bytes,
                                                callable_tools: &raw_provider_exposed,
                                                permission_candidates,
                                            },
                                        );
                                                tool_observation.preflight_result(&result);
                                                let ok = result.is_ok();
                                                let parent =
                                                    turn.provider_meta.data_envelope.as_ref();
                                                match result {
                                                    // Loaded schemas come from the compiled registry.
                                                    Ok(content) => session.conversation.push(
                                                        ChatMessage::tool_result_from(
                                                            (state.mint)(),
                                                            &call.id,
                                                            &crate::model_content::ModelContent::server_text(content),
                                                            parent,
                                                            ServerResultSource::DescribeTools,
                                                        )?,
                                                    ),
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
                                        ToolEffect::ConversationHistory => {
                                            crate::future::phase(|| async {
                                                sink.on_tool_started(
                                                    &call.name,
                                                    &call.id,
                                                    &call.arguments_json,
                                                );
                                                let result = crate::future::boxed(|| {
                                                    retrieval_accounting::dispatch_legacy(
                                                        deps,
                                                        session,
                                                        call,
                                                        &response_message_id,
                                                        (state.mint)(),
                                                        turn.provider_meta.data_envelope.as_ref(),
                                                        &mut attachment_read_budget,
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
                                            })
                                        }
                                    };
                                    if let Some(outcome) = action.await? {
                                        return Ok(Some(outcome));
                                    }
                                }
                                deps.session_seam.save(session).await?;
                                tool_batch.correction_feedback(correction_fence);
                                // Loop again with the tool results in context.
                            }
                        }
                        Ok(None)
                    });
                    Ok(PhaseResult::Ready(tools))
                });
                Ok(PhaseResult::Ready(model))
            });
            Ok(PhaseResult::Ready(context))
        });
        Ok(PhaseResult::Ready(runtime))
    });
    Ok(PhaseResult::Ready(prompt))
}

// Move the owned request into the existing model future outside the driver frame.
#[inline(never)]
pub(super) fn dial_request<'a>(
    model: &'a dyn ModelSeam,
    request: Box<ModelRequest>,
    sink: &'a mut dyn TurnSink,
) -> Pin<Box<dyn Future<Output = Result<crate::chat::ModelTurn, AgentError>> + 'a>> {
    model.call(*request, sink)
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

/// A runtime notice for one request, registered under `parent`.
fn runtime_notice(
    id: &str,
    content: &crate::model_content::ModelContent,
    parent: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
    source: crate::model_message_labels::ServerResultSource,
) -> Result<ChatMessage, AgentError> {
    ChatMessage::from_content(
        ChatRole::SystemEvent,
        id,
        content,
        crate::model_content::Labelling {
            issuing: parent,
            call_id: id,
            source,
            request_only: true,
        },
    )
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
