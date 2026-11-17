//! Register handle-derived Windows identities in the shared recovery ledger.
use super::*;
use std::fs::File;

impl LockedVault {
    /// Create only the directory named by the record's transaction plan and
    /// register its identity. The registration is written by the next ledger
    /// write (staging registration or commit intent); a crash before that
    /// leaves an unregistered private directory that cleanup refuses to adopt,
    /// exactly as a crash between creation and an immediate registration.
    pub fn create_windows_transaction_directory(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &File,
    ) -> io::Result<crate::windows::PrivateDirectory> {
        self.storage.ensure_writable()?;
        let transaction = self
            .ledger
            .records
            .get(id)
            .filter(|record| &record.scope == scope && record.change == ChangeState::BackupReady)
            .and_then(|record| record.transaction.as_ref())
            .ok_or_else(|| invalid("transaction was not planned"))?
            .clone();
        let TransactionIdentity::Windows { files } = &transaction.identity else {
            return Err(invalid("transaction has a different platform identity"));
        };
        let parent_identity =
            crate::windows::file_identity(parent, crate::windows::FileKind::Directory)?;
        if files.directory_file_id.is_some()
            || files.staged_file_id.is_some()
            || files.parent_file_id != parent_identity.file_id
            || files.volume_serial != parent_identity.volume_serial
        {
            return Err(invalid(
                "transaction directory is already registered or its parent changed",
            ));
        }
        let _location = crate::windows::transaction_location(
            Path::new(&transaction.parent),
            parent,
            &transaction.directory,
            None,
            None,
        )?;
        let directory =
            crate::windows::PrivateDirectory::create_new(parent, &transaction.directory)?;
        self.register_windows_handles(scope, id, parent, directory.handle(), None)?;
        self.dirty = true;
        Ok(directory)
    }

    /// Register the transaction directory and its replacement file, then
    /// persist. Already registered directory and staging identities cannot be
    /// changed or retracted. Reopen the vault after any persistence error.
    pub fn register_windows_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &File,
        directory: &File,
        staged: Option<&File>,
    ) -> io::Result<()> {
        self.register_windows_handles(scope, id, parent, directory, staged)?;
        self.persist()
    }

    fn register_windows_handles(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &File,
        directory: &File,
        staged: Option<&File>,
    ) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|record| &record.scope == scope && record.change == ChangeState::BackupReady)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let transaction = record
            .transaction
            .as_mut()
            .ok_or_else(|| invalid("transaction was not planned"))?;
        let _location = crate::windows::transaction_location(
            Path::new(&transaction.parent),
            parent,
            &transaction.directory,
            Some(directory),
            staged,
        )?;
        let TransactionIdentity::Windows { files } = &mut transaction.identity else {
            return Err(invalid("transaction has a different platform identity"));
        };
        files.register_handles(parent, directory, staged)?;
        Self::rebill(record)
    }
}

#[cfg(test)]
#[path = "transaction_windows_tests.rs"]
mod tests;
