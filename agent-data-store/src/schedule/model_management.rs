//! Model management is confined to the current conversation and server origin.
use super::ScheduleStoreError;
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::model_content::{ModelContent, ServerId, Stored};
use desk_diagnose_core::{
    model_observability::{
        InputIssue, PermissionOutcome, Stage, StageOutcome, tool::ToolObservation,
    },
    schedule::management_tools::{Action, proposed_ids},
    session::PersistedAgentSession,
};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use serde_json::Value;

fn scope(owner: i32, session: &PersistedAgentSession) -> Condition {
    let ids = proposed_ids(session);
    let mut origin =
        Condition::any().add(entity::Column::SourceConversationId.eq(&session.conversation_id));
    if !ids.is_empty() {
        origin = origin.add(
            Condition::all()
                .add(entity::Column::CreationSource.eq("ai_proposal"))
                .add(entity::Column::ScheduleId.is_in(ids)),
        );
    }
    let mut subject = Condition::all()
        .add(entity::Column::OwnerUserId.eq(owner))
        .add(origin);
    if !session.device_id.is_empty() {
        subject = subject.add(entity::Column::TargetDeviceId.eq(&session.device_id));
    }
    subject
}

/// Acquire existing task before the conversation row, just like the dispatcher.
pub async fn lock_target<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    session: &PersistedAgentSession,
    action: &Action,
) -> Result<Option<entity::Model>, ScheduleStoreError> {
    let Action::Cancel { schedule_id, .. } = action else {
        return Ok(None);
    };
    let task = entity::Entity::find()
        .filter(scope(owner, session))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?;
    if let Some(task) = task
        .as_ref()
        .filter(|task| task.creation_source == "ai_proposal")
    {
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(task)
}

desk_diagnose_core::model_receipt! {
    /// The task fields a model turn sees in management replies. The title and
    /// rule keep the proofs of whoever wrote them.
    pub struct TaskView {
        schedule_id: ServerId,
        title: Stored<String>,
        kind: ServerId,
        state: ServerId,
        revision: i64,
        creation_source: ServerId,
        can_cancel: bool,
        next_run_at_utc_ms: Option<i64>,
        active_run_id: Option<ServerId>,
        rule: Option<Stored<Value>>,
    }
}

/// The view of `task`, its stored text bound to `destinations`.
pub fn view(
    task: &entity::Model,
    destinations: &[DestinationIdentity],
) -> Result<TaskView, ScheduleStoreError> {
    use super::field_sources::{RULE, TITLE, root};
    let rule = serde_json::from_str::<Value>(&task.spec_json)
        .ok()
        .map(|spec| spec["rule"].clone());
    Ok(TaskView {
        schedule_id: ServerId::new(&task.schedule_id),
        title: Stored::new(
            task.title.clone(),
            root(task, TITLE, &task.title, destinations.to_vec())?,
        ),
        kind: ServerId::new(&task.kind),
        state: ServerId::new(&task.status),
        revision: task.revision,
        creation_source: ServerId::new(&task.creation_source),
        can_cancel: task.creation_source == "ai_proposal"
            && !matches!(task.status.as_str(), "deleted" | "completed" | "expired"),
        next_run_at_utc_ms: task.next_run_at,
        active_run_id: task.active_run_id.as_deref().map(ServerId::new),
        rule: rule
            .map(|rule| {
                let text = rule.to_string();
                root(task, RULE, &text, destinations.to_vec()).map(|root| Stored::new(rule, root))
            })
            .transpose()?,
    })
}

desk_diagnose_core::model_receipt! {
    struct TaskList {
        ok: bool,
        tasks: Vec<TaskView>,
        next_after: Option<i64>,
        observed_at_utc_ms: i64,
    }
}

desk_diagnose_core::model_receipt! {
    struct CancelOutcome {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<TaskView>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'static str>,
    }
}

desk_diagnose_core::model_receipt! {
    struct Cancelled {
        ok: bool,
        schedule_id: ServerId,
        revision: i64,
        state: &'static str,
        future_scheduling_stopped: bool,
        effects_undone: bool,
        run_id: Option<ServerId>,
        run_state: Option<ServerId>,
        message: &'static str,
    }
}

fn refused(
    reason: &'static str,
    task: Option<TaskView>,
    message: Option<&'static str>,
) -> Result<ModelContent, ScheduleStoreError> {
    ModelContent::json(&CancelOutcome {
        ok: false,
        reason: Some(reason),
        state: None,
        task,
        message,
    })
    .map_err(|_| ScheduleStoreError::Invalid)
}

pub async fn list<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    session: &PersistedAgentSession,
    after: i64,
    limit: u64,
    now: i64,
    destinations: &[DestinationIdentity],
) -> Result<ModelContent, ScheduleStoreError> {
    let mut rows = entity::Entity::find()
        .filter(scope(owner, session))
        .filter(entity::Column::Id.gt(after))
        .order_by_asc(entity::Column::Id)
        .limit(limit + 1)
        .all(txn)
        .await?;
    let more = rows.len() > limit as usize;
    if more {
        rows.pop();
    }
    let next = more.then(|| rows.last().map(|row| row.id)).flatten();
    ModelContent::json(&TaskList {
        ok: true,
        tasks: rows
            .iter()
            .map(|row| view(row, destinations))
            .collect::<Result<_, _>>()?,
        next_after: next,
        observed_at_utc_ms: now,
    })
    .map_err(|_| ScheduleStoreError::Invalid)
}

pub async fn cancel<C: ConnectionTrait>(
    txn: &C,
    task: Option<entity::Model>,
    expected: i64,
    now: i64,
    observation: &ToolObservation,
    destinations: &[DestinationIdentity],
) -> Result<ModelContent, ScheduleStoreError> {
    let Some(task) = task else {
        observation.reject(Stage::Reference, InputIssue::UnknownReference);
        return refused("task_not_in_current_conversation", None, None);
    };
    if task.creation_source != "ai_proposal" {
        observation.stage(Stage::Reference, StageOutcome::Passed);
        observation.permission(PermissionOutcome::PolicyRejected);
        return refused(
            "manual_task_cannot_be_cancelled_by_ai",
            Some(view(&task, destinations)?),
            Some(
                "Only the user can cancel this manually created task in the task manager. Do not request permission to bypass this restriction.",
            ),
        );
    }
    if task.status == "deleted" {
        observation.stage(Stage::Reference, StageOutcome::Passed);
        observation.stage(Stage::Preflight, StageOutcome::Passed);
        return ModelContent::json(&CancelOutcome {
            ok: true,
            reason: None,
            state: Some("already_cancelled"),
            task: Some(view(&task, destinations)?),
            message: None,
        })
        .map_err(|_| ScheduleStoreError::Invalid);
    }
    if task.status == "completed" {
        observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
        return refused(
            "task_already_completed",
            Some(view(&task, destinations)?),
            None,
        );
    }
    if task.status == "expired" {
        observation.reject(Stage::Reference, InputIssue::ReferenceExpired);
        return refused(
            "task_expired_with_source_conversation",
            Some(view(&task, destinations)?),
            None,
        );
    }
    if task.revision != expected {
        observation.reject(Stage::Reference, InputIssue::ReferenceExpired);
        return refused(
            "revision_changed_query_again",
            Some(view(&task, destinations)?),
            None,
        );
    }
    observation.stage(Stage::Reference, StageOutcome::Passed);
    observation.stage(Stage::Preflight, StageOutcome::Passed);
    let work = if let Some(id) = task.active_run_id.as_deref() {
        Some(
            run::Entity::find()
                .filter(run::Column::RunId.eq(id))
                .filter(run::Column::ScheduleId.eq(&task.schedule_id))
                .filter(run::Column::OwnerUserId.eq(task.owner_user_id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?,
        )
    } else {
        None
    };
    let mut unstarted = false;
    if let Some(work) = &work {
        unstarted = super::edit::stop_pending(txn, work, now, true).await?;
    }
    let in_flight = work
        .as_ref()
        .is_some_and(|work| !work.failure_accounted && !unstarted);
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            status: Set("deleted".into()),
            next_run_at: Set(None),
            active_run_id: Set(if unstarted {
                None
            } else {
                task.active_run_id.clone()
            }),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(expected))
        .filter(entity::Column::CreationSource.eq("ai_proposal"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    ModelContent::json(&Cancelled {
        ok: true,
        schedule_id: ServerId::new(&task.schedule_id),
        revision: expected + 1,
        state: if in_flight {
            "cancellation_requested"
        } else {
            "cancelled"
        },
        future_scheduling_stopped: true,
        effects_undone: false,
        run_id: work.as_ref().map(|work| ServerId::new(&work.run_id)),
        run_state: work.as_ref().map(|work| {
            ServerId::new(if unstarted {
                "cancelled"
            } else {
                work.status.as_str()
            })
        }),
        message: "No additional approval was required. Completed external effects are not undone.",
    })
    .map_err(|_| ScheduleStoreError::Invalid)
}
