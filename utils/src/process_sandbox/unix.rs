//! Unix limits, applied in the forked child before `exec`, so they are in
//! force before the child reads its request.
use super::SandboxSpec;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// macOS accepts but does not enforce an address-space limit.
pub const MEMORY_LIMIT_ENFORCED: bool = cfg!(target_os = "linux");

/// Unix limits live in the child itself; nothing is held by the parent.
pub struct Containment;

pub fn before_spawn(command: &mut Command, spec: &SandboxSpec) {
    let (cpu_seconds, memory_bytes) = (spec.cpu_seconds, spec.memory_bytes);
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on plain integers.
    unsafe {
        command.pre_exec(move || {
            limit(libc::RLIMIT_CPU, cpu_seconds)?;
            // No core dumps and no regular-file writes: the child only uses its
            // pipes.
            limit(libc::RLIMIT_CORE, 0)?;
            limit(libc::RLIMIT_FSIZE, 0)?;
            let memory = limit(libc::RLIMIT_AS, memory_bytes);
            if MEMORY_LIMIT_ENFORCED {
                memory?;
            }
            Ok(())
        });
    }
}

pub fn after_spawn(_child: &Child, _spec: &SandboxSpec) -> Result<Containment, String> {
    Ok(Containment)
}

#[allow(clippy::unnecessary_cast)]
fn limit(resource: LimitResource, value: u64) -> std::io::Result<()> {
    let value = value as libc::rlim_t;
    let bound = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `bound` is a valid, initialized rlimit for the duration of the call.
    if unsafe { libc::setrlimit(resource, &bound) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
type LimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_os = "linux"))]
type LimitResource = libc::c_int;
