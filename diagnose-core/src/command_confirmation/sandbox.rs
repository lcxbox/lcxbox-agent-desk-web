//! Resolution of an AI command's sandbox mode from the model's literal
//! `sandbox` choice, the classification, the write roots and the device's
//! reported sandbox support and execution identity.

use desk_agent_protocol::AgentError;
use desk_agent_protocol::computer_use::ComputerUseReadiness;
use desk_agent_protocol::exec::{
    ExecIdentity, ExecSandboxAttention, ExecSandboxImplicitWritable, ExecSandboxMode,
    ExecSandboxNotice, ExecSandboxSupport, ExecSandboxUnsupportedReason, ExecSnapshotRootLabel,
};

use super::denied;

/// The target worker's sandbox support and execution identity, as reported
/// in its current readiness. Built only from that readiness, so command
/// policy cannot be assembled without them; the worker probes both once at
/// start and a new worker has a new incarnation in `target_session_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceExecSandbox {
    support: ExecSandboxSupport,
    identity: ExecIdentity,
    os: String,
    user_home: Option<String>,
}

impl DeviceExecSandbox {
    pub fn from_readiness(readiness: &ComputerUseReadiness) -> Self {
        Self {
            support: readiness.exec_sandbox,
            identity: readiness.exec_identity,
            os: readiness.os.clone(),
            user_home: readiness.interactive_user_home.clone(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(support: ExecSandboxSupport, identity: ExecIdentity, os: &str) -> Self {
        Self {
            support,
            identity,
            os: os.into(),
            user_home: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_user_home(mut self, home: &str) -> Self {
        self.user_home = Some(home.into());
        self
    }

    /// The support the resolution uses: a SYSTEM or root identity cannot be
    /// sandboxed, whatever the reported reason.
    pub fn effective_support(&self) -> ExecSandboxSupport {
        if self.identity.is_privileged() {
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity)
        } else {
            self.support
        }
    }

    pub fn identity(&self) -> ExecIdentity {
        self.identity
    }

    fn platform(&self) -> &str {
        crate::request_devices::platform_name(&self.os).unwrap_or("this platform")
    }

    fn is_windows(&self) -> bool {
        self.os.eq_ignore_ascii_case("windows")
    }

    /// Why this device cannot sandbox a command, for the model and the owner.
    fn unsupported_reason(&self, reason: ExecSandboxUnsupportedReason) -> String {
        match reason {
            ExecSandboxUnsupportedReason::SystemIdentity => format!(
                "commands on this device run as {}, which the sandbox cannot restrict",
                identity_name(self.identity)
            ),
            ExecSandboxUnsupportedReason::NotImplemented => {
                format!(
                    "the command sandbox is not available on {}",
                    self.platform()
                )
            }
            ExecSandboxUnsupportedReason::KernelUnsupported => {
                match self.os.to_ascii_lowercase().as_str() {
                    "linux" => "the Linux kernel does not provide Landlock ABI 3 or later (kernel 6.2+ with Landlock enabled)".into(),
                    "macos" => "the macOS sandbox (/usr/bin/sandbox-exec) failed its startup check".into(),
                    "windows" => "the Windows sandbox (low-integrity launcher) failed its startup check".into(),
                    _ => format!("the operating system facility the sandbox needs is unavailable on {}", self.platform()),
                }
            }
        }
    }
}

/// The account name shown for an identity.
pub fn identity_name(identity: ExecIdentity) -> &'static str {
    if identity.system {
        "SYSTEM"
    } else if identity.root {
        "root"
    } else if identity.elevated {
        "an administrator"
    } else {
        "the signed-in user"
    }
}

/// Resolve how the command runs. `mutating` is the classification effect;
/// `elevated` an interactive elevation PTY run by the root daemon;
/// `has_roots` whether the mutating command has write roots (its `cwd`, or
/// else the session's approved directories on the device).
pub(super) fn resolve(
    device: &DeviceExecSandbox,
    requested: Option<bool>,
    mutating: bool,
    elevated: bool,
    has_roots: bool,
) -> Result<ExecSandboxNotice, AgentError> {
    let support = device.effective_support();
    let notice = |mode, attention| ExecSandboxNotice {
        mode,
        device_support: device.support,
        identity: device.identity,
        implicit_writable: if mode == ExecSandboxMode::Sandboxed {
            ExecSandboxImplicitWritable::for_os(&device.os)
        } else {
            Vec::new()
        },
        attention,
    };
    let unrestricted = |attention| Ok(notice(ExecSandboxMode::Unrestricted, attention));
    let sandboxed = || {
        Ok(notice(
            ExecSandboxMode::Sandboxed,
            ExecSandboxAttention::Sandboxed,
        ))
    };
    if elevated {
        return match requested {
            Some(true) => Err(denied(
                "sudo/doas cannot elevate inside the command sandbox; omit sandbox for an interactive elevation command",
            )),
            _ => unrestricted(ExecSandboxAttention::Neutral),
        };
    }
    match (requested, support) {
        (Some(true), ExecSandboxSupport::Unsupported(reason)) => Err(denied(format!(
            "sandbox=true cannot be honored: {}. Set sandbox to false to run this command without the sandbox and explain why in reason, or omit sandbox",
            device.unsupported_reason(reason)
        ))),
        (Some(true), ExecSandboxSupport::Supported) => sandboxed(),
        (Some(false), _) if mutating => unrestricted(ExecSandboxAttention::Prominent),
        (Some(false), _) => unrestricted(ExecSandboxAttention::Neutral),
        (None, ExecSandboxSupport::Supported) if mutating && has_roots => sandboxed(),
        (None, ExecSandboxSupport::Unsupported(_)) if mutating && has_roots => {
            unrestricted(ExecSandboxAttention::Prominent)
        }
        (None, _) => unrestricted(ExecSandboxAttention::Neutral),
    }
}

/// Advisory host checks of a sandboxed command's paths; the worker checks
/// the opened, canonical directories authoritatively.
pub(super) fn check_sandboxed_paths(
    device: &DeviceExecSandbox,
    cwd: Option<&str>,
    write_roots: &[ExecSnapshotRootLabel],
) -> Result<(), AgentError> {
    if let Some(cwd) = cwd
        && !is_absolute(cwd, device.is_windows())
    {
        return Err(denied(
            "cwd must be an absolute path for a command that runs in the sandbox; use an absolute cwd, or set sandbox to false and explain why in reason",
        ));
    }
    if let Some(root) = write_roots
        .iter()
        .find(|root| is_protected_write_root(device, &root.path))
    {
        return Err(denied(format!(
            "{} cannot be a writable sandbox directory: filesystem roots, the home directory and its ancestors, system directories and credential or autostart directories are refused. Set cwd to the narrower project directory the command changes, or set sandbox to false and explain why in reason",
            root.path
        )));
    }
    Ok(())
}

fn is_absolute(path: &str, windows: bool) -> bool {
    if windows {
        let bytes = path.as_bytes();
        (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
            || path.starts_with(r"\\")
    } else {
        path.starts_with('/')
    }
}

/// Lexically normalized, case-folded components (Windows and macOS file
/// systems are case-insensitive by default).
fn components(path: &str, os: &str) -> Vec<String> {
    let windows = os.eq_ignore_ascii_case("windows");
    let fold = windows || os.eq_ignore_ascii_case("macos");
    let mut parts: Vec<String> = Vec::new();
    for part in path.split(|c| c == '/' || (windows && c == '\\')) {
        match part {
            "" | "." => {}
            ".." => {
                // A Windows drive or UNC prefix stays put.
                if parts.len() > usize::from(windows) {
                    parts.pop();
                }
            }
            part if fold => parts.push(part.to_lowercase()),
            part => parts.push(part.to_owned()),
        }
    }
    parts
}

fn within(path: &[String], base: &[String]) -> bool {
    path.len() >= base.len() && path[..base.len()] == *base
}

fn system_directories(os: &str) -> &'static [&'static str] {
    match os.to_ascii_lowercase().as_str() {
        "linux" => &[
            "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/proc",
            "/run", "/sbin", "/sys", "/usr", "/var/lib", "/var/log",
        ],
        "macos" => &[
            "/Applications",
            "/Library",
            "/System",
            "/bin",
            "/dev",
            "/etc",
            "/private/etc",
            "/private/var/db",
            "/sbin",
            "/usr",
        ],
        "windows" => &[
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ],
        _ => &[],
    }
}

/// User-level persistence and credential directories under the home.
const HOME_PERSISTENCE: &[&str] = &[
    ".ssh",
    ".config/systemd/user",
    ".config/autostart",
    "Library/LaunchAgents",
];

fn is_protected_write_root(device: &DeviceExecSandbox, path: &str) -> bool {
    let os = device.os.as_str();
    let parts = components(path, os);
    // A filesystem root, or a Windows drive or share root.
    if parts.len() <= usize::from(device.is_windows()) {
        return true;
    }
    if system_directories(os)
        .iter()
        .any(|system| within(&parts, &components(system, os)))
    {
        return true;
    }
    let Some(home) = device
        .user_home
        .as_deref()
        .filter(|home| is_absolute(home, device.is_windows()))
    else {
        return false;
    };
    let home = components(home, os);
    within(&home, &parts)
        || HOME_PERSISTENCE.iter().any(|relative| {
            let mut protected = home.clone();
            protected.extend(components(relative, os));
            within(&parts, &protected) || within(&protected, &parts)
        })
}

#[cfg(test)]
mod tests;
