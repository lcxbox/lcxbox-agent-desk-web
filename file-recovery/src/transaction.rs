//! Cleanup touches only registered directory objects and their known children.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub parent: String,
    pub directory: String,
    pub identity: TransactionIdentity,
}

/// A user-file transaction planned before the backup; the vault derives the
/// transaction directory name from the record id and persists the plan with
/// the Preparing record.
#[derive(Clone, Debug)]
pub struct TransactionPlan {
    parent: String,
    identity: TransactionIdentity,
}

impl Transaction {
    pub(crate) fn validate(&self, id: &str) -> io::Result<()> {
        self.identity.validate_host()?;
        if !Path::new(&self.parent).is_absolute()
            || self.parent.len() > 4096
            || self.parent.chars().any(char::is_control)
            || self.directory != format!(".assistant-transaction-{id}")
        {
            return Err(invalid("invalid registered transaction location"));
        }
        Ok(())
    }
}

impl TransactionPlan {
    /// The caller has verified `parent`'s identity through its held handle.
    #[cfg(unix)]
    pub fn unix(parent: &str, device: u64, inode: u64, original_inode: u64) -> io::Result<Self> {
        if !Path::new(parent).is_absolute()
            || parent.len() > 4096
            || parent.chars().any(char::is_control)
        {
            return Err(invalid("invalid transaction parent"));
        }
        let files = InodeIdentity {
            parent_device: device,
            parent_inode: inode,
            directory_inode: None,
            original_inode,
            staged_inode: None,
        };
        #[cfg(target_os = "macos")]
        let identity = {
            use std::{
                fs::OpenOptions,
                os::unix::fs::{MetadataExt, OpenOptionsExt},
            };
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(parent)?;
            let metadata = directory.metadata()?;
            if metadata.dev() != device || metadata.ino() != inode {
                return Err(invalid("transaction parent identity changed"));
            }
            TransactionIdentity::Macos {
                volume_uuid: crate::macos::volume_uuid(&directory)?,
                files,
            }
        };
        #[cfg(not(target_os = "macos"))]
        let identity = TransactionIdentity::Unix { files };
        Ok(Self {
            parent: parent.into(),
            identity,
        })
    }

    /// The caller retains its source version guard and authorized handles.
    /// The parent path is resolved and matched against its handle; the plan
    /// does not authorize access to an arbitrary user path.
    #[cfg(windows)]
    pub fn windows(
        parent_path: &Path,
        parent: &std::fs::File,
        original: &std::fs::File,
    ) -> io::Result<Self> {
        let files = WindowsIdentity::from_handles(parent, original)?;
        let _location = crate::windows::transaction_location(parent_path, parent, "", None, None)?;
        Ok(Self {
            parent: parent_path
                .to_str()
                .ok_or_else(|| invalid("invalid transaction parent encoding"))?
                .into(),
            identity: TransactionIdentity::Windows { files },
        })
    }

    pub(crate) fn into_transaction(self, id: &str) -> io::Result<Transaction> {
        let transaction = Transaction {
            parent: self.parent,
            directory: format!(".assistant-transaction-{id}"),
            identity: self.identity,
        };
        transaction.validate(id)?;
        Ok(transaction)
    }
}

impl LockedVault {
    /// The planned transaction directory name of a backed-up record.
    pub fn transaction_directory(&self, scope: &Scope, id: &str) -> io::Result<String> {
        self.record(scope, id)
            .and_then(|record| record.transaction.as_ref())
            .map(|transaction| transaction.directory.clone())
            .ok_or_else(|| invalid("transaction was not planned"))
    }

    /// Register the created transaction directory and, when it already exists,
    /// the staged replacement in one ledger write. Registered identities are
    /// monotonic and cannot be replaced.
    #[cfg(unix)]
    pub fn register_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        directory_inode: u64,
        staged_inode: Option<u64>,
    ) -> io::Result<()> {
        let r = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| &r.scope == scope && r.change == ChangeState::BackupReady)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let tx = r
            .transaction
            .as_mut()
            .ok_or_else(|| invalid("transaction was not planned"))?;
        let files = tx.identity.inodes_mut()?;
        if files
            .directory_inode
            .is_some_and(|inode| inode != directory_inode)
            || files
                .staged_inode
                .is_some_and(|inode| Some(inode) != staged_inode)
        {
            return Err(invalid("transaction identity changed"));
        }
        files.directory_inode = Some(directory_inode);
        files.staged_inode = staged_inode;
        Self::rebill(r)?;
        self.persist()
    }

    /// Remove registered transaction objects and persist the result when the
    /// record changed. Returns whether no transaction remains.
    pub fn settle_transaction(&mut self, scope: &Scope, id: &str) -> io::Result<bool> {
        if self.record(scope, id).is_none() {
            return Err(invalid("transaction unavailable"));
        }
        let settled = self.settle_transaction_in_memory(id)?;
        self.flush()?;
        Ok(settled)
    }

    pub(crate) fn settle_transaction_in_memory(&mut self, id: &str) -> io::Result<bool> {
        let r = self
            .ledger
            .records
            .get(id)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let Some(tx) = r.transaction.clone() else {
            return Ok(true);
        };
        #[cfg(not(windows))]
        let result = clean(&tx);
        #[cfg(windows)]
        let result = (|| {
            let entry = r
                .inline_entry()
                .ok_or_else(|| invalid("Windows transaction requires a text record"))?;
            let namespace = r.namespace();
            let content = match &entry.before {
                Some(before) => self.read_object(&namespace, before)?,
                None => Vec::new(),
            };
            let metadata = match &entry.metadata {
                Some(sha) => self.read_object(&namespace, &ObjRef::full(sha.clone()))?,
                None => b"{}".to_vec(),
            };
            crate::windows::clean_transaction(&tx, r.change, &entry.rel_path, &content, &metadata)
        })();
        let r = self.ledger.records.get_mut(id).unwrap();
        let previous_error = r.cleanup_error.clone();
        match result {
            Ok(()) => {
                r.transaction = None;
                r.cleanup_error = None;
                Self::rebill(r)?;
                self.dirty = true;
                Ok(true)
            }
            Err(e) => {
                r.cleanup_error = Some(format!("transaction cleanup: {:?}", e.kind()));
                self.dirty |= r.cleanup_error != previous_error;
                Ok(false)
            }
        }
    }
}
#[cfg(unix)]
#[path = "transaction_unix.rs"]
mod native;
#[cfg(not(any(unix, windows)))]
#[path = "transaction_unsupported.rs"]
mod native;
#[cfg(not(windows))]
use native::clean;

#[cfg(windows)]
#[path = "transaction_windows_commit.rs"]
mod windows_commit;
#[cfg(windows)]
#[path = "transaction_windows_registration.rs"]
mod windows_registration;
#[cfg(windows)]
pub use windows_commit::{WindowsCommitError, WindowsCommitRequest};
