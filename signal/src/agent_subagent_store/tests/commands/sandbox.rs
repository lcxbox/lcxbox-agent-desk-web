//! Command sandbox at the OSS host: an approved command is revalidated against
//! the worker's current readiness at the AI command edge and at dispatch, so a
//! change of sandbox support, execution identity or worker incarnation needs a
//! new approval while an unchanged readiness keeps it.
use super::snapshot::{
    PLAN_CHANGED, approved, assert_still_current, assistant_tools, context, owner_host_for,
};
use super::*;
use crate::command_policy::tests::{STANDARD_USER, mark_ready_with};
use desk_agent_protocol::Capability;
use desk_agent_protocol::exec::{
    ExecIdentity, ExecSandboxMode, ExecSandboxSupport, ExecSandboxUnsupportedReason,
};
use std::sync::Arc;

/// Readiness changes that each alter the frozen sandbox notice or target
/// session: sandbox support, execution identity and worker incarnation.
fn readiness_changes() -> [(&'static str, ExecSandboxSupport, ExecIdentity); 3] {
    [
        (
            "session-1",
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported),
            STANDARD_USER,
        ),
        (
            "session-1",
            ExecSandboxSupport::Supported,
            ExecIdentity {
                root: true,
                ..STANDARD_USER
            },
        ),
        ("session-2", ExecSandboxSupport::Supported, STANDARD_USER),
    ]
}

#[actix_web::test]
async fn the_command_edge_refuses_an_approval_after_a_readiness_change() {
    let (db, _, child, call, _) = approved_child().await;
    assert_eq!(
        approved(&child, &call).plan.sandbox_mode,
        ExecSandboxMode::Unrestricted
    );
    let (_host, connections) = owner_host_for(&child).await;
    for (incarnation, support, identity) in readiness_changes() {
        // The unchanged readiness keeps the unrestricted approval current.
        mark_ready_with(
            "host",
            "session-1",
            &[Capability::ShellExecConfirmed],
            ExecSandboxSupport::Supported,
            STANDARD_USER,
        );
        assert_still_current(&db, &connections, &child, &call).await;
        mark_ready_with(
            "host",
            incarnation,
            &[Capability::ShellExecConfirmed],
            support,
            identity,
        );
        let error = assistant_tools(&db, &child, connections.clone())
            .confirm_and_exec(&call, &context(&child, &call))
            .await
            .unwrap_err();
        assert_eq!(error.message, PLAN_CHANGED, "{incarnation} {support:?}");
        assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 0);
    }
}

#[actix_web::test]
async fn dispatch_refuses_an_approval_after_a_readiness_change() {
    let (db, _, child, call, _) = approved_child().await;
    let (_host, connections) = owner_host_for(&child).await;
    let confirmation = approved(&child, &call);
    let tools = crate::agent_exec::SignalAgentTools::new(
        db.clone(),
        connections,
        Arc::new(crate::agent_exec::SignalAgentExecPending::new()),
        "host".into(),
        desk_agent_protocol::evidence::EvidenceSnapshot::record(
            "fixture",
            "fixture",
            chrono::Utc::now().to_rfc3339(),
            Vec::new(),
        ),
        ExecAdmissionPolicy::OwnerInteractive,
        RiskLevel::Critical,
        vec!["bash".into()],
        60000,
    );
    let dispatch = || async {
        tools
            .execute_preapproved(
                &child,
                None,
                confirmation.validation_input.clone(),
                &confirmation,
                &call.arguments_json,
                &context(&child, &call),
                desk_agent_protocol::exec::ExecRequestId("sandbox-command".into()),
                "sandbox-generation".into(),
                desk_agent_protocol::exec::ApprovalId("sandbox-approval".into()),
                desk_agent_protocol::exec::ExecAuthorization::PerAction,
            )
            .await
            .unwrap_err()
            .message
    };
    for (incarnation, support, identity) in readiness_changes() {
        // The unchanged readiness passes the policy check and stops only for
        // want of a claimed task.
        mark_ready_with(
            "host",
            "session-1",
            &[Capability::ShellExecConfirmed],
            ExecSandboxSupport::Supported,
            STANDARD_USER,
        );
        assert_eq!(
            dispatch().await,
            "confirmed command has no durable execution task"
        );
        mark_ready_with(
            "host",
            incarnation,
            &[Capability::ShellExecConfirmed],
            support,
            identity,
        );
        assert_eq!(dispatch().await, PLAN_CHANGED, "{incarnation} {support:?}");
    }
}
