//! Previously accepted answers are immutable content of this conversation.
use super::AgentStoreError;
use desk_diagnose_core::{
    ai_agent::source_registry::is_native_output_policy_source,
    model_egress::NativeModelInputSnapshot, session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;
pub(crate) fn validate_on<'a, C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &'a C,
    session: &'a PersistedAgentSession,
    snapshot: &'a NativeModelInputSnapshot,
    now: u64,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<u64>, AgentStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let mut deadline: Option<u64> = None;
        for source in snapshot.policy_sources() {
            if !is_native_output_policy_source(source) {
                continue;
            }
            let head = desk_agent_data_store::output_store::find_head_on(
                db,
                &session.conversation_id,
                source,
            )
            .await?
            .ok_or(AgentStoreError::Inaccessible)?;
            if head.actor_id != session.actor_id {
                return Err(AgentStoreError::Inaccessible);
            }
            if let Some(until) = head
                .summary()?
                .retained_policy
                .and_then(|policy| policy.content_hard_expires_at_unix_ms)
            {
                if until <= now {
                    return Err(AgentStoreError::Inaccessible);
                }
                deadline = Some(deadline.map_or(until, |old| old.min(until)));
            }
            crate::model_egress_store::SignalModelEgressStore::verify_output_head_on(
                db, &head, source,
            )
            .await?;
        }
        Ok(deadline)
    })
}
