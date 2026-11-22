//! Plan, publish and pause on one bounded permission proposal.
use super::*;

/// Turn-local inputs the permission planner needs besides the session itself.
#[derive(Clone, Copy)]
pub(super) struct ProposalContext<'a> {
    pub authority: Option<&'a crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub request_devices: &'a [crate::request_devices::RequestDevice],
    pub selected_reads: &'a [(RegisteredTool, bool)],
    pub step_inventory: Option<&'a [crate::capability_availability::CapabilityAvailability]>,
    pub permission_candidates: &'a [RegisteredTool],
    /// The step's later calls, in call order. They keep no result while the
    /// step waits; the continuation processes them after the decision.
    pub remaining: &'a [crate::chat::ToolCall],
    pub parent_envelope: Option<&'a desk_agent_protocol::data_lineage::DataEnvelope>,
    /// The agent message that issued the step.
    pub issuing_message_id: &'a str,
    /// Step-wide inputs to plan the step's approval group; `None` where no
    /// group may form.
    pub planning: Option<&'a permission_group::Planning<'a>>,
}

/// Result of a later call when the step stops at a pending request that an
/// earlier, unrelated turn created: the model calls the paused call again
/// after that decision, so later calls must not overtake it.
const EXISTING_DECISION_CLOSED: &str = "not executed: an earlier call in this step is waiting for an existing permission decision; call it again after that decision if still needed";
/// Result of a call whose request does not fit the bounded request list.
const TOO_MANY_REQUESTS: &str = "not executed: too many approval requests are open in this conversation; decide or let them finish, then call it again if still needed";

/// The marker of a step pausing at `call` for `request_id`: later calls of
/// the step without a result stay pending.
pub(super) fn step_marker(
    session: &crate::session::PersistedAgentSession,
    call_id: &str,
    request_id: &str,
    issuing_message_id: &str,
    remaining: &[crate::chat::ToolCall],
) -> crate::session::AwaitingPermission {
    let issuing = session.conversation.iter().rposition(|message| {
        message.role == ChatRole::Assistant && message.message_id == issuing_message_id
    });
    crate::session::AwaitingPermission {
        tool_call_id: call_id.into(),
        request_id: request_id.into(),
        issuing_message_id: issuing_message_id.into(),
        pending_call_ids: remaining
            .iter()
            .filter(|call| issuing.is_none_or(|index| !session.answered_after(index, &call.id)))
            .map(|call| call.id.clone())
            .collect(),
        ..Default::default()
    }
}

/// Validate `call` as a permission proposal, record it and pause the turn.
/// A rejected proposal is returned to the model as this call's result.
/// `direct` is the model's tool call from which `call` was synthesized with
/// the same id.
#[allow(clippy::too_many_arguments)]
async fn propose<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    direct: &crate::chat::ToolCall,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let ProposalContext {
        remaining,
        parent_envelope,
        issuing_message_id,
        ..
    } = context;
    sink.on_tool_started(&direct.name, &direct.id, &direct.arguments_json);
    let created_at = (deps.clock)();
    let request_id = stable_lineage_id(
        "permission-request",
        &(
            &session.conversation_id,
            session.input_revision,
            session.last_event_seq.saturating_add(1),
            &call.id,
        ),
    );
    let (request, build_failed) = build_request(
        deps,
        session,
        call,
        &request_id,
        &created_at,
        context,
        false,
    )
    .await;
    tool_observation.preflight_result(&request);
    let request = match request {
        Ok(request) => {
            if let Some((denied_request, denied_item)) =
                crate::permission_tools::unchanged_ai_denied_item(
                    &session.permission_requests,
                    &session.permission_decisions,
                    &request,
                )
            {
                tool_observation
                    .permission(crate::model_observability::PermissionOutcome::PolicyRejected);
                Err(AgentError {
                    kind: AgentErrorKind::InvalidInput,
                    message: format!(
                        "permission_denied: this action is unchanged from AI-denied item `{denied_item}` in request `{denied_request}`. Do not rename or repeat it on the same owner input. Narrow the actual action or ask the owner about this specific rejected operation."
                    ),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })
            } else {
                Ok(request)
            }
        }
        Err(error) => Err(error),
    };
    match request {
        Ok(request) => {
            let existing = session
                .permission_requests
                .iter()
                .rev()
                .find(|existing| {
                    crate::permission_tools::equivalent_permission_request(existing, &request)
                })
                .cloned();
            // An approved member of a group that is not settled yet grants
            // nothing so far; the call waits for that group like a pending
            // request. A settled grouped request whose approval minted no
            // grant is renewed like an expired one.
            let waits_on_group = existing.as_ref().and_then(|existing| {
                existing.group.as_ref().filter(|group| {
                    matches!(
                        existing.state,
                        crate::dynamic_run::PermissionRequestState::Approved
                            | crate::dynamic_run::PermissionRequestState::PartiallyApproved
                    ) && !session.permission_group_settled(&group.group_id)
                })
            });
            let waits_on_group = waits_on_group.map(|group| group.group_id.clone());
            let existing = match existing {
                Some(existing)
                    if waits_on_group.is_none()
                        && (existing.state == crate::dynamic_run::PermissionRequestState::Approved
                            || (existing.group.is_some()
                                && existing.state
                                    == crate::dynamic_run::PermissionRequestState::PartiallyApproved))
                        && deps
                            .session_seam
                            .permission_request_can_renew(session, &existing)
                            .await? =>
                {
                    None
                }
                other => other,
            };
            if let (Some(existing), Some(group_id)) = (&existing, &waits_on_group) {
                // The held call runs once the group settles: the last
                // member's resume releases it.
                tool_observation.permission(crate::model_observability::PermissionOutcome::Waiting);
                let mut marker = step_marker(
                    session,
                    &call.id,
                    &existing.request_id,
                    issuing_message_id,
                    remaining,
                );
                marker.group_id = Some(group_id.clone());
                session.pause_for_permission(marker);
                deps.session_seam.save(session).await?;
                finish_tool(session, &call.id, true, sink);
                return Ok(Some(LoopOutcome::PermissionRequested {
                    request_id: existing.request_id.clone(),
                }));
            }
            if let Some(existing) = existing {
                let actor = session
                    .permission_decisions
                    .iter()
                    .find(|decision| {
                        decision.request_id == existing.request_id
                            && decision.input_revision == existing.input_revision
                    })
                    .map(|decision| decision.actor);
                tool_observation.permission(crate::model_observability::permission::state_outcome(
                    existing.state,
                    actor,
                ));
                let decision_state = match existing.state {
                    crate::dynamic_run::PermissionRequestState::Pending => "pending",
                    crate::dynamic_run::PermissionRequestState::NeedsRevalidation => {
                        "needs_revalidation"
                    }
                    crate::dynamic_run::PermissionRequestState::Approved => "approved",
                    crate::dynamic_run::PermissionRequestState::PartiallyApproved => {
                        "partially_approved"
                    }
                    crate::dynamic_run::PermissionRequestState::Denied => "denied",
                    crate::dynamic_run::PermissionRequestState::Replaced => "replaced",
                    crate::dynamic_run::PermissionRequestState::Withdrawn => "withdrawn",
                };
                let content = crate::permission_tools::existing_request_result(
                    &existing.request_id,
                    decision_state,
                )?;
                session.conversation.push(ChatMessage::tool_result_from(
                    mint(),
                    &call.id,
                    &content,
                    parent_envelope,
                    ServerResultSource::PermissionRequestStatus,
                )?);
                finish_tool(session, &call.id, true, sink);
                if existing.state.can_user_decide() {
                    append_unstarted_tool_results(
                        session,
                        remaining,
                        parent_envelope,
                        &mut *mint,
                        EXISTING_DECISION_CLOSED,
                        ServerResultSource::PermissionPauseClosed,
                    )?;
                    deps.session_seam.save(session).await?;
                    sink.on_permission_requested(&existing.request_id, existing.items.len());
                    return Ok(Some(LoopOutcome::PermissionRequested {
                        request_id: existing.request_id,
                    }));
                }
                deps.session_seam.save(session).await?;
                return Ok(None);
            }
            // Later calls whose request needs no earlier result get their
            // cards now, together with this one.
            let members = match context.planning {
                Some(planning) if permission_group::eligible(session) && !remaining.is_empty() => {
                    crate::future::boxed(|| {
                        permission_group::plan(
                            deps,
                            session,
                            &request,
                            context,
                            planning,
                            &created_at,
                        )
                    })
                    .await
                }
                _ => Vec::new(),
            };
            if !members.is_empty() {
                match publish_group(
                    deps,
                    session,
                    call,
                    &request,
                    members,
                    context,
                    &created_at,
                    tool_observation,
                    sink,
                )
                .await?
                {
                    GroupPublication::Published(outcome) => return Ok(Some(outcome)),
                    GroupPublication::Alone => {}
                    GroupPublication::Rejected => {
                        // The host refused this call's own request.
                        tool_observation.permission(
                            crate::model_observability::PermissionOutcome::PolicyRejected,
                        );
                        session.conversation.push(
                            crate::model_content::ToolError::validation(&AgentError {
                                kind: AgentErrorKind::PermissionDenied,
                                message: "the permission request failed the host authority check"
                                    .into(),
                                retryable: false,
                                safe_for_model: true,
                                error_code: None,
                            })
                            .result_message(
                                mint(),
                                call,
                                parent_envelope,
                            )?,
                        );
                        deps.session_seam.save(session).await?;
                        finish_tool(session, &call.id, false, sink);
                        return Ok(None);
                    }
                }
            }
            // A direct call keeps no result while it waits: its approval
            // resume runs it, and any other continuation closes it first.
            let event_seq = next_event_seq(session.last_event_seq)?;
            let event =
                requested_event(session, &request, event_seq, parent_envelope, &created_at)?;
            // The bounded request list keeps every request a decision is still
            // pending for. When none can make room, this call gets a result
            // and the step continues.
            if let Err(error) = session.add_permission_request(request.clone()) {
                if error != crate::dynamic_run::DynamicRunContractError::InvalidPermissionItemCount
                {
                    return Err(AgentError {
                        kind: AgentErrorKind::Internal,
                        message: format!("invalid persisted permission request: {error}"),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    });
                }
                tool_observation
                    .permission(crate::model_observability::PermissionOutcome::PolicyRejected);
                append_internal_tool_result(
                    session,
                    parent_envelope,
                    mint(),
                    &call.id,
                    &crate::model_content::ModelContent::constant(TOO_MANY_REQUESTS),
                    ServerResultSource::PermissionProposalRejected,
                )?;
                deps.session_seam.save(session).await?;
                finish_tool(session, &call.id, false, sink);
                return Ok(None);
            }
            // The step's later calls keep no result: they continue after the
            // decision.
            session.pause_for_permission(step_marker(
                session,
                &call.id,
                &request.request_id,
                issuing_message_id,
                remaining,
            ));
            session.last_event_seq = event_seq;
            deps.session_seam
                .save_permission_request(session, &event)
                .await?;
            if let Some(alias) = crate::model_observability::ObservationAlias::permission_request(
                &session.conversation_id,
                &request.request_id,
            ) {
                tool_observation.bind(alias);
            }
            tool_observation.permission(crate::model_observability::PermissionOutcome::Waiting);
            finish_tool(session, &call.id, true, sink);
            sink.on_permission_requested(&request.request_id, request.items.len());
            return Ok(Some(LoopOutcome::PermissionRequested {
                request_id: request.request_id,
            }));
        }
        Err(error) => {
            let result = if build_failed {
                crate::model_content::ToolError::validation(&error)
            } else {
                // Planning reads host state whose errors are not all server text.
                crate::model_content::ToolError::host(&error)
            };
            session
                .conversation
                .push(result.result_message(mint(), call, parent_envelope)?);
            if session.trigger_origin == crate::session::TriggerOrigin::ScheduledTask {
                append_unstarted_tool_results(
                    session,
                    remaining,
                    parent_envelope,
                    &mut *mint,
                    "not executed: task permission request was rejected",
                    ServerResultSource::TaskPermissionDenied,
                )?;
                finish_tool(session, &call.id, false, sink);
                return Err(error);
            }
            deps.session_seam.save(session).await?;
            finish_tool(session, &call.id, false, sink);
        }
    }

    Ok(None)
}

fn next_event_seq(last: u64) -> Result<u64, AgentError> {
    last.checked_add(1).ok_or_else(|| AgentError {
        kind: AgentErrorKind::Internal,
        message: "agent run event sequence exhausted".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })
}

/// The append-only event publishing `request` at `event_seq`.
fn requested_event(
    session: &crate::session::PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
    event_seq: u64,
    parent_envelope: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
    created_at: &str,
) -> Result<crate::dynamic_run::PermissionRequestedEvent, AgentError> {
    let event = crate::dynamic_run::PermissionRequestedEvent {
        approval_mode_revision: None,
        event: crate::dynamic_run::AgentRunEvent {
            schema_version: crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: stable_lineage_id(
                "permission-event",
                &(&session.conversation_id, event_seq, &request.request_id),
            ),
            run_id: session.conversation_id.clone(),
            event_seq,
            input_revision: session.input_revision,
            kind: crate::dynamic_run::AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: parent_envelope
                .map(|envelope| vec![envelope.envelope_id.clone()])
                .unwrap_or_default(),
            result_envelope_ids: Vec::new(),
            created_at: created_at.to_owned(),
        },
        request: request.clone(),
    };
    event.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("invalid permission request event: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    Ok(event)
}

/// How a planned approval group was published.
enum GroupPublication {
    /// Every request was saved; the step waits for all of them.
    Published(LoopOutcome),
    /// The host refused another member: publish the pausing request alone.
    Alone,
    /// The host refused the pausing request itself.
    Rejected,
}

/// Publish the pausing request and the step's up-front requests as one
/// group in one host transaction. When the host refuses one member, the
/// session is restored as it was before staging, so the pausing request can
/// be published alone and the other calls get their cards when reached.
#[allow(clippy::too_many_arguments)]
async fn publish_group(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    request: &crate::dynamic_run::PermissionRequest,
    members: Vec<permission_group::Member>,
    context: ProposalContext<'_>,
    created_at: &str,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<GroupPublication, AgentError> {
    let staged_requests = session.permission_requests.clone();
    let staged_decisions = session.permission_decisions.clone();
    let staged_event_seq = session.last_event_seq;
    let staged_awaiting = session.awaiting_permission.clone();
    let staged_released = session.released_permission_call.clone();
    let staged_conversation = session.conversation.len();
    let restore = |session: &mut crate::session::PersistedAgentSession| {
        session.permission_requests = staged_requests.clone();
        session.permission_decisions = staged_decisions.clone();
        session.last_event_seq = staged_event_seq;
        session.awaiting_permission = staged_awaiting.clone();
        session.released_permission_call = staged_released.clone();
        session.conversation.truncate(staged_conversation);
    };
    let first_event_seq = next_event_seq(session.last_event_seq)?;
    let group_id = permission_group::group_id(session, first_event_seq, context.issuing_message_id);
    let size = u16::try_from(members.len() + 1).map_err(|_| AgentError {
        kind: AgentErrorKind::Internal,
        message: "permission group is too large".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let mut requests = std::iter::once(request.clone())
        .chain(members.iter().map(|member| member.request.clone()))
        .collect::<Vec<_>>();
    for (order, request) in (0_u16..).zip(requests.iter_mut()) {
        request.group = Some(crate::dynamic_run::PermissionRequestGroup {
            group_id: group_id.clone(),
            order,
            size,
        });
    }
    let mut events = Vec::with_capacity(requests.len());
    let mut event_seq = staged_event_seq;
    for request in &requests {
        event_seq = next_event_seq(event_seq)?;
        events.push(requested_event(
            session,
            request,
            event_seq,
            context.parent_envelope,
            created_at,
        )?);
    }
    for request in &requests {
        if session.add_permission_request(request.clone()).is_err() {
            restore(session);
            return Ok(GroupPublication::Alone);
        }
    }
    let mut marker = step_marker(
        session,
        &call.id,
        &requests[0].request_id,
        context.issuing_message_id,
        context.remaining,
    );
    marker.group_id = Some(group_id);
    marker.pre_requested = members
        .iter()
        .zip(&requests[1..])
        .map(|(member, request)| crate::session::PreRequested {
            call_id: member.call.id.clone(),
            request_id: request.request_id.clone(),
        })
        .collect();
    session.pause_for_permission(marker);
    session.last_event_seq = event_seq;
    if let Err(error) = deps
        .session_seam
        .save_permission_request_group(session, &events)
        .await
    {
        let Some(rejected) = crate::dynamic_run::rejected_group_member(&error) else {
            return Err(error);
        };
        let own = rejected == requests[0].request_id;
        restore(session);
        return Ok(if own {
            GroupPublication::Rejected
        } else {
            GroupPublication::Alone
        });
    }
    let mut observations = vec![tool_observation.clone()];
    if let Some(planning) = context.planning {
        observations.extend(members.iter().map(|member| {
            planning.tool_batch.input(
                planning
                    .calls
                    .iter()
                    .position(|call| call.id == member.call.id)
                    .unwrap_or(usize::MAX),
            )
        }));
    }
    for (request, observation) in requests.iter().zip(&observations) {
        if let Some(alias) = crate::model_observability::ObservationAlias::permission_request(
            &session.conversation_id,
            &request.request_id,
        ) {
            observation.bind(alias);
        }
        observation.permission(crate::model_observability::PermissionOutcome::Waiting);
    }
    finish_tool(session, &call.id, true, sink);
    for request in &requests {
        sink.on_permission_requested(&request.request_id, request.items.len());
    }
    Ok(GroupPublication::Published(
        LoopOutcome::PermissionRequested {
            request_id: requests[0].request_id.clone(),
        },
    ))
}

/// Build and validate the request for `call` with every check publication
/// runs, without publishing it. Returns the request, or the error and
/// whether building the request itself (server validation) failed.
/// `planning` refuses requests that depend on state the step's earlier calls
/// may still change.
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_request(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    request_id: &str,
    created_at: &str,
    context: ProposalContext<'_>,
    planning: bool,
) -> (
    Result<crate::dynamic_run::PermissionRequest, AgentError>,
    bool,
) {
    let ProposalContext {
        authority,
        request_devices,
        selected_reads,
        step_inventory,
        permission_candidates,
        ..
    } = context;
    let request_id = request_id.to_owned();
    let created_at = created_at.to_owned();
    let selected_request = crate::future::boxed(|| {
        crate::request_devices::plan_device_permission(
            deps.tools,
            deps.provider_registry,
            authority.as_ref().map(|snapshot| snapshot.surface),
            session,
            request_devices,
            selected_reads,
            call,
            &request_id,
            &created_at,
        )
    })
    .await;
    // Building the request reads only the call and the registry, so its own
    // failure is server validation; the later checks read host and device
    // state.
    let build_failed = std::cell::Cell::new(false);
    let request = selected_request.unwrap_or_else(|| deps.provider_registry.ok_or_else(|| AgentError {
                                kind: AgentErrorKind::Internal,
                                message: "permission planning has no Provider catalog".into(),
                                retryable: false,
                                safe_for_model: false,
                                error_code: None,
                            }).and_then(|registry| {
                                crate::permission_tools::build_permission_request(
                                    call,
                                    registry,
                                    request_id.clone(),
                                    session.input_revision,
                                    created_at.clone(),
                                )
                                .inspect_err(|_| build_failed.set(true))
                            }).and_then(|mut request| {
                                let providers = deps.provider_registry.ok_or_else(|| AgentError {
                                    kind: AgentErrorKind::Internal,
                                    message: "permission planning has no Provider catalog".into(),
                                    retryable: false,
                                    safe_for_model: false,
                                    error_code: None,
                                })?;
                                let inventory = step_inventory.ok_or_else(|| AgentError {
                                    kind: AgentErrorKind::Internal,
                                    message: "permission planning has no live capability inventory".into(),
                                    retryable: false,
                                    safe_for_model: false,
                                    error_code: None,
                                })?;
                                // An interactive command needs the owner's live terminal
                                // at approval time; unattended turns cannot provide one.
                                if session.trigger_origin != crate::session::TriggerOrigin::User
                                    && request.items.iter().any(|item| {
                                        item.command_confirmation
                                            .as_ref()
                                            .is_some_and(|confirmation| confirmation.is_interactive())
                                    })
                                {
                                    return Err(AgentError {
                                        kind: AgentErrorKind::PermissionDenied,
                                        message: "interactive (PTY) commands need the owner's live terminal and cannot be requested from a goal or scheduled run; use a non-interactive command".into(),
                                        retryable: false,
                                        safe_for_model: true,
                                        error_code: None,
                                    });
                                }
                                crate::application_ui::bind_request(&mut request, &session.conversation)?;
                                crate::permission_tools::include_desktop_action_reads(&mut request, providers)?;
                                validate_permission_request_availability(
                                    &session.agent_role,
                                    &session.conversation,
                                    &request,
                                    providers,
                                    inventory,
                                    deps.registry,
                                    permission_candidates,
                                )?;
                                let now = chrono::DateTime::parse_from_rfc3339(&request.created_at)
                                    .ok()
                                    .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                                    .unwrap_or(0);
                                if request.items.iter().any(|item| crate::provider_preflight::ArtifactCallPreflight::supports(&item.tool_name)) {
                                    let surface = authority.ok_or_else(|| crate::subagent::invalid("artifact permission has no current authorization context"))?.surface;
                                    crate::permission_tools::bind_artifact_request(&mut request, session, providers, surface, now)?;
                                }
                                for item in &request.items {
                                    crate::provider_preflight::text_file::validate_mutation_permission_input(
                                        session,
                                        &item.tool_name,
                                        item.canonical_input_json.as_deref(),
                                        now,
                                    )?;
                                    crate::provider_preflight::text_file::validate_command_restore_permission_input(
                                        session,
                                        providers,
                                        &item.tool_name,
                                        item.canonical_input_json.as_deref(),
                                        now,
                                    )?;
                                    crate::provider_preflight::text_file::validate_read_permission_input_with_context(
                                        session,
                                        &item.tool_name,
                                        item.canonical_input_json.as_deref(),
                                        now,
                                        deps.tools.original_read_context(),
                                    )?;
                                }
                                Ok(request)
                            }));
    let request = match request {
        // A launch request binds to current readiness: planning never
        // creates one ahead of the calls before it.
        Ok(request)
            if planning
                && request
                    .items
                    .iter()
                    .any(|item| item.tool_name == "launch_application") =>
        {
            Err(crate::subagent::invalid(
                "a launch request is created when its call runs",
            ))
        }
        Ok(mut request) => crate::application_launch::bind_permission_request(
            &mut request,
            session,
            authority.map_or(0, |snapshot| snapshot.readiness_revision),
            deps.tools,
        )
        .await
        .map(|()| request),
        Err(error) => Err(error),
    };
    let request = match (request, deps.provider_registry) {
        (Ok(request), Some(registry)) => deps
            .session_seam
            .validate_task_permission_request(session, &request, registry)
            .await
            .map(|()| request),
        (Ok(_), None) => Err(AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: "Permission provider catalog is unavailable".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
        (Err(error), _) => Err(error),
    };
    (request, build_failed.get())
}

/// Turn a direct call that lacks a matching grant into a call-time proposal.
/// An owner refusal of this tool for the current input stays final; other
/// refusals are judged again by the planner against the concrete new action.
#[allow(clippy::too_many_arguments)]
pub(super) async fn propose_call<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if crate::permission_tools::owner_denied_tool(
        &session.permission_requests,
        &session.permission_decisions,
        &call.name,
        session.input_revision,
    ) {
        tool_observation.permission(crate::model_observability::PermissionOutcome::Denied);
        append_internal_tool_result(
            session,
            context.parent_envelope,
            mint(),
            &call.id,
            &crate::model_content::ModelContent::constant(
                "permission_denied: the owner refused this tool for the current request. No action was dispatched. Do not retry it; explain the blocker or ask the owner.",
            ),
            ServerResultSource::PermissionDenied,
        )?;
        finish_tool(session, &call.id, false, sink);
        return Ok(None);
    }
    let application_scope_uses = deps.session_seam.application_scope_default_uses().await;
    let planning = deps
        .provider_registry
        .ok_or_else(|| AgentError {
            kind: AgentErrorKind::Internal,
            message: "permission planning has no Provider catalog".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })
        .and_then(|registry| {
            crate::permission_tools::synthesize_request_call(
                call,
                registry,
                &session.conversation,
                application_scope_uses,
            )
        });
    let planning = planning.and_then(|planning| bind_request_device(planning, call, session));
    let planning = match planning {
        Ok(planning) => planning,
        Err(error) if error.safe_for_model => {
            tool_observation.reject(
                crate::model_observability::Stage::Preflight,
                crate::model_observability::InputIssue::Semantic,
            );
            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
            append_internal_tool_result(
                session,
                context.parent_envelope,
                mint(),
                &call.id,
                &crate::model_content::ToolError::validation(&error)
                    .describe(call, context.parent_envelope),
                ServerResultSource::PermissionProposalRejected,
            )?;
            finish_tool(session, &call.id, false, sink);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    propose(
        deps,
        session,
        &planning,
        call,
        context,
        mint,
        tool_observation,
        sink,
    )
    .await
}

/// Resolve a host's `GrantRequired` answer: published tasks never receive it,
/// and an exact-input mismatch is explained once per turn before a new
/// proposal for the changed arguments is accepted.
#[allow(clippy::too_many_arguments)]
pub(super) async fn grant_required<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    input_mismatch: bool,
    hinted: &mut HashSet<String>,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if !session.trigger_origin.uses_call_time_approval() {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "a published task received a call-time approval outcome".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    if input_mismatch && hinted.insert(call.name.clone()) {
        tool_observation.permission(crate::model_observability::PermissionOutcome::PolicyRejected);
        append_internal_tool_result(
            session,
            context.parent_envelope,
            mint(),
            &call.id,
            &crate::model_content::ModelContent::constant(
                crate::capability_grant::APPROVED_INPUT_MISMATCH_MESSAGE,
            ),
            ServerResultSource::ApprovedInputMismatch,
        )?;
        finish_tool(session, &call.id, false, sink);
        return Ok(None);
    }
    propose_call(deps, session, call, context, mint, tool_observation, sink).await
}

/// Name the device of a direct device call's proposal: a single admitted
/// target is inferred, and a native UI call takes the device that observed its
/// application.
pub(super) fn bind_request_device(
    planning: crate::chat::ToolCall,
    call: &crate::chat::ToolCall,
    session: &crate::session::PersistedAgentSession,
) -> Result<crate::chat::ToolCall, AgentError> {
    let mut arguments: serde_json::Value = serde_json::from_str(&planning.arguments_json)
        .map_err(|_| crate::subagent::invalid("invalid proposal"))?;
    if !session.uses_request_device_tools() || arguments.get("target_ref").is_some() {
        return Ok(planning);
    }
    if let Some(inferred) = crate::request_devices::infer_single_target(&planning, session)? {
        return Ok(inferred);
    }
    if !crate::request_devices::implies_target(&call.name, session) {
        return Ok(planning);
    }
    let input: serde_json::Value = serde_json::from_str(&call.arguments_json)
        .map_err(|_| crate::subagent::invalid("invalid tool arguments"))?;
    let mut observed = session
        .agent_request_targets()
        .unwrap_or_default()
        .iter()
        .filter(|target| {
            crate::request_devices::confirm_implied_target(session, &target.device_id, &input)
                .is_ok()
        });
    if let (Some(target), None) = (observed.next(), observed.next()) {
        arguments["target_ref"] = serde_json::json!(target.reference);
        return Ok(crate::chat::ToolCall {
            arguments_json: arguments.to_string(),
            ..planning
        });
    }
    Ok(planning)
}
