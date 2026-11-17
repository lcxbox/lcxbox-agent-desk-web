//! Blocking quota RPC for filesystem tasks. The worker event loop delivers replies.
//! Callers send every request while holding their vault lock and wait for the
//! reply before releasing it, so reconciliations never interleave with another
//! worker's reservation of the same vault.
use desk_file_recovery::{
    LockedVault, Record,
    quota::{EpochStatus, FinishEpochRequest, ReconcileRequest, UsageReport},
};
use desk_ipc_protocol::message::*;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

type Pending = Arc<Mutex<HashMap<String, mpsc::SyncSender<FileRecoveryQuotaOutcome>>>>;
#[derive(Clone)]
pub(crate) struct QuotaClient {
    sender: tokio::sync::mpsc::UnboundedSender<WorkerToService>,
    pending: Pending,
    os_user: Option<String>,
}
impl QuotaClient {
    pub(crate) fn new(sender: tokio::sync::mpsc::UnboundedSender<WorkerToService>) -> Self {
        let os_user = crate::file_recovery_service::platform_user::current().ok();
        Self {
            sender,
            pending: Arc::new(Mutex::new(HashMap::new())),
            os_user,
        }
    }
    /// Run only on a blocking filesystem task, never the worker event loop.
    pub(crate) fn request(
        &self,
        command: FileRecoveryQuotaCommand,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        self.request_with_timeout(command, Duration::from_secs(10))
    }
    fn request_with_timeout(
        &self,
        command: FileRecoveryQuotaCommand,
        timeout: Duration,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        Self::policy_response(self.raw_request_with_timeout(command, timeout)?)
    }
    fn raw_request_with_timeout(
        &self,
        command: FileRecoveryQuotaCommand,
        timeout: Duration,
    ) -> io::Result<FileRecoveryQuotaOutcome> {
        let os_user = self.os_user.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "device quota unavailable on this platform",
            )
        })?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| io::Error::other("quota requests unavailable"))?;
            if pending.len() >= 32 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "device quota request capacity exhausted",
                ));
            }
            pending.insert(request_id.clone(), tx);
        }
        let sent = self
            .sender
            .send(WorkerToService::FileRecoveryQuotaRequested(
                FileRecoveryQuotaRequest {
                    request_id: request_id.clone(),
                    os_user,
                    command,
                },
            ));
        let result = if sent.is_ok() {
            rx.recv_timeout(timeout).map_err(|error| {
                io::Error::new(
                    match error {
                        mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                        mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
                    },
                    "device quota response unavailable; file was not changed",
                )
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device quota transport unavailable; file was not changed",
            ))
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request_id);
        }
        result
    }
    fn policy_response(
        outcome: FileRecoveryQuotaOutcome,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        match outcome {
            FileRecoveryQuotaOutcome::Epoch { .. } | FileRecoveryQuotaOutcome::Settled { .. } => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected quota reply",
                ))
            }
            FileRecoveryQuotaOutcome::Applied {
                retention_days,
                max_bytes,
                used_bytes,
                reserved_bytes,
            } => {
                let policy = desk_file_recovery::Policy {
                    retention_days,
                    max_bytes,
                };
                policy.validate()?;
                Ok((policy, used_bytes, reserved_bytes))
            }
            FileRecoveryQuotaOutcome::CapacityExceeded => Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device file backup capacity exhausted; file was not changed",
            )),
            FileRecoveryQuotaOutcome::IdentityChanged => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution user changed; file was not changed",
            )),
            FileRecoveryQuotaOutcome::InvalidRequest => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "device quota request rejected; file was not changed",
            )),
            FileRecoveryQuotaOutcome::StorageUnavailable => Err(io::Error::other(
                "device quota storage unavailable; file was not changed",
            )),
        }
    }
    pub(crate) fn policy(&self) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        self.request(FileRecoveryQuotaCommand::Read)
    }
    pub(crate) fn set_policy(
        &self,
        policy: desk_file_recovery::Policy,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        policy.validate()?;
        self.request(FileRecoveryQuotaCommand::SetPolicy {
            retention_days: policy.retention_days,
            max_bytes: policy.max_bytes,
        })
    }
    /// Reserve the cumulative upper bound of one backup operation.
    pub(crate) fn reserve(&self, record: &Record, bytes: u64) -> io::Result<()> {
        self.request(FileRecoveryQuotaCommand::Reserve {
            identity: record_identity(record),
            bytes,
        })
        .map(|_| ())
    }
    /// Two-part settlement; returns whether the absolute usage was applied.
    pub(crate) fn settle(&self, clear: Option<&Record>, usage: &UsageReport) -> io::Result<bool> {
        self.settle_identity(clear.map(record_identity), usage)
    }
    fn settle_identity(
        &self,
        identity: Option<FileRecoveryQuotaIdentity>,
        usage: &UsageReport,
    ) -> io::Result<bool> {
        match self.raw_request_with_timeout(
            FileRecoveryQuotaCommand::Settle {
                identity,
                usage: usage_message(usage),
            },
            Duration::from_secs(10),
        )? {
            FileRecoveryQuotaOutcome::Settled { applied } => Ok(applied),
            other => {
                Self::policy_response(other)?;
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected quota settlement reply",
                ))
            }
        }
    }
    /// Settle the vault's current absolute usage, clearing `clear`'s
    /// reservation, and acknowledge the settlement when it was applied.
    pub(crate) fn settle_vault(
        &self,
        vault: &mut LockedVault,
        clear: Option<&Record>,
    ) -> io::Result<()> {
        self.settle_vault_clearing(vault, clear.map(record_identity))
    }
    /// As [`Self::settle_vault`], clearing an operation named by its identity;
    /// used when the operation's record is no longer at hand.
    pub(crate) fn settle_vault_clearing(
        &self,
        vault: &mut LockedVault,
        clear: Option<FileRecoveryQuotaIdentity>,
    ) -> io::Result<()> {
        let usage = vault.usage_report();
        if self.settle_identity(clear, &usage)? {
            vault.acknowledge_settlement(&usage)?;
        }
        Ok(())
    }
    pub(crate) fn complete(&self, reply: FileRecoveryQuotaReply) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(sender) = pending.remove(&reply.request_id)
        {
            let _ = sender.send(reply.outcome);
        }
    }
}

fn record_identity(record: &Record) -> FileRecoveryQuotaIdentity {
    FileRecoveryQuotaIdentity {
        execution_epoch: record.storage_epoch,
        authority: record.scope.authority.clone(),
        device: record.scope.device.clone(),
        owner: record.scope.owner.clone(),
        conversation: record.conversation.clone(),
        operation: record.operation.clone(),
    }
}
fn usage_message(usage: &UsageReport) -> FileRecoveryUsage {
    FileRecoveryUsage {
        vault_instance_id: usage.vault_instance_id.clone(),
        revision: usage.revision,
        ledger_bytes: usage.ledger_bytes,
        namespaces: usage.namespaces.clone(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn settlement_clears_the_reservation_and_acknowledges_only_applied_usage() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        let scope = desk_file_recovery::Scope {
            authority: "a".repeat(64),
            device: "d".into(),
            os_user: client.os_user.clone().unwrap(),
            owner: "1".into(),
        };
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let record = crate::file_recovery_service::test_support::backup(
            &mut vault.lock().unwrap(),
            &scope,
            "c",
            "op",
            b"before",
        );
        vault
            .lock()
            .unwrap()
            .delete_conversation(&scope, "c", record.created_at_ms + 1)
            .unwrap();
        for applied in [false, true] {
            let client_worker = client.clone();
            let path = root.path().to_owned();
            let cleared = record.clone();
            let worker = std::thread::spawn(move || {
                let vault = desk_file_recovery::Vault::open(&path).unwrap();
                let mut locked = vault.lock().unwrap();
                let expected = locked.usage_report();
                client_worker
                    .settle_vault(&mut locked, Some(&cleared))
                    .unwrap();
                expected
            });
            let WorkerToService::FileRecoveryQuotaRequested(request) =
                receiver.blocking_recv().unwrap()
            else {
                panic!("quota expected")
            };
            let FileRecoveryQuotaCommand::Settle { identity, usage } = request.command else {
                panic!("settlement expected")
            };
            let identity = identity.unwrap();
            assert_eq!(identity.operation, record.operation);
            assert_eq!(identity.conversation, "c");
            client.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: FileRecoveryQuotaOutcome::Settled { applied },
            });
            let expected = worker.join().unwrap();
            assert_eq!(usage, usage_message(&expected));
            assert_eq!(
                vault.lock().unwrap().cleanup_complete(&scope, "c"),
                applied,
                "a rejected settlement must keep the deletion pending"
            );
        }
        assert!(receiver.try_recv().is_err());
    }
    #[test]
    fn matching_reply_completes_and_timed_out_requests_discard_late_replies() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        let request_client = client.clone();
        let worker =
            std::thread::spawn(move || request_client.request(FileRecoveryQuotaCommand::Read));
        let WorkerToService::FileRecoveryQuotaRequested(request) =
            receiver.blocking_recv().unwrap()
        else {
            panic!("quota expected")
        };
        client.complete(FileRecoveryQuotaReply {
            request_id: "different".into(),
            outcome: FileRecoveryQuotaOutcome::IdentityChanged,
        });
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        client.complete(FileRecoveryQuotaReply {
            request_id: request.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 7,
                max_bytes: 1048576,
                used_bytes: 3,
                reserved_bytes: 0,
            },
        });
        assert_eq!(
            worker.join().unwrap().unwrap(),
            (
                desk_file_recovery::Policy {
                    retention_days: 7,
                    max_bytes: 1048576
                },
                3,
                0
            )
        );
        assert_eq!(
            client
                .request_with_timeout(FileRecoveryQuotaCommand::Read, Duration::from_millis(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let WorkerToService::FileRecoveryQuotaRequested(late) = receiver.blocking_recv().unwrap()
        else {
            panic!("quota expected")
        };
        client.complete(FileRecoveryQuotaReply {
            request_id: late.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 7,
                max_bytes: 1048576,
                used_bytes: 4,
                reserved_bytes: 0,
            },
        });
        assert!(client.pending.lock().unwrap().is_empty());
    }
    #[test]
    fn epoch_coordinator_preserves_user_epoch_and_decodes_acknowledgments() {
        use desk_file_recovery::EpochCoordinator;
        use desk_file_recovery::quota::VaultRevision;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        let mut worker_client = client.clone();
        assert_eq!(
            worker_client.begin("wrong-user", 7).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(receiver.try_recv().is_err());
        let user = client.os_user.clone().unwrap();
        let expected_user = user.clone();
        let usage = UsageReport {
            vault_instance_id: "v".repeat(64),
            revision: 12,
            ledger_bytes: 100,
            namespaces: Default::default(),
        };
        let reconcile = ReconcileRequest {
            execution_epoch: 8,
            usage: usage.clone(),
            live_operations: vec!["b".repeat(64)],
        };
        let worker = std::thread::spawn(move || {
            let advanced = worker_client.begin(&user, 7).unwrap();
            assert_eq!(advanced.epoch, 8);
            assert!(advanced.cleanup_pending);
            assert_eq!(worker_client.reconcile(&user, &reconcile).unwrap().epoch, 8);
            let finished = worker_client
                .finish(
                    &user,
                    &FinishEpochRequest {
                        epoch: 8,
                        vault: VaultRevision {
                            vault_instance_id: "v".repeat(64),
                            revision: 12,
                        },
                    },
                )
                .unwrap();
            assert_eq!(finished.epoch, 8);
            assert!(!finished.cleanup_pending);
        });
        for stage in 0..3 {
            let WorkerToService::FileRecoveryQuotaRequested(request) =
                receiver.blocking_recv().unwrap()
            else {
                panic!("quota request expected");
            };
            assert_eq!(request.os_user, expected_user);
            match (stage, request.command) {
                (0, FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch }) => {
                    assert_eq!(expected_epoch, 7)
                }
                (
                    1,
                    FileRecoveryQuotaCommand::Reconcile {
                        execution_epoch,
                        usage: sent,
                        live_operations,
                    },
                ) => {
                    assert_eq!(execution_epoch, 8);
                    assert_eq!(sent, usage_message(&usage));
                    assert_eq!(live_operations, vec!["b".repeat(64)]);
                }
                (
                    2,
                    FileRecoveryQuotaCommand::FinishEpochCleanup {
                        epoch,
                        vault_instance_id,
                        revision,
                    },
                ) => {
                    assert_eq!((epoch, revision), (8, 12));
                    assert_eq!(vault_instance_id, "v".repeat(64));
                }
                _ => panic!("incorrect cleanup sequence"),
            }
            client.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: FileRecoveryQuotaOutcome::Epoch {
                    execution_epoch: 8,
                    cleanup_pending: stage < 2,
                },
            });
        }
        worker.join().unwrap();
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn disconnected_transport_does_not_leave_pending_requests() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let client = QuotaClient::new(sender);
        assert_eq!(
            client
                .request(FileRecoveryQuotaCommand::Read)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(client.pending.lock().unwrap().is_empty());
    }
}

impl QuotaClient {
    fn epoch_request(
        &self,
        os_user: &str,
        command: FileRecoveryQuotaCommand,
    ) -> io::Result<EpochStatus> {
        if self.os_user.as_deref() != Some(os_user) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution user changed",
            ));
        }
        match self.raw_request_with_timeout(command, Duration::from_secs(10))? {
            FileRecoveryQuotaOutcome::Epoch {
                execution_epoch,
                cleanup_pending,
            } => Ok(EpochStatus {
                epoch: execution_epoch,
                cleanup_pending,
            }),
            other => {
                Self::policy_response(other)?;
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected quota epoch reply",
                ))
            }
        }
    }
}
impl desk_file_recovery::EpochCoordinator for QuotaClient {
    fn begin(&mut self, os_user: &str, expected_epoch: u64) -> io::Result<EpochStatus> {
        self.epoch_request(
            os_user,
            FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch },
        )
    }
    fn reconcile(&mut self, os_user: &str, request: &ReconcileRequest) -> io::Result<EpochStatus> {
        self.epoch_request(
            os_user,
            FileRecoveryQuotaCommand::Reconcile {
                execution_epoch: request.execution_epoch,
                usage: usage_message(&request.usage),
                live_operations: request.live_operations.clone(),
            },
        )
    }
    fn finish(&mut self, os_user: &str, request: &FinishEpochRequest) -> io::Result<EpochStatus> {
        self.epoch_request(
            os_user,
            FileRecoveryQuotaCommand::FinishEpochCleanup {
                epoch: request.epoch,
                vault_instance_id: request.vault.vault_instance_id.clone(),
                revision: request.vault.revision,
            },
        )
    }
}
