//! Lease store of the Low labels on write roots (plan 6.5.3, part two).
//!
//! One store per user, shared by every worker process of that user:
//! `%LOCALAPPDATA%\lcxl-remote-desk-worker\exec-sandbox-labels\labels.json`
//! with a store lock file and one lock file per record (`locks\`). Lock files
//! are never deleted: deleting and recreating one would let two parties each
//! hold "the same" lock.
//!
//! - The store lock guards every read-modify-write of `labels.json`. While it
//!   is held nobody waits on a record lock: record locks are only tried with
//!   `LOCKFILE_FAIL_IMMEDIATELY`, and polling happens outside the store lock.
//! - An execution holds the shared lock of the record that covers its root
//!   (the root itself or an ancestor). Labelling and revoking hold the
//!   record's exclusive lock; the reservation of a new record (`applying`) and
//!   its exclusive lock are taken in the same store-lock critical section.
//! - Labelling runs on a background thread. The caller waits at most until
//!   its deadline (10 s, plan 6.5.3) and is otherwise refused as
//!   [`SandboxError::Preparing`]; the label is completed in the background and
//!   the next request reuses it.
//! - The reaper revokes records idle longer than the configured time, and
//!   `applying`/`revoking` leftovers of crashed processes, skipping every
//!   record whose exclusive lock it cannot take.
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
};
use windows::Win32::System::IO::OVERLAPPED;

use super::SandboxError;
use super::windows_label;

pub(crate) const DEFAULT_IDLE_MINUTES: u32 = 30;
pub(crate) const MIN_IDLE_MINUTES: u32 = 5;
pub(crate) const MAX_IDLE_MINUTES: u32 = 1440;

static IDLE_MINUTES: AtomicU32 = AtomicU32::new(DEFAULT_IDLE_MINUTES);
/// One background labelling per process; further requests are refused as
/// `Preparing` (plan 6.5.3).
static LABELLING: AtomicBool = AtomicBool::new(false);

/// Set the idle time after which an unused label is revoked (clamped).
pub(crate) fn set_idle_minutes(minutes: u32) {
    IDLE_MINUTES.store(
        minutes.clamp(MIN_IDLE_MINUTES, MAX_IDLE_MINUTES),
        Ordering::Relaxed,
    );
}

fn idle() -> Duration {
    Duration::from_secs(u64::from(IDLE_MINUTES.load(Ordering::Relaxed)) * 60)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum State {
    Applying,
    Applied,
    Revoking,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Record {
    path: String,
    volume: u32,
    file: u64,
    state: State,
    last_used: u64,
}

#[derive(Serialize, Deserialize)]
struct Mapping {
    version: u32,
    records: Vec<Record>,
}

const MAPPING_VERSION: u32 = 1;

impl Default for Mapping {
    fn default() -> Self {
        Self {
            version: MAPPING_VERSION,
            records: Vec::new(),
        }
    }
}

/// Case-folded path with a trailing separator, for ancestor comparisons.
fn norm(path: &str) -> String {
    let mut s = path.trim_end_matches('\\').to_lowercase();
    s.push('\\');
    s
}

/// `ancestor` is `path` or one of its ancestors.
fn covers(ancestor: &str, path: &str) -> bool {
    norm(path).starts_with(&norm(ancestor))
}

fn lock(file: &File, exclusive: bool, wait: bool) -> bool {
    let mut flags = 0u32;
    if exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK.0;
    }
    if !wait {
        flags |= LOCKFILE_FAIL_IMMEDIATELY.0;
    }
    let mut overlapped = OVERLAPPED::default();
    unsafe {
        LockFileEx(
            HANDLE(file.as_raw_handle()),
            LOCK_FILE_FLAGS(flags),
            None,
            1,
            0,
            &mut overlapped,
        )
    }
    .is_ok()
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

pub(crate) struct Store {
    dir: PathBuf,
}

/// The current user's store, created on first use. A failure is not kept:
/// the next use tries again.
pub(crate) fn store() -> Result<Arc<Store>, String> {
    static STORE: OnceLock<Arc<Store>> = OnceLock::new();
    open_once(&STORE, open_store)
        .map_err(|e| format!("the sandbox label store is unavailable: {e}"))
}

/// Keep the first store that opens; a failed open leaves `cell` empty.
fn open_once(
    cell: &OnceLock<Arc<Store>>,
    open: impl FnOnce() -> io::Result<Store>,
) -> io::Result<Arc<Store>> {
    if let Some(store) = cell.get() {
        return Ok(store.clone());
    }
    let store = Arc::new(open()?);
    Ok(cell.get_or_init(|| store).clone())
}

pub(super) fn local_app_data() -> io::Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .map_err(io::Error::other)?;
    let path = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    path
}

/// The per-user application directory of the worker; the whole directory is
/// refused as a write root (plan 6.5.3, part two).
pub(crate) fn worker_app_dir() -> Option<PathBuf> {
    local_app_data().ok().map(|local| local.join(APP_DIR))
}

const APP_DIR: &str = "lcxl-remote-desk-worker";

/// Open (creating if needed) a private child of the worker's application
/// directory below `local`. The application directory is created only here,
/// private to the user; a directory made any other way fails validation.
pub(super) fn private_app_child(local: &Path, leaf: &str) -> io::Result<PathBuf> {
    use desk_file_recovery::windows::PrivateDirectory;
    PrivateDirectory::open_data_root(local, APP_DIR)?.open_child(leaf)?;
    Ok(local.join(APP_DIR).join(leaf))
}

pub(super) fn open_store_in(local: &Path) -> io::Result<Store> {
    use desk_file_recovery::windows::PrivateDirectory;
    let labels = PrivateDirectory::open_data_root(local, APP_DIR)?.open_child(STORE_DIR)?;
    labels.open_child("locks")?;
    Ok(Store {
        dir: local.join(APP_DIR).join(STORE_DIR),
    })
}

const STORE_DIR: &str = "exec-sandbox-labels";

/// Unit tests use a store of their own instead of the user's.
#[cfg(test)]
fn open_store() -> io::Result<Store> {
    let dir = std::env::temp_dir().join(format!(
        "lcxl-exec-sandbox-test-store-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("locks"))?;
    Ok(Store { dir })
}

#[cfg(not(test))]
fn open_store() -> io::Result<Store> {
    open_store_in(&local_app_data()?)
}

/// The store lock and the mapping it guards.
struct Locked<'a> {
    store: &'a Store,
    _lock: File,
}

impl Store {
    fn lock(&self) -> io::Result<Locked<'_>> {
        let file = open_lock_file(&self.dir.join("store.lock"))?;
        if !lock(&file, true, true) {
            return Err(io::Error::last_os_error());
        }
        Ok(Locked {
            store: self,
            _lock: file,
        })
    }

    fn record_lock(&self, record: &Record) -> io::Result<File> {
        open_lock_file(
            &self
                .dir
                .join("locks")
                .join(format!("{:08x}-{:016x}.lock", record.volume, record.file)),
        )
    }
}

impl Locked<'_> {
    fn path(&self) -> PathBuf {
        self.store.dir.join("labels.json")
    }

    /// A missing file is an empty mapping; a damaged one fails closed.
    fn load(&self) -> Result<Mapping, SandboxError> {
        let mut text = String::new();
        match File::open(self.path()) {
            Ok(mut file) => {
                file.read_to_string(&mut text).map_err(|e| {
                    SandboxError::Restriction(format!(
                        "the sandbox label mapping is unreadable: {e}"
                    ))
                })?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Mapping::default()),
            Err(e) => {
                return Err(SandboxError::Restriction(format!(
                    "the sandbox label mapping is unreadable: {e}"
                )));
            }
        }
        match serde_json::from_str::<Mapping>(&text) {
            Ok(m) if m.version == MAPPING_VERSION => Ok(m),
            _ => Err(SandboxError::Restriction(
                "the sandbox label mapping is damaged; sandboxed commands are refused until it is \
                 repaired"
                    .into(),
            )),
        }
    }

    fn save(&self, mapping: &Mapping) -> io::Result<()> {
        let tmp = self.store.dir.join("labels.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(mapping).map_err(io::Error::other)?,
        )?;
        std::fs::rename(&tmp, self.path())
    }
}

/// The shared hold of one covering record during an execution. Dropping it
/// records the last use before the lock is released (plan 6.5.3).
pub(crate) struct Lease {
    store: Arc<Store>,
    covering: String,
    lock: Option<File>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(locked) = self.store.lock()
            && let Ok(mut mapping) = locked.load()
        {
            if let Some(record) = mapping
                .records
                .iter_mut()
                .find(|r| r.path.eq_ignore_ascii_case(&self.covering))
            {
                record.last_used = now();
            }
            let _ = locked.save(&mapping);
        }
        drop(self.lock.take());
    }
}

/// Make `root` (an opened, checked write root) writable for the sandbox and
/// hold it for one execution. Blocking; returns `Preparing` when the label is
/// not in place by `deadline`.
pub(crate) fn acquire(
    root: &Path,
    handle: &File,
    deadline: Instant,
) -> Result<Lease, SandboxError> {
    let store = store().map_err(SandboxError::Restriction)?;
    let root_str = root.display().to_string();
    let (volume, file) = windows_label::identity(handle)
        .map_err(|e| SandboxError::RootUnavailable(e.to_string()))?;
    loop {
        if Instant::now() >= deadline {
            return Err(SandboxError::Preparing);
        }
        let locked = store.lock().map_err(|e| {
            SandboxError::Restriction(format!("the sandbox label store is locked: {e}"))
        })?;
        let mut mapping = locked.load()?;
        let covering = mapping
            .records
            .iter()
            .filter(|r| covers(&r.path, &root_str))
            .max_by_key(|r| r.path.len())
            .cloned();
        match covering {
            Some(record) if record.state == State::Applied => {
                let lock_file = store.record_lock(&record).map_err(io_restriction)?;
                if lock(&lock_file, false, false) {
                    let healthy = windows_label::has_root_label(Path::new(&record.path));
                    if healthy {
                        if let Some(r) = mapping
                            .records
                            .iter_mut()
                            .find(|r| r.path.eq_ignore_ascii_case(&record.path))
                        {
                            r.last_used = now();
                        }
                        locked.save(&mapping).map_err(io_restriction)?;
                        return Ok(Lease {
                            store: Arc::clone(&store),
                            covering: record.path,
                            lock: Some(lock_file),
                        });
                    }
                    // The label was removed outside this product: relabel it
                    // when nobody else holds the record.
                    drop(lock_file);
                    let exclusive = store.record_lock(&record).map_err(io_restriction)?;
                    if lock(&exclusive, true, false) {
                        if let Some(r) = mapping
                            .records
                            .iter_mut()
                            .find(|r| r.path.eq_ignore_ascii_case(&record.path))
                        {
                            r.state = State::Applying;
                        }
                        if LABELLING.swap(true, Ordering::AcqRel) {
                            return Err(SandboxError::Preparing);
                        }
                        locked.save(&mapping).map_err(io_restriction)?;
                        drop(locked);
                        wait_for_label(&store, record, exclusive, deadline)?;
                        continue;
                    }
                }
                drop(locked);
            }
            Some(_) => drop(locked),
            None => {
                let descendant_busy = mapping
                    .records
                    .iter()
                    .any(|r| covers(&root_str, &r.path) && r.state != State::Applied);
                if !descendant_busy {
                    if LABELLING.swap(true, Ordering::AcqRel) {
                        return Err(SandboxError::Preparing);
                    }
                    let record = Record {
                        path: root_str.clone(),
                        volume,
                        file,
                        state: State::Applying,
                        last_used: now(),
                    };
                    let exclusive = match store.record_lock(&record) {
                        Ok(f) if lock(&f, true, false) => f,
                        other => {
                            LABELLING.store(false, Ordering::Release);
                            if let Err(e) = other {
                                return Err(io_restriction(e));
                            }
                            drop(locked);
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    };
                    mapping.records.push(record.clone());
                    if let Err(e) = locked.save(&mapping) {
                        LABELLING.store(false, Ordering::Release);
                        return Err(io_restriction(e));
                    }
                    drop(locked);
                    wait_for_label(&store, record, exclusive, deadline)?;
                    continue;
                }
                drop(locked);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn io_restriction(e: io::Error) -> SandboxError {
    SandboxError::Restriction(format!("the sandbox label store failed: {e}"))
}

/// Label `record` on a background thread (which owns the exclusive lock and
/// the process-wide labelling slot) and wait for it until `deadline`.
fn wait_for_label(
    store: &Arc<Store>,
    record: Record,
    exclusive: File,
    deadline: Instant,
) -> Result<(), SandboxError> {
    let (tx, rx) = mpsc::channel();
    let store = Arc::clone(store);
    std::thread::spawn(move || {
        let outcome = label_root(Path::new(&record.path));
        if let Ok(locked) = store.lock()
            && let Ok(mut mapping) = locked.load()
        {
            match &outcome {
                Ok(()) => {
                    if let Some(r) = mapping
                        .records
                        .iter_mut()
                        .find(|r| r.path.eq_ignore_ascii_case(&record.path))
                    {
                        r.state = State::Applied;
                        r.last_used = now();
                    }
                }
                Err(_) => mapping
                    .records
                    .retain(|r| !r.path.eq_ignore_ascii_case(&record.path)),
            }
            let _ = locked.save(&mapping);
        }
        drop(exclusive);
        LABELLING.store(false, Ordering::Release);
        let _ = tx.send(outcome);
    });
    let wait = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(wait) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(reason)) => Err(SandboxError::Restriction(reason)),
        Err(_) => Err(SandboxError::Preparing),
    }
}

/// Scan for multi-link files, then label; any object the label could not
/// reach rolls the label back (plan 6.5.3).
fn label_root(path: &Path) -> Result<(), String> {
    let links = windows_label::multi_link_files(path, 3);
    if !links.is_empty() {
        return Err(format!(
            "{} contains files with more than one hard link ({}); labelling it would make their \
             other names writable too. Run the command with sandbox=false or remove the hard links",
            path.display(),
            links
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let failures = windows_label::set_label(path).map_err(|e| {
        format!(
            "{} could not be prepared for the sandbox: {e}",
            path.display()
        )
    })?;
    if failures.count > 0 {
        let _ = windows_label::clear_label(path);
        return Err(format!(
            "{} could not be prepared for the sandbox: {} item(s) refused the change, for example {}",
            path.display(),
            failures.count,
            failures.sample.join("; ")
        ));
    }
    Ok(())
}

/// Revoke what may be revoked now. `everything` revokes every unused record
/// regardless of idle time (the user's "revoke all"). Returns (revoked,
/// skipped because in use).
pub(crate) fn reap(everything: bool) -> (usize, usize) {
    let Ok(store) = store() else {
        return (0, 0);
    };
    let mut revoked = 0;
    let mut tried: Vec<String> = Vec::new();
    loop {
        let Ok(locked) = store.lock() else {
            break;
        };
        let Ok(mut mapping) = locked.load() else {
            break;
        };
        let limit = idle().as_secs();
        let now = now();
        let candidate = mapping.records.iter().find(|r| {
            !tried.iter().any(|t| t.eq_ignore_ascii_case(&r.path))
                && (r.state != State::Applied
                    || everything
                    || now.saturating_sub(r.last_used) >= limit)
        });
        let Some(record) = candidate.cloned() else {
            break;
        };
        tried.push(record.path.clone());
        let Ok(exclusive) = store.record_lock(&record) else {
            continue;
        };
        if !lock(&exclusive, true, false) {
            continue;
        }
        // Gone, moved or replaced: nothing left to revoke at this path.
        let same = open_dir(Path::new(&record.path))
            .and_then(|h| windows_label::identity(&h))
            .is_ok_and(|id| id == (record.volume, record.file));
        if !same {
            mapping
                .records
                .retain(|r| !r.path.eq_ignore_ascii_case(&record.path));
            let _ = locked.save(&mapping);
            continue;
        }
        if let Some(r) = mapping
            .records
            .iter_mut()
            .find(|r| r.path.eq_ignore_ascii_case(&record.path))
        {
            r.state = State::Revoking;
        }
        if locked.save(&mapping).is_err() {
            break;
        }
        drop(locked);
        match windows_label::clear_label(Path::new(&record.path)) {
            Ok(f) if f.count > 0 => log::warn!(
                "exec sandbox: revoking the label of {} left {} item(s): {}",
                record.path,
                f.count,
                f.sample.join("; ")
            ),
            Ok(_) => {}
            Err(e) => log::warn!(
                "exec sandbox: revoking the label of {} failed: {e}",
                record.path
            ),
        }
        if let Ok(locked) = store.lock()
            && let Ok(mut mapping) = locked.load()
        {
            mapping
                .records
                .retain(|r| !r.path.eq_ignore_ascii_case(&record.path));
            let _ = locked.save(&mapping);
            revoked += 1;
        }
        drop(exclusive);
    }
    let in_use = store
        .lock()
        .ok()
        .and_then(|l| l.load().ok())
        .map(|m| m.records.len())
        .unwrap_or(0);
    (revoked, if everything { in_use } else { 0 })
}

fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Start the per-process reaper once: an immediate convergence pass, then one
/// pass a minute.
pub(crate) fn start_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("exec-sandbox-reaper".into())
            .spawn(|| {
                loop {
                    let (revoked, _) = reap(false);
                    if revoked > 0 {
                        log::info!("exec sandbox: revoked {revoked} idle label(s)");
                    }
                    std::thread::sleep(Duration::from_secs(60));
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_compares_whole_components_case_insensitively() {
        assert!(covers(r"C:\Work", r"C:\work\repo"));
        assert!(covers(r"C:\Work\", r"C:\Work"));
        assert!(!covers(r"C:\Work", r"C:\Workspace"));
        assert!(!covers(r"C:\Work\repo", r"C:\Work"));
    }

    #[test]
    fn a_failed_store_open_is_tried_again() {
        let cell = OnceLock::new();
        let scratch = tempfile::tempdir().unwrap();
        assert!(open_once(&cell, || Err(io::Error::other("not yet"))).is_err());
        let opened = open_once(&cell, || {
            Ok(Store {
                dir: scratch.path().to_path_buf(),
            })
        })
        .unwrap();
        assert_eq!(opened.dir, scratch.path());
        // Once open, the store is kept.
        let again = open_once(&cell, || Err(io::Error::other("unused"))).unwrap();
        assert!(Arc::ptr_eq(&opened, &again));
    }

    #[test]
    fn idle_minutes_are_clamped() {
        set_idle_minutes(1);
        assert_eq!(idle(), Duration::from_secs(5 * 60));
        set_idle_minutes(100_000);
        assert_eq!(idle(), Duration::from_secs(1440 * 60));
        set_idle_minutes(DEFAULT_IDLE_MINUTES);
    }
}
