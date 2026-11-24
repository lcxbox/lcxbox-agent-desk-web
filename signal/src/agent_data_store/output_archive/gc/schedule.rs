//! Existing retention timers drive these bounded GC passes.
//! Shared storage owns discovery and hints; this host owns each collector transaction.
use super::collect_actor;
use crate::agent_data_store::{AgentStoreError, signed};
use desk_agent_data_store::output_gc;
pub(crate) use output_gc::OutputGcSweep;

pub(crate) fn run_due_once(
    db: &crate::config::connection::DatabaseConnection,
) -> impl std::future::Future<Output = Result<OutputGcSweep, AgentStoreError>> + Send + '_ {
    desk_diagnose_core::future::send_phase(|| run_due_once_inner(db))
}

async fn run_due_once_inner(
    db: &crate::config::connection::DatabaseConnection,
) -> Result<OutputGcSweep, AgentStoreError> {
    let now = u64::try_from(jiff::Timestamp::now().as_millisecond())
        .map_err(|_| AgentStoreError::Inaccessible)?;
    let policy = crate::agent_data_config::policy(db).await;
    run_with_policy(db, now, &policy.limits.processing, 1).await
}

#[cfg(test)]
pub(crate) async fn run_at_with_limit(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    maximum: u64,
) -> Result<OutputGcSweep, AgentStoreError> {
    let policy = crate::agent_data_config::policy(db).await;
    run_with_policy(
        db,
        now,
        &policy.limits.processing,
        maximum.min(u64::from(
            policy.limits.processing.output_gc_actor_batch_size,
        )) as u32,
    )
    .await
}

async fn run_with_policy(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    limits: &desk_agent_protocol::ai_agent::limits::AgentProcessingLimits,
    maximum: u32,
) -> Result<OutputGcSweep, AgentStoreError> {
    let now_signed = signed(now)?;
    let candidates = output_gc::due_actors_on(
        db,
        Some(&crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()),
        now_signed,
        limits.output_gc_interval_ms,
        maximum,
    )
    .await?;
    let mut outcome = OutputGcSweep::default();
    for (id, actor, previous) in candidates {
        if !output_gc::claim_on(
            db,
            id,
            &actor,
            previous,
            now_signed,
            limits.output_gc_interval_ms,
        )
        .await?
        {
            continue;
        }
        outcome.claimed_actors += 1;
        let collected = collect_actor(db, now, u64::from(limits.output_gc_batch_size)).await;
        match collected {
            Ok(value) => outcome.include(value)?,
            Err(_) => outcome.failed_actors += 1,
        }
    }
    Ok(outcome)
}

#[cfg(test)]
pub(crate) async fn claim_on<C: sea_orm::ConnectionTrait>(
    db: &C,
    id: i64,
    actor: &str,
    previous: Option<i64>,
    now: i64,
) -> Result<bool, AgentStoreError> {
    output_gc::claim_on(
        db,
        id,
        actor,
        previous,
        now,
        desk_agent_protocol::ai_agent::limits::AgentProcessingLimits::default()
            .output_gc_interval_ms,
    )
    .await
}
