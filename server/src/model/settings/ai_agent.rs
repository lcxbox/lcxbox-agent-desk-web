//! Durable device-owned AI Agent product switch.

pub use desk_agent_protocol::ai_agent::{AiAgentSettings, AiAgentSettingsUpdate};

// Device initialization is separate from an unknown remote authority snapshot.
// Missing wire fields and unknown frontend projections must still fail closed.
pub(super) fn default_ai_agent_settings() -> AiAgentSettings {
    AiAgentSettings {
        revision: 0,
        enabled: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAgentSettingsUpdateError {
    RevisionConflict(AiAgentSettings),
    RevisionExhausted,
}

pub fn apply_ai_agent_settings_update(
    current: AiAgentSettings,
    update: AiAgentSettingsUpdate,
) -> Result<AiAgentSettings, AiAgentSettingsUpdateError> {
    if current.revision != update.expected_revision {
        return Err(AiAgentSettingsUpdateError::RevisionConflict(current));
    }
    Ok(AiAgentSettings {
        revision: current
            .revision
            .checked_add(1)
            .ok_or(AiAgentSettingsUpdateError::RevisionExhausted)?,
        enabled: update.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_configuration_enables_agent_by_default_and_preserves_explicit_off() {
        use crate::model::settings::Settings;
        assert!(Settings::default().ai_agent.enabled);
        let absent: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.ai_agent, default_ai_agent_settings());
        let disabled: Settings =
            serde_json::from_str(r#"{"ai_agent":{"revision":7,"enabled":false}}"#).unwrap();
        assert!(!disabled.ai_agent.enabled);
        assert_eq!(disabled.ai_agent.revision, 7);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert_eq!(restored.ai_agent, disabled.ai_agent);
    }

    #[test]
    fn default_is_revision_zero_and_disabled() {
        let settings = AiAgentSettings::default();
        assert_eq!(settings.revision, 0);
        assert!(!settings.enabled);
        assert_eq!(
            serde_json::from_str::<AiAgentSettings>("{}")
                .unwrap_err()
                .classify(),
            serde_json::error::Category::Data,
            "the public shared contract rejects incomplete snapshots"
        );
    }

    #[test]
    fn update_is_compare_and_set_and_revision_is_device_owned() {
        let current = AiAgentSettings {
            revision: 7,
            enabled: false,
        };
        assert_eq!(
            apply_ai_agent_settings_update(
                current,
                AiAgentSettingsUpdate {
                    expected_revision: 6,
                    enabled: true,
                },
            ),
            Err(AiAgentSettingsUpdateError::RevisionConflict(current))
        );
        assert_eq!(
            apply_ai_agent_settings_update(
                current,
                AiAgentSettingsUpdate {
                    expected_revision: 7,
                    enabled: true,
                },
            )
            .unwrap(),
            AiAgentSettings {
                revision: 8,
                enabled: true,
            }
        );
    }
}
