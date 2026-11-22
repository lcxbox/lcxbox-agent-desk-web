//! Resource-limited child processes of the current executable.
//!
//! Some work can allocate, loop or recurse without bound on hostile input, and
//! a failed allocation or a stack overflow aborts the whole process. Such work
//! runs instead in a short-lived child started from the same executable with a
//! private first argument. The child:
//!
//! - is dispatched by [`run_child_if_requested`] before any runtime, logging or
//!   configuration starts, and writes nothing to stdout except one response
//!   frame;
//! - has its memory, CPU time and file-size limits applied before it reads its
//!   request (`setrlimit` in the forked child on Unix; a job object assigned
//!   before the request is written on Windows);
//! - is killed after a wall-clock deadline, and killed and reaped when the
//!   parent drops it for any reason.
//!
//! It runs with the host's own identity and talks only through its pipes. This
//! is a resource boundary, not a privilege boundary. macOS does not enforce an
//! address-space limit, so callers choose tighter CPU and wall-clock limits
//! there (see [`MEMORY_LIMIT_ENFORCED`]).
//!
//! A frame is a little-endian `u32` header length, a JSON header, a
//! little-endian `u32` payload length and the payload bytes. Both lengths are
//! bounded by the caller before any allocation.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// Whether the platform enforces [`SandboxSpec::memory_bytes`]. Callers pick
/// tighter time limits where it does not.
pub const MEMORY_LIMIT_ENFORCED: bool = platform::MEMORY_LIMIT_ENFORCED;

/// Private first argument that turns the executable into a sandbox child; the
/// second argument names the service.
pub const SUBCOMMAND: &str = "__lcxbox-sandbox";

/// Exit code of a child asked for a service its executable did not register.
/// Distinct from usage errors (2) and panics (101).
pub const UNKNOWN_SERVICE_EXIT_CODE: i32 = 86;

/// A named child-side handler: it reads one request frame from the input and
/// writes one response frame to the output.
pub type Service = (
    &'static str,
    fn(&mut dyn Read, &mut dyn Write) -> io::Result<()>,
);

/// Bounds of one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    pub header_bytes: usize,
    pub payload_bytes: usize,
}

/// Limits and environment of one child. There are no defaults: each caller
/// states the bounds of its own work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Address space (Unix) or committed memory (Windows) of the child.
    pub memory_bytes: u64,
    /// CPU time across all threads of the child.
    pub cpu_seconds: u64,
    /// Wall-clock time from spawn to response.
    pub wall_clock: Duration,
    /// Bounds of the request and the response frames.
    pub frames: FrameLimits,
    /// Extra environment variables of the child.
    pub env: &'static [(&'static str, &'static str)],
}

/// Why a sandboxed request produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxFailure {
    /// The child could not be started or contained, or its executable does
    /// not provide the service. Nothing ran.
    Unavailable(String),
    /// The wall-clock deadline passed; the child was killed.
    Timeout,
    /// The child exited without a response: a resource limit, a crash or a
    /// stack overflow. `None` when the status could not be read.
    Exited(Option<ExitStatus>),
    /// A frame was malformed or exceeded its bounds.
    Protocol(String),
}

/// Serves one request and returns the process exit code when the executable
/// was started as a sandbox child; returns `None` otherwise. Call it first in
/// `main`, before anything can write to stdout.
pub fn run_child_if_requested(services: &[Service]) -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(OsStr::new(SUBCOMMAND)) {
        return None;
    }
    let name = args.next();
    let service = services
        .iter()
        .find(|(service, _)| name.as_deref() == Some(OsStr::new(service)));
    let Some((_, serve)) = service.filter(|_| args.next().is_none()) else {
        return Some(UNKNOWN_SERVICE_EXIT_CODE);
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = serve(&mut stdin.lock(), &mut stdout.lock());
    Some(if result.is_ok() { 0 } else { 1 })
}

pub fn write_frame(
    output: &mut dyn Write,
    header: &impl Serialize,
    payload: &[u8],
    limits: FrameLimits,
) -> io::Result<()> {
    let header = serde_json::to_vec(header).map_err(io::Error::other)?;
    if header.len() > limits.header_bytes || payload.len() > limits.payload_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sandbox frame exceeds its limit",
        ));
    }
    output.write_all(&(header.len() as u32).to_le_bytes())?;
    output.write_all(&header)?;
    output.write_all(&(payload.len() as u32).to_le_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

pub fn read_frame<T: DeserializeOwned>(
    input: &mut dyn Read,
    limits: FrameLimits,
) -> io::Result<(T, Vec<u8>)> {
    let header = read_bounded(input, limits.header_bytes)?;
    let header = serde_json::from_slice(&header)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let payload = read_bounded(input, limits.payload_bytes)?;
    Ok((header, payload))
}

fn read_bounded(input: &mut dyn Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sandbox frame exceeds its limit",
        ));
    }
    let mut bytes = vec![0_u8; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Runs `service` of the current executable on one request.
pub fn run<Request: Serialize, Response: DeserializeOwned + Send + 'static>(
    service: &str,
    request: &Request,
    payload: &[u8],
    spec: &SandboxSpec,
) -> Result<(Response, Vec<u8>), SandboxFailure> {
    let program =
        std::env::current_exe().map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    run_program(&program, service, request, payload, spec)
}

/// Kills and reaps the child on every exit path, including unwinding.
struct ChildGuard {
    child: Child,
    _containment: platform::Containment,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `service` of `program` on one request. [`run`] uses the current
/// executable; tests point this at a built host binary.
pub fn run_program<Request: Serialize, Response: DeserializeOwned + Send + 'static>(
    program: &Path,
    service: &str,
    request: &Request,
    payload: &[u8],
    spec: &SandboxSpec,
) -> Result<(Response, Vec<u8>), SandboxFailure> {
    let started = Instant::now();
    let mut frame = Vec::new();
    write_frame(&mut frame, request, payload, spec.frames)
        .map_err(|error| SandboxFailure::Protocol(error.to_string()))?;
    let mut command = Command::new(program);
    command
        .args([SUBCOMMAND, service])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Fewer allocator arenas keep the reserved address space well below
        // the memory limit for legitimate work.
        .env("MALLOC_ARENA_MAX", "2")
        .envs(spec.env.iter().copied());
    platform::before_spawn(&mut command, spec);
    let mut child = command
        .spawn()
        .map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    let containment = match platform::after_spawn(&child, spec) {
        Ok(containment) => containment,
        Err(detail) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SandboxFailure::Unavailable(detail));
        }
    };
    let mut guard = ChildGuard {
        child,
        _containment: containment,
    };
    let mut stdin = guard
        .child
        .stdin
        .take()
        .ok_or_else(|| SandboxFailure::Unavailable("child stdin is unavailable".into()))?;
    let stdout = guard
        .child
        .stdout
        .take()
        .ok_or_else(|| SandboxFailure::Unavailable("child stdout is unavailable".into()))?;
    let limits = spec.frames;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("process-sandbox-reader".into())
        .spawn(move || {
            let mut stdout = stdout;
            let _ = sender.send(read_frame::<Response>(&mut stdout, limits));
        })
        .map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    // The limits are already in force: the child blocks on this frame before
    // it does anything. A child that died early closes the pipe.
    let written = stdin.write_all(&frame).and_then(|()| stdin.flush());
    drop(stdin);
    let remaining = spec.wall_clock.saturating_sub(started.elapsed());
    match receiver.recv_timeout(remaining) {
        // Dropping the guard kills the child if it is still running.
        Ok(Ok(frame)) => Ok(frame),
        Ok(Err(error)) => {
            let status = guard.child.wait().ok();
            if status.and_then(|status| status.code()) == Some(UNKNOWN_SERVICE_EXIT_CODE) {
                Err(SandboxFailure::Unavailable(format!(
                    "the executable does not provide the {service} sandbox service"
                )))
            } else if written.is_err() || status.is_none_or(|status| !status.success()) {
                Err(SandboxFailure::Exited(status))
            } else {
                Err(SandboxFailure::Protocol(error.to_string()))
            }
        }
        Err(_) => Err(SandboxFailure::Timeout),
    }
}
