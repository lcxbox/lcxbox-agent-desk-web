//! Directories a command snapshot never covers, checked on the canonical path
//! of an opened root: a filesystem or volume root, the user's home directory
//! itself, system directories and everything below them, and this product's
//! configuration, data and log directories together with their ancestors and
//! descendants.
use std::path::{Component, Path, PathBuf, Prefix};

#[derive(Clone, Debug, Default)]
pub(crate) struct ProtectedRoots {
    home: Option<Vec<String>>,
    system: Vec<Vec<String>>,
    product: Vec<Vec<String>>,
}

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
        let absolute = |paths: &mut dyn Iterator<Item = PathBuf>| {
            paths
                .filter(|path| path.is_absolute())
                .map(|path| canonical(&path))
                .collect::<Vec<_>>()
        };
        Self {
            home: home
                .filter(|home| home.is_absolute())
                .map(|home| canonical(&home)),
            system: absolute(&mut system.into_iter()),
            product: absolute(&mut product.into_iter()),
        }
    }

    /// The protected directories of this device: the current user's home,
    /// the platform's system directories, and `product` plus every default
    /// configuration, data and log location of this product.
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
        Self::new(
            crate::user_home::current().map(PathBuf::from),
            system_directories(),
            directories,
        )
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
