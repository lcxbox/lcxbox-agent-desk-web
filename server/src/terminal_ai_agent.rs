//! Daemon-side Terminal AI Agent frame sink.
//!
//! Terminal AI Agent is orchestrated by the central signaling brain; the daemon
//! only relays the resulting [`TerminalAiAgentEvent`] frames back to the asking
//! control end. This module wires the shared [`AgentStreamSink`] lifecycle→frame
//! mapping to the daemon's outbound signaling lane so a neutralized edge handler
//! can report a content-free error to the browser without re-deriving frame
//! serialization.

use desk_agent_protocol::terminal_ai_agent::TerminalAiAgentEvent;
use desk_diagnose_core::terminal_ai_agent::{AgentStreamSink, TerminalAgentFrameSink};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

/// Forwards each [`TerminalAiAgentEvent`] the shared [`AgentStreamSink`] emits
/// to the asking control end: it serializes the notification-style frame
/// (`response_state = None`) and broadcasts it over the daemon's outbound lane.
pub struct SignalingAgentFrames {
    outbound_tx: broadcast::Sender<String>,
    to_connection_id: Option<String>,
}

impl TerminalAgentFrameSink for SignalingAgentFrames {
    fn emit(&self, event: TerminalAiAgentEvent) {
        let data = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[agent] failed to serialise TerminalAiAgentEvent: {e}");
                return;
            }
        };
        let frame = SignalingModel::new(
            &event.request_id,
            SignalingType::TerminalAiAgentUpdated,
            None,
            self.to_connection_id.clone(),
            Some(data),
            None,
        );
        match serde_json::to_string(&frame) {
            Ok(text) => {
                let _ = self.outbound_tx.send(text);
            }
            Err(e) => {
                log::warn!("[agent] failed to serialise TerminalAiAgentEvent frame: {e}")
            }
        }
    }
}

/// The daemon's agent stream sink: the shared lifecycle→frame mapping wired to
/// the signaling outbound lane. The frame mapping itself lives in
/// [`desk_diagnose_core::terminal_ai_agent`] so it cannot drift from the manager.
pub type AgentTurnSink = AgentStreamSink<SignalingAgentFrames>;

/// Build a [`AgentTurnSink`] that streams a single request's frames back to the
/// asking control end (`to_connection_id`).
pub fn agent_signaling_sink(
    outbound_tx: broadcast::Sender<String>,
    to_connection_id: Option<String>,
    request_id: String,
) -> AgentTurnSink {
    AgentStreamSink::new(
        SignalingAgentFrames {
            outbound_tx,
            to_connection_id,
        },
        request_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentError, AgentErrorKind};

    /// An error emitted through the sink is serialized as a `TerminalAiAgentEvent`
    /// frame routed to the asking control end — the path a neutralized edge handler
    /// uses to report that agent is centralized.
    #[tokio::test]
    async fn sink_emits_error_frame_to_control_end() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut sink = agent_signaling_sink(tx, Some("conn-1".into()), "req-1".into());
        sink.emit_error(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "centralized".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });

        let text = rx.try_recv().expect("a frame was broadcast");
        let model: SignalingModel = serde_json::from_str(&text).unwrap();
        assert_eq!(model.signaling_type, SignalingType::TerminalAiAgentUpdated);
        let event = model
            .get_data_with_type::<TerminalAiAgentEvent>()
            .unwrap()
            .expect("the frame carries a TerminalAiAgentEvent");
        assert_eq!(event.request_id, "req-1");
    }
}
