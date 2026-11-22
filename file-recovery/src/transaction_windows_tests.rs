use super::*;
use crate::windows::{FileKind, file_identity};
use ::windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};
use std::{fs, os::windows::fs::OpenOptionsExt};

fn directory(path: &Path) -> File {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .unwrap()
}
fn scope() -> Scope {
    Scope {
        authority: "central".into(),
        device: "device".into(),
        os_user: "user".into(),
        owner: "owner".into(),
    }
}
fn request(plan: TransactionPlan) -> BackupRequest<'static> {
    BackupRequest {
        scope: scope(),
        conversation: "conversation",
        operation: "operation",
        generation: "generation",
        kind: RecordKind::TextEdit,
        directory_identity: "directory",
        entries: vec![EntryInput {
            rel_path: "source.txt",
            before: Some(b"before"),
            after: Some(b"after"),
            metadata: Some(b"{}"),
        }],
        transaction: Some(plan),
        now_ms: 1000,
    }
}
/// Back up `source.txt` with a Windows transaction plan and persist the
/// BackupReady state. Returns the record and the planned directory name.
fn backup(vault: &mut LockedVault, root: &Path, parent: &File, original: &File) -> Record {
    let plan = TransactionPlan::windows(root, parent, original).unwrap();
    let record = vault.backup(request(plan), 0, |_, _| Ok(())).unwrap();
    vault.flush().unwrap();
    record
}
fn planned_name(record: &Record) -> String {
    record.transaction.as_ref().unwrap().directory.clone()
}
fn before(vault: &LockedVault, record: &Record) -> Vec<u8> {
    vault
        .entry_material(&scope(), "conversation", &record.id, 0, 1001)
        .unwrap()
        .before
        .unwrap()
}

#[test]
fn windows_transaction_identity_is_durable_and_monotonic() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let record = backup(&mut locked, root.path(), &parent, &original);
    let name = planned_name(&record);
    assert!(!root.path().join(&name).exists());
    let planned = locked.ledger.records[&record.id]
        .transaction
        .clone()
        .unwrap();
    drop(locked);
    let mut locked = vault.lock().unwrap();
    assert_eq!(
        serde_json::to_vec(&planned).unwrap(),
        serde_json::to_vec(
            &locked.ledger.records[&record.id]
                .transaction
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
    // The operation fence forbids planning the same change again.
    let plan = TransactionPlan::windows(root.path(), &parent, &original).unwrap();
    assert_eq!(
        locked
            .backup(request(plan), 0, |_, _| Ok(()))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    let _private_transaction = locked
        .create_windows_transaction_directory(&scope(), &record.id, &parent)
        .unwrap();
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    let transaction = directory(&root.path().join(&name));
    locked
        .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
        .unwrap();
    fs::write(root.path().join(&name).join("replacement"), b"after").unwrap();
    let staged = File::open(root.path().join(&name).join("replacement")).unwrap();
    locked
        .register_windows_transaction(&scope(), &record.id, &parent, &transaction, Some(&staged))
        .unwrap();
    let saved = serde_json::to_vec(&locked.ledger.records[&record.id].transaction).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
            .is_err()
    );
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&original)
            )
            .is_err()
    );
    let mut other_scope = scope();
    other_scope.owner = "another owner".into();
    assert!(
        locked
            .register_windows_transaction(
                &other_scope,
                &record.id,
                &parent,
                &transaction,
                Some(&staged)
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&locked.ledger.records[&record.id].transaction).unwrap(),
        saved
    );
    drop(locked);
    let mut locked = vault.lock().unwrap();
    let TransactionIdentity::Windows { files } = &locked.ledger.records[&record.id]
        .transaction
        .as_ref()
        .unwrap()
        .identity
    else {
        panic!("wrong platform");
    };
    assert_eq!(
        files.original_file_id,
        file_identity(&original, FileKind::File).unwrap().file_id
    );
    assert_eq!(
        files.staged_file_id,
        Some(file_identity(&staged, FileKind::File).unwrap().file_id)
    );
    assert_eq!(
        files.directory_file_id,
        Some(
            file_identity(&transaction, FileKind::Directory)
                .unwrap()
                .file_id
        )
    );
    locked
        .transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&staged)
            )
            .is_err()
    );
    drop(locked);
    let mut locked = vault.lock().unwrap();
    let unknown = locked.recover_interrupted(1001).unwrap();
    assert_eq!(unknown.len(), 1);
    let recovered = &locked.ledger.records[&record.id];
    assert_eq!(recovered.change, ChangeState::OutcomeUnknown);
    assert_eq!(recovered.material, MaterialState::Saved);
    assert!(recovered.transaction.is_some());
    assert!(recovered.cleanup_error.is_some());
    assert_eq!(fs::read(&source).unwrap(), b"before");
    assert_eq!(
        fs::read(root.path().join(&name).join("replacement")).unwrap(),
        b"after"
    );
}

#[test]
fn unpersisted_directory_registration_retains_material_and_refuses_adoption_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let record = backup(&mut locked, root.path(), &parent, &original);
    let name = planned_name(&record);
    let mut other = scope();
    other.owner = "other".into();
    assert!(
        locked
            .create_windows_transaction_directory(&other, &record.id, &parent)
            .is_err()
    );
    assert!(!root.path().join(&name).exists());
    let _private_transaction = locked
        .create_windows_transaction_directory(&scope(), &record.id, &parent)
        .unwrap();
    assert!(root.path().join(&name).is_dir());
    // The registration is written with the commit intent; that write fails.
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(root.path().join("file-recovery/index.json"))
        .unwrap();
    let error = locked
        .transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<crate::windows::IndexWriteError>()
            .unwrap()
            .outcome,
        crate::windows::IndexWriteOutcome::OutcomeUnknown
    );
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    drop((reader, locked));
    let mut reopened = vault.lock().unwrap();
    let stored = &reopened.ledger.records[&record.id];
    assert_eq!(stored.change, ChangeState::BackupReady);
    let TransactionIdentity::Windows { files } = &stored.transaction.as_ref().unwrap().identity
    else {
        panic!("wrong platform");
    };
    assert!(files.directory_file_id.is_none());
    assert_eq!(
        reopened
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(root.path().join(&name).is_dir());
    assert_eq!(fs::read(source).unwrap(), b"before");
    assert_eq!(before(&reopened, &record), b"before");
}

#[test]
fn windows_registration_rejects_nonprivate_transaction_directory_without_repairing_it() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let record = backup(&mut locked, root.path(), &parent, &original);
    let name = planned_name(&record);
    let path = root.path().join(&name);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("sentinel"), b"unrelated").unwrap();
    let transaction = directory(&path);
    let before = fs::read(root.path().join("file-recovery/index.json")).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
            .is_err()
    );
    assert!(crate::windows::PrivateDirectory::create_new(&parent, &name).is_err());
    assert!(crate::windows::PrivateDirectory::open_or_create(&parent, &name).is_err());
    assert_eq!(
        fs::read(root.path().join("file-recovery/index.json")).unwrap(),
        before
    );
    assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"unrelated");
}

#[test]
fn windows_registration_rejects_objects_outside_the_recorded_location() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let other_path = root.path().join("other");
    fs::create_dir(&other_path).unwrap();
    // A plan names the parent path its handle resolves to, nothing else.
    for path in [other_path.as_path(), Path::new("relative")] {
        assert!(TransactionPlan::windows(path, &parent, &original).is_err());
    }
    let record = backup(&mut locked, root.path(), &parent, &original);
    let name = planned_name(&record);
    let transaction_path = root.path().join(&name);
    let private_transaction = crate::windows::PrivateDirectory::create_new(&parent, &name).unwrap();
    let transaction = directory(&transaction_path);
    let other = directory(&other_path);
    let index_path = root.path().join("file-recovery/index.json");
    let planned = fs::read(&index_path).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &other, None)
            .is_err()
    );
    // Use the production creation guard: it owns DELETE access but denies
    // delete sharing to other handles for the entire registration/commit.
    let mut replacement = private_transaction.create_replacement().unwrap();
    std::io::Write::write_all(&mut replacement, b"replacement").unwrap();
    fs::write(other_path.join("replacement"), b"unrelated").unwrap();
    let unrelated = File::open(other_path.join("replacement")).unwrap();
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&unrelated)
            )
            .is_err()
    );
    assert_eq!(fs::read(&index_path).unwrap(), planned);
    let held = crate::windows::transaction_location(
        root.path(),
        &parent,
        &name,
        Some(&transaction),
        Some(&replacement),
    )
    .unwrap();
    assert!(
        fs::rename(
            transaction_path.join("replacement"),
            transaction_path.join("moved")
        )
        .is_err()
    );
    assert!(fs::rename(&transaction_path, root.path().join("moved-directory")).is_err());
    drop(held);
    locked
        .register_windows_transaction(
            &scope(),
            &record.id,
            &parent,
            &transaction,
            Some(&replacement),
        )
        .unwrap();
    assert_eq!(
        fs::read(other_path.join("replacement")).unwrap(),
        b"unrelated"
    );
}

#[test]
fn failed_preparing_publication_blocks_the_change_until_reopen() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let plan = TransactionPlan::windows(root.path(), &parent, &original).unwrap();
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(root.path().join("file-recovery/index.json"))
        .unwrap();
    // The Preparing record carries the plan; its publication fails before
    // any reservation or object publication.
    let error = locked
        .backup(request(plan), 0, |_, _| {
            panic!("an unpublished Preparing record must not reserve")
        })
        .unwrap_err();
    let publication = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::windows::IndexWriteError>()
        .unwrap();
    assert_eq!(
        publication.outcome,
        crate::windows::IndexWriteOutcome::OutcomeUnknown
    );
    let id = record_id(&scope(), "conversation", "operation").unwrap();
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &id, &parent)
            .is_err()
    );
    assert!(
        locked
            .transition(&scope(), &id, ChangeState::Aborted)
            .is_err()
    );
    assert!(
        !root
            .path()
            .join(format!(".agent-transaction-{id}"))
            .exists()
    );
    assert_eq!(fs::read(&source).unwrap(), b"before");
    drop((reader, locked));
    let reopened = vault.lock().unwrap();
    assert!(!reopened.ledger.records.contains_key(&id));
    assert!(reopened.storage.extra_used_bytes().unwrap() > 0);
    assert!(reopened.ledger.namespaces.is_empty());
}

#[test]
fn aborted_record_cannot_create_its_transaction_directory() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let record = backup(&mut locked, root.path(), &parent, &original);
    let mut other = scope();
    other.device = "other device".into();
    assert!(
        locked
            .create_windows_transaction_directory(&other, &record.id, &parent)
            .is_err()
    );
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    assert!(!root.path().join(planned_name(&record)).exists());
    assert_eq!(fs::read(&source).unwrap(), b"before");
}
