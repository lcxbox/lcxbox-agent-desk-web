use super::*;
use crate::daemon::session_monitor::WindowsSessionRegistration;
use desk_ipc_protocol::message::SessionKey;

fn session(id: u32, generation: u64) -> SessionKey {
    SessionKey {
        platform_session_id: id.to_string(),
        session_generation: generation,
    }
}

fn registration(id: u32, generation: u64, has_user: bool) -> WindowsSessionRegistration {
    WindowsSessionRegistration {
        session: session(id, generation),
        session_id: id,
        display_name: format!("session {id}"),
        station_name: "Console".into(),
        foreground: true,
        has_user,
    }
}

fn key(id: u32, generation: u64, desktop: DesktopTarget) -> WorkerKey {
    WorkerKey {
        session: session(id, generation),
        desktop,
    }
}

#[test]
fn a_signed_in_session_gets_both_workers() {
    let plan = plan_windows_reconcile(&[], &[registration(1, 1, true)]);
    assert_eq!(
        plan.start,
        vec![
            (key(1, 1, DesktopTarget::WindowsDefault), 1),
            (key(1, 1, DesktopTarget::WindowsWinlogon), 1),
        ]
    );
    assert!(plan.stop.is_empty() && plan.stop_sessions.is_empty());
}

#[test]
fn the_sign_in_screen_gets_only_the_restricted_worker() {
    let plan = plan_windows_reconcile(&[], &[registration(1, 1, false)]);
    assert_eq!(
        plan.start,
        vec![(key(1, 1, DesktopTarget::WindowsWinlogon), 1)]
    );
}

#[test]
fn signing_in_adds_the_default_worker_and_keeps_the_connections() {
    let current = [key(1, 1, DesktopTarget::WindowsWinlogon)];
    let plan = plan_windows_reconcile(&current, &[registration(1, 1, true)]);
    assert_eq!(
        plan.start,
        vec![(key(1, 1, DesktopTarget::WindowsDefault), 1)]
    );
    assert!(plan.stop.is_empty());
    assert!(
        plan.stop_sessions.is_empty(),
        "no connection may be dropped"
    );
}

#[test]
fn a_session_that_is_gone_stops_its_workers_and_connections() {
    let current = [
        key(1, 1, DesktopTarget::WindowsDefault),
        key(1, 1, DesktopTarget::WindowsWinlogon),
    ];
    let plan = plan_windows_reconcile(&current, &[registration(1, 2, true)]);
    assert_eq!(plan.stop_sessions, vec![session(1, 1)]);
    assert_eq!(plan.stop, current.to_vec());
    assert_eq!(plan.start.len(), 2);
}

#[test]
fn a_session_without_a_user_keeps_no_default_worker() {
    let current = [
        key(1, 1, DesktopTarget::WindowsDefault),
        key(1, 1, DesktopTarget::WindowsWinlogon),
    ];
    let plan = plan_windows_reconcile(&current, &[registration(1, 1, false)]);
    assert_eq!(plan.stop, vec![key(1, 1, DesktopTarget::WindowsDefault)]);
    assert!(plan.stop_sessions.is_empty());
    assert!(plan.start.is_empty());
}

#[tokio::test]
async fn a_sign_in_screen_answers_capability_lookups_from_its_restricted_worker() {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    let (manager, _messages) = WorkerManager::new(settings, PcRegistry::new());
    let signed_out = session(1, 1);
    manager
        .signed_out_sessions
        .lock()
        .unwrap()
        .insert(signed_out.clone());
    assert_eq!(
        manager.default_interactive_desktop(&signed_out, DesktopTarget::WindowsWinlogon),
        Some(DesktopTarget::WindowsWinlogon)
    );
    assert_eq!(
        manager.default_interactive_desktop(&signed_out, DesktopTarget::WindowsDefault),
        None
    );
    let signed_in = session(2, 1);
    assert_eq!(
        manager.default_interactive_desktop(&signed_in, DesktopTarget::WindowsDefault),
        Some(DesktopTarget::WindowsDefault)
    );
    assert_eq!(
        manager.default_interactive_desktop(&signed_in, DesktopTarget::WindowsWinlogon),
        None
    );
}

#[test]
fn starts_back_off_exponentially_and_reset_after_a_stable_run() {
    let mut backoff = ResidentStartBackoff::default();
    let worker = key(1, 1, DesktopTarget::WindowsWinlogon);
    let t0 = Instant::now();
    assert!(backoff.may_start(&worker, t0));
    backoff.failed(&worker, t0, false);
    assert!(!backoff.may_start(&worker, t0 + Duration::from_millis(900)));
    assert!(backoff.may_start(&worker, t0 + Duration::from_secs(1)));
    backoff.failed(&worker, t0 + Duration::from_secs(1), false);
    assert!(!backoff.may_start(&worker, t0 + Duration::from_millis(2900)));
    assert!(backoff.may_start(&worker, t0 + Duration::from_secs(3)));
    for _ in 0..10 {
        backoff.failed(&worker, t0, false);
    }
    assert!(!backoff.may_start(&worker, t0 + Duration::from_secs(29)));
    assert!(
        backoff.may_start(&worker, t0 + Duration::from_secs(30)),
        "capped at 30 s"
    );

    backoff.started(&worker, t0);
    backoff.failed(&worker, t0 + Duration::from_secs(61), false);
    assert!(
        backoff.may_start(&worker, t0 + Duration::from_secs(62)),
        "a crash after a stable run starts the backoff afresh"
    );
}

#[test]
fn a_fresh_sign_in_retries_quickly_and_a_gone_session_is_forgotten() {
    let mut backoff = ResidentStartBackoff::default();
    let worker = key(1, 1, DesktopTarget::WindowsDefault);
    let t0 = Instant::now();
    for _ in 0..6 {
        backoff.failed(&worker, t0, true);
    }
    assert!(backoff.may_start(&worker, t0 + Duration::from_millis(500)));
    backoff.failed(&worker, t0, false);
    assert!(!backoff.may_start(&worker, t0 + Duration::from_secs(1)));
    backoff.forget_session(&session(1, 1));
    assert!(backoff.may_start(&worker, t0));
}

#[test]
fn foreground_sessions_win_when_sessions_exceed_the_cap() {
    let mut registrations: Vec<_> = (1..=(MAX_RESIDENT_SESSION_WORKERS as u32 + 3))
        .map(|id| {
            let mut registration = registration(id, 1, true);
            registration.foreground = false;
            registration
        })
        .collect();
    registrations.last_mut().unwrap().foreground = true;
    let capped = cap_windows_sessions(registrations);
    assert_eq!(capped.len(), MAX_RESIDENT_SESSION_WORKERS);
    assert!(capped[0].foreground);
}

#[test]
fn workers_receive_settings_without_the_daemon_secrets() {
    let mut settings = crate::model::settings::Settings::default();
    settings.user.login_password = "secret-password-value".into();
    settings.system.session_secret_key = Some("secret-session-value".into());
    settings.system.local_signaling_token = Some("secret-signaling-value".into());
    settings.system.manager_api_token = Some("secret-manager-value".into());
    settings.system.tauri_ipc_token = Some("ipc-secret-value".into());
    settings.turn.static_auth_secret = Some("secret-turn-value".into());
    for profile in [WorkerProfile::SessionUser, WorkerProfile::RestrictedDesktop] {
        let json = worker_settings_json(&settings, profile).unwrap();
        for secret in [
            "secret-password-value",
            "secret-session-value",
            "secret-signaling-value",
            "secret-manager-value",
            "secret-turn-value",
        ] {
            assert!(!json.contains(secret), "{profile:?} received {secret}");
        }
        assert_eq!(
            json.contains("ipc-secret-value"),
            profile == WorkerProfile::SessionUser,
            "only the session worker keeps the host IPC token"
        );
        let parsed: crate::model::settings::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.desk.video_fps, settings.desk.video_fps);
    }
}

#[tokio::test]
async fn a_restarted_worker_receives_the_ceilings_of_its_capped_connections() {
    use crate::daemon::pc_manager::Admission;
    use desk_signal_facade::model::security_settings::SecuritySettings;
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    let registry = PcRegistry::new();
    let (manager, _messages) = WorkerManager::new(settings, registry.clone());
    manager.enable_session_targeting_for_test();
    let target = session(4, 1);
    manager.bind_connection_target("capped", &target).unwrap();
    manager.bind_connection_target("owner", &target).unwrap();
    manager
        .bind_connection_target("elsewhere", &session(5, 1))
        .unwrap();
    registry
        .record_admission("capped", Admission::Capped(SecuritySettings::default()))
        .await;
    registry
        .record_admission("owner", Admission::OwnerFull)
        .await;
    registry
        .record_admission("elsewhere", Admission::Capped(SecuritySettings::default()))
        .await;
    let ceilings = manager.capped_connection_ceilings(&target).await;
    assert_eq!(ceilings.len(), 1);
    assert_eq!(ceilings[0].connection_id, "capped");
    assert!(ceilings[0].ceiling.is_some());
}
