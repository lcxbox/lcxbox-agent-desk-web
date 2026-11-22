//! Original model-call admission, frozen-query scans and atomic result delivery.
//! Scanned-but-omitted sources never advance the delivered-version ledger.

use super::{
    AgentStoreError,
    history_view::AuthorizedHistoryView,
    keyword_search::HistorySearchAuthorization,
    retrieval_cache::{self, StoredQuery},
    retrieval_native::NativeAgentDataFence,
    retrieval_result::{self, RetrievalResultInput},
    unsigned,
};
use crate::{
    config::{
        ConfigConnection,
        connection::{DatabaseConnection, DatabaseTransaction},
    },
    entity::agent_device_binding as device,
};
use desk_agent_protocol::ai_agent::{
    history::{
        AgentDeviceScope, AgentHistoryCoverage, ConversationSearchRequest,
        ConversationSearchResult, HistorySort, HistoryTimeField,
    },
    source::SourceReadPurpose,
};
use desk_diagnose_core::{
    ai_agent::{
        data_access::agent_scope,
        history::validate_search_scope,
        retrieval::query_groups,
        retrieval_query::{PageAnchor, QuerySourceVersion},
        retrieval_response::{ToolReplyBudget, fit_search},
        retrieval_state::{
            FrozenRetrievalQuery, FrozenRetrievalRequest, QueryCursor, RetrievalQueryBinding,
            RetrievalSelection, RetrievalWatermark,
        },
        seam::{AgentDataCall, AgentDataRead},
    },
    native_ids::{self, Kind},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait};
use std::collections::BTreeSet;

pub(crate) async fn search(
    db: &DatabaseConnection,
    context: AgentDataCall<'_>,
    request: &ConversationSearchRequest,
) -> Result<AgentDataRead<ConversationSearchResult>, AgentStoreError> {
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| search_on(&txn, context, request)).await {
        Ok(result) => {
            txn.commit().await?;
            Ok(result)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn search_on(
    db: &DatabaseTransaction,
    context: AgentDataCall<'_>,
    request: &ConversationSearchRequest,
) -> Result<AgentDataRead<ConversationSearchResult>, AgentStoreError> {
    let now = now_ms()?;
    let context = AgentDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence =
        desk_diagnose_core::future::boxed(|| super::retrieval_native::prepare_on(db, context, now))
            .await?;
    if let Some(result) = desk_diagnose_core::future::boxed(|| {
        retrieval_result::replay_on::<_, ConversationSearchResult>(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            SourceReadPurpose::ModelHistory,
            now,
        )
    })
    .await?
    {
        let deadline = fence.finish_on(now_ms()?).await?;
        validate_final_time(&result, deadline, now_ms()?)?;
        return Ok(result);
    }
    let resolved_request = fence
        .context
        .session
        .ui_references
        .resolve_conversation_request(request)
        .map_err(|_| AgentStoreError::Invalid("unknown conversation reference"))?;
    let request = &resolved_request;
    let binding = RetrievalQueryBinding::for_call(&fence.context, SourceReadPurpose::ModelHistory)
        .map_err(AgentStoreError::Invalid)?;
    let original = FrozenRetrievalRequest::Conversations {
        request: request.clone(),
    };
    let stored = if let Some(cursor) = &request.cursor {
        Some(retrieval_cache::load_cursor_on(db, &binding, &original, cursor, now).await?)
    } else {
        retrieval_cache::load_logical_on(db, &binding, context.logical_call_id, &original, now)
            .await?
    };
    if stored
        .as_ref()
        .is_some_and(|stored| stored.proof.was_delivered(context.logical_call_id))
    {
        return Err(AgentStoreError::Invalid(
            "original delivered retrieval response is unavailable",
        ));
    }
    let mut devices = owner_devices_on(db, &fence).await?;
    // Account-level conversations belong to no device and share the device limit.
    devices.insert(String::new());
    if devices.len() > fence.policy.limits.retrieval.episode.query_devices as usize {
        return Err(AgentStoreError::Capacity);
    }
    let mut query = desk_diagnose_core::future::boxed(|| {
        prepare_query_on(
            db,
            &fence,
            QueryPreparation {
                request,
                binding,
                original,
                old: stored.as_ref(),
                devices: &devices,
                now,
            },
        )
    })
    .await?;
    let page = request
        .cursor
        .as_deref()
        .map(|cursor| {
            query
                .page(cursor, &query.binding, &query.request, now)
                .cloned()
        })
        .transpose()
        .map_err(AgentStoreError::Invalid)?;
    let authority = search_authority(&fence, &query, now)?;
    let mut batch = desk_diagnose_core::future::boxed(|| {
        scan_on(db, &authority, &query, request, page.as_ref(), &devices)
    })
    .await?;
    let mut status_roots = BTreeSet::new();
    for view in &mut batch.views {
        if status_roots.insert(view.source.row.root_conversation_id.clone()) {
            view.native_summary = desk_diagnose_core::future::boxed(|| {
                super::native_summary::read_on(
                    db,
                    &authority,
                    &view.source.row.root_conversation_id,
                    view.device.as_ref(),
                )
            })
            .await?;
        }
    }
    let result = ConversationSearchResult {
        hits: batch
            .views
            .iter()
            .map(|view| {
                view.search_hit(
                    fence
                        .context
                        .session
                        .agent_data
                        .as_ref()
                        .unwrap()
                        .limits
                        .retrieval
                        .snippet_bytes,
                )
            })
            .collect::<Result<_, _>>()?,
        scope: request.scope,
        effective_queries: batch.effective_queries,
        retrieval_mode: if request.sort == HistorySort::Time {
            "time"
        } else {
            "bm25_rrf"
        }
        .into(),
        has_more: batch.more,
        next_cursor: None,
        scope_changed: query.reasons.iter().any(|reason| reason == "scope_changed"),
        coverage: coverage(&fence, request, batch.reasons, now),
    };
    let (page_id, provisional) = page_cursor(&query.snapshot.query_id, context.logical_call_id)?;
    let budget = ToolReplyBudget::new(&context.tool_call.id, context.maximum_response_bytes)
        .map_err(AgentStoreError::Invalid)?;
    let mut reply = fit_search(result, &budget, &provisional).map_err(AgentStoreError::Invalid)?;
    if reply.result.has_more {
        let anchor = if reply.selected_count < batch.views.len() {
            batch
                .frontiers
                .get(
                    reply
                        .selected_count
                        .checked_sub(1)
                        .ok_or(AgentStoreError::Capacity)?,
                )
                .cloned()
        } else {
            batch.continuation
        };
        let cursor = query
            .add_page(page_id, anchor.ok_or(AgentStoreError::Inaccessible)?)
            .map_err(AgentStoreError::Invalid)?;
        reply.result.next_cursor = Some(cursor);
    }
    // add_page can reuse an existing canonical token. Hash and store the final
    // real cursor, including all UTF-8 and JSON framing, never the placeholder.
    budget
        .encode(&reply.result)
        .map_err(AgentStoreError::Invalid)?;
    let mut versions = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.version())
        .collect::<Result<Vec<_>, _>>()?;
    let mut selected = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.source.usage.clone())
        .collect::<Vec<_>>();
    let mut delivered_sources = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.source.row.source_id().clone())
        .collect::<BTreeSet<_>>();
    for view in &batch.views[..reply.selected_count] {
        if let Some(native) = &view.native_summary {
            for source in native.sources() {
                if delivered_sources.insert(source.row.source_id().clone()) {
                    versions.push(source.version()?);
                    selected.push(source.usage.clone());
                }
            }
        }
    }

    versions.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    versions.dedup();
    query
        .remember(&versions)
        .map_err(AgentStoreError::Invalid)?;
    let prepared_at = now_ms()?;
    let pending = desk_diagnose_core::future::boxed(|| {
        retrieval_result::prepare_on(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            RetrievalResultInput {
                result: reply.result,
                purpose: SourceReadPurpose::ModelHistory,
                selected: &selected,
            },
            prepared_at,
        )
    })
    .await?;
    let deadline = fence.finish_on(now_ms()?).await?;
    // This is the final source-read phase, followed by quota/result metadata.
    // Query-cache CAS reads below are metadata only; no new native/source reads.
    let stored_at = now_ms()?;
    let result = desk_diagnose_core::future::boxed(|| {
        retrieval_result::store_on(pending, &fence.policy, stored_at)
    })
    .await?;
    if !query
        .mark_delivered(context.logical_call_id)
        .map_err(AgentStoreError::Invalid)?
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let saved = if let Some(old) = &stored {
        retrieval_cache::update_on(db, old, &query, &fence.policy, now_ms()?).await?
    } else {
        retrieval_cache::prepare_on(db, &query, &fence.policy, now_ms()?).await?
    };
    if saved.proof.as_ref() != query.as_ref() {
        return Err(AgentStoreError::Inaccessible);
    }
    validate_final_time(
        &result,
        deadline.min(query.snapshot.expires_at_unix_ms),
        now_ms()?,
    )?;
    Ok(result)
}

pub(super) fn now_ms() -> Result<u64, AgentStoreError> {
    unsigned(chrono::Utc::now().timestamp_millis())
}

pub(super) fn validate_final_time<T>(
    result: &AgentDataRead<T>,
    deadline: u64,
    now: u64,
) -> Result<(), AgentStoreError> {
    let proof = result
        .projection
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?;
    if deadline <= now
        || proof.unused_until_unix_ms <= now
        || proof
            .usage
            .expires_at_unix_ms
            .is_some_and(|until| until <= now)
    {
        return Err(AgentStoreError::Inaccessible);
    }
    Ok(())
}

pub(super) fn page_cursor(
    query_id: &str,
    logical_call_id: &str,
) -> Result<(String, String), AgentStoreError> {
    let page = native_ids::stable(
        Kind::AgentPageCursor,
        &("search-page-v1", query_id, logical_call_id),
    );
    let cursor = QueryCursor::new(query_id, &page)
        .map_err(AgentStoreError::Invalid)?
        .encode();
    Ok((page, cursor))
}

pub(super) fn search_authority<'a>(
    fence: &'a NativeAgentDataFence<'_>,
    query: &'a FrozenRetrievalQuery,
    now: u64,
) -> Result<HistorySearchAuthorization<'a>, AgentStoreError> {
    let scope = fence
        .context
        .session
        .agent_data
        .as_ref()
        .and_then(|data| data.history_access_scope.as_deref())
        .ok_or(AgentStoreError::Inaccessible)?;
    Ok(HistorySearchAuthorization {
        actor_id: &fence.context.session.actor_id,
        current_device_ref: &scope.current_device_ref,
        agent_scope: &query.binding.scope,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        corpus_generation: query.source_observation_generation,
        now_unix_ms: now,
    })
}

pub(super) async fn owner_devices_on(
    db: &DatabaseTransaction,
    fence: &NativeAgentDataFence<'_>,
) -> Result<BTreeSet<String>, AgentStoreError> {
    if fence.context.session.actor_id
        != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
    {
        return Err(AgentStoreError::Inaccessible);
    }
    let maximum = fence
        .context
        .session
        .agent_data
        .as_ref()
        .unwrap()
        .retrieval_budget
        .limits
        .query_devices;
    let config = db.config_read().await;
    let rows = device::Entity::find()
        .filter(device::Column::InstanceId.eq(&config.metadata.instance))
        .order_by_asc(device::Column::BindingId)
        .limit(u64::from(maximum) + 1)
        .all(db)
        .await?;
    if rows.len() > maximum as usize {
        return Err(AgentStoreError::Capacity);
    }
    let mut devices = BTreeSet::new();
    for row in rows {
        if native_ids::stable(Kind::AgentDeviceBinding, &(&row.instance_id, &row.audience))
            != row.binding_id
        {
            return Err(AgentStoreError::Inaccessible);
        }
        devices.insert(row.binding_id);
    }
    Ok(devices)
}

struct QueryPreparation<'a> {
    request: &'a ConversationSearchRequest,
    binding: RetrievalQueryBinding,
    original: FrozenRetrievalRequest,
    old: Option<&'a StoredQuery>,
    devices: &'a BTreeSet<String>,
    now: u64,
}

async fn prepare_query_on(
    db: &DatabaseTransaction,
    fence: &NativeAgentDataFence<'_>,
    input: QueryPreparation<'_>,
) -> Result<Box<FrozenRetrievalQuery>, AgentStoreError> {
    let QueryPreparation {
        request,
        binding,
        original,
        old,
        devices,
        now,
    } = input;
    let scope = binding
        .history_scope
        .as_ref()
        .ok_or(AgentStoreError::Inaccessible)?;
    validate_search_scope(scope, request, now).map_err(AgentStoreError::Invalid)?;
    let runtime = super::runtime_on(db, &binding.actor_id).await?;
    let observed_at = now_ms()?;
    let watermark = RetrievalWatermark {
        corpus_generation: unsigned(runtime.history_data_generation)?,
        memory_generation: fence.access.memory_state_generation,
        control_generation: fence.access.control_generation,
        policy_revision: fence.policy.revision,
    };
    let allowed = devices
        .iter()
        .filter(|device| {
            request.scope == AgentDeviceScope::AllMyDevices
                || device.as_str() == binding.source_device_ref
        })
        .filter(|device| old.is_none_or(|old| old.proof.snapshot.device_refs.contains(device)))
        .cloned()
        .collect::<Vec<_>>();
    let changed = if let Some(old) = old {
        old.proof
            .needs_refresh(watermark)
            .map_err(AgentStoreError::Invalid)?
            || old.proof.snapshot.device_refs != allowed
    } else {
        true
    };
    if !changed {
        return Ok(old.ok_or(AgentStoreError::Inaccessible)?.proof.clone());
    }
    let observation = old.map_or(watermark.corpus_generation, |old| {
        old.proof.source_observation_generation
    });
    let current_scope = agent_scope(fence.context.session).map_err(AgentStoreError::Invalid)?;
    let authority = HistorySearchAuthorization {
        actor_id: &binding.actor_id,
        current_device_ref: &binding.source_device_ref,
        agent_scope: &current_scope,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        corpus_generation: observation,
        now_unix_ms: now,
    };
    let (candidates, reasons) = if request.sort == HistorySort::Relevance {
        let allowed_set = allowed.iter().cloned().collect();
        let ranked = desk_diagnose_core::future::boxed(|| {
            super::keyword_search::relevance_on(db, &authority, request, &allowed_set)
        })
        .await?;
        (
            ranked
                .ranking
                .documents
                .into_iter()
                .map(|candidate| QuerySourceVersion {
                    source_id: candidate.document.source_id,
                    revision: candidate.document.revision,
                })
                .collect(),
            ranked.reasons,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let selection = RetrievalSelection {
        devices: allowed,
        watermark,
        candidates,
        reasons,
    };
    if let Some(old) = old {
        let mut query = old.proof.clone();
        query
            .refresh(selection, observed_at)
            .map_err(AgentStoreError::Invalid)?;
        Ok(query)
    } else {
        Ok(Box::new(
            FrozenRetrievalQuery::prepare(
                binding,
                fence
                    .context
                    .retrieval_reservation
                    .ok_or(AgentStoreError::Inaccessible)?,
                original,
                selection,
                observed_at,
                fence
                    .context
                    .session
                    .agent_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .limits,
            )
            .map_err(AgentStoreError::Invalid)?,
        ))
    }
}

#[allow(clippy::vec_box)]
struct SearchBatch {
    views: Vec<Box<AuthorizedHistoryView>>,
    frontiers: Vec<PageAnchor>,
    continuation: Option<PageAnchor>,
    more: bool,
    effective_queries: Vec<Vec<String>>,
    reasons: Vec<String>,
}

async fn scan_on(
    db: &DatabaseTransaction,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    request: &ConversationSearchRequest,
    page: Option<&PageAnchor>,
    devices: &BTreeSet<String>,
) -> Result<SearchBatch, AgentStoreError> {
    let mut reasons = query.reasons.clone();
    let (candidates, continuation, more, effective_queries) = if request.sort == HistorySort::Time {
        let scanned = desk_diagnose_core::future::boxed(|| {
            super::history_scan::time::enumerate_on(db, context, query, page, devices)
        })
        .await?;
        reasons.extend(scanned.reasons);
        (
            scanned
                .items
                .into_iter()
                .map(|item| {
                    (
                        item.source.clone(),
                        PageAnchor::Time {
                            time_unix_ms: item.time_key_unix_ms,
                            history_id: item.position.history_id,
                        },
                    )
                })
                .collect::<Vec<_>>(),
            scanned.continuation,
            scanned.has_more,
            scanned.effective_queries,
        )
    } else {
        let offset = match page {
            None | Some(PageAnchor::Start) => 0,
            Some(PageAnchor::Relevance { next_offset }) => *next_offset as usize,
            _ => return Err(AgentStoreError::Inaccessible),
        };
        let mut cursor = offset;
        let mut candidates = Vec::new();
        while let Some(source) = query.snapshot.candidates.get(cursor) {
            if !query.was_seen(source) {
                candidates.push((
                    source.clone(),
                    PageAnchor::Relevance {
                        next_offset: (cursor + 1) as u32,
                    },
                ));
            }
            cursor += 1;
            if candidates.len() >= request.limit as usize {
                break;
            }
        }
        let more = cursor < query.snapshot.candidates.len();
        let mut groups = query_groups(&request.queries).map_err(AgentStoreError::Invalid)?;
        groups.sort();
        (
            candidates,
            more.then_some(PageAnchor::Relevance {
                next_offset: cursor as u32,
            }),
            more,
            groups,
        )
    };
    let mut views = Vec::new();
    let mut frontiers = Vec::new();
    for (version, frontier) in candidates {
        match desk_diagnose_core::future::boxed(|| {
            super::history_view::read_on(db, context, &version)
        })
        .await
        {
            Ok(view) => {
                views.push(view);
                frontiers.push(frontier);
            }
            Err(AgentStoreError::Backend(error)) => return Err(error.into()),
            Err(_) => {
                reasons.push("source_unavailable".into());
                super::keyword_search::validate_context_on(db, context).await?;
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    Ok(SearchBatch {
        views,
        frontiers,
        continuation,
        more,
        effective_queries,
        reasons,
    })
}

fn coverage(
    fence: &NativeAgentDataFence<'_>,
    request: &ConversationSearchRequest,
    mut reasons: Vec<String>,
    now: u64,
) -> AgentHistoryCoverage {
    let retained_from =
        now.saturating_sub(u64::from(fence.policy.history_retention_days) * 86_400_000);
    // Counts are lower bounds until a scope-authorized indexing ledger can prove
    // completeness. Actor-wide counters cannot describe an individual scope.
    reasons.push("coverage_not_exhaustive".into());
    reasons.push("coverage_counts_lower_bounds".into());
    if request.time_field == HistoryTimeField::RecordedAt
        && request
            .time_range
            .as_ref()
            .is_some_and(|range| range.from_unix_ms < retained_from)
    {
        reasons.push("outside_retention_window".into());
    }
    reasons.sort();
    reasons.dedup();
    AgentHistoryCoverage {
        retained_from_unix_ms: Some(retained_from),
        retained_until_unix_ms: Some(now),
        indexed_through_unix_ms: None,
        pending: 0,
        failed: 0,
        has_gaps: true,
        partial: true,
        reasons,
    }
}
