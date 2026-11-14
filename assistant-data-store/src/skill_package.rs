//! Validation of Agent Skills packages (`SKILL.md` plus text files) before storage.
//! Every upload, online edit and model proposal goes through this one parser.

mod archive;
pub mod diff;
mod frontmatter;

use desk_agent_protocol::ai_assistant::skill::{
    SkillFileMeta, SkillPackageInput, SkillResourceKind, SkillSkipReason, SkillSkippedFile,
    SkillWarning,
};
use std::collections::BTreeMap;

pub const MAX_FRONTMATTER_BYTES: usize = 8 * 1024;
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_COMPATIBILITY_CHARS: usize = 500;
pub const MAX_METADATA_ENTRIES: usize = 32;
pub const MAX_METADATA_KEY_CHARS: usize = 64;
pub const MAX_METADATA_VALUE_CHARS: usize = 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_RESOURCE_BYTES: usize = 64 * 1024;
pub const MAX_RESOURCES: usize = 64;
pub const MAX_ZIP_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_UNPACKED_BYTES: usize = 1024 * 1024;
const LONG_BODY_LINES: usize = 500;
const SKILL_FILE: &str = "SKILL.md";

/// One stored text file of a package.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageResource {
    pub path: String,
    pub kind: SkillResourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub text: String,
}

impl PackageResource {
    pub fn meta(&self) -> SkillFileMeta {
        SkillFileMeta {
            path: self.path.clone(),
            kind: self.kind,
            language: self.language.clone(),
            size_bytes: self.text.len() as u64,
        }
    }
}

/// A validated package, ready to be stored as one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    pub name: String,
    pub description: String,
    pub compatibility: Option<String>,
    pub license: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
    pub body: String,
    pub resources: Vec<PackageResource>,
}

impl SkillPackage {
    pub fn size_bytes(&self) -> u64 {
        (self.body.len() + self.resources.iter().map(|r| r.text.len()).sum::<usize>()) as u64
    }

    pub fn file_metas(&self) -> Vec<SkillFileMeta> {
        self.resources.iter().map(PackageResource::meta).collect()
    }

    /// The canonical `SKILL.md` used for display, online editing and diffs.
    pub fn render_skill_md(&self) -> String {
        frontmatter::render(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPackage {
    pub package: SkillPackage,
    pub skipped: Vec<SkillSkippedFile>,
    pub warnings: Vec<SkillWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    /// Exactly one of a zip archive or a `SKILL.md` text is required.
    MissingInput,
    InvalidZip,
    ZipTooLarge,
    UnpackedTooLarge,
    MissingSkillFile,
    InvalidFrontmatter(&'static str),
    InvalidName,
    ReservedName,
    NameMismatch,
    InvalidField(&'static str),
    BodyEmpty,
    BodyTooLarge,
    /// The file (or `SKILL.md`) contains a credential.
    Credentials(String),
    CredentialCheckUnavailable,
}

impl PackageError {
    pub fn message(&self) -> String {
        match self {
            Self::MissingInput => "provide either a zip archive or SKILL.md text".into(),
            Self::InvalidZip => "the zip archive could not be read".into(),
            Self::ZipTooLarge => format!("the zip archive exceeds {MAX_ZIP_BYTES} bytes"),
            Self::UnpackedTooLarge => {
                format!("the unpacked package exceeds {MAX_UNPACKED_BYTES} bytes")
            }
            Self::MissingSkillFile => {
                "SKILL.md must be at the archive root or in its only top-level directory".into()
            }
            Self::InvalidFrontmatter(reason) => format!("invalid SKILL.md frontmatter: {reason}"),
            Self::InvalidName => format!(
                "name must be 1-{MAX_NAME_CHARS} lowercase letters, digits or single hyphens"
            ),
            Self::ReservedName => "the name is reserved for a bundled skill".into(),
            Self::NameMismatch => "name must match the skill's directory and existing name".into(),
            Self::InvalidField(field) => format!("invalid {field}"),
            Self::BodyEmpty => "SKILL.md has no instructions".into(),
            Self::BodyTooLarge => format!("SKILL.md exceeds {MAX_BODY_BYTES} bytes"),
            Self::Credentials(path) => format!("{path} contains a credential"),
            Self::CredentialCheckUnavailable => "credential filtering is unavailable".into(),
        }
    }
}

/// Parses an upload. `expected_name` pins the name of an existing skill.
pub fn parse_input(
    input: &SkillPackageInput,
    expected_name: Option<&str>,
) -> Result<ParsedPackage, PackageError> {
    match (&input.zip_base64, &input.skill_md) {
        (Some(zip), None) if input.files.is_empty() => {
            use base64::Engine as _;
            if zip.len() > MAX_ZIP_BYTES.div_ceil(3) * 4 {
                return Err(PackageError::ZipTooLarge);
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(zip.as_bytes())
                .map_err(|_| PackageError::InvalidZip)?;
            parse_zip(&bytes, expected_name)
        }
        (None, Some(skill_md)) => parse_files(
            skill_md,
            input
                .files
                .iter()
                .map(|file| (file.path.clone(), file.text.clone().into_bytes()))
                .collect(),
            None,
            expected_name,
        ),
        _ => Err(PackageError::MissingInput),
    }
}

pub fn parse_zip(bytes: &[u8], expected_name: Option<&str>) -> Result<ParsedPackage, PackageError> {
    let unpacked = archive::unpack(bytes)?;
    parse_files(
        &unpacked.skill_md,
        unpacked.files,
        unpacked.directory.as_deref(),
        expected_name,
    )
    .map(|mut parsed| {
        parsed.skipped.extend(unpacked.skipped);
        parsed
    })
}

fn parse_files(
    skill_md: &str,
    files: Vec<(String, Vec<u8>)>,
    directory: Option<&str>,
    expected_name: Option<&str>,
) -> Result<ParsedPackage, PackageError> {
    let (fields, body) = frontmatter::parse(skill_md)?;
    validate_name(&fields.name)?;
    if desk_diagnose_core::skills::bundled_names().contains(&fields.name.as_str()) {
        return Err(PackageError::ReservedName);
    }
    if directory.is_some_and(|dir| dir != fields.name)
        || expected_name.is_some_and(|name| name != fields.name)
    {
        return Err(PackageError::NameMismatch);
    }
    if body.trim().is_empty() {
        return Err(PackageError::BodyEmpty);
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(PackageError::BodyTooLarge);
    }
    check_credentials(SKILL_FILE, skill_md)?;
    let mut warnings = Vec::new();
    if body.lines().count() > LONG_BODY_LINES {
        warnings.push(SkillWarning::LongBody);
    }
    let mut skipped = Vec::new();
    let mut resources: Vec<PackageResource> = Vec::new();
    let mut total = skill_md.len();
    for (path, bytes) in files {
        if !valid_path(&path) || path == SKILL_FILE || resources.iter().any(|r| r.path == path) {
            skipped.push(skip(path, SkillSkipReason::InvalidPath));
            continue;
        }
        let Some(text) = text_file(bytes) else {
            skipped.push(skip(path, SkillSkipReason::Binary));
            continue;
        };
        if text.len() > MAX_RESOURCE_BYTES {
            skipped.push(skip(path, SkillSkipReason::TooLarge));
            continue;
        }
        if resources.len() == MAX_RESOURCES {
            skipped.push(skip(path, SkillSkipReason::TooManyFiles));
            continue;
        }
        total += text.len();
        if total > MAX_UNPACKED_BYTES {
            return Err(PackageError::UnpackedTooLarge);
        }
        check_credentials(&path, &text)?;
        let (kind, language) = classify(&path);
        resources.push(PackageResource {
            path,
            kind,
            language,
            text,
        });
    }
    resources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ParsedPackage {
        package: SkillPackage {
            name: fields.name,
            description: fields.description,
            compatibility: fields.compatibility,
            license: fields.license,
            metadata: fields.metadata,
            allowed_tools: fields.allowed_tools,
            body,
            resources,
        },
        skipped,
        warnings,
    })
}

fn skip(path: String, reason: SkillSkipReason) -> SkillSkippedFile {
    SkillSkippedFile { path, reason }
}

fn check_credentials(path: &str, text: &str) -> Result<(), PackageError> {
    match desk_diagnose_core::ai_assistant::settings::contains_credentials(text) {
        Ok(false) => Ok(()),
        Ok(true) => Err(PackageError::Credentials(path.into())),
        Err(_) => Err(PackageError::CredentialCheckUnavailable),
    }
}

pub fn validate_name(name: &str) -> Result<(), PackageError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    valid.then_some(()).ok_or(PackageError::InvalidName)
}

/// A relative `/`-separated path without empty, `.` or `..` segments.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !path.chars().any(char::is_control)
}

fn text_file(bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn classify(path: &str) -> (SkillResourceKind, Option<String>) {
    let kind = match path.split('/').next() {
        Some("scripts") => SkillResourceKind::Script,
        Some("references") => SkillResourceKind::Reference,
        Some("assets") => SkillResourceKind::Asset,
        _ => SkillResourceKind::Other,
    };
    let language = (kind == SkillResourceKind::Script)
        .then(|| {
            let extension = path.rsplit_once('.')?.1;
            Some(
                match extension {
                    "py" => "python",
                    "sh" | "bash" => "bash",
                    "js" | "mjs" | "cjs" => "javascript",
                    "ts" => "typescript",
                    "ps1" => "powershell",
                    "rb" => "ruby",
                    "go" => "go",
                    "rs" => "rust",
                    "pl" => "perl",
                    "bat" | "cmd" => "batch",
                    _ => return None,
                }
                .to_owned(),
            )
        })
        .flatten();
    (kind, language)
}

#[cfg(test)]
mod tests;
