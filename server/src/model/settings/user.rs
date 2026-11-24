use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserSettings {
    pub login_user_name: String,
    pub login_password_hash: Option<String>,
    pub auth_revision: u64,
    #[serde(skip)]
    pub(crate) recovery_binding: Option<uuid::Uuid>,
    pub password_reset_code: Option<String>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            login_user_name: "admin".into(),
            login_password_hash: None,
            auth_revision: 0,
            recovery_binding: None,
            password_reset_code: None,
        }
    }
}

impl fmt::Debug for UserSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserSettings")
            .field("login_user_name", &self.login_user_name)
            .field("initialized", &self.login_password_hash.is_some())
            .field("auth_revision", &self.auth_revision)
            .finish_non_exhaustive()
    }
}
