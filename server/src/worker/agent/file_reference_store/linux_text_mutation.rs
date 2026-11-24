//! Handle-relative Linux text changes with retained recovery material.
//!
//! Submission errors retain the transaction because the outcome may be unknown.
//! Successful submissions are verified before reporting a confirmed change.
//! Authorization and expected-version checks happen before the commit.
use super::text_recovery_context::{
    Planned, RECREATED_MESSAGE, RESTORE_DELETED_MESSAGE, RESTORED_MESSAGE, TextMutation,
    TextMutationReceipt, TextTarget, digest, directory_identity, recreated_mode,
};
pub(crate) use super::text_recovery_context::{RecoveryContext, execute};
use super::*;
use desk_file_recovery::{ChangeState, LockedVault, RecordKind, TransactionPlan};
use std::ffi::{CStr, CString};
use std::io::{Seek, SeekFrom};
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
    desk_file_recovery::linux::validate_source(file)
        .map_err(|e| io_error("validate Linux text source", e))?;
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

fn create_private_file(parent: &File, name: &CStr) -> Result<File, AgentError> {
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
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The native rename: `(from directory, from name, to directory, to name, flags)`.
type Submit<'a> = dyn FnMut(&File, &CStr, &File, &CStr, u32) -> std::io::Result<()> + 'a;

/// Commit-time hooks; tests use them to change the filesystem around a commit.
struct Hooks<'a> {
    before_commit: &'a dyn Fn(),
    after_commit: &'a dyn Fn(),
    commit_guard: &'a dyn Fn() -> Result<(), AgentError>,
}

fn renameat2(
    from: &File,
    from_name: &CStr,
    to: &File,
    to_name: &CStr,
    flags: u32,
) -> std::io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(super) fn mutate(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    commit_guard: &dyn Fn() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    mutate_with_submit(
        request,
        context,
        Hooks {
            before_commit: &|| {},
            after_commit: &|| {},
            commit_guard,
        },
        &mut renameat2,
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

fn mutate_with_submit(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    hooks: Hooks<'_>,
    submit: &mut Submit<'_>,
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
    if file_name.is_empty()
        || file_name.len() > 200
        || matches!(file_name.as_str(), "." | "..")
        || file_name.chars().any(|c| c.is_control() || c == '/')
    {
        return Err(conflict());
    }
    let leaf = CString::new(file_name.as_str()).map_err(|_| conflict())?;
    let file =
        match desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(&file_name)) {
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
            context, &mut vault, &location, file, &original, planned, hooks, submit,
        ),
        _ => recreate(context, &mut vault, &location, planned, hooks, submit),
    }
}

fn is_absent(directory: &File, file_name: &str) -> bool {
    matches!(
        desk_file_recovery::linux::open_beneath(directory, Path::new(file_name), false),
        Err(ref error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Exchange in new content, or move the file into the transaction for removal.
#[allow(
    clippy::too_many_arguments,
    reason = "the resolved location, the source and the native seams of one commit"
)]
fn replace(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    location: &Location,
    file: File,
    original: &[u8],
    planned: Planned,
    hooks: Hooks<'_>,
    submit: &mut Submit<'_>,
) -> Result<TextMutationReceipt, AgentError> {
    let Location {
        root,
        parent,
        file_name,
        leaf,
        directory_identity,
    } = location;
    let expected_sha256 = digest(original);
    let source_identity = unix_file_identity(&file).map_err(|_| conflict())?;
    let original_path = root.path.join(file_name).to_string_lossy().into_owned();
    let metadata = desk_file_recovery::linux::capture_metadata(&file, &original_path)
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
    let mut submission_attempted = false;
    let mut settled = None;
    let result = (|| {
        let recovery = create_transaction_directory(context, vault, &saved.id, &parent.handle)?;
        let directory_inode = recovery
            .metadata()
            .map_err(|e| io_error("identify file transaction", e))?
            .ino();
        let original_name = c"original";
        let stage_name = c"replacement";
        let mut staged = replacement
            .as_ref()
            .map(|_| create_private_file(&recovery, stage_name))
            .transpose()?;
        if let (Some(staged), Some(bytes)) = (staged.as_mut(), replacement.as_ref()) {
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
            // Register the inode before writing: a partial write/sync failure
            // must remain identifiable to the guarded transaction cleanup.
            staged
                .write_all(bytes)
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("write text recovery file", cause))?;
            desk_file_recovery::linux::copy_metadata(&file, staged)
                .map_err(|e| io_error("preserve Linux file metadata", e))?;
            staged
                .set_modified(std::time::SystemTime::now())
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("sync text metadata", cause))?;
        }
        (hooks.before_commit)();
        let unchanged = || -> Result<bool, AgentError> {
            let current =
                desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name))
                    .map_err(|_| conflict())?;
            Ok(
                unix_file_identity(&current).map_err(|_| conflict())? == source_identity
                    && digest(&complete_text(&current)?) == expected_sha256
                    && desk_file_recovery::linux::capture_metadata(&current, &original_path)
                        .map_err(|e| io_error("recheck Linux metadata", e))?
                        == metadata,
            )
        };
        if !unchanged()? || open_verified(&root.path)?.identity != parent.identity {
            return Err(conflict());
        }
        let (destination, flags) = if staged.is_some() {
            (stage_name, libc::RENAME_EXCHANGE)
        } else {
            (original_name, libc::RENAME_NOREPLACE)
        };
        recovery
            .sync_all()
            .and_then(|_| parent.handle.sync_all())
            .map_err(|e| io_error("persist Linux transaction directory", e))?;
        vault
            .transition(&context.scope, &saved.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))?;
        if !unchanged()? {
            return Err(conflict());
        }
        (hooks.commit_guard)()?;
        submission_attempted = true;
        if let Err(cause) = submit(
            &parent.handle,
            leaf.as_c_str(),
            &recovery,
            destination,
            flags,
        ) {
            // A storage error does not prove that neither directory was changed.
            // Keep the durable intent and all recovery identities for reconciliation.
            tracing::warn!(error_kind = ?cause.kind(), "text commit returned without a known outcome");
            return Ok(unknown_outcome(
                &saved,
                file_name,
                original.len(),
                &expected_sha256,
            ));
        }
        let displaced = desk_file_recovery::linux::open_text_beneath(
            &recovery,
            Path::new(destination.to_str().unwrap()),
        );
        let source_matches = displaced.as_ref().ok().is_some_and(|old| {
            old.metadata().is_ok_and(|stat| {
                stat.ino() == source_identity.secondary
                    && stat.dev() == source_identity.primary
                    && stat.nlink() == 1
            }) && complete_text(old).is_ok_and(|bytes| digest(&bytes) == expected_sha256)
        });
        let target_matches = match &replacement {
            Some(bytes) => {
                published_matches(&parent.handle, file_name, staged.as_ref().unwrap(), bytes)
            }
            None => is_absent(&parent.handle, file_name),
        };
        let verified = source_matches
            && target_matches
            && parent.handle.sync_all().is_ok()
            && recovery.sync_all().is_ok();
        if !verified {
            return Ok(unknown_outcome(
                &saved,
                file_name,
                original.len(),
                &expected_sha256,
            ));
        }
        settled = Some(context.finish(vault, &saved, ChangeState::Succeeded));
        // Registration failure cannot undo an already verified filesystem change.
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
            recovery: RecoveryContext::descriptor(&saved, false),
            updated_sha256: replacement.as_deref().map(digest),
            file: updated,
            message: match (planned.kind, staged.is_some()) {
                (RecordKind::TextRestore, true) => RESTORED_MESSAGE,
                (RecordKind::TextRestore, false) => RESTORE_DELETED_MESSAGE,
                (_, true) => {
                    "text update verified; prior version saved in device-private backup storage"
                }
                (_, false) => {
                    "selected file deleted; prior version saved in device-private backup storage"
                }
            },
        })
    })();
    finish_unsettled(
        context,
        vault,
        &saved,
        submission_attempted,
        settled,
        result,
    )
}

/// Create a file that is absent now, through the transaction directory,
/// without replacing anything at the final name.
fn recreate(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    location: &Location,
    planned: Planned,
    hooks: Hooks<'_>,
    submit: &mut Submit<'_>,
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
    let mut submission_attempted = false;
    let mut settled = None;
    let result = (|| {
        let recovery = create_transaction_directory(context, vault, &saved.id, &parent.handle)?;
        let directory_inode = recovery
            .metadata()
            .map_err(|e| io_error("identify file transaction", e))?
            .ino();
        let stage_name = c"replacement";
        let mut staged = create_private_file(&recovery, stage_name)?;
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
        staged
            .write_all(&content)
            .and_then(|_| staged.sync_all())
            .map_err(|cause| io_error("write text recovery file", cause))?;
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
        if !is_absent(&parent.handle, file_name)
            || open_verified(&root.path)?.identity != parent.identity
        {
            return Err(conflict());
        }
        recovery
            .sync_all()
            .and_then(|_| parent.handle.sync_all())
            .map_err(|e| io_error("persist Linux transaction directory", e))?;
        vault
            .transition(&context.scope, &saved.id, ChangeState::CommitIntent)
            .map_err(|e| io_error("save file commit intent", e))?;
        if !is_absent(&parent.handle, file_name) {
            return Err(conflict());
        }
        (hooks.commit_guard)()?;
        submission_attempted = true;
        if let Err(cause) = submit(
            &recovery,
            stage_name,
            &parent.handle,
            leaf.as_c_str(),
            libc::RENAME_NOREPLACE,
        ) {
            if cause.raw_os_error() == Some(libc::EEXIST) {
                // The name appeared after the last check; nothing was moved.
                submission_attempted = false;
                return Err(conflict());
            }
            tracing::warn!(error_kind = ?cause.kind(), "file restore returned without a known outcome");
            return Ok(unknown_outcome(&saved, file_name, 0, ""));
        }
        let verified = published_matches(&parent.handle, file_name, &staged, &content)
            && is_absent(&recovery, "replacement")
            && parent.handle.sync_all().is_ok()
            && recovery.sync_all().is_ok();
        if !verified {
            return Ok(unknown_outcome(&saved, file_name, 0, ""));
        }
        settled = Some(context.finish(vault, &saved, ChangeState::Succeeded));
        let updated = issue_updated(root, file_name, &staged, &content);
        (hooks.after_commit)();
        Ok(TextMutationReceipt {
            file_name: file_name.clone(),
            original_size_bytes: 0,
            original_sha256: None,
            verified: true,
            recovery: RecoveryContext::descriptor(&saved, false),
            updated_sha256: Some(digest(&content)),
            file: updated,
            message: RECREATED_MESSAGE,
        })
    })();
    finish_unsettled(
        context,
        vault,
        &saved,
        submission_attempted,
        settled,
        result,
    )
}

/// An attempted commit without a verified result keeps CommitIntent, which
/// restart recovery reports as an unknown outcome.
fn unknown_outcome(
    saved: &desk_file_recovery::Record,
    file_name: &str,
    original_size: usize,
    original_sha256: &str,
) -> TextMutationReceipt {
    TextMutationReceipt {
        file_name: file_name.into(),
        original_size_bytes: original_size as u64,
        original_sha256: (!original_sha256.is_empty()).then(|| original_sha256.into()),
        verified: false,
        recovery: RecoveryContext::descriptor(saved, true),
        updated_sha256: None,
        file: None,
        message: "text operation was submitted but could not be verified; recovery transaction retained; do not replay",
    }
}

/// Abort an operation that never reached submission, settle an unverified
/// one, and report whether the transaction remains.
fn finish_unsettled(
    context: &RecoveryContext,
    vault: &mut LockedVault,
    saved: &desk_file_recovery::Record,
    submission_attempted: bool,
    settled: Option<bool>,
    result: Result<TextMutationReceipt, AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    let settled = match settled {
        Some(settled) => settled,
        None if !submission_attempted => context.finish(vault, saved, ChangeState::Aborted),
        None => {
            context.settle(vault, saved);
            false
        }
    };
    result.map(|mut receipt| {
        receipt.recovery.cleanup_pending = !settled;
        receipt
    })
}

/// The published name holds the staged inode with exactly `bytes`.
fn published_matches(parent: &File, file_name: &str, staged: &File, bytes: &[u8]) -> bool {
    desk_file_recovery::linux::open_text_beneath(parent, Path::new(file_name)).is_ok_and(
        |published| {
            published.metadata().is_ok_and(|stat| {
                staged
                    .metadata()
                    .is_ok_and(|stage| stat.ino() == stage.ino() && stat.dev() == stage.dev())
            }) && complete_text(&published).is_ok_and(|current| current == bytes)
        },
    )
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
    let c_name = CString::new(name.as_str()).map_err(|_| conflict())?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
        return Err(io_error(
            "create file transaction directory",
            std::io::Error::last_os_error(),
        ));
    }
    let recovery = desk_file_recovery::linux::open_beneath(parent, Path::new(&name), true)
        .map_err(|e| io_error("open file transaction directory", e))?;
    desk_file_recovery::linux::make_private(&recovery)
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

/// A durable reference to the verified file, from the staged handle.
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

/// Test adapters with explicit commit hooks and a replaceable native rename.
#[cfg(test)]
mod test_support {
    use super::super::text_recovery_context::TextIntent;
    use super::*;
    use desk_agent_protocol::computer_use::{TextEdit, TextFileChange};
    use std::cell::Cell;

    pub(super) enum TextChange<'a> {
        Write(&'a str),
        Replace { old: &'a str, new: &'a str },
        Delete,
    }

    pub(super) struct TextMutationRequest<'a> {
        pub directory: &'a ObjectRef,
        pub target: &'a ObjectRef,
        pub expected_sha256: &'a str,
        pub change: TextChange<'a>,
    }

    pub(super) struct TestReceipt {
        pub verified: bool,
        pub recovery: desk_agent_protocol::computer_use::FileRecoveryDescriptor,
        pub file: Option<CreatedTextArtifact>,
        pub message: &'static str,
        /// The content saved by this operation's backup.
        pub backup: Option<Vec<u8>>,
    }

    pub(super) fn test_context(_directory: &ObjectRef) -> RecoveryContext {
        super::super::text_recovery_context::test_context()
    }

    pub(super) fn mutate_text(
        directory: &ObjectRef,
        target: &ObjectRef,
        expected_sha256: &str,
        change: TextChange<'_>,
    ) -> Result<TestReceipt, AgentError> {
        mutate_text_managed(
            TextMutationRequest {
                directory,
                target,
                expected_sha256,
                change,
            },
            || {},
            || {},
            || Ok(()),
            &test_context(directory),
        )
    }

    pub(super) fn mutate_text_managed(
        request: TextMutationRequest<'_>,
        before_commit: impl FnOnce(),
        after_commit: impl FnOnce(),
        commit_guard: impl FnOnce() -> Result<(), AgentError>,
        context: &RecoveryContext,
    ) -> Result<TestReceipt, AgentError> {
        mutate_text_with_submit(
            request,
            before_commit,
            after_commit,
            commit_guard,
            context,
            renameat2,
        )
    }

    pub(super) fn mutate_text_with_submit(
        request: TextMutationRequest<'_>,
        before_commit: impl FnOnce(),
        after_commit: impl FnOnce(),
        commit_guard: impl FnOnce() -> Result<(), AgentError>,
        context: &RecoveryContext,
        submit: impl FnOnce(&File, &CStr, &File, &CStr, u32) -> std::io::Result<()>,
    ) -> Result<TestReceipt, AgentError> {
        let change = match request.change {
            TextChange::Write(text) => Some(TextFileChange::Write {
                content_utf8: text.into(),
            }),
            TextChange::Replace { old, new } => Some(TextFileChange::Edit {
                edits: vec![TextEdit {
                    old_text: old.into(),
                    new_text: new.into(),
                    replace_all: false,
                }],
            }),
            TextChange::Delete => None,
        };
        let before_commit = Cell::new(Some(before_commit));
        let after_commit = Cell::new(Some(after_commit));
        let commit_guard = Cell::new(Some(commit_guard));
        let mut submit = Some(submit);
        let receipt = mutate_with_submit(
            TextMutation {
                directory: request.directory,
                target: TextTarget::Reference(request.target),
                expected_sha256: Some(request.expected_sha256),
                intent: change.as_ref().map_or(TextIntent::Delete, TextIntent::Edit),
            },
            context,
            Hooks {
                before_commit: &|| {
                    if let Some(hook) = before_commit.take() {
                        hook()
                    }
                },
                after_commit: &|| {
                    if let Some(hook) = after_commit.take() {
                        hook()
                    }
                },
                commit_guard: &|| commit_guard.take().map_or(Ok(()), |guard| guard()),
            },
            &mut |from, from_name, to, to_name, flags| {
                (submit.take().expect("one native submission"))(from, from_name, to, to_name, flags)
            },
        )?;
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let backup = vault
            .lock()
            .unwrap()
            .entry_material(
                &context.scope,
                &context.conversation_id,
                &receipt.recovery.recovery_id,
                0,
                jiff::Timestamp::now().as_millisecond() as u64,
            )
            .ok()
            .and_then(|material| material.before);
        Ok(TestReceipt {
            verified: receipt.verified,
            recovery: receipt.recovery,
            file: receipt.file,
            message: receipt.message,
            backup,
        })
    }
}
#[cfg(test)]
use test_support::*;

#[cfg(test)]
#[path = "linux_text_mutation/tests.rs"]
mod tests;
