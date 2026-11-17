//! Private path-based storage for Unix recovery and device quota stores.
//! Every component is created owner-only and opened without following links.
use crate::invalid;
#[cfg(target_os = "macos")]
use crate::macos;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const OBJECTS: &str = "objects";
const TEMPORARY: &str = "tmp";

#[cfg(unix)]
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "recovery directory is not private or has an unexpected owner",
        ));
    }
    #[cfg(target_os = "macos")]
    macos::make_private(&File::open(path)?)?;
    Ok(())
}
#[cfg(not(unix))]
fn private_dir(_: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private recovery storage is unavailable on this platform",
    ))
}

fn open_private(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create);
    if create {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(invalid("recovery item is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 || meta.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovery item ownership or permissions changed",
            ));
        }
    }
    #[cfg(target_os = "macos")]
    macos::make_private(&file)?;
    Ok(file)
}
fn bounded_read(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = open_private(path, false)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("recovery record exceeds storage bound"));
    }
    Ok(bytes)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
fn temporary_name(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}-{}-{now:x}-{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
/// Write a new private file, flush it and rename it over `path`.
fn rename_publish(temporary: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let result = (|| {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(temporary)?;
        #[cfg(target_os = "macos")]
        macos::make_private(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        sync_directory(
            path.parent()
                .ok_or_else(|| invalid("recovery path has no parent"))?,
        )
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
/// Directory entries that are regular files, with their sizes; links and
/// other types are never followed.
fn regular_files(path: &Path) -> io::Result<Vec<(String, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let metadata = entry.metadata()?;
        if metadata.file_type().is_file() {
            files.push((name, metadata.len()));
        }
    }
    Ok(files)
}

pub(super) struct Root(PathBuf);
pub(super) struct Locked {
    root: PathBuf,
    _lock: File,
}
impl Root {
    pub fn open(data_root: &Path) -> io::Result<Self> {
        Self::open_named(data_root, "file-recovery")
    }
    pub fn open_named(data_root: &Path, leaf: &str) -> io::Result<Self> {
        let root = data_root.join(leaf);
        private_dir(&root)?;
        Ok(Self(root))
    }
    pub fn lock(&self) -> io::Result<Locked> {
        private_dir(&self.0)?;
        let lock = open_private(&self.0.join("lock"), true)?;
        lock.lock()?;
        Ok(Locked {
            root: self.0.clone(),
            _lock: lock,
        })
    }
    pub fn try_lock(&self) -> io::Result<Option<Locked>> {
        private_dir(&self.0)?;
        let lock = open_private(&self.0.join("lock"), true)?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Locked {
                root: self.0.clone(),
                _lock: lock,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}
impl Locked {
    pub fn read(&self, leaf: &str, limit: u64) -> io::Result<Vec<u8>> {
        bounded_read(&self.root.join(leaf), limit)
    }
    pub fn write_index(&self, bytes: &[u8]) -> io::Result<()> {
        // The lock is held: a temporary left by an interrupted write is stale.
        let temporary = self.root.join("index.json.tmp");
        match fs::remove_file(&temporary) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => (),
        }
        rename_publish(&temporary, &self.root.join("index.json"), bytes)
    }
    pub fn extra_used_bytes(&self) -> io::Result<u64> {
        Ok(0)
    }
    pub fn validate_missing_index(&self) -> io::Result<()> {
        Ok(())
    }
    fn namespace_dir(&self, namespace: &str, create: bool) -> io::Result<PathBuf> {
        if !crate::valid_namespace(namespace) {
            return Err(invalid("invalid recovery object namespace"));
        }
        let objects = self.root.join(OBJECTS);
        let directory = objects.join(namespace);
        if create {
            private_dir(&objects)?;
            private_dir(&directory)?;
            private_dir(&directory.join(TEMPORARY))?;
        }
        Ok(directory)
    }
    fn object_name(name: &str) -> io::Result<&str> {
        if crate::valid_id(name) {
            Ok(name)
        } else {
            Err(invalid("invalid recovery object name"))
        }
    }
    /// Rename-only publication: a present object name always denotes a
    /// complete, flushed object. Returns the replaced file's size, if any.
    pub fn publish_object(
        &self,
        namespace: &str,
        name: &str,
        bytes: &[u8],
    ) -> io::Result<Option<u64>> {
        let directory = self.namespace_dir(namespace, true)?;
        let path = directory.join(Self::object_name(name)?);
        let previous = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => Some(metadata.len()),
            Ok(_) => return Err(invalid("recovery object is not a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let temporary = directory.join(TEMPORARY).join(temporary_name(name));
        rename_publish(&temporary, &path, bytes)?;
        Ok(previous)
    }
    /// Whether a published object of this name exists; links do not count.
    pub fn object_exists(&self, namespace: &str, name: &str) -> io::Result<bool> {
        let directory = self.namespace_dir(namespace, false)?;
        match fs::symlink_metadata(directory.join(Self::object_name(name)?)) {
            Ok(metadata) => Ok(metadata.file_type().is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub fn read_object(&self, namespace: &str, name: &str, limit: u64) -> io::Result<Vec<u8>> {
        let directory = self.namespace_dir(namespace, false)?;
        bounded_read(&directory.join(Self::object_name(name)?), limit)
    }
    /// Removes the directory entry only; a link is removed, never followed.
    pub fn remove_object(&self, namespace: &str, name: &str) -> io::Result<()> {
        let directory = self.namespace_dir(namespace, false)?;
        fs::remove_file(directory.join(name))
    }
    pub fn list_namespaces(&self) -> io::Result<Vec<String>> {
        let objects = self.root.join(OBJECTS);
        let entries = match fs::read_dir(&objects) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut namespaces = Vec::new();
        for entry in entries {
            let entry = entry?;
            if let Ok(name) = entry.file_name().into_string()
                && crate::valid_namespace(&name)
                && entry.file_type()?.is_dir()
            {
                namespaces.push(name);
            }
        }
        Ok(namespaces)
    }
    /// Every entry of the namespace except the temporary directory. Links
    /// and other foreign entries are listed so that a sweep unlinks them.
    pub fn list_objects(&self, namespace: &str) -> io::Result<Vec<(String, u64)>> {
        let directory = self.namespace_dir(namespace, false)?;
        private_dir(&directory)?;
        let mut objects = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let metadata = entry.metadata()?;
            if !metadata.file_type().is_dir() {
                objects.push((name, metadata.len()));
            }
        }
        Ok(objects)
    }
    pub fn clear_temporary(&self, namespace: &str) -> io::Result<()> {
        let directory = self.namespace_dir(namespace, false)?.join(TEMPORARY);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => (),
            Ok(_) => return fs::remove_file(&directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        for (name, _) in regular_files(&directory)? {
            fs::remove_file(directory.join(name))?;
        }
        Ok(())
    }
    /// Remove material of an earlier storage format from the vault root.
    pub fn remove_legacy_material(&self) -> io::Result<()> {
        for (name, _) in regular_files(&self.root)? {
            if name != "lock" && name != "index.json" {
                fs::remove_file(self.root.join(name))?;
            }
        }
        sync_directory(&self.root)
    }
}
