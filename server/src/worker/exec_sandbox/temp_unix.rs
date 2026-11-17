//! The private temporary directory of one sandboxed execution on Linux and
//! macOS: `<private data dir>/exec-tmp/<pid>-<random>`, created `0700` and
//! never reused. Its parent is not writable inside the sandbox, so a command
//! cannot replace the directory itself; removal does not follow links.
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PARENT: &str = "exec-tmp";

pub(crate) struct PerExecTemp {
    /// Real path, as the platform restriction names it.
    path: PathBuf,
    /// The opened directory a Landlock rule attaches to.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    handle: File,
}

fn private_parent(private_dir: &Path) -> io::Result<PathBuf> {
    let parent = private_dir.join(PARENT);
    match DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => {}
        Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => {}
        Err(cause) => return Err(cause),
    }
    let metadata = std::fs::symlink_metadata(&parent)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the temporary parent is not a directory owned by this user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(parent)
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

impl PerExecTemp {
    /// Create a new directory below `private_dir`. Blocking.
    pub(crate) fn create(private_dir: &Path) -> io::Result<Self> {
        let parent = private_parent(private_dir)?;
        for _ in 0..8 {
            let name = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
            let candidate = parent.join(&name);
            match DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => {}
                Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(cause) => return Err(cause),
            }
            let handle = match open_directory(&candidate) {
                Ok(handle) => handle,
                Err(cause) => {
                    let _ = std::fs::remove_dir_all(&candidate);
                    return Err(cause);
                }
            };
            let temp = match super::platform::real_path(&handle) {
                Ok(path) => Self { path, handle },
                Err(cause) => {
                    let _ = std::fs::remove_dir_all(&candidate);
                    return Err(cause);
                }
            };
            return Ok(temp);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no unused temporary directory name",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn handle(&self) -> &File {
        &self.handle
    }

    /// Remove directories whose creating process no longer runs. A worker
    /// that crashed leaves its execution directories behind; directories of
    /// live workers sharing the private data directory are kept.
    pub(crate) fn sweep_stale(private_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(private_dir.join(PARENT)) else {
            return;
        };
        let own = std::process::id();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|name| name.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own || process_alive(pid) {
                continue;
            }
            remove(&entry.path());
        }
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Remove a temporary directory without following links in it. A command
/// may have removed the owner's permissions from directories it created, so
/// they are restored before a second attempt.
fn remove(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() {
        return;
    }
    restore_permissions(path);
    if let Err(cause) = std::fs::remove_dir_all(path) {
        log::warn!(
            "sandbox temporary directory not removed: kind={:?}",
            cause.kind()
        );
    }
}

fn restore_permissions(directory: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(directory.as_os_str().as_bytes()) else {
        return;
    };
    // Never follows a link that replaced the directory.
    if unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            path.as_ptr(),
            0o700,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                restore_permissions(&entry.path());
            }
        }
    }
}

impl Drop for PerExecTemp {
    fn drop(&mut self) {
        remove(&self.path);
    }
}
