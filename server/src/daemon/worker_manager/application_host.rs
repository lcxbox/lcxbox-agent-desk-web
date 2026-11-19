//! Start the ordinary-user application host for a Windows session-user
//! resident worker.
//!
//! That worker runs on the user's elevated token (for UIPI) and cannot create
//! an ordinary-user process: without SeTcb its linked token is only
//! identification-level. The daemon, as LocalSystem, starts its own executable
//! in host mode with the ordinary token Windows created at logon. It never
//! starts anything else: the command line is the daemon's executable, the host
//! mode, a validated pipe name and the requesting worker's PID; session and
//! user are the daemon's own facts about that worker. The worker then talks to
//! the host over its private pipe exactly as when it starts the host itself.
use super::*;
use desk_ipc_protocol::message::{
    ApplicationHostStart, ApplicationHostStartOutcome, ApplicationHostStartRefusal,
    ApplicationHostStartResult,
};

/// Live hosts per worker; a host exits within 50 s on its own.
pub(super) const MAX_LIVE_HOSTS: usize = 4;

/// A started host the daemon still holds, to bound how many run.
pub(super) trait LiveHost: Send {
    fn running(&self) -> bool;
}

/// Hosts started for one worker; dropped with its `WorkerHandle`.
#[derive(Default)]
pub(super) struct HostSlots {
    live: Vec<Box<dyn LiveHost>>,
    reserved: usize,
}

impl HostSlots {
    /// Reserve a slot before the host is created, so concurrent requests
    /// cannot exceed the bound.
    pub(super) fn reserve(&mut self) -> Result<(), ApplicationHostStartRefusal> {
        self.live.retain(|host| host.running());
        if self.live.len() + self.reserved >= MAX_LIVE_HOSTS {
            return Err(ApplicationHostStartRefusal::TooManyHosts);
        }
        self.reserved += 1;
        Ok(())
    }

    pub(super) fn release(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// Turn the reservation into the started host.
    pub(super) fn adopt(&mut self, host: Box<dyn LiveHost>) {
        self.release();
        self.live.push(host);
    }
}

impl WorkerManager {
    pub async fn handle_application_host_start(
        &self,
        worker_key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        request: ApplicationHostStart,
    ) {
        fn reply(
            sender: &mpsc::UnboundedSender<ServiceToWorker>,
            request_id: String,
            outcome: ApplicationHostStartResult,
        ) {
            if let ApplicationHostStartResult::Refused { reason } = &outcome {
                tracing::warn!(?reason, "Application host start refused");
            }
            let _ = sender.send(ServiceToWorker::ApplicationHostStarted(
                ApplicationHostStartOutcome {
                    request_id,
                    outcome,
                },
            ));
        }
        let refused = |reason| ApplicationHostStartResult::Refused { reason };

        let mut inner = self.inner.lock().await;
        let worker = match worker_key {
            Some(key) => inner.resident_workers.get_mut(key),
            None => inner.active_worker.as_mut(),
        };
        let Some(worker) = worker.filter(|worker| worker.incarnation == incarnation) else {
            return;
        };
        let sender = worker.ipc_tx.clone();

        #[cfg(not(windows))]
        {
            reply(
                &sender,
                request.request_id,
                refused(ApplicationHostStartRefusal::Unsupported),
            );
        }

        #[cfg(windows)]
        {
            let Some(key) = worker_key
                .filter(|key| key.desktop == DesktopTarget::WindowsDefault)
                .cloned()
            else {
                reply(
                    &sender,
                    request.request_id,
                    refused(ApplicationHostStartRefusal::NotResidentUser),
                );
                return;
            };
            if !ApplicationHostStart::valid_pipe_name(&request.pipe_name) {
                reply(
                    &sender,
                    request.request_id,
                    refused(ApplicationHostStartRefusal::InvalidPipe),
                );
                return;
            }
            // Identity from the daemon's own handle, taken under the lock;
            // only owned values cross into the blocking task.
            let Some(identity) = windows_host_start::worker_identity(worker) else {
                reply(
                    &sender,
                    request.request_id,
                    refused(ApplicationHostStartRefusal::SessionUnavailable),
                );
                return;
            };
            if let Err(reason) = worker.application_hosts.reserve() {
                reply(&sender, request.request_id, refused(reason));
                return;
            }
            drop(inner);
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let pipe_name = request.pipe_name.clone();
                let started = tokio::task::spawn_blocking(move || {
                    windows_host_start::start_host(&identity, &pipe_name)
                })
                .await
                .unwrap_or(Err(ApplicationHostStartRefusal::NativeFailure { code: 0 }));
                let mut inner = inner.lock().await;
                // A replaced worker gets no answer; its host exits on its own
                // once nobody connects to the pipe.
                let Some(worker) = inner
                    .resident_workers
                    .get_mut(&key)
                    .filter(|worker| worker.incarnation == incarnation)
                else {
                    return;
                };
                let outcome = match started {
                    Ok((process_id, host)) => {
                        worker.application_hosts.adopt(Box::new(host));
                        tracing::info!(process_id, "Application host started for the session user");
                        ApplicationHostStartResult::Started { process_id }
                    }
                    Err(reason) => {
                        worker.application_hosts.release();
                        refused(reason)
                    }
                };
                reply(&worker.ipc_tx, request.request_id, outcome);
            });
        }
    }
}

#[cfg(windows)]
#[path = "windows_host_start.rs"]
mod windows_host_start;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeHost(Arc<AtomicBool>);
    impl LiveHost for FakeHost {
        fn running(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn reservations_bound_live_hosts_and_exited_hosts_free_their_slot() {
        let mut slots = HostSlots::default();
        let alive = Arc::new(AtomicBool::new(true));
        // Concurrent requests reserve before any host exists.
        for _ in 0..MAX_LIVE_HOSTS {
            slots.reserve().unwrap();
        }
        assert_eq!(
            slots.reserve(),
            Err(ApplicationHostStartRefusal::TooManyHosts)
        );
        // A failed start releases its reservation.
        slots.release();
        slots.reserve().unwrap();
        for _ in 0..MAX_LIVE_HOSTS {
            slots.adopt(Box::new(FakeHost(alive.clone())));
        }
        assert_eq!(
            slots.reserve(),
            Err(ApplicationHostStartRefusal::TooManyHosts)
        );
        // Exited hosts are reaped on the next reservation.
        alive.store(false, Ordering::SeqCst);
        slots.reserve().unwrap();
    }
}
