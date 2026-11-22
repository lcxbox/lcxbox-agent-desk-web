//! Upload, preview and catalog rules shared by both hosts. Hosts own the
//! transaction, authorization, audit and the AI security scan.

use super::*;
use crate::skill_package::{PackageError, diff::diff_packages, parse_input};
use desk_agent_protocol::ai_agent::skill::{
    SkillCreate, SkillPackageInput, SkillPreview, SkillSaved, SkillUpdate,
};
use desk_diagnose_core::skills::{
    AuthoredNote, SkillCatalog, SkillResource, StoredSkillContent, StoredSkillEntry,
};

/// The decision behind an AI-authored version, read through its proposal.
async fn authored_note_on<C: ConnectionTrait>(
    db: &C,
    row: &version::Model,
) -> Result<Option<AuthoredNote>, SkillStoreError> {
    if parse_origin(&row.origin) != SkillOrigin::AiAuthored {
        return Ok(None);
    }
    let Some(proposal_id) = &row.proposal_id else {
        return Ok(None);
    };
    Ok(proposal::Entity::find()
        .filter(proposal::Column::ProposalId.eq(proposal_id))
        .one(db)
        .await?
        .and_then(|p| {
            Some(AuthoredNote {
                decision_source: p.decision_source.as_deref().and_then(parse_decision)?,
                decided_at_unix_ms: p.decided_at_ms?,
            })
        }))
}

/// Lines of changed text returned by one preview.
pub const MAX_PREVIEW_DIFF_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillErrorKind {
    Invalid,
    Conflict,
    NotFound,
    Limit,
    Backend,
}

#[derive(Debug)]
pub enum SkillServiceError {
    Package(PackageError),
    Store(SkillStoreError),
}

impl From<PackageError> for SkillServiceError {
    fn from(error: PackageError) -> Self {
        Self::Package(error)
    }
}

impl From<SkillStoreError> for SkillServiceError {
    fn from(error: SkillStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DbErr> for SkillServiceError {
    fn from(error: DbErr) -> Self {
        Self::Store(error.into())
    }
}

impl SkillServiceError {
    pub fn kind(&self) -> SkillErrorKind {
        match self {
            Self::Package(PackageError::CredentialCheckUnavailable) => SkillErrorKind::Backend,
            Self::Package(_) => SkillErrorKind::Invalid,
            Self::Store(SkillStoreError::NotFound) => SkillErrorKind::NotFound,
            Self::Store(SkillStoreError::Conflict) => SkillErrorKind::Conflict,
            Self::Store(SkillStoreError::NameTaken) => SkillErrorKind::Invalid,
            Self::Store(SkillStoreError::Limit(_)) => SkillErrorKind::Limit,
            Self::Store(SkillStoreError::Backend(_)) => SkillErrorKind::Backend,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Package(error) => error.message(),
            Self::Store(SkillStoreError::NotFound) => "skill not found".into(),
            Self::Store(SkillStoreError::Conflict) => {
                "the skill changed; reload and try again".into()
            }
            Self::Store(SkillStoreError::NameTaken) => {
                "a skill with this name already exists".into()
            }
            Self::Store(SkillStoreError::Limit(what)) => format!("too many {what}"),
            Self::Store(SkillStoreError::Backend(_)) => "skill storage is unavailable".into(),
        }
    }
}

/// Validates an upload against the current version without saving it.
pub async fn preview_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: Option<&str>,
    input: &SkillPackageInput,
) -> Result<(ParsedPackage, SkillPreview), SkillServiceError> {
    let current = match skill_id {
        Some(id) => Some(current_package_on(db, scope_key, id).await?.1),
        None => None,
    };
    let parsed = parse_input(input, current.as_ref().map(|p| p.name.as_str()))?;
    let preview = SkillPreview {
        name: parsed.package.name.clone(),
        files: parsed.package.file_metas(),
        skipped: parsed.skipped.clone(),
        warnings: parsed.warnings.clone(),
        diff: diff_packages(current.as_ref(), &parsed.package, MAX_PREVIEW_DIFF_BYTES),
        scan: None,
        scan_id: None,
    };
    Ok((parsed, preview))
}

pub async fn create_from_input_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    request: &SkillCreate,
    now_ms: i64,
) -> Result<SkillSaved, SkillServiceError> {
    let parsed = parse_input(&request.package, None)?;
    let skill = save_manual_on(
        db,
        scope_key,
        &parsed,
        None,
        SkillOrigin::Imported,
        request.scan_id.as_deref(),
        now_ms,
    )
    .await?;
    Ok(saved(skill, parsed))
}

/// A new version, an `enabled` change, or both.
pub async fn update_from_input_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
    request: &SkillUpdate,
    now_ms: i64,
) -> Result<SkillSaved, SkillServiceError> {
    let Some(package) = &request.package else {
        let skill = update_on(
            db,
            scope_key,
            skill_id,
            request.expected_revision,
            request.enabled,
            None,
            now_ms,
        )
        .await?;
        return Ok(SkillSaved {
            skill,
            files: Vec::new(),
            skipped: Vec::new(),
            warnings: Vec::new(),
        });
    };
    let (row, _) = current_package_on(db, scope_key, skill_id).await?;
    let parsed = parse_input(package, Some(&row.name))?;
    let origin = match request.origin {
        Some(desk_agent_protocol::ai_agent::skill::SkillManualOrigin::Edited) => {
            SkillOrigin::Edited
        }
        _ => SkillOrigin::Imported,
    };
    let scan = match &request.scan_id {
        Some(id) => scan_summary_on(db, scope_key, id, now_ms).await?,
        None => None,
    };
    let skill = update_on(
        db,
        scope_key,
        skill_id,
        request.expected_revision,
        request.enabled,
        Some((&parsed.package, &VersionSource::manual(origin, scan))),
        now_ms,
    )
    .await?;
    Ok(saved(skill, parsed))
}

fn saved(skill: SkillSummary, parsed: ParsedPackage) -> SkillSaved {
    SkillSaved {
        skill,
        files: parsed.package.file_metas(),
        skipped: parsed.skipped,
        warnings: parsed.warnings,
    }
}

/// The catalog of one request: enabled skills of `scope_keys` and the pinned
/// content of versions already loaded in its input. A loaded skill outside
/// those scopes or since deleted is left out and projects as unavailable.
pub async fn host_catalog_on<C: ConnectionTrait>(
    db: &C,
    scope_keys: &[String],
    loaded: &[(String, u64)],
    disabled_bundled: Vec<String>,
) -> Result<SkillCatalog, SkillStoreError> {
    let stored = catalog_on(db, scope_keys)
        .await?
        .into_iter()
        .map(|row| StoredSkillEntry {
            skill_id: row.skill_id,
            scope: row.scope,
            name: row.name,
            description: row.description,
            version: row.version,
            origin: row.origin,
            size_bytes: row.size_bytes,
            resources: row.resources,
        })
        .collect();
    let mut contents = Vec::with_capacity(loaded.len());
    for (skill_id, number) in loaded {
        let Some(row) = skill::Entity::find()
            .filter(skill::Column::SkillId.eq(skill_id))
            .filter(skill::Column::ScopeKey.is_in(scope_keys.iter().cloned()))
            .one(db)
            .await?
        else {
            continue;
        };
        let Some(stored_version) = version_on(db, skill_id, *number as i64).await? else {
            continue;
        };
        let package = package_of(&row.name, &stored_version)?;
        let authored = authored_note_on(db, &stored_version).await?;
        contents.push(StoredSkillContent {
            skill_id: skill_id.clone(),
            version: *number,
            body: package.body,
            allowed_tools: package.allowed_tools,
            resources: package
                .resources
                .into_iter()
                .map(|r| SkillResource {
                    path: r.path,
                    kind: r.kind,
                    language: r.language,
                    text: r.text,
                })
                .collect(),
            authored,
        });
    }
    Ok(SkillCatalog {
        stored,
        loaded: contents,
        disabled_bundled,
    })
}

/// Scope keys visible to a request of `actor_id`.
pub fn visible_scopes(actor_id: &str, include_personal: bool) -> Vec<String> {
    let mut scopes = vec![PLATFORM_SCOPE.to_owned()];
    if include_personal {
        scopes.push(personal_scope(actor_id));
    }
    scopes
}

/// Bundled skills with their content and whether this deployment enabled them.
pub fn bundled_view(
    settings: &desk_agent_protocol::ai_agent::skill::BundledSkillSettings,
) -> desk_agent_protocol::ai_agent::skill::BundledSkillList {
    use desk_agent_protocol::ai_agent::skill::{
        BundledSkillList, BundledSkillView, SkillFileMeta, SkillFileView, SkillResourceKind,
    };
    let skills = desk_diagnose_core::skills::bundled_names()
        .iter()
        .filter_map(|name| {
            let (source, files) = desk_diagnose_core::skills::bundled_package(name)?;
            Some(BundledSkillView {
                name: (*name).into(),
                description: desk_diagnose_core::skills::bundled_description(name)?.into(),
                enabled: !settings.disabled.iter().any(|d| d == name),
                skill_md: source.into(),
                files: files
                    .into_iter()
                    .map(|(path, text)| SkillFileView {
                        meta: SkillFileMeta {
                            path: path.into(),
                            kind: SkillResourceKind::Reference,
                            language: None,
                            size_bytes: text.len() as u64,
                        },
                        text: text.into(),
                    })
                    .collect(),
            })
        })
        .collect();
    BundledSkillList {
        revision: settings.revision,
        skills,
    }
}

/// The disabled list after enabling or disabling one bundled skill.
pub fn toggle_bundled(
    settings: &desk_agent_protocol::ai_agent::skill::BundledSkillSettings,
    name: &str,
    enabled: bool,
) -> Option<desk_agent_protocol::ai_agent::skill::BundledSkillSettings> {
    if !desk_diagnose_core::skills::bundled_names().contains(&name) {
        return None;
    }
    let mut next = settings.clone();
    next.disabled.retain(|d| d != name);
    if !enabled {
        next.disabled.push(name.into());
        next.disabled.sort();
    }
    next.revision = settings.revision + 1;
    Some(next)
}
