use super::*;
use crate::model::settings::Settings;
use tempfile::{TempDir, tempdir};

async fn host(display: Option<DisplayResetCode>) -> (TempDir, Arc<Recovery>) {
    let dir = tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut settings = Settings::for_test_config(&dir.path().join("config.toml"));
    // Corruption disables password login, but the configured account can recover.
    settings.user.login_password_hash = Some("damaged PHC".into());
    settings.save().unwrap();
    let settings = Arc::new(SharedSettings::from(settings));
    let coordinator = Arc::new(SettingsCoordinator::from_settings(settings.clone()).await);
    (dir, Recovery::new(settings, coordinator, display).await)
}

#[tokio::test]
async fn native_display_requires_a_live_request_and_never_persists_its_code() {
    let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
    let (_dir, host) = host(Some(Arc::new(move |code| {
        send.send(code).unwrap();
        Ok(())
    })))
    .await;
    let id = host.issue(None).await.unwrap();
    let code = recv.recv().await.unwrap();
    assert!(host.config_hint().is_none());
    assert!(
        !std::fs::read_to_string(&host.path)
            .unwrap()
            .contains("password_reset_code")
    );
    assert_eq!(host.issue(Some(id)).await.unwrap(), id);
    assert!(host.issue(None).await.is_err());
    assert!(
        host.consume(Uuid::new_v4(), code.clone(), "new".into())
            .await
            .is_err()
    );
    for _ in 0..5 {
        assert!(
            host.consume(id, "wrong".into(), "new".into())
                .await
                .is_err()
        );
    }
    assert!(host.consume(id, code, "new".into()).await.is_err());
    assert_eq!(host.account().await.auth_revision, 0);
}

#[tokio::test]
async fn concurrent_attempts_consume_once_and_expired_requests_cannot_commit() {
    let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
    let (_dir, host) = host(Some(Arc::new(move |code| {
        send.send(code).unwrap();
        Ok(())
    })))
    .await;
    let id = host.issue(None).await.unwrap();
    let code = recv.recv().await.unwrap();
    let (a, b) = tokio::join!(
        host.consume(id, code.clone(), "new".into()),
        host.consume(id, code, "new".into())
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(host.account().await.auth_revision, 1);
    host.state.lock().await.last_issue = None;
    let next = host.issue(None).await.unwrap();
    let new_code = recv.recv().await.unwrap();
    host.clear_code(id).await.unwrap();
    assert_eq!(host.account().await.recovery_binding, Some(next));
    host.state.lock().await.request.as_mut().unwrap().deadline = Instant::now();
    assert!(
        host.consume(next, new_code, "expired".into())
            .await
            .is_err()
    );
    host.cancel(next).await.unwrap();
    assert_eq!(host.account().await.recovery_binding, None);
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_checks_file_permissions_and_hard_links() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, host) = host(None).await;
    let link = dir.path().join("other-name");
    std::fs::hard_link(&host.path, &link).unwrap();
    assert!(check_config_delivery(&host.path).is_err());
    std::fs::remove_file(link).unwrap();
    std::fs::set_permissions(&host.path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(host.issue(None).await.is_err());
    assert!(
        !std::fs::read_to_string(&host.path)
            .unwrap()
            .contains("password_reset_code")
    );
}
