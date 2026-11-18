//! Stamp checks for AI, RequestRemoteAccess, and terminal frames.
//!
//! Every upstream the daemon connects to (the manager, the signal at
//! `signaling_url`, and the embedded signal of Default / ServiceDaemon) is a
//! central the owner configured, and every one of them stamps these frames. A
//! frame of these kinds therefore must arrive stamped; an unstamped one is a
//! forgery or a stamp-stripping downgrade and is dropped.

use super::*;

/// Outcome of the authorization check for one inbound AI frame.
#[allow(
    clippy::large_enum_variant,
    reason = "moved once per message; boxing variants would change matches on every platform"
)]
pub(super) enum AuthzGateOutcome {
    /// Forward this (possibly unwrapped) model to the router, carrying the
    /// validated authorization block when the frame was an AI control frame.
    Pass(SignalingModel, Option<AuthorizationBlock>),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// True for the control-end AI frames that carry an authorization wrapper.
pub(super) fn is_ai_control_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::InvokeAgentCapability
            | SignalingType::PreviewExecution
            | SignalingType::DispatchComputerAction
            | SignalingType::CancelComputerAction
            | SignalingType::QueryComputerActionState
            | SignalingType::ManageFileRecovery
    )
}

fn has_stamp_wrapper(model: &SignalingModel) -> bool {
    model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .is_some_and(|o| o.contains_key("authz") && o.contains_key("inner"))
}

fn unwrapped_frame(model: &SignalingModel, inner: serde_json::Value) -> SignalingModel {
    SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(inner),
        model.response_state.clone(),
    )
}

/// Check an inbound AI frame against the authorization wrapper rules:
///
/// - Non-AI frames pass through untouched.
/// - An AI control frame must carry an `AuthorizedControlPayload`; a bare one is
///   dropped rather than falling through to the local default scope, which would
///   bypass the central's policy.
/// - The wrapper is validated against the frame (`request_id`), this daemon's
///   audience, and expiry; on success the inner payload is unwrapped and
///   forwarded with the validated block, which the AI handlers enforce.
pub(super) fn gate_authz_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    if !is_ai_control_frame(model.signaling_type) {
        return AuthzGateOutcome::Pass(model, None);
    }
    if !has_stamp_wrapper(&model) {
        return AuthzGateOutcome::Drop(
            "bare AI control frame (authorization wrapper required)".to_string(),
        );
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return AuthzGateOutcome::Drop("wrapper frame had no data".to_string()),
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => return AuthzGateOutcome::Drop(format!("malformed authz wrapper: {e}")),
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return AuthzGateOutcome::Drop(format!("authz wrapper rejected: {e:?}"));
    }

    let unwrapped = unwrapped_frame(&model, wrapper.inner);
    AuthzGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Outcome of the capability-ceiling check for a `RequestRemoteAccess` or
/// `StartTerminal`.
#[allow(
    clippy::large_enum_variant,
    reason = "moved once per message; boxing variants would change matches on every platform"
)]
pub(super) enum RequestRemoteGateOutcome {
    /// Forward the unwrapped model to the router with its validated stamp.
    Pass(SignalingModel, RequestRemoteAuthz),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// Shared check for the two admission-establishing frames. The central stamps
/// every one (owner → no ceiling, redeemed grant → its ceiling), so a bare frame
/// is a forgery or a grant session stripping its stamp to pass as the owner, and
/// is dropped. A stamp is validated against the frame (`request_id`), this
/// daemon's audience, and expiry before the inner frame is forwarded with it.
fn gate_admission_frame(
    model: SignalingModel,
    label: &str,
    expected_audience: &str,
    now_rfc3339: &str,
    parse: impl FnOnce(
        serde_json::Value,
    ) -> Result<(serde_json::Value, RequestRemoteAuthz), serde_json::Error>,
) -> RequestRemoteGateOutcome {
    if !has_stamp_wrapper(&model) {
        return RequestRemoteGateOutcome::Drop(format!(
            "bare {label} (capability-ceiling stamp required)"
        ));
    }
    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return RequestRemoteGateOutcome::Drop("stamped frame had no data".to_string()),
    };
    let (inner, authz) = match parse(raw) {
        Ok(parts) => parts,
        Err(e) => {
            return RequestRemoteGateOutcome::Drop(format!("malformed {label} stamp wrapper: {e}"));
        }
    };
    if let Err(e) = authz.validate(&model.request_id, expected_audience, now_rfc3339) {
        return RequestRemoteGateOutcome::Drop(format!("{label} stamp rejected: {e:?}"));
    }
    let unwrapped = unwrapped_frame(&model, inner);
    RequestRemoteGateOutcome::Pass(unwrapped, authz)
}

/// Check an inbound `RequestRemoteAccess` against the capability-ceiling stamp
/// rules. The validated stamp is the ceiling the router and worker enforce for
/// the new session.
pub(super) fn gate_request_remote_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    gate_admission_frame(
        model,
        "RequestRemoteAccess",
        expected_audience,
        now_rfc3339,
        |raw| serde_json::from_value::<AuthorizedRequestRemote>(raw).map(|w| (w.inner, w.authz)),
    )
}

/// Check an inbound `StartTerminal` against the capability-ceiling stamp rules.
/// The remote terminal opens on a distinct WS connection that never sends a
/// `RequestRemoteAccess`, so `StartTerminal` is its admission-establishing frame;
/// `handle_start_terminal_inbound` turns the validated stamp into the
/// connection's ceiling, admission, and grant index.
pub(super) fn gate_start_terminal_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    gate_admission_frame(
        model,
        "StartTerminal",
        expected_audience,
        now_rfc3339,
        |raw| serde_json::from_value::<AuthorizedTerminalStart>(raw).map(|w| (w.inner, w.authz)),
    )
}
