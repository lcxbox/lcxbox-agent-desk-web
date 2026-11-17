//! Write sandbox of an AI command.
//!
//! The trusted central resolves how an AI `exec_command` runs from the
//! model's literal `sandbox` argument, the command classification, the write
//! roots (the snapshot roots of a mutating command) and the device's
//! reported sandbox support and execution identity. The result is sealed as
//! [`ExecSandboxMode`] into [`crate::ExecInput`], [`super::ExecPlanDraft`],
//! [`super::ExecPlan`] and the plan fingerprint. A sandboxed command may write
//! only its write roots, a per-execution temporary directory and the
//! [`ExecSandboxImplicitWritable`] locations of its platform; reads and the
//! network are not restricted. Non-AI executions (fleet, local and browser
//! `ConfirmExec`, Manager PTY proxy) always carry
//! [`ExecSandboxMode::Unrestricted`].
//!
//! None of these types has a serde default: a missing value must never decode
//! as the unrestricted mode or as an unprivileged identity.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// How the device runs a sealed command.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSandboxMode {
    /// No write restriction beyond the executing account's own permissions.
    Unrestricted,
    /// Writes are limited to the plan's snapshot roots, the per-execution
    /// temporary directory and the platform's implicit writable locations.
    Sandboxed,
}

impl ExecSandboxMode {
    /// Fingerprint contribution; `None` keeps an unrestricted plan's
    /// fingerprint unchanged. Kept explicit so reordering variants never
    /// shifts an approved plan's fingerprint silently.
    pub(crate) fn fingerprint_byte(self) -> Option<u8> {
        match self {
            Self::Unrestricted => None,
            Self::Sandboxed => Some(1),
        }
    }
}

/// Why a device cannot run a sandboxed command.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSandboxUnsupportedReason {
    /// The platform has no sandbox implementation.
    NotImplemented,
    /// The kernel or OS facility the sandbox needs is missing or blocked
    /// (Linux Landlock below ABI 3, macOS `sandbox-exec` probe failure).
    KernelUnsupported,
    /// The worker runs as SYSTEM or with effective uid 0; a sandbox cannot
    /// restrict that identity.
    SystemIdentity,
}

/// The worker's sandbox capability, probed once when the worker starts and
/// constant for its lifetime.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum ExecSandboxSupport {
    Supported,
    Unsupported(ExecSandboxUnsupportedReason),
}

/// The account a worker runs commands as, reported with its readiness and
/// constant for the worker's lifetime.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
pub struct ExecIdentity {
    /// A Windows elevated (administrator) token.
    pub elevated: bool,
    /// The Windows SYSTEM account.
    pub system: bool,
    /// Effective uid 0 on Linux or macOS.
    pub root: bool,
}

impl ExecIdentity {
    /// SYSTEM or root: no sandbox can be applied to this identity.
    pub fn is_privileged(self) -> bool {
        self.system || self.root
    }

    /// Whether an unrestricted command card must show the identity.
    pub fn is_notable(self) -> bool {
        self.elevated || self.is_privileged()
    }
}

/// Locations a sandboxed command may write besides its write roots.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSandboxImplicitWritable {
    /// A fresh per-execution temporary directory exported as `TMPDIR`,
    /// `TEMP` and `TMP`, removed after the command.
    ExecTempDir,
    /// macOS: the user temporary directory (`/var/folders/…/T`), except this
    /// product's own files in it.
    UserTempDir,
    /// Linux: `/dev/shm`.
    DevShm,
}

impl ExecSandboxImplicitWritable {
    /// The implicit writable locations on a device reporting `os` (the
    /// readiness `os` value: `linux`, `macos`, `windows`).
    pub fn for_os(os: &str) -> Vec<Self> {
        let mut locations = vec![Self::ExecTempDir];
        match os.to_ascii_lowercase().as_str() {
            "macos" => locations.push(Self::UserTempDir),
            "linux" => locations.push(Self::DevShm),
            _ => {}
        }
        locations
    }
}

/// How an approval card marks a command's execution mode.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecSandboxAttention {
    /// The command runs in the sandbox; the card lists its writable locations.
    Sandboxed,
    /// Unrestricted without a special warning: a read-only template, a
    /// mutating command without write roots, or an interactive elevation.
    Neutral,
    /// Unrestricted mutating command that could have been sandboxed or was
    /// explicitly taken out of it: changes outside the roots cannot be undone.
    Prominent,
}

/// The frozen execution-mode facts of one AI command confirmation: what the
/// owner card and the AI review show, and part of the whole-equality
/// revalidation before dispatch.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ExecSandboxNotice {
    pub mode: ExecSandboxMode,
    /// The device's reported support when the plan was resolved.
    pub device_support: ExecSandboxSupport,
    /// The account the command runs as.
    pub identity: ExecIdentity,
    /// Writable locations besides the write roots; empty when unrestricted.
    pub implicit_writable: Vec<ExecSandboxImplicitWritable>,
    pub attention: ExecSandboxAttention,
}

impl ExecSandboxNotice {
    /// A mutating command that runs outside the sandbox where the owner must
    /// be warned prominently; the AI review receives this as `unsandboxed`.
    pub fn unsandboxed(&self) -> bool {
        self.attention == ExecSandboxAttention::Prominent
    }

    /// The identity a card must show: an unrestricted command running as an
    /// administrator, SYSTEM or root.
    pub fn displayed_identity(&self) -> Option<ExecIdentity> {
        (self.mode == ExecSandboxMode::Unrestricted && self.identity.is_notable())
            .then_some(self.identity)
    }

    /// Internal consistency of a frozen notice.
    pub fn is_consistent(&self) -> bool {
        match self.mode {
            ExecSandboxMode::Sandboxed => {
                self.attention == ExecSandboxAttention::Sandboxed
                    && self.device_support == ExecSandboxSupport::Supported
                    && !self.identity.is_privileged()
                    && self.implicit_writable.first()
                        == Some(&ExecSandboxImplicitWritable::ExecTempDir)
            }
            ExecSandboxMode::Unrestricted => {
                self.attention != ExecSandboxAttention::Sandboxed
                    && self.implicit_writable.is_empty()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes_have_no_defaults() {
        assert_eq!(
            serde_json::to_value(ExecSandboxSupport::Unsupported(
                ExecSandboxUnsupportedReason::KernelUnsupported
            ))
            .unwrap(),
            serde_json::json!({"state":"unsupported","reason":"kernel_unsupported"})
        );
        assert_eq!(
            serde_json::to_value(ExecSandboxSupport::Supported).unwrap(),
            serde_json::json!({"state":"supported"})
        );
        // A missing identity flag never decodes as unprivileged.
        assert!(
            serde_json::from_value::<ExecIdentity>(serde_json::json!({"elevated":false})).is_err()
        );
        assert!(serde_json::from_value::<ExecSandboxMode>(serde_json::json!(null)).is_err());
    }

    #[test]
    fn implicit_locations_follow_the_platform() {
        use ExecSandboxImplicitWritable::*;
        assert_eq!(
            ExecSandboxImplicitWritable::for_os("macos"),
            [ExecTempDir, UserTempDir]
        );
        assert_eq!(
            ExecSandboxImplicitWritable::for_os("linux"),
            [ExecTempDir, DevShm]
        );
        assert_eq!(
            ExecSandboxImplicitWritable::for_os("windows"),
            [ExecTempDir]
        );
    }

    #[test]
    fn notice_consistency_and_display() {
        let identity = ExecIdentity {
            elevated: true,
            system: false,
            root: false,
        };
        let mut notice = ExecSandboxNotice {
            mode: ExecSandboxMode::Sandboxed,
            device_support: ExecSandboxSupport::Supported,
            identity,
            implicit_writable: ExecSandboxImplicitWritable::for_os("windows"),
            attention: ExecSandboxAttention::Sandboxed,
        };
        assert!(notice.is_consistent());
        assert!(!notice.unsandboxed());
        // An administrator is shown only on an unrestricted card.
        assert_eq!(notice.displayed_identity(), None);
        notice.identity.root = true;
        assert!(!notice.is_consistent(), "root cannot be sandboxed");
        notice.mode = ExecSandboxMode::Unrestricted;
        notice.attention = ExecSandboxAttention::Prominent;
        assert!(
            !notice.is_consistent(),
            "no writable list when unrestricted"
        );
        notice.implicit_writable.clear();
        assert!(notice.is_consistent());
        assert!(notice.unsandboxed());
        assert_eq!(notice.displayed_identity(), Some(notice.identity));
    }
}
