//! Per-execution temporary directories on Windows (plan 6.1, 6.5.3).
//!
//! They live below `<worker private dir>\exec-sandbox-tmp`, which carries a
//! long-lived inheritable Low label (no lease): it holds nothing but these
//! directories, and it is a sibling of the worker's other private data, which
//! the label never reaches. Each directory is named after the creating
//! process (pid and creation time), so a sweep removes only directories of
//! processes that no longer run, even after pid reuse.
use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::windows_label;
use super::windows_token::OwnedHandle;

const ROOT: &str = "exec-sandbox-tmp";

fn creation_time(process: windows::Win32::Foundation::HANDLE) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn own_tag() -> String {
    let created = creation_time(unsafe { GetCurrentProcess() }).unwrap_or(0);
    format!("{}-{created:x}", std::process::id())
}

/// Whether the process that created a directory tagged `pid-created` runs.
fn creator_alive(pid: u32, created: u64) -> bool {
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let process = OwnedHandle(process);
    creation_time(process.raw()) == Some(created)
}

/// The temporary root, created and labelled when needed.
fn root(private_dir: &Path) -> io::Result<PathBuf> {
    let root = private_dir.join(ROOT);
    std::fs::create_dir_all(&root)?;
    if !windows_label::has_root_label(&root) {
        let failures = windows_label::set_label(&root)?;
        if failures.count > 0 {
            return Err(io::Error::other(format!(
                "the sandbox temporary directory could not be labelled: {}",
                failures.sample.join("; ")
            )));
        }
    }
    Ok(root)
}

pub(crate) struct PerExecTemp {
    path: PathBuf,
}

impl PerExecTemp {
    pub(crate) fn create(private_dir: &Path) -> io::Result<Self> {
        let path = root(private_dir)?.join(format!(
            "exec-{}-{}",
            own_tag(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove directories left behind by processes that no longer run.
    pub(crate) fn sweep_stale(private_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(private_dir.join(ROOT)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = name.strip_prefix("exec-").unwrap_or("").split('-');
            let (Some(pid), Some(created)) = (
                parts.next().and_then(|p| p.parse::<u32>().ok()),
                parts.next().and_then(|c| u64::from_str_radix(c, 16).ok()),
            ) else {
                continue;
            };
            if !creator_alive(pid, created) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

impl Drop for PerExecTemp {
    fn drop(&mut self) {
        // `remove_dir_all` does not follow links out of the directory.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
