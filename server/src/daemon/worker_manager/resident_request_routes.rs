//! Connection-less central requests sent to a resident session worker. Only the
//! worker a request went to may answer it; resident workers of other sessions
//! run as other users and must not stand in for each other.
use super::{ServiceToWorker, WorkerIncarnation, WorkerKey};
use desk_signal_facade::model::signal::SignalingType;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

/// Far longer than a recovery or terminal-list request takes; an unanswered
/// route only waits for this long.
const ROUTE_TTL: Duration = Duration::from_secs(600);
/// Bound on remembered routes. A full table evicts its oldest route rather than
/// refusing new requests.
const MAX_ROUTES: usize = 1024;

struct Route {
    key: WorkerKey,
    incarnation: WorkerIncarnation,
    /// The response type, which a worker also uses for its error reply.
    response: SignalingType,
    deadline: Instant,
}

#[derive(Default)]
pub(crate) struct ResidentRequestRoutes {
    routes: StdMutex<HashMap<String, Route>>,
}

/// The request id and response type of a central request that carries no
/// browser connection and is answered once. Requests with a connection are
/// admitted by that connection's session binding instead.
pub(super) fn central_reply_route(message: &ServiceToWorker) -> Option<(&str, SignalingType)> {
    match message {
        ServiceToWorker::ManageFileRecovery(payload) if payload.connection_id.is_none() => {
            Some((&payload.request_id, SignalingType::FileRecoveryManaged))
        }
        ServiceToWorker::ListTerminalCommands(payload) if payload.connection_id.is_none() => {
            Some((&payload.request_id, SignalingType::TerminalCommandsListed))
        }
        _ => None,
    }
}

impl ResidentRequestRoutes {
    pub(super) fn record(
        &self,
        request_id: &str,
        key: WorkerKey,
        incarnation: WorkerIncarnation,
        response: SignalingType,
    ) {
        self.record_at(request_id, key, incarnation, response, Instant::now());
    }

    fn record_at(
        &self,
        request_id: &str,
        key: WorkerKey,
        incarnation: WorkerIncarnation,
        response: SignalingType,
        now: Instant,
    ) {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes.retain(|_, route| route.deadline > now);
        if routes.len() >= MAX_ROUTES
            && !routes.contains_key(request_id)
            && let Some(oldest) = routes
                .iter()
                .min_by_key(|(_, route)| route.deadline)
                .map(|(id, _)| id.clone())
        {
            routes.remove(&oldest);
        }
        // The central issues request ids; a repeated id follows its latest send.
        routes.insert(
            request_id.to_owned(),
            Route {
                key,
                incarnation,
                response,
                deadline: now + ROUTE_TTL,
            },
        );
    }

    /// Undo a route whose request could not be sent.
    pub(super) fn forget(&self, request_id: &str) {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_id);
    }

    /// Admit a reply of `response` type when it comes from the worker the
    /// request went to, and consume the route: each request is answered once.
    pub(crate) fn take_reply(
        &self,
        request_id: &str,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
        response: SignalingType,
    ) -> bool {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        let matches = routes.get(request_id).is_some_and(|route| {
            route.key == *key
                && route.incarnation == incarnation
                && route.response == response
                && route.deadline > Instant::now()
        });
        if matches {
            routes.remove(request_id);
        }
        matches
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.routes.lock().unwrap().len()
    }
}

#[cfg(test)]
pub(crate) fn record_for_test(
    routes: &ResidentRequestRoutes,
    request_id: &str,
    key: WorkerKey,
    incarnation: WorkerIncarnation,
    response: SignalingType,
) {
    routes.record(request_id, key, incarnation, response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::{DesktopTarget, ListTerminalCommandsPayload, SessionKey};

    fn key(session: &str) -> WorkerKey {
        WorkerKey {
            session: SessionKey {
                platform_session_id: session.into(),
                session_generation: 1,
            },
            desktop: DesktopTarget::LinuxSession,
        }
    }

    #[test]
    fn only_the_dispatched_worker_answers_once_with_the_expected_type() {
        let routes = ResidentRequestRoutes::default();
        let incarnation = WorkerIncarnation(7);
        routes.record(
            "r1",
            key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged,
        );
        // Another session, a replaced worker or another reply type is refused.
        assert!(!routes.take_reply(
            "r1",
            &key("b"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.take_reply(
            "r1",
            &key("a"),
            WorkerIncarnation(8),
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.take_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.take_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        // The route is consumed by its reply.
        assert!(!routes.take_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.take_reply(
            "unknown",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
    }

    #[test]
    fn forgotten_and_expired_routes_admit_nothing() {
        let routes = ResidentRequestRoutes::default();
        let incarnation = WorkerIncarnation(1);
        routes.record(
            "sent",
            key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged,
        );
        routes.forget("sent");
        assert!(!routes.take_reply(
            "sent",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));

        let past = Instant::now() - ROUTE_TTL - Duration::from_secs(1);
        routes.record_at(
            "old",
            key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged,
            past,
        );
        assert!(!routes.take_reply(
            "old",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        // Recording again prunes expired routes.
        routes.record(
            "new",
            key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged,
        );
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn a_full_table_evicts_its_oldest_route_instead_of_refusing() {
        let routes = ResidentRequestRoutes::default();
        let incarnation = WorkerIncarnation(1);
        let start = Instant::now();
        for index in 0..MAX_ROUTES {
            routes.record_at(
                &format!("r{index}"),
                key("a"),
                incarnation,
                SignalingType::TerminalCommandsListed,
                start + Duration::from_millis(index as u64),
            );
        }
        routes.record(
            "latest",
            key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed,
        );
        assert_eq!(routes.len(), MAX_ROUTES);
        assert!(!routes.take_reply(
            "r0",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.take_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.take_reply(
            "latest",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
    }

    #[test]
    fn only_connection_less_recovery_and_terminal_lists_need_a_route() {
        let central = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
            request_id: "t1".into(),
            connection_id: None,
        });
        assert_eq!(
            central_reply_route(&central),
            Some(("t1", SignalingType::TerminalCommandsListed))
        );
        let browser = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
            request_id: "t2".into(),
            connection_id: Some("conn".into()),
        });
        assert_eq!(central_reply_route(&browser), None);
    }
}
