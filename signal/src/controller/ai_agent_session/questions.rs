//! Owner responses target the exact wait in the original conversation.
use super::*;
use desk_agent_protocol::ai_agent::owner_question::RespondToOwnerQuestion;

#[utoipa::path(tag = TAG, summary = "Interact with, answer or close the open question in this conversation",
    request_body = RespondToOwnerQuestion,
    responses((status=200, body=RestResponse<String>)))]
#[post("/my/ai-agent-session/questions/respond")]
pub async fn respond_to_owner_question(
    body: web::Json<RespondToOwnerQuestion>,
) -> Result<HttpResponse, DeskSignalError> {
    if [&body.session, &body.wait_id, &body.client_request_id]
        .into_iter()
        .any(|id| desk_agent_data_store::schedule::key(id).is_err())
    {
        return Ok(HttpResponse::Ok().json(RestResponse::failed(
            DeskErrorCode::INVALID_PARAMS,
            "Invalid question identifier".to_owned(),
        )));
    }
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db();
    let outcome = crate::decision_resume::settle_question(
        db,
        &actor,
        &body.session,
        &body.wait_id,
        Some((&body.client_request_id, &body.action)),
    )
    .await;
    Ok(HttpResponse::Ok().json(match outcome {
        Ok(status) => RestResponse::succeed_with_data(status),
        Err(_) => RestResponse::failed_with_data(
            DeskErrorCode::PRECONDITION_FAILED,
            Some("This question changed. Refresh the conversation.".to_owned()),
            None,
        ),
    }))
}
