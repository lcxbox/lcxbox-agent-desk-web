//! Handle-relative macOS text changes with retained recovery material.
//!
//! Successful filesystem calls determine the mutation result. References are
//! issued from the staged handle, without rereading either side of the commit.
//! Authorization and expected-version checks happen before the commit.
use super::text_recovery_context::{
    Planned, RECREATED_MESSAGE, RESTORE_DELETED_MESSAGE, RESTORED_MESSAGE, TextMutation,
    TextMutationReceipt, TextTarget, digest, directory_identity, recreated_mode,
};
pub(crate) use super::text_recovery_context::{RecoveryContext, execute};
use super::*;
use desk_file_recovery::{ChangeState, LockedVault, RecordKind, TransactionPlan};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

fn conflict() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "text file identity or complete content version changed; read again and request new approval",
        false,
    )
}

fn complete_text(file: &File) -> Result<Vec<u8>, AgentError> {
    let metadata = file.metadata().map_err(|_| conflict())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_TEXT_FILE_BYTES {
        return Err(conflict());
    }
    let before = unix_file_identity(file).map_err(|_| conflict())?;
    let mut handle = file.try_clone().map_err(|_| conflict())?;
    handle.seek(SeekFrom::Start(0)).map_err(|_| conflict())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    handle
        .take(MAX_TEXT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| conflict())?;
    if bytes.len() as u64 > MAX_TEXT_FILE_BYTES
        || bytes.contains(&0)
        || std::str::from_utf8(&bytes).is_err()
        || unix_file_identity(file).map_err(|_| conflict())? != before
    {
        return Err(conflict());
    }
    Ok(bytes)
}

fn create_private_file(parent: &File, name: &CStr, bytes: &[u8]) -> Result<File, AgentError> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io_error(
            "create private text recovery file",
            std::io::Error::last_os_error(),
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|cause| io_error("write text recovery file", cause))?;
    Ok(file)
}

/// Commit-time hooks; tests use them to change the filesystem around a commit.
struct Hooks<'a> {
    before_commit: &'a dyn Fn(),
    after_commit: &'a dyn Fn(),
    commit_guard: &'a dyn Fn() -> Result<(), AgentError>,
}

pub(super) fn mutate(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    commit_guard: &dyn Fn() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    mutate_with_hooks(
        request,
        context,
        Hooks {
            before_commit: &|| {},
            after_commit: &|| {},
            commit_guard,
        },
    )
}

/// The resolved directory and the file name under it.
struct Location {
    root: StoredFile,
    parent: OpenedFile,
    file_name: String,
    leaf: CString,
    directory_identity: String,
}

fn mutate_with_hooks(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    hooks: Hooks<'_>,
) -> Result<TextMutationReceipt, AgentError> {
    if request.directory.object_kind != ObjectKind::Directory {
        return Err(conflict());
    }
    let root = resolve(request.directory)?;
    let parent = open_verified(&root.path)?;
    if !parent.metadata.is_dir() || parent.identity != root.identity {
        return Err(conflict());
    }
    let (file_name, source_identity) = match request.target {
        TextTarget::Reference(target) => {
            if target.object_kind != ObjectKind::File {
                return Err(conflict());
            }
            let source = resolve(target)?;
            if open_verified(source.path.parent().ok_or_else(conflict)?)?.identity
                != parent.identity
            {
                return Err(conflict());
            }
            let name = source
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(conflict)?
                .to_owned();
            (name, Some(source.identity))
        }
        TextTarget::Name(name) => (name.to_owned(), None),
    };
    validate_macos_leaf(&file_name)?;
    let leaf = CString::new(file_name.as_str()).map_err(|_| conflict())?;
    let file = match open_relative_unix(&parent.handle, &leaf) {
        Ok(file) => Some(file),
        Err(cause)
            if cause.kind() == std::io::ErrorKind::NotFound
                && request.expected_sha256.is_none() =>
        {
            None
        }
        Err(_) => return Err(conflict()),
    };
    if let (Some(file), Some(identity)) = (&file, &source_identity)
        && unix_file_identity(file).map_err(|_| conflict())? != *identity
    {
        return Err(conflict());
    }
    let original = file.as_ref().map(complete_text).transpose()?;
    if original.as_deref().map(digest).as_deref() != request.expected_sha256 {
        return Err(conflict());
    }
    let mut vault = context.open_vault()?;
    let location = Location {
        directory_identity: directory_identity(&parent.identity),
        root,
        parent,
        file_name,
        leaf,
    };
    let planned = context.plan(
        &vault,
        &request.intent,
        original.as_deref(),
        &location.file_name,
        &location.directory_identity,
    )?;
    match (file, original) {
        (Some(file), Some(original)) => replace(
            context, &mut vault, &location, file, &original, planned, hooks,
        ),
        _ => recreate(context, &mut vault, &location, planned, hooks),
    }
}

/// Swap in new content, or move the file into the transaction for removal.
fn replace(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    location: &Location,
    file: File,
    original: &[u8],
    planned: Planned,
    hooks: Hooks<'_>,
) -> Result<TextMutationReceipt, AgentError> {
    let Location {
        root,
        parent,
        file_name,
        leaf,
        directory_identity,
    } = location;
    let expected_sha256 = digest(original);
    let file_identity = unix_file_identity(&file).map_err(|_| conflict())?;
    let metadata = desk_file_recovery::macos::capture_metadata(
        &file,
        &root.path.join(file_name).to_string_lossy(),
    )
    .map_err(|e| io_error("save original file metadata", e))?;
    let transaction = TransactionPlan::unix(
        &root.path.to_string_lossy(),
        parent.metadata.dev(),
        parent.metadata.ino(),
        file.metadata()
            .map_err(|e| io_error("identify original file", e))?
            .ino(),
    )
    .map_err(|e| io_error("plan file transaction", e))?;
    let replacement = planned.content;
    let saved = context.backup(
        vault,
        planned.kind,
        directory_identity,
        file_name,
        Some(original),
        replacement.as_deref(),
        Some(&metadata),
        Some(transaction),
    )?;
    let mut committed = false;
    let result = (|| {
        let recovery = create_transaction_directory(context, vault, &saved.id, &parent.handle)?;
        let directory_inode = recovery
            .metadata()
            .map_err(|e| io_error("identify file transaction", e))?
            .ino();
        let original_name = c"original";
        let stage_name = c"replacement";
        let staged = replacement
            .as_ref()
            .map(|bytes| create_private_file(&recovery, stage_name, bytes))
            .transpose()?;
        if let Some(staged) = &staged {
            vault
                .register_transaction(
                    &context.scope,
                    &saved.id,
                    directory_inode,
                    Some(
                        staged
                            .metadata()
                            .map_err(|e| io_error("identify staged file", e))?
                            .ino(),
                    ),
                )
                .map_err(|e| io_error("register staged file identity", e))?;
            // Preserve permissions, ACLs and xattrs through open descriptors, not a
            // second pathname copy. In particular, do not drop quarantine metadata.
            if unsafe {
                libc::fcopyfile(
                    file.as_raw_fd(),
                    staged.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } != 0
            {
                return Err(io_error(
                    "preserve text file metadata",
                    std::io::Error::last_os_error(),
                ));
            }
            staged
                .set_modified(std::time::SystemTime::now())
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("sync text metadata", cause))?;
        }
        (hooks.before_commit)();
        let current = open_relative_unix(&parent.handle, leaf).map_err(|_| conflict())?;
        if unix_file_identity(&current).map_err(|_| conflict())? != file_identity
            || digest(&complete_text(&current)?) != expected_sha256
            || open_verified(&root.path)?.identity != parent.identity
        {
            return Err(conflict());
        }
        let (destination, flags) = if staged.is_some() {
            (stage_name, libc::RENAME_SWAP)
        } else {
            (original_name, libc::RENAME_EXCL)
        };
        (hooks.commit_guard)()?;
        vault
            .transition(&context.scope, &saved.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))?;
        if unsafe {
            libc::renameatx_np(
                parent.handle.as_raw_fd(),
                leaf.as_ptr(),
                recovery.as_raw_fd(),
                destination.as_ptr(),
                flags,
            )
        } != 0
        {
            return Err(io_error(
                "commit recoverable text change",
                std::io::Error::last_os_error(),
            ));
        }
        committed = true;
        let settled = context.finish(vault, &saved, ChangeState::Succeeded);
        // Reference registration and maintenance cannot undo a successful syscall.
        let updated = staged.as_ref().and_then(|staged| {
            issue_updated(root, file_name, staged, replacement.as_deref().unwrap())
        });
        (hooks.after_commit)();
        for directory in [&parent.handle, &recovery] {
            if let Err(cause) = directory.sync_all() {
                tracing::warn!(error_kind = ?cause.kind(), "text change succeeded but directory sync failed");
            }
        }
        Ok(TextMutationReceipt {
            file_name: file_name.clone(),
            original_size_bytes: original.len() as u64,
            original_sha256: Some(expected_sha256.clone()),
            verified: true,
            recovery: RecoveryContext::descriptor(&saved, !settled),
            updated_sha256: replacement.as_deref().map(digest),
            file: updated,
            message: match (planned.kind, staged.is_some()) {
                (RecordKind::TextRestore, true) => RESTORED_MESSAGE,
                (RecordKind::TextRestore, false) => RESTORE_DELETED_MESSAGE,
                (_, true) => {
                    "text update succeeded; prior version saved in device-private backup storage; no post-write readback was performed"
                }
                (_, false) => {
                    "selected file deleted; prior version saved in device-private backup storage"
                }
            },
        })
    })();
    if !committed {
        context.finish(vault, &saved, ChangeState::Aborted);
    }
    result
}

/// Create a file that is absent now, through the transaction directory, with
/// create-new semantics at the final name.
fn recreate(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    location: &Location,
    planned: Planned,
    hooks: Hooks<'_>,
) -> Result<TextMutationReceipt, AgentError> {
    let Location {
        root,
        parent,
        file_name,
        leaf,
        directory_identity,
    } = location;
    let content = planned.content.ok_or_else(conflict)?;
    // No original file: no inode can match the original slot of the transaction.
    let transaction = TransactionPlan::unix(
        &root.path.to_string_lossy(),
        parent.metadata.dev(),
        parent.metadata.ino(),
        0,
    )
    .map_err(|e| io_error("plan file transaction", e))?;
    let saved = context.backup(
        vault,
        planned.kind,
        directory_identity,
        file_name,
        None,
        Some(&content),
        None,
        Some(transaction),
    )?;
    let mut committed = false;
    let result = (|| {
        let recovery = create_transaction_directory(context, vault, &saved.id, &parent.handle)?;
        let directory_inode = recovery
            .metadata()
            .map_err(|e| io_error("identify file transaction", e))?
            .ino();
        let stage_name = c"replacement";
        let staged = create_private_file(&recovery, stage_name, &content)?;
        vault
            .register_transaction(
                &context.scope,
                &saved.id,
                directory_inode,
                Some(
                    staged
                        .metadata()
                        .map_err(|e| io_error("identify staged file", e))?
                        .ino(),
                ),
            )
            .map_err(|e| io_error("register staged file identity", e))?;
        if unsafe {
            libc::fchmod(
                staged.as_raw_fd(),
                recreated_mode(planned.metadata.as_deref()) as libc::mode_t,
            )
        } != 0
        {
            return Err(io_error(
                "set restored file permissions",
                std::io::Error::last_os_error(),
            ));
        }
        staged
            .sync_all()
            .map_err(|cause| io_error("sync restored file", cause))?;
        (hooks.before_commit)();
        if !matches!(open_relative_unix(&parent.handle, leaf), Err(ref cause) if cause.kind() == std::io::ErrorKind::NotFound)
            || open_verified(&root.path)?.identity != parent.identity
        {
            return Err(conflict());
        }
        (hooks.commit_guard)()?;
        vault
            .transition(&context.scope, &saved.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))?;
        if unsafe {
            libc::renameatx_np(
                recovery.as_raw_fd(),
                stage_name.as_ptr(),
                parent.handle.as_raw_fd(),
                leaf.as_ptr(),
                libc::RENAME_EXCL,
            )
        } != 0
        {
            let cause = std::io::Error::last_os_error();
            return Err(if cause.kind() == std::io::ErrorKind::AlreadyExists {
                conflict()
            } else {
                io_error("commit restored file", cause)
            });
        }
        committed = true;
        let settled = context.finish(vault, &saved, ChangeState::Succeeded);
        let updated = issue_updated(root, file_name, &staged, &content);
        (hooks.after_commit)();
        for directory in [&parent.handle, &recovery] {
            if let Err(cause) = directory.sync_all() {
                tracing::warn!(error_kind = ?cause.kind(), "file restore succeeded but directory sync failed");
            }
        }
        Ok(TextMutationReceipt {
            file_name: file_name.clone(),
            original_size_bytes: 0,
            original_sha256: None,
            verified: true,
            recovery: RecoveryContext::descriptor(&saved, !settled),
            updated_sha256: Some(digest(&content)),
            file: updated,
            message: RECREATED_MESSAGE,
        })
    })();
    if !committed {
        context.finish(vault, &saved, ChangeState::Aborted);
    }
    result
}

/// Create and register the record's private transaction directory.
fn create_transaction_directory(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    id: &str,
    parent: &File,
) -> Result<File, AgentError> {
    let name = vault
        .transaction_directory(&context.scope, id)
        .map_err(|e| io_error("register file transaction", e))?;
    let name = CString::new(name).map_err(|_| conflict())?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io_error(
            "create file transaction directory",
            std::io::Error::last_os_error(),
        ));
    }
    let recovery = open_relative_unix(parent, &name)
        .map_err(|e| io_error("open file transaction directory", e))?;
    desk_file_recovery::macos::make_private(&recovery)
        .map_err(|e| io_error("protect file transaction directory", e))?;
    let directory_inode = recovery
        .metadata()
        .map_err(|e| io_error("identify file transaction", e))?
        .ino();
    vault
        .register_transaction(&context.scope, id, directory_inode, None)
        .map_err(|e| io_error("register file transaction identity", e))?;
    Ok(recovery)
}

/// A durable reference to the committed file, from the staged handle.
/// Metadata from the already-open handle identifies a future read; it is not
/// compared with the target to verify this mutation.
fn issue_updated(
    root: &StoredFile,
    file_name: &str,
    staged: &File,
    bytes: &[u8],
) -> Option<CreatedTextArtifact> {
    let reference = (|| {
        let handle = staged
            .try_clone()
            .map_err(|cause| io_error("clone updated file handle", cause))?;
        let metadata = handle
            .metadata()
            .map_err(|cause| io_error("identify updated file", cause))?;
        let identity = unix_file_identity(&handle)
            .map_err(|cause| io_error("identify updated file", cause))?;
        issue_opened_with_lifetime(
            &root.path.join(file_name),
            OpenedFile {
                handle,
                metadata,
                identity,
            },
            DURABLE_ARTIFACT_REF_TTL_SECS,
            true,
        )
    })();
    match reference {
        Ok(reference) => Some(CreatedTextArtifact {
            file: reference,
            file_name: file_name.into(),
            byte_len: bytes.len() as u64,
            sha256: digest(bytes),
        }),
        Err(cause) => {
            tracing::warn!(error_kind = ?cause.kind, "text change succeeded but reference registration failed");
            None
        }
    }
}

#[cfg(test)]
mod tests;
