//! In-place update of the Windows service (`--update-service`): reading the
//! installed command line, and replacing the installed files with a staged
//! swap that can be rolled back (plan
//! `2026-11-20_service-update-and-bfcache-signaling`, part A).

use std::io;
use std::path::{Path, PathBuf};

use clap::Parser as _;

use crate::model::settings::Args;

/// What the installed service runs: its executable and the explicit config
/// it was installed with, read exactly as the service itself reads them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InstalledCommand {
    pub exe: PathBuf,
    pub config_file_path: Option<PathBuf>,
}

/// Parse an already split command line (`argv[0]` is the executable).
pub(crate) fn installed_command(argv: &[String]) -> Option<InstalledCommand> {
    let exe = PathBuf::from(argv.first()?);
    let config_file_path = Args::try_parse_from(argv)
        .ok()
        .and_then(|args| args.config_file_path);
    Some(InstalledCommand {
        exe,
        config_file_path,
    })
}

/// Split a command line with the Windows rules the service's own process
/// uses (`CommandLineToArgvW`), so quoting and escaping match.
#[cfg(target_os = "windows")]
pub(crate) fn split_command_line(command_line: &str) -> Vec<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::UI::Shell::CommandLineToArgvW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
    let mut count = 0i32;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the returned
    // array is freed with LocalFree below.
    let argv = unsafe { CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count) };
    if argv.is_null() {
        return Vec::new();
    }
    let args = (0..count.max(0) as usize)
        .map(|index| {
            // SAFETY: CommandLineToArgvW returned `count` valid pointers.
            unsafe { (*argv.add(index)).to_string() }.unwrap_or_default()
        })
        .collect();
    // SAFETY: allocated by CommandLineToArgvW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(argv as *mut _)));
    }
    args
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn remove_path(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// One installed file or directory being replaced: the new copy is staged
/// at `<path>.new` while the service still runs, the old one is kept at
/// `<path>.old` until the new build has started.
#[derive(Debug)]
struct SwapItem {
    current: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    applied: bool,
    had_current: bool,
}

/// The files of one update.
#[derive(Debug, Default)]
pub(crate) struct StagedUpdate {
    items: Vec<SwapItem>,
}

impl StagedUpdate {
    /// Copy `src` next to `current` (as `<current>.new`); `current` is
    /// untouched. A leftover from an earlier attempt is replaced.
    pub(crate) fn stage(&mut self, src: &Path, current: &Path) -> io::Result<()> {
        let staged = with_suffix(current, ".new");
        remove_path(&staged)?;
        copy_recursive(src, &staged)?;
        self.items.push(SwapItem {
            current: current.to_path_buf(),
            backup: with_suffix(current, ".old"),
            staged,
            applied: false,
            had_current: false,
        });
        Ok(())
    }

    /// Move every staged copy into place, keeping the old one as `.old`.
    /// On a failure the items already moved are put back.
    pub(crate) fn apply(&mut self) -> io::Result<()> {
        for index in 0..self.items.len() {
            if let Err(error) = Self::apply_one(&mut self.items[index]) {
                let _ = self.rollback();
                return Err(error);
            }
        }
        Ok(())
    }

    fn apply_one(item: &mut SwapItem) -> io::Result<()> {
        remove_path(&item.backup)?;
        item.had_current = item.current.exists();
        if item.had_current {
            std::fs::rename(&item.current, &item.backup)?;
        }
        if let Err(error) = std::fs::rename(&item.staged, &item.current) {
            if item.had_current {
                let _ = std::fs::rename(&item.backup, &item.current);
            }
            return Err(error);
        }
        item.applied = true;
        Ok(())
    }

    /// Put the previous files back (after a failed start, or a failed
    /// apply). Best effort: every item is tried; the first error is
    /// returned.
    pub(crate) fn rollback(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for item in self.items.iter_mut().rev().filter(|item| item.applied) {
            let result = remove_path(&item.current).and_then(|()| {
                if item.had_current {
                    std::fs::rename(&item.backup, &item.current)
                } else {
                    Ok(())
                }
            });
            match result {
                Ok(()) => item.applied = false,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// The new build runs: drop the backups and any unused staged copy.
    pub(crate) fn commit(self) {
        for item in self.items {
            let _ = remove_path(&item.backup);
            let _ = remove_path(&item.staged);
        }
    }

    /// Nothing was applied (e.g. the service did not stop): drop the
    /// staged copies.
    pub(crate) fn discard(self) {
        for item in self.items.iter().filter(|item| !item.applied) {
            let _ = remove_path(&item.staged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn the_installed_config_is_read_like_the_service_reads_it() {
        let exe = r"C:\Program Files\LCXL Remote Desktop\lcxl-remote-desk-server.exe";
        for args in [
            argv(&[
                exe,
                "--startup-mode",
                "service-daemon",
                "--config-file-path",
                r"D:\t31\svc\config.toml",
            ]),
            argv(&[
                exe,
                "--startup-mode",
                "service-daemon",
                r"--config-file-path=D:\t31\svc\config.toml",
            ]),
            argv(&[
                exe,
                "-c",
                r"D:\t31\svc\config.toml",
                "--startup-mode",
                "service-daemon",
            ]),
            argv(&[
                exe,
                r"-cD:\t31\svc\config.toml",
                "--startup-mode",
                "service-daemon",
            ]),
        ] {
            let command = installed_command(&args).expect("parsed");
            assert_eq!(command.exe, PathBuf::from(exe));
            assert_eq!(
                command.config_file_path,
                Some(PathBuf::from(r"D:\t31\svc\config.toml")),
                "{args:?}"
            );
        }
        let none = installed_command(&argv(&[exe, "--startup-mode", "service-daemon"])).unwrap();
        assert_eq!(none.config_file_path, None);
        assert!(installed_command(&[]).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_command_line_is_split_with_windows_quoting() {
        let line = r#""C:\Program Files\LCXL Remote Desktop\lcxl-remote-desk-server.exe" --startup-mode service-daemon --config-file-path "D:\my data\config.toml""#;
        let command = installed_command(&split_command_line(line)).unwrap();
        assert_eq!(
            command.exe,
            PathBuf::from(r"C:\Program Files\LCXL Remote Desktop\lcxl-remote-desk-server.exe")
        );
        assert_eq!(
            command.config_file_path,
            Some(PathBuf::from(r"D:\my data\config.toml"))
        );
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// An install dir with an exe and a static dir, and a build dir with
    /// newer copies.
    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        let build = root.path().join("build");
        write(&install.join("server.exe"), "old exe");
        write(&install.join("static").join("index.html"), "old page");
        write(&build.join("server.exe"), "new exe");
        write(&build.join("static").join("index.html"), "new page");
        write(
            &build.join("static").join("assets").join("app.js"),
            "new js",
        );
        (root, install, build)
    }

    fn staged(install: &Path, build: &Path) -> StagedUpdate {
        let mut update = StagedUpdate::default();
        update
            .stage(&build.join("server.exe"), &install.join("server.exe"))
            .unwrap();
        update
            .stage(&build.join("static"), &install.join("static"))
            .unwrap();
        update
    }

    #[test]
    fn staging_leaves_the_running_files_alone_and_apply_swaps_them() {
        let (_root, install, build) = layout();
        let mut update = staged(&install, &build);
        assert_eq!(read(&install.join("server.exe")), "old exe");
        assert_eq!(read(&install.join("server.exe.new")), "new exe");

        update.apply().unwrap();
        assert_eq!(read(&install.join("server.exe")), "new exe");
        assert_eq!(
            read(&install.join("static").join("assets").join("app.js")),
            "new js"
        );
        assert_eq!(read(&install.join("server.exe.old")), "old exe");

        update.commit();
        assert!(!install.join("server.exe.old").exists());
        assert!(!install.join("static.old").exists());
        assert!(!install.join("static.new").exists());
    }

    #[test]
    fn a_rollback_brings_the_previous_build_back() {
        let (_root, install, build) = layout();
        let mut update = staged(&install, &build);
        update.apply().unwrap();
        update.rollback().unwrap();
        assert_eq!(read(&install.join("server.exe")), "old exe");
        assert_eq!(read(&install.join("static").join("index.html")), "old page");
        assert!(!install.join("static").join("assets").exists());
    }

    #[test]
    fn a_failed_apply_puts_back_what_it_already_moved() {
        let (_root, install, build) = layout();
        let mut update = staged(&install, &build);
        // The second item cannot be moved into place.
        std::fs::remove_dir_all(install.join("static.new")).unwrap();
        assert!(update.apply().is_err());
        assert_eq!(read(&install.join("server.exe")), "old exe");
        assert_eq!(read(&install.join("static").join("index.html")), "old page");
    }

    #[test]
    fn a_new_item_without_an_installed_counterpart_is_removed_on_rollback() {
        let (_root, install, build) = layout();
        write(&build.join("drivers").join("x.inf"), "driver");
        let mut update = staged(&install, &build);
        update
            .stage(&build.join("drivers"), &install.join("drivers"))
            .unwrap();
        update.apply().unwrap();
        assert!(install.join("drivers").join("x.inf").exists());
        update.rollback().unwrap();
        assert!(!install.join("drivers").exists());
    }

    #[test]
    fn discarding_an_unapplied_update_removes_the_staged_copies() {
        let (_root, install, build) = layout();
        let update = staged(&install, &build);
        update.discard();
        assert!(!install.join("server.exe.new").exists());
        assert!(!install.join("static.new").exists());
        assert_eq!(read(&install.join("server.exe")), "old exe");
    }
}
