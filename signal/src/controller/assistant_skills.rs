//! Platform and personal skill APIs, mounted inside the OSS owner-authenticated
//! scope. The single local account manages both scopes.

use crate::{assistant_skills, config::ConfigConnection, error::DeskSignalError};
use actix_web::{HttpResponse, delete, get, post, put, web};
use desk_agent_protocol::ai_assistant::skill::{
    BundledSkillList, BundledSkillToggle, SkillCreate, SkillDeleteQuery, SkillDetail, SkillPreview,
    SkillPreviewRequest, SkillSaved, SkillSummary, SkillUpdate,
};
use desk_assistant_data_store::skill_store::{
    self, PLATFORM_SCOPE,
    service::{self, SkillErrorKind, SkillServiceError},
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::Serialize;

const TAG: &str = "AssistantSkills";
type Db = crate::config::connection::DatabaseConnection;

fn db() -> Result<&'static Db, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Skill storage is unavailable",
        )
    })
}

fn reply<T: Serialize + utoipa::ToSchema>(value: T) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(value))
}

fn failed(code: DeskErrorCode, message: String) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<()>::failed(code, message))
}

fn failure(error: SkillServiceError) -> HttpResponse {
    let code = match error.kind() {
        SkillErrorKind::Conflict => DeskErrorCode::REVISION_CONFLICT,
        SkillErrorKind::Backend => DeskErrorCode::SYSTEM_ERROR,
        SkillErrorKind::Invalid | SkillErrorKind::NotFound | SkillErrorKind::Limit => {
            DeskErrorCode::INVALID_PARAMS
        }
    };
    failed(code, error.message())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn list(scope: &str) -> Result<HttpResponse, DeskSignalError> {
    Ok(match skill_store::list_on(db()?, scope).await {
        Ok(skills) => reply(skills),
        Err(error) => failure(error.into()),
    })
}

async fn detail(scope: &str, skill_id: &str) -> Result<HttpResponse, DeskSignalError> {
    Ok(match skill_store::detail_on(db()?, scope, skill_id).await {
        Ok(detail) => reply(detail),
        Err(error) => failure(error.into()),
    })
}

async fn preview(
    scope: &str,
    skill_id: Option<&str>,
    request: &SkillPreviewRequest,
) -> Result<HttpResponse, DeskSignalError> {
    let db = db()?;
    let (parsed, mut preview) =
        match service::preview_on(db, scope, skill_id, &request.package).await {
            Ok(value) => value,
            Err(error) => return Ok(failure(error)),
        };
    if request.ai_scan
        && let Err(error) =
            crate::assistant_skill_scan::attach(db, scope, &parsed, &mut preview, now_ms()).await
    {
        return Ok(failure(error.into()));
    }
    Ok(reply(preview))
}

async fn create(scope: &str, request: &SkillCreate) -> Result<HttpResponse, DeskSignalError> {
    let txn = crate::db::begin_write(db()?, crate::entity::agent_skill::Entity).await?;
    Ok(
        match service::create_from_input_on(&txn, scope, request, now_ms()).await {
            Ok(saved) => {
                txn.commit().await?;
                reply(saved)
            }
            Err(error) => failure(error),
        },
    )
}

async fn update(
    scope: &str,
    skill_id: &str,
    request: &SkillUpdate,
) -> Result<HttpResponse, DeskSignalError> {
    let txn = crate::db::begin_write(db()?, crate::entity::agent_skill::Entity).await?;
    Ok(
        match service::update_from_input_on(&txn, scope, skill_id, request, now_ms()).await {
            Ok(saved) => {
                txn.commit().await?;
                reply(saved)
            }
            Err(error) => failure(error),
        },
    )
}

async fn remove(
    scope: &str,
    skill_id: String,
    expected_revision: u64,
) -> Result<HttpResponse, DeskSignalError> {
    let txn = crate::db::begin_write(db()?, crate::entity::agent_skill::Entity).await?;
    Ok(
        match skill_store::delete_on(&txn, scope, &skill_id, expected_revision).await {
            Ok(()) => {
                txn.commit().await?;
                reply(skill_id)
            }
            Err(error) => failure(error.into()),
        },
    )
}

// Platform skills.

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<Vec<SkillSummary>>)))]
#[get("/admin/system/assistant-skills")]
pub async fn list_platform_skills() -> Result<HttpResponse, DeskSignalError> {
    list(PLATFORM_SCOPE).await
}

#[utoipa::path(tag = TAG, request_body = SkillPreviewRequest,
    responses((status = 200, body = RestResponse<SkillPreview>)))]
#[post("/admin/system/assistant-skills/preview")]
pub async fn preview_platform_skill(
    body: web::Json<SkillPreviewRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    preview(PLATFORM_SCOPE, None, &body).await
}

#[utoipa::path(tag = TAG, request_body = SkillCreate,
    responses((status = 200, body = RestResponse<SkillSaved>)))]
#[post("/admin/system/assistant-skills")]
pub async fn create_platform_skill(
    body: web::Json<SkillCreate>,
) -> Result<HttpResponse, DeskSignalError> {
    create(PLATFORM_SCOPE, &body).await
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<SkillDetail>)))]
#[get("/admin/system/assistant-skills/{skill_id}")]
pub async fn get_platform_skill(path: web::Path<String>) -> Result<HttpResponse, DeskSignalError> {
    detail(PLATFORM_SCOPE, &path).await
}

#[utoipa::path(tag = TAG, request_body = SkillPreviewRequest,
    responses((status = 200, body = RestResponse<SkillPreview>)))]
#[post("/admin/system/assistant-skills/{skill_id}/preview")]
pub async fn preview_platform_skill_version(
    path: web::Path<String>,
    body: web::Json<SkillPreviewRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    preview(PLATFORM_SCOPE, Some(&path), &body).await
}

#[utoipa::path(tag = TAG, request_body = SkillUpdate,
    responses((status = 200, body = RestResponse<SkillSaved>)))]
#[put("/admin/system/assistant-skills/{skill_id}")]
pub async fn update_platform_skill(
    path: web::Path<String>,
    body: web::Json<SkillUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    update(PLATFORM_SCOPE, &path, &body).await
}

#[utoipa::path(tag = TAG, params(SkillDeleteQuery),
    responses((status = 200, body = RestResponse<String>)))]
#[delete("/admin/system/assistant-skills/{skill_id}")]
pub async fn delete_platform_skill(
    path: web::Path<String>,
    query: web::Query<SkillDeleteQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    remove(PLATFORM_SCOPE, path.into_inner(), query.expected_revision).await
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<BundledSkillList>)))]
#[get("/admin/system/assistant-bundled-skills")]
pub async fn list_bundled_skills() -> Result<HttpResponse, DeskSignalError> {
    let settings = db()?.config_read().await.assistant_bundled_skills.clone();
    Ok(reply(service::bundled_view(&settings)))
}

#[utoipa::path(tag = TAG, request_body = BundledSkillToggle,
    responses((status = 200, body = RestResponse<BundledSkillList>)))]
#[put("/admin/system/assistant-bundled-skills/{name}")]
pub async fn set_bundled_skill(
    path: web::Path<String>,
    body: web::Json<BundledSkillToggle>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match assistant_skills::set_bundled(db()?, &path, body.enabled, body.expected_revision)
            .await
        {
            Ok(settings) => reply(service::bundled_view(&settings)),
            Err(assistant_skills::BundledWriteError::Conflict) => failed(
                DeskErrorCode::REVISION_CONFLICT,
                "the setting changed; reload and try again".into(),
            ),
            Err(assistant_skills::BundledWriteError::Invalid) => failed(
                DeskErrorCode::INVALID_PARAMS,
                "unknown bundled skill".into(),
            ),
            Err(assistant_skills::BundledWriteError::Backend(error)) => return Err(error.into()),
        },
    )
}

// Personal skills of the local account.

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<Vec<SkillSummary>>)))]
#[get("/my/ai-assistant-skills")]
pub async fn list_my_skills() -> Result<HttpResponse, DeskSignalError> {
    list(&assistant_skills::personal_scope()).await
}

#[utoipa::path(tag = TAG, request_body = SkillPreviewRequest,
    responses((status = 200, body = RestResponse<SkillPreview>)))]
#[post("/my/ai-assistant-skills/preview")]
pub async fn preview_my_skill(
    body: web::Json<SkillPreviewRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    preview(&assistant_skills::personal_scope(), None, &body).await
}

#[utoipa::path(tag = TAG, request_body = SkillCreate,
    responses((status = 200, body = RestResponse<SkillSaved>)))]
#[post("/my/ai-assistant-skills")]
pub async fn create_my_skill(
    body: web::Json<SkillCreate>,
) -> Result<HttpResponse, DeskSignalError> {
    create(&assistant_skills::personal_scope(), &body).await
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<SkillDetail>)))]
#[get("/my/ai-assistant-skills/{skill_id}")]
pub async fn get_my_skill(path: web::Path<String>) -> Result<HttpResponse, DeskSignalError> {
    detail(&assistant_skills::personal_scope(), &path).await
}

#[utoipa::path(tag = TAG, request_body = SkillPreviewRequest,
    responses((status = 200, body = RestResponse<SkillPreview>)))]
#[post("/my/ai-assistant-skills/{skill_id}/preview")]
pub async fn preview_my_skill_version(
    path: web::Path<String>,
    body: web::Json<SkillPreviewRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    preview(&assistant_skills::personal_scope(), Some(&path), &body).await
}

#[utoipa::path(tag = TAG, request_body = SkillUpdate,
    responses((status = 200, body = RestResponse<SkillSaved>)))]
#[put("/my/ai-assistant-skills/{skill_id}")]
pub async fn update_my_skill(
    path: web::Path<String>,
    body: web::Json<SkillUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    update(&assistant_skills::personal_scope(), &path, &body).await
}

#[utoipa::path(tag = TAG, params(SkillDeleteQuery),
    responses((status = 200, body = RestResponse<String>)))]
#[delete("/my/ai-assistant-skills/{skill_id}")]
pub async fn delete_my_skill(
    path: web::Path<String>,
    query: web::Query<SkillDeleteQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    remove(
        &assistant_skills::personal_scope(),
        path.into_inner(),
        query.expected_revision,
    )
    .await
}
