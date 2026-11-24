mod issue_reads;

use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_agent_data_store::{
    AgentStoreError,
    main_request::coordination::{self, Batch, ReportReadScope, Reservation},
};
use desk_diagnose_core::request_devices::RequestDevice;
use desk_diagnose_core::{chat::ModelTurn, seam::ModelRequest};
use sea_orm::sea_query::Expr;

use crate::agent_subagent_store::notification::{
    batch_report_context_on, batch_report_read_scope_on,
};
use std::collections::BTreeMap;

async fn report_candidates_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    head: &agent_main_request::Model,
    now_ms: i64,
) -> Result<Vec<coordination::ReportCandidate>, AgentError> {
    let Some(workbench) = session.agent_role.workbench_id() else {
        return Ok(Vec::new());
    };
    let frozen = FrozenRequestInput::decode(head).map_err(|error| error.into_agent_error())?;
    let mut reports = Vec::new();
    for row in desk_agent_data_store::worker_report::batch_candidates_on(
        db,
        workbench,
        &session.actor_id,
        now_ms,
    )
    .await
    .map_err(storage)?
    {
        if let Some(scope) =
            batch_report_read_scope_on(db, &row, workbench, &frozen.destination, now_ms)
                .await
                .map_err(storage)?
        {
            reports.push(coordination::ReportCandidate {
                row,
                work_version: scope.work_version,
            });
        }
    }
    Ok(reports)
}

async fn current_report_scopes(
    db: &impl sea_orm::ConnectionTrait,
    batch: &Batch,
    workbench: &str,
    now_ms: i64,
) -> Result<BTreeMap<i64, ReportReadScope>, AgentError> {
    let mut current = BTreeMap::new();
    for report in batch.reports() {
        if coordination::leased_report_on(db, report, now_ms)
            .await
            .map_err(|error| error.into_agent_error())?
            && let Some(scope) =
                batch_report_read_scope_on(db, &report.row, workbench, &batch.destination, now_ms)
                    .await
                    .map_err(storage)?
            && scope.work_version == report.work_version
        {
            current.insert(report.row.id, scope);
        }
    }
    Ok(current)
}

pub(super) async fn close_unclaimed(
    db: &DatabaseConnection,
    head: &agent_main_request::Model,
    execution: &str,
    device: &str,
    reason: MainRequestFailure,
) -> Result<(), AgentError> {
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .map_err(storage)?;
    let Some((row, mut session)) = load_session(&txn, execution, &head.actor_id, device).await?
    else {
        return Ok(());
    };
    if main_request::fail_unclaimed(
        &txn,
        head,
        &mut session,
        desk_agent_data_store::datetime::utc_now(),
        reason,
    )
    .await
    .map_err(|error| error.into_agent_error())?
    {
        store_session(&txn, &row, &mut session).await?;
    }
    txn.commit().await.map_err(storage)
}

pub(crate) async fn fail_destination(
    db: &DatabaseConnection,
    actor: &str,
    conversation: &str,
    execution: &str,
    request: &str,
    claim: Option<i64>,
) -> Result<(), AgentError> {
    let Some(row) = main_request::load(db, actor, conversation, request)
        .await
        .map_err(|error| error.into_agent_error())?
    else {
        return Ok(());
    };
    if row.pending_input_json.is_none() {
        return Ok(());
    }
    // A first activation closes only the row it still holds; a newer attempt
    // may have taken it over or already admitted it.
    if claim.is_some_and(|claim| row.input_revision.is_some() || row.activation_claim != claim) {
        return Ok(());
    }
    if row.input_revision.is_none() {
        return main_request::fail_unactivated(
            db,
            &row,
            claim,
            jiff::Timestamp::now().as_millisecond(),
            MainRequestFailure::DestinationChanged,
        )
        .await
        .map_err(|error| error.into_agent_error());
    }
    let frozen = FrozenRequestInput::decode(&row).map_err(|error| error.into_agent_error())?;
    close_unclaimed(
        db,
        &row,
        execution,
        &frozen.device_id,
        MainRequestFailure::DestinationChanged,
    )
    .await
}

impl SignalMainRequestExecutor {
    pub(super) async fn coordinate(
        &self,
        head: &agent_main_request::Model,
        device: &str,
        owner: i32,
        claim: Option<i64>,
    ) -> Result<bool, AgentError> {
        let (batch, request) = match self.reserve_batch(head, device, claim).await? {
            Reservation::Reserved(batch) => *batch,
            Reservation::Unbatched => return Ok(false),
            // The rolled-back round ends here; the next scan starts afresh.
            Reservation::Changed => return Ok(true),
        };
        let cancel = cancellation::register(owner, &batch.id);
        let result = tokio::time::timeout(
            Duration::from_secs(coordination::MODEL_TIMEOUT_SECS),
            self.call_batch(head, &batch, request, (*cancel).clone()),
        )
        .await
        .ok()
        .and_then(Result::ok);
        self.finish_batch(head, device, &batch, result.as_ref())
            .await?;
        Ok(true)
    }

    async fn reserve_batch(
        &self,
        head: &agent_main_request::Model,
        device: &str,
        claim: Option<i64>,
    ) -> Result<Reservation, AgentError> {
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(storage)?;
        let Some((row, mut session)) =
            load_session(&txn, &head.conversation_id, &head.actor_id, device).await?
        else {
            return Ok(Reservation::Unbatched);
        };
        if session.agent_role.workbench_id().is_some()
            && !agent_run_event_store::main_requests::ready_on(
                &txn,
                &session,
                jiff::Timestamp::now().as_millisecond(),
            )
            .await?
        {
            return Ok(Reservation::Unbatched);
        }
        if agent_run_event_store::main_requests::slot_held(&txn, &session).await? {
            return Ok(Reservation::Unbatched);
        }
        let now_ms = jiff::Timestamp::now().as_millisecond();
        let reports = report_candidates_on(&txn, &session, head, now_ms).await?;
        let disabled_skills = crate::agent_skills::disabled_bundled(&txn).await;
        let reservation = coordination::reserve(
            &txn,
            &mut session,
            head,
            now_ms,
            reports,
            &disabled_skills,
            claim,
        )
        .await
        .map_err(|error| error.into_agent_error())?;
        if matches!(reservation, Reservation::Reserved(_)) {
            store_session(&txn, &row, &mut session).await?;
            txn.commit().await.map_err(storage)?;
        }
        Ok(reservation)
    }

    async fn finish_batch(
        &self,
        head: &agent_main_request::Model,
        device: &str,
        batch: &Batch,
        turn: Option<&ModelTurn>,
    ) -> Result<(), AgentError> {
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(storage)?;
        let Some((row, mut session)) =
            load_session(&txn, &head.conversation_id, &head.actor_id, device).await?
        else {
            return Ok(());
        };
        let now_ms = jiff::Timestamp::now().as_millisecond();
        if !batch.is_current(&session, now_ms) {
            return Ok(());
        }
        if self.gate.is_enabled()
            && !agent_run_event_store::main_requests::slot_held(&txn, &session).await?
        {
            let current = current_report_scopes(&txn, batch, &head.conversation_id, now_ms)
                .await?
                .into_keys()
                .collect();
            coordination::settle(&txn, batch, turn, now_ms, &current)
                .await
                .map_err(|error| error.into_agent_error())?;
        }
        session.main_coordination = None;
        store_session(&txn, &row, &mut session).await?;
        txn.commit().await.map_err(storage)
    }

    async fn call_batch(
        &self,
        head: &agent_main_request::Model,
        batch: &Batch,
        request: ModelRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ModelTurn, AgentError> {
        let config = crate::model_provider::load(&self.db)
            .await
            .map_err(storage)?;
        if config.destination_identity().map_err(|_| unavailable())? != batch.destination {
            return Err(unavailable());
        }
        let inner = SignalModelSeam::from_config(&config)?
            .with_context_db(self.db.clone())
            .with_cancellation(cancel);
        let model = MeteredModel {
            fresh_task: None,
            inner,
            db: self.db.clone(),
            destination: batch.destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: batch.id.clone(),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };
        batch
            .route(
                &model,
                &self.db,
                request,
                async || self.check_batch(head, batch).await,
                async |row, scope, selector, byte_limit| {
                    batch_report_context_on(
                        &self.db,
                        row,
                        scope,
                        selector,
                        &batch.destination,
                        byte_limit,
                    )
                    .await
                },
                coordination::RequestReadPorts {
                    devices: async |row: &agent_main_request::Model, input: &FrozenRequestInput| {
                        self.pending_devices(row, input).await
                    },
                    issues: &issue_reads::Reader {
                        executor: self,
                        head,
                        batch,
                    },
                },
            )
            .await
    }

    async fn pending_devices(
        &self,
        row: &agent_main_request::Model,
        input: &FrozenRequestInput,
    ) -> Result<Option<Vec<RequestDevice>>, AgentStoreError> {
        if row.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
            || input
                .targets
                .iter()
                .any(|target| target.reference != target.device_id)
        {
            return Ok(None);
        }
        let ids = input
            .targets
            .iter()
            .map(|target| target.device_id.clone())
            .collect::<Vec<_>>();
        crate::remote_tool_edge::request_device_catalogue(&self.db, &self.connections, &ids)
            .await
            .map(Some)
            .map_err(|_| AgentStoreError::Inaccessible)
    }

    async fn check_batch(
        &self,
        head: &agent_main_request::Model,
        batch: &Batch,
    ) -> Result<BTreeMap<i64, ReportReadScope>, AgentError> {
        let snapshot = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&head.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&head.actor_id))
            .one(&self.db)
            .await
            .map_err(storage)?
            .ok_or_else(unavailable)?;
        let session = PersistedAgentSession::decode_boxed_json(&snapshot.state_json)
            .map_err(|_| unavailable())?;
        if !self.gate.is_enabled()
            || !batch.is_current(&session, jiff::Timestamp::now().as_millisecond())
        {
            return Err(unavailable());
        }
        let reports = current_report_scopes(
            &self.db,
            batch,
            &head.conversation_id,
            jiff::Timestamp::now().as_millisecond(),
        )
        .await?;
        if reports.len() != batch.reports().len() {
            return Err(unavailable());
        }
        Ok(reports)
    }
}

async fn load_session(
    txn: &DatabaseTransaction,
    conversation: &str,
    actor: &str,
    device: &str,
) -> Result<Option<(agent_session::Model, Box<PersistedAgentSession>)>, AgentError> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation))
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::DeviceId.eq(device))
        .one(txn)
        .await
        .map_err(storage)?;
    row.map(|row| {
        let session =
            PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| unavailable())?;
        if session.version != row.version
            || session.lease_token as i64 != row.lease_token
            || session.check_subject(actor, device).is_err()
        {
            return Err(unavailable());
        }
        Ok((row, session))
    })
    .transpose()
}

async fn store_session(
    txn: &DatabaseTransaction,
    row: &agent_session::Model,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    use agent_session::Column;
    session.version = row.version.checked_add(1).ok_or_else(unavailable)?;
    let changed = agent_session::Entity::update_many()
        .col_expr(Column::Version, Expr::value(session.version))
        .col_expr(Column::LeaseToken, Expr::value(session.lease_token as i64))
        .col_expr(
            Column::StateJson,
            Expr::value(
                session
                    .encode_json_for_storage()
                    .map_err(|_| unavailable())?,
            ),
        )
        .filter(Column::Id.eq(row.id))
        .filter(Column::Version.eq(row.version))
        .filter(Column::LeaseToken.eq(row.lease_token))
        .filter(Column::StateJson.eq(&row.state_json))
        .exec(txn)
        .await
        .map_err(storage)?;
    if changed.rows_affected != 1 {
        return Err(unavailable());
    }
    Ok(())
}

fn unavailable() -> AgentError {
    transport_error("Queued agent coordination is unavailable")
}

fn storage(_: sea_orm::DbErr) -> AgentError {
    unavailable()
}
