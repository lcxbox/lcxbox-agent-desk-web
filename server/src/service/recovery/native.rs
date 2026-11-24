//! Interactive recovery uses a profile-specific native endpoint. No credentials
//! or authorization to reset an account are accepted over the signaling plane.
use super::Recovery;
use anyhow::{Context, Result, bail, ensure};
use desk_utils::host_data_paths::HostDataPaths;
use sha2::{Digest, Sha256};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
#[path = "native_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "native_windows.rs"]
mod platform;

fn identity(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest[..16].iter().map(|v| format!("{v:02x}")).collect()
}
async fn write_frame(s: &mut (impl AsyncWrite + Unpin), data: &str) -> Result<()> {
    ensure!(
        data.len() <= MAX_FRAME,
        "Password recovery input is too long"
    );
    s.write_u32_le(data.len() as u32).await?;
    s.write_all(data.as_bytes()).await?;
    s.flush().await?;
    Ok(())
}
const MAX_FRAME: usize = 128 * 1024;
async fn read_frame(s: &mut (impl AsyncRead + Unpin)) -> Result<String> {
    let n = s.read_u32_le().await? as usize;
    ensure!(n <= MAX_FRAME, "Invalid recovery frame length");
    let mut data = vec![0; n];
    s.read_exact(&mut data).await?;
    String::from_utf8(data).map_err(|_| anyhow::anyhow!("Invalid recovery input"))
}

async fn serve_peer(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    service: Arc<Recovery>,
    id: String,
) -> Result<()> {
    let account = service.account().await;
    if account.login_password_hash.is_none() {
        write_frame(&mut stream, "Account is not initialized").await?;
        return Ok(());
    }
    write_frame(&mut stream, &id).await?;
    let new_password =
        tokio::time::timeout(Duration::from_secs(300), read_frame(&mut stream)).await??;
    match service.reset_local(account, new_password).await {
        Ok(()) => write_frame(&mut stream, "OK").await,
        Err(error) => write_frame(&mut stream, &error.to_string()).await,
    }
}

pub(crate) fn start(service: Arc<Recovery>) {
    let result = service
        .path
        .canonicalize()
        .map_err(anyhow::Error::from)
        .and_then(|config| platform::start(&config, service));
    if let Err(error) = result {
        log::warn!("Local password reset command unavailable: {error}");
    }
}

fn read_hidden(input: &mut impl io::Read, prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    let mut value = Vec::new();
    loop {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        match byte[0] {
            b'\r' | b'\n' => break,
            3 | 4 => bail!("Password reset cancelled"),
            8 | 127 => {
                // Remove one complete UTF-8 character.
                while value.pop().is_some_and(|b| b & 0xc0 == 0x80) {}
            }
            21 => value.clear(),
            b => {
                ensure!(
                    value.len() < crate::service::password::MAX_PASSWORD_BYTES,
                    "Password is too long"
                );
                value.push(b);
            }
        }
    }
    eprintln!();
    String::from_utf8(value).map_err(|_| anyhow::anyhow!("Invalid password encoding"))
}

fn prompt_password() -> Result<String> {
    let _restore_terminal = platform::hide_input()?;
    let mut input = io::stdin().lock();
    let password = read_hidden(&mut input, "New password: ")?;
    ensure!(!password.is_empty(), "Password must not be empty");
    let confirm = read_hidden(&mut input, "Confirm new password: ")?;
    ensure!(confirm == password, "Passwords do not match");
    Ok(password)
}

pub fn run(config_override: Option<PathBuf>) -> Result<()> {
    ensure!(
        io::stdin().is_terminal(),
        "Password reset requires an interactive terminal (use ssh -t for SSH)"
    );
    let config = HostDataPaths::resolve_current(config_override.as_deref())?
        .config_file()
        .canonicalize()
        .context("Cannot locate the selected host configuration")?;
    eprintln!("Reset the local account for {}", config.display());
    let tty = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tty.block_on(async {
        let mut stream = platform::connect(&config)
            .await
            .context("Cannot connect to the running host for this configuration")?;
        let hello =
            tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stream)).await??;
        ensure!(
            hello != "Account is not initialized",
            "Account is not initialized"
        );
        ensure!(
            hello == identity(&config),
            "The selected host or account is unavailable"
        );
        let password = prompt_password()?;
        write_frame(&mut stream, &password).await?;
        let response = tokio::time::timeout(Duration::from_secs(90), read_frame(&mut stream))
            .await
            .context("Response timed out; check whether the new password was applied")?
            .context("Connection closed; check whether the new password was applied")?;
        if response != "OK" {
            bail!("Password reset: {response}")
        }
        println!("Password reset successfully.");
        Ok(())
    })
}
