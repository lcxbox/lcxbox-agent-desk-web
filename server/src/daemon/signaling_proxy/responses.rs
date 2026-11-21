//! Worker response and terminal notification adapters.

use super::*;

/// What to push to a session's connections after a resident worker reports:
/// the device's current offer for that session, when it differs from the last
/// list pushed or the report carries physical-display changes. Pushing an
/// unchanged list would reset an open settings dialog. `None` when there is
/// nothing to push (no offer yet: the serving worker has not reported, and its
/// own report will follow). Returns the list's fingerprint to remember.
pub(super) fn resident_capabilities_push(
    last_pushed: Option<&serde_json::Value>,
    offer: Option<desk_ipc_protocol::message::MediaCapabilities>,
    external_changes: &[String],
) -> Option<(
    desk_ipc_protocol::message::MediaCapabilities,
    serde_json::Value,
)> {
    let offer = offer?;
    let fingerprint =
        serde_json::json!([offer.video_device_list, offer.physical_display_capabilities]);
    (!external_changes.is_empty() || last_pushed != Some(&fingerprint))
        .then_some((offer, fingerprint))
}

pub(super) async fn send_media_capabilities_notifications(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    worker_key: Option<&desk_ipc_protocol::message::WorkerKey>,
    caps: &desk_ipc_protocol::message::MediaCapabilities,
    external_changes: &[String],
    outbound_tx: &broadcast::Sender<String>,
) {
    for connection_id in registry.all_connection_ids().await {
        if let Some(key) = worker_key
            && worker_mgr.connection_target(&connection_id).as_ref() != Some(&key.session)
        {
            continue;
        }
        let Some(pc) = registry.get(&connection_id).await else {
            continue;
        };
        let connection_epoch = pc.read().await.connection_epoch.clone();
        let data = desk_signal_facade::model::signal::MediaCapabilitiesStateChangedData {
            connection_epoch,
            video_device_list: caps.video_device_list.clone(),
            physical_display_mode_supported: cfg!(any(target_os = "windows", target_os = "macos")),
            physical_display_capabilities: caps.physical_display_capabilities.clone(),
            physical_display_external_changes: external_changes.to_vec(),
        };
        send_terminal_notification(
            outbound_tx,
            "MediaCapabilitiesStateChanged",
            &connection_id,
            SignalingType::MediaCapabilitiesStateChanged,
            Some(&data),
        );
    }
}

/// Dispatch a [`WorkerToService::VirtualDisplayAttachResult`] to the
/// supervisor if it exists; in non-service-daemon modes
/// (`virtual_display = None`) production routes never produce this
/// variant, so a stray reply is either a test fixture or a logic bug —
/// drop it with a warning rather than panic. Extracted from the proxy
/// match arm to keep the routing logic unit-testable without spinning
/// up the full proxy task / outbound channel infrastructure.
pub(super) async fn dispatch_attach_result(
    payload: desk_ipc_protocol::message::VirtualDisplayAttachResultPayload,
    virtual_display: Option<
        &std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>,
    >,
) {
    match virtual_display {
        Some(supervisor) => {
            supervisor.on_worker_attach_result(payload).await;
        }
        None => {
            warn!(
                "[SignalingProxy] VirtualDisplayAttachResult arrived while supervisor \
                 disabled (non-service-daemon mode?); dropping instance_id={}",
                payload.instance_id,
            );
        }
    }
}

/// Helper: build the outbound `SignalingModel` for a
/// `WorkerToService::VirtualDisplayMode` response. Applied →
/// success response carrying the mode the driver actually applied
/// (which may have been snapped to a nearby supported configuration);
/// Failed → `SignalingModel::error(INVALID_STATE, reason)`.
///
/// On a successful `Applied` outcome we also update the supervisor's
/// `last_known_refresh_hz` cache — this is the daemon's authoritative
/// source for the refresh-hz fallback when the auto-resolution browser
/// hook sends `refresh_hz=0`. Stray responses in non-service-daemon
/// mode (`supervisor=None`) are tolerated and the cache simply does
/// not update.
///
/// Kept as a free function so the routing logic can be unit-tested
/// without spinning up a signaling-proxy task. The call site in the
/// proxy loop only deals with the serialisation + outbound broadcast.
/// The browser reply to a physical display mode result. `None` for a result
/// without a connection: the restore after the last connection left, which
/// only completes the supervisor operation.
pub(super) fn build_physical_display_response(
    payload: desk_ipc_protocol::message::PhysicalDisplayModeResponsePayload,
) -> Option<Result<SignalingModel, desk_signal_facade::error::DeskSignalFacadeError>> {
    if payload.connection_id.is_empty() {
        return None;
    }
    let connection_id = Some(payload.connection_id);
    Some(match payload.outcome {
        PhysicalDisplayModeOutcome::Applied(data) => {
            let changed = PhysicalDisplayChangedData {
                connection_epoch: payload.connection_epoch,
                device_name: data.device_name,
                selector: data.selector,
                pixel_width: data.pixel_width,
                pixel_height: data.pixel_height,
                refresh_millihz: data.refresh_millihz,
                changed: data.changed,
                restored: data.restored,
            };
            SignalingModel::success_response(
                &payload.request_id,
                SignalingType::DisplaySettingsChanged,
                None,
                connection_id,
                Some(&changed),
            )
        }
        PhysicalDisplayModeOutcome::Failed(reason) => SignalingModel::error(
            &payload.request_id,
            SignalingType::DisplaySettingsChanged,
            None,
            connection_id,
            DeskErrorCode::INVALID_STATE,
            &reason,
        ),
        PhysicalDisplayModeOutcome::AppliedWithoutVideo { reason, .. } => SignalingModel::error(
            &payload.request_id,
            SignalingType::DisplaySettingsChanged,
            None,
            connection_id,
            DeskErrorCode::INVALID_STATE,
            &reason,
        ),
    })
}

pub(super) fn build_virtual_display_response(
    payload: desk_ipc_protocol::message::VirtualDisplayModeResponsePayload,
    supervisor: Option<&std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
) -> Result<SignalingModel, desk_signal_facade::error::DeskSignalFacadeError> {
    let connection_id = Some(payload.connection_id);
    match payload.outcome {
        VirtualDisplayModeOutcome::Applied(data) => {
            if let Some(supervisor) = supervisor {
                // Cache the full mode so the router's idempotent
                // short-circuit can compare exact (width, height,
                // refresh_hz) on the next inbound 205. `record_applied_mode`
                // silently drops any update with a zero component, so a
                // malformed worker echo cannot poison the cache.
                supervisor.record_applied_mode(data.width, data.height, data.refresh_hz);
            }
            let response = ChangeDisplaySettingsPayload {
                connection_epoch: payload.connection_epoch,
                width: data.width,
                height: data.height,
                refresh_hz: data.refresh_hz,
                auto: false,
            };
            SignalingModel::success_response(
                &payload.request_id,
                SignalingType::DisplaySettingsChanged,
                None,
                connection_id,
                Some(&response),
            )
        }
        VirtualDisplayModeOutcome::Failed(reason) => SignalingModel::error(
            &payload.request_id,
            SignalingType::DisplaySettingsChanged,
            None,
            connection_id,
            DeskErrorCode::INVALID_STATE,
            &reason,
        ),
    }
}

/// Rebuild the outbound `Manager*` response `SignalingModel` (with the
/// `request_id` echoed for correlation) and broadcast it to the
/// browser via `outbound_tx`. Build / serialise failures are
/// non-fatal — log + drop, no panic on the bus.
///
/// `from_connection_id` is left `None` (the daemon is the responder
/// here, not a peer browser); `to_connection_id` is `Option<String>`
/// because manager-plane / `ListTerminalCommands` requests can be HTTP-API-
/// triggered without an originating browser PC — in that case the
/// signal/manager server matches the response by `request_id` alone
/// (see `signal-facade::model::connection::request_callback_map`).
pub(super) fn send_manager_response<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    request_id: &str,
    connection_id: &Option<String>,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::success_response(
        request_id,
        signaling_type,
        None,
        connection_id.clone(),
        data,
    ) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} response for {connection_id:?}: \
                 {e} (request_id={request_id})"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} response model for {connection_id:?}: \
             {e} (request_id={request_id})"
        ),
    }
}

/// Build a server-initiated `new_request` `SignalingModel` (no `request_id`
/// correlation — the daemon mints a fresh one inside `new_request`)
/// for terminal-plane notifications (`TerminalOutputProduced`,
/// `TerminalClosed`) and broadcast it to the browser via
/// `outbound_tx`. Build / serialise failures are non-fatal —
/// log + drop, no panic on the bus. Mirrors the shape
/// `service::terminal` used to construct directly when worker still
/// owned the WS path.
pub(super) fn send_terminal_notification<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    connection_id: &str,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::new_request(signaling_type, Some(connection_id.to_string()), data) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} notification for \
                 {connection_id}: {e}"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} notification model for \
             {connection_id}: {e}"
        ),
    }
}

#[cfg(test)]
mod capability_push_tests {
    use super::resident_capabilities_push;
    use desk_ipc_protocol::message::MediaCapabilities;

    fn offer(backends: &[&str]) -> MediaCapabilities {
        MediaCapabilities {
            video_device_list: backends
                .iter()
                .map(|backend| (backend.to_string(), Vec::new()))
                .collect(),
            ..MediaCapabilities::default()
        }
    }

    /// Regression (on device 2026-11-21): the secure-desktop worker's report
    /// during UAC replaced the page's capture list and WGC vanished. The
    /// session's offer is pushed instead, once per change.
    #[test]
    fn the_offer_is_pushed_once_per_change() {
        let (pushed, fingerprint) =
            resident_capabilities_push(None, Some(offer(&["DXGI", "WGC"])), &[]).unwrap();
        assert_eq!(
            pushed.video_device_list.keys().collect::<Vec<_>>(),
            ["DXGI", "WGC"]
        );
        // The other worker reports; the offer is unchanged: no push, so an
        // open settings dialog is not reset.
        assert!(
            resident_capabilities_push(Some(&fingerprint), Some(offer(&["DXGI", "WGC"])), &[])
                .is_none()
        );
        // Physical-display changes are always delivered.
        assert!(
            resident_capabilities_push(
                Some(&fingerprint),
                Some(offer(&["DXGI", "WGC"])),
                &[r"\\.\DISPLAY1".to_string()]
            )
            .is_some()
        );
        // A different offer is pushed.
        assert!(
            resident_capabilities_push(Some(&fingerprint), Some(offer(&["DXGI"])), &[]).is_some()
        );
        // No offer yet: nothing to push.
        assert!(resident_capabilities_push(None, None, &[]).is_none());
    }
}
