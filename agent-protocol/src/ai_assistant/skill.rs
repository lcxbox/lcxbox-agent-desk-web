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

/// Platform setting: names of bundled skills disabled for every user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct BundledSkillSettings {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub disabled: Vec<String>,
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
