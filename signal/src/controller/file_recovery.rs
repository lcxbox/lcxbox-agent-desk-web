//! Cookie-owner management of edge backups; no backup bytes enter the database.
use crate::{
    control_authorizer::SINGLE_ACCOUNT_USER_ID, entity::agent_session, error::DeskSignalError,
};
use actix_web::{HttpResponse, get, http::header, post, web};
use desk_agent_protocol::file_recovery::*;
use desk_signal_facade::{
    controller::file_recovery::*,
    model::{auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum},
    service::file_recovery::{
        ExportError, FetchChunk, StillAllowed, request_authorized, start_export,
    },
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use futures_util::StreamExt as _;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
fn unavailable() -> DeskSignalError {
    DeskSignalError::new_custom_error(
        DeskErrorCode::PRECONDITION_FAILED,
        "File recovery is unavailable; refresh the device and conversation state",
    )
}

#[utoipa::path(tag = "FileRecovery", summary = "List pending backup cleanup after conversation deletion",
    params(("after" = Option<String>, Query)), responses((status = 200, body = RestResponse<FileRecoveryCleanupPage>)))]
#[get("/my/device-file-recovery/cleanup-status")]
pub async fn list_file_recovery_cleanup(
    query: web::Query<FileRecoveryCleanupQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    if query.after.as_ref().is_some_and(|v| v.len() > 512) {
        return Err(unavailable());
    }
    let mut rows = crate::file_recovery_cleanup_store::FileRecoveryCleanupStore::new(
        crate::db::get_db().clone(),
    )
    .pending_for_actor(&SINGLE_ACCOUNT_USER_ID.to_string(), query.after.as_deref())
    .await?;
    let more = rows.len() > 100;
    rows.truncate(100);
    let next_cursor = more.then(|| rows.last().unwrap().conversation_id.clone());
    let records = rows
        .into_iter()
        .map(|row| FileRecoveryCleanupStatus {
            conversation_id: row.conversation_id,
            created_at_unix_ms: row.created_at_unix_ms,
            next_attempt_at_unix_ms: row.next_attempt_at_unix_ms,
            attempts: row.attempts,
            reason: cleanup_reason(row.last_error.as_deref()),
        })
        .collect();
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(FileRecoveryCleanupPage {
            records,
            next_cursor,
        })))
}
async fn audience(connections: &SharedConnectionMap, id: &str) -> Result<String, DeskSignalError> {
    let map = connections.read().await;
    let target = map.get(id).ok_or_else(unavailable)?;
    if target.auth_context.auth_kind != AuthKind::TokenAuth
        || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
    {
        return Err(unavailable());
    }
    target
        .client_id
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or_else(unavailable)
}

#[utoipa::path(tag = "FileRecovery", summary = "Retry pending backup cleanup",
    request_body = FileRecoveryCleanupRetryBody, responses((status = 200, body = RestResponse<FileRecoveryCleanupRetryResult>)))]
#[post("/my/device-file-recovery/cleanup-retry")]
pub async fn retry_file_recovery_cleanup(
    body: web::Json<FileRecoveryCleanupRetryBody>,
) -> Result<HttpResponse, DeskSignalError> {
    if body.conversation_id.is_empty() || body.conversation_id.len() > 512 {
        return Err(unavailable());
    }
    let scheduled = crate::file_recovery_cleanup_store::FileRecoveryCleanupStore::new(
        crate::db::get_db().clone(),
    )
    .request_retry(
        &SINGLE_ACCOUNT_USER_ID.to_string(),
        &body.conversation_id,
        chrono::Utc::now().timestamp_millis(),
    )
    .await?;
    if scheduled {
        crate::file_recovery_dispatch::notify();
    }
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(
            FileRecoveryCleanupRetryResult { scheduled },
        )))
}
/// The owner's existing conversations among `ids` that may hold backups on
/// `device`: the ones bound to it and multi-device conversations (no bound
/// device), matching the conversations the recovery scope registration admits.
async fn live_conversations(
    db: &sea_orm::DatabaseConnection,
    device: &str,
    ids: Vec<String>,
) -> Result<std::collections::HashSet<String>, DeskSignalError> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    Ok(agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(SINGLE_ACCOUNT_USER_ID.to_string()))
        .filter(
            sea_orm::Condition::any()
                .add(agent_session::Column::DeviceId.eq(device))
                .add(agent_session::Column::DeviceId.eq("")),
        )
        .filter(agent_session::Column::ConversationId.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|r| r.conversation_id)
        .collect())
}
#[utoipa::path(tag = "FileRecovery", summary = "Manage the owner's device file backups",
    request_body = FileRecoveryManagementBody, responses((status = 200, body = RestResponse<FileRecoveryReply>)))]
#[post("/my/device-file-recovery/manage")]
pub async fn manage_device_file_recovery(
    connections: web::Data<SharedConnectionMap>,
    body: web::Json<FileRecoveryManagementBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let body = body.into_inner();
    if matches!(
        body.request.command,
        FileRecoveryCommand::DeleteConversation { .. } | FileRecoveryCommand::Export { .. }
    ) {
        return Err(unavailable());
    }
    let device = audience(&connections, &body.connection).await?;
    if let FileRecoveryCommand::Query {
        conversation_id: Some(id),
        ..
    }
    | FileRecoveryCommand::Discard {
        conversation_id: id,
        ..
    }
    | FileRecoveryCommand::QueryEntries {
        conversation_id: id,
        ..
    } = &body.request.command
        && !live_conversations(crate::db::get_db(), &device, vec![id.clone()])
            .await?
            .contains(id)
    {
        return Err(unavailable());
    }
    let mut reply = request_authorized(
        &connections,
        &body.connection,
        SINGLE_ACCOUNT_USER_ID,
        None,
        body.request,
    )
    .await
    .map_err(|_| unavailable())?;
    if let FileRecoveryOutcome::Page { page } = &mut reply.outcome {
        let live = live_conversations(
            crate::db::get_db(),
            &device,
            page.records
                .iter()
                .chain(page.oldest_pending_record.iter())
                .map(|r| r.conversation_id.clone())
                .collect(),
        )
        .await?;
        page.records.retain(|r| live.contains(&r.conversation_id));
        if page
            .oldest_pending_record
            .as_ref()
            .is_some_and(|record| !live.contains(&record.conversation_id))
        {
            page.oldest_pending_record = None;
            page.oldest_pending_at_unix_ms = None;
        }
    }
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(reply)))
}
#[utoipa::path(tag = "FileRecovery", summary = "Export an existing conversation's file backup",
    request_body = FileRecoveryDownloadBody,
    responses((status = 200, content_type = "application/zip", body = Vec<u8>)))]
#[post("/my/device-file-recovery/export")]
pub async fn export_device_file_recovery(
    connections: web::Data<SharedConnectionMap>,
    body: web::Json<FileRecoveryDownloadBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let body = body.into_inner();
    let device = audience(&connections, &body.connection).await?;
    if !live_conversations(
        crate::db::get_db(),
        &device,
        vec![body.conversation_id.clone()],
    )
    .await?
    .contains(&body.conversation_id)
    {
        return Err(unavailable());
    }
    let connections = connections.into_inner();
    let fetch: FetchChunk = {
        let connections = connections.clone();
        let body = body.clone();
        Box::new(move |offset| {
            let connections = connections.clone();
            let body = body.clone();
            Box::pin(async move {
                request_authorized(
                    &connections,
                    &body.connection,
                    SINGLE_ACCOUNT_USER_ID,
                    None,
                    FileRecoveryRequest {
                        expected_authority: body.expected_authority,
                        expected_os_user: body.expected_os_user,
                        command: FileRecoveryCommand::Export {
                            recovery_id: body.recovery_id,
                            conversation_id: body.conversation_id,
                            entry: body.entry,
                            offset,
                        },
                    },
                )
                .await
            })
        })
    };
    // Deletion may race the download: every chunk is released only while the
    // conversation still exists.
    let allowed: StillAllowed = {
        let conversation = body.conversation_id.clone();
        Box::new(move || {
            let device = device.clone();
            let conversation = conversation.clone();
            Box::pin(async move {
                live_conversations(crate::db::get_db(), &device, vec![conversation.clone()])
                    .await
                    .is_ok_and(|live| live.contains(&conversation))
            })
        })
    };
    let stream = match start_export(fetch, allowed).await {
        Ok(stream) => stream,
        Err(ExportError::Request(error)) => return Ok(download_request_failure(error)),
        Err(ExportError::Unavailable(reason)) => return Ok(download_failure(reason)),
        Err(ExportError::Rejected) => return Err(unavailable()),
    };
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .insert_header((header::CONTENT_TYPE, "application/zip"))
        .insert_header((
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"file-recovery.zip\"",
        ))
        .insert_header((
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        ))
        // A declared length makes a stopped stream an incomplete download.
        .no_chunking(stream.total_size)
        .streaming(stream.body.map(|chunk| {
            chunk.map_err(|_| actix_web::error::ErrorServiceUnavailable("export stopped"))
        })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_assistant_data_store::entity_table::EntityTableExt;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Set};

    #[tokio::test]
    async fn listed_conversations_include_multi_device_conversations_of_the_owner() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let schema = sea_orm::Schema::new(db.get_database_backend());
        db.execute(&schema.entity_table(agent_session::Entity).to_owned())
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let owner = SINGLE_ACCOUNT_USER_ID.to_string();
        for (conversation, actor, device) in [
            ("bound", owner.as_str(), "device"),
            ("multi", owner.as_str(), ""),
            ("other-device", owner.as_str(), "other"),
            ("other-actor", "2", ""),
        ] {
            agent_session::ActiveModel {
                conversation_id: Set(conversation.into()),
                actor_id: Set(actor.into()),
                device_id: Set(device.into()),
                state_json: Set("{}".into()),
                version: Set(1),
                lease_token: Set(0),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }
        let ids = ["bound", "multi", "other-device", "other-actor", "deleted"]
            .map(String::from)
            .to_vec();
        let live = live_conversations(&db, "device", ids).await.unwrap();
        assert_eq!(
            live,
            ["bound", "multi"]
                .map(String::from)
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        );
    }
}
