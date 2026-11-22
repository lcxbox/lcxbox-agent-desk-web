use super::*;

pub(super) struct GrantTools {
    pub(super) snapshot: RefCell<crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub(super) reads: std::cell::Cell<usize>,
}

#[async_trait(?Send)]
impl ToolSeam for GrantTools {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        Ok(Some(self.snapshot.borrow().clone()))
    }
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        _: &crate::seam::ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        assert!(matches!(
            call.name.as_str(),
            "launch_application" | "exec_command" | "create_text_file"
        ));
        self.reads.set(self.reads.get() + 1);
        self.snapshot
            .borrow_mut()
            .grants
            .iter_mut()
            .find(|grant| grant.tool_name == call.name)
            .unwrap()
            .remaining_uses = 0;
        Ok(ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                format: crate::seam::ToolOutputFormat::Text,
                content: "launch accepted by test seam".into(),
                image_data_url: None,
                document_preview: None,
            },
            event_id: None,
        })
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.set(self.reads.get() + 1);
        self.snapshot.borrow_mut().grants[0].remaining_uses = 0;
        Ok(ToolRunOutput {
            format: crate::seam::ToolOutputFormat::Text,
            content: "immutable observation".into(),
            image_data_url: None,
            document_preview: None,
        })
    }
}

#[tokio::test]
async fn approval_exposes_without_load_and_last_use_refreshes_next_request() {
    let (mut initial, providers, snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let scope = initial.scope_snapshot.clone();
    let target = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap()
        .registered_tool();
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot.grants[0].provider_id.clone(),
        capability_id: snapshot.grants[0].capability_id.clone(),
        tool_name: target.name().into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                ModelTurn {
                    tool_calls: vec![ToolCall {
                        id: "read".into(),
                        name: target.name().into(),
                        arguments_json: "{}".into(),
                    }],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(tools.reads.get(), 1);
    let requests = requests.borrow();
    assert_eq!(requests[0].messages[0].text, requests[1].messages[0].text);
    for request in requests.iter() {
        let runtime = request
            .messages
            .iter()
            .position(crate::runtime_context::is_runtime)
            .unwrap();
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| crate::runtime_context::is_runtime(message))
                .count(),
            1
        );
        assert!(
            request.messages[..runtime]
                .iter()
                .any(|message| message.role != ChatRole::System)
        );
        assert!(request.messages.iter().all(|message| {
            !message.text.contains("capability_authorization")
                && !message.text.contains("CURRENT AUTHORIZED GRANTS")
                && !message.text.contains("approved_exact_input")
        }));
    }
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.text.contains("immutable observation"))
    );
}

/// Approving many tools must not make every later turn fail: callable tools
/// beyond the definition budget stay out of the request and remain loadable by
/// name from the capability index.
#[tokio::test]
async fn callable_tools_beyond_the_definition_budget_do_not_fail_the_turn() {
    let (mut initial, providers, snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let scope = initial.scope_snapshot.clone();
    let target = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap()
        .registered_tool();
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot.grants[0].provider_id.clone(),
        capability_id: snapshot.grants[0].capability_id.clone(),
        tool_name: target.name().into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("done")].into()),
        requests: requests.clone(),
    };
    // The granted, callable tool's definition alone exceeds the byte budget.
    let mut target = target;
    target
        .spec
        .description
        .push_str(&" ".repeat(crate::capability_disclosure::MAX_ADVERTISED_PROVIDER_TOOL_BYTES));
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    let requests = requests.borrow();
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "inspect_desktop_session")
    );
    assert!(requests[0].messages.iter().any(|message| {
        message.text.contains("callable_when_loaded_now")
            && message.text.contains("inspect_desktop_session")
    }));
}

struct ExactGrantModel(ScriptModel);
#[async_trait(?Send)]
impl ModelSeam for ExactGrantModel {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request
            .caller_output_hard_cap
            .unwrap_or(128_000)
            .min(128_000))
    }

    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(crate::model_egress::ModelEgressPolicy {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "test".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
            selected_source_tools: Default::default(),
            export_authorization_id: "test".into(),
            now_unix_ms: 500,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: true,
        }))
    }
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.0.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.0.call(request, sink).await
    }
}

/// Real registry, scope projection, exact grant and permission-resume loop;
/// native dispatch is replaced by the recording seam, so no app is launched.
#[tokio::test]
async fn permission_resume_keeps_scoped_prerequisites_and_validates_exact_grants() {
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
        PermissionRequestState,
    };
    use desk_agent_protocol::capability_grant::{CapabilityGrantUsePolicy, CapabilityRiskTier};
    use sha2::{Digest, Sha256};
    for blocked in [
        "none",
        "mixed",
        "expired",
        "revoked",
        "changed_input",
        "missing_grant",
        "offline",
        "scope",
        "mode",
    ] {
        let (mut initial, providers, mut snapshot) = crate::grant_disclosure::tests::fixture();
        let action_name = if blocked == "mixed" {
            "exec_command"
        } else {
            "launch_application"
        };
        let descriptor = providers.capability_for_tool(action_name).unwrap();
        let mut registry = vec![descriptor.registered_tool()];
        crate::tool_exposure::retain_candidates(&providers, &mut registry, &[]);
        let mut scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        };
        crate::tool_exposure::extend_candidate_scope(
            &providers,
            &registry,
            &[],
            &[],
            &mut scope.granted,
        );
        if blocked == "scope" {
            scope.granted.clear();
        }
        if blocked == "mode" {
            scope.mode = ExecutionMode::ReadOnly;
        }
        initial.scope_snapshot = scope.clone();
        initial.latest_input_seq = 1;
        initial.focus_epoch.input_revision = 1;
        initial.conversation.push(ChatMessage::text(
            "original",
            ChatRole::User,
            "open the approved app",
        ));
        let launch_exact = r#"{"args":[],"cwd":null,"run_as_admin":false,"target":{"kind":"executable","value":"C:\\app.exe"}}"#;
        let exact = if blocked == "mixed" {
            r#"{"command":"python hello.py","cwd":"C:/Users/Public","shell":"powershell","timeout_ms":60000}"#
        } else {
            launch_exact
        };
        let digest = format!("{:x}", Sha256::digest(exact.as_bytes()));
        let grant = &mut snapshot.grants[0];
        grant.provider_id = providers
            .provider_for_capability(&descriptor.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
        grant.capability_id = descriptor.wire.capability_id.clone();
        grant.tool_name = descriptor.wire.tool_name.clone();
        grant.effect = descriptor.wire.effect;
        grant.risk_tier = CapabilityRiskTier::R3;
        grant.use_policy = CapabilityGrantUsePolicy::OneShotExact;
        grant.canonical_input_digest_sha256 = Some(digest.clone());
        grant.resource_scope = vec![format!("application_input:sha256:{}", "a".repeat(64))];
        grant.operation_scope = vec!["launch_application".into()];
        grant.validate().unwrap();
        initial.permission_requests.push(PermissionRequest {
            input_source: None,
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "approved-launch".into(),
            input_revision: 1,
            state: PermissionRequestState::Approved,
            items: vec![GrantRequestItem {
                command_confirmation: None,
                selected_device_target: None,
                launch_confirmation: None,
                item_id: "launch".into(),
                provider_id: grant.provider_id.clone(),
                tool_name: grant.tool_name.clone(),
                expected_effect: grant.effect,
                resource_scope: grant.resource_scope.clone(),
                operation_scope: grant.operation_scope.clone(),
                export_destinations: vec![],
                canonical_input_json: Some(exact.into()),
                canonical_input_digest_sha256: Some(digest),
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "open app".into(),
            }],
            created_at: "1970-01-01T00:00:00.100Z".into(),
            group: None,
        });
        snapshot.ready_capabilities = if blocked == "offline" {
            vec![]
        } else {
            vec![descriptor.required_capability]
        };
        if blocked == "expired" {
            snapshot.grants[0].expires_at_unix_ms = 400;
        }
        if blocked == "missing_grant" {
            snapshot.grants.clear();
        }
        if blocked == "revoked" {
            snapshot.grants[0].revoked_at_unix_ms = Some(400);
            snapshot.grants[0].revoked_reason = Some("owner".into());
        }
        if blocked == "changed_input" {
            initial.permission_requests[0].items[0].canonical_input_json = Some("{}".into());
        }
        if blocked == "mixed" {
            crate::file_model_input::tests::approve_directory(&mut initial, "test-directory");
            let create = providers.capability_for_tool("create_text_file").unwrap();
            registry.push(create.registered_tool());
            scope.granted.push(create.required_capability);
            initial.scope_snapshot = scope.clone();
            snapshot.ready_capabilities.push(create.required_capability);
            let mut grant = snapshot.grants[0].clone();
            grant.grant_id = "create-grant".into();
            grant.provider_id = providers
                .provider_for_capability(&create.wire.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone();
            grant.capability_id = create.wire.capability_id.clone();
            grant.tool_name = create.wire.tool_name.clone();
            grant.tool_schema_version = create.wire.input_schema_version;
            grant.effect = create.wire.effect;
            grant.use_policy = CapabilityGrantUsePolicy::Reusable;
            grant.canonical_input_digest_sha256 = None;
            grant.resource_scope = vec!["directory:test".into()];
            grant.operation_scope = vec!["create_new_artifact".into()];
            grant.validate().unwrap();
            snapshot.grants.push(grant);
        }
        let mut inventory = vec![crate::capability_availability::CapabilityAvailability {
            provider_id: snapshot
                .grants
                .first()
                .map(|g| g.provider_id.clone())
                .unwrap_or_else(|| "application.launch".into()),
            capability_id: descriptor.wire.capability_id.clone(),
            tool_name: action_name.into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        if blocked == "mixed" {
            let create = providers.capability_for_tool("create_text_file").unwrap();
            let mut available = inventory[0].clone();
            available.provider_id = snapshot.grants[1].provider_id.clone();
            available.capability_id = create.wire.capability_id.clone();
            available.tool_name = "create_text_file".into();
            inventory.push(available);
        }
        let session = MemSession {
            inner: RefCell::new(Some(initial)),
            ..Default::default()
        };
        let requests = Rc::new(RefCell::new(vec![]));
        let turns = if blocked == "mixed" {
            vec![
                tool_use_args(
                    "create",
                    "create_text_file",
                    r#"{"file_name":"hello.py","content_utf8":"print('hello world')"}"#,
                ),
                tool_use_args("launch", action_name, exact),
                answer("done"),
            ]
        } else if blocked == "none" {
            vec![tool_use_args("launch", action_name, exact), answer("done")]
        } else {
            vec![answer("blocked")]
        };
        let model = ExactGrantModel(ScriptModel {
            turns: RefCell::new(turns.into()),
            requests: requests.clone(),
        });
        let tools = GrantTools {
            snapshot: RefCell::new(snapshot),
            reads: std::cell::Cell::new(0),
        };
        let clock = || "1970-01-01T00:00:00.500Z".to_string();
        let mut deps = deps(&session, &model, &tools, &registry, &clock);
        deps.provider_registry = Some(&providers);
        deps.capability_inventory = Some(&inventory);
        let mut claim = exec_claim();
        claim.current_pdp_scope = scope;
        claim.trigger_origin = crate::session::TriggerOrigin::PermissionDecision;
        resume_agent_turn_after_permission(
            &deps,
            claim,
            ChatMessage::text("decision", ChatRole::User, "approved"),
            &mut NullTurnSink,
        )
        .await
        .unwrap();
        assert_eq!(
            tools.reads.get(),
            if blocked == "mixed" {
                2
            } else {
                usize::from(blocked == "none")
            },
            "{blocked}"
        );
        let requests = requests.borrow();
        assert_eq!(
            requests[0].tools.iter().any(|t| t.name == action_name),
            matches!(blocked, "none" | "mixed"),
            "{blocked}"
        );
        if blocked == "mixed" {
            assert!(
                requests[0]
                    .tools
                    .iter()
                    .any(|t| t.name == "create_text_file")
            );
            assert!(
                requests[1]
                    .tools
                    .iter()
                    .any(|t| t.name == "create_text_file")
            );
            assert!(requests[1].tools.iter().any(|t| t.name == action_name));
            assert!(requests[2].tools.iter().any(|t| t.name == action_name));
        } else if blocked == "none" {
            assert!(requests[1].tools.iter().any(|t| t.name == action_name));
        }
    }
}

#[tokio::test]
async fn workbench_coordinator_uses_reads_and_rejects_forged_writes_and_permissions() {
    use crate::{native_ids, subagent::AgentRole};

    struct SelectedRead(RefCell<Vec<String>>);

    #[async_trait(?Send)]
    impl ToolSeam for SelectedRead {
        async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            panic!("unexpected fixed-device read: {}", call.name)
        }

        async fn request_devices(
            &self,
            session: &PersistedAgentSession,
        ) -> Result<Vec<crate::request_devices::RequestDevice>, AgentError> {
            assert!(session.admits_agent_device("device"));
            Ok(vec![crate::request_devices::RequestDevice {
                native_readiness: None,
                read_target: None,
                ready_provider_ids: vec![crate::ai_agent::SYSTEM_INFO_CAPABILITY_ID.into()],
                approved_read_tools: Vec::new(),
                target_ref: "device".into(),
                name: None,
                platform: Some("macOS"),
                online: true,
                ready_capabilities: vec![Capability::SystemInfo, Capability::ShellExecConfirmed],
            }])
        }

        async fn run_request_device_read(
            &self,
            session: &PersistedAgentSession,
            call: &ToolCall,
        ) -> Result<crate::seam::ReadOutcome, AgentError> {
            let (target, _) = crate::request_devices::selected_read(call)?;
            assert!(session.admits_agent_device(&target));
            self.0.borrow_mut().push(target);
            Ok(crate::seam::ReadOutcome::Completed {
                output: ToolRunOutput {
                    format: crate::seam::ToolOutputFormat::Text,
                    content: "macOS 15".into(),
                    image_data_url: None,
                    document_preview: None,
                },
                ok: true,
                data_envelope: None,
                event_id: None,
                background_task: None,
            })
        }
    }

    let providers = crate::ai_agent::ai_agent_provider_registry();
    let write = providers.capability_for_tool("browser_open_page").unwrap();
    let write_provider = providers
        .provider_for_capability(&write.wire.capability_id)
        .unwrap();
    let mut claim = claim();
    claim.device_id.clear();
    claim.current_pdp_scope.mode = ExecutionMode::ConfirmEachAction;
    claim
        .current_pdp_scope
        .granted
        .push(write.required_capability);
    let request_id = native_ids::stable(native_ids::Kind::AgentMainRequest, &"message");
    let mut initial = PersistedAgentSession::new_workbench_coordinator(
        &claim.actor_id,
        "workbench",
        Some(&request_id),
        claim.policy_revision,
        claim.current_pdp_scope.clone(),
        &claim.now,
    )
    .unwrap();
    claim.conversation_id = initial.conversation_id.clone();
    initial.input_revision = 1;
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    initial
        .bind_agent_request_targets(vec!["device".into()])
        .unwrap();
    let candidates = [write.registered_tool()];
    let inventory = [crate::capability_availability::CapabilityAvailability {
        provider_id: write_provider.wire.provider_id.clone(),
        capability_id: write.wire.capability_id.clone(),
        tool_name: write.tool_spec.name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let proposal = ToolCall {
        id: "proposal".into(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"items":[{
            "item_id": "open",
            "tool_name": "browser_open_page",
            "exact_input": {"target": {
                "origin": {"kind":"https","host_ascii":"app.slack.com","port":443},
                "url": "https://app.slack.com/"
            }},
            "suggested_ttl_seconds": 300,
            "suggested_max_uses": 1,
            "reason": "Open the page"
        }]})
        .to_string(),
    };

    // This is a valid native permission proposal and a requestable provider.
    let request = crate::permission_tools::build_permission_request(
        &proposal,
        &providers,
        "request".into(),
        1,
        claim.now.clone(),
    )
    .unwrap();
    validate_permission_request_availability(
        &AgentRole::Main,
        &[],
        &request,
        &providers,
        &inventory,
        &[],
        &candidates,
    )
    .unwrap();
    assert!(
        validate_permission_request_availability(
            &initial.agent_role,
            &[],
            &request,
            &providers,
            &inventory,
            &[],
            &candidates,
        )
        .is_err()
    );
    let mut invalid = initial.clone();
    invalid.permission_requests.push(request);
    assert!(
        PersistedAgentSession::decode_json(&serde_json::to_string(&invalid).unwrap())
            .unwrap_err()
            .to_string()
            .contains("invalid workbench coordination session")
    );

    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                ModelTurn {
                    tool_calls: vec![
                        ToolCall {
                            id: "write".into(),
                            name: "browser_open_page".into(),
                            arguments_json: "{}".into(),
                        },
                        proposal,
                        ToolCall {
                            id: "read".into(),
                            name: crate::request_devices::SYSTEM_INFO_TOOL.into(),
                            arguments_json: r#"{"target_ref":"device"}"#.into(),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("inspection complete"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = SelectedRead(RefCell::new(Vec::new()));
    let registry = candidates.to_vec();
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    deps.capability_permission_candidates = &candidates;
    run_agent_turn(
        &deps,
        claim,
        ChatMessage::text("u", ChatRole::User, "inspect the environment"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let saved = session.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert_eq!(
        tools.0.borrow().as_slice(),
        &["device"],
        "{:?}",
        saved.conversation
    );
    assert!(saved.permission_requests.is_empty());
    for id in ["write", "proposal", "read"] {
        assert!(
            saved
                .conversation
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        );
    }
    let requests = requests.borrow();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == crate::request_devices::SYSTEM_INFO_TOOL)
    );
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "browser_open_page")
    );
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|message| message.text.contains("browser_open_page"))
    );
}

/// Seam whose host finds no grant matching the exact read input.
struct MismatchTools {
    snapshot: crate::grant_disclosure::GrantDisclosureSnapshot,
    reads: std::cell::Cell<usize>,
}

#[async_trait(?Send)]
impl ToolSeam for MismatchTools {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        Ok(Some(self.snapshot.clone()))
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!("a read without a matching grant is never dispatched")
    }
    async fn run_read_versioned(
        &self,
        _: &ToolCall,
        _: &crate::seam::ExecContext,
        _: Option<&crate::action_version::ActionVersion>,
    ) -> crate::seam::ReadCompletion {
        self.reads.set(self.reads.get() + 1);
        crate::seam::ReadCompletion {
            outcome: Ok(ReadOutcome::GrantRequired {
                input_mismatch: true,
            }),
            version_advance: None,
        }
    }
}

fn call_time_case(
    exhausted: bool,
) -> (
    MemSession,
    crate::provider_registry::ProviderRegistry,
    crate::grant_disclosure::GrantDisclosureSnapshot,
    Vec<crate::capability_availability::CapabilityAvailability>,
    RegisteredTool,
    AgentScope,
) {
    let (mut initial, providers, mut snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let scope = initial.scope_snapshot.clone();
    if exhausted {
        snapshot.grants[0].remaining_uses = 0;
    }

    let target = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap()
        .registered_tool();
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot.grants[0].provider_id.clone(),
        capability_id: snapshot.grants[0].capability_id.clone(),
        tool_name: target.name().into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    (session, providers, snapshot, inventory, target, scope)
}

fn direct_read(id: &str) -> ModelTurn {
    ModelTurn {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "inspect_desktop_session".into(),
            arguments_json: "{}".into(),
        }],
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

/// Loading a definition grants nothing; it only makes the tool callable.
fn load_read() -> ModelTurn {
    ModelTurn {
        tool_calls: vec![ToolCall {
            id: "load".into(),
            name: crate::capability_disclosure::LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"tool_names":["inspect_desktop_session"]})
                .to_string(),
        }],
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

fn tool_result<'a>(session: &'a PersistedAgentSession, id: &str) -> &'a str {
    &session
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some(id))
        .unwrap()
        .text
}

/// Seed a request the direct read `{}` would propose, already settled.
fn seed_prior_read(
    session: &MemSession,
    providers: &crate::provider_registry::ProviderRegistry,
    state: crate::dynamic_run::PermissionRequestState,
) {
    let mut stored = session.inner.borrow_mut();
    let stored = stored.as_mut().unwrap();
    let call = ToolCall {
        id: "prior".into(),
        name: "inspect_desktop_session".into(),
        arguments_json: "{}".into(),
    };
    let proposal = crate::permission_tools::synthesize_request_call(
        &call,
        providers,
        &[],
        crate::permission_tools::DEFAULT_APPLICATION_SCOPE_USES,
    )
    .unwrap();
    let mut prior = crate::permission_tools::build_permission_request(
        &proposal,
        providers,
        "permission-prior".into(),
        stored.input_revision,
        "1970-01-01T00:00:00Z".into(),
    )
    .unwrap();
    prior.state = state;
    stored.add_permission_request(prior).unwrap();
}

/// Run one turn in which the model loads and directly calls the read.
/// Returns the outcome and how many reads the host ran.
async fn run_direct_read(
    session: &MemSession,
    providers: &crate::provider_registry::ProviderRegistry,
    snapshot: crate::grant_disclosure::GrantDisclosureSnapshot,
    inventory: &[crate::capability_availability::CapabilityAvailability],
    target: RegisteredTool,
    scope: AgentScope,
    answer_text: &str,
) -> (LoopOutcome, usize) {
    let model = ScriptModel {
        turns: RefCell::new([load_read(), direct_read("read"), answer(answer_text)].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(providers);
    deps.capability_inventory = Some(inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    (outcome, tools.reads.get())
}

#[tokio::test]
async fn a_direct_call_reuses_a_settled_equivalent_request_without_a_new_card() {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(true);
    seed_prior_read(
        &session,
        &providers,
        crate::dynamic_run::PermissionRequestState::Denied,
    );
    let seq = session.inner.borrow().as_ref().unwrap().last_event_seq;
    let (outcome, reads) = run_direct_read(
        &session,
        &providers,
        snapshot,
        &inventory,
        target,
        scope,
        "I will adapt to the recorded denial.",
    )
    .await;
    assert_eq!(
        outcome,
        LoopOutcome::Answered("I will adapt to the recorded denial.".into())
    );
    assert_eq!(reads, 0);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(stored.permission_requests.len(), 1);
    assert_eq!(stored.last_event_seq, seq);
    let result = tool_result(stored, "read");
    assert!(
        result.contains("existing_permission_request") && result.contains("denied"),
        "{result}"
    );
}

#[tokio::test]
async fn an_exhausted_approval_is_renewed_as_a_new_owner_decision_without_dispatch() {
    let (mut session, providers, snapshot, inventory, target, scope) = call_time_case(true);
    session.allow_permission_renewal = true;
    seed_prior_read(
        &session,
        &providers,
        crate::dynamic_run::PermissionRequestState::Approved,
    );
    let (outcome, reads) = run_direct_read(
        &session, &providers, snapshot, &inventory, target, scope, "unused",
    )
    .await;
    assert!(matches!(outcome, LoopOutcome::PermissionRequested { .. }));
    assert_eq!(reads, 0);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    let [prior, renewed] = stored.permission_requests.as_slice() else {
        panic!("{:?}", stored.permission_requests)
    };
    assert_eq!(
        prior.state,
        crate::dynamic_run::PermissionRequestState::Approved
    );
    assert_eq!(
        renewed.state,
        crate::dynamic_run::PermissionRequestState::Pending
    );
    assert_ne!(prior.request_id, renewed.request_id);
}

#[tokio::test]
async fn an_unknown_outcome_still_allows_a_new_call_time_proposal() {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(true);
    session.inner.borrow_mut().as_mut().unwrap().execution_state = ExecutionState::OutcomeUnknown {
        action: crate::session::ActionIdentity::agent_exec(57, "request", "execution"),
        placeholder_message_id: "unknown".into(),
        since: "1970-01-01T00:00:00Z".into(),
    };
    let (outcome, reads) = run_direct_read(
        &session, &providers, snapshot, &inventory, target, scope, "unused",
    )
    .await;
    assert!(matches!(outcome, LoopOutcome::PermissionRequested { .. }));
    assert_eq!(reads, 0);
    assert_eq!(
        session
            .inner
            .borrow()
            .as_ref()
            .unwrap()
            .permission_requests
            .len(),
        1
    );
}

#[tokio::test]
async fn an_approved_direct_call_runs_once_without_the_model_repeating_it() {
    decided_direct_call(true).await;
}

#[tokio::test]
async fn a_refused_direct_call_is_closed_without_the_model_repeating_it() {
    decided_direct_call(false).await;
}

async fn decided_direct_call(approved: bool) {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(true);
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([load_read(), direct_read("read"), answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let LoopOutcome::PermissionRequested { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(tools.reads.get(), 0, "nothing ran before approval");
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let [request] = stored.permission_requests.as_slice() else {
            panic!("one proposal")
        };
        assert_eq!(request.request_id, request_id);
        assert_eq!(request.items[0].item_id, "read");
        assert_eq!(request.items[0].tool_name, "inspect_desktop_session");
        // The paused call keeps no result and is not reported as running.
        assert!(
            !stored
                .conversation
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some("read"))
        );
        assert!(stored.unclosed_tool_call_ids().is_empty());
        assert_eq!(
            crate::permission_tools::proposing_call(stored, &request_id)
                .map(|(message, call)| stored.conversation[message].tool_calls[call].id.as_str()),
            Some("read")
        );
        // The owner approves; the host releases the call in its resume claim.
        stored
            .permission_decisions
            .push(crate::dynamic_run::PermissionDecisionProjection {
                request_id: request_id.clone(),
                input_revision: stored.input_revision,
                actor: crate::dynamic_run::PermissionDecisionActor::Owner,
                items: vec![crate::dynamic_run::PermissionItemDisposition {
                    item_id: "read".into(),
                    approved,
                    actor: crate::dynamic_run::PermissionDecisionActor::Owner,
                }],
            });
        assert!(stored.release_awaiting_permission(&request_id).unwrap());
    }
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    resume_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: claim().current_pdp_scope,
            trigger_origin: crate::session::TriggerOrigin::PermissionDecision,
            ..claim()
        },
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(
        tools.reads.get(),
        usize::from(approved),
        "runs only when approved"
    );
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    let expected = if approved {
        "immutable observation"
    } else {
        "not executed: the owner refused this call"
    };
    assert!(tool_result(stored, "read").contains(expected));
    assert_eq!(
        stored
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some("read"))
            .count(),
        1
    );
    assert!(stored.released_permission_call.is_none());
    // One model request after approval, already holding the call's result.
    let requests = requests.borrow();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("read")
                && message.text.contains(expected))
    );
}

/// Runs the approved read, then fails while recording its result, as a host
/// recording step can after the device already answered.
struct RecordingFails(GrantTools);

#[async_trait(?Send)]
impl ToolSeam for RecordingFails {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        self.0.current_grant_disclosure().await
    }
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.0.run_read(call).await
    }
    fn read_data_envelope(
        &self,
        _: &ToolCall,
        _: &ToolRunOutput,
    ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
        Err(AgentError {
            kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
            message: "the read result could not be recorded".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }
}

#[tokio::test]
async fn a_released_read_that_fails_after_running_answers_its_call_and_frees_the_input() {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(true);
    let model = ScriptModel {
        turns: RefCell::new([load_read(), direct_read("read"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingFails(GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    });
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let LoopOutcome::PermissionRequested { request_id } = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap() else {
        panic!("proposal expected")
    };
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        stored
            .permission_decisions
            .push(crate::dynamic_run::PermissionDecisionProjection {
                request_id: request_id.clone(),
                input_revision: stored.input_revision,
                actor: crate::dynamic_run::PermissionDecisionActor::Owner,
                items: vec![crate::dynamic_run::PermissionItemDisposition {
                    item_id: "read".into(),
                    approved: true,
                    actor: crate::dynamic_run::PermissionDecisionActor::Owner,
                }],
            });
        assert!(stored.release_awaiting_permission(&request_id).unwrap());
    }
    tools.0.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    let resumed = resume_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: claim().current_pdp_scope,
            trigger_origin: crate::session::TriggerOrigin::PermissionDecision,
            ..claim()
        },
        &mut NullTurnSink,
    )
    .await;
    assert!(resumed.is_err(), "the turn still fails: {resumed:?}");
    assert_eq!(tools.0.reads.get(), 1, "the read ran");
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    // The release no longer holds input back, and the call has its only result,
    // so the next turn's model context is complete.
    assert!(stored.released_permission_call.is_none());
    assert!(stored.unclosed_tool_call_ids().is_empty());
    assert!(tool_result(stored, "read").contains("the read result could not be recorded"));
    assert_eq!(
        stored
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some("read"))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_host_input_mismatch_is_explained_once_before_a_new_proposal() {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(false);
    let model = ScriptModel {
        turns: RefCell::new([direct_read("first"), direct_read("second")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = MismatchTools {
        snapshot,
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, LoopOutcome::PermissionRequested { .. }),
        "{outcome:?}"
    );
    assert_eq!(tools.reads.get(), 2);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "first").starts_with("approved_input_mismatch"));
    assert_eq!(
        stored
            .awaiting_permission
            .as_ref()
            .map(|waiting| waiting.tool_call_id.as_str()),
        Some("second")
    );
    let [request] = stored.permission_requests.as_slice() else {
        panic!("one proposal")
    };
    assert_eq!(request.items[0].item_id, "second");
}

/// Loading a built-in tool by mistake fails with server text only, so its
/// error result registers under the issuing output and later answers stay
/// retainable.
#[tokio::test]
async fn a_capability_loading_error_registers_under_the_issuing_output() {
    use crate::ai_agent::source_registry::{
        NativeInputPolicyRegistration, RegisteredSourceContract, SOURCE_CONTRACT_REVISION,
    };
    let (session, providers, _snapshot, inventory, target, scope) = call_time_case(true);
    let issuing = crate::model_message_labels::model_bound_user_message(
        "issuing".into(),
        "stand-in issuing output".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
    .unwrap();
    let mut load = load_read();
    load.tool_calls[0].arguments_json =
        serde_json::json!({"tool_names":[crate::skills::authoring::REMOVE_TOOL]}).to_string();
    load.provider_meta.data_envelope = Some(issuing.clone());
    let model = ScriptModel {
        turns: RefCell::new([load, answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(_snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "disable the skill"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let stored = session.inner.borrow();
    let result = stored
        .as_ref()
        .unwrap()
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("load"))
        .unwrap()
        .clone();
    assert!(
        result.text.contains("built-in conversation tool"),
        "{}",
        result.text
    );
    let label = result.data_envelope.unwrap();
    assert!(
        label
            .envelope_id
            .starts_with(crate::model_message_labels::COMPOSED_RESULT_PREFIX)
    );
    assert_eq!(
        label.provenance.source_tool_name,
        ServerResultSource::ToolCallError.as_str()
    );
    assert_eq!(
        label.provenance.source_envelope_ids,
        std::slice::from_ref(&issuing.envelope_id)
    );
    let inputs = vec![issuing, label];
    let audit = crate::sink_authorizer::SinkProjectionAudit {
        destination: inputs[0].allowed_destinations[0].clone(),
        envelope_ids: inputs
            .iter()
            .map(|input| input.envelope_id.clone())
            .collect(),
        total_bytes: 32,
    };
    let snapshot = crate::model_egress::NativeModelInputSnapshot::capture(&audit, &inputs).unwrap();
    let registration =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert_eq!(
        registration.leaves[1].as_ref().map(|leaf| leaf.contract),
        Some(RegisteredSourceContract::NativeControl)
    );
}

/// Seam whose AI reviewer refuses the exact read.
struct ReviewDeniedReads {
    snapshot: crate::grant_disclosure::GrantDisclosureSnapshot,
}

#[async_trait(?Send)]
impl ToolSeam for ReviewDeniedReads {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        Ok(Some(self.snapshot.clone()))
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!("a refused read is never dispatched")
    }
    async fn run_read_versioned(
        &self,
        _: &ToolCall,
        _: &crate::seam::ExecContext,
        _: Option<&crate::action_version::ActionVersion>,
    ) -> crate::seam::ReadCompletion {
        crate::seam::ReadCompletion {
            outcome: Ok(ReadOutcome::ReviewDenied {
                reason: Box::new(crate::proposal_review::provenance::AiDecisionReason {
                    reason: "The owner did not ask to inspect the session.".into(),
                    reason_code: "owner_not_requested".into(),
                    provenance: crate::proposal_review::provenance::ReviewProvenance {
                        verifiable: true,
                        evidence: Vec::new(),
                        stored: Vec::new(),
                    },
                }),
            }),
            version_advance: None,
        }
    }
}

#[tokio::test]
async fn a_review_denied_read_shows_the_traced_reason_under_the_proposing_output() {
    let (session, providers, snapshot, inventory, target, scope) = call_time_case(false);
    let proposing = crate::model_message_labels::model_bound_user_message(
        "proposing".into(),
        "proposing".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
    .unwrap();
    let mut labelled = direct_read("read");
    labelled.provider_meta.data_envelope = Some(proposing.clone());
    let model = ScriptModel {
        turns: RefCell::new([labelled, answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = ReviewDeniedReads { snapshot };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    let result = stored
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("read"))
        .unwrap();
    assert!(result.text.contains("reason_code=owner_not_requested"));
    assert!(
        result
            .text
            .contains("Reviewer reason: The owner did not ask to inspect the session.")
    );
    let label = result.data_envelope.as_ref().unwrap();
    assert!(
        label
            .envelope_id
            .starts_with(crate::model_message_labels::COMPOSED_RESULT_PREFIX)
    );
    assert_eq!(
        label.provenance.source_envelope_ids[0],
        proposing.envelope_id
    );
}

/// A call-time case with two ungranted conversation-device reads:
/// `inspect_desktop_session` and `read_current_screen`.
fn group_case(
    reject_group_order: Option<u16>,
) -> (
    MemSession,
    crate::provider_registry::ProviderRegistry,
    crate::grant_disclosure::GrantDisclosureSnapshot,
    Vec<crate::capability_availability::CapabilityAvailability>,
    Vec<RegisteredTool>,
    AgentScope,
) {
    let (session, providers, mut snapshot, mut inventory, target, mut scope) = call_time_case(true);
    let screen = providers
        .capability_for_tool("read_current_screen")
        .unwrap();
    snapshot.ready_capabilities.push(screen.required_capability);
    scope.granted.push(screen.required_capability);
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        stored
            .scope_snapshot
            .granted
            .push(screen.required_capability);
        stored
            .turn_start_scope
            .granted
            .push(screen.required_capability);
    }
    inventory.push(crate::capability_availability::CapabilityAvailability {
        provider_id: providers
            .provider_for_capability(&screen.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone(),
        capability_id: screen.wire.capability_id.clone(),
        tool_name: "read_current_screen".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    });
    let mut registry = vec![target, screen.registered_tool()];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    let session = MemSession {
        reject_group_order,
        ..session
    };
    (session, providers, snapshot, inventory, registry, scope)
}

fn load_reads() -> ModelTurn {
    ModelTurn {
        tool_calls: vec![ToolCall {
            id: "load".into(),
            name: crate::capability_disclosure::LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({
                "tool_names": ["inspect_desktop_session", "read_current_screen"]
            })
            .to_string(),
        }],
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

/// One step: `a` reads the screen, `b` inspects the session, and `c` loads a
/// definition, which needs no approval. A screen read resolves ids against
/// observations, so it is never requested ahead of the calls before it; as
/// the first call needing approval it pauses the step.
fn grouped_step() -> ModelTurn {
    ModelTurn {
        tool_calls: vec![
            ToolCall {
                id: "a".into(),
                name: "read_current_screen".into(),
                arguments_json: "{}".into(),
            },
            ToolCall {
                id: "b".into(),
                name: "inspect_desktop_session".into(),
                arguments_json: "{}".into(),
            },
            ToolCall {
                id: "c".into(),
                name: crate::capability_disclosure::LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
                arguments_json: serde_json::json!({"tool_names": ["inspect_desktop_session"]})
                    .to_string(),
            },
        ],
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

fn decide(stored: &mut PersistedAgentSession, request_id: &str, item_id: &str, approved: bool) {
    let request = stored
        .permission_requests
        .iter_mut()
        .find(|request| request.request_id == request_id)
        .unwrap();
    request.state = if approved {
        crate::dynamic_run::PermissionRequestState::Approved
    } else {
        crate::dynamic_run::PermissionRequestState::Denied
    };
    stored
        .permission_decisions
        .push(crate::dynamic_run::PermissionDecisionProjection {
            request_id: request_id.into(),
            input_revision: stored.input_revision,
            actor: crate::dynamic_run::PermissionDecisionActor::Owner,
            items: vec![crate::dynamic_run::PermissionItemDisposition {
                item_id: item_id.into(),
                approved,
                actor: crate::dynamic_run::PermissionDecisionActor::Owner,
            }],
        });
}

/// Run the step until it pauses; returns the pausing request id.
async fn pause_grouped_step(
    session: &MemSession,
    deps: &LoopDeps<'_>,
    scope: AgentScope,
    sink: &mut dyn TurnSink,
) -> String {
    let outcome = run_agent_turn(
        deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session and the screen"),
        sink,
    )
    .await
    .unwrap();
    let LoopOutcome::PermissionRequested { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    // Nothing of the step ran or got a result while it waits.
    for id in ["a", "b", "c"] {
        assert!(
            !stored
                .conversation
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id)),
            "{id} has no result yet"
        );
    }
    assert!(stored.unclosed_tool_call_ids().is_empty());
    request_id
}

async fn resume(deps: &LoopDeps<'_>, scope: &AgentScope) -> LoopOutcome {
    resume_agent_turn(
        deps,
        ClaimTurnParams {
            current_pdp_scope: scope.clone(),
            trigger_origin: crate::session::TriggerOrigin::PermissionDecision,
            ..claim()
        },
        &mut NullTurnSink,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn two_determinable_calls_get_one_group_and_both_run() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut sink = SafetyEventLog(events.clone());
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut sink).await;
    // Planning started no tool for the later call and ran nothing.
    assert!(!events.borrow().iter().any(|event| event.ends_with(":b")));
    assert_eq!(tools.reads.get(), 0);
    assert_eq!(*session.group_saves.borrow(), [2], "one atomic group save");
    let member = {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let [first, second] = stored.permission_requests.as_slice() else {
            panic!("two requests: {:?}", stored.permission_requests)
        };
        assert_eq!(first.request_id, held);
        let (first_group, second_group) =
            (first.group.clone().unwrap(), second.group.clone().unwrap());
        assert_eq!(first_group.group_id, second_group.group_id);
        assert_eq!((first_group.order, second_group.order), (0, 1));
        assert_eq!((first_group.size, second_group.size), (2, 2));
        assert_eq!(second.items[0].item_id, "b");
        let member = second.request_id.clone();
        let waiting = stored.awaiting_permission.as_ref().unwrap();
        assert_eq!(waiting.tool_call_id, "a");
        assert_eq!(waiting.pending_call_ids, ["b", "c"]);
        assert_eq!(waiting.pre_requested_for("b"), Some(member.as_str()));
        assert_eq!(
            waiting.group_id.as_deref(),
            Some(first_group.group_id.as_str())
        );
        // The gate: no request of the group resumes before all are decided.
        decide(stored, &held, "a", true);
        let mut early = stored.clone();
        assert_eq!(
            early.claim_permission_resume(
                &held,
                "turn-2",
                None,
                None,
                1,
                early.scope_snapshot.clone(),
                "1970-01-01T00:00:01Z",
            ),
            Err(crate::session::ResumeClaimError::GroupPending)
        );
        decide(stored, &member, "b", true);
        // Any member's resume releases the step.
        assert!(stored.release_awaiting_permission(&member).unwrap());
        member
    };
    let _ = member;
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    resume(&deps, &resume_scope).await;
    assert_eq!(tools.reads.get(), 2, "both approved calls ran once");
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "a").contains("immutable observation"));
    assert!(tool_result(stored, "b").contains("immutable observation"));
    assert!(!tool_result(stored, "c").starts_with("not executed"));
    assert!(stored.released_permission_call.is_none());
    // The model is called once more, with every result of the step.
    let requests = requests.borrow();
    assert_eq!(requests.len(), 3);
    for id in ["a", "b", "c"] {
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        );
    }
}

#[tokio::test]
async fn approve_one_deny_other() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let member = stored.permission_requests[1].request_id.clone();
        decide(stored, &held, "a", true);
        decide(stored, &member, "b", false);
        assert!(stored.release_awaiting_permission(&held).unwrap());
    }
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    let outcome = resume(&deps, &resume_scope).await;
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(tools.reads.get(), 1, "only the approved call ran");
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "a").contains("immutable observation"));
    assert_eq!(
        tool_result(stored, "b"),
        "not executed: the owner refused this call"
    );
    // A refusal skips only its call: the later call still ran.
    assert!(!tool_result(stored, "c").starts_with("not executed"));
    assert_eq!(requests.borrow().len(), 3, "the model is called once after");
}

#[tokio::test]
async fn group_save_failure_publishes_call_k_alone() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(Some(1));
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    assert!(session.group_saves.borrow().is_empty());
    let mut stored = session.inner.borrow_mut();
    let stored = stored.as_mut().unwrap();
    // The host refused the later call's request: the held call's request is
    // published alone and the later calls get their cards when reached.
    let [request] = stored.permission_requests.as_slice() else {
        panic!("one request: {:?}", stored.permission_requests)
    };
    assert_eq!(request.request_id, held);
    assert!(request.group.is_none());
    assert_eq!(stored.last_event_seq, 1);
    let waiting = stored.awaiting_permission.as_ref().unwrap();
    assert!(waiting.group_id.is_none() && waiting.pre_requested.is_empty());
    assert_eq!(waiting.pending_call_ids, ["b", "c"]);
}

#[tokio::test]
async fn a_continuation_repause_forms_a_new_card_for_a_deferred_call() {
    // The group save is refused, so `b` has no card when the step resumes:
    // the continuation pauses again at `b` with its own request.
    let (session, providers, snapshot, inventory, registry, scope) = group_case(Some(1));
    let resume_scope = scope.clone();
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        decide(stored, &held, "a", true);
        assert!(stored.release_awaiting_permission(&held).unwrap());
    }
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    let LoopOutcome::PermissionRequested { request_id } = resume(&deps, &resume_scope).await else {
        panic!("the continuation pauses at b")
    };
    assert_eq!(tools.reads.get(), 1);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "a").contains("immutable observation"));
    let waiting = stored.awaiting_permission.as_ref().unwrap();
    assert_eq!(waiting.tool_call_id, "b");
    assert_eq!(waiting.request_id, request_id);
    assert_eq!(waiting.pending_call_ids, ["c"]);
    assert!(stored.released_permission_call.is_none());
    assert!(stored.unclosed_tool_call_ids().is_empty());
}

#[tokio::test]
async fn new_input_closes_pending_step_calls_with_accurate_text() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step()].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    let mut stored = session.inner.borrow_mut();
    let stored = stored.as_mut().unwrap();
    let revision = stored.input_revision + 1;
    stored.input_revision = revision;
    stored.begin_focus_epoch(revision, Vec::new()).unwrap();
    assert_eq!(
        tool_result(stored, "a"),
        "not executed: a new owner message replaced this request before approval; call it again if still needed"
    );
    for id in ["b", "c"] {
        assert_eq!(
            tool_result(stored, id),
            crate::session::AWAITING_STEP_CALL_CLOSED
        );
    }
    assert!(
        stored
            .permission_requests
            .iter()
            .all(|request| request.state
                == crate::dynamic_run::PermissionRequestState::NeedsRevalidation)
    );
    assert!(stored.unclosed_tool_call_ids().is_empty());
}

#[tokio::test]
async fn a_replayed_release_does_not_run_a_call_twice() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    let released = {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let member = stored.permission_requests[1].request_id.clone();
        decide(stored, &held, "a", true);
        decide(stored, &member, "b", true);
        assert!(stored.release_awaiting_permission(&held).unwrap());
        stored.released_permission_call.clone()
    };
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    resume(&deps, &resume_scope).await;
    assert_eq!(tools.reads.get(), 2);
    // The same release again: every call already has its result.
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        stored.released_permission_call = released;
        stored.released_in_claim = true;
    }
    let model_after = ScriptModel {
        turns: RefCell::new([answer("again")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let mut deps = super::deps(&session, &model_after, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    resume(&deps, &resume_scope).await;
    assert_eq!(tools.reads.get(), 2, "no call ran twice");
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    for id in ["a", "b", "c"] {
        assert_eq!(
            stored
                .conversation
                .iter()
                .filter(|message| message.tool_call_id.as_deref() == Some(id))
                .count(),
            1
        );
    }
}

#[test]
fn every_tool_effect_is_classified_for_continuation() {
    use crate::registry::ToolEffect;
    // Effects that bind state to the turn that runs them never continue.
    for effect in [
        ToolEffect::SchedulePlanning,
        ToolEffect::ScheduleQuery,
        ToolEffect::SkillAuthoring,
        ToolEffect::DirectoryPlanning,
        ToolEffect::GoalOpenPlanning,
        ToolEffect::GoalControl,
        ToolEffect::SubAgentPlanning,
        ToolEffect::SubAgentControl,
        ToolEffect::SubAgentQuery,
        ToolEffect::SubAgentWait,
        ToolEffect::SubAgentReport,
        ToolEffect::ConversationHistory,
        ToolEffect::ExternalWrite,
    ] {
        assert!(
            !crate::agent_loop::step_calls::continuable(effect),
            "{effect:?}"
        );
    }
    for effect in [
        ToolEffect::ReadOnly,
        ToolEffect::ActionPreview,
        ToolEffect::ExternalRead,
        ToolEffect::Mutating,
        ToolEffect::WaitTask,
        ToolEffect::RunProjection,
        ToolEffect::CapabilityDiscovery,
        ToolEffect::MemoryQuery,
        ToolEffect::MemoryWrite,
        ToolEffect::AgentHistory,
        ToolEffect::SkillLoading,
    ] {
        assert!(
            crate::agent_loop::step_calls::continuable(effect),
            "{effect:?}"
        );
    }
}

#[test]
fn the_model_guard_records_step_result_missing() {
    let (mut stored, _, _) = crate::grant_disclosure::tests::fixture();
    stored.conversation.push(ChatMessage::agent_tool_calls(
        "assistant",
        "",
        vec![crate::chat::ToolCallRef {
            id: "lost".into(),
            name: "inspect_desktop_session".into(),
            arguments_json: "{}".into(),
        }],
    ));
    // The guard records the result and continues; it never panics, even in
    // a debug build.
    assert_eq!(
        crate::agent_loop::close_untracked_unanswered_calls(&mut stored).unwrap(),
        ["lost"]
    );
    assert_eq!(
        tool_result(&stored, "lost"),
        "not executed: no result was recorded for this call; call it again if still needed"
    );
    assert!(stored.unclosed_tool_call_ids().is_empty());
}

#[tokio::test]
async fn an_equivalent_call_waits_on_an_unsettled_group_and_runs_once_it_settles() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let later = ModelTurn {
        tool_calls: vec![ToolCall {
            id: "b2".into(),
            name: "inspect_desktop_session".into(),
            arguments_json: "{}".into(),
        }],
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), later, answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope.clone(), &mut NullTurnSink).await;
    let member = {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let member = stored.permission_requests[1].request_id.clone();
        // The member is approved; the held request is still undecided. Another
        // claim then closes the step, leaving the group unsettled.
        decide(stored, &member, "b", true);
        stored
            .close_awaiting_permission(crate::session::AWAITING_PERMISSION_INTERRUPTED)
            .unwrap();
        stored.turn_state = crate::session::TurnState::Idle;
        member
    };
    // A later turn's call equals the approved member: it waits on the group.
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            turn_id: "turn-2".into(),
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u2", ChatRole::User, "inspect the session again"),
        &mut NullTurnSink,
    )
    .await;
    assert!(
        matches!(&outcome, Ok(LoopOutcome::PermissionRequested { request_id }) if *request_id == member),
        "{outcome:?}"
    );
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let waiting = stored.awaiting_permission.as_ref().unwrap();
        assert_eq!(waiting.tool_call_id, "b2");
        assert_eq!(waiting.request_id, member);
        assert!(waiting.group_id.is_some());
        assert_eq!(stored.permission_requests.len(), 2, "no new card");
        // Deciding the last member settles the group and releases the call.
        decide(stored, &held, "a", false);
        assert!(stored.release_awaiting_permission(&held).unwrap());
    }
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    resume(&deps, &resume_scope).await;
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "b2").contains("immutable observation"));
}

/// A permission-decision resume bridge as a host appends it.
fn permission_bridge(id: &str) -> ChatMessage {
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
        Sensitivity,
    };
    let mut message = ChatMessage::text(
        id,
        ChatRole::User,
        "AUTOMATIC SERVER CONTROL EVENT: the permission request was decided",
    );
    message.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("bridge-{id}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("blob-{id}"),
            sha256: "0".repeat(64),
            size_bytes: 1,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "agent-runtime-control".into(),
            source_tool_name: crate::model_message_labels::sources::PERMISSION_DECISION_RESUME
                .into(),
            source_object_id: Some(id.into()),
            source_envelope_ids: vec![],
        },
        sensitivity: Sensitivity::Public,
        allowed_destinations: vec![],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    });
    assert!(crate::permission_resume::is_permission_resume_message(
        &message
    ));
    message
}

fn step_of(calls: &[(&str, &str, &str)]) -> ModelTurn {
    ModelTurn {
        tool_calls: calls
            .iter()
            .map(|(id, name, arguments)| ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments_json: (*arguments).into(),
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        provider_meta: tool_meta(),
        ..Default::default()
    }
}

const LOAD_INSPECT: (&str, &str, &str) = (
    "c",
    crate::capability_disclosure::LOAD_CAPABILITY_DETAILS_TOOL_NAME,
    r#"{"tool_names":["inspect_desktop_session"]}"#,
);

#[tokio::test]
async fn scheduled_continuation_continues_through_a_second_pause() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let continuation = |scope: &AgentScope| ClaimTurnParams {
        current_pdp_scope: scope.clone(),
        trigger_origin: crate::session::TriggerOrigin::ScheduledContinuation,
        ..claim()
    };
    let outcome = run_agent_turn(
        &deps,
        continuation(&scope),
        ChatMessage::text("u", ChatRole::User, "inspect the session and the screen"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let LoopOutcome::PermissionRequested { request_id: first } = outcome else {
        panic!("{outcome:?}")
    };
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        // A scheduled continuation keeps one card at a time.
        assert_eq!(stored.permission_requests.len(), 1);
        assert!(stored.permission_requests[0].group.is_none());
        assert_eq!(
            stored
                .awaiting_permission
                .as_ref()
                .unwrap()
                .pending_call_ids,
            ["b", "c"]
        );
        decide(stored, &first, "a", true);
        assert!(stored.release_awaiting_permission(&first).unwrap());
    }
    let outcome = resume_agent_turn(&deps, continuation(&scope), &mut NullTurnSink)
        .await
        .unwrap();
    let LoopOutcome::PermissionRequested { request_id: second } = outcome else {
        panic!("{outcome:?}")
    };
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        assert!(tool_result(stored, "a").contains("immutable observation"));
        let waiting = stored.awaiting_permission.as_ref().unwrap();
        assert_eq!(waiting.tool_call_id, "b");
        assert_eq!(waiting.pending_call_ids, ["c"]);
        decide(stored, &second, "b", true);
        assert!(stored.release_awaiting_permission(&second).unwrap());
    }
    let outcome = resume_agent_turn(&deps, continuation(&scope), &mut NullTurnSink)
        .await
        .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(tools.reads.get(), 2);
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "b").contains("immutable observation"));
    assert!(!tool_result(stored, "c").starts_with("not executed"));
}

#[tokio::test]
async fn an_expired_issuing_envelope_closes_the_step() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let model = ScriptModel {
        turns: RefCell::new([load_reads(), grouped_step(), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let held = pause_grouped_step(&session, &deps, scope, &mut NullTurnSink).await;
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let member = stored.permission_requests[1].request_id.clone();
        decide(stored, &held, "a", true);
        decide(stored, &member, "b", true);
        let issuing = stored
            .conversation
            .iter()
            .rposition(|message| message.tool_calls.iter().any(|call| call.id == "a"))
            .unwrap();
        let mut expired = permission_bridge("expired").data_envelope.unwrap();
        expired.retention.expires_at_unix_ms = Some(1);
        stored.conversation[issuing].data_envelope = Some(expired);
        assert!(stored.release_awaiting_permission(&held).unwrap());
    }
    resume(&deps, &resume_scope).await;
    assert_eq!(tools.reads.get(), 0, "nothing of the expired step ran");
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    for id in ["a", "b", "c"] {
        assert_eq!(
            tool_result(stored, id),
            "not executed: the approval wait outlasted this step's data lifetime; call it again if still needed"
        );
    }
    assert!(stored.released_permission_call.is_none());
}

#[tokio::test]
async fn a_dependent_call_gets_its_card_during_the_continuation() {
    // The screen read resolves ids against observations, so it is not
    // requested up front; it gets its card when the continuation reaches it.
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    let resume_scope = scope.clone();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                load_reads(),
                step_of(&[
                    ("b", "inspect_desktop_session", "{}"),
                    ("a", "read_current_screen", "{}"),
                ]),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session and the screen"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let LoopOutcome::PermissionRequested { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        let [request] = stored.permission_requests.as_slice() else {
            panic!("only the first call has a card")
        };
        assert!(request.group.is_none());
        decide(stored, &request_id, "b", true);
        assert!(stored.release_awaiting_permission(&request_id).unwrap());
    }
    tools.snapshot.borrow_mut().grants[0].remaining_uses = 1;
    let outcome = resume(&deps, &resume_scope).await;
    let LoopOutcome::PermissionRequested { request_id: later } = outcome else {
        panic!("{outcome:?}")
    };
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert!(tool_result(stored, "b").contains("immutable observation"));
    assert_ne!(later, request_id);
    assert_eq!(
        stored.awaiting_permission.as_ref().unwrap().tool_call_id,
        "a"
    );
    assert_eq!(stored.permission_requests.len(), 2);
}

#[tokio::test]
async fn a_call_whose_card_does_not_fit_gets_a_result_and_the_step_continues() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    {
        let mut stored = session.inner.borrow_mut();
        let stored = stored.as_mut().unwrap();
        // Every slot of the bounded request list holds a pending request.
        for index in 0..crate::dynamic_run::MAX_PERMISSION_REQUESTS {
            let filler = crate::permission_tools::build_permission_request(
                &ToolCall {
                    id: format!("filler-{index}"),
                    name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                    arguments_json: serde_json::json!({"items":[{
                        "item_id": "info",
                        "tool_name": "read_system_info",
                        "suggested_ttl_seconds": 300,
                        "suggested_max_uses": 1,
                        "reason": "filler"
                    }]})
                    .to_string(),
                },
                &providers,
                format!("filler-{index}"),
                stored.input_revision,
                "1970-01-01T00:00:00Z".into(),
            )
            .unwrap();
            stored.add_permission_request(filler).unwrap();
        }
    }
    let model = ScriptModel {
        turns: RefCell::new(
            [
                load_reads(),
                step_of(&[("a", "read_current_screen", "{}"), LOAD_INSPECT]),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "read the screen"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    let stored = session.inner.borrow();
    let stored = stored.as_ref().unwrap();
    assert_eq!(
        tool_result(stored, "a"),
        "not executed: too many approval requests are open in this conversation; decide or let them finish, then call it again if still needed"
    );
    assert!(!tool_result(stored, "c").starts_with("not executed"));
    assert!(stored.awaiting_permission.is_none());
}

#[tokio::test]
async fn a_dangling_call_is_closed_and_reported_before_the_model_request() {
    let (session, providers, snapshot, inventory, registry, scope) = group_case(None);
    session
        .inner
        .borrow_mut()
        .as_mut()
        .unwrap()
        .conversation
        .push(ChatMessage::agent_tool_calls_with_replay(
            "earlier",
            "",
            vec![crate::chat::ToolCallRef {
                id: "lost".into(),
                name: "inspect_desktop_session".into(),
                arguments_json: "{}".into(),
            }],
            tool_meta().replay.unwrap(),
        ));
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let outcome = run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "continue"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(
        *session.missing_reports.borrow(),
        [vec!["lost".to_string()]]
    );
    // The provider never receives an unanswered call.
    assert!(requests.borrow()[0].messages.iter().any(|message| {
        message.tool_call_id.as_deref() == Some("lost")
            && message
                .text
                .starts_with("not executed: no result was recorded")
    }));
}

struct DeviceCommandPolicy(crate::command_confirmation::CommandPolicyContext);

#[async_trait(?Send)]
impl ToolSeam for DeviceCommandPolicy {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!("planning runs no tool")
    }

    async fn selected_command_policy(
        &self,
        _: &PersistedAgentSession,
        target_ref: &str,
    ) -> Result<crate::command_confirmation::CommandPolicyContext, AgentError> {
        assert_eq!(target_ref, "a");
        Ok(self.0.clone())
    }
}

/// The device case on an AI Agent turn with request devices: the step pauses
/// at a selected-device screen read, and the later `exec_command`, which has
/// no approved confirmation, is requested in the same group, alone in its
/// own request and for the same device.
#[tokio::test]
async fn a1_in_request_device_mode_pre_requests_screen_and_command() {
    let mut session = PersistedAgentSession::new(
        "run",
        "owner",
        "",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-06T00:00:00Z",
    );
    session.surface = AgentSessionSurface::AiAgent;
    session.input_revision = 1;
    session
        .bind_agent_request_targets(vec!["a".into()])
        .unwrap();
    let mut policy = crate::command_confirmation::test_policy();
    policy.actor_id = session.actor_id.clone();
    policy.target_device_id = "a".into();
    policy.directory_grants =
        crate::file_scope::TrustedDirectoryGrants::from_session(&session, "a", 1);
    policy.target_session_id = "connection-a:session-1".into();
    let providers = crate::ai_agent::ai_agent_provider_registry();
    let screen = providers
        .capability_for_tool("read_current_screen")
        .unwrap();
    let command_registry = providers.clone().with_command_policy(policy.clone());
    let exec = command_registry
        .capability_for_tool("exec_command")
        .unwrap();
    let device = crate::request_devices::RequestDevice {
        native_readiness: None,
        read_target: Some(crate::request_devices::SelectedDeviceTarget {
            device_id: "a".into(),
            target_ref: "a".into(),
            connection_id: "connection-a".into(),
            readiness_revision: 1,
        }),
        ready_provider_ids: vec![screen.wire.capability_id.clone()],
        approved_read_tools: vec![],
        target_ref: "a".into(),
        name: None,
        platform: Some("Windows"),
        online: true,
        ready_capabilities: vec![screen.required_capability, Capability::ShellExecConfirmed],
    };
    let devices = vec![device];
    assert!(session.uses_request_device_tools());
    let reads = crate::request_devices::read_tools(&devices, &session, &providers, true, None);
    let (command_tool, approved) =
        crate::request_devices::command_tool(&session, &devices, &providers).unwrap();
    assert!(!approved, "no command is approved yet");
    let screen_tool = reads
        .iter()
        .find(|(tool, _)| tool.name() == "read_current_screen")
        .map(|(tool, _)| tool.clone())
        .unwrap();
    let owned_exposed = vec![screen_tool.clone(), command_tool.clone()];
    let exposed = owned_exposed.iter().collect::<Vec<_>>();
    let definitions = owned_exposed
        .iter()
        .map(|tool| {
            let mut spec = tool.spec.clone();
            crate::ui_model_ids::project_tool(&mut spec);
            (spec.name.clone(), spec)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let inventory = [screen, exec]
        .iter()
        .map(
            |descriptor| crate::capability_availability::CapabilityAvailability {
                provider_id: command_registry
                    .provider_for_capability(&descriptor.wire.capability_id)
                    .unwrap()
                    .wire
                    .provider_id
                    .clone(),
                capability_id: descriptor.wire.capability_id.clone(),
                tool_name: descriptor.tool_spec.name.clone(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: true,
                reason: None,
            },
        )
        .collect::<Vec<_>>();
    let calls = vec![
        ToolCall {
            id: "screen".into(),
            name: "read_current_screen".into(),
            arguments_json: serde_json::json!({"target_ref": "a"}).to_string(),
        },
        ToolCall {
            id: "command".into(),
            name: "exec_command".into(),
            arguments_json: serde_json::json!({
                "target_ref": "a", "shell": "bash", "command": "echo fixture", "timeout_ms": 1000
            })
            .to_string(),
        },
    ];
    // The pausing request: the screen read on device `a`.
    let screen_request = crate::request_devices::plan_device_permission(
        &DeviceCommandPolicy(policy.clone()),
        Some(&providers),
        Some(desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner),
        &session,
        &devices,
        &reads,
        &crate::permission_tools::synthesize_request_call(
            &calls[0],
            &providers,
            &session.conversation,
            crate::permission_tools::DEFAULT_APPLICATION_SCOPE_USES,
        )
        .unwrap(),
        "screen-request",
        "2026-11-06T00:00:00Z",
    )
    .await
    .unwrap()
    .unwrap();
    let store = MemSession {
        inner: RefCell::new(Some(session.clone())),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new(Default::default()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = DeviceCommandPolicy(policy);
    let clock = || "2026-11-06T00:00:00Z".to_string();
    let mut deps = deps(&store, &model, &tools, &owned_exposed, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    let batch = crate::model_observability::tool::ToolBatch::default();
    let planning = crate::agent_loop::permission_group::Planning {
        calls: &calls,
        exposed: &exposed,
        owned_exposed: &owned_exposed,
        model_tool_definitions: &definitions,
        disclosure_enabled: true,
        tool_batch: &batch,
    };
    let members = crate::agent_loop::permission_group::plan(
        &deps,
        &session,
        &screen_request,
        false,
        crate::agent_loop::permission_proposal::ProposalContext {
            authority: None,
            request_devices: &devices,
            selected_reads: &reads,
            step_inventory: Some(&inventory),
            permission_candidates: &[],
            remaining: &calls[1..],
            parent_envelope: None,
            issuing_message_id: "step",
            planning: Some(&planning),
        },
        &planning,
        "2026-11-06T00:00:00Z",
    )
    .await;
    let [member] = members.as_slice() else {
        panic!("the command is requested with the screen read")
    };
    assert_eq!(member.call.id, "command");
    // `exec_command` stays alone in its request, bound to the same device.
    assert!(member.request.exact_command().is_some());
    assert_eq!(member.request.items.len(), 1);
    assert_eq!(
        crate::permission_grant::permission_device_id(&session, &member.request),
        crate::permission_grant::permission_device_id(&session, &screen_request)
    );
}
