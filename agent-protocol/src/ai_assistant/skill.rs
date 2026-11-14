//! Agent Skills (`SKILL.md` packages) managed by a platform administrator or by
//! the owner. A skill is guidance text; it never grants tool or device authority.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Who manages a stored skill and which work may see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    /// Visible to all work on the deployment, including external-source work.
    Platform,
    /// Visible only to work the owner assigned directly.
    Personal,
}

/// How a stored version was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    Imported,
    Edited,
    AiAuthored,
}

/// Display classification of a package file, from its top-level directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillResourceKind {
    Script,
    Reference,
    Asset,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillFileInput {
    pub path: String,
    pub text: String,
}

/// One package upload: exactly one of `zip_base64` or `skill_md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillPackageInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zip_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_md: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SkillFileInput>,
}

/// A manual save of a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillManualOrigin {
    Imported,
    Edited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCreate {
    pub package: SkillPackageInput,
    /// The AI security scan shown before saving; recorded on the new version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_id: Option<String>,
}

/// Saves a new version, changes `enabled`, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillUpdate {
    pub expected_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<SkillPackageInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<SkillManualOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_id: Option<String>,
}

/// Validates an upload without saving it; optionally runs the AI security scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillPreviewRequest {
    pub package: SkillPackageInput,
    #[serde(default)]
    pub ai_scan: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillScanVerdict {
    Clean,
    Suspicious,
    /// The scan did not finish or returned an unusable answer; not a safe result.
    Failed,
    /// No approval model is configured or available.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillScanFinding {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub category: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillScanResult {
    pub verdict: SkillScanVerdict,
    pub findings: Vec<SkillScanFinding>,
    /// Files that could not be scanned within the batch limit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unscanned: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillSkipReason {
    Binary,
    TooLarge,
    InvalidPath,
    TooManyFiles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillSkippedFile {
    pub path: String,
    pub reason: SkillSkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillWarning {
    /// The body is longer than the 500 lines the open standard recommends.
    LongBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillFileMeta {
    pub path: String,
    pub kind: SkillResourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillFileView {
    #[serde(flatten)]
    pub meta: SkillFileMeta,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillDecisionSource {
    AiReview,
    Owner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub skill_id: String,
    pub scope: SkillScope,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub current_version: u64,
    pub revision: u64,
    pub origin: SkillOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_source: Option<SkillDecisionSource>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillVersionInfo {
    pub version: u64,
    pub origin: SkillOrigin,
    pub size_bytes: u64,
    pub created_at_ms: i64,
    /// The AI security scan the uploader saw before saving this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<SkillScanResult>,
}

/// The content of one retained version and its changes to the current version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillVersionView {
    pub info: SkillVersionInfo,
    pub skill_md: String,
    pub files: Vec<SkillFileView>,
    /// Changes from this version to the current version.
    pub diff_to_current: Vec<SkillFileDiff>,
}

/// Whether uploads can be scanned now: an approval model is configured and ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillScanAvailability {
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillDetail {
    pub summary: SkillSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,
    /// The canonical `SKILL.md` of the current version, as edited online.
    pub skill_md: String,
    pub files: Vec<SkillFileView>,
    pub versions: Vec<SkillVersionInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillDiffOp {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillDiffLine {
    pub op: SkillDiffOp,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillDiffHunk {
    /// 1-based first line in the old text; 0 when the file is new.
    pub old_start: u32,
    /// 1-based first line in the new text; 0 when the file is removed.
    pub new_start: u32,
    pub lines: Vec<SkillDiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillFileChange {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillFileDiff {
    pub path: String,
    pub change: SkillFileChange,
    pub hunks: Vec<SkillDiffHunk>,
    /// Hunks were cut at the output limit.
    pub truncated: bool,
}

/// Validation result and changes of a package that was not saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillPreview {
    pub name: String,
    pub files: Vec<SkillFileMeta>,
    pub skipped: Vec<SkillSkippedFile>,
    pub warnings: Vec<SkillWarning>,
    pub diff: Vec<SkillFileDiff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<SkillScanResult>,
    /// Pass back on save to record the scan on the new version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillSaved {
    pub skill: SkillSummary,
    pub files: Vec<SkillFileMeta>,
    pub skipped: Vec<SkillSkippedFile>,
    pub warnings: Vec<SkillWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundledSkillView {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub skill_md: String,
    pub files: Vec<SkillFileView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundledSkillList {
    /// Revision of the platform's disabled list.
    pub revision: u64,
    pub skills: Vec<BundledSkillView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundledSkillToggle {
    pub expected_revision: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct SkillDeleteQuery {
    pub expected_revision: u64,
}

/// Platform setting: names of bundled skills disabled for every user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct BundledSkillSettings {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillProposalKind {
    Create,
    Update,
    /// Disables an AI-authored personal skill; the owner can enable it again.
    Disable,
    /// Deletes an AI-authored personal skill with its retained versions.
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillProposalStatus {
    Pending,
    Approved,
    Rejected,
    /// Withdrawn when the owner stopped the conversation that paused on it.
    Withdrawn,
    /// The skill changed, or a limit was reached, before approval.
    Conflicted,
}

/// A model-authored skill proposal shown in the conversation and the owner's
/// pending list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillProposalView {
    pub proposal_id: String,
    pub conversation_id: String,
    pub kind: SkillProposalKind,
    pub name: String,
    pub reason: String,
    pub status: SkillProposalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_source: Option<SkillDecisionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_skill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u64>,
    /// The proposed `SKILL.md`.
    pub skill_md: String,
    pub files: Vec<SkillFileInput>,
    /// Changes from the base version; empty for a new skill.
    pub diff: Vec<SkillFileDiff>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at_ms: Option<i64>,
    pub revision: u64,
    /// Whether the owner must decide this pending proposal now: manual
    /// approval, the AI review window ended or produced no verdict, or the
    /// approval model is unavailable. Computed by the host.
    #[serde(default)]
    pub awaiting_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillProposalDecision {
    pub proposal_id: String,
    pub approve: bool,
    pub expected_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_input_accepts_text_upload_and_rejects_unknown_fields() {
        let input: SkillPackageInput = serde_json::from_value(serde_json::json!({
            "skillMd": "---\nname: a\ndescription: b\n---\nbody",
            "files": [{"path": "references/a.md", "text": "x"}]
        }))
        .unwrap();
        assert_eq!(input.files.len(), 1);
        assert!(
            serde_json::from_value::<SkillPackageInput>(serde_json::json!({"other": 1})).is_err()
        );
    }

    #[test]
    fn bundled_settings_default_to_nothing_disabled() {
        let settings: BundledSkillSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, BundledSkillSettings::default());
    }
}
