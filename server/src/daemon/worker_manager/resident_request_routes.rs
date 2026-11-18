//! Connection-less central requests sent to a resident session worker. Only the
//! worker a request went to may answer it; resident workers of other sessions
//! run as other users and must not stand in for each other. A Computer Action's
//! record also tells a later stop request which worker runs it.
use super::{ServiceToWorker, WorkerIncarnation, WorkerKey};
use desk_signal_facade::model::signal::SignalingType;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

/// Far longer than a recovery, terminal-list or read request takes, and than a
/// Computer Action is expected to run; an unanswered route only waits this long.
const ROUTE_TTL: Duration = Duration::from_secs(600);
/// Bound on remembered routes. A full table evicts its oldest route rather than
/// refusing new requests.
const MAX_ROUTES: usize = 1024;

struct Route {
    key: WorkerKey,
    incarnation: WorkerIncarnation,
    expected: ExpectedReply,
    deadline: Instant,
}

/// The replies a request may receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExpectedReply {
    /// The final reply, which consumes the route. Also the type a worker uses
    /// for its error reply.
    pub(crate) terminal: SignalingType,
    /// An earlier reply that may precede the final one, admitted without
    /// consuming the route.
    pub(crate) interim: Option<SignalingType>,
}

impl ExpectedReply {
    const fn single(terminal: SignalingType) -> Self {
        Self {
            terminal,
            interim: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ResidentRequestRoutes {
    routes: StdMutex<HashMap<String, Route>>,
}

/// The request id and expected replies of a central request that carries no
/// browser connection. Requests with a connection are admitted by that
/// connection's session binding instead.
pub(super) fn central_reply_route(message: &ServiceToWorker) -> Option<(&str, ExpectedReply)> {
    let single = |request_id, terminal| Some((request_id, ExpectedReply::single(terminal)));
    match message {
        ServiceToWorker::ManageFileRecovery(payload) if payload.connection_id.is_none() => single(
            payload.request_id.as_str(),
            SignalingType::FileRecoveryManaged,
        ),
        ServiceToWorker::ListTerminalCommands(payload) if payload.connection_id.is_none() => {
            single(
                payload.request_id.as_str(),
                SignalingType::TerminalCommandsListed,
            )
        }
        ServiceToWorker::InvokeRemoteRead(payload) => single(
            payload.request_id.as_str(),
            SignalingType::RemoteToolOutputUpdated,
        ),
        // A stop request is answered under its own id.
        ServiceToWorker::ComputerActionCancel(payload) if payload.connection_id.is_none() => {
            single(
                payload.request_id.as_str(),
                SignalingType::ComputerActionStateReported,
            )
        }
        // The id is the execution generation; a stop request finds it by that.
        ServiceToWorker::ComputerActionPlan(payload) if payload.connection_id.is_none() => Some((
            payload.request_id.as_str(),
            ExpectedReply {
                terminal: SignalingType::ComputerActionCompleted,
                interim: Some(SignalingType::ComputerActionStarted),
            },
        )),
        _ => None,
    }
}

impl ResidentRequestRoutes {
    pub(super) fn record(
        &self,
        request_id: &str,
        key: WorkerKey,
        incarnation: WorkerIncarnation,
        expected: ExpectedReply,
    ) {
        self.record_at(request_id, key, incarnation, expected, Instant::now());
    }

    fn record_at(
        &self,
        request_id: &str,
        key: WorkerKey,
        incarnation: WorkerIncarnation,
        expected: ExpectedReply,
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
                expected,
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

    /// Admit a reply of `reply` type when it comes from the worker the request
    /// went to. The final reply consumes the route, so a request is finished
    /// once; an interim reply leaves it in place.
    pub(crate) fn admit_reply(
        &self,
        request_id: &str,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
        reply: SignalingType,
    ) -> bool {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        let Some(route) = routes.get(request_id).filter(|route| {
            route.key == *key && route.incarnation == incarnation && route.deadline > Instant::now()
        }) else {
            return false;
        };
        if route.expected.terminal == reply {
            routes.remove(request_id);
            true
        } else {
            route.expected.interim == Some(reply)
        }
    }

    /// The worker still running the Computer Action with this execution
    /// generation, for a stop request that must reach only that worker. Other
    /// kinds of routes are never returned.
    pub(super) fn dispatched_action_worker(
        &self,
        generation: &str,
    ) -> Option<(WorkerKey, WorkerIncarnation)> {
        let routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes
            .get(generation)
            .filter(|route| {
                route.expected.terminal == SignalingType::ComputerActionCompleted
                    && route.deadline > Instant::now()
            })
            .map(|route| (route.key.clone(), route.incarnation))
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
    expected: ExpectedReply,
) {
    routes.record(request_id, key, incarnation, expected);
}

#[cfg(test)]
impl ExpectedReply {
    pub(crate) const fn single_for_test(terminal: SignalingType) -> Self {
        Self::single(terminal)
    }
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
            ExpectedReply::single(SignalingType::FileRecoveryManaged),
        );
        // Another session, a replaced worker or another reply type is refused.
        assert!(!routes.admit_reply(
            "r1",
            &key("b"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.admit_reply(
            "r1",
            &key("a"),
            WorkerIncarnation(8),
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.admit_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.admit_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        // The route is consumed by its reply.
        assert!(!routes.admit_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::FileRecoveryManaged
        ));
        assert!(!routes.admit_reply(
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
            ExpectedReply::single(SignalingType::FileRecoveryManaged),
        );
        routes.forget("sent");
        assert!(!routes.admit_reply(
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
            ExpectedReply::single(SignalingType::FileRecoveryManaged),
            past,
        );
        assert!(!routes.admit_reply(
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
            ExpectedReply::single(SignalingType::FileRecoveryManaged),
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
                ExpectedReply::single(SignalingType::TerminalCommandsListed),
                start + Duration::from_millis(index as u64),
            );
        }
        routes.record(
            "latest",
            key("a"),
            incarnation,
            ExpectedReply::single(SignalingType::TerminalCommandsListed),
        );
        assert_eq!(routes.len(), MAX_ROUTES);
        assert!(!routes.admit_reply(
            "r0",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.admit_reply(
            "r1",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
        assert!(routes.admit_reply(
            "latest",
            &key("a"),
            incarnation,
            SignalingType::TerminalCommandsListed
        ));
    }

    #[test]
    fn an_action_admits_its_start_and_finishes_with_its_completion() {
        let routes = ResidentRequestRoutes::default();
        let incarnation = WorkerIncarnation(3);
        let action = ExpectedReply {
            terminal: SignalingType::ComputerActionCompleted,
            interim: Some(SignalingType::ComputerActionStarted),
        };
        routes.record("gen-1", key("a"), incarnation, action);
        assert_eq!(
            routes.dispatched_action_worker("gen-1"),
            Some((key("a"), incarnation))
        );
        // The start does not finish the action, and only its worker may send it.
        for _ in 0..2 {
            assert!(routes.admit_reply(
                "gen-1",
                &key("a"),
                incarnation,
                SignalingType::ComputerActionStarted
            ));
        }
        assert!(!routes.admit_reply(
            "gen-1",
            &key("b"),
            incarnation,
            SignalingType::ComputerActionStarted
        ));
        assert!(!routes.admit_reply(
            "gen-1",
            &key("a"),
            incarnation,
            SignalingType::ComputerActionStateReported
        ));
        assert!(routes.admit_reply(
            "gen-1",
            &key("a"),
            incarnation,
            SignalingType::ComputerActionCompleted
        ));
        // Finished: nothing more is admitted and a stop request finds no worker.
        assert!(!routes.admit_reply(
            "gen-1",
            &key("a"),
            incarnation,
            SignalingType::ComputerActionStarted
        ));
        assert_eq!(routes.dispatched_action_worker("gen-1"), None);
    }

    #[test]
    fn only_a_running_action_locates_a_worker_for_a_stop_request() {
        let routes = ResidentRequestRoutes::default();
        let incarnation = WorkerIncarnation(1);
        routes.record(
            "read-1",
            key("a"),
            incarnation,
            ExpectedReply::single(SignalingType::RemoteToolOutputUpdated),
        );
        assert_eq!(routes.dispatched_action_worker("read-1"), None);
        assert_eq!(routes.dispatched_action_worker("unknown"), None);
        let past = Instant::now() - ROUTE_TTL - Duration::from_secs(1);
        routes.record_at(
            "gen-old",
            key("a"),
            incarnation,
            ExpectedReply {
                terminal: SignalingType::ComputerActionCompleted,
                interim: Some(SignalingType::ComputerActionStarted),
            },
            past,
        );
        assert_eq!(routes.dispatched_action_worker("gen-old"), None);
    }

    #[test]
    fn only_connection_less_recovery_and_terminal_lists_need_a_route() {
        let central = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
            request_id: "t1".into(),
            connection_id: None,
        });
        assert_eq!(
            central_reply_route(&central),
            Some((
                "t1",
                ExpectedReply::single(SignalingType::TerminalCommandsListed)
            ))
        );
        let browser = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
            request_id: "t2".into(),
            connection_id: Some("conn".into()),
        });
        assert_eq!(central_reply_route(&browser), None);
    }
}
