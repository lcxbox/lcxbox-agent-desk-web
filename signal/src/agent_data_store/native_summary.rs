//! Bounded retained native observations for one root. Retained terminal states
//! remain readable after the original execution rows have been reclaimed.
use super::{
    AgentStoreError,
    keyword_search::{self, HistorySearchAuthorization, ReadableHistorySource},
    unsigned,
};
use crate::config::ConfigConnection;
use crate::entity::agent_history as history;
use desk_agent_protocol::ai_agent::{
    history::NativeStatusSummary,
    source::{AgentSourceDevice, AgentSourceKind, AgentSourceRef, SourceRefRole},
};
use desk_diagnose_core::ai_agent::native_history::{NativeHistorySnapshot, NativeHistoryStatus};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::collections::BTreeMap;

#[allow(clippy::vec_box)]
pub(crate) struct AuthorizedNativeSummary {
    pub summary: NativeStatusSummary,
    pub source: Box<ReadableHistorySource>,
    additional: Vec<Box<ReadableHistorySource>>,
}
impl AuthorizedNativeSummary {
    pub(crate) fn sources(&self) -> impl Iterator<Item = &ReadableHistorySource> {
        std::iter::once(self.source.as_ref()).chain(self.additional.iter().map(Box::as_ref))
    }
}
pub(crate) async fn read_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    root: &str,
    device: Option<&AgentSourceDevice>,
) -> Result<Option<Box<AuthorizedNativeSummary>>, AgentStoreError> {
    if !context
        .scope
        .allowed_kinds
        .contains(&AgentSourceKind::NativeStatus)
    {
        return Ok(None);
    }
    let maximum = context.platform.limits.processing.native_status_source_refs as usize;
    let rows = history::Entity::find()
        .filter(history::Column::ActorId.eq(context.actor_id))
        .filter(history::Column::RootConversationId.eq(root))
        .filter(history::Column::ConversationId.eq(root))
        .filter(
            history::Column::DeviceRef.eq(device.map_or("", |device| device.device_ref.as_str())),
        )
        .filter(history::Column::Kind.eq("native_status"))
        .filter(history::Column::FragmentIndex.eq(0))
        .filter(history::Column::DeletedAtMs.is_null())
        .filter(history::Column::ExpiresAtMs.gt(super::signed(context.now_unix_ms)?))
        .filter(history::Column::AdmittedGeneration.lte(super::signed(context.corpus_generation)?))
        .order_by_desc(history::Column::AdmittedGeneration)
        .order_by_desc(history::Column::Id)
        .limit(maximum as u64 + 1)
        .all(db)
        .await?;
    let mut complete = rows.len() <= maximum;
    let mut sources = Vec::new();
    let mut observed_at = 0;
    let mut states = BTreeMap::<(String, String), (NativeHistoryStatus, String)>::new();
    for row in rows.into_iter().take(maximum) {
        let source = match keyword_search::readable_history_on(
            db,
            context,
            &row.source_id(),
            unsigned(row.revision)?,
        )
        .await
        {
            Ok(source) => source,
            Err(AgentStoreError::Backend(error)) => return Err(error.into()),
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let native =
            super::history_native::read(&source.row)?.ok_or(AgentStoreError::Inaccessible)?;
        let snapshot = native
            .status_snapshot
            .as_ref()
            .ok_or(AgentStoreError::Inaccessible)?;
        complete &= snapshot.complete;
        observed_at = observed_at.max(snapshot.observed_at_unix_ms);
        // Admission order decides the newest original observation for each
        // actual native identity; a later empty live inventory erases nothing.
        for item in &snapshot.items {
            states
                .entry((item.kind.as_str().into(), item.native_id.clone()))
                .or_insert_with(|| (item.clone(), source.row.source_id().clone()));
        }
        sources.push(source);
    }
    if sources.is_empty() {
        return Ok(None);
    }
    let pending = states
        .values()
        .filter(|(item, _)| !item.is_terminal())
        .count() as u32;
    let unknown = states
        .values()
        .filter(|(item, _)| item.is_unknown())
        .count() as u32;
    let count = states.len();
    let inventory = NativeHistorySnapshot::bounded(
        states.values().map(|(item, _)| item.clone()).collect(),
        complete,
        observed_at,
        context.platform.limits.processing.native_status_items as usize,
    )
    .map_err(AgentStoreError::Invalid)?;
    let mut sources = sources.into_iter();
    let source = sources.next().ok_or(AgentStoreError::Inaccessible)?;
    let additional = sources.collect::<Vec<_>>();
    let mut summary = inventory
        .summary(
            &source.row.source_id(),
            unsigned(source.row.revision)?,
            context.platform.limits.processing.native_status_model_items as usize,
        )
        .map_err(AgentStoreError::Invalid)?;
    summary.pending_count = pending;
    summary.unknown_count = unknown;
    summary.additional_count = count.saturating_sub(summary.items.len()) as u32;
    for item in &mut summary.items {
        item.source_id = states
            .get(&(item.kind.clone(), item.native_id.clone()))
            .ok_or(AgentStoreError::Inaccessible)?
            .1
            .clone();
        item.source_device = device.cloned();
    }
    summary.additional_sources = additional
        .iter()
        .map(|source| {
            Ok(AgentSourceRef {
                kind: AgentSourceKind::NativeStatus,
                id: source.row.source_id().clone(),
                revision: Some(unsigned(source.row.revision)?),
                role: SourceRefRole::Evidence,
            })
        })
        .collect::<Result<_, AgentStoreError>>()?;
    Ok(Some(Box::new(AuthorizedNativeSummary {
        summary,
        source,
        additional,
    })))
}
