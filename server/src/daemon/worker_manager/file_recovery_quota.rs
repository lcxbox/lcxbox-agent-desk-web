//! The shared device ledger is opened only under the daemon's data root.
use super::*;
use desk_file_recovery::quota::{DeviceQuota, QuotaKey};
use desk_ipc_protocol::message::*;

impl WorkerManager {
    pub async fn handle_file_recovery_quota(
        &self,
        worker_key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        request: FileRecoveryQuotaRequest,
    ) {
        let (reply_tx, os_user) = {
            let inner = self.inner.lock().await;
            let worker = match worker_key {
                Some(key) => inner.resident_workers.get(key),
                None if !self.uses_session_targeting() => inner.active_worker.as_ref(),
                _ => None,
            };
            let Some(worker) = worker.filter(|worker| worker.incarnation == incarnation) else {
                return;
            };
            #[cfg(windows)]
            let os_user = windows_identity::worker_user(worker);
            #[cfg(not(windows))]
            let os_user = match worker_key {
                None => {
                    #[cfg(unix)]
                    {
                        Some(unsafe { libc::geteuid() }.to_string())
                    }
                    #[cfg(not(unix))]
                    {
                        None::<String>
                    }
                }
                Some(key) => {
                    #[cfg(target_os = "linux")]
                    {
                        self.session_shell_registration(&key.session)
                            .map(|registration| registration.process_identity.uid.to_string())
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = key;
                        None::<String>
                    }
                }
            };
            (worker.ipc_tx.clone(), os_user)
        };
        let Some(os_user) = os_user.filter(|user| user == &request.os_user) else {
            let _ = reply_tx.send(ServiceToWorker::FileRecoveryQuotaReplied(
                FileRecoveryQuotaReply {
                    request_id: request.request_id,
                    outcome: FileRecoveryQuotaOutcome::IdentityChanged,
                },
            ));
            return;
        };
        let root = self.settings.read().await.paths().data_root().to_owned();
        tokio::spawn(async move {
            let request_id = request.request_id.clone();
            let outcome =
                tokio::task::spawn_blocking(move || apply(&root, &os_user, request.command))
                    .await
                    .unwrap_or(FileRecoveryQuotaOutcome::StorageUnavailable);
            // Send only to the original worker handle, never its replacement.
            let _ = reply_tx.send(ServiceToWorker::FileRecoveryQuotaReplied(
                FileRecoveryQuotaReply {
                    request_id,
                    outcome,
                },
            ));
        });
    }
}
#[cfg(windows)]
#[path = "windows_quota_identity.rs"]
mod windows_identity;

#[cfg(windows)]
pub(super) fn windows_worker_user(worker: &WorkerHandle) -> Option<String> {
    windows_identity::worker_user(worker)
}
/// Usage reports cross the IPC boundary between this daemon and its own
/// workers; the quota validates their namespaces against the OS user.
fn usage_report(usage: FileRecoveryUsage) -> desk_file_recovery::quota::UsageReport {
    desk_file_recovery::quota::UsageReport {
        vault_instance_id: usage.vault_instance_id,
        revision: usage.revision,
        ledger_bytes: usage.ledger_bytes,
        namespaces: usage.namespaces,
    }
}
fn apply(
    root: &std::path::Path,
    os_user: &str,
    command: FileRecoveryQuotaCommand,
) -> FileRecoveryQuotaOutcome {
    use desk_file_recovery::quota::{FinishEpochRequest, ReconcileRequest, VaultRevision};
    use std::io;
    let epoch = |status: desk_file_recovery::quota::EpochStatus| FileRecoveryQuotaOutcome::Epoch {
        execution_epoch: status.epoch,
        cleanup_pending: status.cleanup_pending,
    };
    let result = (|| -> io::Result<_> {
        let mut quota = DeviceQuota::open(root)?.lock()?;
        let key = |identity: FileRecoveryQuotaIdentity| {
            QuotaKey::new(
                &desk_file_recovery::Scope {
                    authority: identity.authority,
                    device: identity.device,
                    owner: identity.owner,
                    os_user: os_user.into(),
                },
                &identity.conversation,
                &identity.operation,
                identity.execution_epoch,
            )
        };
        match command {
            FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch } => {
                return Ok(epoch(quota.begin_epoch_cleanup(os_user, expected_epoch)?));
            }
            FileRecoveryQuotaCommand::FinishEpochCleanup {
                epoch: next,
                vault_instance_id,
                revision,
            } => {
                return Ok(epoch(quota.finish_epoch_cleanup(
                    os_user,
                    &FinishEpochRequest {
                        epoch: next,
                        vault: VaultRevision {
                            vault_instance_id,
                            revision,
                        },
                    },
                )?));
            }
            FileRecoveryQuotaCommand::Reconcile {
                execution_epoch,
                usage,
                live_operations,
            } => {
                return Ok(epoch(quota.reconcile(
                    os_user,
                    &ReconcileRequest {
                        execution_epoch,
                        usage: usage_report(usage),
                        live_operations,
                    },
                )?));
            }
            FileRecoveryQuotaCommand::Settle { identity, usage } => {
                let clear = identity.map(key).transpose()?;
                let applied = quota.settle(os_user, clear.as_ref(), &usage_report(usage))?;
                return Ok(FileRecoveryQuotaOutcome::Settled { applied });
            }
            FileRecoveryQuotaCommand::Read => (),
            FileRecoveryQuotaCommand::SetPolicy {
                retention_days,
                max_bytes,
            } => quota.set_policy(desk_file_recovery::Policy {
                retention_days,
                max_bytes,
            })?,
            FileRecoveryQuotaCommand::Reserve { identity, bytes } => {
                quota.reserve(&key(identity)?, bytes)?
            }
        }
        Ok(FileRecoveryQuotaOutcome::Applied {
            retention_days: quota.policy().retention_days,
            max_bytes: quota.max_bytes(),
            used_bytes: quota.used_bytes(),
            reserved_bytes: quota.reserved_bytes(),
        })
    })();
    result.unwrap_or_else(|error| match error.kind() {
        io::ErrorKind::PermissionDenied => FileRecoveryQuotaOutcome::IdentityChanged,
        io::ErrorKind::StorageFull => FileRecoveryQuotaOutcome::CapacityExceeded,
        io::ErrorKind::InvalidInput | io::ErrorKind::AlreadyExists => {
            // The outcome sent to the worker carries no text, so keep the reason here.
            tracing::warn!(%error, "Device file recovery quota request rejected");
            FileRecoveryQuotaOutcome::InvalidRequest
        }
        _ => {
            tracing::warn!(error_kind = ?error.kind(), "Device file recovery quota operation failed");
            FileRecoveryQuotaOutcome::StorageUnavailable
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(operation: &str) -> FileRecoveryQuotaIdentity {
        FileRecoveryQuotaIdentity {
            execution_epoch: 0,
            authority: "authority".into(),
            device: "device".into(),
            owner: "owner".into(),
            conversation: "conversation".into(),
            operation: operation.into(),
        }
    }

    fn usage(revision: u64, bytes: u64) -> FileRecoveryUsage {
        let namespace = desk_file_recovery::Scope {
            authority: "authority".into(),
            device: "device".into(),
            os_user: "501".into(),
            owner: "owner".into(),
        }
        .namespace();
        FileRecoveryUsage {
            vault_instance_id: "a".repeat(64),
            revision,
            ledger_bytes: 100,
            namespaces: std::collections::BTreeMap::from([(namespace, bytes)]),
        }
    }

    #[test]
    fn reservations_settle_to_newer_absolute_usage_and_epochs_finish_after_reconciliation() {
        let root = tempfile::tempdir().unwrap();
        let apply = |command| apply(root.path(), "501", command);
        let applied = |outcome| match outcome {
            FileRecoveryQuotaOutcome::Applied {
                used_bytes,
                reserved_bytes,
                ..
            } => (used_bytes, reserved_bytes),
            other => panic!("applied expected, got {other:?}"),
        };
        assert_eq!(
            applied(apply(FileRecoveryQuotaCommand::Reserve {
                identity: identity("op"),
                bytes: 8192,
            })),
            (8192, 8192)
        );
        // Reservations are idempotent and only grow.
        assert_eq!(
            applied(apply(FileRecoveryQuotaCommand::Reserve {
                identity: identity("op"),
                bytes: 4096,
            })),
            (8192, 8192)
        );
        assert!(matches!(
            apply(FileRecoveryQuotaCommand::Settle {
                identity: Some(identity("op")),
                usage: usage(5, 4096),
            }),
            FileRecoveryQuotaOutcome::Settled { applied: true }
        ));
        assert_eq!(applied(apply(FileRecoveryQuotaCommand::Read)), (4196, 0));
        // An older report clears nothing more and does not replace usage.
        assert!(matches!(
            apply(FileRecoveryQuotaCommand::Settle {
                identity: None,
                usage: usage(4, 1),
            }),
            FileRecoveryQuotaOutcome::Settled { applied: false }
        ));
        assert!(matches!(
            apply(FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch: 0 }),
            FileRecoveryQuotaOutcome::Epoch {
                execution_epoch: 1,
                cleanup_pending: true
            }
        ));
        let finish = || FileRecoveryQuotaCommand::FinishEpochCleanup {
            epoch: 1,
            vault_instance_id: "a".repeat(64),
            revision: 6,
        };
        assert!(matches!(
            apply(finish()),
            FileRecoveryQuotaOutcome::StorageUnavailable
        ));
        assert!(matches!(
            apply(FileRecoveryQuotaCommand::Reconcile {
                execution_epoch: 1,
                usage: usage(6, 2048),
                live_operations: vec![],
            }),
            FileRecoveryQuotaOutcome::Epoch {
                execution_epoch: 1,
                ..
            }
        ));
        assert!(matches!(
            apply(finish()),
            FileRecoveryQuotaOutcome::Epoch {
                execution_epoch: 1,
                cleanup_pending: false
            }
        ));
        assert_eq!(applied(apply(FileRecoveryQuotaCommand::Read)), (2148, 0));
        // Old-epoch reservations are refused.
        assert!(matches!(
            apply(FileRecoveryQuotaCommand::Reserve {
                identity: identity("late"),
                bytes: 1,
            }),
            FileRecoveryQuotaOutcome::IdentityChanged
        ));
    }
}
