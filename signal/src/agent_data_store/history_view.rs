//! Model-authorized retained fragments and exact source links. Owner viewing
//! supplies locator formatting only, after the separate model-purpose gate.

use super::{
    AgentStoreError,
    keyword_search::{self, HistorySearchAuthorization, ReadableHistorySource},
    unsigned,
};
use crate::config::ConfigConnection;
use desk_agent_protocol::ai_agent::{
    history::{ConversationSearchHit, HistoryMessage, HistoryTimeRange},
    source::{AgentSourceDevice, AgentSourceKind, AgentSourceLocator, SourceAvailability},
};
use desk_diagnose_core::ai_agent::retrieval_query::QuerySourceVersion;
use sea_orm::ConnectionTrait;

pub(crate) struct AuthorizedHistoryView {
    pub source: Box<ReadableHistorySource>,
    pub locator: AgentSourceLocator,
    /// Absent for account-level conversations that belong to no device.
    pub device: Option<AgentSourceDevice>,
    pub kind: AgentSourceKind,
    pub availability: SourceAvailability,
    pub native_summary: Option<Box<super::native_summary::AuthorizedNativeSummary>>,
}

pub(crate) async fn read_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    version: &QuerySourceVersion,
) -> Result<Box<AuthorizedHistoryView>, AgentStoreError> {
    let source = desk_diagnose_core::future::boxed(|| {
        keyword_search::readable_history_on(db, context, &version.source_id, version.revision)
    })
    .await?;
    let view = super::owner_source::history_source_on(
        db,
        context.actor_id,
        &source.row,
        context.platform,
        context.now_unix_ms,
    )
    .await?;
    if !matches!(
        &view.availability,
        SourceAvailability::Available | SourceAvailability::Partial | SourceAvailability::Redacted
    ) || view.source_id != version.source_id
        || view.revision != version.revision
        || view.content.as_deref() != Some(source.row.content.as_str())
        || view.recorded_at_unix_ms != Some(unsigned(source.row.recorded_at_ms)?)
        || view.statement_timezone.as_deref() != Some(source.row.statement_timezone.as_str())
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let locator = view.locator.ok_or(AgentStoreError::Inaccessible)?;
    let device = view.source_device;
    if locator.conversation_id != source.row.conversation_id
        || locator.root_conversation_id != source.row.root_conversation_id
        || device
            .as_ref()
            .map_or("", |device| device.device_ref.as_str())
            != source.row.device_ref
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let kind = match source.row.kind.as_str() {
        "input" => AgentSourceKind::Input,
        "agent_message" => AgentSourceKind::AgentMessage,
        "receipt" => AgentSourceKind::Receipt,
        "native_status" => AgentSourceKind::NativeStatus,
        _ => return Err(AgentStoreError::Inaccessible),
    };
    // An ordinary message timestamp is never promoted to an action receipt time.
    if matches!(kind, AgentSourceKind::Input | AgentSourceKind::AgentMessage)
        && (source.row.native_action_at_ms.is_some()
            || source.row.native_action_from_ms.is_some()
            || source.row.native_action_until_ms.is_some()
            || source.row.time_basis.is_some())
    {
        return Err(AgentStoreError::Inaccessible);
    }
    validate_native_time(&source.row)?;
    keyword_search::validate_context_on(db, context).await?;
    Ok(Box::new(AuthorizedHistoryView {
        source,
        locator,
        device,
        kind,
        availability: view.availability,
        native_summary: None,
    }))
}

impl AuthorizedHistoryView {
    pub(crate) fn version(&self) -> Result<QuerySourceVersion, AgentStoreError> {
        self.source.version()
    }
    pub(crate) fn search_hit(
        &self,
        snippet_bytes: u32,
    ) -> Result<ConversationSearchHit, AgentStoreError> {
        let row = &self.source.row;
        let mut end = row.content.len().min(snippet_bytes as usize);
        while !row.content.is_char_boundary(end) {
            end -= 1;
        }
        Ok(ConversationSearchHit {
            source_id: row.source_id().clone(),
            revision: unsigned(row.revision)?,
            kind: self.kind,
            locator: self.locator.clone(),
            source_device: self.device.clone(),
            conversation_title: None,
            snippet: row.content[..end].into(),
            recorded_at_unix_ms: unsigned(row.recorded_at_ms)?,
            statement_timezone: row.statement_timezone.clone(),
            native_action_at_unix_ms: row.native_action_at_ms.map(unsigned).transpose()?,
            native_action_interval: row
                .native_action_from_ms
                .zip(row.native_action_until_ms)
                .map(
                    |(from, until)| -> Result<HistoryTimeRange, AgentStoreError> {
                        Ok(HistoryTimeRange {
                            from_unix_ms: unsigned(from)?,
                            until_unix_ms: unsigned(until)?,
                        })
                    },
                )
                .transpose()?,
            native_time_basis: row.time_basis.clone(),
            native_status_summary: self
                .native_summary
                .as_ref()
                .map(|native| native.summary.clone()),
        })
    }
    pub(crate) fn message(&self) -> Result<HistoryMessage, AgentStoreError> {
        let row = &self.source.row;
        Ok(HistoryMessage {
            source_id: row.source_id().clone(),
            message_id: row.native_id.clone(),
            revision: unsigned(row.revision)?,
            role: row.role.clone(),
            content: row.content.clone(),
            recorded_at_unix_ms: unsigned(row.recorded_at_ms)?,
            statement_timezone: row.statement_timezone.clone(),
            source_device: self.device.clone(),
        })
    }
}

fn validate_native_time(row: &crate::entity::agent_history::Model) -> Result<(), AgentStoreError> {
    let interval = match (row.native_action_from_ms, row.native_action_until_ms) {
        (None, None) => None,
        (Some(from), Some(until)) if from > 0 && until > from => Some(HistoryTimeRange {
            from_unix_ms: unsigned(from)?,
            until_unix_ms: unsigned(until)?,
        }),
        _ => return Err(AgentStoreError::Inaccessible),
    };
    if let Some(interval) = &interval {
        interval.validate().map_err(AgentStoreError::Invalid)?;
    }
    if row.native_action_at_ms.is_some_and(|at| at <= 0)
        || (row.native_action_at_ms.is_some() && interval.is_some())
        || row.time_basis.as_ref().is_some_and(|basis| {
            basis.is_empty() || basis.len() > 128 || basis.chars().any(char::is_control)
        })
        || (row.time_basis.is_some() != (row.native_action_at_ms.is_some() || interval.is_some()))
    {
        return Err(AgentStoreError::Inaccessible);
    }
    Ok(())
}
