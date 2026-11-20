use super::*;
use desk_ipc_protocol::message::{DesktopChangedPayload, SessionKey};

fn observation(name: &str, restricted: bool, tick: u64) -> DesktopObservation {
    DesktopObservation {
        name: name.to_string(),
        restricted,
        observed_tick_ms: tick,
        received: 0,
    }
}

fn serve(desktop: DesktopTarget) -> Option<InteractiveDesktopDecision> {
    Some(InteractiveDesktopDecision::Serve(desktop))
}

fn frozen(decision: Option<InteractiveDesktopDecision>) -> bool {
    matches!(decision, Some(InteractiveDesktopDecision::Freeze(_)))
}

#[test]
fn the_newest_report_of_either_worker_decides_the_route() {
    use DesktopTarget::{WindowsDefault, WindowsWinlogon};
    let open_default = observation("Default", false, 10);
    let open_saver = observation("Screen-saver", false, 10);
    let restricted = observation("Winlogon", true, 30);
    let secure = observation("Winlogon", false, 20);
    let on_default = observation("Default", false, 40);
    let on_saver = observation("Screen-saver", false, 40);
    let on_other = observation("Custom", false, 40);

    // Nothing reported yet: no decision.
    assert_eq!(decide_interactive_desktop(false, None, None), None);
    // The session worker can open its newest desktop, whatever its name.
    assert_eq!(
        decide_interactive_desktop(false, Some(&open_default), None),
        serve(WindowsDefault)
    );
    assert_eq!(
        decide_interactive_desktop(
            false,
            Some(&open_saver),
            Some(&observation("Winlogon", false, 5))
        ),
        serve(WindowsDefault)
    );
    // UAC: the session worker is shut out and the restricted worker sees Winlogon.
    assert_eq!(
        decide_interactive_desktop(false, Some(&restricted), Some(&secure)),
        serve(WindowsWinlogon)
    );
    assert_eq!(
        decide_interactive_desktop(false, Some(&open_default), Some(&secure)),
        serve(WindowsWinlogon)
    );
    // Shut out, and the restricted worker has not confirmed the secure desktop.
    assert!(frozen(decide_interactive_desktop(
        false,
        Some(&restricted),
        None
    )));
    assert!(frozen(decide_interactive_desktop(
        false,
        Some(&restricted),
        Some(&observation("Custom", false, 10))
    )));
    // Back from UAC: the restricted worker's newest report names Default.
    assert_eq!(
        decide_interactive_desktop(false, Some(&restricted), Some(&on_default)),
        serve(WindowsDefault)
    );
    // A desktop the session worker last opened, reported newer by the restricted worker.
    assert_eq!(
        decide_interactive_desktop(false, Some(&open_saver), Some(&on_saver)),
        serve(WindowsDefault)
    );
    // Any other desktop waits for the session worker.
    assert!(frozen(decide_interactive_desktop(
        false,
        Some(&open_default),
        Some(&on_other)
    )));
    // The restricted worker never serves a desktop it cannot read.
    assert!(frozen(decide_interactive_desktop(
        false,
        None,
        Some(&observation("Winlogon", true, 50))
    )));
}

#[test]
fn equal_ticks_are_ordered_by_receipt() {
    let mut first = observation("Default", false, 7);
    let mut second = observation("Winlogon", false, 7);
    first.received = 1;
    second.received = 2;
    assert_eq!(
        decide_interactive_desktop(false, Some(&first), Some(&second)),
        serve(DesktopTarget::WindowsWinlogon)
    );
    first.received = 3;
    assert_eq!(
        decide_interactive_desktop(false, Some(&first), Some(&second)),
        serve(DesktopTarget::WindowsDefault)
    );
}

#[test]
fn the_sign_in_screen_is_served_only_on_the_secure_desktop() {
    assert_eq!(
        decide_interactive_desktop(true, None, Some(&observation("Winlogon", false, 1))),
        serve(DesktopTarget::WindowsWinlogon)
    );
    assert!(frozen(decide_interactive_desktop(
        true,
        None,
        Some(&observation("Default", false, 2))
    )));
    assert_eq!(decide_interactive_desktop(true, None, None), None);
}

fn manager_with_session() -> (WorkerManager, SessionKey, WorkerKey, WorkerKey) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    let (manager, _messages) = WorkerManager::new(settings, PcRegistry::new());
    manager.enable_session_targeting_for_test();
    let session = SessionKey {
        platform_session_id: "3".into(),
        session_generation: 1,
    };
    let default_key = WorkerKey {
        session: session.clone(),
        desktop: DesktopTarget::WindowsDefault,
    };
    let winlogon_key = WorkerKey {
        session: session.clone(),
        desktop: DesktopTarget::WindowsWinlogon,
    };
    (manager, session, default_key, winlogon_key)
}

fn report(name: &str, restricted: bool, tick: u64) -> DesktopChangedPayload {
    DesktopChangedPayload {
        name: name.into(),
        restricted,
        observed_tick_ms: tick,
    }
}

#[tokio::test]
async fn only_the_current_incarnation_of_a_worker_is_heard() {
    let (manager, _session, default_key, _winlogon_key) = manager_with_session();
    let (tx, _rx) = mpsc::unbounded_channel();
    let incarnation = manager
        .install_resident_for_test(default_key.clone(), tx)
        .await;
    let stale = WorkerIncarnation(incarnation.0.wrapping_sub(1));
    assert_eq!(
        manager
            .record_desktop_observation(&default_key, stale, &report("Default", false, 1))
            .await,
        None
    );
    assert_eq!(
        manager
            .record_desktop_observation(&default_key, incarnation, &report("Default", false, 1))
            .await,
        serve(DesktopTarget::WindowsDefault)
    );
}

#[tokio::test]
async fn a_target_that_is_not_ready_freezes_the_current_route() {
    let (manager, session, default_key, winlogon_key) = manager_with_session();
    let (default_tx, mut default_rx) = mpsc::unbounded_channel();
    let (winlogon_tx, _winlogon_rx) = mpsc::unbounded_channel();
    let default_incarnation = manager
        .install_resident_for_test(default_key.clone(), default_tx)
        .await;
    manager
        .install_resident_for_test(winlogon_key.clone(), winlogon_tx)
        .await;
    manager.active_interactive_routes.lock().unwrap().insert(
        session.clone(),
        ActiveInteractiveRoute {
            worker_key: default_key.clone(),
            incarnation: default_incarnation,
            route_epoch: 1,
            activated_at: Instant::now(),
            activated_at_unix_ms: 1,
            accepting_interactive: true,
        },
    );
    // UAC appears but the Winlogon worker has not reported capabilities.
    let result = manager
        .apply_interactive_desktop_decision(
            &session,
            InteractiveDesktopDecision::Serve(DesktopTarget::WindowsWinlogon),
        )
        .await;
    assert!(result.is_err());
    assert!(
        !manager.active_interactive_routes.lock().unwrap()[&session].accepting_interactive,
        "the session worker must not keep serving the secure desktop"
    );
    assert!(matches!(
        default_rx.recv().await,
        Some(ServiceToWorker::SetInteractiveRoute(route)) if !route.active
    ));
    assert_eq!(
        manager
            .desired_interactive_desktops
            .lock()
            .unwrap()
            .get(&session),
        Some(&DesktopTarget::WindowsWinlogon),
        "the target activates when its capabilities arrive"
    );
}

#[tokio::test]
async fn a_frozen_session_is_not_reactivated_by_a_capability_refresh() {
    let (manager, session, default_key, _winlogon_key) = manager_with_session();
    let (default_tx, _default_rx) = mpsc::unbounded_channel();
    manager
        .install_resident_for_test(default_key.clone(), default_tx)
        .await;
    let _ = manager
        .apply_interactive_desktop_decision(
            &session,
            InteractiveDesktopDecision::Freeze("test".into()),
        )
        .await;
    assert!(manager.frozen_sessions.lock().unwrap().contains(&session));
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
    let capabilities = MediaCapabilities {
        video_codecs: vec![desk_ipc_protocol::message::MediaCodec::H264],
        video_device_list: std::collections::BTreeMap::from([(
            "dxgi".to_string(),
            vec![DisplayInfo {
                device_name: "display".to_string(),
                display_device_name: None,
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                resolutions: vec![],
                attached_to_desktop: true,
                rotation: 0,
                current_capture_resolution: None,
            }],
        )]),
        ..Default::default()
    };
    manager
        .set_resident_worker_capabilities(&default_key, capabilities)
        .await;
    tokio::task::yield_now().await;
    assert!(
        !manager
            .active_interactive_routes
            .lock()
            .unwrap()
            .contains_key(&session),
        "a frozen session stays without an interactive route"
    );
}

#[tokio::test]
async fn a_wayland_portal_command_reaches_the_only_linux_session_worker() {
    let (manager, _session, _default_key, _winlogon_key) = manager_with_session();
    let linux_key = |id: &str| WorkerKey {
        session: SessionKey {
            platform_session_id: id.into(),
            session_generation: 1,
        },
        desktop: DesktopTarget::LinuxSession,
    };
    let cancel = || {
        ServiceToWorker::CancelWaylandPortal(
            desk_ipc_protocol::message::CancelWaylandPortalPayload {
                operation_id: "op".into(),
                generation: 1,
            },
        )
    };

    assert!(
        manager
            .send_to_wayland_session_worker(cancel())
            .await
            .is_err()
    );

    let (first_tx, mut first_rx) = mpsc::unbounded_channel();
    manager
        .install_resident_for_test(linux_key("2"), first_tx)
        .await;
    manager
        .send_to_wayland_session_worker(cancel())
        .await
        .expect("the only session worker takes it");
    assert!(matches!(
        first_rx.try_recv(),
        Ok(ServiceToWorker::CancelWaylandPortal(_))
    ));

    let (second_tx, mut second_rx) = mpsc::unbounded_channel();
    manager
        .install_resident_for_test(linux_key("4"), second_tx)
        .await;
    assert!(
        manager
            .send_to_wayland_session_worker(cancel())
            .await
            .is_err()
    );
    assert!(first_rx.try_recv().is_err());
    assert!(second_rx.try_recv().is_err());
}

/// Regression (fast user switch, on device 2026-11-20): the capabilities
/// task found the route already switched by the reconcile, yet paused and
/// replayed media a second time, which sent the worker a second StartMedia.
#[tokio::test]
async fn activating_the_current_route_reports_it_without_switching() {
    let (manager, session, default_key, _winlogon_key) = manager_with_session();
    let (default_tx, mut default_rx) = mpsc::unbounded_channel();
    let incarnation = manager
        .install_resident_for_test(default_key.clone(), default_tx)
        .await;
    manager.active_interactive_routes.lock().unwrap().insert(
        session.clone(),
        ActiveInteractiveRoute {
            worker_key: default_key.clone(),
            incarnation,
            route_epoch: 7,
            activated_at: Instant::now(),
            activated_at_unix_ms: 1,
            accepting_interactive: true,
        },
    );
    assert_eq!(
        manager.activate_interactive_worker(&default_key).await,
        Ok(InteractiveActivation::AlreadyCurrent(7))
    );
    assert!(
        default_rx.try_recv().is_err(),
        "an already-current route sends the worker nothing"
    );
}
