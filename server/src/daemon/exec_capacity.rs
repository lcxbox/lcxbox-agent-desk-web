//! The host's own ceiling on how many commands may run at once.
//!
//! The manager schedules executions across a fleet and can cap them centrally,
//! but that cap only binds work the manager dispatched. A control end talking to
//! an open-source signal server reaches this host directly, and a host that
//! trusted the central cap alone would accept as many concurrent commands as it
//! was sent. The limit therefore lives here too, applied to every path.
//!
//! # Why this is not the ledger
//!
//! The ledger answers "did this dispatch already happen", and its answers are
//! permanent so a redelivery can never run twice. Capacity asks "how many are
//! running *now*", and stale answers are actively harmful: an execution the host
//! lost track of would occupy a slot for ever and the host would slowly refuse
//! all work. The two want opposite lifetimes, so they are kept apart.
//!
//! # Self-healing
//!
//! A slot is held under a deadline derived from the command's own timeout rather
//! than released only on completion. If a worker dies without reporting, its
//! slots free themselves once the commands could no longer legitimately still be
//! running — no crash-recovery plumbing required, and no path by which a lost
//! execution permanently shrinks the host's capacity.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desk_ipc_protocol::message::WorkerKey;

use super::worker_manager::{ExecWorkerTarget, WorkerIncarnation};

/// Frozen dispatch destination and controlling channel. A central cancellation
/// has no browser channel; it must still target the originally admitted worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBinding {
    pub target: ExecWorkerTarget,
    pub owner_connection_id: Option<String>,
    pub daemon_pty: bool,
}

struct Slot {
    deadline: Instant,
    binding: Option<ExecBinding>,
}

/// Grace added to a command's own timeout before its slot is reclaimed, covering
/// the spawn, the snapshot scans around a mutating AI command (which do not
/// count against its timeout), the result's trip back, and clock jitter.
/// Generous on purpose: reclaiming a slot early would let one more command run
/// than the ceiling allows, which is the failure this exists to prevent.
const SLOT_GRACE: Duration =
    Duration::from_millis(30_000 + desk_agent_protocol::exec::EXEC_SNAPSHOT_TIME_BUDGET_MS as u64);

/// Refusal returned when the host is already at its ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityFull {
    pub running: usize,
    pub limit: usize,
}

impl std::fmt::Display for CapacityFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this device is already running {} of {} permitted commands",
            self.running, self.limit
        )
    }
}

/// Live count of executions this host has accepted and not yet accounted for.
#[derive(Default)]
pub struct ExecCapacity {
    /// Generation → the moment its slot may be reclaimed even without a report.
    slots: Mutex<HashMap<String, Slot>>,
}

impl ExecCapacity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a slot for `generation`, or refuse if the host is at `limit`.
    ///
    /// Re-admitting a generation that already holds a slot is not an error and
    /// does not consume a second one: the ledger has already decided whether that
    /// dispatch may proceed, and capacity must not double-count one execution.
    /// Returns whether this call created a slot, so a duplicate ledger lookup
    /// cannot release the original execution's destination or capacity.
    pub fn try_admit(
        &self,
        generation: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<bool, CapacityFull> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        slots.retain(|_, slot| slot.deadline > now);

        if slots.contains_key(generation) {
            return Ok(false);
        }
        if slots.len() >= limit {
            return Err(CapacityFull {
                running: slots.len(),
                limit,
            });
        }
        slots.insert(
            generation.to_string(),
            Slot {
                deadline: now + timeout + SLOT_GRACE,
                binding: None,
            },
        );
        Ok(true)
    }

    /// Bind before native dispatch. A lost or rebound slot cannot dispatch to a
    /// replacement worker, even if the selected desktop has the same name.
    pub fn bind(&self, generation: &str, binding: ExecBinding) -> Result<(), String> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots
            .get_mut(generation)
            .filter(|slot| slot.deadline > Instant::now())
            .ok_or_else(|| "execution capacity reservation is no longer current".to_string())?;
        match slot.binding.as_ref() {
            Some(current) if current != &binding => {
                Err("execution destination cannot be rebound".into())
            }
            Some(_) => Ok(()),
            None => {
                slot.binding = Some(binding);
                Ok(())
            }
        }
    }

    pub fn binding_for_owner(
        &self,
        generation: &str,
        owner_connection_id: Option<&str>,
    ) -> Option<ExecBinding> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .get(generation)
            .filter(|slot| slot.deadline > Instant::now())
            .and_then(|slot| slot.binding.as_ref())
            .filter(|binding| binding.owner_connection_id.as_deref() == owner_connection_id)
            .cloned()
    }

    /// The frozen destination of an execution, whether or not its slot has
    /// passed its deadline. Admitting a resident worker's connection-less
    /// output checks it; an expired slot remains until the next admission
    /// prunes it, so a late result is admitted only on a best-effort basis.
    pub fn binding(&self, generation: &str) -> Option<ExecBinding> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(generation)
            .and_then(|slot| slot.binding.clone())
    }

    /// Detach the central executions a worker instance was running, now that
    /// the instance is gone, and return their generations.
    ///
    /// Only the bindings go: each slot stays until its original deadline,
    /// because the command may outlive the worker that started it and freeing
    /// the slot early would let one more command run than the ceiling allows.
    /// Expired slots are included, matching [`Self::binding`]. A daemon-run root
    /// PTY and a browser-owned execution do not depend on the worker for their
    /// result, so neither is touched.
    pub fn take_bound_to(
        &self,
        worker_key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
    ) -> Vec<String> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter_mut()
            .filter(|(_, slot)| {
                slot.binding.as_ref().is_some_and(|binding| {
                    !binding.daemon_pty
                        && binding.owner_connection_id.is_none()
                        && binding.target.worker_key.as_ref() == worker_key
                        && binding.target.source_incarnation == incarnation
                })
            })
            .map(|(generation, slot)| {
                slot.binding = None;
                generation.clone()
            })
            .collect()
    }

    /// Give back a slot once the execution is accounted for — it finished, or it
    /// never started.
    pub fn release(&self, generation: &str) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(generation);
    }

    /// How many slots are held right now, ignoring any whose deadline has passed.
    pub fn in_flight(&self) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        slots.retain(|_, slot| slot.deadline > now);
        slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(10);

    /// A slot outlives the command's own timeout by the snapshot scans around
    /// a mutating AI command plus the spawn and result margin.
    #[test]
    fn the_slot_grace_covers_the_snapshot_budget() {
        assert_eq!(
            SLOT_GRACE,
            Duration::from_secs(30)
                + Duration::from_millis(u64::from(
                    desk_agent_protocol::exec::EXEC_SNAPSHOT_TIME_BUDGET_MS
                ))
        );
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 1, Duration::ZERO).is_ok());
        assert_eq!(
            cap.in_flight(),
            1,
            "a zero-timeout slot is held for the grace"
        );
    }

    /// The ceiling is enforced, and a released slot is immediately reusable.
    #[test]
    fn the_ceiling_bounds_concurrent_commands() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 2, T).is_ok());
        assert!(cap.try_admit("b", 2, T).is_ok());
        assert_eq!(
            cap.try_admit("c", 2, T),
            Err(CapacityFull {
                running: 2,
                limit: 2
            })
        );

        cap.release("a");
        assert!(cap.try_admit("c", 2, T).is_ok());
        assert_eq!(cap.in_flight(), 2);
    }

    /// Re-admitting the same dispatch does not consume a second slot; whether a
    /// redelivery may proceed at all is the ledger's decision, not capacity's.
    #[test]
    fn one_dispatch_never_holds_two_slots() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 1, T).is_ok());
        assert!(cap.try_admit("a", 1, T).is_ok());
        assert_eq!(cap.in_flight(), 1);
    }

    /// A slot whose command could no longer be running is reclaimed without any
    /// report, so a worker that died without answering cannot permanently shrink
    /// the host's capacity.
    #[test]
    fn a_lost_execution_does_not_hold_its_slot_for_ever() {
        let cap = ExecCapacity::new();
        // A command whose deadline is already in the past: the worker vanished.
        cap.slots.lock().unwrap().insert(
            "lost".to_string(),
            Slot {
                deadline: Instant::now() - Duration::from_secs(1),
                binding: None,
            },
        );

        assert_eq!(cap.in_flight(), 0, "the stale slot was not reclaimed");
        assert!(cap.try_admit("fresh", 1, T).is_ok());
    }

    fn central_binding(worker: Option<&str>, incarnation: u64) -> ExecBinding {
        ExecBinding {
            target: ExecWorkerTarget {
                worker_key: worker.map(|session| WorkerKey {
                    session: desk_ipc_protocol::message::SessionKey {
                        platform_session_id: session.into(),
                        session_generation: 1,
                    },
                    desktop: desk_ipc_protocol::message::DesktopTarget::LinuxSession,
                }),
                source_incarnation: WorkerIncarnation::for_test(incarnation),
                session_target_id: worker.unwrap_or("legacy").into(),
                registration_generation: 1,
                wire_worker_incarnation: incarnation,
            },
            owner_connection_id: None,
            daemon_pty: false,
        }
    }

    /// Only the central executions of the departed worker instance are
    /// detached; their slots keep counting against the ceiling until their own
    /// deadlines, including a slot already past its deadline.
    #[test]
    fn a_departed_worker_releases_its_bindings_but_not_its_slots() {
        let cap = ExecCapacity::new();
        let lost = central_binding(Some("s1"), 7);
        let mut root_pty = lost.clone();
        root_pty.daemon_pty = true;
        let mut browser = lost.clone();
        browser.owner_connection_id = Some("browser".into());
        let mut replacement = lost.clone();
        replacement.target.source_incarnation = WorkerIncarnation::for_test(8);
        let other_session = central_binding(Some("s2"), 7);
        let legacy = central_binding(None, 7);
        for (generation, binding) in [
            ("lost", &lost),
            ("expired", &lost),
            ("root-pty", &root_pty),
            ("browser", &browser),
            ("replacement", &replacement),
            ("other-session", &other_session),
            ("legacy", &legacy),
        ] {
            assert!(cap.try_admit(generation, 8, T).unwrap());
            cap.bind(generation, binding.clone()).unwrap();
        }
        cap.slots
            .lock()
            .unwrap()
            .get_mut("expired")
            .unwrap()
            .deadline = Instant::now() - Duration::from_secs(1);

        let mut taken = cap.take_bound_to(
            lost.target.worker_key.as_ref(),
            WorkerIncarnation::for_test(7),
        );
        taken.sort();
        assert_eq!(taken, ["expired", "lost"]);
        assert_eq!(cap.binding("lost"), None);
        assert_eq!(cap.binding("expired"), None);
        for (generation, binding) in [
            ("root-pty", &root_pty),
            ("browser", &browser),
            ("replacement", &replacement),
            ("other-session", &other_session),
            ("legacy", &legacy),
        ] {
            assert_eq!(
                cap.binding(generation).as_ref(),
                Some(binding),
                "{generation}"
            );
        }
        assert_eq!(cap.in_flight(), 6, "the live detached slot still counts");
        assert_eq!(
            cap.try_admit("next", 6, T),
            Err(CapacityFull {
                running: 6,
                limit: 6
            })
        );
        assert!(
            cap.take_bound_to(
                lost.target.worker_key.as_ref(),
                WorkerIncarnation::for_test(7)
            )
            .is_empty(),
            "a generation is detached only once"
        );
        assert_eq!(
            cap.take_bound_to(None, WorkerIncarnation::for_test(7)),
            ["legacy"]
        );
    }

    /// A limit of zero refuses everything rather than admitting one.
    #[test]
    fn a_zero_limit_admits_nothing() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 0, T).is_err());
    }
    #[test]
    fn execution_destination_and_owner_are_immutable_and_expire_with_slot() {
        let cap = ExecCapacity::new();
        let binding = ExecBinding {
            target: ExecWorkerTarget {
                worker_key: None,
                source_incarnation: super::super::worker_manager::WorkerIncarnation::for_test(1),
                session_target_id: "session".into(),
                registration_generation: 0,
                wire_worker_incarnation: 0,
            },
            owner_connection_id: Some("browser".into()),
            daemon_pty: false,
        };
        assert!(cap.bind("generation", binding.clone()).is_err());
        assert!(cap.try_admit("generation", 1, T).unwrap());
        cap.bind("generation", binding.clone()).unwrap();
        assert!(!cap.try_admit("generation", 1, T).unwrap());
        assert_eq!(
            cap.binding_for_owner("generation", Some("browser")),
            Some(binding.clone())
        );
        assert!(cap.binding_for_owner("generation", None).is_none());
        let mut changed = binding.clone();
        changed.target.source_incarnation =
            super::super::worker_manager::WorkerIncarnation::for_test(2);
        assert!(cap.bind("generation", changed).is_err());
        cap.slots
            .lock()
            .unwrap()
            .get_mut("generation")
            .unwrap()
            .deadline = Instant::now() - T;
        assert!(
            cap.binding_for_owner("generation", Some("browser"))
                .is_none()
        );
        assert!(cap.bind("generation", binding).is_err());
    }
}
