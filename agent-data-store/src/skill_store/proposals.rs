//! Model-authored personal skill proposals: saving one under the proposing
//! turn's fence, the AI or owner decision, lazy expiry and the pending count.
//! Callers own the transaction; on any error they roll it back.

use super::*;
use crate::schedule::{
    ScheduleStoreError, ToolReceipt, authority_now, commit_tool_receipt_on, lock_tool_call_on,
    session::{OrgMode, ScheduleHostStores, SessionKey},
};
use crate::skill_package::{diff::diff_packages, parse_input, validate_name};
use desk_agent_protocol::ai_agent::skill::{
    SkillFileInput, SkillPackageInput, SkillProposalKind, SkillProposalStatus, SkillProposalView,
};
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall},
    model_observability::tool::ToolObservation,
    proposal_review::{DecisionKind, ProposalKind, ProposalReviewRef, SKILL_DECIDED_EVENT},
    session::PersistedAgentSession,
    skills::authoring::{self, AuthoredSkillSource, SkillProposal},
};

/// Line text of changed hunks shown to the reviewer and the owner.
pub const PROPOSAL_DIFF_BYTES: usize = 48 * 1024;

#[derive(Debug)]
pub enum SkillProposalError {
    /// A refusal the model or the owner may read.
    Rejected(String),
    Store(ScheduleStoreError),
}

impl From<ScheduleStoreError> for SkillProposalError {
    fn from(error: ScheduleStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DbErr> for SkillProposalError {
    fn from(error: DbErr) -> Self {
        Self::Store(ScheduleStoreError::Backend(error))
    }
}

impl From<SkillStoreError> for SkillProposalError {
    fn from(error: SkillStoreError) -> Self {
        match error {
            SkillStoreError::Backend(error) => Self::Store(ScheduleStoreError::Backend(error)),
            SkillStoreError::NotFound => Self::Store(ScheduleStoreError::NotFound),
            SkillStoreError::Conflict => Self::Store(ScheduleStoreError::Conflict),
            SkillStoreError::NameTaken => Self::Rejected(authoring::NAME_TAKEN.into()),
            SkillStoreError::Limit(what) => Self::Rejected(format!("too many {what}")),
        }
    }
}

fn rejected(message: impl Into<String>) -> SkillProposalError {
    SkillProposalError::Rejected(message.into())
}

/// Who decided a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDecider {
    Owner,
    /// The delegated approval reviewer; its reason reaches the model only
    /// when its provenance can be verified.
    Ai(desk_diagnose_core::proposal_review::provenance::AiDecisionReason),
}

fn status_name(status: SkillProposalStatus) -> &'static str {
    match status {
        SkillProposalStatus::Pending => "pending",
        SkillProposalStatus::Approved => "approved",
        SkillProposalStatus::Rejected => "rejected",
        SkillProposalStatus::Withdrawn => "withdrawn",
        SkillProposalStatus::Conflicted => "conflicted",
    }
}

desk_diagnose_core::model_receipt! {
    struct PendingReceipt {
        proposal_id: desk_diagnose_core::model_content::ServerId,
        status: &'static str,
        skill_ref: desk_diagnose_core::model_content::ModelArg<String>,
        kind: &'static str,
        base_version: Option<u64>,
        awaiting_decision: bool,
        message: &'static str,
    }
}

fn kind_name(kind: SkillProposalKind) -> &'static str {
    match kind {
        SkillProposalKind::Create => "create",
        SkillProposalKind::Update => "update",
        SkillProposalKind::Disable => "disable",
        SkillProposalKind::Delete => "delete",
    }
}

fn parse_kind(value: &str) -> SkillProposalKind {
    match value {
        "update" => SkillProposalKind::Update,
        "disable" => SkillProposalKind::Disable,
        "delete" => SkillProposalKind::Delete,
        _ => SkillProposalKind::Create,
    }
}

fn is_removal(kind: SkillProposalKind) -> bool {
    matches!(kind, SkillProposalKind::Disable | SkillProposalKind::Delete)
}

/// Whether the agent may remove `skill`: disabling needs an
/// agent-written current version, deleting needs every retained version
/// to be agent-written. Versions past the retention limit no longer count.
async fn agent_written_on<C: ConnectionTrait>(
    db: &C,
    skill: &skill::Model,
    kind: SkillProposalKind,
) -> Result<bool, DbErr> {
    let mut select = version::Entity::find().filter(version::Column::SkillId.eq(&skill.skill_id));
    if kind == SkillProposalKind::Disable {
        select = select.filter(version::Column::Version.eq(skill.current_version));
    }
    let origins: Vec<String> = select
        .select_only()
        .column(version::Column::Origin)
        .into_tuple()
        .all(db)
        .await?;
    Ok(!origins.is_empty()
        && origins
            .iter()
            .all(|origin| parse_origin(origin) == SkillOrigin::AiAuthored))
}

fn parse_status(value: &str) -> SkillProposalStatus {
    match value {
        "approved" => SkillProposalStatus::Approved,
        "rejected" => SkillProposalStatus::Rejected,
        "withdrawn" => SkillProposalStatus::Withdrawn,
        "conflicted" => SkillProposalStatus::Conflicted,
        _ => SkillProposalStatus::Pending,
    }
}

/// Saves the proposal of `call` and its receipt under the turn fence and
/// opens its review. Returns the receipt id and the session to adopt after
/// the caller commits. Refusals never reach the reviewer.
pub async fn propose_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    call: &ToolCall,
    observation: &ToolObservation,
) -> Result<(String, PersistedAgentSession), SkillProposalError> {
    use desk_diagnose_core::model_observability::{InputIssue, Stage};
    if call.name == authoring::REMOVE_TOOL {
        return propose_removal_on(host, txn, session, call, observation).await;
    }
    let draft = authoring::parse(session, call, observation).map_err(rejected)?;
    let (row, parent_index, now) = lock_tool_call_on(host, txn, session, call).await?;
    let parent = &session.conversation[parent_index];
    // A temporary conversation keeps no output archive, so it has no proof.
    let Some((policy, model_source)) = host.skill_proposal_source(txn, session, parent).await?
    else {
        observation.reject(Stage::Preflight, InputIssue::Precondition);
        return Err(rejected(authoring::NO_PROOF));
    };
    let source =
        AuthoredSkillSource::new(&session.actor_id, policy, model_source).map_err(|_| {
            observation.reject(Stage::Preflight, InputIssue::Precondition);
            rejected(authoring::NOT_RETAINABLE)
        })?;
    validate_name(&draft.name).map_err(|error| rejected(error.message()))?;
    let scope = personal_scope(&session.actor_id);
    let existing = find_by_name_on(txn, &scope, &draft.name).await?;
    let base = match (draft.base_version, existing) {
        (None, None) => None,
        (None, Some(_)) => return Err(rejected(authoring::NAME_TAKEN)),
        (Some(version), Some(row)) if row.current_version as u64 == version => {
            let current = current_of(txn, &row).await?;
            let package = package_of(&row.name, &current)?;
            Some((row, package))
        }
        (Some(_), _) => return Err(rejected(authoring::BASE_CHANGED)),
    };
    let rendered = SkillPackage {
        name: draft.name.clone(),
        description: draft.description.clone(),
        compatibility: draft.compatibility.clone(),
        // An update keeps the frontmatter the model does not edit.
        license: base.as_ref().and_then(|(_, old)| old.license.clone()),
        metadata: base
            .as_ref()
            .map(|(_, old)| old.metadata.clone())
            .unwrap_or_default(),
        allowed_tools: draft.allowed_tools.clone(),
        body: draft.body.clone(),
        resources: Vec::new(),
    }
    .render_skill_md();
    let parsed = parse_input(
        &SkillPackageInput {
            zip_base64: None,
            skill_md: Some(rendered),
            files: draft.files.clone(),
        },
        Some(&draft.name),
    )
    .map_err(|error| {
        observation.reject(Stage::Preflight, InputIssue::Semantic);
        rejected(error.message())
    })?;
    if let Some(skipped) = parsed.skipped.first() {
        observation.reject(Stage::Preflight, InputIssue::Semantic);
        return Err(rejected(format!(
            "file {} cannot be stored ({:?}); skill files must be UTF-8 text within the size limits",
            skipped.path, skipped.reason
        )));
    }
    let package = parsed.package;
    let content = canonical_input(&package);
    let changes = base
        .as_ref()
        .map(|(_, old)| diff_packages(Some(old), &package, PROPOSAL_DIFF_BYTES))
        .unwrap_or_default();
    let kind = if base.is_some() {
        SkillProposalKind::Update
    } else {
        SkillProposalKind::Create
    };
    save_on(
        host,
        txn,
        session,
        call,
        (row, parent_index, now),
        NewProposal {
            kind,
            name: package.name.clone(),
            base: base
                .as_ref()
                .map(|(row, _)| (row.skill_id.clone(), row.current_version)),
            content,
            changes,
            source: Some(source),
            reason: draft.reason,
        },
        observation,
    )
    .await
}

/// A validated proposal about to be saved.
struct NewProposal {
    kind: SkillProposalKind,
    name: String,
    /// The skill and version a non-create proposal acts on.
    base: Option<(String, i64)>,
    content: SkillPackageInput,
    changes: Vec<desk_agent_protocol::ai_agent::skill::SkillFileDiff>,
    /// `None` for a removal, which stores no model text.
    source: Option<AuthoredSkillSource>,
    reason: String,
}

/// Saves `new` with its pending slot and writes the submission receipt into
/// the proposing turn.
async fn save_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    call: &ToolCall,
    (row, parent_index, now): (crate::schedule::session::SessionRow<H::Row>, usize, i64),
    new: NewProposal,
    observation: &ToolObservation,
) -> Result<(String, PersistedAgentSession), SkillProposalError> {
    use desk_diagnose_core::model_observability::{InputIssue, Stage, StageOutcome};
    let parent = &session.conversation[parent_index];
    let scope = personal_scope(&session.actor_id);
    let removal = is_removal(new.kind);
    let proposal_id = new_id();
    let base_version = new.base.as_ref().map(|(_, version)| *version as u64);
    let base_skill = match &new.base {
        Some((skill_id, version)) => base_skill_on(txn, skill_id, *version, &new.name).await?,
        None => None,
    };
    let proposal = SkillProposal {
        proposal_id: proposal_id.clone(),
        revision: 1,
        owner_id: session.actor_id.clone(),
        conversation_id: session.conversation_id.clone(),
        input_revision: session.input_revision,
        created_at_unix_ms: u64::try_from(now).map_err(|_| ScheduleStoreError::Invalid)?,
        kind: new.kind,
        name: new.name.clone(),
        base_version,
        skill_md: new.content.skill_md.clone().unwrap_or_default(),
        files: new.content.files.clone(),
        changes: new.changes,
        reason: new.reason.clone(),
        base_skill,
    };
    if !authoring::fits_review(session, &proposal, &call.id) {
        observation.reject(Stage::Preflight, InputIssue::Length);
        return Err(rejected(if removal {
            authoring::REMOVAL_TOO_LARGE
        } else {
            authoring::TOO_LARGE
        }));
    }
    adjust_scope_on(
        txn,
        &scope,
        ScopeDelta {
            pending: 1,
            ..Default::default()
        },
    )
    .await
    .map_err(|error| match error {
        SkillStoreError::Limit(_) => rejected(format!(
            "the owner already has {MAX_PENDING_PROPOSALS} undecided skill proposals; wait for them to be decided"
        )),
        error => error.into(),
    })?;
    let input_revision =
        i64::try_from(session.input_revision).map_err(|_| ScheduleStoreError::Invalid)?;
    proposal::Entity::insert(proposal::ActiveModel {
        proposal_id: Set(proposal_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        conversation_id: Set(session.conversation_id.clone()),
        input_revision: Set(input_revision),
        kind: Set(kind_name(new.kind).into()),
        name: Set(new.name.clone()),
        base_skill_id: Set(new.base.as_ref().map(|(id, _)| id.clone())),
        base_version: Set(new.base.as_ref().map(|(_, version)| *version)),
        content_json: Set(json(&new.content)),
        source_policy_json: Set(json(&new.source)),
        reason: Set(new.reason),
        status: Set("pending".into()),
        decision_source: Set(None),
        decision_reason: Set(None),
        created_at_ms: Set(now),
        decided_at_ms: Set(None),
        revision: Set(1),
        ..Default::default()
    })
    .exec_without_returning(txn)
    .await?;
    observation.stage(Stage::Preflight, StageOutcome::Passed);
    let message = if removal {
        "Submitted for a decision. This turn ends now; when the AI reviewer or the owner decides, the server continues this requirement and this result shows the decision. Do not report the skill as disabled or deleted before it."
    } else {
        "Submitted for a decision. This turn ends now; when the AI reviewer or the owner decides, the server continues this requirement and this result shows the decision. Do not report the skill as saved before it. A saved skill grants no tool permission."
    };
    // The skill name is the call's own argument.
    let content = desk_diagnose_core::model_content::ModelContent::json(&PendingReceipt {
        proposal_id: desk_diagnose_core::model_content::ServerId::new(&proposal_id),
        status: "pending",
        skill_ref: desk_diagnose_core::model_content::ModelArg::of_call(
            call,
            parent.data_envelope.as_ref(),
            format!("personal/{}", new.name),
        ),
        kind: kind_name(new.kind),
        base_version,
        awaiting_decision: true,
        message,
    })
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let envelope = desk_diagnose_core::model_message_labels::composed_envelope(
        &content,
        parent.data_envelope.as_ref(),
        &call.id,
        desk_diagnose_core::model_message_labels::ServerResultSource::SkillProposal,
        false,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let text = content.text().to_owned();
    let receipt_id = format!("{}{}", ProposalKind::Skill.receipt_prefix(), call.id);
    let next = commit_tool_receipt_on(
        host,
        txn,
        row,
        session,
        ToolReceipt {
            message_id: &receipt_id,
            call_id: &call.id,
            content: text,
            envelope,
            stored_roots: Vec::new(),
            awaiting: Some(ProposalReviewRef::skill(&proposal_id)),
            now,
        },
    )
    .await?;
    Ok((receipt_id, next))
}

/// Saves a proposal to disable or delete an agent-written personal skill.
/// Its stored content is the current version, shown to the decider.
async fn propose_removal_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    call: &ToolCall,
    observation: &ToolObservation,
) -> Result<(String, PersistedAgentSession), SkillProposalError> {
    use desk_diagnose_core::model_observability::{InputIssue, Stage};
    let draft = authoring::parse_removal(session, call, observation).map_err(rejected)?;
    let (row, parent_index, now) = lock_tool_call_on(host, txn, session, call).await?;
    let _ = parent_index;
    // A removal stores no model text, so it needs no retention proof; it
    // still needs a conversation that may keep owner data.
    if !host.data_eligible(session) {
        observation.reject(Stage::Preflight, InputIssue::Precondition);
        return Err(rejected(authoring::UNAVAILABLE));
    }
    let scope = personal_scope(&session.actor_id);
    let skill = find_by_name_on(txn, &scope, &draft.name)
        .await?
        .filter(|skill| skill.current_version as u64 == draft.version)
        .ok_or_else(|| rejected(authoring::BASE_CHANGED))?;
    if !agent_written_on(txn, &skill, draft.kind).await? {
        observation.reject(Stage::Preflight, InputIssue::Precondition);
        return Err(rejected(authoring::NOT_AI_AUTHORED));
    }
    if draft.kind == SkillProposalKind::Disable && !skill.enabled {
        observation.reject(Stage::Preflight, InputIssue::Precondition);
        return Err(rejected(authoring::ALREADY_DISABLED));
    }
    let current = current_of(txn, &skill).await?;
    let package = package_of(&skill.name, &current)?;
    save_on(
        host,
        txn,
        session,
        call,
        (row, parent_index, now),
        NewProposal {
            kind: draft.kind,
            name: skill.name.clone(),
            base: Some((skill.skill_id.clone(), skill.current_version)),
            content: canonical_input(&package),
            changes: Vec::new(),
            source: None,
            // The model's reason is not kept; the owner sees the skill and action.
            reason: String::new(),
        },
        observation,
    )
    .await
}

/// The stored form of a proposed package: the same input as an import.
fn canonical_input(package: &SkillPackage) -> SkillPackageInput {
    SkillPackageInput {
        zip_base64: None,
        skill_md: Some(package.render_skill_md()),
        files: package
            .resources
            .iter()
            .map(|r| SkillFileInput {
                path: r.path.clone(),
                text: r.text.clone(),
            })
            .collect(),
    }
}

fn stored_input(row: &proposal::Model) -> Result<SkillPackageInput, ScheduleStoreError> {
    serde_json::from_str(&row.content_json).map_err(|_| ScheduleStoreError::Invalid)
}

/// Withdraws the pending proposal of `conversation_id`, when the owner
/// stopped that conversation or resumed its queue, and releases its pending
/// slot. It writes only proposal rows and counts; the caller owns the session
/// write. Returns whether a proposal was withdrawn.
pub async fn withdraw_on<C: ConnectionTrait>(
    txn: &C,
    actor_id: &str,
    conversation_id: &str,
    now: i64,
) -> Result<bool, SkillProposalError> {
    let changed = proposal::Entity::update_many()
        .col_expr(proposal::Column::Status, Expr::value("withdrawn"))
        .col_expr(proposal::Column::DecidedAtMs, Expr::value(Some(now)))
        .col_expr(
            proposal::Column::Revision,
            sea_orm::sea_query::ExprTrait::add(Expr::col(proposal::Column::Revision), 1),
        )
        .filter(proposal::Column::ActorId.eq(actor_id))
        .filter(proposal::Column::ConversationId.eq(conversation_id))
        .filter(proposal::Column::Status.eq("pending"))
        .exec(txn)
        .await?
        .rows_affected;
    if changed > 0 {
        adjust_scope_on(
            txn,
            &personal_scope(actor_id),
            ScopeDelta {
                pending: -(changed as i64),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(changed > 0)
}

/// The proposal waiting in `session`'s turn for the AI reviewer, or `None`
/// once it was decided, expired or belongs to another conversation.
pub async fn review_proposal_on<C: ConnectionTrait>(
    db: &C,
    session: &PersistedAgentSession,
    proposal_id: &str,
) -> Result<Option<SkillProposal>, ScheduleStoreError> {
    let Some(row) = proposal::Entity::find()
        .filter(proposal::Column::ProposalId.eq(proposal_id))
        .filter(proposal::Column::ActorId.eq(&session.actor_id))
        .filter(proposal::Column::ConversationId.eq(&session.conversation_id))
        .filter(proposal::Column::Status.eq("pending"))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let input = stored_input(&row)?;
    let changes = diff_of_on(db, &row, &input).await?;
    let base_skill = match (&row.base_skill_id, row.base_version) {
        (Some(skill_id), Some(version)) => base_skill_on(db, skill_id, version, &row.name)
            .await
            .map_err(|_| ScheduleStoreError::Invalid)?,
        _ => None,
    };
    Ok(Some(SkillProposal {
        proposal_id: row.proposal_id,
        revision: row.revision,
        owner_id: row.actor_id,
        conversation_id: row.conversation_id,
        input_revision: u64::try_from(row.input_revision)
            .map_err(|_| ScheduleStoreError::Invalid)?,
        created_at_unix_ms: u64::try_from(row.created_at_ms)
            .map_err(|_| ScheduleStoreError::Invalid)?,
        kind: parse_kind(&row.kind),
        name: row.name,
        base_version: row.base_version.map(|v| v as u64),
        skill_md: input.skill_md.unwrap_or_default(),
        files: input.files,
        changes,
        reason: row.reason,
        base_skill,
    }))
}

/// The stored version a proposal derives from, as its review records it;
/// `None` once that version is gone.
async fn base_skill_on<C: ConnectionTrait>(
    db: &C,
    skill_id: &str,
    version: i64,
    name: &str,
) -> Result<Option<desk_diagnose_core::approval_review::ReviewSkillBase>, SkillStoreError> {
    Ok(version_on(db, skill_id, version).await?.map(|row| {
        desk_diagnose_core::approval_review::ReviewSkillBase {
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            version: version as u64,
            ai_authored: parse_origin(&row.origin) == SkillOrigin::AiAuthored,
        }
    }))
}

/// Changes from the base version. A removed base version shows the whole
/// proposal as new; approval then marks it conflicted.
async fn diff_of_on<C: ConnectionTrait>(
    db: &C,
    row: &proposal::Model,
    input: &SkillPackageInput,
) -> Result<Vec<desk_agent_protocol::ai_agent::skill::SkillFileDiff>, ScheduleStoreError> {
    let (Some(skill_id), Some(base_version)) = (&row.base_skill_id, row.base_version) else {
        return Ok(Vec::new());
    };
    // A removal shows the version it acts on, not a change.
    if is_removal(parse_kind(&row.kind)) {
        return Ok(Vec::new());
    }
    let new = parse_input(input, Some(&row.name)).map_err(|_| ScheduleStoreError::Invalid)?;
    let old = match version_on(db, skill_id, base_version).await {
        Ok(Some(version)) => Some(package_of(&row.name, &version).map_err(store_error)?),
        Ok(None) => None,
        Err(error) => return Err(store_error(error)),
    };
    Ok(diff_packages(
        old.as_ref(),
        &new.package,
        PROPOSAL_DIFF_BYTES,
    ))
}

fn store_error(error: SkillStoreError) -> ScheduleStoreError {
    match error {
        SkillStoreError::Backend(error) => ScheduleStoreError::Backend(error),
        SkillStoreError::NotFound => ScheduleStoreError::NotFound,
        _ => ScheduleStoreError::Conflict,
    }
}

/// Decides a pending proposal of `actor_id`. Approval writes the new version
/// with `origin = ai_authored`; a moved base version, a taken name or a full
/// scope marks it conflicted instead. Every final state releases its pending
/// slot, and the receipt is written into the proposing turn while it waits.
#[allow(clippy::too_many_arguments)]
pub async fn decide_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    actor_id: &str,
    proposal_id: &str,
    expected_revision: Option<i64>,
    approve: bool,
    decider: &SkillDecider,
) -> Result<proposal::Model, SkillProposalError> {
    let row = proposal::Entity::find()
        .filter(proposal::Column::ProposalId.eq(proposal_id))
        .filter(proposal::Column::ActorId.eq(actor_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if expected_revision.is_some_and(|revision| revision != row.revision) || row.status != "pending"
    {
        return Err(ScheduleStoreError::Conflict.into());
    }
    // The decision receipt writes the proposing session; take its
    // conversation lock before the proposal revision CAS.
    host.lock_conversation(txn, &row.conversation_id).await?;
    // The proposing turn pauses only when it ends; deciding before that would
    // leave the pause without a decision, so the caller retries.
    if proposing_turn_running_on(host, txn, &row).await? {
        return Err(ScheduleStoreError::Conflict.into());
    }
    let now = authority_now(txn).await?;
    let scope = personal_scope(actor_id);
    let (status, version) = if !approve {
        (SkillProposalStatus::Rejected, None)
    } else {
        approve_on(txn, &scope, &row, now).await?
    };
    // Only fixed server text is kept as a reason; an AI reviewer's free text
    // may come from material that cannot be kept, so only its closed code is.
    let reason = match (status, decider) {
        (SkillProposalStatus::Conflicted, _) if is_removal(parse_kind(&row.kind)) => Some(
            "The skill changed, was deleted, or is no longer agent-written; nothing was changed."
                .to_owned(),
        ),
        (SkillProposalStatus::Conflicted, _) => Some(
            "The skill changed, its name is taken, or the skill limit is reached; propose again based on the current version."
                .to_owned(),
        ),
        _ => None,
    };
    let reason_code = match (status, decider) {
        (SkillProposalStatus::Conflicted, _) | (_, SkillDecider::Owner) => None,
        (_, SkillDecider::Ai(ai)) => Some(ai.reason_code.clone()),
    };
    let decision_source = Some(match decider {
        SkillDecider::Ai(_) => "ai_review",
        SkillDecider::Owner => "owner",
    });
    let changed = proposal::Entity::update_many()
        .col_expr(proposal::Column::Status, Expr::value(status_name(status)))
        .col_expr(
            proposal::Column::DecisionSource,
            Expr::value(decision_source.map(str::to_owned)),
        )
        .col_expr(
            proposal::Column::DecisionReason,
            Expr::value(reason.clone()),
        )
        .col_expr(
            proposal::Column::DecisionReasonCode,
            Expr::value(reason_code),
        )
        .col_expr(proposal::Column::DecidedAtMs, Expr::value(Some(now)))
        .col_expr(proposal::Column::Revision, Expr::value(row.revision + 1))
        .filter(proposal::Column::Id.eq(row.id))
        .filter(proposal::Column::Revision.eq(row.revision))
        .filter(proposal::Column::Status.eq("pending"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict.into());
    }
    adjust_scope_on(
        txn,
        &scope,
        ScopeDelta {
            pending: -1,
            ..Default::default()
        },
    )
    .await?;
    let decided = proposal::Entity::find_by_id(row.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    write_receipt_on(host, txn, &decided, status, version, decider, now).await?;
    Ok(decided)
}

/// Writes the approved version, or reports why it conflicts.
async fn approve_on<C: ConnectionTrait>(
    txn: &C,
    scope: &str,
    row: &proposal::Model,
    now: i64,
) -> Result<(SkillProposalStatus, Option<(String, u64)>), SkillProposalError> {
    let kind = parse_kind(&row.kind);
    if is_removal(kind) {
        return Ok((approve_removal_on(txn, scope, row, kind, now).await?, None));
    }
    let input = stored_input(row)?;
    let package = parse_input(&input, Some(&row.name))
        .map_err(|_| ScheduleStoreError::Invalid)?
        .package;
    let source = VersionSource {
        origin: SkillOrigin::AiAuthored,
        proposal_id: Some(row.proposal_id.clone()),
        source_policy_json: Some(row.source_policy_json.clone()),
        scan_summary_json: None,
    };
    let saved = match (&row.base_skill_id, row.base_version) {
        (Some(skill_id), Some(base_version)) => match skill_on(txn, scope, skill_id).await {
            Ok(current) if current.current_version == base_version => {
                update_on(
                    txn,
                    scope,
                    skill_id,
                    current.revision as u64,
                    None,
                    Some((&package, &source)),
                    now,
                )
                .await
            }
            Ok(_) | Err(SkillStoreError::NotFound) => {
                return Ok((SkillProposalStatus::Conflicted, None));
            }
            Err(error) => return Err(error.into()),
        },
        _ => create_on(txn, scope, &package, &source, now).await,
    };
    match saved {
        Ok(summary) => Ok((
            SkillProposalStatus::Approved,
            Some((summary.skill_id, summary.current_version)),
        )),
        Err(SkillStoreError::NameTaken | SkillStoreError::Limit(_)) => {
            Ok((SkillProposalStatus::Conflicted, None))
        }
        Err(error) => Err(error.into()),
    }
}

/// Disables or deletes the proposed skill when it is still the reviewed,
/// agent-written version; otherwise the proposal conflicts.
async fn approve_removal_on<C: ConnectionTrait>(
    txn: &C,
    scope: &str,
    row: &proposal::Model,
    kind: SkillProposalKind,
    now: i64,
) -> Result<SkillProposalStatus, SkillProposalError> {
    let (Some(skill_id), Some(base_version)) = (&row.base_skill_id, row.base_version) else {
        return Ok(SkillProposalStatus::Conflicted);
    };
    let current = match skill_on(txn, scope, skill_id).await {
        Ok(current) => current,
        Err(SkillStoreError::NotFound) => return Ok(SkillProposalStatus::Conflicted),
        Err(error) => return Err(error.into()),
    };
    if current.current_version != base_version
        || (kind == SkillProposalKind::Disable && !current.enabled)
        || !agent_written_on(txn, &current, kind).await?
    {
        return Ok(SkillProposalStatus::Conflicted);
    }
    let revision = current.revision as u64;
    let changed = match kind {
        SkillProposalKind::Disable => {
            update_on(txn, scope, skill_id, revision, Some(false), None, now)
                .await
                .map(drop)
        }
        _ => delete_on(txn, scope, skill_id, revision).await,
    };
    match changed {
        Ok(()) => Ok(SkillProposalStatus::Approved),
        Err(SkillStoreError::Conflict | SkillStoreError::NotFound) => {
            Ok(SkillProposalStatus::Conflicted)
        }
        Err(error) => Err(error.into()),
    }
}

/// Whether the conversation that proposed `row` still runs a turn.
async fn proposing_turn_running_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    row: &proposal::Model,
) -> Result<bool, SkillProposalError> {
    let Some(session_row) = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &row.conversation_id,
                actor: Some(&row.actor_id),
                device: None,
            },
            OrgMode::PersonalOnly,
            false,
        )
        .await?
    else {
        return Ok(false);
    };
    Ok(PersistedAgentSession::decode_json(&session_row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?
        .turn_state
        .is_active())
}

/// Appends the decision receipt and records the resume only while the
/// proposing conversation is paused on this proposal.
async fn write_receipt_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    row: &proposal::Model,
    status: SkillProposalStatus,
    version: Option<(String, u64)>,
    decider: &SkillDecider,
    now: i64,
) -> Result<(), SkillProposalError> {
    let Some(session_row) = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &row.conversation_id,
                actor: Some(&row.actor_id),
                device: None,
            },
            OrgMode::PersonalOnly,
            false,
        )
        .await?
    else {
        return Ok(());
    };
    let session = PersistedAgentSession::decode_json(&session_row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if !crate::decision_resume::paused_on(&session, DecisionKind::Skill, &row.proposal_id) {
        return Ok(());
    }
    let loaded = version.as_ref().and_then(|(skill_id, _)| {
        session
            .skills
            .loaded_stored(session.input_revision)
            .into_iter()
            .find(|(loaded, _)| loaded == skill_id)
            .map(|(_, loaded)| loaded)
    });
    let note = match (loaded, &version) {
        (Some(loaded), Some((_, current))) => Some(format!(
            "This request keeps using the loaded version {loaded}; new owner messages use version {current}."
        )),
        (None, Some(_)) => {
            Some("The skill appears in the skill catalogue from the next model call.".to_owned())
        }
        _ => None,
    };
    // A conflict's reason is fixed server text, not the reviewer's.
    let ai = match (status, decider) {
        (SkillProposalStatus::Conflicted, _) | (_, SkillDecider::Owner) => None,
        (_, SkillDecider::Ai(ai)) => Some(ai),
    };
    let id = format!("skill-decision:{}:{}", row.proposal_id, row.revision);
    let text = serde_json::json!({
        "event": SKILL_DECIDED_EVENT,
        "proposal_id": row.proposal_id,
        "status": status_name(status),
        "kind": row.kind,
        "skill_ref": format!("personal/{}", row.name),
        "version": version.as_ref().map(|(_, version)| version),
        "decided_by": match decider {
            SkillDecider::Ai(_) => "approval_ai",
            SkillDecider::Owner => "owner",
        },
        // The receipt carries the reviewer's full reason for this request.
        "reason": ai.map(|ai| ai.reason.as_str()).or(row.decision_reason.as_deref()),
        "reason_code": ai.map(|ai| &ai.reason_code),
        "note": note,
        "authority": "guidance_only_no_tool_permissions",
        "instruction": if is_removal(parse_kind(&row.kind)) {
            "This server receipt is the final decision."
        } else {
            "This server receipt is the final decision. A saved skill is guidance only; every action it asks for still needs its normal approval."
        },
    })
    .to_string();
    let last_labelled = session
        .conversation
        .iter()
        .rev()
        .find_map(|message| message.data_envelope.as_ref());
    let mut message = ChatMessage::system_event(&id, &text);
    desk_diagnose_core::proposal_review::provenance::decision_receipt_envelope(
        &session,
        // The receipt is written only while the turn waits on this proposal.
        desk_diagnose_core::proposal_review::provenance::proposing_output(&session),
        ai,
        last_labelled,
        &id,
        &text,
        desk_diagnose_core::model_message_labels::ServerResultSource::SkillProposalDecision,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?
    .apply(&mut message);
    crate::decision_resume::record_decision_on(host, txn, &session_row, session, message, now)
        .await?;
    Ok(())
}

/// The AI reviewer's verdict on the exact reviewed revision, in the caller's
/// review-settlement transaction.
pub async fn decide_by_ai_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    proposal: &SkillProposal,
    approve: bool,
    ai: &desk_diagnose_core::proposal_review::provenance::AiDecisionReason,
) -> Result<(), SkillProposalError> {
    decide_on(
        host,
        txn,
        &proposal.owner_id,
        &proposal.proposal_id,
        Some(proposal.revision),
        approve,
        &SkillDecider::Ai(ai.clone()),
    )
    .await?;
    Ok(())
}

/// Proposals of `actor_id`, newest first: only undecided ones, or every
/// proposal of one conversation. `awaiting_owner` is left for the host.
pub async fn list_on<C: ConnectionTrait>(
    db: &C,
    actor_id: &str,
    conversation_id: Option<&str>,
) -> Result<Vec<SkillProposalView>, SkillProposalError> {
    let mut select = proposal::Entity::find().filter(proposal::Column::ActorId.eq(actor_id));
    select = match conversation_id {
        Some(conversation) => select.filter(proposal::Column::ConversationId.eq(conversation)),
        None => select.filter(proposal::Column::Status.eq("pending")),
    };
    let rows = select
        .order_by_desc(proposal::Column::CreatedAtMs)
        .limit(32)
        .all(db)
        .await?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(view_on(db, row).await?);
    }
    Ok(views)
}

pub async fn view_on<C: ConnectionTrait>(
    db: &C,
    row: proposal::Model,
) -> Result<SkillProposalView, SkillProposalError> {
    let input = stored_input(&row)?;
    let diff = diff_of_on(db, &row, &input).await?;
    let status = parse_status(&row.status);
    Ok(SkillProposalView {
        proposal_id: row.proposal_id,
        conversation_id: row.conversation_id,
        kind: parse_kind(&row.kind),
        name: row.name,
        reason: row.reason,
        status,
        decision_source: row.decision_source.as_deref().and_then(parse_decision),
        decision_reason: row.decision_reason,
        decision_reason_code: row.decision_reason_code,
        base_skill_id: row.base_skill_id,
        base_version: row.base_version.map(|v| v as u64),
        skill_md: input.skill_md.unwrap_or_default(),
        files: input.files,
        diff,
        created_at_ms: row.created_at_ms,
        decided_at_ms: row.decided_at_ms,
        revision: row.revision as u64,
        awaiting_owner: false,
    })
}

/// The retention policy of an AI-authored skill message, restored from the
/// stored proof of the version it names. `None` when the label does not name
/// a current AI-authored version of this actor's personal skills.
pub async fn authored_policy_on<C: ConnectionTrait>(
    db: &C,
    actor_id: &str,
    source: &desk_agent_protocol::data_lineage::DataEnvelope,
) -> Result<Option<desk_agent_protocol::ai_agent::source::SourceUsePolicy>, DbErr> {
    let Some((skill_id, number)) = source
        .provenance
        .source_object_id
        .as_deref()
        .and_then(|id| {
            desk_diagnose_core::skills::authored_version_of(
                &source.provenance.source_provider_id,
                &source.provenance.source_tool_name,
                id,
            )
        })
    else {
        return Ok(None);
    };
    let owned = skill::Entity::find()
        .filter(skill::Column::SkillId.eq(&skill_id))
        .filter(skill::Column::ScopeKey.eq(personal_scope(actor_id)))
        .one(db)
        .await?
        .is_some();
    if !owned {
        return Ok(None);
    }
    let Some(row) = version::Entity::find()
        .filter(version::Column::SkillId.eq(&skill_id))
        .filter(version::Column::Version.eq(number as i64))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    Ok(row
        .source_policy_json
        .as_deref()
        .filter(|_| parse_origin(&row.origin) == SkillOrigin::AiAuthored)
        .and_then(|json| serde_json::from_str::<AuthoredSkillSource>(json).ok())
        .filter(|proof| proof.validate(actor_id).is_ok())
        .map(|proof| proof.policy))
}

#[cfg(test)]
mod tests;
