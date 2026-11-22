//! Private data directory of a user-session worker on Unix.
//!
//! A system daemon hands its session workers the user's data directory (for
//! example `$XDG_DATA_HOME/lcxbox-agent-desk`), which nothing else creates; the
//! recovery library and the other user stores create only their own entries
//! below it.

use std::{io, os::unix::fs::DirBuilderExt as _, path::Path};

/// Create the directory, private to the worker's user, when it is missing.
/// Missing parents are created with the default mode; an existing directory
/// keeps its mode.
pub(super) fn prepare(data_dir: &Path) -> io::Result<()> {
    if !data_dir.is_absolute() {
        return Err(io::Error::other("worker data root must be absolute"));
    }
    if let Some(parent) = data_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(data_dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_missing_data_directory_is_created_private_and_opens_the_recovery_library() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = home.path().join(".local/share/lcxbox-agent-desk");
        assert!(desk_file_recovery::Vault::open(&data_dir).is_err());

        prepare(&data_dir).unwrap();

        let mode = std::fs::metadata(&data_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        desk_file_recovery::Vault::open(&data_dir).unwrap();
    }

    #[test]
    fn an_existing_data_directory_keeps_its_mode_and_relative_paths_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = home.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o750)).unwrap();

        prepare(&data_dir).unwrap();

        let mode = std::fs::metadata(&data_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
        assert!(prepare(Path::new("relative/data")).is_err());
    }
}
