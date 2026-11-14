//! AI security scan of an uploaded skill package with the OSS approval model
//! (`[approval_gateway]`). The result is advice shown to the uploader; it never
//! blocks, approves or authorizes anything. Each batch is one model call with
//! its own admission permit and model egress receipt, like a reviewer call.

use crate::config::connection::DatabaseConnection;
use crate::model_dial::SignalModelSeam;
use crate::model_egress_store::SignalModelEgressStore;
use desk_agent_protocol::ai_assistant::skill::{SkillPreview, SkillScanFinding, SkillScanResult};
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_assistant_data_store::skill_package::ParsedPackage;
use desk_assistant_data_store::skill_store::{self, SkillStoreError};
use desk_diagnose_core::chat::StopReason;
use desk_diagnose_core::future::boxed;
use desk_diagnose_core::model_egress::ModelEgressPolicy;
use desk_diagnose_core::model_observability::Origin;
use desk_diagnose_core::seam::{ModelSeam, NullTurnSink};
use desk_diagnose_core::skill_scan::{self, ScanBatch, ScanFile};

/// Scans a validated upload and stores the summary for the save that follows,
/// so the preview carries the result and the `scan_id` to pass back on save.
pub async fn attach(
    db: &DatabaseConnection,
    scope_key: &str,
    parsed: &ParsedPackage,
    preview: &mut SkillPreview,
    now_ms: i64,
) -> Result<(), SkillStoreError> {
    let files = files(parsed);
    let result = boxed(|| scan(db, &files)).await;
    let summary = serde_json::to_string(&result)
        .map_err(|error| SkillStoreError::Backend(sea_orm::DbErr::Custom(error.to_string())))?;
    let txn = boxed(|| crate::db::begin_write(db, crate::entity::agent_skill_scan::Entity)).await?;
    let scan_id = boxed(|| skill_store::save_scan_on(&txn, scope_key, &summary, now_ms)).await?;
    boxed(|| txn.commit()).await?;
    preview.scan = Some(result);
    preview.scan_id = Some(scan_id);
    Ok(())
}

/// The files of a validated package as the scanner sees them: the canonical
/// `SKILL.md` followed by every stored text resource.
pub fn files(parsed: &ParsedPackage) -> Vec<ScanFile> {
    std::iter::once(ScanFile {
        path: "SKILL.md".into(),
        text: parsed.package.render_skill_md(),
    })
    .chain(parsed.package.resources.iter().map(|resource| ScanFile {
        path: resource.path.clone(),
        text: resource.text.clone(),
    }))
    .collect()
}

/// Scans `files` batch by batch. The scan is unavailable when the device-owned
/// AI Assistant switch is off or the approval model is disabled or not configured.
pub async fn scan(db: &DatabaseConnection, files: &[ScanFile]) -> SkillScanResult {
    if !crate::ai_assistant_gate::global_ai_assistant_gate().is_enabled() {
        return skill_scan::unavailable();
    }
    let Ok(config) = boxed(|| crate::approval_model_provider::load(db)).await else {
        return skill_scan::unavailable();
    };
    let Ok(destination) = config.destination_identity() else {
        return skill_scan::unavailable();
    };
    let (batches, unscanned) = skill_scan::plan(files);
    let scan_id = uuid::Uuid::new_v4().simple().to_string();
    let mut outcomes = Vec::with_capacity(batches.len());
    for (index, batch) in batches.iter().enumerate() {
        let request_id = format!("skill-scan-{scan_id}-{index}");
        let outcome = boxed(|| scan_batch(db, &config, &destination, batch, &request_id)).await;
        outcomes.push(outcome.map_err(|reason| {
            log::warn!("[skill-scan] batch failed: request={request_id}, reason={reason}");
            skill_scan::UnusableAnswer
        }));
    }
    skill_scan::summarize(outcomes, unscanned)
}

/// One fenced model call. The egress receipt is committed before provider I/O
/// and closed with the output lineage or as failed.
async fn scan_batch(
    db: &DatabaseConnection,
    config: &crate::approval_model_provider::ApprovalModelConfig,
    destination: &DestinationIdentity,
    batch: &ScanBatch,
    request_id: &str,
) -> Result<Vec<SkillScanFinding>, &'static str> {
    let request =
        skill_scan::request(batch, destination, request_id).map_err(|_| "request_labels")?;
    let receipt_id = format!("skill-scan-call:{request_id}");
    let export_id = format!("skill-scan-export:{request_id}");
    let policy = egress_policy(destination, &export_id)?;
    let authorized = policy
        .authorize_request(request)
        .map_err(|_| "egress_rejected")?;
    let snapshot = authorized.input_snapshot().map_err(|_| "egress_snapshot")?;
    let seam = SignalModelSeam::from_approval_config(config)
        .map_err(|_| "configuration")?
        .with_context_db(db.clone());
    let permit = boxed(|| seam.acquire_admission())
        .await
        .map_err(|_| "admission")?;
    let txn = boxed(|| crate::db::begin_write(db, crate::entity::model_egress_receipt::Entity))
        .await
        .map_err(|_| "storage")?;
    boxed(|| seam.validate_current_on(&txn))
        .await
        .map_err(|_| "configuration_changed")?;
    boxed(|| {
        SignalModelEgressStore::record_snapshot_intent_on(
            &txn,
            receipt_id.clone(),
            export_id.clone(),
            1,
            &snapshot,
        )
    })
    .await
    .map_err(|_| "egress_receipt")?;
    if !crate::ai_assistant_gate::global_ai_assistant_gate().is_enabled() {
        return Err("assistant_disabled");
    }
    boxed(|| txn.commit()).await.map_err(|_| "storage")?;

    let store = SignalModelEgressStore::new(db.clone());
    let mut request = authorized.request;
    request.observation = seam.observation_context(request.use_case, Origin::User);
    let mut sink = NullTurnSink;
    let turn = match boxed(|| seam.call_admitted(request, &mut sink, permit)).await {
        Ok(turn) => turn,
        Err(_) => {
            close_failed(&store, &receipt_id).await;
            return Err("provider");
        }
    };
    boxed(|| {
        store.record_terminal_usage(
            &receipt_id,
            &turn.usage,
            turn.provider_meta.context_observation.as_ref(),
        )
    })
    .await
    .map_err(|_| "usage_receipt")?;
    let output = egress_policy(destination, &export_id)?
        .derive_model_output_envelope(&turn, &authorized.input_envelopes);
    match output {
        Ok(output) => boxed(|| store.mark_succeeded(&receipt_id, &output))
            .await
            .map_err(|_| "egress_receipt")?,
        Err(_) => {
            close_failed(&store, &receipt_id).await;
            return Err("output_lineage");
        }
    }
    if turn.stop_reason != StopReason::EndTurn || !turn.tool_calls.is_empty() {
        return Err("incomplete_answer");
    }
    skill_scan::parse(&turn.text, batch).map_err(|_| "invalid_answer")
}

fn egress_policy(
    destination: &DestinationIdentity,
    export_id: &str,
) -> Result<ModelEgressPolicy, &'static str> {
    Ok(ModelEgressPolicy {
        destination: destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: export_id.into(),
        now_unix_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| "clock")?,
        byte_cap: desk_diagnose_core::approval_review::MAX_APPROVAL_CONTEXT_BYTES,
        permission_resume: false,
    })
}

async fn close_failed(store: &SignalModelEgressStore, receipt_id: &str) {
    if let Err(error) = boxed(|| store.mark_failed(receipt_id)).await {
        log::warn!("[skill-scan] failed to close receipt {receipt_id}: {error}");
    }
}

#[cfg(test)]
mod tests;
