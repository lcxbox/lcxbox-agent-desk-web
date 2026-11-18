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
