//! OSS Signal's owner-only AI Agent orchestrator.

use crate::config::connection::DatabaseConnection;
pub(crate) mod cancellation;
pub mod main_requests;

use actix_web::web;
use desk_agent_protocol::agent_event::AgentEvent;
use desk_agent_protocol::ai_agent::{
    AiAgentAsk, AiAgentContextUpdate, AiAgentContextUpdated, AiAgentEvent,
    AiAgentObjectContextUpdate, AiAgentObjectContextUpdated,
};
use desk_agent_protocol::computer_use::ComputerUseReadiness;
use desk_agent_protocol::data_lineage::DestinationIdentity;
#[cfg(test)]
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
};
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope, ExecutionMode};
use desk_diagnose_core::agent_loop::{
    LoopDeps, LoopOutcome, resume_agent_turn_after_permission, run_agent_turn,
    run_preclaimed_goal_slice,
};
use desk_diagnose_core::agent_policy::PERSONAL_AGENT_POLICY_REVISION;
use desk_diagnose_core::ai_agent::{ai_agent_provider_registry, build_ai_agent_system_message};
use desk_diagnose_core::capability_availability::{
    CapabilityAvailability, agent_central_capability_readiness, callable_tools, inventory_snapshot,
    project_capability_availability,
};
#[cfg(test)]
use desk_diagnose_core::chat::ChatMessage;
#[cfg(test)]
use desk_diagnose_core::chat::ChatRole;
use desk_diagnose_core::context_attachment::ContextAttachmentKind;
use desk_diagnose_core::conversation_key::{
    AgentConversationScope, derive_agent_conversation_key, derive_conversation_key,
    is_valid_client_conversation_id,
};
use desk_diagnose_core::goal::GoalModelBinding;
use desk_diagnose_core::model_capability::{
    ModelCapabilities, apply_model_compatibility, filter_model_compatible_tools,
};
use desk_diagnose_core::prompt::ResponseFormatSpec;
#[cfg(test)]
use desk_diagnose_core::seam::ModelRequest;
use desk_diagnose_core::seam::{
    ClaimTurnParams, HeartbeatGuard, LeaseHeartbeat, ModelSeam, SessionSeam, TurnSink,
};
use desk_diagnose_core::session::{AgentSessionSurface, TriggerOrigin};
use desk_diagnose_core::stream::StreamingTurnSink;
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};

#[cfg(test)]
use sha2::{Digest, Sha256};

use crate::model_dial::SignalModelSeam;
pub(crate) mod fresh;
mod goal;
mod subagents;
pub use goal::resume_queued_goal;
pub use subagents::resume_subagent_turn;
mod scheduled;
mod scheduled_dispatch;
pub use fresh::resume_fresh_task;
pub use scheduled::resume_scheduled_turn;
mod scheduled_permission;
pub use scheduled_permission::{ClaimedScheduledPermission, claim_scheduled_permission};

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const BUSY_FOLLOWUP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
use desk_diagnose_core::permission_resume::authorized_permission_resume_message;
#[cfg(test)]
use desk_diagnose_core::permission_resume::model_bound_permission_resume_message;

fn has_active_resume_desktop_read_grant(
    grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
    tool_name: &str,
    now_unix_ms: u64,
) -> bool {
    let providers = ai_agent_provider_registry();
    let Some(capability) = providers.capability_for_tool(tool_name) else {
        return false;
    };
    let provider = providers
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    desk_diagnose_core::ai_agent::is_requestable_desktop_read(tool_name)
        && grants.iter().any(|grant| {
            grant.provider_id == provider.wire.provider_id
                && grant.capability_id == capability.wire.capability_id
                && grant.tool_name == tool_name
                && grant.effect == capability.wire.effect
                && grant.issued_at_unix_ms <= now_unix_ms
                && grant.revoked_at_unix_ms.is_none()
                && grant.expires_at_unix_ms > now_unix_ms
                && grant.remaining_uses > 0
        })
}

#[cfg(test)]
fn latest_committed_answer(
    snapshot: &crate::agent_session_store::SessionSnapshot,
) -> Option<String> {
    snapshot
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.is_empty()
                && !message.text.trim().is_empty()
        })
        .map(|message| message.text.clone())
}

pub(crate) async fn current_capability_projection<
    C: sea_orm::ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    connections: &SharedConnectionMap,
    target_connection_id: Option<&str>,
    model_capabilities: ModelCapabilities,
) -> (
    desk_diagnose_core::provider_registry::ProviderRegistry,
    Vec<CapabilityAvailability>,
    u64,
    Option<ComputerUseReadiness>,
) {
    let search_binding = match Box::pin(crate::web_search_config::read(db)).await {
        Ok(config) => config.binding(),
        Err(_) => {
            log::warn!("Web Search configuration is unavailable");
            None
        }
    };
    let provider_registry = ai_agent_provider_registry().with_web_search_binding(search_binding);
    let cached_readiness = match target_connection_id {
        Some(target) if connections.read().await.contains_key(target) => {
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(target, chrono::Utc::now())
        }
        _ => None,
    };
    let provider_registry = provider_registry.with_service_platform(
        cached_readiness
            .as_ref()
            .map(|r| r.readiness.os.as_str())
            .unwrap_or("unknown"),
    );
    let readiness = match cached_readiness
        .as_ref()
        .map(|cached| desk_diagnose_core::ai_agent::provider_readiness_reports(&cached.readiness))
        .transpose()
    {
        Ok(Some(readiness)) => readiness,
        Ok(None) => Vec::new(),
        Err(error) => {
            log::warn!("ignoring invalid AI Agent capability readiness: {error}");
            Vec::new()
        }
    };
    let generated_at_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
        .expect("current time is after the Unix epoch");
    let mut inventory = project_capability_availability(
        &provider_registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        generated_at_unix_ms,
        agent_central_capability_readiness(
            provider_registry.web_search_binding().is_some(),
            target_connection_id.is_some(),
        ),
        readiness,
    )
    .expect("validated Computer Use readiness must match the static Provider registry");
    apply_model_compatibility(&mut inventory, model_capabilities);
    (
        provider_registry,
        inventory,
        generated_at_unix_ms,
        cached_readiness.map(|cached| cached.readiness),
    )
}

fn prepare_live_targets(
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    device_id: &str,
    selected_ids: &[String],
    original: Option<&crate::agent_run_event_store::ReadContextSelection>,
    readiness: Option<&ComputerUseReadiness>,
    now: u64,
) -> Result<Vec<desk_diagnose_core::input_read_context::live_read::LiveReadTarget>, AgentError> {
    use desk_diagnose_core::input_read_context::{ReadContextSelection, live_read};
    if let Some(original) = original {
        if !device_id.is_empty() {
            live_read::validate_current(original, device_id, readiness, now)?;
            for name in &original.tool_names {
                if live_read::target_kind(name).is_some() {
                    live_read::target(original, device_id, name, now)?;
                }
            }
        }
        return Ok(original.live_targets.clone());
    }
    let mut names = selected_ids
        .iter()
        .filter_map(|id| registry.capability(id))
        .map(|capability| capability.wire.tool_name.clone())
        .filter(|name| live_read::target_kind(name).is_some())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    live_read::capture(
        &ReadContextSelection {
            external_issues: Vec::new(),
            external_issue_sources: Vec::new(),
            tool_names: names,
            expires_at: None,
            object_attachments: vec![],
            live_targets: vec![],
        },
        device_id,
        readiness,
        now,
    )
}

async fn capture_selectable_live_targets(
    connections: &SharedConnectionMap,
    device_ids: &[String],
    attachments: &[desk_diagnose_core::context_attachment::ContextAttachment],
    destination: &DestinationIdentity,
) -> Result<Vec<desk_diagnose_core::input_read_context::live_read::LiveReadTarget>, AgentError> {
    use desk_diagnose_core::{
        context_attachment::ContextAttachmentKind,
        input_read_context::{ReadContextSelection, live_read},
    };
    let now = chrono::Utc::now();
    let mut selection = ReadContextSelection::default();
    for device_id in device_ids.iter().filter(|id| {
        attachments.iter().any(|item| {
            item.device_id.as_str() == id.as_str()
                && item.kind == ContextAttachmentKind::InteractiveSession
        })
    }) {
        let connection_id = connections
            .read()
            .await
            .current_oss_server(device_id)
            .filter(|state| {
                !state.lifecycle.is_retired()
                    && state
                        .model
                        .ai_agent_settings
                        .as_ref()
                        .is_some_and(|settings| settings.enabled)
            })
            .map(|state| state.model.connection_id.clone());
        if let Some(readiness) = connection_id.and_then(|connection| {
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&connection, now)
        }) {
            live_read::capture_selected(
                &mut selection,
                device_id,
                attachments,
                &readiness.readiness,
                destination,
                now.timestamp_millis().max(0) as u64,
            )?;
        }
    }
    Ok(selection.live_targets)
}

#[allow(clippy::too_many_arguments)]
fn context_selection_claim(
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    inventory: &[CapabilityAvailability],
    readiness: Option<&ComputerUseReadiness>,
    selected_capability_ids: &[String],
    request_id: &str,
    actor_id: &str,
    device_id: &str,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
    skip_unready: bool,
) -> Result<crate::agent_session_store::ContextSelectionClaim, AgentError> {
    desk_diagnose_core::live_context::build_live_context(
        desk_diagnose_core::live_context::LiveContextBuild {
            registry,
            inventory,
            readiness,
            selected_capability_ids,
            request_id,
            actor_id,
            device_id,
            destination,
            now_unix_ms,
            skip_unready,
        },
        || uuid::Uuid::new_v4().to_string(),
    )
}

/// Whether a queued input may expose `tool`: every non-read tool, desktop
/// reads (planning candidates until an owner grant records them), and the
/// reads its frozen input recorded.
fn exposed_for_frozen_reads(
    tool: &desk_diagnose_core::registry::RegisteredTool,
    recorded: &[String],
) -> bool {
    tool.effect != desk_diagnose_core::registry::ToolEffect::ReadOnly
        || desk_diagnose_core::ai_agent::is_requestable_desktop_read(tool.name())
        || recorded.iter().any(|name| name == tool.name())
}

/// The error frame of a preparation exit. A first queued activation keeps its
/// code for the executor, since the frame itself is withheld.
fn activation_error(
    owner_queue: Option<&OwnerQueueTurn>,
    request_id: &str,
    seq: u32,
    error: AgentError,
) -> AgentEvent {
    log_activation_error(request_id, &error);
    if let Some(progress) = owner_queue.and_then(OwnerQueueTurn::activation_progress) {
        progress.note_error(&error);
    }
    AgentEvent::error(request_id, seq, error)
}

/// Background activations (resumes, queued owner inputs) have no browser to
/// receive their error and are retried about once a second, so a failure is
/// logged here, at most once a minute per request.
fn log_activation_error(request_id: &str, error: &AgentError) {
    use std::time::{Duration, Instant};
    static LAST: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
        std::sync::LazyLock::new(Default::default);
    let now = Instant::now();
    {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last
            .get(request_id)
            .is_some_and(|at| now.duration_since(*at) < Duration::from_secs(60))
        {
            return;
        }
        if last.len() >= 512 {
            last.retain(|_, at| now.duration_since(*at) < Duration::from_secs(600));
            if last.len() >= 512 {
                last.clear();
            }
        }
        last.insert(request_id.to_string(), now);
    }
    log::warn!(
        "[ai-agent] turn activation failed request_id={request_id} kind={:?} code={:?} retryable={}: {}",
        error.kind,
        error.error_code,
        error.retryable,
        error.message
    );
}

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn model_rejected(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::ModelRejected,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

async fn send_frame(conn: &ConnectionState, frame: &SignalingModel) -> Result<(), String> {
    let text = serde_json::to_string(frame).map_err(|e| format!("encode frame: {e}"))?;
    tokio::time::timeout(SEND_TIMEOUT, async {
        conn.session.write().await.text(text).await
    })
    .await
    .map_err(|_| format!("send to {} timed out", conn.model.connection_id))?
    .map_err(|e| format!("send to {}: {e}", conn.model.connection_id))
}

async fn stream_event(
    connections: &SharedConnectionMap,
    browser_connection_id: &str,
    event: &AiAgentEvent,
) {
    let browser = {
        let map = connections.read().await;
        map.get(browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &event.request_id,
        SignalingType::AiAgentUpdated,
        None,
        Some(browser_connection_id.to_string()),
        serde_json::to_value(event).ok(),
        None,
    );
    if let Err(e) = send_frame(&browser, &frame).await {
        log::warn!(
            "[ai-agent] failed to stream request_id={} kind={:?}: {e}",
            event.request_id,
            event.kind
        );
    }
}

pub async fn send_capability_inventory(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
) {
    let model_capabilities = crate::model_provider::load(&db)
        .await
        .map(|config| ModelCapabilities {
            image_input: config.supports_image_input,
        })
        .unwrap_or_default();
    let (registry, inventory, generated_at_unix_ms, _) = desk_diagnose_core::future::boxed(|| {
        current_capability_projection(
            &db,
            connections.as_ref(),
            Some(&target_connection_id),
            model_capabilities,
        )
    })
    .await;
    let snapshot = inventory_snapshot(
        &registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        generated_at_unix_ms,
        &inventory,
    )
    .expect("static Provider inventory projection must be valid");
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::AiAgentCapabilitiesUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(snapshot).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[ai-agent] failed to send capability inventory: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn render_document_preview_page(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    request: desk_agent_protocol::document_conversion::DocumentPreviewPageRequest,
) {
    use desk_agent_protocol::document_conversion::DocumentPreviewPageResponse;
    let actor_id = actor_user_id.to_string();
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db);
    let selectable = request.conversation_target.is_selectable();
    let session_device = if selectable { "" } else { &target_device_id };
    let accessible = desk_diagnose_core::future::boxed(|| {
        store.read_agent_snapshot_for_subject(&request.conversation_id, &actor_id, session_device)
    })
    .await
    .ok()
    .flatten()
    .is_some_and(|snapshot| {
        !selectable
            || desk_diagnose_core::document_preview::has_device_receipt(
                &snapshot.session.messages,
                &target_device_id,
                &request.preview_id,
                request.page,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(u64::MAX),
            )
    });
    let result = if accessible {
        desk_diagnose_core::future::boxed(|| {
            crate::remote_tool_edge::document_preview::render_page(
                connections.as_ref(),
                &target_connection_id,
                &actor_id,
                &target_device_id,
                &request.conversation_id,
                &request.preview_id,
                request.page,
            )
        })
        .await
    } else {
        Err(transport_error("Document preview is unavailable"))
    };
    let response = DocumentPreviewPageResponse {
        conversation_id: request.conversation_id,
        preview_id: request.preview_id,
        page: request.page,
        frame: result.as_ref().ok().cloned(),
        error: result.err().map(|error| error.message),
    };
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else { return };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::DocumentPreviewPageUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(response).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[ai-agent] failed to send document preview page: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_context(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    update: AiAgentContextUpdate,
) {
    let result = async {
        desk_diagnose_core::live_context::validate_durable_update(&update)?;
        let actor_id = actor_user_id.to_string();
        let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .with_client_metadata(Some(update.conversation_id.clone()), AgentSessionSurface::AiAgent);
        let mut params = crate::agent_session_store::UpdateLiveContext {
            run_id: AgentConversationScope::Personal
                .context_key(&actor_id, &target_device_id, &update.conversation_id, update.conversation_target)
                .map_err(transport_error)?,
            actor_id: actor_id.clone(), device_id: target_device_id.clone(), update: update.clone(),
            selection: None, created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Some(changed) = desk_diagnose_core::future::boxed(|| store.replay_live_context(&params)).await? {
            return Ok(changed);
        }
        if update.selected_capability_ids.is_empty() {
            params.selection = Some(crate::agent_session_store::ContextSelectionClaim {
                selected_capability_ids: vec![], runtime_bindings: vec![], candidates: vec![],
                now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
            });
            params.created_at = chrono::Utc::now().to_rfc3339();
            return desk_diagnose_core::future::boxed(|| store.update_live_context(&params)).await;
        }
        let config = crate::model_provider::load(&db).await.map_err(|error| {
            transport_error(format!("failed to load model provider config: {error}"))
        })?;
        let destination = config.destination_identity().map_err(|error| {
            transport_error(format!("failed to resolve model destination: {error}"))
        })?;
        let (registry, inventory, now_unix_ms, readiness) =
            desk_diagnose_core::future::boxed(|| current_capability_projection(
                &db,
                connections.as_ref(),
                Some(&target_connection_id),
                ModelCapabilities {
                    image_input: config.supports_image_input,
                },
            ))
            .await;
        if update.selected_capability_ids.iter().any(|capability_id| {
            capability_id
                == desk_diagnose_core::ai_agent::CURRENT_SCREEN_CAPABILITY_ID
        }) {
            return Err(transport_error(
                "CurrentScreen is a one-turn sensitive selection and cannot be saved as durable context",
            ));
        }
        let selection = context_selection_claim(
            &registry,
            &inventory,
            readiness.as_ref(),
            &update.selected_capability_ids,
            &update.client_request_id,
            &actor_id,
            &target_device_id,
            &destination,
            now_unix_ms,
            // The owner's own selection: an unready capability is an error.
            false,
        )?;
        params.selection = Some(selection);
        params.created_at = chrono::Utc::now().to_rfc3339();
        desk_diagnose_core::future::boxed(|| store.update_live_context(&params)).await
    }
    .await;

    let ack = AiAgentContextUpdated {
        conversation_id: update.conversation_id,
        client_request_id: update.client_request_id,
        changed: result.as_ref().copied().unwrap_or(false),
        error: result.err().map(|error| error.message),
    };
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::AiAgentContextUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[ai-agent] failed to send context update: {error}");
    }
}

pub(crate) async fn reject_live_context(
    actor: &ConnectionState,
    request: &SignalingModel,
    message: String,
) {
    let payload = request.get_data::<serde_json::Value>().ok();
    let selector = |name: &str| {
        payload
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.len() <= 256)
            .unwrap_or_default()
            .to_string()
    };
    let ack = AiAgentContextUpdated {
        conversation_id: selector("conversation_id"),
        client_request_id: selector("client_request_id"),
        changed: false,
        error: Some(message),
    };
    let frame = SignalingModel::new(
        &request.request_id,
        SignalingType::AiAgentContextUpdated,
        None,
        Some(actor.model.connection_id.clone()),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) = send_frame(actor, &frame).await {
        log::warn!("[ai-agent] failed to send context rejection: {error}");
    }
}

/// Answer a rejected object-context update the way an applied one is answered,
/// echoing the request's correlation fields so the browser can settle it.
pub(crate) async fn reject_object_context(
    actor: &ConnectionState,
    request: &SignalingModel,
    message: String,
) {
    let payload = request.get_data::<serde_json::Value>().ok();
    let selector = |name: &str| {
        payload
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.len() <= 128)
            .unwrap_or_default()
            .to_string()
    };
    let ack = AiAgentObjectContextUpdated {
        conversation_id: selector("conversation_id"),
        client_request_id: selector("client_request_id"),
        changed: false,
        error: Some(message),
    };
    let frame = SignalingModel::new(
        &request.request_id,
        SignalingType::AiAgentObjectContextUpdated,
        None,
        Some(actor.model.connection_id.clone()),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) = send_frame(actor, &frame).await {
        log::warn!("[ai-agent] failed to send object context rejection: {error}");
    }
}

/// Answer a rejected ask with the turn's error event, so the browser settles the
/// submission instead of waiting for an acknowledgement.
pub(crate) async fn reject_ask(
    actor: &ConnectionState,
    request: &SignalingModel,
    error: AgentError,
) {
    let event = AgentEvent::error(&request.request_id, 0, error);
    let frame = SignalingModel::new(
        &request.request_id,
        SignalingType::AiAgentUpdated,
        None,
        Some(actor.model.connection_id.clone()),
        serde_json::to_value(&event).ok(),
        None,
    );
    if let Err(error) = send_frame(actor, &frame).await {
        log::warn!("[ai-agent] failed to send ask rejection: {error}");
    }
}

pub(crate) async fn apply_object_context_update(
    db: DatabaseConnection,
    actor_user_id: i32,
    target_device_id: String,
    update: &AiAgentObjectContextUpdate,
) -> Result<bool, AgentError> {
    update.validate().map_err(transport_error)?;
    let actor_id = actor_user_id.to_string();
    let mut params = crate::agent_session_store::UpdateObjectContext {
        run_id: AgentConversationScope::Personal
            .object_context_key(&actor_id, &target_device_id, update)
            .map_err(transport_error)?,
        actor_id,
        device_id: target_device_id,
        update: update.clone(),
        destination: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            Some(update.conversation_id.clone()),
            AgentSessionSurface::AiAgent,
        );
    if let Some(directory_update) = desk_diagnose_core::file_scope::transaction::from_owner_decision(
        update,
        desk_diagnose_core::file_scope::FileScopeSubject {
            actor_id: params.actor_id.clone(),
            device_id: params.device_id.clone(),
            conversation_id: params.run_id.clone(),
        },
    ) {
        return Ok(desk_diagnose_core::future::boxed(|| async {
            store
                .update_file_scope(&directory_update, chrono::Utc::now())
                .await
        })
        .await?
        .changed);
    }
    if let Some(changed) = desk_diagnose_core::future::boxed(|| async {
        desk_diagnose_core::future::boxed(|| store.replay_object_context(&params)).await
    })
    .await?
    {
        return Ok(changed);
    }
    if !matches!(
        update.operation,
        desk_agent_protocol::ai_agent::AiAgentObjectContextOperation::Detach { .. }
    ) {
        let config =
            desk_diagnose_core::future::boxed(|| async { crate::model_provider::load(&db).await })
                .await
                .map_err(|_| transport_error("failed to load model provider config"))?;
        params.destination = Some(
            config
                .destination_identity()
                .map_err(|_| transport_error("failed to resolve model destination"))?,
        );
    }
    desk_diagnose_core::future::boxed(|| async {
        desk_diagnose_core::future::boxed(|| store.update_object_context(&params)).await
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_object_context(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    update: AiAgentObjectContextUpdate,
) {
    let result =
        if let desk_agent_protocol::ai_agent::AiAgentObjectContextOperation::SelectDirectory {
            path,
            ..
        } = &update.operation
        {
            async {
                use desk_diagnose_core::file_scope::{FileScopeSubject, transaction};
                update.validate().map_err(transport_error)?;
                let actor_id = actor_user_id.to_string();
                let conversation_id = AgentConversationScope::Personal
                    .object_context_key(&actor_id, &target_device_id, &update)
                    .map_err(transport_error)?;
                let subject = FileScopeSubject {
                    actor_id,
                    device_id: target_device_id.clone(),
                    conversation_id,
                };
                let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                    .with_client_metadata(
                        Some(update.conversation_id.clone()),
                        AgentSessionSurface::AiAgent,
                    );
                if let Some(receipt) = desk_diagnose_core::future::boxed(|| async {
                    store
                        .read_file_scope_receipt(
                            &subject,
                            &update.conversation_id,
                            &update.client_request_id,
                        )
                        .await
                })
                .await?
                {
                    transaction::match_owner_selection(&receipt, &update, &subject).map_err(
                        |_| {
                            transport_error(
                                "Directory selection conflicts with its original request",
                            )
                        },
                    )?;
                    return Ok(receipt.changed);
                }
                if update.conversation_target.is_selectable() {
                    use desk_agent_data_store::device_roster::{load, personal_selection_key};
                    if load(
                        &db,
                        &subject.actor_id,
                        &personal_selection_key(&subject.conversation_id),
                    )
                    .await
                    .map_err(|error| error.into_agent_error())?
                    .is_none_or(|roster| !roster.members.contains_key(&subject.device_id))
                    {
                        return Err(transport_error(
                            "Select the directory's device in this agent first",
                        ));
                    }
                }
                let resolved = desk_diagnose_core::future::boxed(|| async {
                    crate::remote_tool_edge::directory::resolve_candidate(
                        &connections,
                        &target_connection_id,
                        &subject.actor_id,
                        &subject.device_id,
                        path,
                    )
                    .await
                })
                .await?;
                let selection = transaction::owner_selection(&update, subject, resolved)
                    .map_err(|_| transport_error("Invalid directory selection"))?;
                Ok(desk_diagnose_core::future::boxed(|| async {
                    store
                        .update_file_scope(&selection, chrono::Utc::now())
                        .await
                })
                .await?
                .changed)
            }
            .await
        } else {
            desk_diagnose_core::future::boxed(|| async {
                apply_object_context_update(db, actor_user_id, target_device_id, &update).await
            })
            .await
        };

    let ack = AiAgentObjectContextUpdated {
        conversation_id: update.conversation_id,
        client_request_id: update.client_request_id,
        changed: result.as_ref().copied().unwrap_or(false),
        error: result.err().map(|error| error.message),
    };
    let browser = {
        let map = desk_diagnose_core::future::boxed(|| async { connections.read().await }).await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::AiAgentObjectContextUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) =
        desk_diagnose_core::future::boxed(|| async { send_frame(&browser, &frame).await }).await
    {
        log::warn!("[ai-agent] failed to send object context update: {error}");
    }
}

struct SignalStoreHeartbeat {
    cancel: tokio_util::sync::CancellationToken,
    store: crate::agent_session_store::SignalAgentSessionStore,
}

impl LeaseHeartbeat for SignalStoreHeartbeat {
    fn is_healthy(&self) -> bool {
        !self.cancel.is_cancelled()
    }
    fn start(&self, conversation_id: String, lease_token: u64) -> Box<dyn HeartbeatGuard> {
        let store = Box::new(self.store.clone());
        let cancel = self.cancel.clone();
        let handle = actix_web::rt::spawn(async move {
            let error = desk_signal_facade::service::lease_heartbeat::renew_until_lost(
                HEARTBEAT_INTERVAL,
                std::time::Duration::from_secs(crate::agent_session_store::LEASE_TTL_SECS as u64),
                || async {
                    store
                        .heartbeat(
                            &conversation_id,
                            lease_token,
                            &chrono::Utc::now().to_rfc3339(),
                        )
                        .await
                },
            )
            .await;
            log::warn!(
                "[agent-lease] renewal stopped: conversation_id={conversation_id}, kind={:?}",
                error.kind
            );
            cancel.cancel();
        });
        Box::new(SignalHeartbeatGuard(handle))
    }
}

struct SignalHeartbeatGuard(actix_web::rt::task::JoinHandle<()>);
impl HeartbeatGuard for SignalHeartbeatGuard {}
impl Drop for SignalHeartbeatGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

use crate::agent_model::MeteredModel;

use desk_agent_data_store::main_request::{
    self, OwnerAdmission, OwnerAdmissionResult, OwnerQueueTurn,
};
use desk_diagnose_core::model_message_labels::model_bound_user_message;

#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
) {
    run_incoming_turn(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn submit_owner_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
) {
    let queue = main_request::accepts_owner_ask(&ask).then(|| {
        Box::new(OwnerQueueTurn::Submit {
            ask: ask.clone(),
            reply_connection_id: browser_connection_id.clone(),
        })
    });
    run_incoming_turn(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        queue,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_incoming_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    owner_queue: Option<Box<OwnerQueueTurn>>,
) {
    log::info!(
        "[agent-input] received request_id={} message_id={} conversation_id={:?}",
        request_id,
        ask.client_message_id,
        ask.conversation_id
    );
    run_turn_inner_with_queue(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        None,
        owner_queue,
    )
    .await;
}

/// Resume the exact persisted requirement after its owner records a permission
/// decision. A hidden, server-authored protocol bridge is appended so
/// chat-completions providers reliably start a new turn; it is not recorded as
/// user input and carries no new requirement.
#[allow(clippy::too_many_arguments)]
pub async fn resume_after_permission_decision(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    conversation_id: String,
    permission_request_id: String,
    ask: AiAgentAsk,
) {
    if !target_device_id.is_empty() {
        match crate::agent_goal_store::wake_for_permission_decision(
            &db,
            &conversation_id,
            &actor_user_id.to_string(),
            &target_device_id,
            &permission_request_id,
            chrono::Utc::now(),
        )
        .await
        {
            Ok(desk_diagnose_core::goal::GoalPermissionWake::NoGoal) => {}
            Ok(
                desk_diagnose_core::goal::GoalPermissionWake::Queued
                | desk_diagnose_core::goal::GoalPermissionWake::Held,
            ) => return,
            Err(error) => {
                log::warn!("[ai-agent-goal] permission decision routing deferred: {error}");
                return;
            }
        }
    }
    run_turn_inner_with_queue(
        connections,
        db,
        request_id,
        String::new(),
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        Some(PermissionResume {
            conversation_id,
            permission_request_id: Some(permission_request_id),
            work_event: None,
            decision: None,
        }),
        None,
    )
    .await;
}

/// Resume the exact paused requirement after the AI reviewer or the owner
/// decided the proposal or directory request its turn paused on. A goal
/// segment is woken instead by the decision-resume executor.
#[allow(clippy::too_many_arguments)]
pub async fn resume_after_decision(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    conversation_id: String,
    kind: desk_diagnose_core::proposal_review::DecisionKind,
    decided_request_id: String,
    input_revision: u64,
    ask: AiAgentAsk,
) {
    run_turn_inner_with_queue(
        connections,
        db,
        request_id,
        String::new(),
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        Some(PermissionResume {
            conversation_id,
            permission_request_id: None,
            work_event: None,
            decision: Some(DecisionResume {
                kind,
                request_id: decided_request_id,
                input_revision,
            }),
        }),
        None,
    )
    .await;
}

/// Continue the owner's existing requirement after a durable background result.
/// The caller matched the persisted receipt to its target and original input.
pub async fn resume_after_work_completion(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    target_connection_id: String,
    session: desk_diagnose_core::session::PersistedAgentSession,
    selected_event_id: Option<String>,
) -> Result<LoopOutcome, AgentError> {
    let question =
        desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
            .ok_or_else(|| transport_error("original requirement is unavailable"))?
            .text
            .clone();
    let request_id = uuid::Uuid::new_v4().to_string();
    let actor_user_id = session
        .actor_id
        .parse::<i32>()
        .map_err(|_| transport_error("invalid continuation owner"))?;
    let ask = AiAgentAsk {
        temporary_conversation: None,
        question,
        locale: session.response_locale.clone(),
        client_message_id: request_id.clone(),
        conversation_id: session.client_conversation_id.clone(),
        ..Default::default()
    };
    compose_turn(
        connections,
        db,
        request_id,
        String::new(),
        target_connection_id,
        actor_user_id,
        session.device_id,
        ask,
        Some(PermissionResume {
            conversation_id: session.conversation_id,
            permission_request_id: None,
            work_event: selected_event_id.map(|event| (session.input_revision, event)),
            decision: None,
        }),
        None,
        None,
        None,
    )
    .await?
    .ok_or_else(|| transport_error("work continuation did not pass runtime preflight"))
}

/// Conversation tools the host serves itself. The model never gets a tool to
/// request permissions: a direct call without a matching grant becomes the
/// approval request, built by the server in the same internal format.
fn extend_internal_tools(registry: &mut Vec<desk_diagnose_core::registry::RegisteredTool>) {
    // Internal run projection is not a Provider capability and grants no device
    // authority. It is always callable so the model can keep the user-visible
    // task assessment current even when no device context was selected.
    registry.extend(desk_diagnose_core::task_status_tools::task_status_tool_registry());
    registry.extend(desk_diagnose_core::subagent::tools::registry());
    registry.extend(desk_diagnose_core::goal_tools::registry());
    registry.extend(desk_diagnose_core::goal_tools::open_registry());
    registry.extend(desk_diagnose_core::directory_tools::registry());
    registry.extend(desk_diagnose_core::schedule::proposal::registry());
    registry.extend(desk_diagnose_core::skills::authoring::registry());
    registry
        .extend(desk_diagnose_core::capability_disclosure::capability_discovery_tool_registry());
    registry.extend(desk_diagnose_core::conversation_history::conversation_history_tool_registry());
    registry.extend(desk_diagnose_core::wait_tools::wait_tool_registry());
}

/// A server-authored continuation of an existing requirement: after a
/// permission decision (`permission_request_id`), a background result
/// (`work_event`) or a proposal or directory decision (`decision`).
#[derive(Clone)]
struct PermissionResume {
    conversation_id: String,
    permission_request_id: Option<String>,
    work_event: Option<(u64, String)>,
    decision: Option<DecisionResume>,
}

/// The decided request a paused conversation resumes after.
#[derive(Clone)]
struct DecisionResume {
    kind: desk_diagnose_core::proposal_review::DecisionKind,
    request_id: String,
    input_revision: u64,
}

impl PermissionResume {
    /// A decision of the owner or the AI reviewer resumes the requirement.
    fn is_decided(&self) -> bool {
        self.permission_request_id.is_some() || self.decision.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn run_turn_inner(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    resume_conversation_id: Option<PermissionResume>,
) {
    run_turn_inner_with_queue(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        resume_conversation_id,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_inner_with_queue(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    resume_conversation_id: Option<PermissionResume>,
    owner_queue: Option<Box<OwnerQueueTurn>>,
) {
    let result = compose_turn_with_queue(
        connections.clone(),
        db,
        request_id.clone(),
        browser_connection_id.clone(),
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        resume_conversation_id,
        None,
        None,
        None,
        owner_queue,
    )
    .await;
    if let Err(error) = result {
        log::warn!(
            "[agent-input] turn preparation failed: {:?}: {}",
            error.kind,
            error.message
        );
        if !browser_connection_id.is_empty() {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(&request_id, 1, error),
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
// Keep child execution state on the heap instead of embedding it in each caller.
#[inline(never)]
fn compose_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    resume_conversation_id: Option<PermissionResume>,
    scheduled: Option<scheduled::PreparedResume>,
    goal_resume: Option<desk_diagnose_core::goal::GoalRun>,
    delegated: Option<desk_diagnose_core::subagent::runtime::RuntimeTurn>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<Option<LoopOutcome>, AgentError>>>>
{
    compose_turn_with_queue(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        resume_conversation_id,
        scheduled,
        goal_resume,
        delegated,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn compose_turn_with_queue(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    resume_conversation_id: Option<PermissionResume>,
    scheduled: Option<scheduled::PreparedResume>,
    goal_resume: Option<desk_diagnose_core::goal::GoalRun>,
    delegated: Option<desk_diagnose_core::subagent::runtime::RuntimeTurn>,
    owner_queue: Option<Box<OwnerQueueTurn>>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<Option<LoopOutcome>, AgentError>>>>
{
    let preparation = desk_diagnose_core::future::boxed(|| {
        prepare_composed_turn(
            connections,
            db,
            request_id,
            browser_connection_id,
            target_connection_id,
            actor_user_id,
            target_device_id,
            ask,
            resume_conversation_id,
            scheduled,
            goal_resume,
            delegated,
            owner_queue,
        )
    });
    Box::pin(async move {
        let mut preparation: TurnPreparation = preparation;
        let prepared = loop {
            match preparation.await? {
                PreparationStep::Continue(next) => preparation = next,
                PreparationStep::Ready(prepared) => break prepared,
            }
        };
        match prepared {
            Some(prepared) => prepared.await,
            None => Ok(None),
        }
    })
}

type PreparedTurn =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<LoopOutcome>, AgentError>>>>;

// Each preparation continuation owns its state; completed poll frames are
// released before the driver starts the next phase. No additional task is used.
type TurnPreparation =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<PreparationStep, AgentError>>>>;
enum PreparationStep {
    Continue(TurnPreparation),
    Ready(Option<PreparedTurn>),
}

#[allow(clippy::too_many_arguments)]
async fn prepare_composed_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: AiAgentAsk,
    resume_conversation_id: Option<PermissionResume>,
    scheduled: Option<scheduled::PreparedResume>,
    goal_resume: Option<desk_diagnose_core::goal::GoalRun>,
    delegated: Option<desk_diagnose_core::subagent::runtime::RuntimeTurn>,
    owner_queue: Option<Box<OwnerQueueTurn>>,
) -> Result<PreparationStep, AgentError> {
    let targetless = target_device_id.is_empty();
    if targetless
        && (!target_connection_id.is_empty()
            || actor_user_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID
            || resume_conversation_id
                .as_ref()
                .is_some_and(|resume| !resume.is_decided() && resume.work_event.is_none()))
    {
        return Err(transport_error(
            "targetless agent input cannot carry device context",
        ));
    }
    // A delegated child resumes after its permission decision by native id.
    // It has no client conversation; the server rebuilt its ask and its task
    // source is required below.
    let resumed_child = targetless
        && delegated.is_none()
        && resume_conversation_id.as_ref().is_some_and(|resume| {
            resume.is_decided()
                && desk_diagnose_core::agent_policy::targetless_subject(
                    &actor_user_id.to_string(),
                    &resume.conversation_id,
                    None,
                    desk_diagnose_core::agent_policy::TargetlessEntry::PermissionResume,
                ) == Ok(desk_diagnose_core::agent_policy::TargetlessSubject::Child)
        });
    if delegated.is_none() && !resumed_child {
        desk_diagnose_core::agent_policy::validate_agent_ask_scope(&ask, !targetless)
            .map_err(transport_error)?;
    }
    if let Some(runtime) = &delegated
        && (resume_conversation_id.is_some()
            || scheduled.is_some()
            || goal_resume.is_some()
            || ask.start_goal
            || actor_user_id.to_string() != runtime.session().actor_id
            || target_device_id != runtime.session().device_id
            || ask.question != runtime.source().owner_requirement.text
            || ask.locale != runtime.session().response_locale
            || !ask.selected_attachment_ids.is_empty())
    {
        return Err(transport_error("invalid delegated runtime composition"));
    }
    let cancel_registration = cancellation::register(actor_user_id, &request_id);
    // A first queued activation withholds every frame until native admission.
    let (browser_connection_id, admitted_browser) = match owner_queue
        .as_deref()
        .and_then(OwnerQueueTurn::activation_progress)
    {
        Some(_) => (String::new(), browser_connection_id),
        None => (browser_connection_id, String::new()),
    };
    desk_diagnose_core::future::boxed(|| async {
        stream_event(
            connections.as_ref(),
            &browser_connection_id,
            &AgentEvent::status(&request_id, 0, "accepting"),
        )
        .await
    })
    .await;

    let config = match desk_diagnose_core::future::boxed(|| async {
        crate::model_provider::load(&db).await
    })
    .await
    {
        Ok(config) => config,
        Err(e) => {
            if goal_resume.is_some() || delegated.is_some() {
                return Err(AgentError {
                    kind: AgentErrorKind::ModelUnavailable,
                    message: format!("failed to load model provider config: {e}"),
                    retryable: true,
                    safe_for_model: true,
                    error_code: None,
                });
            }
            desk_diagnose_core::future::boxed(|| async {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(
                        owner_queue.as_deref(),
                        &request_id,
                        1,
                        transport_error(format!("failed to load model provider config: {e}")),
                    ),
                )
                .await
            })
            .await;
            return Ok(PreparationStep::Ready(None));
        }
    };
    let model_cancel = (*cancel_registration).clone();
    let seam = match SignalModelSeam::from_config(&config) {
        Ok(seam) => seam
            .with_context_db(db.clone())
            .with_cancellation(model_cancel.clone()),
        Err(error) => {
            if goal_resume.is_some() || delegated.is_some() {
                return Err(model_rejected(error.message));
            }
            desk_diagnose_core::future::boxed(|| async {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(owner_queue.as_deref(), &request_id, 1, error),
                )
                .await
            })
            .await;
            return Ok(PreparationStep::Ready(None));
        }
    };
    let resolved = config.destination_identity();
    #[cfg(test)]
    let resolved = resolved.and_then(|destination| {
        if activation_faults::destination_fails(&request_id) {
            Err(crate::model_provider::ModelDestinationError::NotConfigured)
        } else {
            Ok(destination)
        }
    });
    let destination = match resolved {
        Ok(destination) => destination,
        Err(error) => {
            if goal_resume.is_some() || delegated.is_some() {
                return Err(model_rejected(format!(
                    "failed to resolve model destination: {error}"
                )));
            }
            desk_diagnose_core::future::boxed(|| async {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(
                        owner_queue.as_deref(),
                        &request_id,
                        1,
                        transport_error(format!("failed to resolve model destination: {error}")),
                    ),
                )
                .await
            })
            .await;
            return Ok(PreparationStep::Ready(None));
        }
    };
    let actor_id = actor_user_id.to_string();
    if let Some(runtime) = &delegated
        && !desk_diagnose_core::model_egress::same_gateway(
            &destination,
            &runtime.source().model_destination,
        )
    {
        return Err(model_rejected(
            "delegated task's frozen model destination changed",
        ));
    }
    let client_conversation_id = ask
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| is_valid_client_conversation_id(id))
        .map(str::to_string);
    let conversation_id = if let Some(runtime) = delegated.as_ref() {
        runtime.session().conversation_id.clone()
    } else if targetless {
        use desk_diagnose_core::agent_policy::{
            PERSONAL_KEY_MISMATCH, TargetlessEntry, targetless_subject,
        };
        let (conversation, entry) = match resume_conversation_id.as_ref() {
            Some(resume) => (
                resume.conversation_id.clone(),
                if resume.is_decided() {
                    TargetlessEntry::PermissionResume
                } else {
                    TargetlessEntry::Input
                },
            ),
            None => (
                derive_agent_conversation_key(
                    &actor_id,
                    AgentConversationScope::Personal,
                    client_conversation_id.as_deref().ok_or_else(|| {
                        transport_error("targetless input requires a valid continuation id")
                    })?,
                ),
                TargetlessEntry::Input,
            ),
        };
        targetless_subject(
            &actor_id,
            &conversation,
            client_conversation_id.as_deref(),
            entry,
        )
        .map_err(|error| {
            transport_error(if error == PERSONAL_KEY_MISMATCH {
                "permission resume does not match its personal conversation"
            } else {
                error
            })
        })?;
        conversation
    } else {
        scheduled
            .as_ref()
            .map(|resume| resume.claimed.session.conversation_id.clone())
            .or_else(|| {
                resume_conversation_id
                    .as_ref()
                    .map(|resume| resume.conversation_id.clone())
            })
            .or_else(|| {
                goal_resume
                    .as_ref()
                    .map(|goal| goal.conversation_id.clone())
            })
            .unwrap_or_else(|| {
                derive_conversation_key(
                    &actor_id,
                    &target_device_id,
                    ask.conversation_id.as_deref(),
                    &request_id,
                )
            })
    };
    let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
        .expect("current time is after the Unix epoch");
    let conversation_id = match owner_queue
        .as_deref()
        .map(|queue| queue.execution_conversation(&actor_id, &conversation_id))
        .transpose()
        .map_err(|error| error.into_agent_error())?
    {
        Some(Some(execution)) => execution.to_owned(),
        _ => conversation_id,
    };
    if let Some(OwnerQueueTurn::Activate {
        request_id,
        conversation_id: queue,
        destination: original,
        ..
    }) = owner_queue.as_deref()
        // A queued input follows the owner's model within its gateway family;
        // the activated turn is labelled for the current one.
        && !desk_diagnose_core::model_egress::same_recipient(original, &destination)
    {
        main_requests::fail_destination(
            &db,
            &actor_id,
            queue,
            &conversation_id,
            request_id,
            owner_queue
                .as_deref()
                .and_then(OwnerQueueTurn::activation_claim),
        )
        .await?;
        return Err(main_request::failure_error(
            desk_agent_protocol::ai_agent::MainRequestFailure::DestinationChanged,
        ));
    }
    let session_reader = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(client_conversation_id.clone(), AgentSessionSurface::AiAgent);
    #[cfg(test)]
    crate::stack_probe::observe(&conversation_id, "prepare");
    let snapshot = match desk_diagnose_core::future::boxed(|| async {
        session_reader
            .read_snapshot_for_subject(&conversation_id, &actor_id, &target_device_id)
            .await
    })
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            desk_diagnose_core::future::boxed(|| async {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(owner_queue.as_deref(), &request_id, 1, error),
                )
                .await
            })
            .await;
            return Ok(PreparationStep::Ready(None));
        }
    };
    if let Some(revision) = resume_conversation_id.as_ref().and_then(|resume| {
        resume
            .work_event
            .as_ref()
            .map(|(revision, _)| revision)
            .or(resume
                .decision
                .as_ref()
                .map(|decision| &decision.input_revision))
    }) && snapshot
        .as_ref()
        .is_some_and(|session| *revision != session.input_revision)
    {
        return Ok(PreparationStep::Ready(Some(Box::pin(async {
            Ok(Some(LoopOutcome::TurnBusy))
        }))));
    }
    let permission_child_context = if resume_conversation_id
        .as_ref()
        .is_some_and(PermissionResume::is_decided)
    {
        let snapshot = snapshot
            .as_ref()
            .ok_or_else(|| transport_error("permission session is unavailable"))?;
        desk_diagnose_core::future::boxed(|| async {
            crate::agent_subagent_store::SubAgentStore::new(db.clone())
                .child_context_for_subject(
                    &conversation_id,
                    &actor_id,
                    &target_device_id,
                    snapshot.seq,
                )
                .await
        })
        .await
        .map_err(|_| transport_error("delegated permission source changed"))?
    } else {
        None
    };
    if resumed_child && permission_child_context.is_none() {
        return Err(transport_error("delegated permission source changed"));
    }
    if let Some((creation, _)) = &permission_child_context
        && !desk_diagnose_core::model_egress::same_gateway(
            &destination,
            &creation.source.model_destination,
        )
    {
        return Err(model_rejected(
            "permission continuation changed the delegated model destination",
        ));
    }
    let permission_child_context = permission_child_context.map(Box::new);
    let snapshot = snapshot.map(Box::new);
    let mut scheduled = scheduled.map(Box::new);
    let goal_resume = goal_resume.map(Box::new);
    let delegated = delegated.map(Box::new);
    Ok(PreparationStep::Continue(
        desk_diagnose_core::future::phase(|| async move {
            let event_store =
                crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone());
            let client_conversation_id = if resume_conversation_id.is_some()
                || scheduled.is_some()
                || goal_resume.is_some()
                || delegated.is_some()
            {
                snapshot
                    .as_ref()
                    .and_then(|session| session.client_conversation_id.clone())
            } else {
                client_conversation_id
            };
            let subject = crate::agent_run_event_store::InputSubject {
                run_id: &conversation_id,
                actor_id: &actor_id,
                device_id: &target_device_id,
                client_conversation_id: client_conversation_id.as_deref(),
            };
            let selection = async {
                let original = if let Some(runtime) = &delegated {
                    runtime.read_context()
                } else if let Some(resume) = scheduled.as_ref() {
                    resume.original.clone()
                } else if goal_resume.is_some() {
                    let session = snapshot
                        .as_ref()
                        .ok_or_else(|| transport_error("goal session is missing"))?;
                    desk_diagnose_core::future::boxed(|| async {
                        event_store
                            .original_read_context(subject, session.input_revision)
                            .await
                    })
                    .await?
                } else if let Some(resume) = resume_conversation_id.as_ref() {
                    let session = snapshot
                        .as_ref()
                        .ok_or_else(|| transport_error("original input session is missing"))?;
                    use desk_diagnose_core::dynamic_run::PermissionRequestState;
                    if let Some(permission_request_id) = &resume.permission_request_id
                        && !session.permission_requests.iter().any(|request| {
                            &request.request_id == permission_request_id
                                && request.input_revision == session.input_revision
                                && matches!(
                                    request.state,
                                    PermissionRequestState::Approved
                                        | PermissionRequestState::PartiallyApproved
                                        | PermissionRequestState::Denied
                                )
                        })
                    {
                        return Err(transport_error(
                            "permission decision no longer belongs to the current input",
                        ));
                    }
                    if let Some(decision) = &resume.decision
                        && session.decision_pause.as_ref().is_none_or(|pause| {
                            !pause.is(decision.kind, &decision.request_id)
                                || pause.input_revision != session.input_revision
                        })
                    {
                        return Err(transport_error(
                            "the decided request no longer pauses the current input",
                        ));
                    }
                    desk_diagnose_core::future::boxed(|| async {
                        event_store
                            .original_read_context(subject, session.input_revision)
                            .await
                    })
                    .await?
                } else {
                    None
                };
                let objects = if resume_conversation_id.is_some()
                    || scheduled.is_some()
                    || goal_resume.is_some()
                    || delegated.is_some()
                {
                    original
                        .as_ref()
                        .map(|selection| selection.object_attachments.clone())
                        .unwrap_or_default()
                } else {
                    desk_diagnose_core::future::boxed(|| async {
                        event_store
                            .select_objects(
                                subject,
                                &ask.client_message_id,
                                &ask.selected_attachment_ids,
                                now_unix_ms,
                            )
                            .await
                    })
                    .await?
                };
                Ok::<_, AgentError>((objects, original))
            }
            .await;
            let (selected_objects, original_read_context) = match selection {
                Ok(selection) => selection,
                Err(error) => {
                    desk_diagnose_core::future::boxed(|| async {
                        stream_event(
                            connections.as_ref(),
                            &browser_connection_id,
                            &activation_error(owner_queue.as_deref(), &request_id, 1, error),
                        )
                        .await
                    })
                    .await;
                    return Ok(PreparationStep::Ready(None));
                }
            };
            let mut ask = ask;
            if resume_conversation_id.is_some()
                || scheduled.is_some()
                || goal_resume.is_some()
                || delegated.is_some()
            {
                if let Some((creation, _)) = permission_child_context.as_deref() {
                    ask.locale = creation.response_locale.clone();
                }
                ask.question = permission_child_context
                    .as_deref()
                    .map(|(creation, _)| creation.source.owner_requirement.text.clone())
                    .or_else(|| {
                        delegated
                            .as_ref()
                            .map(|runtime| runtime.source().owner_requirement.text.clone())
                    })
                    .or_else(|| {
                        snapshot
                            .as_ref()
                            .and_then(|session| {
                                desk_diagnose_core::permission_resume::latest_user_requirement(
                                    &session.messages,
                                )
                            })
                            .map(|message| message.text.clone())
                    })
                    .unwrap_or_default();
                let providers = ai_agent_provider_registry();
                ask.selected_capability_ids =
                    original_read_context
                        .as_ref()
                        .filter(|_| !target_device_id.is_empty())
                        .map(|selection| {
                            selection
                    .tool_names
                    .iter()
                    .filter_map(|name| providers.capability_for_tool(name))
                    .map(|capability| capability.wire.capability_id.clone())
                    .filter(|id| {
                        desk_diagnose_core::ai_agent::is_selectable_context_capability_id(id)
                    })
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
                        })
                        .unwrap_or_default();
                ask.selected_attachment_ids = selected_objects
                    .iter()
                    .map(|object| object.attachment_id.clone())
                    .collect();
            }
            let mut selected_file_roots = Vec::new();
            let mut selected_spreadsheet_roots = Vec::new();
            let mut selected_terminal_roots = Vec::new();
            let mut selected_window_roots = Vec::new();
            for attachment in &selected_objects {
                if attachment.kind == ContextAttachmentKind::InteractiveSession {
                    continue;
                }
                let reference = match serde_json::from_str::<
                    desk_agent_protocol::computer_use::ObjectRef,
                >(&attachment.object_ref.opaque_token)
                {
                    Ok(reference) => reference,
                    Err(_) => {
                        desk_diagnose_core::future::boxed(|| async {
                            stream_event(
                                connections.as_ref(),
                                &browser_connection_id,
                                &activation_error(
                                    owner_queue.as_deref(),
                                    &request_id,
                                    1,
                                    transport_error("original object reference is invalid"),
                                ),
                            )
                            .await
                        })
                        .await;
                        return Ok(PreparationStep::Ready(None));
                    }
                };
                if attachment.kind == ContextAttachmentKind::TerminalSessionRef {
                    selected_terminal_roots.push(reference);
                } else if attachment.kind == ContextAttachmentKind::WindowSelection {
                    selected_window_roots.push(reference);
                } else {
                    // Display labels do not determine file type or grant read authority.
                    // The original device validates the selected object and its format.
                    selected_spreadsheet_roots.push(reference.clone());
                    selected_file_roots.push(reference);
                }
            }
            Ok(PreparationStep::Continue(
                desk_diagnose_core::future::phase(|| async move {
                    let model_name = config.model.clone().unwrap_or_default();
                    let (provider_registry, inventory, _, readiness) =
                        desk_diagnose_core::future::boxed(|| async {
                            desk_diagnose_core::future::boxed(|| {
                                current_capability_projection(
                                    &db,
                                    connections.as_ref(),
                                    (!targetless).then_some(target_connection_id.as_str()),
                                    ModelCapabilities {
                                        image_input: config.supports_image_input,
                                    },
                                )
                            })
                            .await
                        })
                        .await;
                    let readiness_revision = readiness.as_ref().map_or(
                        desk_diagnose_core::web_research::CENTRAL_WEB_READINESS_REVISION,
                        |readiness| readiness.revision,
                    );
                    let provider_registry = if !targetless
                        && let Ok(policy) = desk_diagnose_core::future::boxed(|| async {
                            // A claimed scheduled turn already holds its
                            // session; a conversation not stored yet starts
                            // without directory approvals.
                            let stored = if scheduled.is_some() {
                                None
                            } else {
                                crate::command_policy::stored_session(
                                    &db,
                                    &conversation_id,
                                    &actor_id,
                                    &target_device_id,
                                )
                                .await?
                            };
                            let fresh;
                            let session = match (scheduled.as_ref(), stored.as_deref()) {
                                (Some(resume), _) => &resume.claimed.session,
                                (None, Some(session)) => session,
                                (None, None) => {
                                    fresh = Box::new(
                                        desk_diagnose_core::session::PersistedAgentSession::new(
                                            conversation_id.clone(),
                                            actor_id.clone(),
                                            target_device_id.clone(),
                                            PERSONAL_AGENT_POLICY_REVISION,
                                            AgentScope {
                                                granted: Vec::new(),
                                                mode: ExecutionMode::ReadOnly,
                                                expires_at: None,
                                                policy_name: None,
                                            },
                                            chrono::Utc::now().to_rfc3339(),
                                        ),
                                    );
                                    &*fresh
                                }
                            };
                            crate::command_policy::current(
                                &db,
                                connections.as_ref(),
                                &target_connection_id,
                                &actor_id,
                                session,
                                now_unix_ms,
                            )
                            .await
                        })
                        .await
                    {
                        provider_registry.with_command_policy(policy)
                    } else {
                        provider_registry
                    };
                    let office_selected = ask.selected_capability_ids.iter().any(|capability_id| {
                        capability_id == desk_diagnose_core::ai_agent::OFFICE_DOCUMENT_CAPABILITY_ID
                    });
                    let live_targets = match prepare_live_targets(
                        &provider_registry,
                        &target_device_id,
                        &ask.selected_capability_ids,
                        original_read_context.as_ref(),
                        readiness.as_ref(),
                        now_unix_ms,
                    ) {
                        Ok(targets) => targets,
                        Err(error) => {
                            desk_diagnose_core::future::boxed(|| async {
                                stream_event(
                                    connections.as_ref(),
                                    &browser_connection_id,
                                    &activation_error(
                                        owner_queue.as_deref(),
                                        &request_id,
                                        1,
                                        error,
                                    ),
                                )
                                .await
                            })
                            .await;
                            return Ok(PreparationStep::Ready(None));
                        }
                    };
                    let live_ref = |name: &str| {
                        live_targets
                            .iter()
                            .find(|target| {
                                target.device_id == target_device_id && target.tool_name == name
                            })
                            .map(|target| target.object_ref.clone())
                    };
                    let selected_office_document = live_ref("inspect_office_selection");
                    let selected_live_spreadsheet = live_ref("inspect_live_spreadsheet");
                    let selected_live_document = live_ref("inspect_live_document");
                    let selected_live_presentation = live_ref("inspect_live_presentation");
                    let selected_browser_surface =
                        desk_diagnose_core::ai_agent::browser_surface_context(readiness.as_ref());
                    let selected_outlook_surface = readiness.as_ref().and_then(|readiness| {
                        readiness
                            .context_references
                            .iter()
                            .find(|reference| {
                                reference.capability
                    == desk_agent_protocol::Capability::CommunicationOutlookNewHandoffConfirmed
                            })
                            .map(|reference| reference.object_ref.clone())
                    });
                    if office_selected && selected_office_document.is_none() {
                        desk_diagnose_core::future::boxed(|| async { stream_event(
        connections.as_ref(),
        &browser_connection_id,
        &activation_error(
 owner_queue.as_deref(),
            &request_id,
            1,
            transport_error(
                "the selected Excel document is no longer available; refresh context before sending",
            ),
        ),
    )
    .await }).await;
                        return Ok(PreparationStep::Ready(None));
                    }
                    let selected_permission = targetless
                        .then(|| {
                            let permission_id = resume_conversation_id
                                .as_ref()?
                                .permission_request_id
                                .as_deref()?;
                            snapshot
                                .as_ref()?
                                .permission_requests
                                .iter()
                                .find(|request| request.request_id == permission_id)
                        })
                        .flatten();
                    let (grant_device, grant_readiness_revision) = selected_permission
                        .and_then(|request| {
                            let device = request.selected_device_id()?;
                            let connection = request
                                .selected_device_target()
                                .map(|target| target.connection_id.as_str())
                                .or_else(|| {
                                    request
                                        .exact_command()
                                        .map(|command| command.target_connection_id())
                                })?;
                            Some((device, connection))
                        })
                        .map_or(
                            (target_device_id.as_str(), readiness_revision),
                            |(device, connection)| {
                                let revision =
                                crate::computer_use_readiness::global_computer_use_readiness_cache(
                                )
                                .get_fresh(connection, chrono::Utc::now())
                                .map_or(0, |current| current.readiness.revision);
                                (device, revision)
                            },
                        );
                    let capability_grants = match desk_diagnose_core::future::boxed(|| async {
                        crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
                            .list_for_subject(&conversation_id, &actor_id, grant_device)
                            .await
                    })
                    .await
                    {
                        Ok(grants) => grants,
                        Err(error) => {
                            desk_diagnose_core::future::boxed(|| async {
                                stream_event(
                                    connections.as_ref(),
                                    &browser_connection_id,
                                    &activation_error(
                                        owner_queue.as_deref(),
                                        &request_id,
                                        1,
                                        transport_error(format!(
                                            "failed to load capability grants: {error}"
                                        )),
                                    ),
                                )
                                .await
                            })
                            .await;
                            return Ok(PreparationStep::Ready(None));
                        }
                    };
                    let current_desktop_grants = capability_grants
                        .iter()
                        .filter(|grant| {
                            grant.surface
                == desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner
                && grant.actor_id == actor_id
                && grant.run_id == conversation_id
                && grant.target_device_id == target_device_id
                && grant.policy_revision == PERSONAL_AGENT_POLICY_REVISION
                && grant.readiness_revision == readiness_revision
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    let resumed_desktop_reads = provider_registry
                        .registered_tools()
                        .into_iter()
                        .filter(|tool| {
                            provider_registry
                                .capability_for_tool(tool.name())
                                .is_some_and(|descriptor| {
                                    descriptor.exposure
                        == desk_diagnose_core::tool_exposure::ExposureRequirement::DesktopRead
                                })
                        })
                        .filter(|tool| {
                            has_active_resume_desktop_read_grant(
                                &current_desktop_grants,
                                tool.name(),
                                now_unix_ms,
                            )
                        })
                        .map(|tool| tool.spec.name)
                        .collect::<Vec<_>>();
                    let mut selected_source_tools = ask
                        .selected_capability_ids
                        .iter()
                        .filter_map(|capability_id| provider_registry.capability(capability_id))
                        .map(|capability| capability.wire.tool_name.clone())
                        .collect::<std::collections::BTreeSet<_>>();
                    // Only owner-selected or currently approved desktop sources are eligible
                    // for read-back. Device invocation and model egress remain separate checks.
                    selected_source_tools
                        .extend(resumed_desktop_reads.iter().map(|name| (*name).to_string()));
                    if let Some(request) = selected_permission
                        && let Some(target) = request.selected_device_target()
                        && grant_readiness_revision == target.readiness_revision
                        && matches!(request.state, desk_diagnose_core::dynamic_run::PermissionRequestState::Approved | desk_diagnose_core::dynamic_run::PermissionRequestState::PartiallyApproved)
                    {
                        for item in &request.items {
                            if capability_grants.iter().any(|grant| {
                                grant.grant_id == desk_diagnose_core::permission_grant::permission_item_grant_id(&conversation_id, request, &item.item_id)
                                    && grant.readiness_revision == target.readiness_revision
                                    && grant.revoked_at_unix_ms.is_none() && grant.remaining_uses > 0
                                    && grant.expires_at_unix_ms > now_unix_ms
                            }) { selected_source_tools.insert(item.tool_name.clone()); }
                        }
                    }
                    if !selected_file_roots.is_empty() {
                        selected_source_tools
                            .insert(desk_diagnose_core::tool_names::INSPECT_FILES.into());
                        let selected_file_count = selected_file_roots
                            .iter()
                            .filter(|object_ref| {
                                object_ref.object_kind
                                    == desk_agent_protocol::computer_use::ObjectKind::File
                            })
                            .count();
                        let has_output_directory = selected_file_roots.iter().any(|object_ref| {
                            object_ref.object_kind
                                == desk_agent_protocol::computer_use::ObjectKind::Directory
                        });
                        if selected_file_count > 0 {
                            selected_source_tools
                                .insert(desk_diagnose_core::tool_names::READ_TEXT_FILE.into());
                        }
                        if selected_file_count == 1 {
                            selected_source_tools.extend(
                                [
                                    "inspect_numbers_file",
                                    "inspect_pages_file",
                                    "inspect_keynote_file",
                                    "inspect_powerpoint_file",
                                    "inspect_word_file",
                                    "inspect_excel_cell",
                                ]
                                .into_iter()
                                .map(str::to_string),
                            );
                            if has_output_directory {
                                selected_source_tools.extend(
                                    [
                                        "patch_numbers_copy",
                                        "replace_pages_copy_body",
                                        "patch_keynote_copy",
                                        "patch_powerpoint_copy",
                                        "replace_word_copy_body",
                                        "patch_excel_copy",
                                    ]
                                    .into_iter()
                                    .map(str::to_string),
                                );
                            }
                        }
                        if has_output_directory {
                            selected_source_tools
                                .insert(desk_diagnose_core::tool_names::CREATE_TEXT_FILE.into());
                            selected_source_tools.insert("create_local_message_draft".into());
                        }
                    }
                    if !selected_spreadsheet_roots.is_empty() {
                        selected_source_tools
                            .insert(desk_diagnose_core::tool_names::INSPECT_SPREADSHEETS.into());
                        selected_source_tools.insert(
                            desk_diagnose_core::tool_names::PREVIEW_SPREADSHEET_MERGE.into(),
                        );
                        if selected_file_roots.iter().any(|object_ref| {
                            object_ref.object_kind
                                == desk_agent_protocol::computer_use::ObjectKind::Directory
                        }) {
                            selected_source_tools.insert("create_workbook".into());
                            selected_source_tools.insert("create_formula_workbook".into());
                            selected_source_tools.insert("create_word_report".into());
                        }
                    }
                    if !selected_terminal_roots.is_empty() {
                        selected_source_tools.insert("read_terminal_output".into());
                    }
                    if selected_browser_surface.is_some() {
                        selected_source_tools.extend(
                            desk_diagnose_core::ai_agent::BROWSER_CONTEXT_CAPABILITY_IDS
                                .into_iter()
                                .filter_map(|capability_id| {
                                    provider_registry.capability(capability_id)
                                })
                                .map(|capability| capability.wire.tool_name.clone()),
                        );
                    }
                    if selected_outlook_surface.is_some() {
                        selected_source_tools.insert("prepare_outlook_draft".into());
                    }
                    if selected_live_spreadsheet.is_some() {
                        selected_source_tools.insert("inspect_live_spreadsheet".into());
                        selected_source_tools.insert("patch_live_spreadsheet_cell".into());
                    }
                    if selected_live_document.is_some() {
                        selected_source_tools.insert("inspect_live_document".into());
                        selected_source_tools.insert("replace_live_document_body".into());
                    }
                    if selected_live_presentation.is_some() {
                        selected_source_tools.insert("inspect_live_presentation".into());
                        selected_source_tools.insert("patch_live_presentation_slide".into());
                    }
                    selected_source_tools.insert(
                        desk_diagnose_core::ai_agent::PREVIEW_COMPUTER_ACTION_TOOL.to_string(),
                    );
                    selected_source_tools
                        .insert(crate::web_research::WEB_FETCH_TOOL_NAME.to_string());
                    selected_source_tools
                        .insert(crate::web_research::WEB_SEARCH_TOOL_NAME.to_string());
                    if targetless {
                        selected_source_tools.extend(
                            desk_diagnose_core::request_devices::default_read_source_tools(
                                &provider_registry,
                            )
                            .map(str::to_owned),
                        );
                    } else {
                        selected_source_tools.extend(
                            desk_diagnose_core::ai_agent::SYSTEM_DIAGNOSTIC_TOOL_NAMES
                                .into_iter()
                                .map(str::to_owned),
                        );
                    }
                    selected_source_tools
                        .insert(desk_diagnose_core::exec_tools::EXEC_TOOL_NAME.into());
                    // Candidates only. Directory consent and every read/write grant remain
                    // independently checked against the authoritative conversation.
                    selected_source_tools.extend(
                        [
                            desk_diagnose_core::tool_names::CREATE_TEXT_FILE,
                            "create_local_message_draft",
                            desk_diagnose_core::tool_names::READ_TEXT_FILE,
                            desk_diagnose_core::tool_names::INSPECT_FILES,
                            desk_diagnose_core::provider_preflight::text_file::UPDATE_TEXT_TOOL,
                            desk_diagnose_core::provider_preflight::text_file::DELETE_TEXT_TOOL,
                            desk_diagnose_core::provider_preflight::text_file::RESTORE_TEXT_TOOL,
                            desk_diagnose_core::provider_preflight::text_file::COMMAND_RESTORE_TOOL,
                        ]
                        .into_iter()
                        .map(str::to_string),
                    );
                    selected_source_tools.insert(
                        desk_diagnose_core::ai_agent::EXECUTE_CONFIRMED_UI_ACTION_TOOL.into(),
                    );
                    selected_source_tools.insert(
                        desk_diagnose_core::ai_agent::EXECUTE_CONFIRMED_RAW_INPUT_TOOL.into(),
                    );
                    selected_source_tools
                        .insert(desk_diagnose_core::ai_agent::EXECUTE_BACKGROUND_INPUT_TOOL.into());
                    let turn_id = scheduled
                        .as_ref()
                        .map(|resume| resume.claimed.run.turn_id.clone())
                        .or_else(|| {
                            goal_resume.as_ref().map(|goal| {
                                format!(
                                    "goal:{}:slice:{}",
                                    goal.goal_id,
                                    goal.slice_seq.saturating_add(1)
                                )
                            })
                        })
                        .unwrap_or_else(|| {
                            resume_conversation_id
                                .as_ref()
                                .and_then(|resume| {
                                    resume.permission_request_id.clone().or_else(|| {
                                        resume.decision.as_ref().map(|decision| {
                                            format!(
                                                "decision:{}:{}",
                                                decision.kind.as_str(),
                                                decision.request_id
                                            )
                                        })
                                    })
                                })
                                .map_or_else(
                                    || uuid::Uuid::new_v4().to_string(),
                                    |permission_request_id| {
                                        crate::agent_session_store::permission_resume::turn_id(
                                            &conversation_id,
                                            &permission_request_id,
                                        )
                                    },
                                )
                        });
                    let export_source = if resume_conversation_id.is_some()
                        || scheduled.is_some()
                        || goal_resume.is_some()
                        || delegated.is_some()
                    {
                        crate::agent_model::ModelExportSource::Turn(&turn_id)
                    } else {
                        crate::agent_model::ModelExportSource::Input(&ask.client_message_id)
                    };
                    let export_authorization_id = crate::agent_model::model_export_id(
                        &actor_id,
                        &target_device_id,
                        &conversation_id,
                        export_source,
                    );
                    // A scheduled run reads back the tools approved within this run.
                    if scheduled
                        .as_ref()
                        .is_some_and(|resume| resume.fresh.is_some())
                        && let Some(session) = snapshot.as_ref()
                    {
                        selected_source_tools.extend(
                            session
                                .permission_requests
                                .iter()
                                .filter(|request| {
                                    matches!(
                                        request.state,
                                        desk_diagnose_core::dynamic_run::PermissionRequestState::Approved
                                            | desk_diagnose_core::dynamic_run::PermissionRequestState::PartiallyApproved
                                    )
                                })
                                .flat_map(|request| &request.items)
                                .map(|item| item.tool_name.clone()),
                        );
                    }
                    if let Some(resume) = scheduled.as_mut()
                        && let Some(fresh) = resume.fresh.as_mut()
                    {
                        let store = crate::agent_subagent_store::SubAgentStore::new(db.clone());
                        let lease_owner = resume
                            .claimed
                            .run
                            .lease_owner
                            .as_deref()
                            .ok_or_else(|| transport_error("task executor missing"))?;
                        let (session, creation) = desk_diagnose_core::future::boxed(|| {
                            store.initialize_scheduled_source(
                                &resume.claimed.session,
                                lease_owner,
                                resume.claimed.run.lease_epoch,
                                &destination,
                            )
                        })
                        .await
                        .map_err(|_| transport_error("published task source changed"))?;
                        resume.claimed.session = *session;
                        fresh.creation = Some(creation);
                    }
                    let model = MeteredModel {
                        fresh_task: scheduled.as_ref().and_then(|resume| {
                            resume.fresh.as_ref().map(|fresh| {
                                crate::agent_model::FreshTaskModelContext {
                                    run_id: resume.claimed.run.run_id.clone(),
                                    device_id: target_device_id.clone(),
                                    node_id: resume
                                        .claimed
                                        .run
                                        .lease_owner
                                        .clone()
                                        .unwrap_or_default(),
                                    run_epoch: resume.claimed.run.lease_epoch,
                                    session_token: resume.claimed.session.lease_token,
                                    target_connection_id: target_connection_id.clone(),
                                    connections: connections.clone(),
                                    gate: fresh.gate.clone(),
                                }
                            })
                        }),
                        inner: seam,
                        db: db.clone(),
                        destination: destination.clone(),
                        selected_source_tools,
                        export_authorization_id,
                        permission_resume: resume_conversation_id.is_some() || scheduled.is_some(),
                        completed_compression_receipt: std::cell::RefCell::new(None),
                        model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
                    };
                    Ok(PreparationStep::Continue(
                        desk_diagnose_core::future::phase(|| async move {
                            let callable = callable_tools(&provider_registry, &inventory)
        .expect("static Provider registry must produce complete capability availability");
                            let mut registry = filter_model_compatible_tools(
                                &callable,
                                ModelCapabilities {
                                    image_input: config.supports_image_input,
                                },
                            );
                            let mut selected_tool_capability_ids =
                                ask.selected_capability_ids.clone();
                            if !selected_window_roots.is_empty() {
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::CURRENT_SCREEN_CAPABILITY_ID
                                        .to_string(),
                                );
                            }
                            selected_tool_capability_ids.push(
                                desk_diagnose_core::ai_agent::WEB_RESEARCH_FETCH_CAPABILITY_ID
                                    .into(),
                            );
                            selected_tool_capability_ids.extend(
                                desk_diagnose_core::ai_agent::SYSTEM_DIAGNOSTIC_CAPABILITY_IDS
                                    .into_iter()
                                    .map(str::to_string),
                            );
                            selected_tool_capability_ids.push(
                                desk_diagnose_core::ai_agent::SYSTEM_COMMAND_CAPABILITY_ID.into(),
                            );
                            selected_tool_capability_ids.extend([
        desk_diagnose_core::ai_agent::FILE_ARTIFACT_CREATE_CAPABILITY_ID.into(),
        desk_diagnose_core::ai_agent::LOCAL_COMMUNICATION_DRAFT_CREATE_CAPABILITY_ID.into(),
    ]);
                            if !selected_file_roots.is_empty() {
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::FILE_METADATA_CAPABILITY_ID
                                        .into(),
                                );
                                let selected_file_count = selected_file_roots
                                    .iter()
                                    .filter(|object_ref| {
                                        object_ref.object_kind
                                            == desk_agent_protocol::computer_use::ObjectKind::File
                                    })
                                    .count();
                                let has_output_directory = selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        });
                                if selected_file_count > 0 {
                                    selected_tool_capability_ids.push(
                                        desk_diagnose_core::ai_agent::FILE_CONTENT_CAPABILITY_ID
                                            .into(),
                                    );
                                }
                                if selected_file_count == 1 {
                                    selected_tool_capability_ids.extend(
                [
                    desk_diagnose_core::ai_agent::SPREADSHEET_BATCH_INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::ai_agent::DOCUMENT_BATCH_INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::ai_agent::PRESENTATION_BATCH_INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::ai_agent::windows_excel::INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::ai_agent::windows_word::INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::ai_agent::windows_office::INSPECT_CAPABILITY_ID,
                ]
                .into_iter()
                .map(str::to_string),
            );
                                    if has_output_directory {
                                        selected_tool_capability_ids.extend(
                    [
                        desk_diagnose_core::ai_agent::SPREADSHEET_BATCH_PATCH_CAPABILITY_ID,
                        desk_diagnose_core::ai_agent::DOCUMENT_BATCH_PATCH_CAPABILITY_ID,
                        desk_diagnose_core::ai_agent::PRESENTATION_BATCH_PATCH_CAPABILITY_ID,
                        desk_diagnose_core::ai_agent::windows_excel::PATCH_CAPABILITY_ID,
                        desk_diagnose_core::ai_agent::windows_word::PATCH_CAPABILITY_ID,
                        desk_diagnose_core::ai_agent::windows_office::PATCH_CAPABILITY_ID,
                    ]
                    .into_iter()
                    .map(str::to_string),
                );
                                    }
                                }
                                if has_output_directory {
                                    selected_tool_capability_ids
                .push(desk_diagnose_core::ai_agent::FILE_ARTIFACT_CREATE_CAPABILITY_ID.into());
                                    selected_tool_capability_ids.push(
                desk_diagnose_core::ai_agent::LOCAL_COMMUNICATION_DRAFT_CREATE_CAPABILITY_ID
                    .into(),
            );
                                }
                            }
                            if !selected_spreadsheet_roots.is_empty() {
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::SPREADSHEET_FILE_CAPABILITY_ID
                                        .into(),
                                );
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::SPREADSHEET_MERGE_CAPABILITY_ID
                                        .into(),
                                );
                                if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        }) {
            selected_tool_capability_ids.push(
                desk_diagnose_core::ai_agent::SPREADSHEET_WORKBOOK_CREATE_CAPABILITY_ID.into(),
            );
            selected_tool_capability_ids.push(
                desk_diagnose_core::ai_agent::SPREADSHEET_FORMULA_WORKBOOK_CREATE_CAPABILITY_ID
                    .into(),
            );
            selected_tool_capability_ids
                .push(desk_diagnose_core::ai_agent::WORD_DOCUMENT_CREATE_CAPABILITY_ID.into());
        }
                            }
                            if !selected_terminal_roots.is_empty() {
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::TERMINAL_OUTPUT_CAPABILITY_ID
                                        .into(),
                                );
                            }
                            if selected_browser_surface.is_some() {
                                desk_diagnose_core::ai_agent::extend_browser_context_capability_ids(
                                    &mut selected_tool_capability_ids,
                                );
                            }
                            if selected_outlook_surface.is_some() {
                                selected_tool_capability_ids.push(
                                    desk_diagnose_core::ai_agent::OUTLOOK_NEW_HANDOFF_CAPABILITY_ID
                                        .into(),
                                );
                            }
                            if selected_live_spreadsheet.is_some() {
                                selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::ai_agent::SPREADSHEET_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::ai_agent::SPREADSHEET_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
                            }
                            if selected_live_document.is_some() {
                                selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::ai_agent::DOCUMENT_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::ai_agent::DOCUMENT_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
                            }
                            if selected_live_presentation.is_some() {
                                selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::ai_agent::PRESENTATION_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::ai_agent::PRESENTATION_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
                            }
                            desk_diagnose_core::ai_agent::retain_selected_context_tools(
                                &provider_registry,
                                &mut registry,
                                &selected_tool_capability_ids,
                            );
                            // The frozen read selection of a queued input, read once here.
                            let frozen_selection = if original_read_context.is_none() {
                                main_request::frozen_read_selection(
                                    &db,
                                    owner_queue.as_deref(),
                                    &actor_id,
                                    &conversation_id,
                                    &target_device_id,
                                    &ask,
                                )
                                .await
                                .map_err(|error| error.into_agent_error())?
                            } else {
                                None
                            };
                            if let Some(original) = original_read_context.as_ref() {
                                let recorded = &original.tool_names;
                                // Object reads retain the original selection. Desktop reads remain planning
                                // candidates; only a current owner grant can restore their scope.
                                registry.retain(|tool| {
            tool.effect != desk_diagnose_core::registry::ToolEffect::ReadOnly
                || desk_diagnose_core::ai_agent::is_requestable_desktop_read(tool.name())
                || provider_registry
                    .capability_for_tool(tool.name())
                    .is_some_and(|capability| {
                        capability.exposure
                            == desk_diagnose_core::tool_exposure::ExposureRequirement::NoAttachment
                    })
                || recorded.iter().any(|name| name == tool.name())
        });
                            }
                            // A queued input, with or without a target device, exposes no read
                            // tool its frozen input did not record, so admission and the resume of
                            // an admitted input both compare the read scope that was enqueued.
                            // Desktop reads stay planning candidates; they are not recorded until
                            // an owner grant restores their scope.
                            if matches!(
                                owner_queue.as_deref(),
                                Some(OwnerQueueTurn::Activate { .. })
                            ) && let Some(frozen) = &frozen_selection
                            {
                                registry.retain(|tool| {
                                    exposed_for_frozen_reads(tool, &frozen.tool_names)
                                });
                            }
                            if scheduled
                                .as_ref()
                                .is_some_and(|resume| resume.fresh.is_some())
                            {
                                registry = filter_model_compatible_tools(
                                    &callable,
                                    ModelCapabilities {
                                        image_input: config.supports_image_input,
                                    },
                                );
                            }
                            let capability_catalog_metrics =
                                desk_diagnose_core::permission_tools::capability_catalog_metrics(
                                    &provider_registry,
                                    &inventory,
                                    &registry,
                                    &[],
                                );
                            extend_internal_tools(&mut registry);
                            if targetless {
                                registry.retain(
                                    desk_diagnose_core::agent_policy::is_targetless_web_tool,
                                );
                                if let Some(original) = original_read_context.as_ref() {
                                    registry.retain(|tool| {
                                        !desk_diagnose_core::web_research::is_web_tool(tool.name())
                                            || original
                                                .tool_names
                                                .iter()
                                                .any(|name| name == tool.name())
                                    });
                                }
                            }
                            // A resume after a permission decision rebuilds the
                            // selection from the original input. A capability
                            // that is not ready now (e.g. desktop reads during a
                            // UAC prompt) must not hold the resume: the model
                            // still has to receive the decision.
                            let after_permission_decision = resume_conversation_id
                                .as_ref()
                                .is_some_and(|resume| resume.permission_request_id.is_some());
                            if after_permission_decision {
                                let unready =
                                    desk_diagnose_core::live_context::unready_selected_capabilities(
                                        &inventory,
                                        &ask.selected_capability_ids,
                                    );
                                if !unready.is_empty() {
                                    log::info!(
                                        "[ai-agent] resume request_id={request_id} continues without unready context capabilities {unready:?}"
                                    );
                                }
                            }
                            let context_selection = match context_selection_claim(
                                &provider_registry,
                                &inventory,
                                readiness.as_ref(),
                                &ask.selected_capability_ids,
                                &request_id,
                                &actor_id,
                                &target_device_id,
                                &destination,
                                now_unix_ms,
                                after_permission_decision,
                            ) {
                                Ok(selection) => selection,
                                Err(error) => {
                                    desk_diagnose_core::future::boxed(|| async {
                                        stream_event(
                                            connections.as_ref(),
                                            &browser_connection_id,
                                            &activation_error(
                                                owner_queue.as_deref(),
                                                &request_id,
                                                1,
                                                error,
                                            ),
                                        )
                                        .await
                                    })
                                    .await;
                                    return Ok(PreparationStep::Ready(None));
                                }
                            };
                            let sessions =
                                crate::agent_session_store::SignalAgentSessionStore::new(
                                    db.clone(),
                                )
                                .with_client_metadata(
                                    client_conversation_id.clone(),
                                    AgentSessionSurface::AiAgent,
                                )
                                .with_context_selection(context_selection);
                            let sessions = if let Some(resume) = &resume_conversation_id {
                                let snapshot =
                                    snapshot.as_ref().expect("validated resume snapshot");
                                let sessions =
                                    sessions.with_expected_input_revision(snapshot.input_revision);
                                match &resume.permission_request_id {
                                    Some(permission_request_id) => sessions.with_permission_resume(
                                        permission_request_id.clone(),
                                        snapshot.seq,
                                        capability_grants.clone(),
                                    ),
                                    None if resume.decision.is_some() => {
                                        let decision =
                                            resume.decision.as_ref().expect("checked decision");
                                        sessions.with_decision_resume(
                                            decision.kind,
                                            decision.request_id.clone(),
                                        )
                                    }
                                    None => match &resume.work_event {
                                        Some((_, event_id)) => {
                                            sessions.with_work_event(event_id.clone())
                                        }
                                        None => sessions,
                                    },
                                }
                            } else {
                                sessions
                            };
                            let heartbeat: Box<dyn LeaseHeartbeat> = match scheduled.as_ref() {
                                Some(resume) if resume.fresh.is_some() => Box::new(
                                    crate::schedule_store::ScheduleHeartbeat::new_fresh(
                                        crate::schedule_store::ScheduleStore::new(db.clone()),
                                        crate::schedule_store::FreshTaskLease {
                                            owner: actor_user_id,
                                            run_id: &resume.claimed.run.run_id,
                                            node_id: resume
                                                .claimed
                                                .run
                                                .lease_owner
                                                .as_deref()
                                                .ok_or_else(|| {
                                                    transport_error("task node missing")
                                                })?,
                                            run_epoch: resume.claimed.run.lease_epoch,
                                            session_token: resume.claimed.session.lease_token,
                                        },
                                        resume.lease_seconds,
                                        model_cancel,
                                    )
                                    .await
                                    .map_err(|_| transport_error("task leases changed"))?,
                                ),
                                Some(resume) => Box::new(
                                    crate::schedule_store::ScheduleHeartbeat::new(
                                        crate::schedule_store::ScheduleStore::new(db.clone()),
                                        &resume.claimed,
                                        resume.claimed.run.lease_owner.clone().ok_or_else(
                                            || transport_error("scheduled node missing"),
                                        )?,
                                        resume.lease_seconds,
                                        model_cancel,
                                    )
                                    .await
                                    .map_err(|_| transport_error("scheduled leases changed"))?,
                                ),
                                None => Box::new(SignalStoreHeartbeat {
                                    cancel: model_cancel.clone(),
                                    store: sessions.clone(),
                                }),
                            };
                            let mut granted = Vec::new();
                            desk_diagnose_core::tool_exposure::extend_candidate_scope(
                                &provider_registry,
                                &registry,
                                &selected_tool_capability_ids,
                                &resumed_desktop_reads,
                                &mut granted,
                            );
                            // The sealed command classifier can dispatch the read-only protocol variant.
                            if granted
                                .contains(&desk_agent_protocol::Capability::ShellExecConfirmed)
                            {
                                granted.push(desk_agent_protocol::Capability::ShellExecReadonly);
                            }
                            let mutation_enabled =
                                desk_diagnose_core::tool_exposure::scope_has_mutation(
                                    &provider_registry,
                                    &granted,
                                );
                            let scope = AgentScope {
                                granted,
                                // Even a provider configured for confirmed exec is hard-clamped here.
                                mode: if mutation_enabled {
                                    config
                                        .execution_mode
                                        .restrict_to(ExecutionMode::ConfirmEachAction)
                                } else {
                                    ExecutionMode::ReadOnly
                                },
                                expires_at: original_read_context
                                    .as_ref()
                                    .and_then(|selection| selection.expires_at.clone()),
                                policy_name: Some("oss-ai-agent-provider".into()),
                            };
                            let scope = scheduled
                                .as_ref()
                                .filter(|resume| resume.fresh.is_some())
                                .map_or(scope, |resume| {
                                    resume.claimed.session.scope_snapshot.clone()
                                });
                            if let Some(resume) =
                                scheduled.as_mut().filter(|resume| resume.fresh.is_none())
                            {
                                resume.claimed.session.scope_snapshot =
                                    desk_diagnose_core::session::narrow_scope(
                                        &resume.claimed.session.scope_snapshot,
                                        &scope,
                                    );
                            }
                            let read_context =
                                match crate::agent_run_event_store::ReadContextSelection::capture(
                                    &registry, &scope,
                                ) {
                                    Ok(mut selection) => {
                                        selection.object_attachments = selected_objects;
                                        if target_device_id.is_empty() {
                                            selection.include_selectable_object_tools();
                                        }
                                        let frozen = frozen_selection;
                                        let targets = if target_device_id.is_empty() {
                                            if original_read_context.is_some() {
                                                live_targets
                                            } else if let Some(frozen) = &frozen {
                                                frozen.live_targets.clone()
                                            } else {
                                                capture_selectable_live_targets(
                                                    connections.as_ref(),
                                                    &ask.target_refs,
                                                    &selection.object_attachments,
                                                    &destination,
                                                )
                                                .await?
                                            }
                                        } else {
                                            live_targets
                                        };
                                        desk_diagnose_core::input_read_context::live_read::install_targets(&mut selection, targets)?;
                                        selection.external_issues =
                                            if let Some(original) = &original_read_context {
                                                selection.external_issue_sources =
                                                    original.external_issue_sources.clone();
                                                original.external_issues.clone()
                                            } else if let Some(frozen) = frozen {
                                                selection.external_issue_sources =
                                                    frozen.external_issue_sources;
                                                frozen.external_issues
                                            } else {
                                                crate::issue_connections::select_issues(
                                                    &db,
                                                    &actor_id,
                                                    &ask.selected_issues,
                                                )
                                                .await?
                                            };
                                        selection
                                    }
                                    Err(error) => {
                                        desk_diagnose_core::future::boxed(|| async {
                                            stream_event(
                                                connections.as_ref(),
                                                &browser_connection_id,
                                                &activation_error(
                                                    owner_queue.as_deref(),
                                                    &request_id,
                                                    1,
                                                    error,
                                                ),
                                            )
                                            .await
                                        })
                                        .await;
                                        return Ok(PreparationStep::Ready(None));
                                    }
                                };
                            let clock = || chrono::Utc::now().to_rfc3339();
                            let (available_exec_shells, max_command_runtime_ms) = {
                                let connections = desk_diagnose_core::future::boxed(|| async {
                                    connections.read().await
                                })
                                .await;
                                connections
                                    .get(&target_connection_id)
                                    .map(|target| {
                                        (
                    target.model.exec_capabilities.available_exec_shells.clone(),
                    target
                        .model.exec_capabilities.max_ai_command_runtime_ms
                        .unwrap_or(desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS),
                )
                                    })
                                    .unwrap_or_default()
                            };
                            Ok(PreparationStep::Continue(
                                desk_diagnose_core::future::phase(|| async move {
                                    let tools = crate::remote_tool_edge::SignalAiAgentTools::new(
                                        db.clone(),
                                        provider_registry.clone(),
                                        connections.clone().into_inner(),
                                        crate::remote_tool_edge::global_remote_tool_pending(),
                                        target_connection_id,
                                        target_device_id.clone(),
                                        actor_id.clone(),
                                        config.wire_protocol.map(|value| format!("{value:?}")),
                                        Some(model_name.clone()),
                                        selected_office_document,
                                        selected_live_spreadsheet,
                                        selected_live_document,
                                        selected_live_presentation,
                                        selected_file_roots,
                                        selected_spreadsheet_roots,
                                        selected_terminal_roots,
                                        selected_browser_surface,
                                        selected_outlook_surface,
                                        ask.question.clone(),
                                        conversation_id.clone(),
                                        turn_id.clone(),
                                        PERSONAL_AGENT_POLICY_REVISION,
                                        readiness_revision,
                                        available_exec_shells,
                                        max_command_runtime_ms,
                                    )
                                    .with_model_egress_policy(
                                        model
                                            .model_egress_policy()
                                            .expect("validated model policy"),
                                    );
                                    if (resume_conversation_id.is_some()
                                        || scheduled.is_some()
                                        || goal_resume.is_some()
                                        || delegated
                                            .as_ref()
                                            .and_then(|runtime| runtime.read_context_binding())
                                            .is_some())
                                        && let Some(original) = original_read_context
                                    {
                                        let result = async {
                                            tools.bind_original_input(
                                                snapshot
                                                    .as_ref()
                                                    .expect("validated resume snapshot")
                                                    .input_revision,
                                                original.clone(),
                                                destination.clone(),
                                                client_conversation_id.clone(),
                                            )?;
                                            if !original.object_attachments.is_empty()
                                                || !original.live_targets.is_empty()
                                            {
                                                desk_diagnose_core::future::boxed(|| async {
                                                    tools.validate_original_objects().await
                                                })
                                                .await?;
                                            }
                                            Ok::<_, AgentError>(())
                                        }
                                        .await;
                                        if let Err(error) = result {
                                            desk_diagnose_core::future::boxed(|| async {
                                                stream_event(
                                                    connections.as_ref(),
                                                    &browser_connection_id,
                                                    &activation_error(
                                                        owner_queue.as_deref(),
                                                        &request_id,
                                                        1,
                                                        error,
                                                    ),
                                                )
                                                .await
                                            })
                                            .await;
                                            return Ok(PreparationStep::Ready(None));
                                        }
                                    }
                                    let mut system_prompt =
                                        build_ai_agent_system_message(ask.locale.as_deref());
                                    if scheduled
                                        .as_ref()
                                        .is_some_and(|resume| resume.fresh.is_some())
                                    {
                                        desk_diagnose_core::runtime_context::append_instruction(
            &mut system_prompt,
            &desk_diagnose_core::model_content::ModelContent::constant(
                desk_diagnose_core::schedule::task_prompt::FRESH_TASK_INSTRUCTIONS,
            ),
        );
                                    }
                                    let goal_approval_request_id = goal_resume
                                        .as_ref()
                                        .and_then(|goal| goal.checkpoint.as_ref())
                                        .and_then(|checkpoint| {
                                            checkpoint.evidence_ids.iter().find(|reference| {
                                                checkpoint.summary.strip_prefix("Waiting for ")
                                                    == Some(reference.as_str())
                                                    && snapshot.as_ref().is_some_and(|session| {
                                                        session.permission_decisions.iter().any(
                                                            |decision| {
                                                                decision.request_id
                                                                    == reference.as_str()
                                                            },
                                                        )
                                                    })
                                            })
                                        })
                                        .map(String::as_str);
                                    let decision_request_id = resume_conversation_id
                                        .as_ref()
                                        .and_then(|resume| resume.permission_request_id.as_deref())
                                        .or_else(|| {
                                            scheduled.as_ref()?.permission_request_id.as_deref()
                                        })
                                        .or(goal_approval_request_id);
                                    if let Some(decision_request_id) = decision_request_id {
                                        let decision = desk_diagnose_core::future::boxed(|| async { crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .permission_decision_event(
                crate::agent_session_store::PermissionDecisionSubject {
                    conversation_id: &conversation_id,
                    actor_id: &actor_id,
                    device_id: &target_device_id,
                },
                decision_request_id,
            )
            .await }).await?
            .ok_or_else(|| transport_error("permission decision receipt is unavailable"))?;
                                        desk_diagnose_core::approval_review::defer_denial_notice(
                                            &mut system_prompt,
                                            decision,
                                        )
                                        .map_err(|_| {
                                            transport_error("AI approval denial receipt is invalid")
                                        })?;
                                    }
                                    Ok(PreparationStep::Ready(Some(Box::pin(async move {
                                        let _cancel_registration = cancel_registration;
                                        let deps = LoopDeps {
            session_seam: &sessions,
            model: &model,
            tools: &tools,
            content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
            registry: &registry,
            provider_registry: Some(&provider_registry),
            capability_inventory: Some(&inventory),
            capability_permission_candidates: &[],
            capability_catalog_metrics: Some(capability_catalog_metrics),
            response_format: ResponseFormatSpec::None,
            system_prompt,
            max_steps_per_turn: config.max_steps_per_turn,
            response_locale: ask.locale.clone(),
            interactive_user_home: scheduled
                .is_none()
                .then(|| {
                    readiness
                        .as_ref()
                        .and_then(|state| state.interactive_user_home.as_deref())
                })
                .flatten(),
            interactive_user_home_incarnation: scheduled
                .is_none()
                .then(|| {
                    readiness
                        .as_ref()
                        .map(|state| state.interactive_session_incarnation.as_str())
                })
                .flatten(),
            max_same_tool_per_turn: config.max_same_tool_calls_per_turn,
            clock: &clock,
            heartbeat: Some(heartbeat.as_ref()),
        };
                                        if let Some(runtime) = delegated {
                                            // The loop dependencies stay on the heap rather than
                                            // inside this phase future.
                                            let deps = Box::new(deps);
                                            return desk_diagnose_core::future::phase(|| async move {
                let claim = Box::new(ClaimTurnParams {
                    conversation_id,
                    actor_id,
                    device_id: target_device_id,
                    policy_revision: PERSONAL_AGENT_POLICY_REVISION,
                    current_pdp_scope: scope,
                    turn_id: turn_id.clone(),
                    request_id: Some(request_id.clone()),
                    connection_id: None,
                    trigger_origin: runtime.origin(),
                    now: clock(),
                });
                let store = crate::agent_subagent_store::SubAgentStore::new(db.clone());
                let mut sink =
                    StreamingTurnSink::starting_at(|_event: AiAgentEvent| {}, request_id, 0);
                sink.set_provenance(AiProvenance::stamp(config.model, Some(clock())));
                sink.turn_started(&turn_id);
                // Borrow the boxed turn so its run stays outside this phase future.
                let outcome = match &*runtime {
                    desk_diagnose_core::subagent::runtime::RuntimeTurn::Child { run, .. } => {
                        match desk_diagnose_core::future::boxed(|| {
                            store.claim_child(
                                &claim,
                                &run.binding.task_id,
                                run.fence(),
                                &destination,
                            )
                        })
                        .await
                        .map_err(|error| {
                            transport_error(format!("claim delegated task: {error}"))
                        })? {
                            crate::agent_subagent_store::SubAgentClaimOutcome::Claimed(claimed) => {
                                desk_diagnose_core::future::boxed(|| {
                                    desk_diagnose_core::agent_loop::run_preclaimed_subagent_turn(
                                        &deps,
                                        claimed.session,
                                        &mut sink,
                                    )
                                })
                                .await?
                            }
                            crate::agent_subagent_store::SubAgentClaimOutcome::Blocked(_) => {
                                LoopOutcome::TurnBusy
                            }
                        }
                    }
                    desk_diagnose_core::subagent::runtime::RuntimeTurn::ParentCompletion {
                        ..
                    } => {
                        match desk_diagnose_core::future::boxed(|| {
                            store.claim_parent_completion(&claim, &destination)
                        })
                        .await
                        .map_err(|error| {
                            transport_error(format!(
                                "claim delegated result interpretation: {error}"
                            ))
                        })? {
                            Some(claimed) => desk_diagnose_core::future::boxed(|| {
                                desk_diagnose_core::agent_loop::run_preclaimed_subagent_completion(
                                    &deps,
                                    claimed.session,
                                    &mut sink,
                                )
                            })
                            .await?,
                            None => LoopOutcome::TurnBusy,
                        }
                    }
                };
                sink.finish_outcome(&outcome);
                Ok(Some(outcome))
            })
            .await;
                                        }
                                        if let Some(goal) = goal_resume {
                                            return desk_diagnose_core::future::phase(|| async move {
                let bound = GoalModelBinding::from_destination(&destination)
                    .map_err(|_| model_rejected("goal model binding is unavailable"))?;
                if !goal.model_binding.follows(&bound)
                    || goal.conversation_id != conversation_id
                    || goal.owner_id != actor_id
                    || goal.device_id != target_device_id
                    || snapshot
                        .as_ref()
                        .is_none_or(|session| session.input_revision != goal.input_revision)
                {
                    return Err(model_rejected(
                        "goal continuation is stale or its model changed",
                    ));
                }
                let claim = ClaimTurnParams {
                    conversation_id,
                    actor_id,
                    device_id: target_device_id,
                    policy_revision: PERSONAL_AGENT_POLICY_REVISION,
                    current_pdp_scope: scope,
                    turn_id: turn_id.clone(),
                    request_id: None,
                    connection_id: None,
                    trigger_origin: TriggerOrigin::GoalContinuation,
                    now: clock(),
                };
                let claimed = desk_diagnose_core::future::boxed(|| async { crate::agent_goal_store::claim_slice(
                    &db,
                    &claim,
                    &goal.goal_id,
                    Some(goal.state_version),
                    &turn_id, Some(&bound),
                )
                .await }).await
                .map_err(|error| transport_error(format!("claim AI Agent goal: {error}")))?;
                let Some(claimed) = claimed else {
                    return Ok(Some(LoopOutcome::TurnBusy));
                };
                let mut sink =
                    StreamingTurnSink::starting_at(|_event: AiAgentEvent| {}, request_id, 0);
                sink.set_provenance(AiProvenance::stamp(config.model, Some(clock())));
                sink.turn_started(&turn_id);
                let outcome = desk_diagnose_core::future::boxed(|| {
                    run_preclaimed_goal_slice(&deps, claimed.session, &mut sink)
                })
                .await?;
                sink.finish_outcome(&outcome);
                Ok(Some(outcome))
            })
            .await;
                                        }
                                        if let Some(resume) = scheduled {
                                            return desk_diagnose_core::future::phase(
                                                || async move {
                                                    let resume = *resume;
                                                    let mut sink = StreamingTurnSink::starting_at(
                                                        |_event: AiAgentEvent| {},
                                                        request_id,
                                                        0,
                                                    );
                                                    sink.set_provenance(AiProvenance::stamp(
                                                        config.model,
                                                        Some(clock()),
                                                    ));
                                                    sink.turn_started(&turn_id);
                                                    let outcome =
                                                        desk_diagnose_core::future::boxed(
                                                            || async {
                                                                scheduled_dispatch::drive(
                                                                    &deps,
                                                                    Box::new(
                                                                        resume.claimed.session,
                                                                    ),
                                                                    &resume.claimed.run.run_id,
                                                                    resume
                                                                        .permission_request_id
                                                                        .as_deref(),
                                                                    resume.fresh.as_ref(),
                                                                    &mut sink,
                                                                )
                                                                .await
                                                            },
                                                        )
                                                        .await?;
                                                    sink.finish_outcome(&outcome);
                                                    Ok(Some(outcome))
                                                },
                                            )
                                            .await;
                                        }
                                        let accepted_at = clock();
                                        if resume_conversation_id
                                            .as_ref()
                                            .is_some_and(|resume| !resume.is_decided())
                                        {
                                            let model = &model;
                                            return desk_diagnose_core::future::phase(|| async move {
                                                let policy = model.model_egress_policy()?.ok_or_else(|| {
                                                    transport_error("continuation model policy is unavailable")
                                                })?;
                                                let original = snapshot
                                                    .as_ref()
                                                    .and_then(|session| {
                                                        desk_diagnose_core::permission_resume::latest_user_requirement(
                                                            &session.messages,
                                                        )
                                                    })
                                                    .ok_or_else(|| transport_error("original requirement is unavailable"))?;
                                                let bridge = desk_diagnose_core::permission_resume::authorized_work_completion_resume_message(
                                                    format!("{request_id}-completion"),
                                                    &policy,
                                                    original,
                                                )?;
                                                let claim = ClaimTurnParams {
                                                    conversation_id,
                                                    actor_id,
                                                    device_id: target_device_id,
                                                    policy_revision: PERSONAL_AGENT_POLICY_REVISION,
                                                    current_pdp_scope: scope,
                                                    turn_id: turn_id.clone(),
                                                    request_id: None,
                                                    connection_id: None,
                                                    trigger_origin: TriggerOrigin::WorkContinuation,
                                                    now: accepted_at,
                                                };
                                                let mut sink = StreamingTurnSink::starting_at(
                                                    |_event: AiAgentEvent| {},
                                                    request_id,
                                                    0,
                                                );
                                                sink.set_provenance(AiProvenance::stamp(
                                                    config.model,
                                                    Some(chrono::Utc::now().to_rfc3339()),
                                                ));
                                                sink.turn_started(&turn_id);
                                                let outcome = desk_diagnose_core::future::boxed(|| {
                                                    resume_agent_turn_after_permission(&deps, claim, bridge, &mut sink)
                                                })
                                                .await?;
                                                sink.finish_outcome(&outcome);
                                                Ok(Some(outcome))
                                            })
                                            .await;
                                        }
                                        if resume_conversation_id.is_some() {
                                            let model = &model;
                                            return desk_diagnose_core::future::phase(|| async move {

            let proposal_decision = resume_conversation_id
                .as_ref()
                .is_some_and(|resume| resume.decision.is_some());
            let decision_message = match (|| {
                let policy = model
                    .model_egress_policy()?
                    .ok_or_else(|| transport_error("permission model policy is unavailable"))?;
                if let Some((creation, child)) = permission_child_context.as_deref() {
                    let binding = child.agent_role.binding().ok_or_else(|| transport_error("delegated permission role is unavailable"))?;
                    return if proposal_decision {
                        desk_diagnose_core::permission_resume::authorized_child_decision_resume_message(
                            format!("{request_id}-decision"), &policy, creation, binding,
                        )
                    } else {
                        desk_diagnose_core::permission_resume::authorized_child_permission_resume_message(
                            format!("{request_id}-decision"), &policy, creation, binding,
                        )
                    };
                }
                let original = snapshot
                    .as_ref()
                    .and_then(|session| {
                        desk_diagnose_core::permission_resume::latest_user_requirement(
                            &session.messages,
                        )
                    })
                    .ok_or_else(|| transport_error("original permission input is unavailable"))?;
                if proposal_decision {
                    return desk_diagnose_core::permission_resume::authorized_decision_resume_message(
                        format!("{request_id}-decision"),
                        &policy,
                        original,
                    );
                }
                authorized_permission_resume_message(
                    format!("{request_id}-decision"),
                    &policy,
                    original,
                )
            })() {
                Ok(message) => message,
                Err(error) => {
                    log::warn!("[ai-agent] failed to bind permission resume event: {error:?}");
                    return Ok(None);
                }
            };
            let reply = if let Some(snapshot) = &snapshot {
                desk_diagnose_core::future::boxed(|| main_request::reply_for_input(
                    &db, &actor_id, &conversation_id, snapshot.input_revision,
                )).await.unwrap_or_else(|_| {
                    log::debug!("[ai-agent] permission stream route unavailable");
                    None
                })
            } else {
                None
            };
            let forwarder = if let Some((parent_request, browser)) = reply {
                let (send, mut events) = tokio::sync::mpsc::unbounded_channel::<AiAgentEvent>();
                let connections = connections.clone();
                let session_id = conversation_id.clone();
                let resumed_turn_id = turn_id.clone();
                let task = actix_web::rt::spawn(async move {
                    let mut started = false;
                    while let Some(event) = events.recv().await {
                        if !connections.read().await.get(&browser).is_some_and(|connection| {
                            connection.auth_context.is_browser_user(actor_user_id)
                                && !connection.lifecycle.is_retired()
                        }) {
                            break;
                        }
                        if !started {
                            let mut start = AgentEvent::turn_started(&event.request_id, 0, &resumed_turn_id);
                            start.main_request_id = Some(parent_request.clone());
                            start.main_request_session_id = Some(session_id.clone());
                            stream_event(connections.as_ref(), &browser, &start).await;
                            started = true;
                        }
                        stream_event(connections.as_ref(), &browser, &event).await;
                    }
                });
                Some((send, task))
            } else {
                None
            };
            let (sender, task) = forwarder.unzip();
            let claim = ClaimTurnParams {
                conversation_id,
                actor_id,
                device_id: target_device_id,
                policy_revision: PERSONAL_AGENT_POLICY_REVISION,
                current_pdp_scope: scope,
                turn_id: turn_id.clone(),
                request_id: sender.as_ref().map(|_| request_id.clone()),
                connection_id: None,
                trigger_origin: if proposal_decision {
                    TriggerOrigin::ProposalDecision
                } else {
                    TriggerOrigin::PermissionDecision
                },
                now: accepted_at,
            };
            let mut sink =
                StreamingTurnSink::starting_at(|event: AiAgentEvent| {
                    if let Some(sender) = &sender {
                        let _ = sender.send(event);
                    }
                }, request_id, 1);
            sink.set_provenance(AiProvenance::stamp(
                config.model,
                Some(chrono::Utc::now().to_rfc3339()),
            ));
            match desk_diagnose_core::future::boxed(|| {
                resume_agent_turn_after_permission(&deps, claim, decision_message, &mut sink)
            })
            .await
            {
                Ok(LoopOutcome::TurnBusy) => {
                    // A newer owner follow-up won the claim. Its input supersedes
                    // this grant-triggered resume, so no retry is appropriate.
                }
                Ok(outcome) => {
                    sink.finish_outcome(&outcome);
                }
                Err(error) => {
                    log::warn!(
                        "[ai-agent] permission-resumed turn failed: kind={:?}, retryable={}, message={}",
                        error.kind,
                        error.retryable,
                        error.message
                    );
                    // The forwarder opens the turn before its first event, so
                    // the owner sees the failure even when nothing streamed.
                    sink.error(error);
                }
            }
            drop(sink);
            drop(sender);
            if let Some(task) = task {
                let _ = task.await;
            }
            Ok(None)

}).await;
                                        }
                                        let tools = &tools;
                                        let sessions = &sessions;
                                        desk_diagnose_core::future::phase(|| async move {
        let issue_source = owner_queue.as_deref().and_then(OwnerQueueTurn::issue_source);
        let request_origin = if issue_source.is_some() { TriggerOrigin::IssueSubscription } else { TriggerOrigin::User };
        let user = match if let Some(source) = issue_source {
            source.purpose_message(&destination)
        } else {
            model_bound_user_message(ask.client_message_id.clone(), ask.question, destination.clone())
        } {
            Ok(user) => user,
            Err(error) => {
                desk_diagnose_core::future::boxed(|| async { stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(
 owner_queue.as_deref(),&request_id, 1, error),
                )
                .await }).await;
                return Ok(None);
            }
        };
        let user = Box::new(user);
        if !ask.start_goal && ask.previous_completed_goal_id.is_some() {
            return Err(transport_error(
                "previous completed goal requires start_goal",
            ));
        }
        let goal_start = if ask.start_goal {
            let model_binding = GoalModelBinding::from_destination(&destination)
                .map_err(|_| transport_error("goal model binding is unavailable"))?;
            Some(crate::agent_run_event_store::StartUserGoal {
                goal_id: desk_diagnose_core::native_ids::stable(
                    desk_diagnose_core::native_ids::Kind::Goal,
                    &(&conversation_id, &ask.client_message_id),
                ),
                previous_completed_goal_id: ask.previous_completed_goal_id.clone(),
                model_binding,
            })
        } else {
            None
        };
        let goal_id = goal_start.as_ref().map(|goal| goal.goal_id.clone());
        let input = Box::new(crate::agent_run_event_store::AppendUserFollowupParams {
            target_refs: ask.target_refs,
            client_timezone: ask.client_timezone,
            temporary_conversation: ask.temporary_conversation,
            event_id: ask.client_message_id,
            run_id: conversation_id.clone(),
            client_conversation_id: client_conversation_id.clone(),
            actor_id: actor_id.clone(),
            device_id: target_device_id.clone(),
            surface: AgentSessionSurface::AiAgent,
            policy_revision: PERSONAL_AGENT_POLICY_REVISION,
            current_scope: scope.clone(),
            read_context: Some(read_context.clone()),
            message: *user,
            created_at: accepted_at.clone(),
        });
        let event_store = crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone());
        let admission = owner_queue.as_deref().map_or(OwnerAdmission::Native, OwnerQueueTurn::admission);
        #[cfg(test)]
        activation_faults::before_admission(&request_id).await;
        let append_result = desk_diagnose_core::future::boxed(|| event_store.admit_user_input(&input, goal_start, admission)).await;
        let progress = owner_queue.as_deref().and_then(OwnerQueueTurn::activation_progress);
        let ack = match append_result {
            Ok(OwnerAdmissionResult::Native(ack)) => {
                if let Some(progress) = progress {
                    progress.mark_admitted();
                }
                ack
            }
            Ok(OwnerAdmissionResult::Queued { row, waiting }) => {
                let event = AgentEvent::queued(&request_id, 1, &row.request_id, &row.conversation_id, waiting);
                desk_diagnose_core::future::boxed(|| stream_event(
                    connections.as_ref(), &browser_connection_id,
                    &event,
                )).await;
                main_requests::wake();
                return Ok(Some(LoopOutcome::RequestQueued {
                    request_id: row.request_id,
                    session_id: row.conversation_id,
                    waiting,
                }));
            }
            Ok(OwnerAdmissionResult::Dispatched) => {
                let browser_connection_id = match progress {
                    Some(progress) => {
                        progress.mark_admitted();
                        admitted_browser
                    }
                    None => browser_connection_id,
                };
                desk_diagnose_core::future::boxed(|| async { stream_event(
                    connections.as_ref(), &browser_connection_id,
                    &AgentEvent::status(&request_id, 1, "accepted"),
                ).await }).await;
                return Ok(Some(LoopOutcome::SubAgentYielded));
            }
            Ok(OwnerAdmissionResult::SourceRejected) => {
                // The executor's next source check defers or closes the request.
                if let Some(progress) = progress {
                    progress.mark_source_rejected();
                }
                return Err(main_request::failure_error(
                    desk_agent_protocol::ai_agent::MainRequestFailure::SourceUnavailable,
                ));
            }
            Ok(OwnerAdmissionResult::Waiting) => {
                if let Some(progress) = progress {
                    progress.mark_waiting();
                }
                return Ok(Some(LoopOutcome::TurnBusy));
            }
            Err(error) => {
                if admission.activation().is_some() {
                    return Err(error);
                }
                desk_diagnose_core::future::boxed(|| async { stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &activation_error(
 owner_queue.as_deref(),&request_id, 1, error),
                )
                .await }).await;
                return Ok(None);
            }
        };
        let browser_connection_id = if progress.is_some() {
            admitted_browser
        } else {
            browser_connection_id
        };
        log::info!(
            "[agent-input] accepted request_id={} message_id={} conversation_id={} input_seq={} already_handled={}",
            request_id,
            input.message.message_id,
            conversation_id,
            ack.input_seq,
            ack.already_handled
        );
        desk_diagnose_core::future::boxed(|| async { stream_event(
            connections.as_ref(),
            &browser_connection_id,
            &AgentEvent::status(&request_id, 1, "accepted"),
        )
        .await }).await;
        if let Err(error) = tools.bind_original_input(
            ack.input_revision,
            read_context,
            destination,
            client_conversation_id.clone(),
        ) {
            desk_diagnose_core::future::boxed(|| async { stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &activation_error(
 owner_queue.as_deref(),&request_id, 2, error),
            )
            .await }).await;
            return Ok(None);
        }
        let mut input_sessions = sessions
            .clone()
            .with_expected_input_revision(ack.input_revision);
        if let Some(request_id) = &ack.main_request_id {
            input_sessions = input_sessions.with_queued_input(
                request_id.clone(), input.message.message_id.clone(),
            );
        }
        let deps = LoopDeps {
            session_seam: &input_sessions,
            ..deps
        };
        if ack.already_handled {
            let recovered = desk_diagnose_core::future::boxed(|| event_store.handled_outcome(
                &input,
                &ack,
            )).await;
            let frames = std::cell::RefCell::new(Vec::new());
            {
                let mut replay = StreamingTurnSink::starting_at(
                    |frame| frames.borrow_mut().push(frame), request_id.clone(), 2,
                );
                match recovered {
                    Ok(Some(outcome)) => {
                        if let LoopOutcome::Answered(answer) = &outcome {
                            replay.on_answer_committed(answer);
                        }
                        replay.finish_outcome(&outcome);
                    }
                    Ok(None) => replay.error(transport_error(
                        "the accepted input completion is unavailable; synchronize the conversation",
                    )),
                    Err(error) => replay.error(error),
                }
            }
            for frame in frames.into_inner() {
                desk_diagnose_core::future::boxed(|| stream_event(
                    connections.as_ref(), &browser_connection_id, &frame,
                )).await;
            }
            return Ok(None);
        }
        let claim = ClaimTurnParams {
            conversation_id,
            actor_id,
            device_id: target_device_id,
            policy_revision: PERSONAL_AGENT_POLICY_REVISION,
            current_pdp_scope: scope,
            turn_id: turn_id.clone(),
            request_id: Some(request_id.clone()),
            connection_id: Some(browser_connection_id.clone()),
            trigger_origin: if ask.start_goal {
                TriggerOrigin::GoalContinuation
            } else {
                request_origin
            },
            now: accepted_at,
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AiAgentEvent>();
        let forward_connections = connections.clone();
        let forward_browser = browser_connection_id.clone();
        let forwarder = actix_web::rt::spawn(async move {
            while let Some(event) = desk_diagnose_core::future::boxed(|| async { rx.recv().await }).await {
                desk_diagnose_core::future::boxed(|| async { stream_event(forward_connections.as_ref(), &forward_browser, &event).await }).await;
            }
        });
        let mut sink = StreamingTurnSink::starting_at(
            move |event: AiAgentEvent| {
                let _ = tx.send(event);
            },
            request_id,
            2,
        );
        sink.set_provenance(AiProvenance::stamp(
            config.model,
            Some(chrono::Utc::now().to_rfc3339()),
        ));
        sink.turn_started(&turn_id);
        if ask.start_goal {
            let goal_id = goal_id.as_deref().ok_or_else(|| transport_error("goal id is unavailable"))?;
            let claimed = desk_diagnose_core::future::boxed(|| crate::agent_goal_store::claim_slice(
                &db, &claim, goal_id, None, &turn_id, None,
            )).await
            .map_err(|error| transport_error(format!("claim AI Agent goal: {error}")))?;
            match claimed {
                Some(claimed) => {
                    match desk_diagnose_core::future::boxed(|| {
                        run_preclaimed_goal_slice(&deps, claimed.session, &mut sink)
                    })
                    .await
                    {
                        Ok(outcome) => sink.finish_outcome(&outcome),
                        Err(error) => sink.error(error),
                    }
                }
                None => sink.on_answer_committed(
                    "The goal was queued and will resume when the session is available.",
                ),
            }
            drop(sink);
            let _ = forwarder.await;
            return Ok(None);
        }
        loop {
            match desk_diagnose_core::future::boxed(|| {
                run_agent_turn(&deps, claim.clone(), input.message.clone(), &mut sink)
            })
            .await
            {
                Ok(LoopOutcome::TurnBusy) => {
                    if admission.activation().is_some() {
                        break;
                    }
                    match desk_diagnose_core::future::boxed(|| event_store.handled_outcome(
                        &input,
                        &ack,
                    )).await {
                        Ok(Some(outcome)) => {
                            if let LoopOutcome::Answered(answer) = &outcome {
                                sink.on_answer_committed(answer);
                            }
                            sink.finish_outcome(&outcome);
                            break;
                        }
                        Ok(None) => {
                            // The accepted input remains owned by its existing run.
                            desk_diagnose_core::future::boxed(|| async { tokio::time::sleep(BUSY_FOLLOWUP_RETRY_INTERVAL).await }).await;
                        }
                        Err(error) => {
                            sink.error(error);
                            break;
                        }
                    }
                }
                Ok(outcome) => {
                    sink.finish_outcome(&outcome);
                    break;
                }
                Err(error) => {
                    log::warn!(
                        "[ai-agent] owner turn failed: kind={:?}, retryable={}, message={}",
                        error.kind,
                        error.retryable,
                        error.message
                    );
                    sink.error(error);
                    break;
                }
            }
        }
        drop(sink);
        let _ = forwarder.await;
        Ok(None)
}).await
                                    }))))
                                }),
                            ))
                        }),
                    ))
                }),
            ))
        }),
    ))
}

/// Test-only faults at preparation boundaries that configuration cannot reach.
#[cfg(test)]
pub(crate) mod activation_faults {
    use std::{
        collections::{BTreeMap, BTreeSet},
        future::Future,
        pin::Pin,
        sync::Mutex,
    };

    type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

    static DESTINATION: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    static BEFORE_ADMISSION: Mutex<BTreeMap<String, Hook>> = Mutex::new(BTreeMap::new());

    /// Run `hook` once, after the executor's own checks and right before the
    /// request's native admission.
    pub(crate) fn run_before_admission<F>(
        request_id: &str,
        hook: impl FnOnce() -> F + Send + 'static,
    ) where
        F: Future<Output = ()> + 'static,
    {
        BEFORE_ADMISSION
            .lock()
            .unwrap()
            .insert(request_id.into(), Box::new(move || Box::pin(hook())));
    }

    pub(super) async fn before_admission(request_id: &str) {
        let hook = BEFORE_ADMISSION.lock().unwrap().remove(request_id);
        if let Some(hook) = hook {
            hook().await;
        }
    }

    pub(crate) fn fail_destination(request_id: &str) {
        DESTINATION.lock().unwrap().insert(request_id.into());
    }

    pub(super) fn destination_fails(request_id: &str) -> bool {
        DESTINATION.lock().unwrap().contains(request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::test_timing::{MODEL_IO_TIMEOUT, SCENARIO_TIMEOUT};
    mod compaction;
    mod conversation_title;
    mod desktop_permission;
    mod live_subagents;
    mod original_input;
    mod permission_object;
    mod reply_history;
    mod scheduled;
    mod work_continuation;
    use desk_agent_protocol::data_lineage::Sensitivity;
    use desk_diagnose_core::seam::NullTurnSink;
    use sea_orm::EntityTrait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The model calls tools directly; a call without a grant becomes the
    /// approval request. No tool lets the model request permissions itself.
    #[test]
    fn the_agent_tool_list_has_no_permission_request_tool() {
        let mut registry = Vec::new();
        extend_internal_tools(&mut registry);
        assert!(!registry.is_empty());
        assert!(registry.iter().all(|tool| {
            tool.name() != desk_diagnose_core::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
                && tool.effect != desk_diagnose_core::registry::ToolEffect::PermissionPlanning
        }));
    }
    use tokio::net::TcpListener;

    #[actix_web::test]
    async fn a_child_permission_resume_reaches_its_stored_session_without_a_client_conversation() {
        use desk_diagnose_core::native_ids::{Kind, stable};
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::ai_agent_gate::enable_test_host();
        // No provider request is made before the conversation subject is checked.
        crate::model_provider::save(
            &db,
            crate::model_provider::ModelProviderConfig {
                wire_protocol: Some(
                    desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
                ),
                model: Some("fake-model".into()),
                base_url: Some("http://127.0.0.1:9".into()),
                api_key: Some("test-only-key".into()),
                max_context_bytes: Some(131_072),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let connections = web::Data::new(SharedConnectionMap::default());
        let compose = |resume: Option<PermissionResume>, client: Option<&str>| {
            compose_turn(
                connections.clone(),
                db.clone(),
                "request".into(),
                String::new(),
                String::new(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                String::new(),
                AiAgentAsk {
                    question: "task".into(),
                    client_message_id: "message".into(),
                    conversation_id: client.map(str::to_string),
                    ..Default::default()
                },
                resume,
                None,
                None,
                None,
            )
        };
        let permission = |conversation_id: String| PermissionResume {
            conversation_id,
            permission_request_id: Some("rq_child".into()),
            work_event: None,
            decision: None,
        };
        let input = compose(None, None).await.unwrap_err();
        assert_eq!(
            input.message,
            "personal input requires a valid continuation id"
        );
        // A personal resume still matches its personal conversation key.
        let mismatched = compose(Some(permission("another".into())), Some("thread"))
            .await
            .unwrap_err();
        assert_eq!(
            mismatched.message,
            "permission resume does not match its personal conversation"
        );
        // A delegated child passes both input checks and reaches its stored
        // session, which this empty database does not hold.
        let child = compose(
            Some(permission(stable(Kind::ChildConversation, &"child"))),
            None,
        )
        .await;
        assert_eq!(
            child.unwrap_err().message,
            "permission session is unavailable"
        );
    }

    #[actix_web::test]
    async fn user_cancellation_stops_the_turn_at_tool_boundaries() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let request_id = uuid::Uuid::new_v4().to_string();
        let registration = cancellation::register(17, &request_id);
        let heartbeat = SignalStoreHeartbeat {
            cancel: (*registration).clone(),
            store: crate::agent_session_store::SignalAgentSessionStore::new(db),
        };
        assert!(heartbeat.check_current().await);
        assert!(!cancellation::cancel(18, &request_id));
        assert!(heartbeat.check_current().await);
        assert!(cancellation::cancel(17, &request_id));
        assert!(!heartbeat.check_current().await);
    }

    #[actix_web::test]
    async fn failed_renewal_closes_the_same_cancellation_and_health_state() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
        db.close().await.unwrap();
        let heartbeat = SignalStoreHeartbeat {
            cancel: tokio_util::sync::CancellationToken::new(),
            store,
        };
        tokio::time::pause();
        let _guard = heartbeat.start("closed-database".into(), 1);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(
            crate::agent_session_store::LEASE_TTL_SECS as u64,
        ))
        .await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!heartbeat.check_current().await);
        assert!(heartbeat.cancel.is_cancelled());
    }

    #[test]
    fn permission_resume_bridge_replays_original_requirement_with_user_sensitivity() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway-1".into(),
            connection_revision: 2,
            model_id: "model-1".into(),
            profile_revision: 3,
        };
        let message = model_bound_permission_resume_message(
            "permission-resume-request-1-decision".into(),
            destination.clone(),
            "prepare the exact draft and do not send it",
        )
        .unwrap();

        assert_eq!(message.role, ChatRole::User);
        assert!(message.text.starts_with("AUTOMATIC SERVER CONTROL EVENT"));
        assert!(
            message
                .text
                .contains("prepare the exact draft and do not send it")
        );
        let envelope = message.data_envelope.unwrap();
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.allowed_destinations, vec![destination]);
        assert_eq!(
            envelope.provenance.source_provider_id,
            "agent-runtime-control"
        );
        assert!(envelope.retention.delete_with_run);
        envelope.validate().unwrap();
    }

    #[test]
    fn exact_ui_action_remains_in_scope_for_permission_resume() {
        let mut tools =
            desk_diagnose_core::ai_agent::ai_agent_provider_registry().registered_tools();
        desk_diagnose_core::ai_agent::retain_selected_context_tools(
            &desk_diagnose_core::ai_agent::ai_agent_provider_registry(),
            &mut tools,
            &[],
        );
        let mut granted = desk_diagnose_core::ai_agent::selected_context_capabilities(&[]).unwrap();

        desk_diagnose_core::tool_exposure::extend_candidate_scope(
            &ai_agent_provider_registry(),
            &tools,
            &[],
            &[],
            &mut granted,
        );

        assert!(
            granted.contains(&desk_agent_protocol::Capability::DesktopUiActionConfirmed),
            "an exact approved action must remain callable after its observation attachment expires"
        );
        assert!(
            granted.contains(&desk_agent_protocol::Capability::DesktopInputFallbackConfirmed),
            "an exact approved raw-input fallback must remain callable after its observation attachment expires"
        );
        assert!(
            granted.contains(&desk_agent_protocol::Capability::DesktopBackgroundInputConfirmed)
        );
        assert!(granted.contains(&desk_agent_protocol::Capability::DesktopOutputInputConfirmed));
        assert!(desk_diagnose_core::tool_exposure::scope_has_mutation(
            &ai_agent_provider_registry(),
            &granted
        ));

        assert!(granted.contains(&desk_agent_protocol::Capability::ApplicationLaunchConfirmed));
        tools.clear();
        let mut unavailable = Vec::new();
        desk_diagnose_core::tool_exposure::extend_candidate_scope(
            &ai_agent_provider_registry(),
            &tools,
            &[],
            &[],
            &mut unavailable,
        );
        assert!(unavailable.is_empty());
    }

    #[test]
    fn permission_resume_recognizes_only_active_desktop_read_grants() {
        use desk_agent_protocol::capability_grant::{
            CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer,
            CapabilityGrantLimits, CapabilityGrantUsePolicy, CapabilityRiskTier,
        };
        use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};

        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "grant-ui-read".into(),
            actor_id: "owner".into(),
            run_id: "run".into(),
            input_revision: 1,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device".into(),
            target_session_id: None,
            provider_id: desk_diagnose_core::ai_agent::DESKTOP_UI_PROVIDER_ID.into(),
            capability_id: desk_diagnose_core::ai_agent::DESKTOP_UI_CAPABILITY_ID.into(),
            tool_name: "inspect_desktop_ui".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R1,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: Vec::new(),
            use_policy: CapabilityGrantUsePolicy::Reusable,
            canonical_input_digest_sha256: None,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1_000,
            remaining_uses: 2,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 64 * 1024,
                max_items_per_call: 180,
                max_calls: 4,
            },
            policy_revision: 1,
            readiness_revision: 1,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        };

        let providers = ai_agent_provider_registry();
        for name in [
            "list_applications",
            "inspect_desktop_session",
            "inspect_desktop_ui",
            "read_current_screen",
        ] {
            let descriptor = providers.capability_for_tool(name).unwrap();
            let mut current = grant.clone();
            current.tool_name = name.into();
            current.capability_id = descriptor.wire.capability_id.clone();
            current.provider_id = providers
                .provider_for_capability(&current.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone();
            current.effect = descriptor.wire.effect;
            assert!(has_active_resume_desktop_read_grant(
                std::slice::from_ref(&current),
                name,
                500
            ));
            current.revoked_at_unix_ms = Some(400);
            assert!(!has_active_resume_desktop_read_grant(
                std::slice::from_ref(&current),
                name,
                500
            ));
            current.revoked_at_unix_ms = None;
            current.remaining_uses = 0;
            assert!(!has_active_resume_desktop_read_grant(&[current], name, 500));
        }
        assert!(has_active_resume_desktop_read_grant(
            std::slice::from_ref(&grant),
            "inspect_desktop_ui",
            500
        ));
        let mut exhausted = grant.clone();
        exhausted.remaining_uses = 0;
        assert!(!has_active_resume_desktop_read_grant(
            &[exhausted],
            "inspect_desktop_ui",
            500
        ));
        assert!(!has_active_resume_desktop_read_grant(
            &[grant],
            "inspect_desktop_ui",
            1_000
        ));
    }

    async fn capture_one_openai_request_with_sse(
        listener: impl std::borrow::Borrow<TcpListener>,
        sse: &str,
    ) -> Vec<u8> {
        capture_one_openai_request_with_reply(listener, |_| sse.to_owned()).await
    }

    async fn capture_openai_answer_with_title(
        listener: impl std::borrow::Borrow<TcpListener>,
        sse: &str,
    ) -> (Vec<u8>, Vec<u8>) {
        let answer = capture_one_openai_request_with_sse(listener.borrow(), sse).await;
        let title = capture_one_openai_request_with_reply(listener.borrow(), |body| {
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert!(
                request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Create a short conversation title"))
            );
            assert!(
                request["tools"]
                    .as_array()
                    .is_none_or(|tools| tools.is_empty())
            );
            original_input::memory_sse(
                serde_json::json!({"content":r#"{"title":"Synthetic conversation"}"#}),
                "stop",
            )
        })
        .await;
        (answer, title)
    }

    async fn capture_one_openai_request_with_reply(
        listener: impl std::borrow::Borrow<TcpListener>,
        reply: impl FnOnce(&[u8]) -> String,
    ) -> Vec<u8> {
        capture_one_openai_request_with_reply_paused(listener, reply, None).await
    }

    async fn capture_one_openai_request_with_reply_paused(
        listener: impl std::borrow::Borrow<TcpListener>,
        reply: impl FnOnce(&[u8]) -> String,
        pause: Option<(
            tokio::sync::oneshot::Sender<Vec<u8>>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    ) -> Vec<u8> {
        let (mut socket, _) = listener.borrow().accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(
                read > 0,
                "fake gateway connection closed before request body"
            );
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .expect("awc send_json must use a bounded Content-Length");
                break (header_end + 4, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fake gateway connection closed mid-body");
            request.extend_from_slice(&buffer[..read]);
        }

        let body = &request[header_end..header_end + content_length];
        if let Some((arrived, release)) = pause {
            arrived.send(body.to_vec()).unwrap();
            release.await.unwrap();
        }
        let sse = reply(body);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
            sse.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        request[header_end..header_end + content_length].to_vec()
    }

    async fn capture_one_openai_request(listener: TcpListener) -> Vec<u8> {
        capture_one_openai_request_with_sse(
            listener,
            concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"captured-ok\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
            ),
        )
        .await
    }

    fn selected_tool_envelope(content: &str) -> DataEnvelope {
        let digest_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "selected-tool-result".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "selected-tool-result-content".into(),
                sha256: digest_sha256.clone(),
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "windows-uia".into(),
                source_tool_name: "inspect_desktop_ui".into(),
                source_object_id: Some("snapshot-1".into()),
                source_envelope_ids: Vec::new(),
            },
            sensitivity: Sensitivity::Sensitive,
            // A read result has no implicit model destination. MeteredModel must
            // mint the exact ExportData projection before provider I/O.
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    #[actix_web::test]
    async fn compression_gateway_binds_original_durable_receipt() {
        use desk_diagnose_core::model_context::*;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = actix_web::rt::spawn(async move {
            let answer = r#"{"goals":[{"text":"Earlier goal","source_message_ids":["old"]}]}"#;
            let sse = format!(
                "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":answer}}]})
            );
            capture_one_openai_request_with_sse(listener, &sse).await
        });
        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::ai_agent_gate::enable_test_host();
        crate::model_provider::save(&db, config.clone())
            .await
            .unwrap();
        let config = crate::model_provider::load(&db).await.unwrap();
        crate::context_management_config::update(&db, &desk_signal_facade::context_management::UpdateContextManagementRequest {
            compression_history_target_percent: 50,
expected_revision: 0, summary_max_output_tokens: 4096, strategy: desk_signal_facade::context_management::ContextManagementStrategyDto::CheckpointSummary,
        }).await.unwrap();
        let model = MeteredModel {
            fresh_task: None,
            inner: SignalModelSeam::from_config(&config)
                .unwrap()
                .with_context_db(db.clone()),
            db: db.clone(),
            destination: config.destination_identity().unwrap(),
            selected_source_tools: Default::default(),
            export_authorization_id: "original-compression-export".into(),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };
        let context = model
            .context_policy(desk_diagnose_core::model_capability::ModelRequirements::TEXT_ONLY)
            .await
            .unwrap();
        let policy = model.model_egress_policy().unwrap().unwrap();
        let conversation = [("old", 70_000), ("recent", 68_000)]
            .into_iter()
            .map(|(id, size)| {
                model_bound_user_message(id.into(), "x".repeat(size), policy.destination.clone())
                    .unwrap()
                    .with_turn_id(format!("turn-{id}"))
            })
            .collect::<Vec<_>>();
        let state = ModelContextState::default();
        let ContextBuildPlan::NeedsCompression(plan) = plan_model_context(
            &conversation,
            &state,
            &context,
            &ContextProtectionSet::default(),
            7,
        )
        .unwrap() else {
            panic!("compression expected");
        };
        let input = authorize_compression_input(&policy, &plan, &conversation).unwrap();
        let mut request = ModelRequest::text_only(input.messages.clone(), ResponseFormatSpec::None);
        request.use_case = desk_diagnose_core::model_profile::ModelUseCase::ContextCompression;
        assert!(
            model
                .context_compression_provenance("current", "2026-09-07T00:00:00Z")
                .is_err()
        );
        let turn = model.call(request, &mut NullTurnSink).await.unwrap();
        let provenance = model
            .context_compression_provenance("current", "2026-09-07T00:00:00Z")
            .unwrap();
        let mut summary = parse_validated_context_summary(&turn.text, &plan, provenance).unwrap();
        bind_context_summary_lineage(&policy, &mut summary, &turn, &input).unwrap();
        let (next, _) =
            apply_validated_checkpoint(&plan, summary, &conversation, &state, 7).unwrap();
        let restored: ModelContextState =
            serde_json::from_str(&serde_json::to_string(&next).unwrap()).unwrap();
        let trace = &restored.entries[0]
            .checkpoint
            .as_ref()
            .unwrap()
            .v1()
            .lineage
            .as_ref()
            .unwrap()
            .derivations[0];
        let inputs = crate::model_egress_store::SignalModelEgressStore::read_compression_inputs_on(
            &db,
            &model.export_authorization_id,
            trace,
        )
        .await
        .unwrap();
        assert!(
            inputs
                .iter()
                .any(|input| input.envelope_id == trace.compression_input.envelope_id)
        );
        for case in 0..3 {
            let mut changed = trace.clone();
            match case {
                0 => changed.compressor.provider_call_key = "a".repeat(64),
                1 => changed.export_authorization_id = "another-export".into(),
                _ => changed.model_output.envelope_id = "model-output-other".into(),
            }
            assert!(
                crate::model_egress_store::SignalModelEgressStore::read_compression_inputs_on(
                    &db,
                    &model.export_authorization_id,
                    &changed
                )
                .await
                .is_err()
            );
        }
        assert!(
            crate::model_egress_store::SignalModelEgressStore::read_compression_inputs_on(
                &db,
                "another-owner-export",
                trace
            )
            .await
            .is_err()
        );
        // A later rejected compression must not reuse the previous success key.
        let mut denied = ModelRequest::text_only(
            vec![ChatMessage::text(
                "unlabeled",
                ChatRole::User,
                "not authorized",
            )],
            ResponseFormatSpec::None,
        );
        denied.use_case = desk_diagnose_core::model_profile::ModelUseCase::ContextCompression;
        assert!(model.call(denied, &mut NullTurnSink).await.is_err());
        assert!(
            model
                .context_compression_provenance("current", "2026-09-07T00:00:00Z")
                .is_err()
        );
        let body = String::from_utf8(capture.await.unwrap()).unwrap();
        assert!(body.contains("summarize_prefix"));
        assert!(!body.contains("data_envelope"));
    }

    #[actix_web::test]
    async fn fake_gateway_captures_only_authorized_envelopes_and_persists_receipt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = actix_web::rt::spawn(capture_one_openai_request(listener));

        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::ai_agent_gate::enable_test_host();
        crate::model_provider::save(&db, config.clone())
            .await
            .unwrap();
        let config = crate::model_provider::load(&db).await.unwrap();
        let destination = config.destination_identity().unwrap();
        let model = MeteredModel {
            fresh_task: None,
            inner: SignalModelSeam::from_config(&config)
                .unwrap()
                .with_context_db(db.clone()),
            db: db.clone(),
            destination: destination.clone(),
            selected_source_tools: ["inspect_desktop_ui".to_string()].into_iter().collect(),
            export_authorization_id: crate::agent_model::model_export_id(
                "1",
                "device",
                "conversation",
                crate::agent_model::ModelExportSource::Input("user-message-1"),
            ),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };

        let removed_attachment_marker = "removed-marker-is-not-attached";
        let user = model_bound_user_message(
            "user-message-1".into(),
            "inspect the selected context".into(),
            destination,
        )
        .unwrap();
        let selected_content = r#"{\"window\":\"Excel\",\"button\":\"Help\"}"#;
        let mut tool = ChatMessage::tool_result("tool-1", "call-1", selected_content);
        tool.data_envelope = Some(selected_tool_envelope(selected_content));
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::public_system("system", "trusted system prompt"),
                user,
                tool,
            ],
            ResponseFormatSpec::None,
        );
        let mut history = desk_diagnose_core::session::PersistedAgentSession::new(
            "conversation",
            "1",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            chrono::Utc::now().to_rfc3339(),
        );
        history.conversation = request.messages.clone();
        history
            .conversation
            .iter_mut()
            .for_each(|message| message.turn_id = Some("initial-turn".into()));
        let mut sink = NullTurnSink;
        let turn = model.call(request, &mut sink).await.unwrap();
        assert_eq!(turn.text, "captured-ok");

        let body = capture.await.unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("selected context"));
        assert!(body.contains("Excel"));
        assert!(!body.contains(removed_attachment_marker));
        assert!(!body.contains("data_envelope"));
        assert!(!body.contains("allowed_destinations"));
        assert!(!body.contains("test-only-key"));

        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        let lineage: Vec<desk_diagnose_core::model_egress::ModelInputLineage> =
            serde_json::from_str(&receipts[0].input_lineage_json).unwrap();
        assert_eq!(lineage.len(), 3);
        assert!(lineage[0].public_system_prompt);
        assert!(!lineage[1].public_system_prompt);
        assert!(!lineage[2].public_system_prompt);
        let original = selected_tool_envelope(selected_content);
        let exported = lineage
            .iter()
            .find(|entry| entry.source_tool_name == "inspect_desktop_ui")
            .unwrap();
        assert_eq!(
            exported.source_envelope_ids.as_slice(),
            std::slice::from_ref(&original.envelope_id)
        );
        assert_eq!(exported.size_bytes, original.content.size_bytes());

        let output = turn.provider_meta.data_envelope.as_ref().unwrap();
        assert_eq!(
            receipts[0].model_output_digest_sha256.as_deref(),
            output.content_sha256()
        );

        let mut stored_message = ChatMessage::text(
            "stored-answer",
            desk_diagnose_core::chat::ChatRole::Assistant,
            turn.text.clone(),
        );
        stored_message.data_envelope = Some(output.clone());
        let receipt = &receipts[0];
        let observed = crate::model_egress_store::SignalModelEgressStore::read_output_evidence_on(
            &db,
            &receipt.receipt_id,
            &receipt.export_authorization_id,
            1,
            &stored_message,
        )
        .await
        .unwrap();
        assert_eq!(observed, lineage);

        assert!(
            crate::model_egress_store::SignalModelEgressStore::read_output_evidence_on(
                &db,
                &receipt.receipt_id,
                "other-run-export",
                1,
                &stored_message,
            )
            .await
            .is_err()
        );
        assert!(
            crate::model_egress_store::SignalModelEgressStore::read_output_evidence_on(
                &db,
                &receipt.receipt_id,
                &receipt.export_authorization_id,
                2,
                &stored_message,
            )
            .await
            .is_err()
        );
        stored_message.text.push_str("tampered");
        assert!(
            crate::model_egress_store::SignalModelEgressStore::read_output_evidence_on(
                &db,
                &receipt.receipt_id,
                &receipt.export_authorization_id,
                1,
                &stored_message,
            )
            .await
            .is_err()
        );
        assert_eq!(
            crate::entity::model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap(),
            receipts
        );
        for input in &lineage {
            assert!(
                output
                    .provenance
                    .source_envelope_ids
                    .contains(&input.envelope_id)
            );
        }
        assert!(!receipts[0].input_lineage_json.contains("selected context"));
        assert!(!receipts[0].input_lineage_json.contains("test-only-key"));
        assert!(
            !receipts[0]
                .input_lineage_json
                .contains(removed_attachment_marker)
        );

        assert_eq!(
            serde_json::from_str::<desk_diagnose_core::chat::TokenUsage>(
                receipts[0].usage_json.as_deref().unwrap()
            )
            .unwrap(),
            turn.usage
        );
        assert_eq!(
            receipts[0].state,
            crate::model_egress_store::STATE_SUCCEEDED
        );
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&receipts[0].envelope_ids_json)
                .unwrap()
                .len(),
            3
        );
    }

    #[actix_web::test]
    async fn empty_gateway_end_turn_reaches_bounded_agent_loop_recovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = actix_web::rt::spawn(capture_one_openai_request_with_sse(
            listener,
            concat!(
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":0}}\n\n",
                "data: [DONE]\n\n"
            ),
        ));
        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        crate::ai_agent_gate::enable_test_host();
        crate::model_provider::save(&db, config.clone())
            .await
            .unwrap();
        let config = crate::model_provider::load(&db).await.unwrap();
        let destination = config.destination_identity().unwrap();
        let model = MeteredModel {
            fresh_task: None,
            inner: SignalModelSeam::from_config(&config)
                .unwrap()
                .with_context_db(db.clone()),
            db: db.clone(),
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "empty-http-export".into(),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::public_system("system", "trusted system prompt"),
                model_bound_user_message(
                    "user-message-empty".into(),
                    "continue".into(),
                    destination,
                )
                .unwrap(),
            ],
            ResponseFormatSpec::None,
        );
        let turn = model.call(request, &mut NullTurnSink).await.unwrap();
        assert!(turn.text.is_empty());
        assert!(turn.tool_calls.is_empty());
        assert_eq!(
            turn.stop_reason,
            desk_diagnose_core::chat::StopReason::EndTurn
        );
        let _ = capture.await.unwrap();
        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            serde_json::from_str::<desk_diagnose_core::chat::TokenUsage>(
                receipts[0].usage_json.as_deref().unwrap()
            )
            .unwrap(),
            turn.usage
        );
        assert_eq!(receipts[0].state, crate::model_egress_store::STATE_FAILED);
        assert!(receipts[0].model_output_envelope_id.is_none());
    }
}
