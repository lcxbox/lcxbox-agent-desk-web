use super::*;

fn gateway_parent(text: &str) -> Option<desk_agent_protocol::data_lineage::DataEnvelope> {
    crate::model_message_labels::model_bound_user_message(
        "model-input".into(),
        text.into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
}

/// Stores a proposal receipt the way the data store does: labelled, bound to
/// the proposing turn and awaiting a decision.
fn store_receipt(
    session: &mut PersistedAgentSession,
    message_id: &str,
    call_id: &str,
    text: &str,
    source: crate::model_message_labels::ServerResultSource,
) -> Result<(), AgentError> {
    append_internal_tool_result(
        session,
        gateway_parent(source.as_str()).as_ref(),
        message_id.into(),
        call_id,
        &crate::model_content::ModelContent::server_text(text),
        source,
    )?;
    session.conversation.last_mut().unwrap().turn_id = session.current_turn_id.clone();
    Ok(())
}

struct ProposalStore<'a> {
    mem: &'a MemSession,
}
#[async_trait(?Send)]
impl SessionSeam for ProposalStore<'_> {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.mem.claim_turn(params).await?;
        session.surface = AgentSessionSurface::AiAgent;
        Ok(session)
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.mem.save(session).await
    }
    async fn latest_input_revision(&self, id: &str) -> Result<Option<u64>, AgentError> {
        self.mem.latest_input_revision(id).await
    }
    async fn manage_schedule_tool(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        _observation: crate::model_observability::tool::ToolObservation,
    ) -> Result<String, AgentError> {
        let id = format!("schedule-proposal:{}", call.id);
        store_receipt(
            session,
            &id,
            &call.id,
            r#"{"schedule_id":"timer","state":"pending_review","kind":"fresh_task","awaiting_decision":true}"#,
            crate::model_message_labels::ServerResultSource::ScheduleProposal,
        )?;
        self.mem.save(session).await?;
        Ok(id)
    }
    async fn propose_skill(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        _observation: crate::model_observability::tool::ToolObservation,
    ) -> Result<String, AgentError> {
        let id = format!("skill-proposal:{}", call.id);
        store_receipt(
            session,
            &id,
            &call.id,
            r#"{"proposal_id":"p1","status":"pending","awaiting_decision":true}"#,
            crate::model_message_labels::ServerResultSource::SkillProposal,
        )?;
        self.mem.save(session).await?;
        Ok(id)
    }
}

/// Writes the decision event as the deciding transaction does, then clears
/// the pause as the decision-resume claim does.
fn decide(mem: &MemSession, text: &str, source: &str) {
    let mut guard = mem.inner.borrow_mut();
    let session = guard.as_mut().unwrap();
    let mut message = ChatMessage::system_event("decision", text);
    let parent = session
        .conversation
        .iter()
        .rev()
        .find_map(|message| message.data_envelope.as_ref());
    message.data_envelope = crate::model_message_labels::named_envelope(
        &crate::model_content::ModelContent::builder()
            .unverified(&crate::model_content::Unverified::new(text))
            .build(),
        parent,
        "decision",
        source,
        false,
    )
    .unwrap();
    session.conversation.push(message);
    session.decision_pause = None;
}

fn resume_claim() -> ClaimTurnParams {
    ClaimTurnParams {
        turn_id: "turn-2".into(),
        request_id: None,
        connection_id: None,
        trigger_origin: crate::session::TriggerOrigin::ProposalDecision,
        ..claim()
    }
}

fn bridge() -> ChatMessage {
    ChatMessage::text(
        "bridge",
        ChatRole::User,
        "AUTOMATIC SERVER CONTROL EVENT: a decision was recorded.",
    )
}

#[tokio::test]
async fn a_schedule_proposal_ends_the_turn_and_its_decision_resumes_it() {
    for decision in ["scheduled_task_activated", "scheduled_task_rejected"] {
        let mem = MemSession::default();
        let requests = Rc::new(RefCell::new(vec![]));
        let store = ProposalStore { mem: &mem };
        let model = ScriptModel {
            turns: RefCell::new(
                [
                    tool_use_args("create", crate::schedule::proposal::REQUEST_SCHEDULE, r#"{"kind":"fresh_task","title":"Reminder","prompt":"Remind me","rule":{"kind":"once","at":"2026-09-10T00:00:00Z"}}"#),
                    answer("decision received"),
                ]
                .into(),
            ),
            requests: requests.clone(),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let registry = crate::schedule::proposal::registry();
        let clock = || "2026-09-09T00:00:00Z".to_string();
        let mut deps = deps(&mem, &model, &tools, &registry, &clock);
        deps.session_seam = &store;
        let paused = run_agent_turn(
            &deps,
            claim(),
            ChatMessage::text("user", ChatRole::User, "remind me in one minute"),
            &mut NullTurnSink,
        )
        .await
        .unwrap();
        let LoopOutcome::DecisionRequested { pause } = paused else {
            panic!("the proposal must end the turn: {paused:?}");
        };
        assert_eq!(pause.kind, crate::proposal_review::DecisionKind::Schedule);
        assert_eq!(
            (pause.id.as_str(), pause.call_id.as_str()),
            ("timer", "create")
        );
        assert_eq!(requests.borrow().len(), 1, "no model call while paused");
        {
            let saved = mem.inner.borrow();
            let saved = saved.as_ref().unwrap();
            assert_eq!(saved.decision_pause.as_ref(), Some(&pause));
            assert!(!saved.turn_state.is_active());
        }

        let text = serde_json::json!({"event": decision, "schedule_id": "timer", "state": if decision == "scheduled_task_activated" { "active" } else { "deleted" }, "decided_by": "owner"}).to_string();
        decide(&mem, &text, "schedule_activation");
        let resumed =
            resume_agent_turn_after_permission(&deps, resume_claim(), bridge(), &mut NullTurnSink)
                .await
                .unwrap();
        assert!(matches!(resumed, LoopOutcome::Answered(_)));
        assert_eq!(requests.borrow().len(), 2);
        let second = &requests.borrow()[1];
        let projected = second
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("create"))
            .unwrap();
        let projected: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
        assert_eq!(projected["review_complete"], true);
        assert_eq!(projected["awaiting_decision"], false);
        assert!(second.messages.iter().any(|m| m.text.contains(decision)));
        // The stored receipt is unchanged; only the model sees the decision on it.
        assert!(
            mem.inner
                .borrow()
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .any(|m| m.role == ChatRole::Tool && m.text.contains("pending_review"))
        );
        assert!(tools.calls.borrow().is_empty());
    }
}

#[tokio::test]
async fn a_skill_proposal_ends_the_turn_and_later_calls_wait_for_the_decision() {
    let mem = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let store = ProposalStore { mem: &mem };
    let model = ScriptModel {
        turns: RefCell::new(
            [
                ModelTurn {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "propose".into(),
                            name: crate::skills::authoring::PROPOSE_TOOL.into(),
                            arguments_json: r#"{"name":"disk-report","description":"Report disks","body":"Run df -h","reason":"Owner asked"}"#.into(),
                        },
                        ToolCall {
                            id: "after".into(),
                            name: crate::skills::authoring::PROPOSE_TOOL.into(),
                            arguments_json: r#"{"name":"other","description":"Other","body":"x","reason":"y"}"#.into(),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("noted"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = crate::skills::authoring::registry();
    let clock = || "2026-09-09T00:00:00Z".to_string();
    let mut deps = deps(&mem, &model, &tools, &registry, &clock);
    deps.session_seam = &store;
    let paused = run_agent_turn(
        &deps,
        claim(),
        ChatMessage::text("user", ChatRole::User, "keep this as a skill"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert!(matches!(
        paused,
        LoopOutcome::DecisionRequested { ref pause }
            if pause.kind == crate::proposal_review::DecisionKind::Skill && pause.id == "p1"
    ));
    {
        let saved = mem.inner.borrow();
        let saved = saved.as_ref().unwrap();
        // The later call of the same batch did not run.
        assert!(saved.conversation.iter().any(|m| {
            m.tool_call_id.as_deref() == Some("after")
                && m.text
                    .contains("this step paused for a decision on an earlier call")
        }));
    }

    let text = serde_json::json!({"event": crate::proposal_review::SKILL_DECIDED_EVENT,
        "proposal_id": "p1", "status": "approved", "version": 1, "decided_by": "approval_ai"})
    .to_string();
    decide(&mem, &text, "skill_proposal_decision");
    let resumed =
        resume_agent_turn_after_permission(&deps, resume_claim(), bridge(), &mut NullTurnSink)
            .await
            .unwrap();
    assert!(matches!(resumed, LoopOutcome::Answered(_)));
    let second = &requests.borrow()[1];
    let projected = second
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("propose"))
        .unwrap();
    let projected: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
    assert_eq!(projected["status"], "approved");
    assert_eq!(projected["review_complete"], true);
    assert_eq!(projected["awaiting_decision"], false);
    // The resume has no paused device call to replay and cannot propose again.
    assert!(
        !second
            .messages
            .iter()
            .any(|m| m.text.contains("PERMISSION CONTINUATION CHECKPOINT"))
    );
    assert!(
        !second
            .tools
            .iter()
            .any(|tool| tool.name == crate::skills::authoring::PROPOSE_TOOL)
    );
}

#[test]
fn owner_question_pauses_once_and_decline_publishes_a_separate_text_wait_on_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(Box::pin(exercise_owner_question_decline()));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

async fn exercise_owner_question_decline() {
    let mem = MemSession::default();
    let store = ProposalStore { mem: &mem };
    let questions =
        r#"{"questions":[{"question":"Zone?","options":["UTC","Local"]},{"question":"Day?"}]}"#;
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("question", "ask_user", questions),
                answer("What would you like me to adjust?"),
            ]
            .into(),
        ),
        requests: Default::default(),
    };
    let tools = RecordingTools {
        calls: Default::default(),
        reply: String::new(),
    };
    let registry = crate::owner_question::registry();
    assert!(
        registry
            .iter()
            .all(crate::agent_policy::is_targetless_web_tool)
    );
    let clock = || "2026-09-09T00:00:00Z".to_owned();
    let mut deps = deps(&mem, &model, &tools, &registry, &clock);
    deps.session_seam = &store;
    let outcome = run_agent_turn(
        &deps,
        claim(),
        ChatMessage::text("user", ChatRole::User, "Report outages"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let LoopOutcome::DecisionRequested { pause } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(model.requests.borrow().len(), 1);
    {
        let mut stored = mem.inner.borrow_mut();
        let session = stored.as_mut().unwrap();
        let wait = session.owner_question.as_ref().unwrap();
        assert_eq!(wait.view.deadline_unix_ms, None);
        assert_eq!(pause.id, wait.view.wait_id);
        session
            .conversation
            .push(ChatMessage::system_event("later", "Background detail"));
        crate::owner_question::insert_result(
            session,
            &desk_agent_protocol::ai_agent::owner_question::OwnerQuestionResult::Declined,
            None,
        )
        .unwrap();
        let issuing = session
            .conversation
            .iter()
            .position(|m| {
                m.message_id == session.owner_question.as_ref().unwrap().issuing_message_id
            })
            .unwrap();
        assert_eq!(
            session.conversation[issuing + 1].text,
            r#"{"status":"declined"}"#
        );
        assert_eq!(session.conversation[issuing + 2].message_id, "later");
        session.owner_question.as_mut().unwrap().open = false;
        session.owner_question.as_mut().unwrap().needs_adjustment = true;
        session.decision_pause = None;
    }
    let resumed =
        resume_agent_turn_after_permission(&deps, resume_claim(), bridge(), &mut NullTurnSink)
            .await
            .unwrap();
    let LoopOutcome::DecisionRequested { pause: follow_up } = resumed else {
        panic!("{resumed:?}")
    };
    assert_ne!(follow_up.id, pause.id);
    let stored = mem.inner.borrow();
    let wait = stored.as_ref().unwrap().owner_question.as_ref().unwrap();
    assert!(wait.accepts_reply("Use both"));
    assert!(
        model.requests.borrow()[1]
            .tools
            .iter()
            .all(|spec| crate::model_controls::is_control(&spec.name))
    );
    assert!(tools.calls.borrow().is_empty());
}
