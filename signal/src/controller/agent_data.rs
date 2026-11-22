//! Mounted only under the authenticated single-account owner REST scope.

use crate::{
    agent_data_config::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::ai_agent::memory::{
    AgentDataControlReceipt, MemoryClearRequest, MemoryDeleteRequest, MemoryListRequest,
    MemoryListResult,
};
use desk_agent_protocol::ai_agent::policy::{PolicyAssessment, PolicyView, PolicyWriteData};
use desk_agent_protocol::ai_agent::source::{AgentSourceReadRequest, AgentSourceResolution};
use desk_agent_protocol::ai_agent::user_instructions::{
    AgentDataPolicyUpdate, AgentDataSettingsUpdate, AgentDataSettingsView, UserInstructions,
    UserInstructionsUpdate,
};
use desk_diagnose_core::ai_agent::policy as policy_rules;
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::Serialize;

pub const TAG: &str = "AgentData";
fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Agent data storage is unavailable",
        )
    })
}
fn reply<T: Serialize + utoipa::ToSchema>(value: T) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(value))
}
fn failure(error: WriteError) -> HttpResponse {
    let (code, revision) = match error {
        WriteError::Conflict { current_revision } => {
            (DeskErrorCode::REVISION_CONFLICT, Some(current_revision))
        }
        WriteError::Invalid => (DeskErrorCode::INVALID_PARAMS, None),
        WriteError::PolicyInvalid(assessment) => return policy_assessment(assessment),
        WriteError::Backend(_) => (DeskErrorCode::SYSTEM_ERROR, None),
    };
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<serde_json::Value>::failed_with_data(
            code,
            Some("Agent data could not be saved; reload before retrying".into()),
            revision
                .map(|current_revision| serde_json::json!({"current_revision": current_revision})),
        ))
}

fn store_failure(error: crate::agent_data_store::AgentStoreError) -> HttpResponse {
    use crate::agent_data_store::AgentStoreError;
    let (code, revision) = match error {
        AgentStoreError::Inaccessible | AgentStoreError::SourceUnavailable => {
            (DeskErrorCode::PERMISSION_ERROR, None)
        }
        AgentStoreError::Capacity => (DeskErrorCode::PRECONDITION_FAILED, None),
        AgentStoreError::Invalid(_) => (DeskErrorCode::INVALID_PARAMS, None),
        AgentStoreError::Stale { current_revision } => {
            (DeskErrorCode::REVISION_CONFLICT, Some(current_revision))
        }
        AgentStoreError::Backend(_) => (DeskErrorCode::SYSTEM_ERROR, None),
    };
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<serde_json::Value>::failed_with_data(
            code,
            Some("Agent data could not be accessed; reload before retrying".into()),
            revision
                .map(|current_revision| serde_json::json!({"current_revision": current_revision})),
        ))
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<AgentDataSettingsView>)))]
#[get("/my/ai-agent-settings")]
pub async fn get_agent_data_settings() -> Result<HttpResponse, DeskSignalError> {
    let db = db()?;
    Ok(reply(AgentDataSettingsView {
        settings: agent_data_config::settings(db).await,
        policy: agent_data_config::policy(db).await,
    }))
}
#[utoipa::path(tag = TAG, request_body = AgentDataSettingsUpdate, responses((status = 200, body = RestResponse<AgentDataSettingsView>)))]
#[post("/my/ai-agent-settings")]
pub async fn update_agent_data_settings(
    body: web::Json<AgentDataSettingsUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = db()?;
    Ok(match agent_data_config::update_settings(db, &body).await {
        Ok(settings) => reply(AgentDataSettingsView {
            settings,
            policy: agent_data_config::policy(db).await,
        }),
        Err(error) => failure(error),
    })
}
#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<UserInstructions>)))]
#[get("/my/ai-agent-user-instructions")]
pub async fn get_agent_user_instructions() -> Result<HttpResponse, DeskSignalError> {
    Ok(reply(agent_data_config::instructions(db()?).await))
}
#[utoipa::path(tag = TAG, request_body = UserInstructionsUpdate, responses((status = 200, body = RestResponse<UserInstructions>)))]
#[post("/my/ai-agent-user-instructions")]
pub async fn update_agent_user_instructions(
    body: web::Json<UserInstructionsUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match agent_data_config::update_instructions(db()?, &body).await {
            Ok(value) => reply(value),
            Err(error) => failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(
    ("limit" = Option<u32>, Query), ("scope" = Option<String>, Query), ("include_superseded" = Option<bool>, Query),
    ("cursor" = Option<String>, Query), ("memory_state_generation" = Option<u64>, Query), ("control_generation" = Option<u64>, Query)
), responses((status = 200, body = RestResponse<MemoryListResult>)))]
#[get("/my/ai-agent-memories")]
pub async fn get_agent_memories(
    query: web::Query<MemoryListRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::memories(
            db()?,
            &query,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = MemoryDeleteRequest, responses((status = 200, body = RestResponse<AgentDataControlReceipt>)))]
#[post("/my/ai-agent-memory/delete")]
pub async fn delete_agent_memory(
    body: web::Json<MemoryDeleteRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::forget_memory(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = MemoryClearRequest, responses((status = 200, body = RestResponse<AgentDataControlReceipt>)))]
#[post("/my/ai-agent-memory/clear")]
pub async fn clear_agent_memories(
    body: web::Json<MemoryClearRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::clear_memories(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(("source_id" = String, Query)), responses((status = 200, body = RestResponse<AgentSourceResolution>)))]
#[get("/my/ai-agent-source")]
pub async fn get_agent_source(
    query: web::Query<AgentSourceReadRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::owner_source(
            db()?,
            &query.source_id,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<PolicyView>)))]
#[get("/admin/system/ai-agent-data")]
pub async fn get_agent_data_policy() -> Result<HttpResponse, DeskSignalError> {
    let policy = agent_data_config::policy(db()?).await;
    let issues = policy_rules::warnings(&policy);
    Ok(reply(PolicyView { policy, issues }))
}
#[utoipa::path(tag = TAG, request_body = AgentDataPolicyUpdate, responses((status = 200, body = RestResponse<PolicyWriteData>)))]
#[post("/admin/system/ai-agent-data")]
pub async fn update_agent_data_policy(
    body: web::Json<AgentDataPolicyUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match agent_data_config::update_policy(db()?, &body).await {
        Ok(value) => reply(value),
        Err(error) => failure(error),
    })
}

#[utoipa::path(tag = TAG, params(
    ("source_id" = String, Query), ("revision" = u64, Query),
    ("view" = Option<String>, Query), ("cursor" = Option<String>, Query)
), responses((status = 200, body = RestResponse<desk_agent_protocol::ai_agent::history_window::AgentHistoryWindow>)))]
#[get("/my/ai-agent-history/window")]
pub async fn get_agent_history_window(
    query: web::Query<desk_agent_protocol::ai_agent::history_window::AgentHistoryWindowRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::owner_history_window(
            db()?,
            &query,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, request_body = desk_agent_protocol::ai_agent::owner_history::AgentHistorySearchRequest,
    responses((status = 200, body = RestResponse<desk_agent_protocol::ai_agent::owner_history::AgentHistorySearchResult>)))]
#[post("/my/ai-agent-history/search")]
pub async fn search_agent_history(
    body: web::Json<desk_agent_protocol::ai_agent::owner_history::AgentHistorySearchRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::owner_history_search(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, params(("limit" = Option<u32>, Query), ("cursor" = Option<String>, Query)),
    responses((status = 200, body = RestResponse<desk_agent_protocol::ai_agent::owner_history::AgentHistoryConversations>)))]
#[get("/my/ai-agent-history/conversations")]
pub async fn list_retained_agent_conversations(
    body: web::Query<
        desk_agent_protocol::ai_agent::owner_history::AgentHistoryConversationsRequest,
    >,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match crate::agent_data_store::owner_history_conversations(
            db()?,
            &body,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        {
            Ok(value) => reply(value),
            Err(error) => store_failure(error),
        },
    )
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<desk_agent_protocol::ai_agent::processing::AgentProcessingStatus>)))]
#[get("/my/ai-agent-processing")]
pub async fn get_agent_processing() -> Result<HttpResponse, DeskSignalError> {
    Ok(match crate::agent_data_store::processing(db()?).await {
        Ok(value) => reply(value),
        Err(error) => store_failure(error),
    })
}

fn policy_assessment(assessment: PolicyAssessment) -> HttpResponse {
    if assessment.has_errors() {
        HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::failed_with_data(
                DeskErrorCode::INVALID_PARAMS,
                Some("Agent policy contains invalid values".into()),
                Some(assessment),
            ))
    } else {
        reply(assessment)
    }
}

#[cfg(test)]
mod tests;
