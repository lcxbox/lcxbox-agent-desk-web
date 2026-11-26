//! An approved child command that the host rejects before dispatch closes its
//! call with a status message; the permission-resumed turn still reaches its
//! next model call.
use super::*;
use crate::agent_exec_store::SignalAgentExecStore;
use desk_agent_protocol::{AgentError, AgentErrorKind, edge_exec::EdgeExecDisposition};
use desk_diagnose_core::{
    agent_loop::{LoopDeps, resume_agent_turn_after_permission},
    prompt::ResponseFormatSpec,
    seam::{ExecOutcome, NullTurnSink, ToolRunOutput},
};

#[test]
fn rejected_before_dispatch_resume_fits_production_thread_stack() {
    std::thread::Builder::new()
        .name("child-rejected-command-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for kind in ["rejected", "dispatch_failed", "capacity"] {
                actix_web::rt::System::new().block_on(assert_resume_reaches_model(kind));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn disposition(kind: &str) -> EdgeExecDisposition {
    let error = EdgeExecDisposition::safe_error(
        AgentErrorKind::PermissionDenied,
        "pep_rejected:owner_interactive_mode_disabled",
        false,
    );
    match kind {
        "rejected" => EdgeExecDisposition::RejectedBeforeDispatch { error },
        "dispatch_failed" => EdgeExecDisposition::DispatchFailedBeforeWorker { error },
        _ => EdgeExecDisposition::HostAtCapacity { error },
    }
}

/// The host side of an approved command, as `SignalAgentTools` runs it: the
/// confirmed call is claimed for its device, the device rejects it before any
/// worker starts, the confirming turn consumes the event and the model sees
/// an execution error.
struct RejectingTools {
    db: DatabaseConnection,
    conversation_id: String,
    call: ToolCall,
    grant: CapabilityGrant,
    policy: desk_diagnose_core::model_egress::ModelEgressPolicy,
    disposition: EdgeExecDisposition,
}

#[async_trait::async_trait(?Send)]
impl ToolSeam for RejectingTools {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        Err(desk_diagnose_core::subagent::invalid(
            "no device reads in this rejected command check",
        ))
    }

    async fn request_devices(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Vec<desk_diagnose_core::request_devices::RequestDevice>, AgentError> {
        Ok(vec![desk_diagnose_core::request_devices::RequestDevice {
            native_readiness: None,
            read_target: None,
            ready_provider_ids: vec![],
            approved_read_tools: vec![],
            target_ref: session.device_id.clone(),
            name: None,
            platform: Some("linux"),
            online: true,
            ready_capabilities: vec![desk_agent_protocol::Capability::ShellExecConfirmed],
        }])
    }

    async fn selected_command_policy(
        &self,
        session: &PersistedAgentSession,
        _: &str,
    ) -> Result<CommandPolicyContext, AgentError> {
        Ok(child_command_policy(session))
    }

    async fn confirm_and_exec(
        &self,
        executed: &ToolCall,
        _: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        let (db, call, grant) = (&self.db, &self.call, &self.grant);
        let executed = ToolCall {
            id: executed.id.clone(),
            ..call.clone()
        };
        let child = super::super::paused_permission::session(db, &self.conversation_id).await;
        let now = jiff::Timestamp::now().as_millisecond() as u64;
        let grants = SignalCapabilityGrantStore::new(db.clone());
        grants.prepare(prepared(&child, call, grant)).await.unwrap();
        let DispatchIntentResult::Recorded { dispatch_id, .. } = grants
            .record_dispatch_intent(prepared(&child, call, grant))
            .await
            .unwrap()
        else {
            panic!("one original dispatch required")
        };
        let origin = ActionResultOrigin::capture_confirmed_command(
            &desk_diagnose_core::ai_agent::ai_agent_provider_registry(),
            &child,
            &child.device_id,
            &with_target_ref(&executed, &child.device_id),
            now,
        )
        .unwrap();
        let export =
            crate::capability_grant_store::computer_export::ComputerExportContext::capture_command(
                &self.policy,
                &child,
                origin.command_completion.as_ref().unwrap(),
            )
            .unwrap();
        grants
            .claim_command_dispatch(
                &dispatch_id,
                now,
                "host",
                &child.conversation_id,
                &executed.id,
                60000,
                &[],
                &origin,
                Some(&export),
            )
            .await
            .unwrap();
        let exec = SignalAgentExecStore::new(db.clone());
        let task = exec
            .finalize("host", &dispatch_id, &self.disposition)
            .await
            .unwrap()
            .unwrap();
        exec.consume_event(&task.event_id).await.unwrap();
        let (EdgeExecDisposition::RejectedBeforeDispatch { error }
        | EdgeExecDisposition::DispatchFailedBeforeWorker { error }
        | EdgeExecDisposition::HostAtCapacity { error }) = self.disposition.clone()
        else {
            unreachable!()
        };
        Err(error)
    }
}

/// The production model seam up to the provider transport. Each physical call
/// is recorded instead of sent: the first proposes the approved command again,
/// as a resumed child does; the call after its rejection is the boundary this
/// regression checks.
struct CapturingModel {
    inner: crate::agent_model::MeteredModel,
    proposal: ToolCall,
    source: desk_diagnose_core::replay::SourceContextKey,
    requests: std::cell::RefCell<Vec<desk_diagnose_core::seam::ModelRequest>>,
}

#[async_trait::async_trait(?Send)]
impl desk_diagnose_core::seam::ModelSeam for CapturingModel {
    fn prepare_request(
        &self,
        request: &desk_diagnose_core::seam::ModelRequest,
    ) -> Result<Option<desk_diagnose_core::model_preparation::ModelRequestPreparation>, AgentError>
    {
        self.inner.prepare_request(request)
    }
    fn context_planning_options(
        &self,
    ) -> desk_diagnose_core::model_context::ContextPlanningOptions {
        self.inner.context_planning_options()
    }
    fn model_output_token_limit(
        &self,
        request: &desk_diagnose_core::seam::ModelRequest,
    ) -> Result<i64, AgentError> {
        self.inner.model_output_token_limit(request)
    }
    fn model_input_token_upper_bound(
        &self,
        request: &desk_diagnose_core::seam::ModelRequest,
    ) -> Result<Option<u64>, AgentError> {
        self.inner.model_input_token_upper_bound(request)
    }
    fn model_egress_policy(
        &self,
    ) -> Result<Option<desk_diagnose_core::model_egress::ModelEgressPolicy>, AgentError> {
        self.inner.model_egress_policy()
    }
    async fn context_policy(
        &self,
        requirements: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: desk_diagnose_core::seam::ModelRequest,
        _: &mut dyn desk_diagnose_core::seam::TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        let first = self.requests.borrow().is_empty();
        self.requests.borrow_mut().push(request.clone());
        if !first {
            return Err(AgentError {
                kind: AgentErrorKind::ModelUnavailable,
                message: "fixture stops at the provider transport".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            });
        }
        let policy = self.inner.model_egress_policy()?.unwrap();
        let authorized = policy.authorize_request(request).unwrap();
        let mut turn = desk_diagnose_core::chat::ModelTurn {
            text: String::new(),
            tool_calls: vec![self.proposal.clone()],
            stop_reason: desk_diagnose_core::chat::StopReason::ToolUse,
            usage: Default::default(),
            provider_meta: Default::default(),
        };
        turn.provider_meta.stop_reason = desk_diagnose_core::chat::StopReason::ToolUse;
        turn.provider_meta.replay =
            Some(desk_diagnose_core::replay::ReplayDisposition::NotRequired {
                source_context_key: self.source.clone(),
            });
        turn.provider_meta.data_envelope = Some(
            policy
                .derive_model_output_envelope(&turn, &authorized.input_envelopes)
                .unwrap(),
        );
        Ok(turn)
    }
}

async fn assert_resume_reaches_model(kind: &str) {
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("child-rejected".into()),
        base_url: Some("http://127.0.0.1:9".into()),
        api_key: Some("test-only".into()),
        max_context_bytes: Some(1_048_576),
        ..Default::default()
    };
    let db = database().await;
    let schema = Schema::new(db.get_database_backend());
    db.execute(
        schema
            .entity_table(crate::entity::model_probe_observation::Entity)
            .if_not_exists(),
    )
    .await
    .unwrap();
    crate::model_provider::save(&db, config.clone())
        .await
        .unwrap();
    let config = crate::model_provider::load(&db).await.unwrap();
    let destination = config.destination_identity().unwrap();
    let now = jiff::Timestamp::now().as_millisecond() as u64;
    let policy = desk_diagnose_core::model_egress::ModelEgressPolicy {
        destination: destination.clone(),
        selected_source_tools: [desk_diagnose_core::command_confirmation::COMMAND_TOOL.to_string()]
            .into_iter()
            .collect(),
        export_authorization_id: "rejected-child-export".into(),
        now_unix_ms: now,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: true,
    };
    // A production proposal is the labelled output of the model that issued it.
    let source = desk_diagnose_core::seam::ModelSeam::context_policy(
        &crate::model_dial::SignalModelSeam::from_config(&config)
            .unwrap()
            .with_context_db(db.clone()),
        desk_diagnose_core::model_capability::ModelRequirements::TEXT_ONLY,
    )
    .await
    .unwrap()
    .source_context_key;
    let (db, _, child, call, grant) = Box::pin(approved_child_decided_on(
        db,
        None,
        destination.clone(),
        desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
        false,
        Some((&policy, source.clone())),
    ))
    .await;
    crate::ai_agent_gate::enable_test_host();

    let child = super::super::paused_permission::session(&db, &child.conversation_id).await;
    let tasks = SubAgentStore::new(db.clone());
    let creation = tasks.child_creation_context(&child).await.unwrap().unwrap();
    let model = CapturingModel {
        inner: crate::agent_model::MeteredModel {
            fresh_task: None,
            inner: crate::model_dial::SignalModelSeam::from_config(&config)
                .unwrap()
                .with_context_db(db.clone()),
            db: db.clone(),
            destination: destination.clone(),
            selected_source_tools: policy.selected_source_tools.clone(),
            export_authorization_id: policy.export_authorization_id.clone(),
            permission_resume: true,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        },
        proposal: ToolCall {
            id: "child-command-again".into(),
            name: call.name.clone(),
            arguments_json: serde_json::json!({
                "shell": "bash", "command": "sleep 60", "timeout_ms": 60000,
                "target_ref": child.device_id,
            })
            .to_string(),
        },
        source,
        requests: Default::default(),
    };
    let bridge = desk_diagnose_core::permission_resume::authorized_child_permission_resume_message(
        "rejected-child-decision".into(),
        &policy,
        &creation,
        child.agent_role.binding().unwrap(),
    )
    .unwrap();
    let capability_grants = SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&child.conversation_id, &child.actor_id, &child.device_id)
        .await
        .unwrap();
    let sessions = SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(child.client_conversation_id.clone(), child.surface)
        .with_expected_input_revision(child.input_revision)
        .with_permission_resume(
            "paused-child-permission".into(),
            child.version,
            capability_grants,
        );
    let heartbeat = crate::agent_runtime::SignalStoreHeartbeat {
        store: sessions.clone(),
    };
    let tools = RejectingTools {
        db: db.clone(),
        conversation_id: child.conversation_id.clone(),
        call,
        grant,
        policy: policy.clone(),
        disposition: disposition(kind),
    };
    let registry = desk_diagnose_core::ai_agent::ai_agent_provider_registry();
    let capability = registry
        .capability_for_tool(desk_diagnose_core::command_confirmation::COMMAND_TOOL)
        .unwrap();
    let callable = [capability.registered_tool()];
    let inventory = [
        desk_diagnose_core::capability_availability::CapabilityAvailability {
            provider_id: registry
                .provider_for_capability(&capability.wire.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: desk_diagnose_core::command_confirmation::COMMAND_TOOL.into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        },
    ];
    let clock = || desk_utils::datetime::rfc3339(jiff::Timestamp::now());
    let deps = LoopDeps {
        session_seam: &sessions,
        model: &model,
        tools: &tools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &callable,
        provider_registry: Some(&registry),
        capability_inventory: Some(&inventory),
        capability_permission_candidates: &[],
        capability_catalog_metrics: None,
        response_format: ResponseFormatSpec::None,
        system_prompt: desk_diagnose_core::ai_agent::build_ai_agent_system_message(None),
        response_locale: None,
        interactive_user_home: None,
        interactive_user_home_incarnation: None,
        max_steps_per_turn: 4,
        max_same_tool_per_turn: 2,
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let claim = desk_diagnose_core::seam::ClaimTurnParams {
        conversation_id: child.conversation_id.clone(),
        actor_id: child.actor_id.clone(),
        device_id: child.device_id.clone(),
        policy_revision: child.policy_revision,
        // The device policy confirms each command, as the approval required.
        current_pdp_scope: desk_agent_protocol::AgentScope {
            mode: ExecutionMode::ConfirmEachAction,
            granted: vec![desk_agent_protocol::Capability::ShellExecConfirmed],
            ..child.scope_snapshot.clone()
        },
        turn_id: crate::agent_session_store::permission_resume::turn_id(
            &child.conversation_id,
            "paused-child-permission",
        ),
        request_id: None,
        connection_id: None,
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::PermissionDecision,
        now: clock(),
    };
    let outcome = Box::pin(resume_agent_turn_after_permission(
        &deps,
        claim,
        bridge,
        &mut NullTurnSink,
    ))
    .await;
    let requests = model.requests.take();
    let [_, request] = requests.as_slice() else {
        panic!("{kind}: resumed child did not reach its next model call: {outcome:?}")
    };
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.text.contains("runtime_delegation_state")),
        "{kind}: the request carries the delegated runtime projection"
    );
    assert!(
        request
            .messages
            .iter()
            .any(
                |message| message.tool_call_id.as_deref() == Some("child-command-again")
                    && message
                        .text
                        .contains("pep_rejected:owner_interactive_mode_disabled")
            ),
        "{kind}: the model sees the rejection status for the original call: {:?}",
        request
            .messages
            .iter()
            .filter(|m| m.tool_call_id.is_some())
            .map(|m| (&m.tool_call_id, &m.text))
            .collect::<Vec<_>>()
    );
    let stored = super::super::paused_permission::session(&db, &child.conversation_id).await;
    assert!(stored.unclosed_tool_call_ids().is_empty(), "{kind}");
    let task = agent_exec_task::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.delivery_state, "consumed", "{kind}");
}
