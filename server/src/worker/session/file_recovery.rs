//! Each interactive worker maintains its OS user's private backup store.
//! The store lock serializes this with actions and other workers/processes.
//!
//! The first pass recovers interrupted operations, recounts object usage with
//! a sweep and reconciles the device quota, which clears reservations of
//! operations that are no longer live. Later passes never wait for the lock:
//! they purge expired material, sweep when due and settle changed usage.
use desk_file_recovery::LockedVault;
use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct Maintenance(tokio::task::JoinHandle<()>);
impl Drop for Maintenance {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn now_ms() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis() as u64)
}

/// Restart recovery: interrupted operations become aborted or unknown, the
/// sweep recounts namespace usage and the reconciliation replaces the quota's
/// usage and live reservations.
fn recover(locked: &mut LockedVault, quota: &super::QuotaClient, os_user: &str) -> io::Result<()> {
    let now = now_ms()?;
    locked.recover_interrupted(now)?;
    locked.sweep(now)?;
    locked.reconcile(os_user, &mut quota.clone())?;
    Ok(())
}

/// Periodic pass: purge, sweep when due and settle usage that changed.
fn maintain(locked: &mut LockedVault, quota: &super::QuotaClient) -> io::Result<()> {
    let now = now_ms()?;
    let revision = locked.revision();
    locked.cleanup(now)?;
    let swept = locked.sweep_if_due(now)?.is_some();
    if swept || locked.revision() != revision || locked.pending_cleanup_count() > 0 {
        quota.settle_vault(locked, None)?;
    }
    Ok(())
}

pub(super) fn start(data_root: PathBuf, quota: super::QuotaClient) -> Maintenance {
    Maintenance(tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous_unknown = None;
        let mut recovered = false;
        loop {
            timer.tick().await;
            let root = data_root.clone();
            let quota = quota.clone();
            let result = tokio::task::spawn_blocking(move || -> io::Result<Option<u64>> {
                let vault = desk_file_recovery::Vault::open(&root)?;
                let Some(mut locked) = vault.try_lock()? else {
                    return Ok(None);
                };
                locked.observe_system_clock()?;
                let (policy, _, _) = quota.policy()?;
                if locked.policy() != &policy {
                    locked.set_policy(policy)?;
                }
                let os_user = crate::file_recovery_service::platform_user::current()?;
                if recovered {
                    maintain(&mut locked, &quota)?;
                } else {
                    recover(&mut locked, &quota, &os_user)?;
                }
                locked.maintain_epoch_indexes(&os_user, &mut quota.clone(), 64)?;
                Ok(Some(locked.unknown_outcome_count(None)))
            })
            .await;
            match result {
                Ok(Ok(Some(unknown))) => {
                    recovered = true;
                    if unknown > 0 && previous_unknown != Some(unknown) {
                        tracing::warn!(
                            unknown_operations = unknown,
                            "File recovery retained operations with missing commit results; mutations will not be repeated"
                        );
                    }
                    previous_unknown = Some(unknown);
                }
                Ok(Ok(None)) => (),
                Ok(Err(error)) => tracing::warn!(error_kind = ?error.kind(), error = %error,
                    clock_changed = error.get_ref().is_some_and(|error| error.is::<desk_file_recovery::CleanupClockError>()),
                    "File recovery maintenance failed; pending materials will be retried"),
                Err(_) => {
                    tracing::warn!("File recovery maintenance task failed; retrying next interval")
                }
            }
        }
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::*;

    fn answer(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
        client: &super::super::QuotaClient,
    ) -> FileRecoveryQuotaCommand {
        let next = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv()).await
            });
        let Ok(Some(WorkerToService::FileRecoveryQuotaRequested(request))) = next else {
            panic!("quota request expected")
        };
        client.complete(FileRecoveryQuotaReply {
            request_id: request.request_id,
            outcome: match request.command {
                FileRecoveryQuotaCommand::Settle { .. } => {
                    FileRecoveryQuotaOutcome::Settled { applied: true }
                }
                _ => FileRecoveryQuotaOutcome::Epoch {
                    execution_epoch: 0,
                    cleanup_pending: false,
                },
            },
        });
        request.command
    }

    #[test]
    fn restart_recovery_reconciles_without_live_operations_after_a_recount_sweep() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = super::super::QuotaClient::new(sender);
        let os_user = crate::file_recovery_service::platform_user::current().unwrap();
        let scope = desk_file_recovery::Scope {
            authority: "a".repeat(64),
            device: "d".into(),
            os_user: os_user.clone(),
            owner: "1".into(),
        };
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let now = now_ms().unwrap();
        // A crash left this operation in flight with a reservation.
        let interrupted = crate::file_recovery_service::test_support::prepare(
            &mut vault.lock().unwrap(),
            &scope,
            "c",
            "op",
            b"before",
            now,
        );
        let worker_client = client.clone();
        let path = root.path().to_owned();
        let worker = std::thread::spawn(move || {
            let vault = desk_file_recovery::Vault::open(&path).unwrap();
            let mut locked = vault.lock().unwrap();
            recover(&mut locked, &worker_client, &os_user).unwrap();
            locked.usage_report()
        });
        let FileRecoveryQuotaCommand::Reconcile {
            usage,
            live_operations,
            ..
        } = answer(&mut receiver, &client)
        else {
            panic!("reconciliation expected")
        };
        let settled = worker.join().unwrap();
        assert!(live_operations.is_empty());
        assert_eq!(usage.revision, settled.revision);
        assert_eq!(usage.namespaces, settled.namespaces);
        let locked = vault.lock().unwrap();
        assert_eq!(
            locked.record(&scope, &interrupted.id).unwrap().change,
            desk_file_recovery::ChangeState::Aborted
        );
    }

    #[test]
    fn periodic_maintenance_settles_only_changed_usage() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = super::super::QuotaClient::new(sender);
        let scope = desk_file_recovery::Scope {
            authority: "a".repeat(64),
            device: "d".into(),
            os_user: crate::file_recovery_service::platform_user::current().unwrap(),
            owner: "1".into(),
        };
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        // A completed backup whose retention already ended.
        let created = now_ms().unwrap() - 8 * 86_400_000;
        {
            let mut locked = vault.lock().unwrap();
            let record = crate::file_recovery_service::test_support::prepare(
                &mut locked,
                &scope,
                "c",
                "op",
                b"before",
                created,
            );
            locked
                .transition(&scope, &record.id, desk_file_recovery::ChangeState::Aborted)
                .unwrap();
        }
        // The pass purges the expired record and sweeps: usage changed.
        let worker_client = client.clone();
        let path = root.path().to_owned();
        let worker = std::thread::spawn(move || {
            let vault = desk_file_recovery::Vault::open(&path).unwrap();
            maintain(&mut vault.lock().unwrap(), &worker_client).unwrap();
        });
        let FileRecoveryQuotaCommand::Settle { identity, usage } = answer(&mut receiver, &client)
        else {
            panic!("settlement expected")
        };
        worker.join().unwrap();
        assert!(identity.is_none());
        // Nothing changed since: no quota request.
        maintain(&mut vault.lock().unwrap(), &client).unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(usage.namespaces.values().all(|bytes| *bytes < 4096));
    }
}
