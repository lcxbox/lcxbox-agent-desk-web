//! Grants of an approval group, issued by the decision that settles it.
//!
//! A grouped request's decision is recorded at once, but its grants are
//! issued only when every request of the group is decided, so each grant's
//! time limit starts then. The settling decision rebuilds the grants of every
//! other approved member from that member's durable decision, against the
//! current delegation, goal and unattended mode. A member that no longer
//! builds gets no grant; its call is skipped when the step reaches it.
use super::*;
use desk_diagnose_core::dynamic_run::{PermissionDecisionSource, PermissionRequest};

/// Build the grants of the group members other than `deciding_request_id`.
/// A storage failure is returned; a member whose grants no longer validate
/// is logged and skipped.
pub(super) async fn settlement_grants_on(
    txn: &crate::config::connection::DatabaseTransaction,
    session: &PersistedAgentSession,
    deciding_request_id: &str,
    members: &[PermissionRequest],
    context: &PermissionGrantIssuanceContext<'_>,
) -> Result<Vec<desk_agent_protocol::capability_grant::CapabilityGrant>, AgentError> {
    let mut issued = Vec::new();
    for member in members
        .iter()
        .filter(|member| member.request_id != deciding_request_id)
    {
        let Some(decision) =
            permission_receipt::decided_on(txn, session, &member.request_id).await?
        else {
            log::warn!(
                "[ai-agent] approval group member {} has no recorded decision; no grant issued",
                member.request_id
            );
            continue;
        };
        // A storage failure fails the decision transaction; only a member that
        // no longer validates is skipped.
        match member_grants_on(txn, session, member, &decision, context).await? {
            Ok(grants) => issued.extend(grants),
            Err(error) => log::warn!(
                "[ai-agent] approval group member {} issued no grant at settlement: {}",
                member.request_id,
                error.message
            ),
        }
    }
    Ok(issued)
}

async fn member_grants_on(
    txn: &crate::config::connection::DatabaseTransaction,
    session: &PersistedAgentSession,
    member: &PermissionRequest,
    decision: &desk_diagnose_core::dynamic_run::PermissionDecidedEvent,
    context: &PermissionGrantIssuanceContext<'_>,
) -> Result<
    Result<Vec<desk_agent_protocol::capability_grant::CapabilityGrant>, AgentError>,
    AgentError,
> {
    // The grant keeps the readiness its request was frozen against; dispatch
    // refuses it once the device has moved on.
    let member_context = PermissionGrantIssuanceContext {
        surface: context.surface,
        registry: context.registry,
        inventory: context.inventory,
        readiness_revision: member
            .selected_device_target()
            .map_or(context.readiness_revision, |target| {
                target.readiness_revision
            }),
        now_unix_ms: context.now_unix_ms,
        implicit_fresh_object_refs: context.implicit_fresh_object_refs,
    };
    let original_reads = if permission_device_id(session, member).is_empty()
        || desk_diagnose_core::permission_grant::requires_original_read_context(
            member,
            &decision.items,
        ) {
        desk_diagnose_core::future::boxed(|| {
            crate::agent_run_event_store::input_context::original_on(txn, session)
        })
        .await?
    } else {
        None
    };
    // The grants are compiled from the recorded decision as when it was made:
    // the member is replayed as the request that decision applied to.
    let mut decided = member.clone();
    decided.state = desk_diagnose_core::dynamic_run::PermissionRequestState::Pending;
    let mut grants = match build_permission_grants(
        session,
        &decided,
        &decision.items,
        &member_context,
        original_reads.as_ref(),
    ) {
        Ok(grants) => grants,
        Err(error) => return Ok(Err(error)),
    };
    match &decision.decision_source {
        PermissionDecisionSource::AutomaticReview {
            delegation_id,
            goal_binding,
            ..
        } => {
            let (delegation, current_goal) =
                match crate::agent_approval_store::ai_grant_parent_on(txn, session, delegation_id)
                    .await
                {
                    Ok(parent) => parent,
                    // The delegation is gone or no longer decodes.
                    Err(sea_orm::DbErr::Custom(_)) => {
                        return Ok(Err(internal("AI approval delegation is unavailable")));
                    }
                    Err(error) => {
                        return Err(internal(format!("read AI approval delegation: {error}")));
                    }
                };
            if &current_goal != goal_binding {
                return Ok(Err(internal("the reviewed goal is no longer current")));
            }
            if let Err(error) = desk_diagnose_core::permission_grant::bind_ai_permission_grants(
                session,
                &decided,
                decision,
                &delegation,
                current_goal
                    .as_ref()
                    .map(|(id, revision)| (id.as_str(), *revision)),
                context.now_unix_ms,
                &mut grants,
            ) {
                return Ok(Err(error));
            }
        }
        PermissionDecisionSource::OwnerNoReview { .. } => {
            let Some(preference) =
                crate::agent_approval_store::preference_for_session_on(txn, session)
                    .await
                    .map_err(|_| internal("unattended command mode is unavailable"))?
            else {
                return Ok(Err(internal(
                    "unattended command mode is no longer current",
                )));
            };
            if let Err(error) = desk_diagnose_core::permission_grant::bind_no_review_command_grant(
                session,
                &preference,
                decision,
                &mut grants,
            ) {
                return Ok(Err(error));
            }
        }
        PermissionDecisionSource::UserDecision
        | PermissionDecisionSource::SystemDecision { .. } => {}
    }
    Ok(Ok(grants))
}
