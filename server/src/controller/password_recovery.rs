use crate::{
    error::{DeskError, DeskErrorCode},
    service::{client_ip::ClientIpExtractor, rate_limit::AuthRateLimiter, recovery::Recovery},
};
use actix_web::{
    HttpRequest, HttpResponse,
    cookie::{Cookie, SameSite},
    post, web,
};
use desk_signal_facade::model::auth::EmptyResponseDto;
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

const COOKIE: &str = "desk_password_recovery";
const PATH: &str = "/api/auth/local-recovery";

#[derive(Serialize, ToSchema)]
pub struct RecoveryLocation {
    pub config_file: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ResetPassword {
    pub code: String,
    pub new_password: String,
}

fn guard(req: &HttpRequest) -> Result<(), DeskError> {
    let origin = super::host_readiness::validate_local_mutation(req)?;
    let local = req.app_config().local_addr();
    let scheme = if req.app_config().secure() {
        "https"
    } else {
        "http"
    };
    if origin.scheme() != scheme
        || origin.port_or_known_default() != Some(local.port())
        || req.headers().keys().any(|k| {
            k.as_str() == "forwarded"
                || k.as_str().starts_with("x-forwarded-")
                || k.as_str() == "x-real-ip"
                || k.as_str() == "via"
        })
    {
        return DeskError::custom_error(
            DeskErrorCode::PERMISSION_ERROR,
            "Direct local access is required",
        );
    }
    Ok(())
}

fn binding(req: &HttpRequest) -> Option<Uuid> {
    req.cookie(COOKIE).and_then(|c| c.value().parse().ok())
}

#[utoipa::path(tag="Auth", responses((status=200,body=RestResponse<RecoveryLocation>)))]
#[post("/api/auth/local-recovery/start")]
pub async fn start_local_recovery(
    req: HttpRequest,
    recovery: web::Data<Recovery>,
) -> Result<HttpResponse, DeskError> {
    guard(&req)?;
    let recovery = recovery.into_inner();
    let id = recovery.issue(binding(&req)).await?;
    let cookie = Cookie::build(COOKIE, id.to_string())
        .path(PATH)
        .http_only(true)
        .secure(req.app_config().secure())
        .same_site(SameSite::Strict)
        .finish();
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .cookie(cookie)
        .json(RestResponse::succeed_with_data(RecoveryLocation {
            config_file: recovery.config_hint().map(|p| p.display().to_string()),
        })))
}

#[utoipa::path(tag="Auth", request_body=ResetPassword,responses((status=200,body=RestResponse<EmptyResponseDto>)))]
#[post("/api/auth/local-recovery/finish")]
pub async fn finish_local_recovery(
    req: HttpRequest,
    input: crate::service::password::CredentialJson<ResetPassword>,
    recovery: web::Data<Recovery>,
    ip: web::Data<ClientIpExtractor>,
    limiter: web::Data<Arc<AuthRateLimiter>>,
) -> Result<HttpResponse, DeskError> {
    guard(&req)?;
    let id = binding(&req).ok_or_else(|| {
        DeskError::new_custom_error(
            DeskErrorCode::INVALID_OR_EXPIRED_TOKEN,
            "Missing recovery request",
        )
    })?;
    let input = input.0;
    recovery.consume(id, input.code, input.new_password).await?;
    limiter.clear_login(&ip.network_key(&req));
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed()))
}

#[utoipa::path(tag="Auth", responses((status=200,body=RestResponse<EmptyResponseDto>)))]
#[post("/api/auth/local-recovery/cancel")]
pub async fn cancel_local_recovery(
    req: HttpRequest,
    recovery: web::Data<Recovery>,
) -> Result<HttpResponse, DeskError> {
    guard(&req)?;
    if let Some(id) = binding(&req) {
        recovery.cancel(id).await?;
    }
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed()))
}
