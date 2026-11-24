use super::super::text_recovery_context::{TextIntent, backed_up, test_context};
use super::*;
use desk_agent_protocol::computer_use::{
    FilePatchAction, RestoreExpectation, TextEdit, TextFileChange, TextFileMutationOutput,
};
use std::os::unix::fs::PermissionsExt;

fn write(text: &str) -> TextFileChange {
    TextFileChange::Write {
        content_utf8: text.into(),
    }
}

fn replace(old: &str, new: &str) -> TextFileChange {
    TextFileChange::Edit {
        edits: vec![TextEdit {
            old_text: old.into(),
            new_text: new.into(),
            replace_all: false,
        }],
    }
}

/// An update with `change`, or a deletion without one.
fn request<'a>(
    directory: &'a ObjectRef,
    target: &'a ObjectRef,
    expected_sha256: &'a str,
    change: Option<&'a TextFileChange>,
) -> TextMutation<'a> {
    TextMutation {
        directory,
        target: TextTarget::Reference(target),
        expected_sha256: Some(expected_sha256),
        intent: change.map_or(TextIntent::Delete, TextIntent::Edit),
    }
}

fn mutate_text_with_hooks(
    request: TextMutation<'_>,
    context: &RecoveryContext,
    before_commit: &dyn Fn(),
    after_commit: &dyn Fn(),
) -> Result<TextMutationReceipt, AgentError> {
    mutate_with_hooks(
        request,
        context,
        Hooks {
            before_commit,
            after_commit,
            commit_guard: &|| Ok(()),
        },
    )
}

/// Run a change in a fresh vault; returns the receipt and its backed-up content.
fn mutate_text(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: Option<&TextFileChange>,
) -> Result<(TextMutationReceipt, Vec<u8>), AgentError> {
    let context = test_context();
    let receipt = mutate(
        request(directory, target, expected_sha256, change),
        &context,
        &|| Ok(()),
    )?;
    let backup = backed_up(&context, &receipt.recovery.recovery_id).unwrap();
    Ok((receipt, backup))
}

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

fn restore_action(
    directory: &ObjectRef,
    recovery_id: &str,
    expected_current: RestoreExpectation,
) -> FilePatchAction {
    FilePatchAction::RestoreText {
        directory: directory.clone(),
        file_name: "notes.txt".into(),
        recovery_id: recovery_id.into(),
        expected_current,
    }
}

fn restore(
    context: &RecoveryContext,
    directory: &ObjectRef,
    recovery_id: &str,
    expected_current: RestoreExpectation,
) -> Result<TextFileMutationOutput, AgentError> {
    let action = restore_action(directory, recovery_id, expected_current);
    let (output, _) = execute(directory, &action, context, || Ok(()))?;
    output.validate_for(directory, &action).unwrap();
    Ok(output)
}

fn update(
    context: &RecoveryContext,
    directory: &ObjectRef,
    target: &ObjectRef,
    old: &[u8],
    change: TextFileChange,
) -> TextFileMutationOutput {
    let action = FilePatchAction::UpdateText {
        directory: directory.clone(),
        expected_sha256: digest(old),
        change,
    };
    let (output, _) = execute(target, &action, context, || Ok(())).unwrap();
    output.validate_for(target, &action).unwrap();
    output
}

fn delete(
    context: &RecoveryContext,
    directory: &ObjectRef,
    target: &ObjectRef,
    old: &[u8],
) -> TextFileMutationOutput {
    let action = FilePatchAction::DeleteText {
        directory: directory.clone(),
        expected_sha256: digest(old),
    };
    let (output, _) = execute(target, &action, context, || Ok(())).unwrap();
    output.validate_for(target, &action).unwrap();
    output
}

#[test]
fn mismatched_recovery_epoch_stops_before_backup_or_file_commit() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "old").unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    let mut context = test_context();
    context.execution_epoch = 1;
    let change = write("new");
    let result = mutate_text_with_hooks(
        request(&directory, &file, &digest(b"old"), Some(&change)),
        &context,
        &|| panic!("must not reach commit"),
        &|| {},
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    assert!(vault.lock().unwrap().list(&context.scope, None).is_empty());
}

#[test]
fn backup_storage_failure_blocks_update_and_delete() {
    let _guard = file_store_test_lock();
    for deleting in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let file = issue(&path).unwrap();
        let context = test_context();
        drop(desk_file_recovery::Vault::open(&context.data_root).unwrap());
        // Objects cannot be published once their directory is not writable.
        let objects = context.data_root.join("file-recovery").join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o500)).unwrap();
        let change = write("new");
        let result = mutate_text_with_hooks(
            request(
                &directory,
                &file,
                &digest(b"old"),
                (!deleting).then_some(&change),
            ),
            &context,
            &|| panic!("backup failure must stop before commit preparation"),
            &|| panic!("backup failure must not commit"),
        );
        std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.err().unwrap();
        assert!(
            error.message.contains("save private file backup"),
            "{}",
            error.message
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let records = vault
            .lock()
            .unwrap()
            .list(&context.scope, Some(&context.conversation_id));
        assert!(
            records
                .iter()
                .all(|record| record.change == desk_file_recovery::ChangeState::Aborted)
        );
    }
}

#[test]
fn commit_intent_write_failure_keeps_original_for_update_and_delete() {
    let _guard = file_store_test_lock();
    for deleting in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let file = issue(&path).unwrap();
        let context = test_context();
        let index = context.data_root.join("file-recovery/index.json");
        let held = context.data_root.join("held-index.json");
        let change = write("new");
        let result = mutate_text_with_hooks(
            request(
                &directory,
                &file,
                &digest(b"old"),
                (!deleting).then_some(&change),
            ),
            &context,
            &|| {
                std::fs::rename(&index, &held).unwrap();
                std::fs::create_dir(&index).unwrap();
            },
            &|| panic!("commit intent failure must not commit"),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        std::fs::remove_dir(&index).unwrap();
        std::fs::rename(&held, &index).unwrap();
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let mut locked = vault.lock().unwrap();
        let records = locked.list(&context.scope, Some(&context.conversation_id));
        assert_eq!(records.len(), 1);
        assert_eq!(
            locked
                .entry_material(
                    &context.scope,
                    &context.conversation_id,
                    &records[0].id,
                    0,
                    records[0].created_at_ms
                )
                .unwrap()
                .before
                .unwrap(),
            b"old"
        );
        locked
            .recover_interrupted(jiff::Timestamp::now().as_millisecond() as u64)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
    }
}

#[test]
fn update_and_recoverable_delete_preserve_original_bytes() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "你好 old").unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    let change = replace("old", "new");
    let (updated, backup) = mutate_text(
        &directory,
        &file,
        &digest("你好 old".as_bytes()),
        Some(&change),
    )
    .unwrap();
    assert!(updated.verified);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "你好 new");
    assert_eq!(backup, "你好 old".as_bytes());
    assert_eq!(updated.updated_sha256, Some(digest("你好 new".as_bytes())));
    let created = updated.file.unwrap();
    let (deleted, backup) = mutate_text(&directory, &created.file, &created.sha256, None).unwrap();
    assert!(deleted.verified);
    assert!(deleted.updated_sha256.is_none());
    assert!(!path.exists());
    assert_eq!(backup, "你好 new".as_bytes());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn multiple_edits_apply_all_or_none() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    let original = digest(b"alpha\r\nbeta\r\ngamma\r\n");
    let edit = |old: &str, new: &str| TextEdit {
        old_text: old.into(),
        new_text: new.into(),
        replace_all: false,
    };
    let failing = TextFileChange::Edit {
        edits: vec![edit("alpha", "ALPHA"), edit("missing", "x")],
    };
    let error = mutate_text(&directory, &file, &original, Some(&failing))
        .err()
        .unwrap();
    assert!(error.message.contains("edit 2 of 2"), "{}", error.message);
    assert_eq!(std::fs::read(&path).unwrap(), b"alpha\r\nbeta\r\ngamma\r\n");
    // A multi-line edit written with LF matches and is written as CRLF.
    let change = TextFileChange::Edit {
        edits: vec![edit("alpha", "ALPHA"), edit("beta\ngamma", "b\nc\nd")],
    };
    let (receipt, _) = mutate_text(&directory, &file, &original, Some(&change)).unwrap();
    assert!(receipt.verified);
    assert_eq!(std::fs::read(&path).unwrap(), b"ALPHA\r\nb\r\nc\r\nd\r\n");
}

#[test]
fn large_files_update_within_the_text_file_limit() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    let mut content = "line\n".repeat(3 * 1024 * 1024);
    content.push_str("end\n");
    std::fs::write(&path, &content).unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    let change = replace("end", "END");
    let (receipt, backup) = mutate_text(
        &directory,
        &file,
        &digest(content.as_bytes()),
        Some(&change),
    )
    .unwrap();
    assert!(receipt.verified);
    assert_eq!(backup, content.as_bytes());
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.ends_with("END\n"));
    // Growth past the limit is rejected before anything is written.
    let grow = TextFileChange::Edit {
        edits: vec![TextEdit {
            old_text: "line".into(),
            new_text: "lines".into(),
            replace_all: true,
        }],
    };
    let current = receipt.file.unwrap();
    assert!(mutate_text(&directory, &current.file, &current.sha256, Some(&grow)).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
}

#[test]
fn conflicts_links_and_ambiguous_replacements_do_not_overwrite() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "repeat repeat").unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    let hash = digest(b"repeat repeat");
    let ambiguous = replace("repeat", "new");
    assert!(mutate_text(&directory, &file, &hash, Some(&ambiguous)).is_err());
    let change = write("new");
    let conflict = mutate_text_with_hooks(
        request(&directory, &file, &hash, Some(&change)),
        &test_context(),
        &|| std::fs::write(&path, "external change").unwrap(),
        &|| {},
    );
    assert!(conflict.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external change");
    let fresh = issue(&path).unwrap();
    std::fs::hard_link(&path, root.path().join("alias.txt")).unwrap();
    assert!(mutate_text(&directory, &fresh, &digest(b"external change"), None).is_err());
    assert!(path.exists());
}

#[test]
fn empty_updates_and_binary_text_are_exact() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "aaa").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let overlapping = replace("aa", "b");
    let binary = write("binary\0payload");
    for change in [&overlapping, &binary] {
        assert!(mutate_text(&directory, &target, &digest(b"aaa"), Some(change)).is_err());
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"aaa");
    let empty = write("");
    let (receipt, _) = mutate_text(&directory, &target, &digest(b"aaa"), Some(&empty)).unwrap();
    assert!(receipt.verified);
    assert_eq!(receipt.original_sha256, Some(digest(b"aaa")));
    let artifact = receipt.file.unwrap();
    assert_eq!(artifact.byte_len, 0);
    assert_eq!(artifact.sha256, digest(b""));
    assert!(std::fs::read(&path).unwrap().is_empty());
    assert!(receipt.message.contains("no post-write readback"));
}

#[test]
fn replacement_preserves_permissions_and_extended_attributes() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let source = File::open(&path).unwrap();
    let value = b"retained metadata";
    assert_eq!(
        unsafe {
            libc::fsetxattr(
                source.as_raw_fd(),
                c"com.lcxbox.text-test".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        },
        0
    );
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let change = write("new");
    let (receipt, _) = mutate_text(&directory, &target, &digest(b"old"), Some(&change)).unwrap();
    assert!(receipt.verified);
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
    let result = File::open(&path).unwrap();
    let mut actual = [0u8; 64];
    let size = unsafe {
        libc::fgetxattr(
            result.as_raw_fd(),
            c"com.lcxbox.text-test".as_ptr(),
            actual.as_mut_ptr().cast(),
            actual.len(),
            0,
            0,
        )
    };
    assert_eq!(size, value.len() as isize);
    assert_eq!(&actual[..size as usize], value);
}

#[test]
fn exact_native_dispatch_rechecks_commit_guard_and_returns_typed_recovery() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "old").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let action = FilePatchAction::UpdateText {
        directory: directory.clone(),
        expected_sha256: digest(b"old"),
        change: write("new"),
    };
    assert!(execute(&target, &action, &test_context(), || Err(conflict())).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    let context = test_context();
    let (receipt, _) = execute(&target, &action, &context, || Ok(())).unwrap();
    receipt.validate_for(&target, &action).unwrap();
    assert!(receipt.verified);
    assert_eq!(receipt.directory, directory);
    assert_eq!(receipt.updated_sha256, Some(digest(b"new")));
    assert_eq!(
        backed_up(&context, &receipt.recovery.recovery_id).unwrap(),
        b"old"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
}

#[test]
fn later_external_write_does_not_reclassify_successful_commit() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "approved old").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context();
    let change = write("new");
    let receipt = mutate_text_with_hooks(
        request(&directory, &target, &digest(b"approved old"), Some(&change)),
        &context,
        &|| {},
        &|| std::fs::write(&path, "external after commit").unwrap(),
    )
    .unwrap();
    assert!(receipt.verified);
    assert_eq!(std::fs::read(&path).unwrap(), b"external after commit");
    assert_eq!(
        backed_up(&context, &receipt.recovery.recovery_id).unwrap(),
        b"approved old"
    );
    assert!(receipt.message.contains("no post-write readback"));
    let later = write("later");
    assert!(
        mutate_text(
            &directory,
            &receipt.file.unwrap().file,
            &digest(b"new"),
            Some(&later),
        )
        .is_err()
    );
}

#[test]
fn reference_persistence_failure_does_not_turn_committed_update_into_failure() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "old").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let previous = {
        let mut state = store().lock().unwrap();
        state
            .durable_registry_path
            .replace(path.join("registry.json"))
    };
    let change = write("new");
    let result = mutate_text(&directory, &target, &digest(b"old"), Some(&change));
    store().lock().unwrap().durable_registry_path = previous;
    let (receipt, backup) = result.unwrap();
    assert!(receipt.verified);
    assert!(receipt.file.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert_eq!(backup, b"old");
}

#[test]
fn directory_mismatch_and_symlink_replacement_never_mutate_target() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    let other = outside.path().join("notes.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::write(&other, "outside").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    assert!(
        mutate_text(
            &issue(outside.path()).unwrap(),
            &target,
            &digest(b"old"),
            None
        )
        .is_err()
    );
    let result = mutate_text_with_hooks(
        request(&directory, &target, &digest(b"old"), None),
        &test_context(),
        &|| {
            std::fs::rename(&path, root.path().join("preserved.txt")).unwrap();
            std::os::unix::fs::symlink(&other, &path).unwrap();
        },
        &|| {},
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&other).unwrap(), b"outside");
    assert_eq!(
        std::fs::read(root.path().join("preserved.txt")).unwrap(),
        b"old"
    );
}

#[test]
fn restore_puts_back_an_update_and_is_itself_undoable() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "original").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context();
    let updated = update(&context, &directory, &target, b"original", write("by ai"));
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
    assert_eq!(restored.original_file_name, "notes.txt");
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
fn restore_recreates_a_deleted_file_with_safe_permission_bits() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "kept text").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4640)).unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context();
    let deleted = delete(&context, &directory, &target, b"kept text");
    assert!(!path.exists());
    // Present but expected absent: nothing is written.
    let sha_of_absent = restore(
        &next_operation(&context),
        &directory,
        &deleted.recovery.recovery_id,
        RestoreExpectation::Sha(digest(b"kept text")),
    );
    assert!(sha_of_absent.is_err());
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
    assert_eq!(restored.original_size_bytes, 0);
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
    let context = test_context();
    let deleted = delete(&context, &directory, &target, b"deleted");
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
    let restore_context = next_operation(&context);
    let raced = mutate_text_with_hooks(
        TextMutation {
            directory: &directory,
            target: TextTarget::Name("notes.txt"),
            expected_sha256: None,
            intent: TextIntent::Restore {
                recovery_id: &deleted.recovery.recovery_id,
            },
        },
        &restore_context,
        &|| std::fs::write(&path, "raced").unwrap(),
        &|| panic!("a raced restore must not commit"),
    );
    assert!(raced.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"raced");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn restore_rejects_another_or_replaced_directory() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let original_directory = root.path().join("project");
    std::fs::create_dir(&original_directory).unwrap();
    let path = original_directory.join("notes.txt");
    std::fs::write(&path, "content").unwrap();
    let directory = issue(&original_directory).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context();
    let deleted = delete(&context, &directory, &target, b"content");
    // Another selected directory without that file.
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let other_directory = issue(&other).unwrap();
    assert!(
        restore(
            &next_operation(&context),
            &other_directory,
            &deleted.recovery.recovery_id,
            RestoreExpectation::Absent,
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
    // The original directory was replaced by a new one at the same path.
    std::fs::rename(&original_directory, root.path().join("moved")).unwrap();
    std::fs::create_dir(&original_directory).unwrap();
    let replacement = issue(&original_directory).unwrap();
    assert!(
        restore(
            &next_operation(&context),
            &replacement,
            &deleted.recovery.recovery_id,
            RestoreExpectation::Absent,
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&original_directory).unwrap().count(), 0);
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    assert_eq!(
        vault
            .lock()
            .unwrap()
            .list(&context.scope, Some(&context.conversation_id))
            .len(),
        1,
        "a rejected restore records no backup"
    );
}

#[test]
fn durable_directory_references_restore_after_a_worker_restart() {
    let _guard = file_store_test_lock();
    let registry = tempfile::tempdir().unwrap();
    configure_durable_artifact_store(Some(registry.path()));
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, "before").unwrap();
    let durable = resolve_directory(root.path().to_str().unwrap())
        .unwrap()
        .directory;
    let transient = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context();
    let updated = update(&context, &durable, &target, b"before", write("after"));
    reset_worker_incarnation();
    let expected = RestoreExpectation::Sha(updated.updated_sha256.clone().unwrap());
    let stale = restore(
        &next_operation(&context),
        &transient,
        &updated.recovery.recovery_id,
        expected.clone(),
    );
    let restored = restore(
        &next_operation(&context),
        &durable,
        &updated.recovery.recovery_id,
        expected,
    );
    configure_durable_artifact_store(None);
    assert!(stale.is_err(), "a worker-incarnation reference is stale");
    assert!(restored.unwrap().verified);
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
}

#[test]
fn a_sixteen_mebibyte_file_reads_in_windows_and_by_search() {
    use desk_agent_protocol::computer_use::{FileContentReadParams, TextReadSelection};
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.txt");
    let line = "0123456789abcdef\n";
    let lines = (MAX_TEXT_FILE_BYTES as usize - 32) / line.len();
    let mut content = line.repeat(lines);
    content.push_str("needle at the end\n");
    assert!(content.len() as u64 <= MAX_TEXT_FILE_BYTES);
    std::fs::write(&path, &content).unwrap();
    let file = issue(&path).unwrap();
    let read = |selection| {
        read_text(&FileContentReadParams {
            file: file.clone(),
            max_bytes: 64 * 1024,
            selection,
        })
        .unwrap()
    };
    let first = read(TextReadSelection::Whole);
    assert!(first.truncated);
    assert!(first.content_utf8.len() <= 64 * 1024);
    assert_eq!(first.sha256, digest(content.as_bytes()));
    assert_eq!(first.byte_len, content.len() as u64);
    assert_eq!(first.total_lines, lines as u64 + 1);
    let last = read(TextReadSelection::Lines {
        start_line: lines as u64,
        max_lines: Some(2),
    });
    assert_eq!(last.content_utf8, format!("{line}needle at the end\n"));
    assert_eq!(
        (last.start_line, last.end_line),
        (lines as u64, lines as u64 + 1)
    );
    let found = read(TextReadSelection::Search {
        query: "needle".into(),
        case_sensitive: true,
        context_lines: 0,
        max_matches: 5,
    });
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches[0].line, lines as u64 + 1);
    // One byte more than the limit is rejected.
    std::fs::write(&path, vec![b'a'; MAX_TEXT_FILE_BYTES as usize + 1]).unwrap();
    assert!(
        read_text(&FileContentReadParams {
            file: issue(&path).unwrap(),
            max_bytes: 64 * 1024,
            selection: TextReadSelection::Whole,
        })
        .is_err()
    );
}
