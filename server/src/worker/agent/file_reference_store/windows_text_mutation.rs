//! Windows execution of the shared exact text-change contract.
use super::text_recovery_context::{
    Planned, RECREATED_MESSAGE, RESTORE_DELETED_MESSAGE, RESTORED_MESSAGE, TextMutation,
    TextMutationReceipt, TextTarget, digest, directory_identity,
};
pub(crate) use super::text_recovery_context::{RecoveryContext, execute};
use super::*;
use desk_agent_protocol::computer_use::CreatedFileArtifactOutput;
use desk_file_recovery::{
    ChangeState, LockedVault, RecordKind, TransactionPlan, WindowsCommitRequest,
    windows::{
        FileKind, MUTATION_DIRECTORY_ACCESS, MoveOutcome, SourceSnapshot, file_identity,
        move_no_replace,
    },
};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

fn conflict() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "Selected file or backup context changed; read again and request approval",
        false,
    )
}

// Adding children and flushing the directory do not require changing its
// attributes or extended attributes. Public directories may deny those rights
// while permitting safe replacement of user-owned files.
fn open_mutation_parent(path: &Path) -> Result<OpenedFile, AgentError> {
    let parent = open_verified_with_access(
        path,
        MUTATION_DIRECTORY_ACCESS.0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE).0,
    )
    .map_err(|mut cause| {
        cause.message = format!(
            "open parent directory for recoverable file replacement (read/create children): {}",
            cause.message
        );
        cause
    })?;
    // Fail before backup or any rename if this filesystem cannot flush the held
    // handle. Commit still flushes after the moves and retains unknown outcomes.
    parent.handle.sync_all().map_err(|cause| {
        io_error(
            "flush parent directory before recoverable file replacement",
            cause,
        )
    })?;
    Ok(parent)
}

fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn mutate(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    commit_guard: &dyn Fn() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    if request.directory.object_kind != ObjectKind::Directory
        || crate::file_recovery_service::platform_user::current().map_err(|_| conflict())?
            != context.scope.os_user
    {
        return Err(conflict());
    }
    commit_guard()?;
    let root = resolve(request.directory)?;
    let (parent_anchor, _ancestors, parent_id) =
        windows_path_anchor::open_anchored(&root.path, FileKind::Directory)?;
    if parent_anchor.identity != root.identity {
        return Err(conflict());
    }
    let (name, selected_identity) = match request.target {
        TextTarget::Reference(target) => {
            if target.object_kind != ObjectKind::File {
                return Err(conflict());
            }
            let selected = resolve(target)?;
            let (selected_parent, _selected_ancestors, selected_parent_id) =
                windows_path_anchor::open_anchored(
                    selected.path.parent().ok_or_else(conflict)?,
                    FileKind::Directory,
                )?;
            if parent_id != selected_parent_id {
                return Err(conflict());
            }
            drop(selected_parent);
            let name = selected
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(conflict)?
                .to_owned();
            (name, Some(selected.identity))
        }
        TextTarget::Name(name) => (name.to_owned(), None),
    };
    if name.is_empty()
        || name.len() > 255
        || matches!(name.as_str(), "." | "..")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(conflict());
    }
    let parent = open_mutation_parent(&root.path)?;
    if file_identity(&parent.handle, FileKind::Directory).map_err(|_| conflict())? != parent_id {
        return Err(conflict());
    }
    let path = root.path.join(&name);
    let source = if request.expected_sha256.is_none() && absent(&path) {
        None
    } else {
        let opened = open_verified(&path)?;
        if selected_identity
            .as_ref()
            .is_some_and(|identity| opened.identity != *identity)
        {
            return Err(conflict());
        }
        let expected_id = file_identity(&opened.handle, FileKind::File).map_err(|_| conflict())?;
        let source = SourceSnapshot::capture(&parent.handle, &name, expected_id)
            .map_err(|cause| io_error("capture recoverable text source", cause))?;
        drop(opened);
        Some(source)
    };
    if source
        .as_ref()
        .map(SourceSnapshot::content_sha256)
        .as_deref()
        != request.expected_sha256
    {
        return Err(conflict());
    }
    let mut vault = context.open_vault()?;
    let directory_identity = directory_identity(&parent.identity);
    let planned = context.plan(
        &vault,
        &request.intent,
        source.as_ref().map(SourceSnapshot::content),
        &name,
        &directory_identity,
    )?;
    match source {
        Some(source) => replace(
            context,
            &mut vault,
            &root,
            &parent,
            &directory_identity,
            source,
            planned,
            commit_guard,
        ),
        None => recreate(
            context,
            &mut vault,
            &root,
            &parent,
            parent_id,
            &directory_identity,
            &name,
            planned,
            commit_guard,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the resolved directory, source and guard of one commit"
)]
fn replace(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    root: &StoredFile,
    parent: &OpenedFile,
    directory_identity: &str,
    source: SourceSnapshot,
    planned: Planned,
    commit_guard: &dyn Fn() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    let name = source.file_name().to_owned();
    let original_sha256 = source.content_sha256();
    let size = source.content().len() as u64;
    let metadata = source
        .metadata()
        .map_err(|e| io_error("capture file metadata", e))?;
    let transaction = TransactionPlan::windows(&root.path, &parent.handle, source.handle())
        .map_err(|e| io_error("plan source transaction", e))?;
    let content = planned.content;
    let saved = context.backup(
        vault,
        planned.kind,
        directory_identity,
        &name,
        Some(source.content()),
        content.as_deref(),
        Some(&metadata),
        Some(transaction),
    )?;
    let mut attempted = false;
    let result = (|| -> Result<(bool, Option<CreatedTextArtifact>), AgentError> {
        let directory = vault
            .create_windows_transaction_directory(&context.scope, &saved.id, &parent.handle)
            .map_err(|e| io_error("create private transaction", e))?;
        let staged = content
            .as_ref()
            .map(|bytes| {
                source.prepare_replacement(&directory, bytes, |file| {
                    vault.register_windows_transaction(
                        &context.scope,
                        &saved.id,
                        &parent.handle,
                        directory.handle(),
                        Some(file),
                    )
                })
            })
            .transpose()
            .map_err(|e| io_error("prepare text replacement", e))?;
        let outcome = vault.commit_windows_transaction(
            WindowsCommitRequest {
                scope: &context.scope,
                id: &saved.id,
                parent: &parent.handle,
                directory: &directory,
                source: &source,
                replacement: staged.as_ref(),
            },
            || commit_guard().map_err(|e| std::io::Error::other(e.message)),
        );
        match outcome {
            Err(cause) if cause.outcome == MoveOutcome::NotStarted => {
                return Err(io_error(
                    "text change did not start",
                    std::io::Error::other(cause),
                ));
            }
            Err(cause) => {
                // The receipt only says the outcome is unknown; keep the cause
                // for diagnosis (it names a step, never file content).
                tracing::warn!(error = %cause, "text change stopped after the commit started");
                attempted = true;
                return Ok((false, None));
            }
            Ok(()) => attempted = true,
        }
        let updated = if let Some(staged) = &staged {
            let path = root.path.join(&name);
            let opened = open_verified(&path)?;
            if file_identity(&opened.handle, FileKind::File).map_err(|_| conflict())?
                != staged.identity()
            {
                return Ok((false, None));
            }
            let file =
                issue_opened_with_lifetime(&path, opened, DURABLE_ARTIFACT_REF_TTL_SECS, true)?;
            Some(CreatedTextArtifact {
                file,
                file_name: name.clone(),
                byte_len: staged.content().len() as u64,
                sha256: staged.content_sha256(),
            })
        } else {
            None
        };
        Ok((true, updated))
    })();
    drop(source);
    // A commit that moved files but was not verified stays CommitIntent and
    // recovers as an unknown outcome.
    let settled = match (&result, attempted) {
        (_, false) => context.finish(vault, &saved, ChangeState::Aborted),
        (Ok((true, _)), true) => context.finish(vault, &saved, ChangeState::Succeeded),
        _ => {
            context.settle(vault, &saved);
            false
        }
    };
    // Reference issuance or maintenance after native commit cannot be reported
    // as a definitely failed/no-effect operation.
    let (verified, updated_file) = match result {
        Ok(result) => result,
        Err(_) if attempted => (false, None),
        Err(error) => return Err(error),
    };
    Ok(TextMutationReceipt {
        file_name: name,
        original_size_bytes: size,
        original_sha256: Some(original_sha256),
        verified,
        recovery: RecoveryContext::descriptor(&saved, !settled),
        updated_sha256: content.as_deref().map(digest),
        file: updated_file,
        message: match (verified, planned.kind, content.is_some()) {
            (false, ..) => {
                "File change outcome is unknown; recovery material has been retained and the operation will not be repeated"
            }
            (true, RecordKind::TextRestore, true) => RESTORED_MESSAGE,
            (true, RecordKind::TextRestore, false) => RESTORE_DELETED_MESSAGE,
            (true, ..) => {
                "File change completed; the prior version is available in device-private recovery storage"
            }
        },
    })
}

/// Recreate an absent file. The staging file is created in the parent
/// directory, so it inherits the parent's ACL like any new file, and is moved
/// to its final name without replacing anything.
#[allow(
    clippy::too_many_arguments,
    reason = "the resolved directory, name and guard of one commit"
)]
fn recreate(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    root: &StoredFile,
    parent: &OpenedFile,
    parent_id: desk_file_recovery::windows::FileIdentity,
    directory_identity: &str,
    name: &str,
    planned: Planned,
    commit_guard: &dyn Fn() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FileDispositionInfo, SetFileInformationByHandle,
    };
    let content = planned.content.ok_or_else(conflict)?;
    let saved = context.backup(
        vault,
        planned.kind,
        directory_identity,
        name,
        None,
        Some(&content),
        None,
        None,
    )?;
    let staging = root.path.join(format!(".agent-restore-{}", saved.id));
    let mut attempted = false;
    let mut staged_file = None;
    let result = (|| -> Result<CreatedTextArtifact, AgentError> {
        let mut staged = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&staging)
            .map_err(|e| io_error("create restored file", e))?;
        staged_file = Some(staged.try_clone().map_err(|_| conflict())?);
        staged
            .write_all(&content)
            .and_then(|_| staged.sync_all())
            .map_err(|e| io_error("write restored file", e))?;
        let staged_id = file_identity(&staged, FileKind::File).map_err(|_| conflict())?;
        if !absent(&root.path.join(name))
            || file_identity(&parent.handle, FileKind::Directory).map_err(|_| conflict())?
                != parent_id
        {
            return Err(conflict());
        }
        commit_guard()?;
        vault
            .transition(&context.scope, &saved.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))?;
        if let Err(cause) = move_no_replace(&staged, staged_id, &parent.handle, parent_id, name) {
            attempted = cause.outcome != MoveOutcome::NotStarted;
            return Err(io_error(
                "commit restored file",
                std::io::Error::other(cause),
            ));
        }
        attempted = true;
        parent
            .handle
            .sync_all()
            .map_err(|e| io_error("flush restored file directory", e))?;
        let path = root.path.join(name);
        let opened = open_verified(&path)?;
        if file_identity(&opened.handle, FileKind::File).map_err(|_| conflict())? != staged_id {
            return Err(conflict());
        }
        staged_file = None;
        let file = issue_opened_with_lifetime(&path, opened, DURABLE_ARTIFACT_REF_TTL_SECS, true)?;
        Ok(CreatedTextArtifact {
            file,
            file_name: name.into(),
            byte_len: content.len() as u64,
            sha256: digest(&content),
        })
    })();
    if !attempted && let Some(staged) = staged_file {
        // The staging file never reached its final name; delete it by handle.
        let delete = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                HANDLE(staged.as_raw_handle()),
                FileDispositionInfo,
                (&delete as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .is_err()
        {
            tracing::warn!("restore staging file could not be removed");
        }
    }
    let settled = match (&result, attempted) {
        (_, false) => context.finish(vault, &saved, ChangeState::Aborted),
        (Ok(_), true) => context.finish(vault, &saved, ChangeState::Succeeded),
        (Err(_), true) => {
            context.settle(vault, &saved);
            false
        }
    };
    let (verified, file) = match result {
        Ok(file) => (true, Some(file)),
        Err(_) if attempted => (false, None),
        Err(error) => return Err(error),
    };
    Ok(TextMutationReceipt {
        file_name: name.into(),
        original_size_bytes: 0,
        original_sha256: None,
        verified,
        recovery: RecoveryContext::descriptor(&saved, !settled),
        updated_sha256: Some(digest(&content)),
        file,
        message: if verified {
            RECREATED_MESSAGE
        } else {
            "File restore outcome is unknown; recovery material has been retained and the operation will not be repeated"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::FilePatchAction;
    use desk_file_recovery::windows::{PrivateDirectory, move_no_replace};
    use std::process::Command;
    use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

    #[test]
    fn replacement_and_delete_without_parent_attribute_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("hello.py");
        std::fs::write(&path, b"before").unwrap();
        let sid = desk_file_recovery::windows::current_user_sid().unwrap();
        // No inheritance flags: only this temporary directory denies attribute
        // writes; its files remain writable, matching the Public directory case.
        let result = Command::new("icacls.exe")
            .arg(root.path())
            .arg("/deny")
            .arg(format!("*{sid}:(WA,WEA)"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            open_verified_with_access(
                root.path(),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                (FILE_SHARE_READ | FILE_SHARE_WRITE).0
            )
            .is_err()
        );
        let parent = open_mutation_parent(root.path()).unwrap();
        let parent_id = file_identity(&parent.handle, FileKind::Directory).unwrap();
        let original_id = file_identity(&File::open(&path).unwrap(), FileKind::File).unwrap();
        let source = SourceSnapshot::capture(&parent.handle, "hello.py", original_id).unwrap();
        let tx = PrivateDirectory::open_or_create(&parent.handle, "transaction").unwrap();
        let staged = source
            .prepare_replacement(&tx, b"after", |_| Ok(()))
            .unwrap();
        staged.verify_for_source(&source).unwrap();
        let tx_id = file_identity(tx.handle(), FileKind::Directory).unwrap();
        move_no_replace(
            source.handle(),
            source.identity(),
            tx.handle(),
            tx_id,
            "original",
        )
        .unwrap();
        tx.handle().sync_all().unwrap();
        move_no_replace(
            staged.handle(),
            staged.identity(),
            &parent.handle,
            parent_id,
            "hello.py",
        )
        .unwrap();
        source.finish_replacement_metadata(&staged).unwrap();
        parent.handle.sync_all().unwrap();
        assert_eq!(staged.content(), b"after");
        assert_eq!(source.content(), b"before");
        drop((source, staged, tx));
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        let id = file_identity(&File::open(&path).unwrap(), FileKind::File).unwrap();
        let source = SourceSnapshot::capture(&parent.handle, "hello.py", id).unwrap();
        let tx = PrivateDirectory::open_or_create(&parent.handle, "deletion").unwrap();
        move_no_replace(
            source.handle(),
            source.identity(),
            tx.handle(),
            file_identity(tx.handle(), FileKind::Directory).unwrap(),
            "original",
        )
        .unwrap();
        parent.handle.sync_all().unwrap();
        assert!(!path.exists());
        assert_eq!(source.content(), b"after");
    }

    fn context(data_root: &Path) -> RecoveryContext {
        RecoveryContext {
            data_root: data_root.to_owned(),
            execution_epoch: 0,
            quota: None,
            scope: desk_file_recovery::Scope {
                authority: "test".into(),
                device: "device".into(),
                os_user: crate::file_recovery_service::platform_user::current().unwrap(),
                owner: "owner".into(),
            },
            conversation_id: "conversation".into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            generation: "generation".into(),
            _test_data: None,
        }
    }

    /// Give `path` a protected DACL through the legacy API, which leaves out the
    /// auto-inherited flag, as directories under a user's profile often have.
    fn set_legacy_dacl(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows::Win32::Security::{
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW,
        };
        use windows_core::PCWSTR;
        let wide = |text: &std::ffi::OsStr| -> Vec<u16> {
            text.encode_wide().chain(std::iter::once(0)).collect()
        };
        let sddl = wide(std::ffi::OsStr::new(sddl));
        let path = wide(path.as_os_str());
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )
            .unwrap();
            let set =
                SetFileSecurityW(PCWSTR(path.as_ptr()), DACL_SECURITY_INFORMATION, descriptor);
            LocalFree(Some(HLOCAL(descriptor.0)));
            assert!(set.as_bool());
        }
    }

    #[test]
    fn update_restore_and_delete_under_a_legacy_dacl_are_verified() {
        use desk_agent_protocol::computer_use::TextFileChange;
        let _guard = file_store_test_lock();
        let data = tempfile::tempdir().unwrap();
        let outer = tempfile::tempdir().unwrap();
        let sid = desk_file_recovery::windows::current_user_sid().unwrap();
        // Administrators is passed on to the directory's files but not by the
        // private transaction directory, so renaming the original into it
        // turns that entry explicit (Windows 11 26H2).
        set_legacy_dacl(
            outer.path(),
            &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid})"),
        );
        let root = outer.path().join("work");
        std::fs::create_dir(&root).unwrap();
        let root = root.as_path();
        let path = root.join("notes.txt");
        std::fs::write(&path, b"line one\n").unwrap();
        let acl_before = Command::new("icacls.exe").arg(&path).output().unwrap();
        let directory = issue(root).unwrap();
        let target = issue(&path).unwrap();
        let update = FilePatchAction::UpdateText {
            directory: directory.clone(),
            expected_sha256: digest(b"line one\n"),
            change: TextFileChange::Write {
                content_utf8: "line one\nline two\n".into(),
            },
        };
        let (updated, _) = execute(&target, &update, &context(data.path()), || Ok(())).unwrap();
        updated.validate_for(&target, &update).unwrap();
        assert!(updated.verified, "{updated:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"line one\nline two\n");
        // No transaction directory is left behind and the file keeps its ACL.
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
        let acl_after = Command::new("icacls.exe").arg(&path).output().unwrap();
        assert_eq!(acl_after.stdout, acl_before.stdout);

        // Undo the update from its recovery record.
        let restore = FilePatchAction::RestoreText {
            directory: directory.clone(),
            file_name: "notes.txt".into(),
            recovery_id: updated.recovery.recovery_id.clone(),
            expected_current: desk_agent_protocol::computer_use::RestoreExpectation::Sha(digest(
                b"line one\nline two\n",
            )),
        };
        let (restored, _) =
            execute(&directory, &restore, &context(data.path()), || Ok(())).unwrap();
        restored.validate_for(&directory, &restore).unwrap();
        assert!(restored.verified, "{restored:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"line one\n");
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
        let acl_restored = Command::new("icacls.exe").arg(&path).output().unwrap();
        assert_eq!(acl_restored.stdout, acl_before.stdout);

        let target = issue(&path).unwrap();
        let delete = FilePatchAction::DeleteText {
            directory,
            expected_sha256: digest(b"line one\n"),
        };
        let (deleted, _) = execute(&target, &delete, &context(data.path()), || Ok(())).unwrap();
        deleted.validate_for(&target, &delete).unwrap();
        assert!(deleted.verified, "{deleted:?}");
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn restore_recreates_a_deleted_file_with_the_parent_acl_and_undoes_it() {
        use desk_agent_protocol::computer_use::RestoreExpectation;
        let _guard = file_store_test_lock();
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, b"kept").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let delete = FilePatchAction::DeleteText {
            directory: directory.clone(),
            expected_sha256: digest(b"kept"),
        };
        let (deleted, _) = execute(&target, &delete, &context(data.path()), || Ok(())).unwrap();
        deleted.validate_for(&target, &delete).unwrap();
        // The temporary directory passes on more entries than the private
        // transaction directory, so the rename makes some explicit; the
        // deletion must still be verified rather than left unknown.
        assert!(deleted.verified, "{deleted:?}");
        assert!(!path.exists());
        let restore = |recovery_id: &str, expected_current| {
            let action = FilePatchAction::RestoreText {
                directory: directory.clone(),
                file_name: "notes.txt".into(),
                recovery_id: recovery_id.into(),
                expected_current,
            };
            let (output, _) = execute(&directory, &action, &context(data.path()), || Ok(()))?;
            output.validate_for(&directory, &action).unwrap();
            Ok::<_, AgentError>(output)
        };
        let restored = restore(&deleted.recovery.recovery_id, RestoreExpectation::Absent).unwrap();
        assert!(restored.verified);
        assert_eq!(std::fs::read(&path).unwrap(), b"kept");
        // Only the restored file remains; the staging name is gone.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        let parent_acl = Command::new("icacls.exe")
            .arg(root.path())
            .output()
            .unwrap();
        let file_acl = Command::new("icacls.exe").arg(&path).output().unwrap();
        // Inherited entries are marked "(I)"; a recreated file has no explicit ones.
        let file_acl = String::from_utf8_lossy(&file_acl.stdout);
        assert!(parent_acl.status.success());
        assert!(
            file_acl
                .lines()
                .filter(|line| line.contains(':'))
                .all(|line| line.contains("(I)")),
            "{file_acl}"
        );
        let undone = restore(
            &restored.recovery.recovery_id,
            RestoreExpectation::Sha(digest(b"kept")),
        )
        .unwrap();
        assert!(undone.verified);
        assert!(!path.exists());
    }
}
