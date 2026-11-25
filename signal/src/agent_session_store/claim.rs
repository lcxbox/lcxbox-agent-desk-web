//! Compare-and-swap attempts for existing and newly created conversations.
use super::*;

impl SignalAgentSessionStore {
    pub(super) async fn claim_existing(
        &self,
        row: agent_session::Model,
        params: &ClaimTurnParams,
        now: sea_orm::prelude::DateTimeUtc,
    ) -> Result<Option<Box<PersistedAgentSession>>, ClaimError> {
        if matches!(
            params.trigger_origin,
            desk_diagnose_core::session::TriggerOrigin::WorkCompletion { .. }
                | desk_diagnose_core::session::TriggerOrigin::WorkContinuation
        ) && crate::agent_goal_store::load_latest_for_subject(
            &self.db,
            &params.conversation_id,
            &params.actor_id,
            &params.device_id,
        )
        .await
        .map_err(|e| ClaimError::Backend(internal(format!("load goal for completion: {e}"))))?
        .is_some_and(|goal| !goal.state.is_terminal())
        {
            // Goal continuations own background results. Every goal
            // transition also changes the session version, so the
            // session CAS below closes the read/claim race.
            return Err(ClaimError::Busy);
        }
        let mut session =
            PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|e| {
                ClaimError::Backend(internal(format!("decode agent session state: {e}")))
            })?;
        session.version = row.version;
        session
            .check_subject(&params.actor_id, &params.device_id)
            .map_err(ClaimError::Subject)?;
        session
            .check_surface(self.surface)
            .map_err(ClaimError::Subject)?;
        session
            .validate_request_origin(params.trigger_origin)
            .map_err(|error| ClaimError::Backend(internal(error)))?;
        if session.agent_role.workbench_id().is_some()
            && params.trigger_origin.is_request_input()
            && (session.agent_role.workbench_request_id().is_none()
                || session.agent_role.workbench_request_id()
                    != self.queued_input.as_ref().map(|(id, _)| id.as_str()))
        {
            return Err(ClaimError::Backend(internal(
                "workbench claim source changed",
            )));
        }
        if self.surface == AgentSessionSurface::AiAgent
            && params.device_id.is_empty()
            && (!session.has_admitted_personal_input()
                || self.expected_input_revision != Some(session.input_revision))
        {
            return Err(ClaimError::Backend(internal(
                "targetless turn requires its admitted input revision",
            )));
        }
        if self
            .expected_input_revision
            .is_some_and(|revision| revision != session.input_revision)
        {
            return Err(ClaimError::Backend(internal(
                "prepared input was superseded before the turn claim",
            )));
        }
        if self.queued_input.is_some() && session.main_stopped {
            return Err(ClaimError::Busy);
        }
        // A paused turn resumes only through its decision.
        if session.decision_pause.is_some() && self.decision_resume.is_none() {
            return Err(ClaimError::Busy);
        }
        if let Some((kind, request_id)) = &self.decision_resume {
            desk_diagnose_core::agent_policy::validate_decision_resume_claim(
                &session,
                params,
                *kind,
                request_id,
                self.expected_input_revision.ok_or(ClaimError::Busy)?,
            )
            .map_err(|_| ClaimError::Busy)?;
        } else if let (Some(event_id), Some(revision)) =
            (&self.work_event, self.expected_input_revision)
        {
            desk_diagnose_core::agent_policy::validate_personal_work_claim(
                &session, params, revision, event_id,
            )
            .map_err(|_| ClaimError::Busy)?;
        } else {
            desk_diagnose_core::agent_policy::validate_claim(
                self.surface,
                Some(session.policy_revision),
                params,
            )
            .map_err(ClaimError::Backend)?;
        }
        session.adopt_client_metadata(self.client_conversation_id.as_deref(), self.surface);
        self.reconcile_context_selection(&mut session)
            .map_err(ClaimError::Backend)?;

        if session.turn_state.is_active() {
            let lease_live = row.lease_deadline.is_some_and(|d| d >= now);
            if lease_live {
                return Err(ClaimError::Busy);
            }
            recover_dangling_calls(&self.db, &mut session, &params.now)
                .await
                .map_err(ClaimError::Backend)?;
        }
        if matches!(
            session.begin_turn(
                params.turn_id.clone(),
                params.request_id.clone(),
                params.connection_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            ),
            Err(TurnClaimError::Busy)
        ) {
            return Err(ClaimError::Busy);
        }
        session.adopt_trigger(params.trigger_origin, &params.turn_id);
        if self.decision_resume.is_some() {
            desk_diagnose_core::owner_question::claim_resume(&mut session);
        }
        if let Some((_, message_id)) = &self.queued_input {
            if !params.trigger_origin.is_request_input()
                || self.expected_input_revision != Some(session.input_revision)
            {
                return Err(ClaimError::Busy);
            }
            let message = session
                .conversation
                .iter_mut()
                .find(|message| {
                    message.message_id == *message_id
                        && message.role == desk_diagnose_core::chat::ChatRole::User
                })
                .ok_or(ClaimError::Busy)?;
            if message.turn_id.is_some() {
                return Err(ClaimError::Busy);
            }
            message.turn_id = Some(params.turn_id.clone());
        }
        let old_version = row.version;
        let new_version = old_version + 1;
        session.version = new_version;
        let state_json = session.encode_json_for_storage().map_err(|e| {
            ClaimError::Backend(internal(format!("encode agent session state: {e}")))
        })?;
        let update = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(state_json))
            .col_expr(agent_session::Column::Version, Expr::value(new_version))
            .col_expr(
                agent_session::Column::LeaseToken,
                Expr::value(session.lease_token as i64),
            )
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(
                    now + desk_agent_data_store::datetime::database_interval(
                        jiff::SignedDuration::from_secs(LEASE_TTL_SECS),
                    ),
                )),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(old_version));
        let result = if let Some((request_id, message_id)) = &self.queued_input {
            let txn = crate::db::begin_write(&self.db, agent_session::Entity)
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            crate::issue_connections::check_work_on(
                &txn,
                &session,
                false,
                jiff::Timestamp::now().as_millisecond(),
            )
            .await
            .map_err(|error| ClaimError::Backend(error.into_agent_error()))?;
            if let Some(workbench_id) = session.agent_role.workbench_id()
                && !crate::agent_run_event_store::main_requests::claim_workbench_on(
                    &txn,
                    &session.actor_id,
                    workbench_id,
                    request_id,
                    now.timestamp_millis(),
                )
                .await
                .map_err(ClaimError::Backend)?
            {
                return Err(ClaimError::Busy);
            }
            let result = update
                .exec(&txn)
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            if result.rows_affected == 1
                && !desk_agent_data_store::main_request::mark_claimed(
                    &txn,
                    request_id,
                    &session,
                    message_id,
                    now.timestamp_millis(),
                )
                .await
                .map_err(|error| ClaimError::Backend(error.into_agent_error()))?
            {
                return Err(ClaimError::Busy);
            }
            txn.commit()
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            result
        } else if let Some((kind, request_id)) = &self.decision_resume {
            // The resume record leaves `pending` exactly once.
            let txn = crate::db::begin_write(&self.db, agent_session::Entity)
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            // A child resumes only while its task source still admits it.
            if session.agent_role.binding().is_some()
                && !crate::agent_subagent_store::child_resume_admitted_on(
                    &txn,
                    &session,
                    now.timestamp_millis(),
                )
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?
            {
                return Err(ClaimError::Busy);
            }
            let record = desk_agent_data_store::decision_resume::find_on(&txn, *kind, request_id)
                .await
                .map_err(|error| ClaimError::Backend(internal(format!("{error:?}"))))?
                .filter(|record| record.conversation_id == session.conversation_id)
                .ok_or(ClaimError::Busy)?;
            if !desk_agent_data_store::decision_resume::transition_on(
                &txn,
                record.id,
                desk_agent_data_store::decision_resume::PENDING,
                desk_agent_data_store::decision_resume::STARTED,
                now.timestamp_millis(),
            )
            .await
            .map_err(|error| ClaimError::Backend(internal(format!("{error:?}"))))?
            {
                return Err(ClaimError::Busy);
            }
            let result = update
                .exec(&txn)
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            if result.rows_affected != 1 {
                return Err(ClaimError::Busy);
            }
            txn.commit()
                .await
                .map_err(|error| ClaimError::Backend(internal(error.to_string())))?;
            result
        } else {
            update
                .exec(&self.db)
                .await
                .map_err(|e| ClaimError::Backend(internal(format!("claim agent session: {e}"))))?
        };
        if result.rows_affected == 1 {
            return Ok(Some(session));
        }
        Ok(None)
    }

    pub(super) async fn claim_new(
        &self,
        params: &ClaimTurnParams,
        now: sea_orm::prelude::DateTimeUtc,
    ) -> Result<Option<Box<PersistedAgentSession>>, ClaimError> {
        desk_diagnose_core::agent_policy::validate_claim(self.surface, None, params)
            .map_err(ClaimError::Backend)?;
        if self.expected_input_revision.is_some()
            || (self.surface == AgentSessionSurface::AiAgent && params.device_id.is_empty())
        {
            return Err(ClaimError::Backend(internal(
                "prepared input session is missing",
            )));
        }
        let mut session = Box::new(PersistedAgentSession::new(
            params.conversation_id.clone(),
            params.actor_id.clone(),
            params.device_id.clone(),
            params.policy_revision,
            params.current_pdp_scope.clone(),
            params.now.clone(),
        ));
        session.adopt_client_metadata(self.client_conversation_id.as_deref(), self.surface);
        self.reconcile_context_selection(&mut session)
            .map_err(ClaimError::Backend)?;
        let _ = session.begin_turn(
            params.turn_id.clone(),
            params.request_id.clone(),
            params.connection_id.clone(),
            params.policy_revision,
            params.current_pdp_scope.clone(),
            params.now.clone(),
        );
        session.adopt_trigger(params.trigger_origin, &params.turn_id);
        let state_json = session.encode_json_for_storage().map_err(|e| {
            ClaimError::Backend(internal(format!("encode agent session state: {e}")))
        })?;
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(|error| {
                ClaimError::Backend(internal(format!("create conversation: {error}")))
            })?;
        if crate::agent_subagent_store::deleted_on(&txn, &params.conversation_id)
            .await
            .map_err(|_| ClaimError::Backend(internal("conversation deletion state unavailable")))?
        {
            return Err(ClaimError::Backend(internal("conversation was deleted")));
        }
        let inserted = agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            conversation_schema_version: Set(Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            ))),
            state_json: Set(state_json),
            version: Set(0),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(Some(
                now + desk_agent_data_store::datetime::database_interval(
                    jiff::SignedDuration::from_secs(LEASE_TTL_SECS),
                ),
            )),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await;
        let inserted = match inserted {
            Ok(value) => {
                txn.commit().await.map_err(|error| {
                    ClaimError::Backend(internal(format!("create conversation: {error}")))
                })?;
                Ok(value)
            }
            Err(error) => {
                txn.rollback().await.map_err(|rollback| {
                    ClaimError::Backend(internal(format!("create conversation: {rollback}")))
                })?;
                Err(error)
            }
        };
        match inserted {
            Ok(_) => Ok(Some(session)),
            Err(_)
                if find(&self.db, &params.conversation_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some() =>
            {
                Ok(None)
            }
            Err(e) => Err(ClaimError::Backend(internal(format!(
                "create agent session: {e}"
            )))),
        }
    }
}
