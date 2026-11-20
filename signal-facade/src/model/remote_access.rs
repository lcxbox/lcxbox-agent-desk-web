use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateRemoteAccessLockData {
    pub request_id: String,
    pub lock_id: Option<String>,
    pub state_version: u64,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoteAccessLockUpdatedData {
    pub request_id: String,
    pub lock_id: Option<String>,
    pub state_version: u64,
    pub locked: bool,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminateRemotePeerData {
    pub operation_id: String,
    pub target_connection_id: String,
    /// Why the host ends this peer; decides the close code the browser sees.
    /// Absent from older hosts, which only ever terminated.
    #[serde(default)]
    pub reason: PeerEvictionReason,
}

/// Why a host evicts a browser peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerEvictionReason {
    /// The host ended the connection (disconnect, remote-access lock,
    /// revocation). The browser should not reconnect on its own.
    #[default]
    HostTerminated,
    /// The desktop session the connection was bound to left the console
    /// (fast user switch, sign-out). The browser may reconnect and be
    /// admitted to the session now on the console.
    SessionChanged,
    /// A reason this build does not know; treated as `HostTerminated`.
    #[serde(other)]
    Unknown,
}

impl PeerEvictionReason {
    /// WebSocket close code sent to the evicted browser.
    pub fn close_code(self) -> u16 {
        match self {
            Self::SessionChanged => PEER_CLOSE_SESSION_CHANGED,
            Self::HostTerminated | Self::Unknown => PEER_CLOSE_HOST_TERMINATED,
        }
    }
}

/// Close code for a browser socket the host terminated.
pub const PEER_CLOSE_HOST_TERMINATED: u16 = 4001;
/// Close code for a browser socket whose desktop session left the console.
pub const PEER_CLOSE_SESSION_CHANGED: u16 = 4002;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerEvictionOutcome {
    Delivered,
    Scheduled,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemotePeerTerminationResolvedData {
    pub operation_id: String,
    pub target_connection_id: String,
    pub outcome: PeerEvictionOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_update_and_updated_response_round_trip() {
        let request = UpdateRemoteAccessLockData {
            request_id: "req-1".into(),
            lock_id: Some("lock-1".into()),
            state_version: 7,
            locked: true,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<UpdateRemoteAccessLockData>(&encoded).unwrap(),
            request
        );

        let updated = RemoteAccessLockUpdatedData {
            request_id: "req-1".into(),
            lock_id: Some("lock-1".into()),
            state_version: 7,
            locked: true,
            generation: 3,
        };
        let encoded = serde_json::to_string(&updated).unwrap();
        assert_eq!(
            serde_json::from_str::<RemoteAccessLockUpdatedData>(&encoded).unwrap(),
            updated
        );
    }
}

#[cfg(test)]
mod eviction_reason_tests {
    use super::*;

    #[test]
    fn an_eviction_without_a_reason_is_a_host_termination() {
        let data: TerminateRemotePeerData =
            serde_json::from_str(r#"{"operation_id":"o","target_connection_id":"c"}"#).unwrap();
        assert_eq!(data.reason, PeerEvictionReason::HostTerminated);
        assert_eq!(data.reason.close_code(), PEER_CLOSE_HOST_TERMINATED);
    }

    #[test]
    fn a_session_change_closes_with_its_own_code_and_unknown_reasons_terminate() {
        let data: TerminateRemotePeerData = serde_json::from_str(
            r#"{"operation_id":"o","target_connection_id":"c","reason":"session_changed"}"#,
        )
        .unwrap();
        assert_eq!(data.reason.close_code(), PEER_CLOSE_SESSION_CHANGED);
        let future: TerminateRemotePeerData = serde_json::from_str(
            r#"{"operation_id":"o","target_connection_id":"c","reason":"something_new"}"#,
        )
        .unwrap();
        assert_eq!(future.reason, PeerEvictionReason::Unknown);
        assert_eq!(future.reason.close_code(), PEER_CLOSE_HOST_TERMINATED);
    }
}
