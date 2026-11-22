//! Edge-side execution of a central AI Agent read Provider call.
//!
//! When the agentic loop runs centrally on the manager, the manager ships a
//! server-stamped [`RemoteToolRequest`](desk_agent_protocol::remote_tool::RemoteToolRequest)
//! (one capability call) to this host over the signaling link. The edge keeps
//! **final say** over what may leave the machine: before running anything it
//! re-checks the operation against the envelope's granted scope and the device's
//! local collection policy (the in-process agent itself does not enforce the
//! scope), runs the read, redacts the result fail-closed, and returns a sanitized
//! [`RemoteToolOutput`](desk_agent_protocol::remote_tool::RemoteToolOutput).
//! Raw screenshot bytes are stripped and the model-ready image is carried in a
//! separate field before the router chunks that into a
//! `RemoteToolResponse`. A gate denial or a redaction failure surfaces as an
//! error, never as leaked raw output.

use std::sync::Arc;

use desk_agent_protocol::remote_tool::{
    MAX_REMOTE_TOOL_RESULT_BYTES, RemoteToolImage, RemoteToolOutput,
};
use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, AgentOutcome, Capability, DeviceAgent,
};

use super::redaction::{Redactor, RegexRedactor, redact_snapshot};
use crate::model::settings::{CollectionPolicySettings, SharedSettings};
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;
use desk_ipc_protocol::message::RemoteReadResult;

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A model-safe permission denial (the edge's final say; the loop turns it into an
/// error tool-result).
fn denied(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn invalid_preview(error: desk_diagnose_core::image_input::ImageInputError) -> AgentError {
    AgentError {
        kind: if error == desk_diagnose_core::image_input::ImageInputError::ImageTooLarge {
            AgentErrorKind::OutputLimitExceeded
        } else {
            AgentErrorKind::InvalidInput
        },
        message: format!("document preview image/page validation failed: {error}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Convert the already-redacted operation result into the only wire shape the
/// manager may receive. Screenshot bytes are fitted first and then erased from
/// the structured outcome; the image travels solely as a validated attachment.
fn sanitize_remote_output(
    cap: Capability,
    mut outcome: AgentOutcome,
) -> Result<RemoteToolOutput, AgentError> {
    let mut document_preview = None;
    let mut document_preview_page = None;
    let image = if cap == Capability::ScreenCaptureCurrent {
        let AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &mut outcome
        else {
            return Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "screen capture returned an unexpected output shape".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        };
        let fitted = super::screenshot::fit_screenshot_to_budget(
            &shot.image,
            super::screenshot::DEFAULT_MAX_DIMENSION,
            super::screenshot::DEFAULT_MAX_BYTES,
        )
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "failed to prepare the screen capture for visual diagnosis".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        if fitted.jpeg.is_empty() || fitted.jpeg.len() > super::screenshot::DEFAULT_MAX_BYTES {
            return Err(AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: "screen capture could not be fitted within the image limit".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        let image = RemoteToolImage {
            data_url: fitted.to_data_url(),
            media_type: "image/jpeg".into(),
            width: fitted.width,
            height: fitted.height,
            decoded_bytes: fitted.jpeg.len(),
        };
        desk_diagnose_core::image_input::validate_remote_tool_image(&image).map_err(|_| {
            AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: "screen capture failed image validation".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }
        })?;
        shot.image.clear();
        Some(image)
    } else if cap == Capability::DocumentPreview {
        let (page, descriptor) = match &mut outcome {
            AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::DocumentPreview(descriptor),
            )) => {
                let page = descriptor.first_page.take().ok_or_else(|| AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "document preview did not render its first page".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })?;
                (page, Some(descriptor.clone()))
            }
            AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::DocumentPreviewPage(page),
            )) => {
                let page = std::mem::replace(
                    page,
                    desk_agent_protocol::document_conversion::DocumentPreviewPageOutput {
                        preview_id: String::new(),
                        page: 0,
                        page_count: 0,
                        png: Vec::new(),
                        width: 0,
                        height: 0,
                        pixels_per_point_milli: 0,
                    },
                );
                (page, None)
            }
            _ => {
                return Err(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "document preview returned an unexpected output shape".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                });
            }
        };
        use base64::Engine as _;
        let preview_data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&page.png)
        );
        let page = desk_agent_protocol::document_conversion::DocumentPreviewPageDescriptor {
            preview_id: page.preview_id,
            page: page.page,
            page_count: page.page_count,
            width: page.width,
            height: page.height,
            pixels_per_point_milli: page.pixels_per_point_milli,
        };
        if let Some(descriptor) = descriptor {
            let frame = desk_agent_protocol::document_conversion::DocumentPreviewFrame {
                descriptor,
                page,
                preview_data_url,
            };
            desk_diagnose_core::image_input::validate_document_preview_frame(&frame)
                .map_err(invalid_preview)?;
            document_preview = Some(frame);
        } else {
            let frame = desk_agent_protocol::document_conversion::DocumentPreviewPageFrame {
                page,
                preview_data_url,
            };
            desk_diagnose_core::image_input::validate_document_preview_page_frame(&frame)
                .map_err(invalid_preview)?;
            document_preview_page = Some(frame);
        }
        None
    } else {
        None
    };

    Ok(RemoteToolOutput {
        outcome,
        image,
        document_preview,
        document_preview_page,
    })
}

/// The edge's final say before a central read runs, against the device's
/// authoritative settings. The manager stamps the envelope, but the operation
/// must be within its granted scope (`granted = [cap]` is always stamped, so a
/// mismatch is a malformed or forged request) and the local collection policy
/// must permit it (e.g. logs may be disabled here). Returns the read capability.
pub fn gate_remote_read(
    envelope: &AgentEnvelope,
    policy: &CollectionPolicySettings,
) -> Result<Capability, AgentError> {
    let cap = read_capability(envelope)?;
    if !envelope.scope.granted.contains(&cap) {
        return Err(denied("operation is outside the granted scope"));
    }
    let is_log_read = matches!(
        cap,
        Capability::LogRecent | Capability::ContainerLogs | Capability::ContainerInspect
    );
    if is_log_read && !policy.allow_logs {
        return Err(denied("this read is disabled by the device's local policy"));
    }
    if cap == Capability::ScreenCaptureCurrent && !policy.allow_screen {
        return Err(denied(
            "screen capture is disabled by the device's local policy",
        ));
    }
    Ok(cap)
}

fn read_capability(envelope: &AgentEnvelope) -> Result<Capability, AgentError> {
    envelope
        .operation
        .input
        .capability()
        .ok_or_else(|| AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "remote tool request carries no read capability".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
}

/// A session worker's half of a ServiceDaemon remote read. The daemon has
/// already gated the envelope with its authoritative settings, so the worker
/// reads as the session user, redacts and encodes the result here; neither raw
/// output nor an unfitted screenshot leaves this process.
pub async fn read_for_daemon(
    agent: &LocalDeviceAgent,
    envelope: AgentEnvelope,
) -> RemoteReadResult {
    let result = async {
        let cap = read_capability(&envelope)?;
        let output = read_redacted(agent, &RegexRedactor::new(), envelope, cap).await?;
        encode_remote_output(&output)
    }
    .await;
    match result {
        Ok(bytes) => RemoteReadResult::Output(bytes),
        Err(error) => RemoteReadResult::Error(error),
    }
}

/// Run an already gated read and return its sanitized [`RemoteToolOutput`]. The
/// result is redacted via the same one-entry snapshot path the Direct read seam
/// uses, so the exact send-time redaction and screenshot refit run; a redaction
/// failure is an error, never leaked raw output. Runs wherever the reading agent
/// lives: in process for Default / DeskServer, in the session worker for a
/// ServiceDaemon.
pub async fn read_redacted(
    agent: &LocalDeviceAgent,
    redactor: &dyn Redactor,
    envelope: AgentEnvelope,
    cap: Capability,
) -> Result<RemoteToolOutput, AgentError> {
    let output = agent.invoke(envelope).await?;
    let mut snapshot = EvidenceSnapshot::record(
        "live",
        String::new(),
        now_rfc3339(),
        vec![(cap, AgentOutcome::Ok(output))],
    );
    redact_snapshot(redactor, &mut snapshot).map_err(|e| AgentError {
        kind: AgentErrorKind::RedactionFailed,
        message: format!("evidence redaction failed: {}", e.reason),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    let entry = snapshot
        .contexts
        .into_iter()
        .next()
        .expect("the one entry we recorded is present");
    sanitize_remote_output(cap, entry.outcome)
}

/// Serialize a read result for chunking to the central, refusing one over the
/// remote tool result limit rather than sending an oversized result.
pub fn encode_remote_output(output: &RemoteToolOutput) -> Result<Vec<u8>, AgentError> {
    let bytes = serde_json::to_vec(output).map_err(|e| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to encode remote tool result: {e}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    if bytes.len() as u64 > MAX_REMOTE_TOOL_RESULT_BYTES {
        return Err(AgentError {
            kind: AgentErrorKind::OutputLimitExceeded,
            message: "the read result exceeds the remote tool result limit".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    Ok(bytes)
}

/// Runs a single server-stamped read envelope against the in-process device agent
/// (Default / DeskServer, where the daemon process itself reads). A ServiceDaemon
/// gates the same way and has the session worker run [`read_redacted`].
pub struct EdgeReadInvoker {
    agent: Arc<LocalDeviceAgent>,
    redactor: Arc<dyn Redactor>,
    settings: Arc<SharedSettings>,
}

impl EdgeReadInvoker {
    pub fn new(
        agent: Arc<LocalDeviceAgent>,
        redactor: Arc<dyn Redactor>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self {
            agent,
            redactor,
            settings,
        }
    }

    /// Gate, invoke and redact `envelope`. A gate or read error, or a redaction
    /// failure, returns `Err` (the router turns it into a wholesale
    /// `RemoteToolResponse::Error`, never leaking raw output).
    pub async fn invoke_redacted(
        &self,
        envelope: AgentEnvelope,
    ) -> Result<RemoteToolOutput, AgentError> {
        let policy = self.settings.read().await.collection_policy;
        let cap = gate_remote_read(&envelope, &policy)?;
        read_redacted(&self.agent, self.redactor.as_ref(), envelope, cap).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_adapter::redaction::RegexRedactor;
    use crate::model::settings::Settings;
    use desk_agent_protocol::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        ContextKind, ExecutionMode, LogRecentParams, OperationInput, ProtocolVersion,
        ReadContextInput, RequestId, SystemInfoParams, TargetRef,
    };
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
    use std::io::Cursor;

    fn settings(allow_logs: bool, allow_screen: bool) -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.collection_policy.allow_logs = allow_logs;
        s.collection_policy.allow_screen = allow_screen;
        Arc::new(SharedSettings::from(s))
    }

    fn read_envelope(cap_input: ContextKind, granted: Capability) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("rt-1".into()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: "manager".into(),
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: Some("example".into()),
                model_name: Some("m".into()),
                adapter: Some("manager".into()),
            },
            scope: AgentScope {
                granted: vec![granted],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::ReadContext(ReadContextInput { kind: cap_input }),
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("remote read".into()),
            },
        }
    }

    /// A granted system-info read runs against the real agent and returns a
    /// redacted `Ok` outcome (succeeds on every CI host).
    #[tokio::test]
    async fn invokes_and_returns_redacted_outcome() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::SystemInfo,
        );
        let output = invoker.invoke_redacted(envelope).await.expect("read ok");
        let json = serde_json::to_string(&output.outcome).unwrap();
        assert!(json.contains("SystemInfo") || json.contains("hostname"));
        assert!(output.image.is_none());
    }

    /// An envelope whose granted scope does not cover the operation is denied by
    /// the edge's re-check — final say, an `Err`, never raw output.
    #[tokio::test]
    async fn denies_operation_outside_granted_scope() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        // Operation reads system info, but the scope grants only process.list.
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::ProcessList,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("scope mismatch must be denied");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
        assert!(err.safe_for_model);
    }

    /// The device's local policy has final say: a log read is denied when the host
    /// has `allow_logs = false`, even though the manager scope granted it.
    #[tokio::test]
    async fn local_policy_denies_logs_when_disabled() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(false, false),
        );
        let envelope = read_envelope(
            ContextKind::LogRecent(LogRecentParams::default()),
            Capability::LogRecent,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("local policy must deny logs");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn local_policy_denies_screen_when_disabled() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        let envelope = read_envelope(
            ContextKind::ScreenCaptureCurrent(Default::default()),
            Capability::ScreenCaptureCurrent,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("screen policy must deny before capture");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
        assert!(err.safe_for_model);
    }

    /// The gate a ServiceDaemon applies before handing a read to the session
    /// worker: the granted capability passes, everything else is refused.
    #[test]
    fn the_gate_returns_the_granted_capability_and_refuses_the_rest() {
        let open = CollectionPolicySettings {
            allow_logs: true,
            allow_screen: true,
        };
        let system = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::SystemInfo,
        );
        assert_eq!(
            gate_remote_read(&system, &open).unwrap(),
            Capability::SystemInfo
        );
        let outside = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::LogRecent,
        );
        assert_eq!(
            gate_remote_read(&outside, &open).unwrap_err().kind,
            AgentErrorKind::PermissionDenied
        );
        let logs = read_envelope(
            ContextKind::LogRecent(LogRecentParams::default()),
            Capability::LogRecent,
        );
        let no_logs = CollectionPolicySettings {
            allow_logs: false,
            ..open
        };
        assert!(gate_remote_read(&logs, &open).is_ok());
        assert_eq!(
            gate_remote_read(&logs, &no_logs).unwrap_err().kind,
            AgentErrorKind::PermissionDenied
        );
        let screen = read_envelope(
            ContextKind::ScreenCaptureCurrent(Default::default()),
            Capability::ScreenCaptureCurrent,
        );
        let no_screen = CollectionPolicySettings {
            allow_screen: false,
            ..open
        };
        assert_eq!(
            gate_remote_read(&screen, &no_screen).unwrap_err().kind,
            AgentErrorKind::PermissionDenied
        );
    }

    /// The worker's half returns the same redacted output the in-process
    /// invoker produces, as the bytes the daemon chunks.
    #[tokio::test]
    async fn a_worker_read_returns_the_encoded_redacted_output() {
        let agent = LocalDeviceAgent::with_settings(settings(false, false));
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::SystemInfo,
        );
        let RemoteReadResult::Output(bytes) = read_for_daemon(&agent, envelope).await else {
            panic!("a system-info read succeeds on every host");
        };
        let output: RemoteToolOutput = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(output.outcome, AgentOutcome::Ok(_)));
        assert!(output.image.is_none());
    }

    #[test]
    fn an_encoded_result_never_exceeds_the_remote_tool_limit() {
        let small = RemoteToolOutput {
            outcome: AgentOutcome::Err(denied("small")),
            image: None,
            document_preview: None,
            document_preview_page: None,
        };
        let bytes = encode_remote_output(&small).unwrap();
        assert_eq!(
            serde_json::from_slice::<RemoteToolOutput>(&bytes).unwrap(),
            small
        );
        let large = RemoteToolOutput {
            outcome: AgentOutcome::Err(denied("x".repeat(MAX_REMOTE_TOOL_RESULT_BYTES as usize))),
            ..small
        };
        assert_eq!(
            encode_remote_output(&large).unwrap_err().kind,
            AgentErrorKind::OutputLimitExceeded
        );
    }

    #[test]
    fn sanitized_screen_output_carries_only_the_fitted_attachment() {
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(ImageBuffer::from_pixel(4, 2, Rgb([4, 8, 16])))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let outcome = AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(
                desk_agent_protocol::ScreenCaptureOutput {
                    frame_observation: None,
                    window_geometry: None,
                    display: r"\\.\DISPLAY1".into(),
                    format: desk_agent_protocol::ImageFormat::Png,
                    width: 4,
                    height: 2,
                    dpi_x: 96,
                    dpi_y: 96,
                    window: None,
                    image: png,
                    truncated: false,
                },
            ),
        ));

        let output = sanitize_remote_output(Capability::ScreenCaptureCurrent, outcome).unwrap();
        let AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &output.outcome
        else {
            panic!("unexpected screen output shape");
        };
        assert!(shot.image.is_empty());
        let image = output.image.expect("fitted image attachment");
        assert!(image.data_url.starts_with("data:image/jpeg;base64,"));
        assert!(image.decoded_bytes <= desk_diagnose_core::image_input::MAX_IMAGE_DECODED_BYTES);
        let json = serde_json::to_string(&output.outcome).unwrap();
        assert!(!json.contains("137,80,78,71"));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use desk_agent_protocol::document_conversion::{
        DocumentPreviewOutput, DocumentPreviewPageOutput,
    };
    use desk_agent_protocol::{OperationOutput, ReadContextOutput};

    fn valid_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut state = 7u32;
        let pixels = (0..128 * 128 * 3)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect::<Vec<_>>();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 128, 128);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
        bytes
    }

    fn preview(png: Vec<u8>) -> OperationOutput {
        OperationOutput::ReadContext(ReadContextOutput::DocumentPreview(DocumentPreviewOutput {
            preview_id: "pv_fixture".into(),
            source_digest_sha256: "a".repeat(64),
            page_count: 1,
            template_version: "test".into(),
            font_set_sha256: "b".repeat(64),
            engine: "test".into(),
            warnings: vec![],
            first_page: Some(DocumentPreviewPageOutput {
                preview_id: "pv_fixture".into(),
                page: 1,
                page_count: 1,
                png,
                width: 128,
                height: 128,
                pixels_per_point_milli: 1000,
            }),
        }))
    }

    #[test]
    fn large_valid_preview_is_checked_as_metadata_then_delivered_only_to_owner_ui() {
        let png = valid_png();
        assert!(png.len() > 32 * 1024);
        assert!(image::load_from_memory(&png).is_ok());
        let output = preview(png.clone());
        desk_diagnose_core::conversation_attachment::source::validate_output(&output).unwrap();
        let prepared =
            sanitize_remote_output(Capability::DocumentPreview, AgentOutcome::Ok(output)).unwrap();
        assert!(prepared.image.is_none());
        let owner = prepared.document_preview.as_ref().unwrap();
        assert!(owner.descriptor.first_page.is_none());
        assert!(!owner.preview_data_url.is_empty());
        desk_diagnose_core::image_input::validate_document_preview_frame(owner).unwrap();
        let AgentOutcome::Ok(OperationOutput::ReadContext(ReadContextOutput::DocumentPreview(
            descriptor,
        ))) = prepared.outcome
        else {
            panic!("a document descriptor is required")
        };
        assert!(descriptor.first_page.is_none());
        assert!(serde_json::to_vec(&descriptor).unwrap().len() < 32 * 1024);
    }

    #[test]
    fn independent_image_budget_and_page_metadata_still_reject_invalid_previews() {
        assert!(
            sanitize_remote_output(
                Capability::DocumentPreview,
                AgentOutcome::Ok(preview(vec![0; 400001]))
            )
            .is_err()
        );
        let mut output = preview(valid_png());
        let OperationOutput::ReadContext(ReadContextOutput::DocumentPreview(descriptor)) =
            &mut output
        else {
            unreachable!()
        };
        descriptor.first_page.as_mut().unwrap().width = 0;
        assert!(
            sanitize_remote_output(Capability::DocumentPreview, AgentOutcome::Ok(output)).is_err()
        );
    }
}
