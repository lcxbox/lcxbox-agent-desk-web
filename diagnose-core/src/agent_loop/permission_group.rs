//! Plan the approval group of one model step.
//!
//! When a step pauses for an approval at one call, every later call whose
//! permission need and request can be fixed without running the earlier calls
//! gets its own request at the same time, so the owner sees the whole step's
//! cards at once. Planning only reads: it starts no tool, records no
//! observation outcome, dispatches nothing and saves nothing. Any failure only
//! defers the call to the continuation, which processes it normally.
use super::*;

/// The step-wide inputs planning needs besides the proposal context.
pub(super) struct Planning<'a> {
    /// Every call of the step, in call order.
    pub calls: &'a [crate::chat::ToolCall],
    pub exposed: &'a [&'a RegisteredTool],
    pub owned_exposed: &'a [RegisteredTool],
    pub model_tool_definitions: &'a std::collections::BTreeMap<String, crate::chat::ToolSpec>,
    pub disclosure_enabled: bool,
    pub tool_batch: &'a crate::model_observability::tool::ToolBatch,
}

/// One later call of the step and the request created for it up front.
pub(super) struct Member {
    pub call: crate::chat::ToolCall,
    pub request: crate::dynamic_run::PermissionRequest,
}

/// Whether a pause at a newly created request may form a group. Scheduled
/// approvals come from the host at dispatch and track one approval each.
pub(super) fn eligible(session: &crate::session::PersistedAgentSession) -> bool {
    session.trigger_origin.uses_call_time_approval()
        && session.trigger_origin != crate::session::TriggerOrigin::ScheduledContinuation
}

/// The group id of a step pausing with its first request at `event_seq`.
pub(super) fn group_id(
    session: &crate::session::PersistedAgentSession,
    event_seq: u64,
    issuing_message_id: &str,
) -> String {
    stable_lineage_id(
        "permission-group",
        &(
            &session.conversation_id,
            session.input_revision,
            event_seq,
            issuing_message_id,
        ),
    )
}

/// Plan the later calls of the step that pauses at the request `pausing`.
/// `explicit` is true when `pausing` is an explicit request call. Returns the
/// members in call order; each has its own request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn plan(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    pausing: &crate::dynamic_run::PermissionRequest,
    explicit: bool,
    context: permission_proposal::ProposalContext<'_>,
    planning: &Planning<'_>,
    created_at: &str,
) -> Vec<Member> {
    // One slot stays free for a card the continuation may still need.
    let capacity = crate::dynamic_run::MAX_PERMISSION_REQUESTS
        .saturating_sub(session.non_evictable_permission_requests())
        .saturating_sub(2);
    let device = crate::permission_grant::permission_device_id(session, pausing).to_owned();
    let mut members: Vec<Member> = Vec::new();
    for call in context.remaining {
        if members.len() >= capacity {
            break;
        }
        let event_seq = session
            .last_event_seq
            .saturating_add(2)
            .saturating_add(members.len() as u64);
        let Some(request) = plan_call(
            deps, session, call, context, planning, created_at, event_seq,
        )
        .await
        else {
            continue;
        };
        // The decision and grant context is built per target device.
        if crate::permission_grant::permission_device_id(session, &request) != device {
            continue;
        }
        let in_group = std::iter::once(pausing).chain(members.iter().map(|member| &member.request));
        let overlaps = in_group.clone().any(|existing| {
            crate::permission_tools::equivalent_permission_request(existing, &request)
        }) || (explicit
            && in_group
                .flat_map(|existing| existing.items.iter())
                .any(|item| {
                    request.items.iter().any(|candidate| {
                        crate::permission_tools::same_permission_action(item, candidate)
                    })
                }));
        if overlaps {
            continue;
        }
        members.push(Member {
            call: call.clone(),
            request,
        });
    }
    members
}

/// Build the request of one later call, or `None` to defer the call.
#[allow(clippy::too_many_arguments)]
async fn plan_call(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    planning: &Planning<'_>,
    created_at: &str,
    event_seq: u64,
) -> Option<crate::dynamic_run::PermissionRequest> {
    // A call already requested by an unresolved group keeps that request.
    if session
        .released_permission_call
        .as_ref()
        .is_some_and(|released| released.pre_requested_for(&call.id).is_some())
    {
        return None;
    }
    if !planning
        .exposed
        .iter()
        .any(|advertised| advertised.name() == call.name)
    {
        return None;
    }
    let tool = lookup_for_session(planning.owned_exposed, &call.name, session)?;
    if !matches!(
        tool.effect,
        ToolEffect::ReadOnly | ToolEffect::ExternalRead | ToolEffect::Mutating
    ) || !step_calls::continuable(tool.effect)
    {
        return None;
    }
    // Ids that resolve against observations are bound when the call runs.
    if crate::ui_model_ids::needs_resolution(&call.name)
        || crate::visual_evidence::blocks_targeting(session, &call.name)
    {
        return None;
    }
    if session.surface == AgentSessionSurface::AiAgent {
        let spec = planning.model_tool_definitions.get(&call.name)?;
        let mut input = serde_json::from_str::<serde_json::Value>(&call.arguments_json).ok()?;
        if crate::request_devices::implies_target(&call.name, session)
            && let Some(input) = input.as_object_mut()
        {
            input.remove("target_ref");
        }
        crate::model_input::validate_format_reported(&call.name, &spec.parameters_schema, &input)
            .ok()?;
    }
    if !needs_request(deps, session, call, tool, context, planning).await? {
        return None;
    }
    let registry = deps.provider_registry?;
    if crate::permission_tools::owner_denied_tool(
        &session.permission_requests,
        &session.permission_decisions,
        &call.name,
        session.input_revision,
    ) {
        return None;
    }
    let application_scope_uses = deps.session_seam.application_scope_default_uses().await;
    let synthesized = crate::permission_tools::synthesize_request_call(
        call,
        registry,
        &session.conversation,
        application_scope_uses,
    )
    .ok()?;
    let synthesized = permission_proposal::bind_request_device(synthesized, call, session).ok()?;
    let request_id = stable_lineage_id(
        "permission-request",
        &(
            &session.conversation_id,
            session.input_revision,
            event_seq,
            &call.id,
        ),
    );
    let (request, _) = permission_proposal::build_request(
        deps,
        session,
        &synthesized,
        &request_id,
        created_at,
        context,
        true,
    )
    .await;
    let request = request.ok()?;
    // An application scope binds to what earlier calls observed; it is
    // requested when the call runs, after them.
    if request.items.iter().any(|item| {
        crate::application_ui::from_canonical(&item.tool_name, item.canonical_input_json.as_deref())
            .is_some()
    }) {
        return None;
    }
    if crate::permission_tools::unchanged_ai_denied_item(
        &session.permission_requests,
        &session.permission_decisions,
        &request,
    )
    .is_some()
        || session.permission_requests.iter().any(|existing| {
            crate::permission_tools::equivalent_permission_request(existing, &request)
        })
    {
        return None;
    }
    Some(request)
}

/// Whether the call would ask for approval when it runs, judged the same way
/// the step loop judges it for each kind of call. `None` defers the call.
async fn needs_request(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    tool: &RegisteredTool,
    context: permission_proposal::ProposalContext<'_>,
    planning: &Planning<'_>,
) -> Option<bool> {
    if crate::request_devices::is_selected_action_tool(tool, session) {
        return None;
    }
    let providers = deps.provider_registry?;
    let capability = providers.capability_for_tool(&call.name)?;
    let floor_r0 = crate::capability_risk::classify_provider_descriptor_floor(
        capability.wire.effect,
        &capability.wire.data_policy,
    ) == desk_agent_protocol::capability_grant::CapabilityRiskTier::R0;
    if crate::request_devices::is_selected_read_tool(tool, session) {
        let (reference, _) = crate::request_devices::selected_read(call).ok()?;
        let device = context
            .request_devices
            .iter()
            .find(|device| device.target_ref == reference)?;
        return Some(!floor_r0 && !device.approved_read_tools.contains(&call.name));
    }
    if call.name == crate::command_confirmation::COMMAND_TOOL
        && crate::request_devices::selected_command_turn(session)
    {
        let (reference, canonical) = crate::request_devices::command_input(call, true).ok()?;
        let policy = deps
            .tools
            .selected_command_policy(session, &reference?)
            .await
            .ok()?;
        return Some(
            crate::command_confirmation::CommandConfirmation::approved_request_for_call(
                session,
                &policy.target_device_id,
                &canonical,
            )
            .is_err(),
        );
    }
    if !planning.disclosure_enabled {
        return None;
    }
    let snapshot = deps.tools.current_grant_disclosure().await.ok()??;
    let now = current_unix_ms(deps.clock).ok()?;
    let grants = snapshot.subject_grants(session, providers);
    let active = crate::grant_disclosure::active_tool_names(
        &grants,
        session,
        now,
        snapshot.readiness_revision,
    );
    let ready = capability.wire.execution_locality
        == desk_agent_protocol::capability_provider::ExecutionLocality::Central
        || tool.has_device_capability(&snapshot.ready_capabilities);
    let policy_read = tool.effect == ToolEffect::ReadOnly
        && (floor_r0 || tool.has_device_capability(&snapshot.policy_read_capabilities));
    let ungranted = ready
        && session.trigger_origin.uses_call_time_approval()
        && !active.contains(&call.name)
        && !policy_read;
    // A call with some grant for its tool is judged by the host against its
    // exact input when it runs; its card, if any, comes then.
    Some(ungranted && (tool.effect == ToolEffect::ReadOnly || session.allows_new_mutation()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_origins_keep_sequential_cards() {
        let (mut session, _, _) = crate::grant_disclosure::tests::fixture();
        for (origin, grouped) in [
            (crate::session::TriggerOrigin::User, true),
            (crate::session::TriggerOrigin::ScheduledContinuation, false),
            (crate::session::TriggerOrigin::ScheduledTask, false),
        ] {
            session.trigger_origin = origin;
            assert_eq!(eligible(&session), grouped, "{origin:?}");
        }
    }
}
