use super::*;
use std::cell::Cell;

struct ReviewStore<'a> {
    mem: &'a MemSession,
    requests: Rc<RefCell<Vec<ModelRequest>>>,
    polls: Cell<usize>,
    decision: &'static str,
    cancelled: &'a Cell<bool>,
}
#[async_trait(?Send)]
impl SessionSeam for ReviewStore<'_> {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.mem.claim_turn(params).await?;
        session.surface = AgentSessionSurface::AiAssistant;
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
        let parent = crate::model_message_labels::model_bound_user_message(
            "model-input".into(),
            "timer".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope;
        append_internal_tool_result(
            session,
            parent.as_ref(),
            "draft".into(),
            &call.id,
            r#"{"schedule_id":"timer","state":"pending_review","kind":"fresh_task","awaiting_decision":true}"#.into(),
            "schedule_proposal",
        )?;
        session.pending_proposal_review =
            Some(crate::proposal_review::ProposalReviewRef::schedule("timer"));
        self.mem.save(session).await?;
        Ok("draft".into())
    }
    async fn poll_proposal_review(
        &self,
        session: &mut PersistedAgentSession,
        reference: &crate::proposal_review::ProposalReviewRef,
        _observation: crate::model_observability::tool::ToolObservation,
    ) -> Result<bool, AgentError> {
        assert_eq!(
            reference,
            &crate::proposal_review::ProposalReviewRef::schedule("timer")
        );
        let id = reference.id.as_str();
        assert_eq!(
            self.requests.borrow().len(),
            1,
            "no model call while awaiting the owner"
        );
        assert!(session.turn_state.is_active());
        let count = self.polls.get() + 1;
        self.polls.set(count);
        if self.decision == "stop" {
            self.cancelled.set(true);
            return Ok(false);
        }
        if count < 3 {
            return Ok(false);
        }
        let text = serde_json::json!({"event": self.decision, "schedule_id": id, "state": if self.decision == "scheduled_task_activated" { "active" } else { "deleted" }, "decided_by": "owner"}).to_string();
        let mut message = ChatMessage::system_event("decision", &text);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            session.conversation.last().unwrap().data_envelope.as_ref(),
            "decision",
            &text,
            "schedule_activation",
        )?;
        session.conversation.push(message);
        self.mem.save(session).await?;
        Ok(true)
    }
}
struct ReviewHeartbeat<'a>(&'a Cell<bool>);
struct ReviewGuard;
impl crate::seam::HeartbeatGuard for ReviewGuard {}
impl crate::seam::LeaseHeartbeat for ReviewHeartbeat<'_> {
    fn is_healthy(&self) -> bool {
        !self.0.get()
    }
    fn start(&self, _: String, _: u64) -> Box<dyn crate::seam::HeartbeatGuard> {
        Box::new(ReviewGuard)
    }
}

#[tokio::test]
async fn model_waits_for_explicit_schedule_decision_and_stop_interrupts_wait() {
    for decision in [
        "scheduled_task_activated",
        "scheduled_task_rejected",
        "stop",
    ] {
        let mem = MemSession::default();
        let requests = Rc::new(RefCell::new(vec![]));
        let cancelled = Cell::new(false);
        let store = ReviewStore {
            mem: &mem,
            requests: requests.clone(),
            polls: Cell::new(0),
            decision,
            cancelled: &cancelled,
        };
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
        let heartbeat = ReviewHeartbeat(&cancelled);
        let mut deps = deps(&mem, &model, &tools, &registry, &clock);
        deps.session_seam = &store;
        deps.heartbeat = Some(&heartbeat);
        let result = run_agent_turn(
            &deps,
            claim(),
            ChatMessage::text("user", ChatRole::User, "remind me in one minute"),
            &mut NullTurnSink,
        )
        .await;
        if decision == "stop" {
            assert!(result.is_err());
            assert_eq!(requests.borrow().len(), 1);
            assert_eq!(store.polls.get(), 1);
        } else {
            assert!(matches!(result.unwrap(), LoopOutcome::Answered(_)));
            assert_eq!(store.polls.get(), 3);
            assert_eq!(requests.borrow().len(), 2);
            let projected_requests = requests.borrow();
            let result = projected_requests[1]
                .messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some("create"))
                .unwrap();
            let projected: serde_json::Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(projected["review_complete"], true);
            assert_eq!(projected["awaiting_decision"], false);
            assert!(
                mem.inner
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .conversation
                    .iter()
                    .any(|m| m.role == ChatRole::Tool && m.text.contains("pending_review"))
            );
            drop(projected_requests);
            assert!(
                requests.borrow()[1]
                    .messages
                    .iter()
                    .any(|m| m.text.contains(decision))
            );
        }
        assert!(
            mem.inner
                .borrow()
                .as_ref()
                .unwrap()
                .pending_proposal_review
                .is_none()
        );
        assert!(tools.calls.borrow().is_empty());
    }
}

/// A skill proposal shares the waiting phase: a decision receipt ends the
/// wait, and a closed AI window leaves the proposal to the owner.
struct SkillStore<'a> {
    mem: &'a MemSession,
    polls: Cell<usize>,
    decide: bool,
    window_closed: &'a Cell<bool>,
}
#[async_trait(?Send)]
impl SessionSeam for SkillStore<'_> {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.mem.claim_turn(params).await?;
        session.surface = AgentSessionSurface::AiAssistant;
        Ok(session)
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.mem.save(session).await
    }
    async fn latest_input_revision(&self, id: &str) -> Result<Option<u64>, AgentError> {
        self.mem.latest_input_revision(id).await
    }
    async fn propose_skill(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        _observation: crate::model_observability::tool::ToolObservation,
    ) -> Result<String, AgentError> {
        let parent = crate::model_message_labels::model_bound_user_message(
            "model-input".into(),
            "skill".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope;
        append_internal_tool_result(
            session,
            parent.as_ref(),
            "skill-proposal:propose".into(),
            &call.id,
            r#"{"proposal_id":"p1","status":"pending","awaiting_decision":true}"#.into(),
            "skill_proposal",
        )?;
        session.pending_proposal_review =
            Some(crate::proposal_review::ProposalReviewRef::skill("p1"));
        self.mem.save(session).await?;
        Ok("skill-proposal:propose".into())
    }
    async fn poll_proposal_review(
        &self,
        session: &mut PersistedAgentSession,
        reference: &crate::proposal_review::ProposalReviewRef,
        _observation: crate::model_observability::tool::ToolObservation,
    ) -> Result<bool, AgentError> {
        assert_eq!(
            reference,
            &crate::proposal_review::ProposalReviewRef::skill("p1")
        );
        let count = self.polls.get() + 1;
        self.polls.set(count);
        if !self.decide {
            // The clock passes the AI review window after the first poll.
            self.window_closed.set(true);
            return Ok(false);
        }
        if count < 2 {
            return Ok(false);
        }
        let text = serde_json::json!({"event": crate::proposal_review::SKILL_DECIDED_EVENT,
            "proposal_id": "p1", "status": "approved", "version": 1, "decided_by": "approval_ai"})
        .to_string();
        let mut message = ChatMessage::system_event("skill-decision", &text);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            session.conversation.last().unwrap().data_envelope.as_ref(),
            "skill-decision",
            &text,
            "skill_proposal_decision",
        )?;
        session.conversation.push(message);
        self.mem.save(session).await?;
        Ok(true)
    }
}

#[tokio::test]
async fn skill_proposal_waits_for_its_decision_or_the_review_window() {
    for decide in [true, false] {
        let mem = MemSession::default();
        let requests = Rc::new(RefCell::new(vec![]));
        let window_closed = Cell::new(false);
        let store = SkillStore {
            mem: &mem,
            polls: Cell::new(0),
            decide,
            window_closed: &window_closed,
        };
        let model = ScriptModel {
            turns: RefCell::new(
                [
                    tool_use_args(
                        "propose",
                        crate::skills::authoring::PROPOSE_TOOL,
                        r#"{"name":"disk-report","description":"Report disks","body":"Run df -h","reason":"Owner asked"}"#,
                    ),
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
        let clock = || {
            if window_closed.get() {
                "2026-09-09T00:11:00Z"
            } else {
                "2026-09-09T00:00:00Z"
            }
            .to_string()
        };
        let mut deps = deps(&mem, &model, &tools, &registry, &clock);
        deps.session_seam = &store;
        let result = run_agent_turn(
            &deps,
            claim(),
            ChatMessage::text("user", ChatRole::User, "keep this as a skill"),
            &mut NullTurnSink,
        )
        .await;
        assert!(matches!(result.unwrap(), LoopOutcome::Answered(_)));
        assert_eq!(requests.borrow().len(), 2);
        let second = &requests.borrow()[1];
        let projected = second
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("propose"))
            .unwrap();
        let projected: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
        assert_eq!(projected["awaiting_decision"], false);
        if decide {
            assert_eq!(store.polls.get(), 2);
            assert_eq!(projected["status"], "approved");
            assert_eq!(projected["review_complete"], true);
        } else {
            assert_eq!(store.polls.get(), 1);
            assert_eq!(projected["status"], "pending");
            assert_eq!(
                projected["event"],
                crate::proposal_review::SKILL_AWAITING_OWNER_EVENT
            );
        }
        let saved = mem.inner.borrow();
        let saved = saved.as_ref().unwrap();
        assert!(saved.pending_proposal_review.is_none());
        // The stored proposal receipt is unchanged; the model projection is not saved.
        assert!(
            saved
                .conversation
                .iter()
                .any(|m| m.role == ChatRole::Tool && m.text.contains("\"awaiting_decision\":true"))
        );
    }
}
