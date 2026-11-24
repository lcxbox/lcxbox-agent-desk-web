use std::io;

#[cfg(unix)]
#[path = "permissions_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "permissions_windows.rs"]
mod platform;
pub(super) use platform::check;
#[cfg(windows)]
pub(super) use platform::owner;

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Restrict the configuration and its directory to the service account and local administrators before requesting a recovery code",
    )
}
