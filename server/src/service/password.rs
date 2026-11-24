//! Password hashing and the browser session boundary of the standalone host.
use actix_session::{Session, SessionExt};
use actix_web::{
    Error,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    middleware::Next,
    web,
};
use argon2::password_hash::rand_core::OsRng;
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use tokio::sync::Semaphore;

use crate::{
    error::{DeskError, DeskErrorCode},
    model::settings::{SharedSettings, UserSettings},
};

static HASH_SLOTS: Semaphore = Semaphore::const_new(2);
static HASH_ADMISSION: Semaphore = Semaphore::const_new(16);
pub(super) const MAX_PASSWORD_BYTES: usize = 16 * 1024;
const REVISION: &str = "local_auth_revision";

pub(crate) fn hash_sync(password: &str) -> Result<String, DeskError> {
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return DeskError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "Password length is out of range",
        );
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| {
            DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "Password hashing failed")
        })
}

pub(crate) fn matches_sync(password: &str, encoded: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let Ok(hash) = PasswordHash::new(encoded) else {
        return false;
    };
    if hash.algorithm != Algorithm::Argon2id.ident()
        || hash.version != Some(Version::V0x13.into())
        || Params::try_from(&hash)
            .ok()
            .is_none_or(|p| p.m_cost() > 65536 || p.t_cost() > 6 || p.p_cost() > 4)
    {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

async fn compute<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DeskError> {
    let _admission = HASH_ADMISSION.try_acquire().map_err(|_| {
        DeskError::new_custom_error(DeskErrorCode::TOO_MANY_ATTEMPTS, "Password service is busy")
    })?;
    let permit = HASH_SLOTS.acquire().await.map_err(|_| {
        DeskError::new_custom_error(DeskErrorCode::TOO_MANY_ATTEMPTS, "Password service is busy")
    })?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await?)
}

pub(crate) async fn hash(password: String) -> Result<String, DeskError> {
    compute(move || hash_sync(&password)).await?
}

pub(crate) async fn verify(password: String, encoded: String) -> Result<bool, DeskError> {
    compute(move || matches_sync(&password, &encoded)).await
}

pub(crate) fn begin_session(session: &Session, revision: u64) -> Result<(), Error> {
    session.clear();
    session.renew();
    session.insert(REVISION, revision)?;
    Ok(())
}

pub(crate) async fn enforce_revision(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let session = req.get_session();
    let authenticated = session
        .entries()
        .contains_key(desk_server_user::service::SESSION_KEY_USERNAME)
        || session
            .entries()
            .contains_key(desk_signal_facade::model::code_session::CODE_SESSION_KEY);
    if authenticated {
        let revision = match req.app_data::<web::Data<SharedSettings>>() {
            Some(settings) => Some(settings.read().await.user.auth_revision),
            None => None,
        };
        if revision.is_none() || session.get::<u64>(REVISION)? != revision {
            session.clear();
        }
    }
    next.call(req).await
}

pub(crate) fn advance_revision(user: &mut UserSettings) -> Result<(), DeskError> {
    user.auth_revision = user.auth_revision.checked_add(1).ok_or_else(|| {
        DeskError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Authentication revision exhausted",
        )
    })?;
    user.recovery_binding = None;
    user.password_reset_code = None;
    Ok(())
}

/// Limit credential bodies before buffering or deserializing them. JSON errors
/// can include untrusted values, so only return a fixed error description.
pub struct CredentialJson<T>(pub T);

impl<T: serde::de::DeserializeOwned + 'static> actix_web::FromRequest for CredentialJson<T> {
    type Error = actix_web::Error;
    type Future = futures_util::future::LocalBoxFuture<'static, Result<Self, Self::Error>>;
    fn from_request(
        req: &actix_web::HttpRequest,
        payload: &mut actix_web::dev::Payload,
    ) -> Self::Future {
        let body = actix_web::dev::JsonBody::new(req, payload, None, true).limit(64 * 1024);
        Box::pin(async move {
            body.await.map(Self).map_err(|_| {
                let message = "Invalid credential request body";
                let response = actix_web::HttpResponse::BadRequest().json(
                    desk_utils::rest::RestResponse::failed(
                        DeskErrorCode::INVALID_PARAMS,
                        message.into(),
                    ),
                );
                actix_web::error::InternalError::from_response(message, response).into()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn salts_are_fresh_and_cost_is_bounded() {
        let a = hash_sync("a secret").unwrap();
        let b = hash_sync("a secret").unwrap();
        assert_ne!(
            PasswordHash::new(&a).unwrap().salt,
            PasswordHash::new(&b).unwrap().salt
        );
        assert!(matches_sync("a secret", &b));
        assert!(!matches_sync("another", &b));
        assert!(!matches_sync(
            "a secret",
            &b.replace("m=19456", "m=99999999")
        ));
        assert!(hash_sync("").is_err());
    }
}
