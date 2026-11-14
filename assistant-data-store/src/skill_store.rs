//! Storage rules for platform and personal skills. Callers own the
//! transaction, authorization and audit; every write here runs on `db`.

use crate::entity::{
    agent_skill as skill, agent_skill_proposal as proposal, agent_skill_scan as scan,
    agent_skill_scope as scope, agent_skill_version as version,
};
use crate::skill_package::{PackageResource, ParsedPackage, SkillPackage};
use desk_agent_protocol::ai_assistant::skill::{
    SkillDecisionSource, SkillDetail, SkillFileMeta, SkillFileView, SkillOrigin, SkillScope,
    SkillSummary, SkillVersionInfo,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, SqlErr, sea_query::Expr, sea_query::OnConflict,
};
use std::collections::BTreeMap;

pub const PLATFORM_SCOPE: &str = "platform";
pub const MAX_SKILLS_PER_SCOPE: i64 = 64;
pub const MAX_ENABLED_PER_SCOPE: i64 = 32;
pub const MAX_PENDING_PROPOSALS: i64 = 8;
pub const RETAINED_VERSIONS: i64 = 10;
pub const SCAN_TTL_MS: i64 = 60 * 60 * 1000;

pub fn personal_scope(actor_id: &str) -> String {
    format!("personal:{actor_id}")
}

pub fn scope_of(scope_key: &str) -> SkillScope {
    if scope_key == PLATFORM_SCOPE {
        SkillScope::Platform
    } else {
        SkillScope::Personal
    }
}

#[derive(Debug)]
pub enum SkillStoreError {
    NotFound,
    /// The skill or scope changed since the caller read it.
    Conflict,
    NameTaken,
    Limit(&'static str),
    Backend(DbErr),
}

impl From<DbErr> for SkillStoreError {
    fn from(error: DbErr) -> Self {
        match error.sql_err() {
            Some(SqlErr::UniqueConstraintViolation(_)) => Self::NameTaken,
            _ => Self::Backend(error),
        }
    }
}

fn origin_name(origin: SkillOrigin) -> &'static str {
    match origin {
        SkillOrigin::Imported => "imported",
        SkillOrigin::Edited => "edited",
        SkillOrigin::AiAuthored => "ai_authored",
    }
}

fn parse_origin(value: &str) -> SkillOrigin {
    match value {
        "edited" => SkillOrigin::Edited,
        "ai_authored" => SkillOrigin::AiAuthored,
        _ => SkillOrigin::Imported,
    }
}

pub fn parse_decision(value: &str) -> Option<SkillDecisionSource> {
    match value {
        "ai_review" => Some(SkillDecisionSource::AiReview),
        "owner" => Some(SkillDecisionSource::Owner),
        _ => None,
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("skill rows serialize")
}

fn invalid_row() -> SkillStoreError {
    SkillStoreError::Backend(DbErr::Custom("invalid stored skill version".into()))
}

/// How a new version came to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSource {
    pub origin: SkillOrigin,
    pub proposal_id: Option<String>,
    pub source_policy_json: Option<String>,
    pub scan_summary_json: Option<String>,
}

impl VersionSource {
    pub fn manual(origin: SkillOrigin, scan_summary_json: Option<String>) -> Self {
        Self {
            origin,
            proposal_id: None,
            source_policy_json: None,
            scan_summary_json,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ScopeDelta {
    skills: i64,
    enabled: i64,
    pending: i64,
}

/// Initializes, locks and updates the scope counter row inside the caller's transaction.
async fn adjust_scope_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    delta: ScopeDelta,
) -> Result<(), SkillStoreError> {
    scope::Entity::insert(scope::ActiveModel {
        scope_key: Set(scope_key.into()),
        skill_count: Set(0),
        enabled_count: Set(0),
        pending_proposal_count: Set(0),
        revision: Set(1),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(scope::Column::ScopeKey)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    let mut select = scope::Entity::find().filter(scope::Column::ScopeKey.eq(scope_key));
    if db.get_database_backend() == DatabaseBackend::Postgres {
        select = select.lock_exclusive();
    }
    let row = select.one(db).await?.ok_or(SkillStoreError::Conflict)?;
    let next = |current: i64, change: i64, max: i64, what: &'static str| {
        let value = current + change;
        if value < 0 {
            return Err(SkillStoreError::Conflict);
        }
        if change > 0 && value > max {
            return Err(SkillStoreError::Limit(what));
        }
        Ok(value)
    };
    let skills = next(
        row.skill_count,
        delta.skills,
        MAX_SKILLS_PER_SCOPE,
        "skills",
    )?;
    let enabled = next(
        row.enabled_count,
        delta.enabled,
        MAX_ENABLED_PER_SCOPE,
        "enabled skills",
    )?;
    let pending = next(
        row.pending_proposal_count,
        delta.pending,
        MAX_PENDING_PROPOSALS,
        "pending skill proposals",
    )?;
    let changed = scope::Entity::update_many()
        .col_expr(scope::Column::SkillCount, Expr::value(skills))
        .col_expr(scope::Column::EnabledCount, Expr::value(enabled))
        .col_expr(scope::Column::PendingProposalCount, Expr::value(pending))
        .col_expr(scope::Column::Revision, Expr::value(row.revision + 1))
        .filter(scope::Column::ScopeKey.eq(scope_key))
        .filter(scope::Column::Revision.eq(row.revision))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(SkillStoreError::Conflict);
    }
    Ok(())
}

fn version_row(
    skill_id: &str,
    number: i64,
    package: &SkillPackage,
    source: &VersionSource,
    now_ms: i64,
) -> version::ActiveModel {
    version::ActiveModel {
        skill_id: Set(skill_id.into()),
        version: Set(number),
        origin: Set(origin_name(source.origin).into()),
        proposal_id: Set(source.proposal_id.clone()),
        source_policy_json: Set(source.source_policy_json.clone()),
        scan_summary_json: Set(source.scan_summary_json.clone()),
        description: Set(package.description.clone()),
        compatibility: Set(package.compatibility.clone()),
        license: Set(package.license.clone()),
        metadata_json: Set(json(&package.metadata)),
        allowed_tools: Set(package.allowed_tools.clone()),
        body: Set(package.body.clone()),
        resource_index_json: Set(json(&package.file_metas())),
        resources_json: Set(json(&package.resources)),
        size_bytes: Set(package.size_bytes() as i64),
        created_at_ms: Set(now_ms),
        ..Default::default()
    }
}

/// Rebuilds the validated package of a stored version.
pub fn package_of(name: &str, row: &version::Model) -> Result<SkillPackage, SkillStoreError> {
    Ok(SkillPackage {
        name: name.into(),
        description: row.description.clone(),
        compatibility: row.compatibility.clone(),
        license: row.license.clone(),
        metadata: serde_json::from_str::<BTreeMap<String, String>>(&row.metadata_json)
            .map_err(|_| invalid_row())?,
        allowed_tools: row.allowed_tools.clone(),
        body: row.body.clone(),
        resources: serde_json::from_str::<Vec<PackageResource>>(&row.resources_json)
            .map_err(|_| invalid_row())?,
    })
}

pub fn resource_index_of(row: &version::Model) -> Result<Vec<SkillFileMeta>, SkillStoreError> {
    serde_json::from_str(&row.resource_index_json).map_err(|_| invalid_row())
}

async fn skill_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
) -> Result<skill::Model, SkillStoreError> {
    skill::Entity::find()
        .filter(skill::Column::SkillId.eq(skill_id))
        .filter(skill::Column::ScopeKey.eq(scope_key))
        .one(db)
        .await?
        .ok_or(SkillStoreError::NotFound)
}

pub async fn version_on<C: ConnectionTrait>(
    db: &C,
    skill_id: &str,
    number: i64,
) -> Result<Option<version::Model>, SkillStoreError> {
    Ok(version::Entity::find()
        .filter(version::Column::SkillId.eq(skill_id))
        .filter(version::Column::Version.eq(number))
        .one(db)
        .await?)
}

async fn decision_of<C: ConnectionTrait>(
    db: &C,
    row: &version::Model,
) -> Result<Option<SkillDecisionSource>, SkillStoreError> {
    let Some(proposal_id) = &row.proposal_id else {
        return Ok(None);
    };
    Ok(proposal::Entity::find()
        .filter(proposal::Column::ProposalId.eq(proposal_id))
        .one(db)
        .await?
        .and_then(|p| p.decision_source.as_deref().and_then(parse_decision)))
}

async fn summary_of<C: ConnectionTrait>(
    db: &C,
    row: &skill::Model,
    current: &version::Model,
) -> Result<SkillSummary, SkillStoreError> {
    Ok(SkillSummary {
        skill_id: row.skill_id.clone(),
        scope: scope_of(&row.scope_key),
        name: row.name.clone(),
        description: current.description.clone(),
        enabled: row.enabled,
        current_version: row.current_version as u64,
        revision: row.revision as u64,
        origin: parse_origin(&current.origin),
        decision_source: decision_of(db, current).await?,
        updated_at_ms: row.updated_at_ms,
    })
}

async fn current_of<C: ConnectionTrait>(
    db: &C,
    row: &skill::Model,
) -> Result<version::Model, SkillStoreError> {
    version_on(db, &row.skill_id, row.current_version)
        .await?
        .ok_or_else(invalid_row)
}

pub async fn list_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
) -> Result<Vec<SkillSummary>, SkillStoreError> {
    let rows = skill::Entity::find()
        .filter(skill::Column::ScopeKey.eq(scope_key))
        .order_by_asc(skill::Column::Name)
        .all(db)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let current = current_of(db, &row).await?;
        out.push(summary_of(db, &row, &current).await?);
    }
    Ok(out)
}

pub async fn detail_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
) -> Result<SkillDetail, SkillStoreError> {
    let row = skill_on(db, scope_key, skill_id).await?;
    let current = current_of(db, &row).await?;
    let package = package_of(&row.name, &current)?;
    let versions = version::Entity::find()
        .filter(version::Column::SkillId.eq(skill_id))
        .order_by_desc(version::Column::Version)
        .all(db)
        .await?
        .into_iter()
        .map(|v| SkillVersionInfo {
            version: v.version as u64,
            origin: parse_origin(&v.origin),
            size_bytes: v.size_bytes as u64,
            created_at_ms: v.created_at_ms,
        })
        .collect();
    Ok(SkillDetail {
        summary: summary_of(db, &row, &current).await?,
        compatibility: package.compatibility.clone(),
        license: package.license.clone(),
        metadata: package.metadata.clone(),
        allowed_tools: package.allowed_tools.clone(),
        skill_md: package.render_skill_md(),
        files: package
            .resources
            .iter()
            .map(|r| SkillFileView {
                meta: r.meta(),
                text: r.text.clone(),
            })
            .collect(),
        versions,
    })
}

/// The current package and revision of a skill, for previews and proposals.
pub async fn current_package_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
) -> Result<(skill::Model, SkillPackage), SkillStoreError> {
    let row = skill_on(db, scope_key, skill_id).await?;
    let current = current_of(db, &row).await?;
    let package = package_of(&row.name, &current)?;
    Ok((row, package))
}

pub async fn find_by_name_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    name: &str,
) -> Result<Option<skill::Model>, SkillStoreError> {
    Ok(skill::Entity::find()
        .filter(skill::Column::ScopeKey.eq(scope_key))
        .filter(skill::Column::Name.eq(name))
        .one(db)
        .await?)
}

/// Creates an enabled skill with its first version.
pub async fn create_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    package: &SkillPackage,
    source: &VersionSource,
    now_ms: i64,
) -> Result<SkillSummary, SkillStoreError> {
    if find_by_name_on(db, scope_key, &package.name)
        .await?
        .is_some()
    {
        return Err(SkillStoreError::NameTaken);
    }
    adjust_scope_on(
        db,
        scope_key,
        ScopeDelta {
            skills: 1,
            enabled: 1,
            pending: 0,
        },
    )
    .await?;
    let skill_id = new_id();
    skill::Entity::insert(skill::ActiveModel {
        skill_id: Set(skill_id.clone()),
        scope_key: Set(scope_key.into()),
        name: Set(package.name.clone()),
        enabled: Set(true),
        current_version: Set(1),
        revision: Set(1),
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await?;
    version::Entity::insert(version_row(&skill_id, 1, package, source, now_ms))
        .exec_without_returning(db)
        .await?;
    let row = skill_on(db, scope_key, &skill_id).await?;
    let current = current_of(db, &row).await?;
    summary_of(db, &row, &current).await
}

/// Changes `enabled`, stores a new version, or both, under the caller's revision.
pub async fn update_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
    expected_revision: u64,
    enabled: Option<bool>,
    new_version: Option<(&SkillPackage, &VersionSource)>,
    now_ms: i64,
) -> Result<SkillSummary, SkillStoreError> {
    let row = skill_on(db, scope_key, skill_id).await?;
    if row.revision as u64 != expected_revision {
        return Err(SkillStoreError::Conflict);
    }
    if let Some((package, _)) = new_version
        && package.name != row.name
    {
        return Err(SkillStoreError::Conflict);
    }
    let enabled = enabled.unwrap_or(row.enabled);
    if enabled != row.enabled {
        adjust_scope_on(
            db,
            scope_key,
            ScopeDelta {
                enabled: if enabled { 1 } else { -1 },
                ..Default::default()
            },
        )
        .await?;
    }
    let next_version = row.current_version + i64::from(new_version.is_some());
    let changed = skill::Entity::update_many()
        .col_expr(skill::Column::Enabled, Expr::value(enabled))
        .col_expr(skill::Column::CurrentVersion, Expr::value(next_version))
        .col_expr(skill::Column::Revision, Expr::value(row.revision + 1))
        .col_expr(skill::Column::UpdatedAtMs, Expr::value(now_ms))
        .filter(skill::Column::SkillId.eq(skill_id))
        .filter(skill::Column::Revision.eq(row.revision))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(SkillStoreError::Conflict);
    }
    if let Some((package, source)) = new_version {
        version::Entity::insert(version_row(skill_id, next_version, package, source, now_ms))
            .exec_without_returning(db)
            .await?;
        version::Entity::delete_many()
            .filter(version::Column::SkillId.eq(skill_id))
            .filter(version::Column::Version.lte(next_version - RETAINED_VERSIONS))
            .exec(db)
            .await?;
    }
    let row = skill_on(db, scope_key, skill_id).await?;
    let current = current_of(db, &row).await?;
    summary_of(db, &row, &current).await
}

pub async fn delete_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    skill_id: &str,
    expected_revision: u64,
) -> Result<(), SkillStoreError> {
    let row = skill_on(db, scope_key, skill_id).await?;
    if row.revision as u64 != expected_revision {
        return Err(SkillStoreError::Conflict);
    }
    let removed = skill::Entity::delete_many()
        .filter(skill::Column::SkillId.eq(skill_id))
        .filter(skill::Column::Revision.eq(row.revision))
        .exec(db)
        .await?;
    if removed.rows_affected != 1 {
        return Err(SkillStoreError::Conflict);
    }
    version::Entity::delete_many()
        .filter(version::Column::SkillId.eq(skill_id))
        .exec(db)
        .await?;
    adjust_scope_on(
        db,
        scope_key,
        ScopeDelta {
            skills: -1,
            enabled: -i64::from(row.enabled),
            pending: 0,
        },
    )
    .await
}

/// Removes every personal skill, version, proposal and counter of an account.
pub async fn delete_personal_skills_on<C: ConnectionTrait>(
    db: &C,
    actor_id: &str,
) -> Result<(), SkillStoreError> {
    let scope_key = personal_scope(actor_id);
    let ids: Vec<String> = skill::Entity::find()
        .filter(skill::Column::ScopeKey.eq(&scope_key))
        .select_only()
        .column(skill::Column::SkillId)
        .into_tuple()
        .all(db)
        .await?;
    if !ids.is_empty() {
        version::Entity::delete_many()
            .filter(version::Column::SkillId.is_in(ids))
            .exec(db)
            .await?;
    }
    skill::Entity::delete_many()
        .filter(skill::Column::ScopeKey.eq(&scope_key))
        .exec(db)
        .await?;
    proposal::Entity::delete_many()
        .filter(proposal::Column::ActorId.eq(actor_id))
        .exec(db)
        .await?;
    scan::Entity::delete_many()
        .filter(scan::Column::ScopeKey.eq(&scope_key))
        .exec(db)
        .await?;
    scope::Entity::delete_many()
        .filter(scope::Column::ScopeKey.eq(&scope_key))
        .exec(db)
        .await?;
    Ok(())
}

/// One enabled skill as listed in a conversation catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub skill_id: String,
    pub scope: SkillScope,
    pub name: String,
    pub description: String,
    pub version: u64,
    pub origin: SkillOrigin,
    pub size_bytes: u64,
    pub resources: Vec<SkillFileMeta>,
}

/// Enabled skills of the given scopes, without file contents.
pub async fn catalog_on<C: ConnectionTrait>(
    db: &C,
    scope_keys: &[String],
) -> Result<Vec<CatalogRow>, SkillStoreError> {
    let rows = skill::Entity::find()
        .filter(skill::Column::ScopeKey.is_in(scope_keys.iter().cloned()))
        .filter(skill::Column::Enabled.eq(true))
        .order_by_asc(skill::Column::Name)
        .all(db)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let current = version::Entity::find()
            .filter(version::Column::SkillId.eq(&row.skill_id))
            .filter(version::Column::Version.eq(row.current_version))
            .select_only()
            .columns([
                version::Column::Description,
                version::Column::Origin,
                version::Column::SizeBytes,
                version::Column::ResourceIndexJson,
            ])
            .into_tuple::<(String, String, i64, String)>()
            .one(db)
            .await?
            .ok_or_else(invalid_row)?;
        out.push(CatalogRow {
            skill_id: row.skill_id,
            scope: scope_of(&row.scope_key),
            name: row.name,
            description: current.0,
            version: row.current_version as u64,
            origin: parse_origin(&current.1),
            size_bytes: current.2 as u64,
            resources: serde_json::from_str(&current.3).map_err(|_| invalid_row())?,
        });
    }
    Ok(out)
}

/// Stores a scan shown before saving; it expires after [`SCAN_TTL_MS`].
pub async fn save_scan_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    summary_json: &str,
    now_ms: i64,
) -> Result<String, SkillStoreError> {
    scan::Entity::delete_many()
        .filter(scan::Column::ExpiresAtMs.lt(now_ms))
        .exec(db)
        .await?;
    let scan_id = new_id();
    scan::Entity::insert(scan::ActiveModel {
        scan_id: Set(scan_id.clone()),
        scope_key: Set(scope_key.into()),
        summary_json: Set(summary_json.into()),
        expires_at_ms: Set(now_ms + SCAN_TTL_MS),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await?;
    Ok(scan_id)
}

/// The summary of a live scan of the same scope; a missing or expired scan is ignored.
pub async fn scan_summary_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    scan_id: &str,
    now_ms: i64,
) -> Result<Option<String>, SkillStoreError> {
    Ok(scan::Entity::find()
        .filter(scan::Column::ScanId.eq(scan_id))
        .filter(scan::Column::ScopeKey.eq(scope_key))
        .filter(scan::Column::ExpiresAtMs.gte(now_ms))
        .one(db)
        .await?
        .map(|row| row.summary_json))
}

/// Saves an upload or edit: a new skill, or a new version of `skill_id`.
pub async fn save_manual_on<C: ConnectionTrait>(
    db: &C,
    scope_key: &str,
    parsed: &ParsedPackage,
    target: Option<(&str, u64)>,
    origin: SkillOrigin,
    scan_id: Option<&str>,
    now_ms: i64,
) -> Result<SkillSummary, SkillStoreError> {
    let scan = match scan_id {
        Some(id) => scan_summary_on(db, scope_key, id, now_ms).await?,
        None => None,
    };
    let source = VersionSource::manual(origin, scan);
    match target {
        None => create_on(db, scope_key, &parsed.package, &source, now_ms).await,
        Some((skill_id, revision)) => {
            update_on(
                db,
                scope_key,
                skill_id,
                revision,
                None,
                Some((&parsed.package, &source)),
                now_ms,
            )
            .await
        }
    }
}

pub mod service;

#[cfg(test)]
mod tests;
