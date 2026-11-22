//! Per-field source proofs of a task's model-visible text.
//!
//! A task's title, rule and prompt are read back into later requests: in
//! management replies and in the input of a run. Each field keeps the proof
//! of where its current text came from: the owner who wrote or edited it, or
//! the model output that proposed it. Editing one field replaces only that
//! field's proof.

use super::{ScheduleStoreError, json, session::ScheduleHostStores};
use crate::entity::agent_schedule as entity;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::model_content::{StoredRoot, StoredTextSource};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

/// The stored-text kind of task fields.
pub const KIND: &str = "schedule";
pub const TITLE: &str = "title";
pub const RULE: &str = "rule";
pub const PROMPT: &str = "prompt";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleFieldSources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<StoredTextSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<StoredTextSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<StoredTextSource>,
}

impl ScheduleFieldSources {
    /// Every field from one source.
    pub fn all(source: Option<StoredTextSource>) -> Self {
        Self {
            title: source.clone(),
            rule: source.clone(),
            prompt: source,
        }
    }

    /// The proofs stored with `task`; none when it has none.
    pub fn of(task: &entity::Model) -> Self {
        task.field_sources_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default()
    }

    pub fn encode(&self) -> Result<String, ScheduleStoreError> {
        json(self)
    }

    fn proof(&self, field: &str) -> Option<StoredTextSource> {
        match field {
            TITLE => self.title.clone(),
            RULE => self.rule.clone(),
            PROMPT => self.prompt.clone(),
            _ => None,
        }
    }
}

/// The owner's own proof, at the current policy revision.
pub async fn owner_source<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    actor: &str,
    device: &str,
) -> Result<StoredTextSource, ScheduleStoreError> {
    let raw =
        desk_diagnose_core::future::send_phase(|| host.owner_prompt_retention(txn, actor, device))
            .await?;
    let retention: desk_diagnose_core::schedule::published_input::TaskPromptRetention =
        serde_json::from_str(&raw).map_err(|_| ScheduleStoreError::Invalid)?;
    Ok(StoredTextSource::owner(
        actor,
        retention.policy.policy_revision,
    ))
}

/// The root of `field` of `task` holding `text`, bound to `destinations`.
pub fn root(
    task: &entity::Model,
    field: &'static str,
    text: &str,
    destinations: Vec<DestinationIdentity>,
) -> Result<StoredRoot, ScheduleStoreError> {
    StoredRoot::text(
        KIND,
        &task.schedule_id,
        field,
        text,
        ScheduleFieldSources::of(task).proof(field),
        destinations,
    )
    .map_err(|_| ScheduleStoreError::Invalid)
}

/// Stores `sources` on the task the caller just created under its fence.
pub async fn record_on<C: ConnectionTrait>(
    txn: &C,
    mut task: entity::Model,
    sources: &ScheduleFieldSources,
) -> Result<entity::Model, ScheduleStoreError> {
    let encoded = sources.encode()?;
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            field_sources_json: Set(Some(encoded.clone())),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    task.field_sources_json = Some(encoded);
    Ok(task)
}
