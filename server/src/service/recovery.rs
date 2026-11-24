//! Local password recovery. Only the live request authorizes consuming a code.
use crate::{
    error::{DeskError, DeskErrorCode},
    model::{
        settings::{SharedSettings, UserSettings},
        settings_coordinator::SettingsCoordinator,
    },
    service::password,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

pub mod native;
mod permissions;

pub type DisplayResetCode = Arc<dyn Fn(String) -> std::io::Result<()> + Send + Sync>;
const TTL: Duration = Duration::from_secs(300);
const ISSUE_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) fn check_config_delivery(path: &Path) -> std::io::Result<()> {
    if path.is_symlink() {
        return Err(std::io::Error::other(
            "A private regular configuration file is required",
        ));
    }
    let path = path.canonicalize()?;
    let static_root = std::env::current_exe()?.with_file_name("static");
    if static_root.exists() && path.starts_with(static_root.canonicalize()?) {
        return Err(std::io::Error::other(
            "Move the configuration outside the public static directory",
        ));
    }
    permissions::check(&path)
}

fn different_account(live: &UserSettings, expected: &UserSettings) -> bool {
    live.auth_revision != expected.auth_revision
        || live.login_password_hash != expected.login_password_hash
        || live.login_user_name != expected.login_user_name
}
fn stale() -> DeskError {
    DeskError::new_custom_error(
        DeskErrorCode::INVALID_OR_EXPIRED_TOKEN,
        "Invalid or expired recovery request",
    )
}

struct Request {
    binding: Uuid,
    code: String,
    account: UserSettings,
    deadline: Instant,
    attempts: u8,
}
#[derive(Default)]
struct State {
    request: Option<Request>,
    last_issue: Option<Instant>,
}

pub(crate) struct Recovery {
    settings: Arc<SharedSettings>,
    coordinator: Arc<SettingsCoordinator>,
    state: Mutex<State>,
    path: PathBuf,
    display: Option<DisplayResetCode>,
}

impl Recovery {
    pub(crate) async fn new(
        settings: Arc<SharedSettings>,
        coordinator: Arc<SettingsCoordinator>,
        display: Option<DisplayResetCode>,
    ) -> Arc<Self> {
        let path = settings.read().await.paths().config_file().to_owned();
        Arc::new(Self {
            settings,
            coordinator,
            state: Mutex::default(),
            path,
            display,
        })
    }

    pub(crate) fn config_hint(&self) -> Option<&Path> {
        self.display.is_none().then_some(&self.path)
    }

    pub(crate) async fn issue(self: &Arc<Self>, previous: Option<Uuid>) -> Result<Uuid, DeskError> {
        let host = self.clone();
        // Finish publication and schedule expiry even if the HTTP caller disconnects.
        tokio::spawn(async move { host.issue_locked(previous).await }).await?
    }

    async fn issue_locked(self: &Arc<Self>, previous: Option<Uuid>) -> Result<Uuid, DeskError> {
        let mut state = self.state.lock().await;
        let account = self.settings.read().await.user.clone();
        if account.login_password_hash.is_none() {
            return DeskError::custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Initialize the account first",
            );
        }
        let now = Instant::now();
        if let Some(active) = &state.request
            && active.deadline > now
            && active.attempts > 0
            && !different_account(&account, &active.account)
        {
            if Some(active.binding) == previous {
                return Ok(active.binding);
            }
            return DeskError::custom_error(
                DeskErrorCode::TOO_MANY_ATTEMPTS,
                "Another recovery request is active",
            );
        }
        if state
            .last_issue
            .is_some_and(|t| t.elapsed() < ISSUE_INTERVAL)
        {
            return DeskError::custom_error(
                DeskErrorCode::TOO_MANY_ATTEMPTS,
                "Wait before requesting another code",
            );
        }
        if let Some(old) = &state.request {
            self.clear_code(old.binding).await?;
            state.request = None;
        }
        state.last_issue = Some(now);
        let binding = Uuid::new_v4();
        let code = format!("{:08}", rand::random_range(0..100_000_000u32));
        let expected = account.clone();
        let stored = self.display.is_none().then(|| code.clone());
        state.request = Some(Request {
            binding,
            code: code.clone(),
            account,
            deadline: now + TTL,
            attempts: 5,
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(TTL).await;
            if let Some(service) = weak.upgrade() {
                while service.cancel(binding).await.is_err() {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
        self.coordinator
            .commit_account(move |user| {
                if different_account(user, &expected) {
                    return Err(stale());
                }
                user.recovery_binding = Some(binding);
                user.password_reset_code = stored;
                Ok(())
            })
            .await
            .inspect_err(|_| {
                if let Some(r) = &mut state.request {
                    r.attempts = 0;
                }
            })?;
        if let Some(show) = &self.display
            && let Err(error) = show(code)
        {
            state.request = None;
            return Err(error.into());
        }
        Ok(binding)
    }

    pub(crate) async fn cancel(&self, binding: Uuid) -> Result<(), DeskError> {
        let mut state = self.state.lock().await;
        if let Some(r) = &mut state.request
            && r.binding == binding
        {
            r.attempts = 0;
            self.clear_code(binding).await?;
            state.request = None;
        }
        Ok(())
    }

    async fn clear_code(&self, binding: Uuid) -> Result<(), DeskError> {
        if self.settings.read().await.user.recovery_binding == Some(binding) {
            self.coordinator
                .commit_account(move |user| {
                    if user.recovery_binding == Some(binding) {
                        user.password_reset_code = None;
                        user.recovery_binding = None;
                    }
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn consume(
        &self,
        binding: Uuid,
        code: String,
        new_password: String,
    ) -> Result<(), DeskError> {
        let mut state = self.state.lock().await;
        let request = state
            .request
            .as_mut()
            .filter(|r| r.binding == binding && r.attempts != 0 && Instant::now() < r.deadline)
            .ok_or_else(stale)?;
        if !crate::constant_time_eq(code.as_bytes(), request.code.as_bytes()) {
            request.attempts -= 1;
            if request.attempts == 0 {
                self.clear_code(binding).await?;
                state.request = None;
            }
            return Err(stale());
        }
        let expected = request.account.clone();
        let deadline = request.deadline;
        drop(state);
        let encoded = password::hash(new_password).await?;
        let mut state = self.state.lock().await;
        if !state
            .request
            .as_ref()
            .is_some_and(|r| r.binding == binding && r.attempts > 0)
        {
            return Err(stale());
        }
        self.apply(expected, encoded, Some((binding, deadline)))
            .await?;
        state.request = None;
        Ok(())
    }

    pub(super) async fn account(&self) -> UserSettings {
        self.settings.read().await.user.clone()
    }

    pub(super) async fn reset_local(
        &self,
        account: UserSettings,
        new_password: String,
    ) -> Result<(), DeskError> {
        let encoded = password::hash(new_password).await?;
        let mut state = self.state.lock().await;
        self.apply(account, encoded, None).await?;
        state.request = None;
        Ok(())
    }

    async fn apply(
        &self,
        account: UserSettings,
        encoded: String,
        request: Option<(Uuid, Instant)>,
    ) -> Result<(), DeskError> {
        self.coordinator
            .commit_account(move |user| {
                if account.login_password_hash.is_none()
                    || different_account(user, &account)
                    || request.is_some_and(|(id, t)| {
                        user.recovery_binding != Some(id) || Instant::now() >= t
                    })
                {
                    return Err(stale());
                }
                password::advance_revision(user)?;
                user.login_password_hash = Some(encoded);
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests;
