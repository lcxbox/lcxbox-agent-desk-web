//! Trusted recovery binding and the platform-independent part of native text
//! changes: what an action asks for, the content to leave behind, and the
//! vault steps around the platform commit.
//!
//! Every platform follows the same crash-safe order: a durable Preparing
//! record, the quota reservation, object publication and BackupReady
//! (`backup`), the platform commit under CommitIntent, `finish`, and an
//! absolute usage settlement. Quota requests are answered before the vault
//! lock is released.
use super::*;
use desk_agent_protocol::computer_use::{
    CreatedFileArtifactOutput, FilePatchAction, FileRecoveryDescriptor, RestoreExpectation,
    TextFileChange, TextFileMutationOperation, TextFileMutationOutput,
};
use desk_file_recovery::{
    BackupRequest, ChangeState, EntryInput, LockedVault, Record, RecordKind, TransactionPlan,
};

pub(crate) struct RecoveryContext {
    pub data_root: PathBuf,
    pub execution_epoch: u64,
    pub quota: Option<crate::worker::session::QuotaClient>,
    pub scope: desk_file_recovery::Scope,
    pub conversation_id: String,
    pub operation_id: String,
    pub generation: String,
    #[cfg(test)]
    pub(crate) _test_data: Option<std::sync::Arc<tempfile::TempDir>>,
}

/// The file an action names.
pub(super) enum TextTarget<'a> {
    /// An issued file reference whose identity must still match.
    Reference(&'a ObjectRef),
    /// A recorded file name under the directory, for a restore.
    Name(&'a str),
}

pub(super) enum TextIntent<'a> {
    Edit(&'a TextFileChange),
    Delete,
    Restore { recovery_id: &'a str },
}

pub(super) struct TextMutation<'a> {
    pub directory: &'a ObjectRef,
    pub target: TextTarget<'a>,
    /// SHA-256 the current file must have; `None` requires it to be absent.
    pub expected_sha256: Option<&'a str>,
    pub intent: TextIntent<'a>,
}

/// The content an action leaves at the file name.
pub(super) struct Planned {
    pub kind: RecordKind,
    /// `None` removes the file.
    pub content: Option<Vec<u8>>,
    /// Backed-up metadata of the restored version, used when recreating it.
    pub metadata: Option<Vec<u8>>,
}

pub(super) struct TextMutationReceipt {
    pub file_name: String,
    pub original_size_bytes: u64,
    pub original_sha256: Option<String>,
    pub verified: bool,
    pub recovery: FileRecoveryDescriptor,
    pub updated_sha256: Option<String>,
    pub file: Option<CreatedTextArtifact>,
    pub message: &'static str,
}

/// Receipt messages of restores. A recreated file keeps only what the
/// platform can safely reproduce, and says so.
#[cfg(unix)]
pub(super) const RECREATED_MESSAGE: &str = "file restored; it was recreated with the saved permission bits (without setuid, setgid or sticky) and the current user as owner; the original owner and ACLs were not restored";
#[cfg(windows)]
pub(super) const RECREATED_MESSAGE: &str = "file restored; it was recreated with the permissions it inherits from its directory; the original owner and ACL were not restored";
pub(super) const RESTORED_MESSAGE: &str = "file restored to its saved version; the replaced state is saved in device-private backup storage";
pub(super) const RESTORE_DELETED_MESSAGE: &str = "file removed as recorded before the restored operation; its content is saved in device-private backup storage";

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn now_ms() -> u64 {
    Utc::now().timestamp_millis().max(1) as u64
}

/// The filesystem identity of a resolved directory as stored with a backup.
pub(super) fn directory_identity(identity: &FileIdentity) -> String {
    format!("{}:{}", identity.primary, identity.secondary)
}

/// Permission bits for a recreated file: the backed-up mode without setuid,
/// setgid and sticky bits.
#[cfg(unix)]
pub(super) fn recreated_mode(metadata: Option<&[u8]>) -> u32 {
    metadata
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|value| value.get("mode")?.as_u64())
        .map_or(0o600, |mode| mode as u32 & 0o777)
}

fn restore_conflict(message: &str) -> AgentError {
    error(AgentErrorKind::PermissionDenied, message, false)
}

impl RecoveryContext {
    /// Lock the vault and bring it to the device policy and execution epoch.
    pub(crate) fn open_vault(&self) -> Result<LockedVault, AgentError> {
        let vault = desk_file_recovery::Vault::open(&self.data_root)
            .map_err(|e| io_error("open private file backups", e))?;
        let mut vault = vault
            .lock()
            .map_err(|e| io_error("lock private file backups", e))?;
        vault
            .observe_system_clock()
            .map_err(|e| io_error("observe backup clock", e))?;
        if let Some(quota) = &self.quota {
            vault
                .maintain_epoch_indexes(&self.scope.os_user, &mut quota.clone(), usize::MAX)
                .map_err(|e| io_error("resume backup index cleanup", e))?;
            let (policy, _, _) = quota
                .policy()
                .map_err(|e| io_error("read device backup policy", e))?;
            if vault.policy() != &policy {
                vault
                    .set_policy(policy)
                    .map_err(|e| io_error("apply device backup policy", e))?;
            }
        }
        vault
            .require_execution_epoch(self.execution_epoch)
            .map_err(|e| {
                io_error(
                    "validate backup execution epoch; request a new operation",
                    e,
                )
            })?;
        vault
            .cleanup(now_ms())
            .map_err(|e| io_error("maintain private file backups", e))?;
        Ok(vault)
    }

    /// The content `intent` leaves at `file_name`. A restore reads the backed-up
    /// version only when the record belongs to this conversation and its
    /// directory has the identity of the directory resolved now, so a
    /// recreated file never lands in another directory.
    pub(super) fn plan(
        &self,
        vault: &LockedVault,
        intent: &TextIntent<'_>,
        original: Option<&[u8]>,
        file_name: &str,
        directory_identity: &str,
    ) -> Result<Planned, AgentError> {
        match intent {
            TextIntent::Edit(change) => {
                let text = original
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .ok_or_else(|| restore_conflict("text file is unavailable"))?;
                let content = super::text_edit::apply(text, change)?.into_bytes();
                if content.contains(&0) {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "text changes must not contain NUL characters",
                        false,
                    ));
                }
                Ok(Planned {
                    kind: RecordKind::TextEdit,
                    content: Some(content),
                    metadata: None,
                })
            }
            TextIntent::Delete => Ok(Planned {
                kind: RecordKind::TextDelete,
                content: None,
                metadata: None,
            }),
            TextIntent::Restore { recovery_id } => {
                let material = vault
                    .entry_material(&self.scope, &self.conversation_id, recovery_id, 0, now_ms())
                    .map_err(|e| io_error("read the backed-up version", e))?;
                if material.entry.rel_path != file_name {
                    return Err(restore_conflict(
                        "backup belongs to another file; nothing was changed",
                    ));
                }
                if material.record.directory_identity != directory_identity {
                    return Err(restore_conflict(
                        "backup belongs to another directory or the directory was replaced; nothing was changed",
                    ));
                }
                if original.is_none() && material.before.is_none() {
                    return Err(restore_conflict(
                        "file is already absent as recorded; nothing was changed",
                    ));
                }
                Ok(Planned {
                    kind: RecordKind::TextRestore,
                    content: material.before,
                    metadata: material.metadata,
                })
            }
        }
    }

    /// Steps 1-4: durable Preparing record, quota reservation, object
    /// publication and BackupReady. A failed backup aborts its record; its
    /// reservation is cleared here.
    #[allow(
        clippy::too_many_arguments,
        reason = "one backup entry and its transaction; a request struct would only rename them"
    )]
    pub(super) fn backup(
        &self,
        vault: &mut LockedVault,
        kind: RecordKind,
        directory_identity: &str,
        file_name: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
        metadata: Option<&[u8]>,
        transaction: Option<TransactionPlan>,
    ) -> Result<Record, AgentError> {
        let request = BackupRequest {
            scope: self.scope.clone(),
            conversation: &self.conversation_id,
            operation: &self.operation_id,
            generation: &self.generation,
            kind,
            directory_identity,
            entries: vec![EntryInput {
                rel_path: file_name,
                before,
                after,
                metadata,
            }],
            transaction,
            now_ms: now_ms(),
        };
        let result = match &self.quota {
            Some(quota) => vault.backup(request, self.execution_epoch, |record, bytes| {
                quota.reserve(record, bytes)
            }),
            None => {
                #[cfg(test)]
                {
                    vault.backup(request, self.execution_epoch, |_, _| Ok(()))
                }
                #[cfg(not(test))]
                {
                    let _ = request;
                    Err(std::io::Error::other(
                        "device backup quota is unavailable; file was not changed",
                    ))
                }
            }
        };
        result.map_err(|e| {
            if let Some(record) = vault
                .list(&self.scope, Some(&self.conversation_id))
                .into_iter()
                .find(|record| record.operation == self.operation_id)
            {
                self.settle(vault, &record);
            }
            io_error("save private file backup; target not changed", e)
        })
    }

    /// Record a final outcome and remove the transaction, then settle.
    /// Returns whether no transaction remains.
    pub(super) fn finish(
        &self,
        vault: &mut LockedVault,
        record: &Record,
        outcome: ChangeState,
    ) -> bool {
        let settled = match vault.finish(&self.scope, &record.id, outcome) {
            Ok(settled) => settled,
            Err(e) => {
                tracing::warn!(error_kind = ?e.kind(), "file change outcome needs journal recovery");
                false
            }
        };
        self.settle(vault, record);
        settled
    }

    /// Clear the operation's reservation and settle absolute usage. The
    /// published objects are already in the usage; a failed settlement is
    /// corrected by the next reconciliation.
    pub(crate) fn settle(&self, vault: &mut LockedVault, record: &Record) {
        if let Some(quota) = &self.quota
            && let Err(e) = quota.settle_vault(vault, Some(record))
        {
            tracing::warn!(error_kind = ?e.kind(), "file backup quota settlement unconfirmed; reconciliation will correct it");
        }
    }

    /// Clear this context's operation reservation and settle absolute usage
    /// when no record of the operation is at hand (a skipped, abandoned or
    /// discarded command snapshot).
    pub(crate) fn settle_operation(&self, vault: &mut LockedVault) {
        let Some(quota) = &self.quota else {
            return;
        };
        let identity = desk_ipc_protocol::message::FileRecoveryQuotaIdentity {
            execution_epoch: self.execution_epoch,
            authority: self.scope.authority.clone(),
            device: self.scope.device.clone(),
            owner: self.scope.owner.clone(),
            conversation: self.conversation_id.clone(),
            operation: self.operation_id.clone(),
        };
        if let Err(e) = quota.settle_vault_clearing(vault, Some(identity)) {
            tracing::warn!(error_kind = ?e.kind(), "file backup quota settlement unconfirmed; reconciliation will correct it");
        }
    }

    pub(super) fn descriptor(record: &Record, cleanup_pending: bool) -> FileRecoveryDescriptor {
        FileRecoveryDescriptor {
            recovery_id: record.id.clone(),
            created_at_unix_ms: record.created_at_ms,
            expires_at_unix_ms: record.expires_at_ms,
            cleanup_pending,
        }
    }
}

/// Run one dispatched update, deletion or restore and project its receipt.
pub(crate) fn execute(
    target: &ObjectRef,
    action: &FilePatchAction,
    context: &RecoveryContext,
    commit_guard: impl Fn() -> Result<(), AgentError>,
) -> Result<(TextFileMutationOutput, &'static str), AgentError> {
    let conflict = || {
        error(
            AgentErrorKind::PermissionDenied,
            "text file identity or complete content version changed; read again and request new approval",
            false,
        )
    };
    action.validate_text_mutation().map_err(|_| conflict())?;
    let (directory, mutation, operation) = match action {
        FilePatchAction::UpdateText {
            directory,
            expected_sha256,
            change,
        } => (
            directory,
            TextMutation {
                directory,
                target: TextTarget::Reference(target),
                expected_sha256: Some(expected_sha256),
                intent: TextIntent::Edit(change),
            },
            TextFileMutationOperation::Update,
        ),
        FilePatchAction::DeleteText {
            directory,
            expected_sha256,
        } => (
            directory,
            TextMutation {
                directory,
                target: TextTarget::Reference(target),
                expected_sha256: Some(expected_sha256),
                intent: TextIntent::Delete,
            },
            TextFileMutationOperation::Delete,
        ),
        FilePatchAction::RestoreText {
            directory,
            file_name,
            recovery_id,
            expected_current,
        } => {
            if target != directory {
                return Err(conflict());
            }
            (
                directory,
                TextMutation {
                    directory,
                    target: TextTarget::Name(file_name),
                    expected_sha256: match expected_current {
                        RestoreExpectation::Sha(sha256) => Some(sha256),
                        RestoreExpectation::Absent => None,
                    },
                    intent: TextIntent::Restore { recovery_id },
                },
                TextFileMutationOperation::Restore,
            )
        }
        _ => return Err(conflict()),
    };
    let receipt = super::text_mutation::mutate(mutation, context, &commit_guard)?;
    let output = TextFileMutationOutput {
        operation,
        original: target.clone(),
        directory: directory.clone(),
        original_file_name: receipt.file_name,
        original_size_bytes: receipt.original_size_bytes,
        original_sha256: receipt.original_sha256,
        recovery: receipt.recovery,
        verified: receipt.verified,
        updated_sha256: receipt.updated_sha256.filter(|_| receipt.verified),
        updated_file: receipt.file.filter(|_| receipt.verified).map(|file| {
            CreatedFileArtifactOutput {
                content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                    artifact_id: file.file.token.clone(),
                    sha256: file.sha256.clone(),
                    size_bytes: file.byte_len,
                    media_type: "text/plain;charset=utf-8".into(),
                },
                file: file.file,
                file_name: file.file_name,
                media_type: "text/plain;charset=utf-8".into(),
                size_bytes: file.byte_len,
                digest_sha256: file.sha256,
            }
        }),
    };
    Ok((output, receipt.message))
}

#[cfg(test)]
pub(super) fn test_context() -> RecoveryContext {
    let fixture = std::sync::Arc::new(tempfile::tempdir().unwrap());
    let data_root = fixture.path().to_path_buf();
    RecoveryContext {
        _test_data: Some(fixture),
        execution_epoch: 0,
        data_root,
        quota: None,
        scope: desk_file_recovery::Scope {
            authority: "test".into(),
            device: "device".into(),
            os_user: "test-user".into(),
            owner: "owner".into(),
        },
        conversation_id: "conversation".into(),
        operation_id: uuid::Uuid::new_v4().to_string(),
        generation: "generation".into(),
    }
}

/// The content a recovery record saved for its file.
#[cfg(test)]
pub(super) fn backed_up(context: &RecoveryContext, recovery_id: &str) -> Option<Vec<u8>> {
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    let locked = vault.lock().unwrap();
    locked
        .entry_material(
            &context.scope,
            &context.conversation_id,
            recovery_id,
            0,
            now_ms(),
        )
        .unwrap()
        .before
}
