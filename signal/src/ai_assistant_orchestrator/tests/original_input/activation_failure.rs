//! A queued request whose first activation ends before native admission is
//! retried with bounded backoff and shown as failed, never streamed.

use super::*;
use crate::entity::agent_main_request as request;
use desk_agent_protocol::ai_assistant::{MainRequestFailure, MainRequestState};
use sea_orm::{ColumnTrait, ConnectionTrait, QueryFilter, sea_query::Expr};

struct Host {
    db: DatabaseConnection,
    listener: std::sync::Arc<TcpListener>,
    connections: web::Data<SharedConnectionMap>,
    executor: main_requests::SignalMainRequestExecutor,
    _directory: tempfile::TempDir,
}

async fn host(model: &str) -> Host {
    crate::ai_assistant_gate::enable_test_host();
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("activation.db").display()
    );
    let db = crate::config::test_support::Database::connect(&url)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    save_model(&db, &listener, Some(model)).await;
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let gate = std::sync::Arc::new(crate::ai_assistant_gate::AiAssistantGate::new(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    ));
    let executor =
        main_requests::SignalMainRequestExecutor::new(db.clone(), connections.clone(), gate);
    Host {
        db,
        listener,
        connections,
        executor,
        _directory: directory,
    }
}

async fn save_model(db: &DatabaseConnection, listener: &TcpListener, model: Option<&str>) {
    crate::model_provider::save(
        db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: model.map(str::to_owned),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

impl Host {
    async fn submit(&self, message_id: &str, question: &str) -> request::Model {
        submit_owner_turn(
            web::Data::new(SharedConnectionMap::new_oss()),
            self.db.clone(),
            format!("submit-{message_id}"),
            "closed-browser".into(),
            String::new(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            String::new(),
            AiAssistantAsk {
                question: question.into(),
                client_message_id: message_id.into(),
                conversation_id: Some("activation-conversation".into()),
                client_timezone: "UTC".into(),
                ..Default::default()
            },
        )
        .await;
        request::Entity::find()
            .filter(request::Column::ClientMessageId.eq(message_id))
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn reload(&self, row: &request::Model) -> request::Model {
        request::Entity::find_by_id(row.id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// The submitting browser is connected and authenticated as the owner.
    async fn attach_browser(&self, row: &request::Model) -> actix_web::HttpResponse {
        use desk_signal_facade::model::{
            auth_context::AuthContext, os::OperationSystemEnum, signal::RemoteDeskTypeEnum,
        };
        let peer = register_request_device(
            &self.connections,
            "activation-browser",
            "Browser",
            OperationSystemEnum::Mac,
        )
        .await;
        let mut connections = self.connections.write().await;
        let mut browser = connections.get("live-activation-browser").unwrap().clone();
        browser.auth_context = AuthContext::cookie(
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            RemoteDeskTypeEnum::Browser,
        );
        connections.insert("live-activation-browser".into(), browser);
        drop(connections);
        request::Entity::update_many()
            .col_expr(
                request::Column::ReplyConnectionId,
                Expr::value("live-activation-browser"),
            )
            .filter(request::Column::Id.eq(row.id))
            .exec(&self.db)
            .await
            .unwrap();
        peer
    }

    async fn scan(&self) {
        tokio::time::timeout(SCENARIO_TIMEOUT, self.executor.scan_once(0))
            .await
            .unwrap()
            .unwrap();
    }

    /// Stand in for the elapsed backoff of every queued row.
    async fn elapse_backoff(&self) {
        request::Entity::update_many()
            .col_expr(request::Column::ActivationRetryAtMs, Expr::value(1_i64))
            .filter(request::Column::ActivationRetryAtMs.is_not_null())
            .exec(&self.db)
            .await
            .unwrap();
    }

    async fn elapse_coordination_retry(&self) {
        request::Entity::update_many()
            .col_expr(request::Column::RetryAtMs, Expr::value(1_i64))
            .filter(request::Column::RetryAtMs.is_not_null())
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Run one owner request to its answer so the conversation has a session.
    async fn answer_first(&self) {
        let first = self.submit("first-input", "Answer the first input").await;
        let listener = self.listener.clone();
        let answer = actix_web::rt::spawn(async move {
            let sse = memory_sse(serde_json::json!({"content":"First answer"}), "stop");
            capture_openai_answer_with_title(listener.as_ref(), &sse).await
        });
        self.scan().await;
        tokio::time::timeout(MODEL_IO_TIMEOUT, answer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(self.reload(&first).await.state, "activated");
        assert!(
            main_request::recovery_heads(&self.db, 0, 4)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

async fn no_frame(peer: actix_web::HttpResponse) {
    let mut body = peer.into_body();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            receive_device_frame_from(&mut body),
        )
        .await
        .is_err(),
        "a withheld activation sends no browser frame"
    );
}

#[actix_web::test]
async fn activation_without_a_model_backs_off_silently_and_fails_with_a_generic_reason() {
    let host = host("activation-model").await;
    let row = host.submit("no-model", "Needs a model").await;
    let peer = host.attach_browser(&row).await;
    save_model(&host.db, &host.listener, None).await;
    for attempt in 1..main_request::MAX_ACTIVATION_ATTEMPTS {
        host.scan().await;
        let saved = host.reload(&row).await;
        assert_eq!(saved.state, main_request::QUEUED);
        assert_eq!(saved.activation_attempts, attempt);
        assert!(saved.activation_retry_at_ms.is_some());
        assert!(saved.pending_input_json.is_some());
        // The backoff holds the row until it elapses.
        host.scan().await;
        assert_eq!(host.reload(&row).await.activation_attempts, attempt);
        host.elapse_backoff().await;
    }
    host.scan().await;
    let failed = host.reload(&row).await;
    assert_eq!(failed.state, "failed");
    assert_eq!(
        failed.activation_attempts,
        main_request::MAX_ACTIVATION_ATTEMPTS
    );
    let [receipt]: [_; 1] = main_request::receipts(
        &host.db,
        &row.actor_id,
        &row.conversation_id,
        Some(&row.request_id),
    )
    .await
    .unwrap()
    .try_into()
    .unwrap();
    assert_eq!(receipt.state, MainRequestState::Failed);
    assert_eq!(receipt.failure, Some(MainRequestFailure::ActivationFailed));
    assert!(receipt.error_code.is_none());
    host.scan().await;
    assert_eq!(host.reload(&row).await, failed, "no further execution");
    no_frame(peer).await;
}

#[actix_web::test]
async fn activation_records_a_failed_destination_resolution() {
    let host = host("activation-model").await;
    let row = host.submit("destination", "Resolve the destination").await;
    crate::ai_assistant_orchestrator::activation_faults::fail_destination(&row.request_id);
    host.scan().await;
    let saved = host.reload(&row).await;
    assert_eq!(saved.state, main_request::QUEUED);
    assert_eq!(saved.activation_attempts, 1);
    assert!(saved.activation_retry_at_ms.is_some());
}

#[actix_web::test]
async fn a_turn_that_fails_after_admission_keeps_its_session_failure() {
    let host = host("activation-model").await;
    let row = host.submit("admitted", "Fail after admission").await;
    let peer = host.attach_browser(&row).await;
    // The model endpoint refuses connections once the input is admitted.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    crate::model_provider::save(
        &host.db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("activation-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    host.scan().await;
    let saved = host.reload(&row).await;
    assert_eq!(saved.state, "activated");
    assert_eq!(saved.activation_attempts, 0);
    assert!(saved.activation_retry_at_ms.is_none());
    let mut body = peer.into_body();
    let frame = tokio::time::timeout(MODEL_IO_TIMEOUT, receive_device_frame_from(&mut body))
        .await
        .unwrap();
    assert_eq!(frame.request_id, row.request_id);
}

/// Another instance changed the second request between the batch read and its
/// reservation: the round rolls back and charges nothing.
#[actix_web::test]
async fn a_changed_batch_ends_the_round_without_charging_the_head() {
    let host = host("activation-model").await;
    host.answer_first().await;
    let b = host.submit("batch-b", "Coordinate B").await;
    let c = host.submit("batch-c", "Coordinate C").await;
    host.db
        .execute_unprepared(&format!(
            "CREATE TRIGGER concurrent_activation_failure AFTER UPDATE OF coordination_attempts \
             ON agent_main_request WHEN NEW.id = {} BEGIN UPDATE agent_main_request \
             SET activation_attempts = activation_attempts + 1 WHERE id = {}; END",
            b.id, c.id
        ))
        .await
        .unwrap();
    host.scan().await;
    for row in [&b, &c] {
        let saved = host.reload(row).await;
        assert_eq!(saved.state, main_request::QUEUED);
        assert_eq!(saved.coordination_attempts, 0);
        assert_eq!(saved.activation_attempts, 0);
        assert!(saved.activation_retry_at_ms.is_none());
    }
    host.db
        .execute_unprepared("DROP TRIGGER concurrent_activation_failure")
        .await
        .unwrap();
}

/// A coordination model that keeps failing spends coordination attempts only.
#[actix_web::test]
async fn persistent_coordination_failure_exhausts_coordination_not_activation() {
    let host = host("activation-model").await;
    host.answer_first().await;
    let b = host.submit("exhaust-b", "Coordinate B").await;
    let c = host.submit("exhaust-c", "Coordinate C").await;
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let db = host.db.clone();
    // The same model at a refusing endpoint keeps the queued destination.
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("activation-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for attempt in 1..=3 {
        host.scan().await;
        let saved = host.reload(&b).await;
        assert_eq!(saved.coordination_attempts, attempt);
        assert_eq!(saved.activation_attempts, 0);
        host.elapse_coordination_retry().await;
    }
    for row in [&b, &c] {
        let [receipt]: [_; 1] = main_request::receipts(
            &host.db,
            &row.actor_id,
            &row.conversation_id,
            Some(&row.request_id),
        )
        .await
        .unwrap()
        .try_into()
        .unwrap();
        assert_eq!(
            receipt.failure,
            Some(MainRequestFailure::CoordinationExhausted)
        );
        assert_eq!(host.reload(row).await.activation_attempts, 0);
    }
}

/// An error before the attempt registers the request's call token is retried
/// on the next scan without charging the request.
#[actix_web::test]
async fn an_error_before_the_call_registration_is_not_charged() {
    use sea_orm::{ActiveModelTrait, Set};
    let host = host("activation-model").await;
    let row = host.submit("unregistered", "Check readiness first").await;
    let now = chrono::Utc::now();
    let session = crate::entity::agent_session::ActiveModel {
        conversation_id: Set(row.conversation_id.clone()),
        actor_id: Set(row.actor_id.clone()),
        device_id: Set(String::new()),
        state_json: Set("{}".into()),
        conversation_schema_version: Set(None),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&host.db)
    .await
    .unwrap();
    for _ in 0..main_request::MAX_ACTIVATION_ATTEMPTS {
        host.scan().await;
    }
    let saved = host.reload(&row).await;
    assert_eq!(saved.state, main_request::QUEUED);
    assert_eq!(saved.activation_attempts, 0);
    assert!(saved.activation_retry_at_ms.is_none());
    crate::entity::agent_session::Entity::delete_by_id(session.id)
        .exec(&host.db)
        .await
        .unwrap();
}
