//! Standalone-signal host for platform and personal skills. The single local
//! account is also the platform account, so it manages both scopes.

use crate::config::ConfigConnection;

/// Bundled skills disabled in `config.toml`.
pub async fn disabled_bundled<C: ConfigConnection>(db: &C) -> Vec<String> {
    db.config_read()
        .await
        .assistant_bundled_skills
        .disabled
        .clone()
}
