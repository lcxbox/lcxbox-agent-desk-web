use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
};
use tokio::net::{UnixListener, UnixStream};

fn socket_path(config: &Path) -> Result<PathBuf> {
    let owner = fs::metadata(config)?.uid();
    let parent = config
        .parent()
        .context("Missing recovery endpoint directory")?;
    let name = format!(".account-{}.sock", identity(config));
    let path = parent.join(&name);
    if path.as_os_str().as_encoded_bytes().len() < 104 && trusted_directory(parent, owner).is_ok() {
        return Ok(path);
    }
    // The account database is independent of SSH sessions and client overrides.
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut entry = std::mem::MaybeUninit::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
    let status = unsafe {
        libc::getpwuid_r(
            owner,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        status == 0 && !result.is_null(),
        "Cannot locate the account's home directory"
    );
    let home = unsafe { CStr::from_ptr((*result).pw_dir) };
    let dir = Path::new(std::ffi::OsStr::from_bytes(home.to_bytes())).join(".lcxbox-account");
    Ok(dir.join(name))
}
fn trusted_directory(dir: &Path, expected: u32) -> Result<()> {
    let stat = fs::symlink_metadata(dir)?;
    ensure!(
        stat.is_dir() && (stat.uid() == 0 || stat.uid() == expected) && stat.mode() & 0o077 == 0,
        "Untrusted recovery socket directory"
    );
    Ok(())
}
pub(super) fn start(config: &Path, service: Arc<Recovery>) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let socket = socket_path(config)?;
    let dir = socket.parent().context("Missing socket directory")?;
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    trusted_directory(dir, uid)?;
    if let Ok(metadata) = fs::symlink_metadata(&socket) {
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == uid,
            "Untrusted recovery socket"
        );
        match std::os::unix::net::UnixStream::connect(&socket) {
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(&socket)?,
            _ => bail!("Password recovery endpoint already in use"),
        }
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let id = identity(config);
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(peer) = stream.peer_cred() else {
                continue;
            };
            if peer.uid() != 0 && peer.uid() != uid {
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let (service, id) = (service.clone(), id.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let _ = serve_peer(stream, service, id).await;
            });
        }
    });
    Ok(())
}
pub(super) async fn connect(config: &Path) -> Result<UnixStream> {
    let owner = fs::metadata(config)?.uid();
    let socket = socket_path(config)?;
    trusted_directory(socket.parent().context("Missing socket directory")?, owner)?;
    let stream = UnixStream::connect(socket).await?;
    let peer = stream.peer_cred()?.uid();
    ensure!(
        peer == 0 || peer == owner,
        "Unexpected password recovery service identity"
    );
    Ok(stream)
}

pub(super) struct TerminalMode(libc::termios);
impl Drop for TerminalMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

pub(super) fn hide_input() -> io::Result<TerminalMode> {
    let mut mode = std::mem::MaybeUninit::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, mode.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let original = TerminalMode(unsafe { mode.assume_init() });
    let mut hidden = original.0;
    hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    hidden.c_cc[libc::VMIN] = 1;
    hidden.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(original)
}
