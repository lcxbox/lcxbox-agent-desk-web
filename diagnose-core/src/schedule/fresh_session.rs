//! Construct an isolated initial context from a scheduled task definition.
use super::definition::ScheduledTaskDefinition;
use crate::{
    chat::{ChatMessage, ChatRole},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::AgentScope;
use sha2::{Digest, Sha256};

pub struct FreshSessionInput<'a> {
    pub run_id: &'a str,
    pub actor_id: &'a str,
    pub prompt: &'a str,
    pub locale: Option<&'a str>,
    pub policy_revision: i64,
    pub scope: AgentScope,
    pub now: &'a str,
}

/// The caller must verify current run authority and atomically insert this
/// session with its occurrence claim. A validated definition alone is not an
/// authorization.
pub fn initial_session(
    definition: &ScheduledTaskDefinition,
    input: FreshSessionInput<'_>,
) -> Result<PersistedAgentSession, &'static str> {
    if !input.run_id.starts_with("schedule-run-")
        || input.run_id.len() > 256
        || input.run_id.chars().any(char::is_whitespace)
        || input.run_id.chars().any(char::is_control)
        || input.actor_id.parse::<i32>().ok().is_none_or(|id| id <= 0)
        || input.prompt.is_empty()
        || input
            .locale
            .is_some_and(|locale| locale.is_empty() || locale.len() > 64)
        || desk_utils::datetime::parse_rfc3339(input.now).is_err()
    {
        return Err("invalid fresh task session identity");
    }
    crate::agent_policy::require_current_policy(input.policy_revision)
        .map_err(|_| "task policy is not current")?;
    let turn_id = format!("{}-turn", input.run_id);
    let mut session = PersistedAgentSession::new(
        input.run_id,
        input.actor_id,
        &definition.target_device_id,
        input.policy_revision,
        input.scope.clone(),
        input.now,
    );
    // This server-authored display identity belongs only to this occurrence.
    // Task management still resolves the session through the owner-bound run.
    let client_id = format!("task_{:x}", Sha256::digest(input.run_id.as_bytes()));
    session.adopt_client_metadata(Some(&client_id), AgentSessionSurface::AiAgent);
    session.response_locale = input.locale.map(str::to_owned);
    if definition.approved_by_ai {
        session.forbid_no_review();
    }
    session.begin_focus_epoch(1, Vec::<String>::new())?;
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.bind_agent_input_targets(&[])?;
    session
        .begin_turn(
            &turn_id,
            Some(input.run_id.into()),
            None,
            input.policy_revision,
            input.scope,
            input.now,
        )
        .map_err(|_| "fresh task turn cannot start")?;
    session.adopt_trigger(TriggerOrigin::ScheduledTask, &turn_id);
    session.conversation.push(
        ChatMessage::text(
            format!("{}:input", input.run_id),
            ChatRole::User,
            input.prompt,
        )
        .with_turn_id(turn_id),
    );
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(approved_by_ai: bool) -> PersistedAgentSession {
        let definition = ScheduledTaskDefinition {
            schedule_id: "schedule".into(),
            approved_by_ai,
            task_revision: 1,
            target_device_id: "device".into(),
            prompt: "Report the disk usage".into(),
            locale: None,
            model_id: None,
            policy_revision: 0,
            budget: crate::schedule::DEFAULT_TASK_BUDGET,
        };
        initial_session(
            &definition,
            FreshSessionInput {
                run_id: "schedule-run-1",
                actor_id: "1",
                prompt: "Report the disk usage",
                locale: None,
                policy_revision: crate::agent_policy::PERSONAL_AGENT_POLICY_REVISION,
                scope: AgentScope {
                    granted: vec![],
                    mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                    expires_at: None,
                    policy_name: None,
                },
                now: "2026-11-26T00:00:00Z",
            },
        )
        .unwrap()
    }

    #[test]
    fn a_run_of_a_task_the_approval_ai_approved_cannot_use_no_review() {
        assert!(run(false).has_direct_approval_origin());
        assert!(!run(true).has_direct_approval_origin());
    }
}
