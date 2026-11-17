//! Platforms without a command sandbox backend: they report
//! `not_implemented`.
use std::fs::File;
use std::io;
use std::path::PathBuf;

use super::{ExecSandboxSupport, ExecSandboxUnsupportedReason};

pub(super) fn probe() -> ExecSandboxSupport {
    ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::NotImplemented)
}

pub(super) fn real_path(_handle: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "command sandbox is not implemented on this platform",
    ))
}

/// Never constructed: preparing a sandbox fails before it.
pub(super) enum Prepared {}

impl Prepared {
    pub(super) fn pipe_command(&self, _program: &str, _args: &[String]) -> tokio::process::Command {
        match *self {}
    }
}

pub(super) fn refused_program(_exit_code: Option<i32>, _stderr: &str) -> bool {
    false
}
