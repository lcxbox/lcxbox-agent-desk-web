//! Central wrapper and edge-exec authorization gates.

use super::*;

/// Optional-wrapper gate for the central remote read-tool call
/// (`RemoteToolRequest`). Unlike the AI control frames it may arrive either bare
/// or wrapped, so it gets this gate rather than [`gate_authz_frame`]'s
/// require-wrapper rule. The frame may arrive either:
///
/// - **bare** — the enterprise manager emits the raw payload; the configured
///   link's authentication is the trust anchor, so a bare frame passes through
///   to the router unchanged; or
/// - **wrapped** in an [`AuthorizedControlPayload`] — an OSS signal central brain
///   stamps and wraps every frame. A wrapper is validated against the frame's
///   `request_id`, this daemon's audience, and expiry (replay / misroute
///   defense-in-depth), then unwrapped to its inner payload for the router.
///
/// A wrapper that fails validation is dropped (no denied-result protocol exists
/// for this read-only frame; the central reaper times the pending entry out).
pub(super) fn gate_optional_central_wrapper(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        return AuthzGateOutcome::Pass(model, None);
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

    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    AuthzGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Outcome of the dedicated `EdgeExecRequest` authorization gate. Unlike the
/// generic [`gate_authz_frame`] (which drops a frame whose wrapper fails to
/// validate), an edge-exec request that fails
/// validation is answered with a synthesized denied result so the central
/// pending entry resolves rather than hanging. Only a frame that cannot be
/// correlated at all (no `request_id`) is dropped outright.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "moved once per message; boxing variants would change matches on every platform"
)]
pub(super) enum EdgeExecGateOutcome {
    /// Validated: the unwrapped frame (data = inner `EdgeExecRequestPayload`) plus the
    /// validated authorization block to thread into the router handler.
    Pass(SignalingModel, AuthorizationBlock),
    /// Trusted source but the request is unauthorized / malformed; answer the
    /// central brain with a `RejectedBeforeDispatch` carrying `reason`.
    Denied { request_id: String, reason: String },
    /// Uncorrelatable garbage; drop silently (no result can be attributed).
    Drop(String),
}

/// Dedicated authorization gate for `EdgeExecRequest` (central → daemon). Validates the
/// `AuthorizedControlPayload<EdgeExecRequestPayload>` wrapper; on success unwraps
/// the inner payload and returns the validated authorization block.
pub(super) fn gate_edge_exec_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> EdgeExecGateOutcome {
    let request_id = model.request_id.clone();
    if request_id.is_empty() {
        return EdgeExecGateOutcome::Drop(
            "EdgeExecRequest without request_id (cannot correlate a result)".to_string(),
        );
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => {
            return EdgeExecGateOutcome::Denied {
                request_id,
                reason: "pep_rejected:authz:missing_payload".to_string(),
            };
        }
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return EdgeExecGateOutcome::Denied {
                request_id,
                reason: format!("pep_rejected:authz:malformed_wrapper:{e}"),
            };
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&request_id, expected_audience, now_rfc3339)
    {
        return EdgeExecGateOutcome::Denied {
            request_id,
            reason: format!("pep_rejected:authz:{e:?}"),
        };
    }

    let unwrapped = SignalingModel::new(
        &request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    EdgeExecGateOutcome::Pass(unwrapped, wrapper.authz)
}
