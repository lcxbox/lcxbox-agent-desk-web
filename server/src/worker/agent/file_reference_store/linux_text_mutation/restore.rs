//! In-place restore of AI updates and deletions through the Linux commit path.
use super::super::super::text_recovery_context::{TextIntent, backed_up};
use super::*;
use desk_agent_protocol::computer_use::{
    FilePatchAction, RestoreExpectation, TextFileChange, TextFileMutationOutput,
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// A later operation of the same conversation and vault.
fn next_operation(context: &RecoveryContext) -> RecoveryContext {
    RecoveryContext {
        data_root: context.data_root.clone(),
        execution_epoch: context.execution_epoch,
        quota: None,
        scope: context.scope.clone(),
        conversation_id: context.conversation_id.clone(),
        operation_id: uuid::Uuid::new_v4().to_string(),
        generation: context.generation.clone(),
        _test_data: context._test_data.clone(),
    }
}

fn restore(
    context: &RecoveryContext,
    directory: &ObjectRef,
    recovery_id: &str,
    expected_current: RestoreExpectation,
) -> Result<TextFileMutationOutput, AgentError> {
    let action = FilePatchAction::RestoreText {
        directory: directory.clone(),
        file_name: "notes.txt".into(),
        recovery_id: recovery_id.into(),
        expected_current,
    };
    let (output, _) = execute(directory, &action, context, || Ok(()))?;
    output.validate_for(directory, &action).unwrap();
    Ok(output)
}

fn run(
    context: &RecoveryContext,
    target: &ObjectRef,
    action: FilePatchAction,
) -> TextFileMutationOutput {
    let (output, _) = execute(target, &action, context, || Ok(())).unwrap();
    output.validate_for(target, &action).unwrap();
    output
}

#[test]
fn restore_puts_back_an_update_and_is_itself_undoable() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "original").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let updated = run(
        &context,
        &target,
        FilePatchAction::UpdateText {
            directory: directory.clone(),
            expected_sha256: digest(b"original"),
            change: TextFileChange::Write {
                content_utf8: "by ai".into(),
            },
        },
    );
    // The current content must be the AI version.
    std::fs::write(&path, "edited by user").unwrap();
    let conflict = restore(
        &next_operation(&context),
        &directory,
        &updated.recovery.recovery_id,
        RestoreExpectation::Sha(updated.updated_sha256.clone().unwrap()),
    );
    assert!(conflict.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"edited by user");
    // A newer version the owner chose to overwrite is checked by its own SHA.
    let restore_context = next_operation(&context);
    let restored = restore(
        &restore_context,
        &directory,
        &updated.recovery.recovery_id,
        RestoreExpectation::Sha(digest(b"edited by user")),
    )
    .unwrap();
    assert!(restored.verified);
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(restored.original_sha256, Some(digest(b"edited by user")));
    assert_eq!(restored.updated_sha256, Some(digest(b"original")));
    assert_eq!(
        backed_up(&restore_context, &restored.recovery.recovery_id).unwrap(),
        b"edited by user"
    );
    // Undo the restore.
    let undone = restore(
        &next_operation(&context),
        &directory,
        &restored.recovery.recovery_id,
        RestoreExpectation::Sha(digest(b"original")),
    )
    .unwrap();
    assert!(undone.verified);
    assert_eq!(std::fs::read(&path).unwrap(), b"edited by user");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn restore_recreates_a_deleted_file_with_its_permission_bits() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "kept text").unwrap();
    // Linux never mutates a file with special mode bits, so none reaches a
    // recreating restore.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4640)).unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let refused = execute(
        &target,
        &FilePatchAction::DeleteText {
            directory: directory.clone(),
            expected_sha256: digest(b"kept text"),
        },
        &context,
        || Ok(()),
    );
    assert!(refused.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"kept text");
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0o4640);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let target = issue(&path).unwrap();
    let context = next_operation(&context);
    let deleted = run(
        &context,
        &target,
        FilePatchAction::DeleteText {
            directory: directory.clone(),
            expected_sha256: digest(b"kept text"),
        },
    );
    assert!(!path.exists());
    // Expected present but absent: nothing is written.
    let sha_of_absent = restore(
        &next_operation(&context),
        &directory,
        &deleted.recovery.recovery_id,
        RestoreExpectation::Sha(digest(b"kept text")),
    );
    assert!(sha_of_absent.is_err());
    assert!(!path.exists());
    let restore_context = next_operation(&context);
    let restored = restore(
        &restore_context,
        &directory,
        &deleted.recovery.recovery_id,
        RestoreExpectation::Absent,
    )
    .unwrap();
    assert!(restored.verified);
    assert_eq!(restored.original_sha256, None);
    assert_eq!(restored.updated_sha256, Some(digest(b"kept text")));
    assert_eq!(std::fs::read(&path).unwrap(), b"kept text");
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o640);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    // The recreating restore backed up "nothing"; undoing it deletes the file.
    assert!(backed_up(&restore_context, &restored.recovery.recovery_id).is_none());
    let undone = restore(
        &next_operation(&context),
        &directory,
        &restored.recovery.recovery_id,
        RestoreExpectation::Sha(digest(b"kept text")),
    )
    .unwrap();
    assert!(undone.verified);
    assert!(undone.updated_sha256.is_none());
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn restore_of_a_deletion_never_replaces_a_new_file_with_that_name() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "deleted").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let deleted = run(
        &context,
        &target,
        FilePatchAction::DeleteText {
            directory: directory.clone(),
            expected_sha256: digest(b"deleted"),
        },
    );
    std::fs::write(&path, "newer file").unwrap();
    assert!(
        restore(
            &next_operation(&context),
            &directory,
            &deleted.recovery.recovery_id,
            RestoreExpectation::Absent,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"newer file");
    // A file appearing between the checks and the commit is not replaced.
    std::fs::remove_file(&path).unwrap();
    let raced = mutate_with_submit(
        TextMutation {
            directory: &directory,
            target: TextTarget::Name("notes.txt"),
            expected_sha256: None,
            intent: TextIntent::Restore {
                recovery_id: &deleted.recovery.recovery_id,
            },
        },
        &next_operation(&context),
        Hooks {
            before_commit: &|| std::fs::write(&path, "raced").unwrap(),
            after_commit: &|| panic!("a raced restore must not commit"),
            commit_guard: &|| Ok(()),
        },
        &mut renameat2,
    );
    assert!(raced.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"raced");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
