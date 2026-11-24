//! Worker-side backup management. Remote operations always retain their frozen Scope.
use desk_agent_protocol::file_recovery::*;
use desk_file_recovery::{ChangeState, LockedVault, MaterialState, Record, Scope};
use std::{io, path::Path};
#[path = "file_recovery_local.rs"]
pub(crate) mod local;
#[path = "file_recovery_local_quota.rs"]
mod local_quota;
#[path = "file_recovery_user.rs"]
pub(crate) mod platform_user;

static SERVICE_LEDGER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process as the system ServiceDaemon. Called once at its start,
/// before anything opens the device quota.
pub(crate) fn use_service_quota_ledger() {
    SERVICE_LEDGER.store(true, std::sync::atomic::Ordering::Release);
}

/// The device quota ledger of this process's mode. The ServiceDaemon and a
/// daemon running as the user keep separate ledgers, so switching modes never
/// needs one identity to open a private directory the other one owns.
pub(crate) fn open_device_quota(
    data_root: &Path,
) -> io::Result<desk_file_recovery::quota::DeviceQuota> {
    open_device_quota_for(
        data_root,
        SERVICE_LEDGER.load(std::sync::atomic::Ordering::Acquire),
    )
}

fn open_device_quota_for(
    data_root: &Path,
    service: bool,
) -> io::Result<desk_file_recovery::quota::DeviceQuota> {
    use desk_file_recovery::quota::{DeviceQuota, SERVICE_LEDGER, USER_LEDGER};
    DeviceQuota::open_named(
        data_root,
        if service { SERVICE_LEDGER } else { USER_LEDGER },
    )
}

pub(crate) fn project_record(vault: &LockedVault, r: Record, now: u64) -> FileRecoveryRecordDto {
    FileRecoveryRecordDto {
        cleanup_reason: if r.transaction.is_some() && r.cleanup_error.is_some() {
            Some("temporary_file_pending".into())
        } else if r.cleanup_error.is_some() {
            Some("storage_unavailable".into())
        } else {
            None
        },
        cleanup_pending: r.cleanup_error.is_some()
            || r.transaction.is_some()
            || vault.is_deleted(&r.scope, &r.conversation),
        export_available: r.material == MaterialState::Saved
            && (r.expires_at_ms > now || r.change == ChangeState::OutcomeUnknown)
            && !r.discard_requested
            && !vault.is_deleted(&r.scope, &r.conversation),
        file_name: r.file_name().unwrap_or_default().to_owned(),
        entry_count: u32::try_from(r.entry_count()).unwrap_or(u32::MAX),
        recovery_id: r.id,
        conversation_id: r.conversation,
        created_at_unix_ms: r.created_at_ms,
        expires_at_unix_ms: r.expires_at_ms,
        size_bytes: r.published_bytes,
        change_state: match r.change {
            ChangeState::Preparing => "preparing",
            ChangeState::BackupReady => "backup_ready",
            ChangeState::CommitIntent | ChangeState::OutcomeUnknown => "outcome_unknown",
            ChangeState::SnapshotPending => "snapshot_pending",
            ChangeState::Succeeded | ChangeState::Completed => "succeeded",
            ChangeState::Aborted => "aborted",
        }
        .into(),
        material_state: match r.material {
            MaterialState::Saved => "saved",
            MaterialState::Purged => "purged",
        }
        .into(),
    }
}
/// One package cache per worker process; it bounds concurrent export
/// streams.
fn export_cache() -> &'static std::sync::Mutex<desk_file_recovery::ExportCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<desk_file_recovery::ExportCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}
pub(crate) fn export_chunk(
    vault: &LockedVault,
    produce: impl FnOnce(
        &LockedVault,
        &mut desk_file_recovery::ExportCache,
    ) -> io::Result<desk_file_recovery::ExportChunk>,
) -> io::Result<desk_file_recovery::ExportChunk> {
    let mut cache = export_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    produce(vault, &mut cache)
}
fn policy(vault: &LockedVault) -> FileRecoveryPolicyDto {
    FileRecoveryPolicyDto {
        retention_days: vault.policy().retention_days,
        max_bytes: vault.policy().max_bytes,
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(crate) fn execute(
    root: Option<&Path>,
    payload: &desk_ipc_protocol::message::FileRecoveryRequestPayload,
    quota: Option<&crate::worker::session::QuotaClient>,
) -> FileRecoveryReply {
    let os_user = match platform_user::current() {
        Ok(user) => user,
        Err(_) => {
            return FileRecoveryReply {
                authority: payload.authority.clone(),
                os_user: String::new(),
                outcome: FileRecoveryOutcome::Unavailable {
                    reason: FileRecoveryFailure::IdentityChanged,
                },
            };
        }
    };
    let result = (|| {
        payload
            .request
            .command
            .validate()
            .map_err(|_| FileRecoveryFailure::InvalidRequest)?;
        if payload
            .request
            .expected_authority
            .as_deref()
            .is_some_and(|id| id != payload.authority)
        {
            return Err(FileRecoveryFailure::IdentityChanged);
        }
        if payload
            .request
            .expected_os_user
            .as_deref()
            .is_some_and(|id| id != os_user)
        {
            return Err(FileRecoveryFailure::IdentityChanged);
        }
        let quota = quota.ok_or(FileRecoveryFailure::StorageUnavailable)?;
        if let FileRecoveryCommand::SetPolicy {
            retention_days,
            max_bytes,
        } = &payload.request.command
        {
            let (policy, _, _) = quota
                .set_policy(desk_file_recovery::Policy {
                    retention_days: *retention_days,
                    max_bytes: *max_bytes,
                })
                .map_err(storage_failure)?;
            return Ok(FileRecoveryOutcome::Policy {
                policy: FileRecoveryPolicyDto {
                    retention_days: policy.retention_days,
                    max_bytes: policy.max_bytes,
                },
            });
        }
        let root = root.ok_or(FileRecoveryFailure::StorageUnavailable)?;
        let vault = desk_file_recovery::Vault::open(root).map_err(storage_failure)?;
        let mut locked = vault
            .try_lock()
            .map_err(storage_failure)?
            .ok_or(FileRecoveryFailure::Busy)?;
        locked.observe_system_clock().map_err(storage_failure)?;
        let scope = Scope {
            authority: payload.authority.clone(),
            device: payload.device_id.clone(),
            os_user: os_user.clone(),
            owner: payload.actor_id.clone(),
        };
        locked
            .maintain_epoch_indexes(
                &os_user,
                &mut quota.clone(),
                if matches!(payload.request.command, FileRecoveryCommand::Query { .. }) {
                    64
                } else {
                    usize::MAX
                },
            )
            .map_err(storage_failure)?;
        let usage = if matches!(
            payload.request.command,
            FileRecoveryCommand::DeleteConversation { .. }
        ) {
            None
        } else {
            let (policy, used, reserved) = quota.policy().map_err(storage_failure)?;
            if locked.policy() != &policy {
                locked.set_policy(policy).map_err(storage_failure)?;
            }
            Some((used, reserved))
        };
        let now = jiff::Timestamp::now().as_millisecond().max(1) as u64;
        let mut outcome = execute_locked(&mut locked, &scope, &payload.request.command, now)?;
        match &payload.request.command {
            FileRecoveryCommand::RetryCleanup => {
                locked
                    .reconcile(&os_user, &mut quota.clone())
                    .map_err(storage_failure)?;
                locked
                    .maintain_epoch_indexes(&os_user, &mut quota.clone(), 1)
                    .map_err(storage_failure)?;
            }
            FileRecoveryCommand::DeleteConversation { .. }
            | FileRecoveryCommand::Discard { .. } => {
                if let Err(error) = quota.settle_vault(&mut locked, None) {
                    tracing::warn!(error_kind = ?error.kind(), "File recovery quota settlement unconfirmed; deletion stays pending");
                }
            }
            _ => (),
        }
        match &mut outcome {
            FileRecoveryOutcome::Page { page } => {
                if let Some((used, reserved)) = usage {
                    page.used_bytes = used;
                    page.reserved_bytes = reserved;
                }
            }
            FileRecoveryOutcome::Deleted { complete } => {
                if let FileRecoveryCommand::DeleteConversation { conversation_id } =
                    &payload.request.command
                {
                    *complete = locked.cleanup_complete(&scope, conversation_id);
                }
            }
            FileRecoveryOutcome::Cleanup { report } => {
                report.pending_files = locked.pending_cleanup_count()
            }
            _ => (),
        }
        Ok(outcome)
    })();
    FileRecoveryReply {
        authority: payload.authority.clone(),
        os_user,
        outcome: result.unwrap_or_else(|reason| FileRecoveryOutcome::Unavailable { reason }),
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn execute(
    _: Option<&Path>,
    payload: &desk_ipc_protocol::message::FileRecoveryRequestPayload,
    _: Option<&crate::worker::session::QuotaClient>,
) -> FileRecoveryReply {
    FileRecoveryReply {
        authority: payload.authority.clone(),
        os_user: String::new(),
        outcome: FileRecoveryOutcome::Unavailable {
            reason: FileRecoveryFailure::Unsupported,
        },
    }
}
pub(crate) fn storage_failure(error: io::Error) -> FileRecoveryFailure {
    let reason = if let Some(reason) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<desk_file_recovery::ExportError>())
    {
        match reason {
            desk_file_recovery::ExportError::Unavailable => {
                FileRecoveryFailure::MaterialUnavailable
            }
            desk_file_recovery::ExportError::Expired => FileRecoveryFailure::MaterialExpired,
            desk_file_recovery::ExportError::Cleaning => FileRecoveryFailure::MaterialCleaning,
            desk_file_recovery::ExportError::Cleaned => FileRecoveryFailure::MaterialCleaned,
            desk_file_recovery::ExportError::Preparing => FileRecoveryFailure::Busy,
        }
    } else if error
        .get_ref()
        .is_some_and(|error| error.is::<desk_file_recovery::CleanupClockError>())
    {
        FileRecoveryFailure::ClockChanged
    } else if error.kind() == io::ErrorKind::WouldBlock {
        FileRecoveryFailure::Busy
    } else {
        match error.kind() {
            io::ErrorKind::InvalidInput => FileRecoveryFailure::InvalidRequest,
            io::ErrorKind::NotFound => FileRecoveryFailure::MaterialUnavailable,
            io::ErrorKind::Unsupported => FileRecoveryFailure::Unsupported,
            _ => FileRecoveryFailure::StorageUnavailable,
        }
    };
    tracing::warn!(error_kind = ?error.kind(), ?reason, "File recovery management storage operation failed");
    reason
}
/// Vault-only part of a management command. Quota settlement and
/// reconciliation follow in `execute` under the same lock.
fn execute_locked(
    vault: &mut LockedVault,
    scope: &Scope,
    command: &FileRecoveryCommand,
    now: u64,
) -> Result<FileRecoveryOutcome, FileRecoveryFailure> {
    match command {
        FileRecoveryCommand::ConfirmClock {
            displayed_time_unix_ms,
            confirmed,
        } => {
            if !confirmed {
                return Err(FileRecoveryFailure::InvalidRequest);
            }
            vault
                .acknowledge_clock(*displayed_time_unix_ms)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        FileRecoveryFailure::InvalidRequest
                    } else {
                        storage_failure(error)
                    }
                })?;
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: vault.pending_cleanup_count(),
                    unknown_outcomes: vault.unknown_outcome_count(Some(scope)),
                },
            })
        }
        FileRecoveryCommand::Discard {
            recovery_id,
            conversation_id,
            confirmed,
        } => {
            if !confirmed {
                return Err(FileRecoveryFailure::InvalidRequest);
            }
            vault
                .discard(scope, conversation_id, recovery_id, now)
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: vault.pending_cleanup_count(),
                    unknown_outcomes: vault
                        .list(scope, None)
                        .iter()
                        .filter(|record| {
                            record.change == ChangeState::OutcomeUnknown
                                && record.material != MaterialState::Purged
                        })
                        .count() as u64,
                },
            })
        }
        FileRecoveryCommand::Query {
            conversation_id,
            after,
        } => {
            let oldest = vault.oldest_pending_record(Some(scope), conversation_id.as_deref());
            let mut records: Vec<_> = vault
                .list(scope, conversation_id.as_deref())
                .into_iter()
                .filter(|r| {
                    r.material != MaterialState::Purged
                        && after.as_deref().is_none_or(|id| r.id.as_str() > id)
                })
                .take(101)
                .collect();
            let more = records.len() > 100;
            records.truncate(100);
            Ok(FileRecoveryOutcome::Page {
                page: FileRecoveryPageDto {
                    execution_epoch: vault.execution_epoch(),
                    cleanup_warning: vault
                        .cleanup_clock_paused()
                        .then_some(FileRecoveryFailure::ClockChanged),
                    clock_confirmation_time_unix_ms: vault.cleanup_clock_paused().then_some(now),
                    oldest_pending_at_unix_ms: oldest.as_ref().map(|record| record.created_at_ms),
                    oldest_pending_record: oldest.map(|record| project_record(vault, record, now)),
                    policy: policy(vault),
                    used_bytes: vault.used_bytes(),
                    reserved_bytes: 0,
                    next_cursor: more.then(|| records.last().unwrap().id.clone()),
                    records: records
                        .into_iter()
                        .map(|r| project_record(vault, r, now))
                        .collect(),
                },
            })
        }
        FileRecoveryCommand::SetPolicy {
            retention_days,
            max_bytes,
        } => {
            vault
                .set_policy(desk_file_recovery::Policy {
                    retention_days: *retention_days,
                    max_bytes: *max_bytes,
                })
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Policy {
                policy: policy(vault),
            })
        }
        FileRecoveryCommand::DeleteConversation { conversation_id } => {
            let foreign = vault.has_foreign_conversation(scope, conversation_id);
            vault
                .delete_conversation(scope, conversation_id, now)
                .map_err(storage_failure)?;
            if foreign {
                return Err(FileRecoveryFailure::IdentityChanged);
            }
            Ok(FileRecoveryOutcome::Deleted {
                complete: vault.cleanup_complete(scope, conversation_id),
            })
        }
        FileRecoveryCommand::RetryCleanup => {
            vault.recover_interrupted(now).map_err(storage_failure)?;
            vault.sweep(now).map_err(storage_failure)?;
            let records = vault.list(scope, None);
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: records
                        .iter()
                        .filter(|r| r.transaction.is_some() || r.cleanup_error.is_some())
                        .count() as u64,
                    unknown_outcomes: records
                        .iter()
                        .filter(|r| {
                            matches!(
                                r.change,
                                ChangeState::CommitIntent | ChangeState::OutcomeUnknown
                            ) && r.material != MaterialState::Purged
                        })
                        .count() as u64,
                },
            })
        }
        FileRecoveryCommand::Export {
            recovery_id,
            conversation_id,
            entry,
            offset,
        } => {
            use base64::Engine;
            let chunk = export_chunk(vault, |vault, cache| {
                vault.export_chunk(
                    cache,
                    scope,
                    conversation_id,
                    recovery_id,
                    u64::from(*entry),
                    *offset,
                    now,
                )
            })
            .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::ExportChunk {
                package_revision: chunk.package_revision,
                total_size: chunk.total_size,
                offset: chunk.offset,
                data_base64: base64::engine::general_purpose::STANDARD.encode(chunk.data),
            })
        }
        FileRecoveryCommand::QueryEntries {
            recovery_id,
            conversation_id,
            offset,
        } => {
            let page = vault
                .query_entries(scope, conversation_id, recovery_id, u64::from(*offset))
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Entries {
                page: FileRecoveryEntryPageDto {
                    entries: page
                        .entries
                        .into_iter()
                        .map(|entry| FileRecoveryEntryDto {
                            entry: u32::try_from(entry.index).unwrap_or(u32::MAX),
                            path: entry.rel_path,
                            export_available: entry.before_sha256.is_some(),
                        })
                        .collect(),
                    next_offset: page
                        .next_offset
                        .map(|next| u32::try_from(next).unwrap_or(u32::MAX)),
                },
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use desk_file_recovery::{
        BackupRequest, ChangeState, EntryInput, LockedVault, Record, RecordKind, Scope,
    };
    /// A deletion backup of `before` left at BackupReady, as an in-flight change.
    pub(crate) fn prepare(
        vault: &mut LockedVault,
        scope: &Scope,
        conversation: &str,
        operation: &str,
        before: &[u8],
        now_ms: u64,
    ) -> Record {
        vault
            .backup(
                BackupRequest {
                    scope: scope.clone(),
                    conversation,
                    operation,
                    generation: "generation",
                    kind: RecordKind::TextDelete,
                    directory_identity: "1:2",
                    entries: vec![EntryInput {
                        rel_path: "notes.txt",
                        before: Some(before),
                        after: None,
                        metadata: Some(b"{}"),
                    }],
                    transaction: None,
                    now_ms,
                },
                vault.execution_epoch(),
                |_, _| Ok(()),
            )
            .unwrap()
    }
    /// A settled single-file backup of `before`, as a completed deletion.
    pub(crate) fn backup(
        vault: &mut LockedVault,
        scope: &Scope,
        conversation: &str,
        operation: &str,
        before: &[u8],
    ) -> Record {
        let now = jiff::Timestamp::now().as_millisecond().max(1) as u64;
        let record = prepare(vault, scope, conversation, operation, before, now);
        vault
            .transition(scope, &record.id, ChangeState::CommitIntent)
            .unwrap();
        vault
            .finish(scope, &record.id, ChangeState::Succeeded)
            .unwrap();
        vault.record(scope, &record.id).unwrap().clone()
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::test_support::{backup, prepare};
    use super::*;
    use desk_ipc_protocol::message::*;
    fn payload(scope: &Scope, command: FileRecoveryCommand) -> FileRecoveryRequestPayload {
        FileRecoveryRequestPayload {
            request_id: "request".into(),
            connection_id: None,
            authority: scope.authority.clone(),
            actor_id: scope.owner.clone(),
            device_id: scope.device.clone(),
            request: FileRecoveryRequest {
                expected_authority: None,
                expected_os_user: Some(scope.os_user.clone()),
                command,
            },
        }
    }
    fn local_scope() -> Scope {
        Scope {
            os_user: unsafe { libc::geteuid() }.to_string(),
            ..scope()
        }
    }
    #[test]
    fn management_rejects_changed_os_user_before_opening_storage() {
        let mut changed = scope();
        changed.os_user = "different-user".into();
        assert!(matches!(
            execute(
                None,
                &payload(
                    &changed,
                    FileRecoveryCommand::SetPolicy {
                        retention_days: 7,
                        max_bytes: 1_048_576,
                    }
                ),
                None
            )
            .outcome,
            FileRecoveryOutcome::Unavailable {
                reason: FileRecoveryFailure::IdentityChanged
            }
        ));
    }
    #[test]
    fn remote_query_uses_shared_policy_and_usage_without_exposing_other_user_records() {
        let root = tempfile::tempdir().unwrap();
        let scope = local_scope();
        let record = backup(
            &mut desk_file_recovery::Vault::open(root.path())
                .unwrap()
                .lock()
                .unwrap(),
            &scope,
            "conversation",
            "operation",
            b"before",
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let client = crate::worker::session::QuotaClient::new(tx);
        let responder = client.clone();
        let path = root.path().to_owned();
        let worker = std::thread::spawn(move || {
            execute(
                Some(&path),
                &payload(
                    &scope,
                    FileRecoveryCommand::Query {
                        conversation_id: Some("conversation".into()),
                        after: None,
                    },
                ),
                Some(&client),
            )
        });
        let WorkerToService::FileRecoveryQuotaRequested(request) = next_quota_request(&mut rx)
        else {
            panic!("quota read expected")
        };
        assert!(matches!(request.command, FileRecoveryQuotaCommand::Read));
        responder.complete(FileRecoveryQuotaReply {
            request_id: request.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 12,
                max_bytes: 1048576,
                used_bytes: 999999,
                reserved_bytes: 123456,
            },
        });
        let FileRecoveryOutcome::Page { page } = worker.join().unwrap().outcome else {
            panic!("page expected")
        };
        assert_eq!(page.policy.retention_days, 12);
        assert_eq!(page.used_bytes, 999999);
        assert_eq!(page.reserved_bytes, 123456);
        assert_eq!(page.records.len(), 1);
        let projected = &page.records[0];
        assert_eq!(projected.conversation_id, "conversation");
        assert_eq!(
            projected.expires_at_unix_ms,
            record.created_at_ms + 12 * 86_400_000
        );
        assert_eq!(projected.file_name, "notes.txt");
        assert_eq!(projected.entry_count, 1);
        assert_eq!(projected.size_bytes, record.published_bytes);
        assert!(projected.size_bytes > 0 && projected.size_bytes % 4096 == 0);
    }
    #[test]
    fn deletion_settles_absolute_usage_before_reporting_completion() {
        let root = tempfile::tempdir().unwrap();
        let scope = local_scope();
        for applied in [false, true] {
            let conversation = format!("conversation-{applied}");
            let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
            backup(
                &mut vault.lock().unwrap(),
                &scope,
                &conversation,
                "operation",
                b"before",
            );
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let client = crate::worker::session::QuotaClient::new(tx);
            let responder = client.clone();
            let path = root.path().to_owned();
            let payload_scope = scope.clone();
            let deleted = conversation.clone();
            let worker = std::thread::spawn(move || {
                execute(
                    Some(&path),
                    &payload(
                        &payload_scope,
                        FileRecoveryCommand::DeleteConversation {
                            conversation_id: deleted,
                        },
                    ),
                    Some(&client),
                )
            });
            let WorkerToService::FileRecoveryQuotaRequested(request) = next_quota_request(&mut rx)
            else {
                panic!("quota settlement expected")
            };
            let FileRecoveryQuotaCommand::Settle { identity, usage } = request.command else {
                panic!("settlement expected")
            };
            assert!(identity.is_none());
            assert!(usage.namespaces.is_empty(), "deleted objects were swept");
            responder.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: FileRecoveryQuotaOutcome::Settled { applied },
            });
            assert!(matches!(
                worker.join().unwrap().outcome,
                FileRecoveryOutcome::Deleted { complete } if complete == applied
            ));
            assert_eq!(
                vault
                    .lock()
                    .unwrap()
                    .cleanup_complete(&scope, &conversation),
                applied
            );
        }
    }
    fn next_quota_request(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
    ) -> WorkerToService {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .expect("worker quota request timed out")
                    .expect("worker quota channel closed")
            })
    }
    fn scope() -> Scope {
        Scope {
            authority: "a".repeat(64),
            device: "device".into(),
            os_user: "501".into(),
            owner: "1".into(),
        }
    }
    #[test]
    fn clock_confirmation_rejects_missing_approval_and_stale_time_before_resuming() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        locked.cleanup(1000).unwrap();
        assert!(locked.cleanup(999).is_err());
        let scope = scope();
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: 1000,
                    confirmed: false
                },
                1000
            ),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: 1000,
                    confirmed: true
                },
                1000
            ),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(locked.cleanup_clock_paused());
        let now = jiff::Timestamp::now().as_millisecond() as u64;
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: now,
                    confirmed: true
                },
                now
            ),
            Ok(FileRecoveryOutcome::Cleanup { .. })
        ));
        assert!(!locked.cleanup_clock_paused());
    }
    #[test]
    fn clock_failure_is_readable_and_query_keeps_the_warning_and_oldest_record() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let scope = scope();
        let record = prepare(
            &mut locked,
            &scope,
            "conversation",
            "operation",
            b"before",
            1000,
        );
        locked
            .transition(&scope, &record.id, ChangeState::CommitIntent)
            .unwrap();
        locked.recover_interrupted(1001).unwrap();
        assert!(matches!(
            execute_locked(&mut locked, &scope, &FileRecoveryCommand::RetryCleanup, 999),
            Err(FileRecoveryFailure::ClockChanged)
        ));
        let FileRecoveryOutcome::Page { page } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: None,
                after: None,
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert_eq!(
            page.cleanup_warning,
            Some(FileRecoveryFailure::ClockChanged)
        );
        assert_eq!(page.oldest_pending_at_unix_ms, Some(1000));
        assert_eq!(
            page.oldest_pending_record.as_ref().unwrap().recovery_id,
            record.id
        );
        let FileRecoveryOutcome::Page { page: beyond } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: Some("conversation".into()),
                after: Some("z".into()),
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert!(beyond.records.is_empty());
        assert_eq!(beyond.oldest_pending_record.unwrap().recovery_id, record.id);
        let FileRecoveryOutcome::Page { page: unrelated } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: Some("other-conversation".into()),
                after: None,
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert!(unrelated.oldest_pending_record.is_none());
        assert!(unrelated.oldest_pending_at_unix_ms.is_none());

        assert!(page.records[0].export_available);
        let mut other = scope.clone();
        other.owner = "other".into();
        assert_eq!(locked.oldest_pending_created_at(Some(&other)), None);
        assert_eq!(
            storage_failure(io::Error::from(io::ErrorKind::WouldBlock)),
            FileRecoveryFailure::Busy
        );
    }
    #[test]
    fn discard_requires_confirmation_and_preserves_unknown_result() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let scope = scope();
        let record = prepare(
            &mut locked,
            &scope,
            "conversation",
            "operation",
            b"before",
            1000,
        );
        locked
            .transition(&scope, &record.id, ChangeState::CommitIntent)
            .unwrap();
        let later = record.expires_at_ms + 1;
        locked.recover_interrupted(later).unwrap();
        assert!(
            project_record(&locked, locked.list(&scope, None)[0].clone(), later).export_available
        );
        let mut command = FileRecoveryCommand::Discard {
            recovery_id: record.id.clone(),
            conversation_id: "conversation".into(),
            confirmed: false,
        };
        assert!(matches!(
            execute_locked(&mut locked, &scope, &command, later),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(
            locked
                .entry_material(&scope, "conversation", &record.id, 0, later)
                .is_ok()
        );
        if let FileRecoveryCommand::Discard { confirmed, .. } = &mut command {
            *confirmed = true;
        }
        assert!(matches!(
            execute_locked(&mut locked, &scope, &command, later),
            Ok(FileRecoveryOutcome::Cleanup { .. })
        ));
        let retained = locked.list(&scope, None)[0].clone();
        assert_eq!(retained.change, ChangeState::OutcomeUnknown);
        assert_eq!(retained.material, MaterialState::Purged);
        assert!(!project_record(&locked, retained, later).export_available);
    }
    #[test]
    fn export_failures_keep_material_categories() {
        for (source, expected) in [
            (
                desk_file_recovery::ExportError::Expired,
                FileRecoveryFailure::MaterialExpired,
            ),
            (
                desk_file_recovery::ExportError::Cleaning,
                FileRecoveryFailure::MaterialCleaning,
            ),
            (
                desk_file_recovery::ExportError::Cleaned,
                FileRecoveryFailure::MaterialCleaned,
            ),
            (
                desk_file_recovery::ExportError::Preparing,
                FileRecoveryFailure::Busy,
            ),
            (
                desk_file_recovery::ExportError::Unavailable,
                FileRecoveryFailure::MaterialUnavailable,
            ),
        ] {
            assert_eq!(
                storage_failure(io::Error::new(io::ErrorKind::NotFound, source)),
                expected
            );
        }
    }
    fn export(
        vault: &mut LockedVault,
        scope: &Scope,
        conversation: &str,
        id: &str,
        offset: u64,
        now: u64,
    ) -> Result<(u64, u64, u64, Vec<u8>), FileRecoveryFailure> {
        use base64::Engine;
        match execute_locked(
            vault,
            scope,
            &FileRecoveryCommand::Export {
                recovery_id: id.into(),
                conversation_id: conversation.into(),
                entry: 0,
                offset,
            },
            now,
        )? {
            FileRecoveryOutcome::ExportChunk {
                package_revision,
                total_size,
                offset,
                data_base64,
            } => Ok((
                package_revision,
                total_size,
                offset,
                base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .unwrap(),
            )),
            _ => panic!("export chunk expected"),
        }
    }
    /// Pull every chunk; a finished stream frees its export slot.
    fn download(vault: &mut LockedVault, scope: &Scope, id: &str, now: u64) -> (u64, Vec<u8>) {
        let mut package = Vec::new();
        let mut first = None;
        loop {
            let (revision, total, offset, data) =
                export(vault, scope, "conversation", id, package.len() as u64, now).unwrap();
            assert_eq!(offset, package.len() as u64);
            assert!(data.len() <= MAX_EXPORT_CHUNK_BYTES);
            assert_eq!(*first.get_or_insert((revision, total)), (revision, total));
            package.extend(data);
            if package.len() as u64 == total {
                return (revision, package);
            }
        }
    }
    #[test]
    fn chunked_export_reassembles_one_revision_and_lists_entries() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut v = vault.lock().unwrap();
        let s = scope();
        // Incompressible text forces a package larger than one chunk.
        let mut state = 0x9e37_79b9_u32;
        let content: Vec<u8> = (0..1_200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                b'a' + (state % 26) as u8
            })
            .collect();
        let saved = backup(&mut v, &s, "conversation", "op", &content);
        let now = saved.created_at_ms + 1;
        let (revision, package) = download(&mut v, &s, &saved.id, now);
        assert!(package.len() > MAX_EXPORT_CHUNK_BYTES);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(package)).unwrap();
        let mut before = Vec::new();
        archive
            .by_name("before.txt")
            .unwrap()
            .read_to_end(&mut before)
            .unwrap();
        assert_eq!(before, content);
        let FileRecoveryOutcome::Entries { page } = execute_locked(
            &mut v,
            &s,
            &FileRecoveryCommand::QueryEntries {
                recovery_id: saved.id.clone(),
                conversation_id: "conversation".into(),
                offset: 0,
            },
            now,
        )
        .unwrap() else {
            panic!("entries expected")
        };
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].path, "notes.txt");
        assert!(page.entries[0].export_available);
        assert!(page.next_offset.is_none());
        // A retention change bumps the package revision between chunks.
        v.set_policy(desk_file_recovery::Policy {
            retention_days: 30,
            max_bytes: desk_file_recovery::Policy::default().max_bytes,
        })
        .unwrap();
        assert_ne!(download(&mut v, &s, &saved.id, now).0, revision);
    }
    #[test]
    fn remote_recovery_keeps_old_authorities_and_binds_exports_to_conversations() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut v = vault.lock().unwrap();
        let s = scope();
        let saved = backup(&mut v, &s, "conversation", "op", b"before");
        let now = saved.created_at_ms;
        assert!(matches!(
            export(&mut v, &s, "other", &saved.id, 0, now + 1),
            Err(FileRecoveryFailure::MaterialUnavailable)
        ));
        let mut changed = s.clone();
        changed.authority = "b".repeat(64);
        assert!(matches!(
            execute_locked(
                &mut v,
                &changed,
                &FileRecoveryCommand::DeleteConversation {
                    conversation_id: "conversation".into()
                },
                now + 1
            ),
            Err(FileRecoveryFailure::IdentityChanged)
        ));
        assert!(export(&mut v, &s, "conversation", &saved.id, 0, now + 2).is_ok());
        assert!(matches!(
            execute_locked(
                &mut v,
                &s,
                &FileRecoveryCommand::DeleteConversation {
                    conversation_id: "conversation".into()
                },
                now + 3
            ),
            Ok(FileRecoveryOutcome::Deleted { complete: false })
        ));
        // Completion waits for the device quota to accept the swept usage.
        let usage = v.usage_report();
        v.acknowledge_settlement(&usage).unwrap();
        assert!(v.cleanup_complete(&s, "conversation"));
        assert!(matches!(
            export(&mut v, &s, "conversation", &saved.id, 0, now + 4),
            Err(FileRecoveryFailure::MaterialUnavailable)
        ));
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::open_device_quota_for;

    #[test]
    fn the_service_and_a_portable_run_keep_separate_quota_ledgers() {
        let root = tempfile::tempdir().unwrap();
        let mut portable = open_device_quota_for(root.path(), false)
            .unwrap()
            .lock()
            .unwrap();
        portable
            .set_policy(desk_file_recovery::Policy {
                retention_days: 3,
                max_bytes: 64 * 1024 * 1024,
            })
            .unwrap();
        drop(portable);
        // The service never reads the portable ledger, and its own starts fresh.
        let service = open_device_quota_for(root.path(), true)
            .unwrap()
            .lock()
            .unwrap();
        assert_ne!(service.policy().retention_days, 3);
        drop(service);
        assert!(root.path().join("file-recovery-quota").is_dir());
        assert!(root.path().join("file-recovery-quota-service").is_dir());
        assert!(
            desk_file_recovery::quota::DeviceQuota::open_named(root.path(), "elsewhere").is_err()
        );
    }
}
