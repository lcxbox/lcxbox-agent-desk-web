//! Directories a command snapshot never covers, checked on the canonical path
//! of an opened root: a filesystem or volume root, the user's home directory
//! itself, system directories and everything below them, and this product's
//! configuration, data and log directories together with their ancestors and
//! descendants.
//!
//! A sandboxed command's write root is refused on a wider list
//! ([`ProtectedRoots::refuses_write_root`]): additionally every ancestor of the
//! home directory, the user-level persistence and credential directories with
//! their ancestors and descendants, and every directory that equals or
//! contains a `PATH` entry of the worker.
use std::path::{Component, Path, PathBuf, Prefix};

#[derive(Clone, Debug, Default)]
pub(crate) struct ProtectedRoots {
    home: Option<Vec<String>>,
    system: Vec<Vec<String>>,
    product: Vec<Vec<String>>,
    /// User-level persistence and credential directories.
    persistence: Vec<Vec<String>>,
    /// Absolute `PATH` entries of the worker.
    executables: Vec<Vec<String>>,
}

/// Home-relative directories whose content another process later runs or
/// trusts: SSH keys and configuration, systemd user units, XDG autostart
/// entries and launchd agents.
const PERSISTENCE_DIRECTORIES: &[&str] = &[
    ".ssh",
    ".config/systemd/user",
    ".config/autostart",
    "Library/LaunchAgents",
];

/// Case-folded components; Windows drive and verbatim prefixes compare equal.
fn normalize(path: &Path) -> Vec<String> {
    let fold = |value: &std::ffi::OsStr| {
        let value = value.to_string_lossy();
        if cfg!(any(windows, target_os = "macos")) {
            value.to_lowercase()
        } else {
            value.into_owned()
        }
    };
    path.components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    format!("{}:", char::from(drive).to_ascii_lowercase())
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    format!("//{}/{}", fold(server), fold(share))
                }
                _ => fold(prefix.as_os_str()),
            }),
            Component::RootDir => Some("/".to_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_owned()),
            Component::Normal(part) => Some(fold(part)),
        })
        .collect()
}

/// The canonical form of `path`; a directory that does not exist yet is
/// placed under the canonical form of its nearest existing ancestor.
fn canonical(path: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            resolved.extend(missing.iter().rev());
            return normalize(&resolved);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return normalize(path),
        }
    }
}

fn within(path: &[String], base: &[String]) -> bool {
    path.len() >= base.len() && path[..base.len()] == *base
}

impl ProtectedRoots {
    pub(crate) fn new(
        home: Option<PathBuf>,
        system: impl IntoIterator<Item = PathBuf>,
        product: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let home = home.filter(|home| home.is_absolute());
        let persistence = home.iter().flat_map(|home| {
            PERSISTENCE_DIRECTORIES
                .iter()
                .map(move |relative| home.join(relative))
        });
        Self {
            persistence: absolute(persistence),
            home: home.map(|home| canonical(&home)),
            system: absolute(system),
            product: absolute(product),
            executables: Vec::new(),
        }
    }

    /// Add persistence directories outside the home-relative defaults, such
    /// as those below `XDG_CONFIG_HOME`.
    pub(crate) fn with_persistence(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.persistence.extend(absolute(paths));
        self
    }

    /// Protect the directories of a `PATH` value. Empty and relative entries
    /// (`.`, `::`) name no fixed directory and are ignored.
    pub(crate) fn with_executable_search_path(mut self, path: &std::ffi::OsStr) -> Self {
        self.executables
            .extend(absolute(std::env::split_paths(path)));
        self
    }

    /// The protected directories of this device: the current user's home
    /// and its persistence directories, the platform's system directories,
    /// `product` plus every default configuration, data and log location and
    /// the temporary-directory entries of this product, and the worker's
    /// `PATH`.
    pub(crate) fn current(product: impl IntoIterator<Item = PathBuf>) -> Self {
        use desk_utils::host_data_paths::{HostDataPaths, HostDataScope};
        let mut directories: Vec<PathBuf> = product.into_iter().collect();
        for scope in [
            HostDataScope::Machine,
            HostDataScope::System,
            HostDataScope::User,
        ] {
            if let Ok(paths) = HostDataPaths::resolve(scope, None) {
                directories.extend(paths.config_file().parent().map(Path::to_path_buf));
                directories.push(paths.data_root().to_path_buf());
                directories.push(paths.log_dir().to_path_buf());
                directories.extend(paths.runtime_root().map(Path::to_path_buf));
            }
        }
        directories.extend(temporary_product_entries());
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let protected = Self::new(
            crate::user_home::current().map(PathBuf::from),
            system_directories(),
            directories,
        )
        .with_persistence(
            xdg_config
                .iter()
                .flat_map(|config| [config.join("systemd/user"), config.join("autostart")]),
        );
        match std::env::var_os("PATH") {
            Some(path) => protected.with_executable_search_path(&path),
            None => protected,
        }
    }

    /// Whether the opened root at canonical `path` must not be snapshotted.
    pub(crate) fn refuses(&self, path: &Path) -> bool {
        let path = normalize(path);
        !path
            .iter()
            .any(|part| part != "/" && !part.ends_with(':') && !part.starts_with("//"))
            || self.home.as_ref() == Some(&path)
            || self.system.iter().any(|system| within(&path, system))
            || self
                .product
                .iter()
                .any(|product| within(&path, product) || within(product, &path))
    }

    /// Whether the opened root at canonical `path` must not become a write
    /// root of a sandboxed command.
    pub(crate) fn refuses_write_root(&self, path: &Path) -> bool {
        if self.refuses(path) {
            return true;
        }
        let path = normalize(path);
        self.home.as_ref().is_some_and(|home| within(home, &path))
            || self
                .persistence
                .iter()
                .any(|directory| within(&path, directory) || within(directory, &path))
            || self
                .executables
                .iter()
                .any(|directory| within(directory, &path))
    }
}

fn absolute(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Vec<String>> {
    paths
        .into_iter()
        .filter(|path| path.is_absolute())
        .map(|path| canonical(&path))
        .collect()
}

/// This product's files directly in the user's temporary directory: the
/// single-instance lock and the worker socket directory.
#[cfg(unix)]
fn temporary_product_entries() -> Vec<PathBuf> {
    let temp = std::env::temp_dir();
    let euid = unsafe { libc::geteuid() };
    vec![
        temp.join("lcxl_remote_desk_server.lock"),
        temp.join(format!("lcxl-rd-{euid}")),
    ]
}

#[cfg(not(unix))]
fn temporary_product_entries() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn system_directories() -> Vec<PathBuf> {
    [
        "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/proc", "/run",
        "/sbin", "/sys", "/usr", "/var/lib", "/var/log",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(target_os = "macos")]
fn system_directories() -> Vec<PathBuf> {
    [
        "/Applications",
        "/Library",
        "/System",
        "/bin",
        "/dev",
        "/etc",
        "/private/var/db",
        "/sbin",
        "/usr",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(windows)]
fn system_directories() -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect();
    directories.push(PathBuf::from(r"C:\Windows"));
    directories
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn system_directories() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn roots_homes_system_and_product_directories_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let product = home.path().join(".local/share/desk");
        std::fs::create_dir_all(product.join("recovery")).unwrap();
        std::fs::create_dir_all(home.path().join("code/repo")).unwrap();
        let protected = ProtectedRoots::new(
            Some(home.path().to_path_buf()),
            system_directories(),
            [product.clone()],
        );
        let at = |path: &Path| protected.refuses(&std::fs::canonicalize(path).unwrap());
        assert!(protected.refuses(Path::new("/")));
        assert!(at(home.path()), "the home directory itself");
        assert!(at(&product), "the product data directory");
        assert!(at(&product.join("recovery")), "below product data");
        assert!(at(&home.path().join(".local")), "above product data");
        assert!(at(Path::new("/etc")), "a system directory");
        assert!(at(Path::new("/usr/bin")), "below a system directory");
        assert!(!at(&home.path().join("code/repo")), "a project under home");
        assert!(!at(&home.path().join("code")));
        // A product directory that does not exist yet still protects the
        // canonical location it will have.
        let pending = ProtectedRoots::new(None, Vec::new(), [home.path().join("code/data")]);
        assert!(pending.refuses(&std::fs::canonicalize(home.path().join("code")).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn write_roots_also_refuse_home_ancestors_persistence_and_path_directories() {
        let home = tempfile::tempdir().unwrap();
        let home_path = std::fs::canonicalize(home.path()).unwrap();
        for directory in [".ssh/keys", ".config/autostart", "Library/LaunchAgents"] {
            std::fs::create_dir_all(home_path.join(directory)).unwrap();
        }
        std::fs::create_dir_all(home_path.join("tools/bin")).unwrap();
        std::fs::create_dir_all(home_path.join("code/repo")).unwrap();
        let search_path = std::env::join_paths([
            home_path.join("tools/bin"),
            PathBuf::from("."),
            PathBuf::from(""),
            PathBuf::from("relative/bin"),
        ])
        .unwrap();
        let protected = ProtectedRoots::new(Some(home_path.clone()), Vec::new(), Vec::new())
            .with_executable_search_path(&search_path);
        let refused = |relative: &str| protected.refuses_write_root(&home_path.join(relative));

        let parent = home_path.parent().unwrap();
        assert!(protected.refuses_write_root(parent), "an ancestor of home");
        assert!(
            !protected.refuses(parent),
            "snapshots keep their narrower list"
        );
        assert!(refused(".ssh"));
        assert!(refused(".ssh/keys"), "below a credential directory");
        assert!(refused(".config"), "above a persistence directory");
        assert!(refused(".config/autostart"));
        assert!(refused("Library"));
        assert!(refused("Library/LaunchAgents"));
        assert!(refused("tools/bin"), "a PATH directory");
        assert!(refused("tools"), "a directory containing a PATH directory");
        assert!(!refused("tools/bin/sub"), "below a PATH directory");
        assert!(!refused("code/repo"), "an ordinary project");
        assert!(!refused("code"));
        // `.`, empty and relative PATH entries do not protect the directory a
        // command happens to run in.
        let working = std::env::current_dir().unwrap();
        assert!(
            !ProtectedRoots::new(None, Vec::new(), Vec::new())
                .with_executable_search_path(&search_path)
                .refuses_write_root(&std::fs::canonicalize(home_path.join("code/repo")).unwrap()),
        );
        assert!(
            !ProtectedRoots::new(None, Vec::new(), Vec::new())
                .with_executable_search_path(std::ffi::OsStr::new(".::relative"))
                .refuses_write_root(&working.join("child-of-cwd")),
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_roots_refuse_xdg_persistence_and_temporary_product_entries() {
        let config = tempfile::tempdir().unwrap();
        let config_path = std::fs::canonicalize(config.path()).unwrap();
        let protected = ProtectedRoots::new(None, Vec::new(), temporary_product_entries())
            .with_persistence([config_path.join("systemd/user")]);
        assert!(protected.refuses_write_root(&config_path.join("systemd")));
        assert!(protected.refuses_write_root(&config_path.join("systemd/user/x")));
        assert!(!protected.refuses_write_root(&config_path.join("other")));
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            protected.refuses_write_root(&temp),
            "the temporary directory holds this product's lock and sockets"
        );
        assert!(!protected.refuses_write_root(&temp.join("some-project")));
    }

    #[test]
    fn components_compare_whole_names() {
        let base = normalize(Path::new("/data/desk"));
        assert!(within(&normalize(Path::new("/data/desk/x")), &base));
        assert!(!within(&normalize(Path::new("/data/desktop")), &base));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_fold_case_and_verbatim_prefixes() {
        assert_eq!(
            normalize(Path::new(r"\\?\C:\Users\Me")),
            normalize(Path::new(r"c:\users\me"))
        );
        let protected = ProtectedRoots::new(None, [PathBuf::from(r"C:\Windows")], []);
        assert!(protected.refuses(Path::new(r"\\?\C:\")));
        assert!(protected.refuses(Path::new(r"\\?\c:\windows\system32")));
        assert!(!protected.refuses(Path::new(r"\\?\C:\work\repo")));
    }
}
