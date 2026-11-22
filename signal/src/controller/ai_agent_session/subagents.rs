//! Recoverable owner controls; opaque child IDs never provide authority.
use super::*;

#[utoipa::path(tag = TAG, summary = "List a bounded page of this conversation's subagents",
    params(
        ("connection" = String, Query, description = "Original target connection"),
        ("conversation" = Option<String>, Query, description = "Client conversation intent"),
        ("session" = Option<String>, Query, description = "Opaque original main session"),
        ("cursor" = Option<String>, Query, description = "Exclusive opaque page cursor"),
        ("limit" = Option<u32>, Query, description = "Page size, 1 through 20"),
    ), responses((status = 200, body = RestResponse<AiAgentSubAgentPage>)))]
#[get("/my/ai-agent-session/subagents")]
pub async fn list_ai_agent_subagents(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<AiAgentSubAgentsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((root, device_id)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor_id,
        &query.connection,
        query.session.as_deref(),
        query.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let page = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .list_for_owner(
            &root,
            &actor_id,
            &device_id,
            query.cursor.as_deref(),
            query.limit.unwrap_or(10),
        )
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Subagent list is unavailable; refresh and retry",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(page)))
}

#[utoipa::path(tag = TAG, summary = "Read the authoritative status of one subagent",
    params(
        ("connection" = String, Query), ("conversation" = Option<String>, Query),
        ("session" = Option<String>, Query), ("task_id" = String, Query),
    ), responses((status = 200, body = RestResponse<AiAgentSubAgentSummary>)))]
#[get("/my/ai-agent-session/subagents/status")]
pub async fn get_ai_agent_subagent_status(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<AiAgentSubAgentQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((root, device_id)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor_id,
        &query.connection,
        query.session.as_deref(),
        query.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let task = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .status_for_owner(&root, &actor_id, &device_id, &query.task_id)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Subagent not found or not accessible",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(task)))
}

#[utoipa::path(tag = TAG, summary = "Read the Worker request, recent reports and any open question",
    params(
        ("connection" = String, Query), ("conversation" = Option<String>, Query),
        ("session" = Option<String>, Query), ("task_id" = String, Query),
        ("before_report_seq" = Option<u64>, Query),
    ), responses((status = 200, body = RestResponse<AiAgentSubAgentResult>)))]
#[get("/my/ai-agent-session/subagents/result")]
pub async fn read_ai_agent_subagent_result(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<AiAgentSubAgentQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((root, device_id)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor_id,
        &query.connection,
        query.session.as_deref(),
        query.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let result = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .result_for_owner(
            &root,
            &actor_id,
            &device_id,
            &query.task_id,
            query.before_report_seq,
        )
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Subagent not found or not accessible",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}

#[utoipa::path(tag = TAG, summary = "Cancel or adjust a Worker request or answer its open question",
    request_body = AiAgentSubAgentControlBody,
    responses((status = 200, body = RestResponse<AiAgentSubAgentSummary>)))]
#[post("/my/ai-agent-session/subagents/control")]
pub async fn control_ai_agent_subagent(
    connection_map: web::Data<SharedConnectionMap>,

    body: web::Json<AiAgentSubAgentControlBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((root, device_id)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor_id,
        &body.connection,
        body.session.as_deref(),
        body.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let outcome = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .control_for_owner(&root, &actor_id, &device_id, &body.control)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Subagent control changed or its source expired; refresh and retry",
            )
        })?;
    if let Some(request) = &outcome.cancel_request_id {
        crate::ai_agent_orchestrator::cancellation::cancel(SINGLE_ACCOUNT_USER_ID, request);
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(outcome.task)))
}

#[utoipa::path(tag = TAG, summary = "Mark displayed subagent notifications as read in the owner UI",
    request_body = AiAgentSubAgentReadBody, responses((status = 200, body = RestResponse<bool>)))]
#[post("/my/ai-agent-session/subagents/read")]
pub async fn mark_ai_agent_subagent_read(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<AiAgentSubAgentReadBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor = SINGLE_ACCOUNT_USER_ID.to_string();
    let db = crate::db::get_db();
    let Some((root, device)) = recovery::resolve(
        &SignalAgentSessionStore::new(db.clone()),
        &connection_map,
        &actor,
        &body.connection,
        body.session.as_deref(),
        body.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let changed = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .mark_ui_read_for_owner(
            &root,
            &actor,
            &device,
            &body.task_id,
            body.through_state_revision,
        )
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Subagent notification not found or not accessible",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(changed)))
}
