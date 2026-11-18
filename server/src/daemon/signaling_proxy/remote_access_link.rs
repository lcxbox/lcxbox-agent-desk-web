//! Manager-link gating and remote-access central-source selection.

use super::*;

/// Resolve when the shared manager-link gate flips to disabled. For links the
/// manager toggle does not govern (`None` receiver) this never resolves, so the
/// `select!` branch that awaits it stays inert.
pub(super) async fn wait_manager_link_disabled(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => {
            // `wait_for` re-checks the current value, so a disable that already
            // happened is observed rather than missed.
            let _ = rx.wait_for(|enabled| !*enabled).await;
        }
        None => std::future::pending::<()>().await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteAccessCentralLink {
    None,
    Local,
    RemoteSignal,
    Manager,
}

pub(super) async fn remote_access_link_is_primary(
    settings: &web::Data<SharedSettings>,
    candidate: RemoteAccessCentralLink,
) -> bool {
    let settings = settings.read().await;
    let manager = manager_link_should_connect(
        &settings.system.manager_url,
        &settings.system.manager_api_token,
        settings.system.manager_enabled,
    );
    let remote_signal = settings
        .system
        .signaling_url
        .as_ref()
        .is_some_and(|value| !value.is_empty())
        && settings
            .system
            .signaling_token
            .as_ref()
            .is_some_and(|value| !value.is_empty());
    let primary =
        select_remote_access_central_link(manager, remote_signal, &settings.args.startup_mode);
    candidate == primary
}

pub(super) fn select_remote_access_central_link(
    manager: bool,
    remote_signal: bool,
    startup_mode: &StartupMode,
) -> RemoteAccessCentralLink {
    if manager {
        RemoteAccessCentralLink::Manager
    } else if remote_signal {
        RemoteAccessCentralLink::RemoteSignal
    } else if matches!(
        startup_mode,
        StartupMode::Default | StartupMode::ServiceDaemon
    ) {
        // Both modes run an embedded signal endpoint and maintain a loopback
        // Server connection to it. ServiceDaemon must elect that connection as
        // the durable mirror when no external central is configured; otherwise
        // a successful local LockAll remains pending forever.
        RemoteAccessCentralLink::Local
    } else {
        RemoteAccessCentralLink::None
    }
}

pub(super) async fn receive_remote_access_command(
    rx: &mut Option<broadcast::Receiver<String>>,
) -> Option<String> {
    let Some(rx) = rx else {
        return std::future::pending::<Option<String>>().await;
    };
    loop {
        match rx.recv().await {
            Ok(command) => return Some(command),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("[remote-access] skipped {skipped} stale peer eviction commands");
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}
