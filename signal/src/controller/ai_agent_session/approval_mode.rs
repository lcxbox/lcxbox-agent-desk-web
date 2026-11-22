//! Owner approval mode for a root conversation.

use super::*;
use crate::agent_approval_store::{load_preference, save_preference};
use desk_agent_protocol::ai_agent::approval_mode::{
    AgentApprovalModeView, UpdateAgentApprovalMode,
};
use desk_diagnose_core::agent_approval_mode::AgentApprovalPreference;
use sea_orm::DbErr;

#[derive(serde::Deserialize)]
pub struct ApprovalModeQuery {
    session: String,
}

#[utoipa::path(tag = TAG, params(("session" = String, Query)), responses((status = 200, body = RestResponse<AgentApprovalModeView>)))]
#[get("/my/ai-agent-session/approval-mode")]
pub async fn get_ai_agent_approval_mode(
    session: Session,
    query: web::Query<ApprovalModeQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    let result = load_preference(
        crate::db::get_db(),
        &SINGLE_ACCOUNT_USER_ID.to_string(),
        &query.session,
    )
    .await
    .map(Some);
    response(result).await
}

#[utoipa::path(tag = TAG, request_body = UpdateAgentApprovalMode, responses((status = 200, body = RestResponse<AgentApprovalModeView>)))]
#[post("/my/ai-agent-session/approval-mode")]
pub async fn update_ai_agent_approval_mode(
    session: Session,
    body: web::Json<UpdateAgentApprovalMode>,
) -> Result<HttpResponse, DeskSignalError> {
    if !super::device_roster::owner(&session)? {
        return Ok(not_accessible());
    }
    let result = save_preference(
        crate::db::get_db(),
        &SINGLE_ACCOUNT_USER_ID.to_string(),
        body.into_inner(),
    )
    .await;
    response(result).await
}

async fn response(
    result: Result<Option<AgentApprovalPreference>, DbErr>,
) -> Result<HttpResponse, DeskSignalError> {
    let preference = match result {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::PRECONDITION_FAILED,
                "Invalid or stale approval mode; reload the current setting".into(),
            )));
        }
        Err(DbErr::RecordNotFound(_)) => return Ok(not_accessible()),
        Err(error) => return Err(error.into()),
    };
    let configured = crate::approval_model_provider::load(crate::db::get_db()).await?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(
            preference.view(configured.destination_identity().is_ok()),
        )))
}
