//! Process-local projection of the device-owned AI Agent switch.
//!
//! The durable authority lives in the host settings file. The embedded OSS
//! signal reads this projection only to reject new work without an async lock;
//! it never persists or increments the revision itself.

use std::sync::{Arc, OnceLock, RwLock};

use desk_agent_protocol::ai_agent::AiAgentSettings;

#[derive(Debug, Default)]
pub struct AiAgentGate {
    state: RwLock<AiAgentSettings>,
}

impl AiAgentGate {
    pub fn new(state: AiAgentSettings) -> Self {
        Self {
            state: RwLock::new(state),
        }
    }

    pub fn snapshot(&self) -> AiAgentSettings {
        *self.state.read().expect("AI agent gate")
    }

    pub fn is_enabled(&self) -> bool {
        self.snapshot().enabled
    }

    /// Replace the runtime projection after the device has durably committed
    /// this exact snapshot.
    pub fn replace(&self, state: AiAgentSettings) {
        *self.state.write().expect("AI agent gate") = state;
    }
}

pub fn global_ai_agent_gate() -> Arc<AiAgentGate> {
    static GATE: OnceLock<Arc<AiAgentGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(AiAgentGate::default()))
        .clone()
}

#[cfg(test)]
pub(crate) fn enable_test_host() {
    global_ai_agent_gate().replace(AiAgentSettings {
        revision: 1,
        enabled: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fail_closed_and_replaces_exact_snapshot() {
        let gate = AiAgentGate::default();
        assert!(!gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 0);

        gate.replace(AiAgentSettings {
            revision: 4,
            enabled: true,
        });
        assert!(gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 4);
    }
}
