//! Windows command sandbox maintenance for the local OS user (plan 6.5.3,
//! part six). Deliberately unavailable through manager/remote proxies.
use super::host_readiness::validate_local_mutation;
use crate::error::DeskError;
use actix_web::{HttpRequest, HttpResponse, http::header, post};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Result of revoking the current user's sandbox labels.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct ExecSandboxLabelsRevoked {
    /// Labels removed now.
    pub revoked: u32,
    /// Labels a running command still uses; they are revoked automatically
    /// once their lease runs out.
    pub in_use: u32,
}

fn unavailable(message: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, message)
}

#[cfg(windows)]
async fn revoke(req: &HttpRequest) -> Result<ExecSandboxLabelsRevoked, DeskError> {
    use actix_web::web;
    let local_user = crate::windows_local_user::authenticate(req).map_err(|_| {
        DeskError::new_custom_error(
            DeskErrorCode::PERMISSION_ERROR,
            "Revoking sandbox labels requires the local OS user",
        )
    })?;
    // A host running as the requesting user (Default, DeskServer) revokes in
    // process. The ServiceDaemon runs as another account, so it hands the
    // request to the resident worker running as that user.
    if let Some(manager) = req
        .app_data::<web::Data<crate::model::settings_coordinator::SettingsCoordinator>>()
        .and_then(|coordinator| coordinator.worker_manager())
        .filter(|manager| manager.uses_session_targeting())
    {
        use crate::daemon::worker_manager::RevokeForwardError as E;
        let reply = manager
            .request_exec_sandbox_revoke(&local_user)
            .await
            .map_err(|error| {
                unavailable(match error {
                    E::WorkerUnavailable => "No session worker runs for this user",
                    E::Busy => "Another revocation is in progress",
                    E::TimedOut => "The session worker did not answer in time",
                    E::IdentityChanged => "The requesting user changed",
                })
            })?;
        if let Some(error) = reply.error {
            return Err(unavailable(&error));
        }
        return Ok(ExecSandboxLabelsRevoked {
            revoked: reply.revoked,
            in_use: reply.in_use,
        });
    }
    let (revoked, in_use) = tokio::task::spawn_blocking(move || {
        if !local_user.is_alive()
            || crate::file_recovery_service::platform_user::current().ok()
                != Some(local_user.sid.clone())
        {
            return Err("Revoking sandbox labels requires the same OS user");
        }
        Ok(crate::worker::exec_sandbox::revoke_all_labels())
    })
    .await
    .map_err(|_| unavailable("The revocation task failed"))?
    .map_err(|message| DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, message))?;
    Ok(ExecSandboxLabelsRevoked {
        revoked: u32::try_from(revoked).unwrap_or(u32::MAX),
        in_use: u32::try_from(in_use).unwrap_or(u32::MAX),
    })
}

#[cfg(not(windows))]
async fn revoke(_req: &HttpRequest) -> Result<ExecSandboxLabelsRevoked, DeskError> {
    Err(unavailable("Sandbox labels exist only on Windows"))
}

#[utoipa::path(
    tag = "AiModel",
    summary = "Revoke the current OS user's Windows command sandbox labels",
    responses((status = 200, body = RestResponse<ExecSandboxLabelsRevoked>)),
)]
#[post("/settings/exec-sandbox/revoke-labels")]
pub async fn revoke_exec_sandbox_labels(req: HttpRequest) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let result = revoke(&req).await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(result)))
}
