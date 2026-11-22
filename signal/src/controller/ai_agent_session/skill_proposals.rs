//! The owner's view and decision of AI-proposed personal skills.

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::ai_agent::skill::{SkillProposalDecision, SkillProposalView};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

use super::TAG;
use crate::agent_skills::ProposalDecisionError;
use crate::error::DeskSignalError;

#[utoipa::path(
    tag = TAG,
    summary = "List undecided AI-proposed personal skills",
    responses((status = 200, body = RestResponse<Vec<SkillProposalView>>)),
)]
#[get("/my/ai-agent-session/skill-proposals")]
pub async fn list_ai_agent_skill_proposals() -> Result<HttpResponse, DeskSignalError> {
    let proposals = crate::agent_skills::pending_proposals(crate::db::get_db())
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Skill proposals are unavailable",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(proposals)))
}

#[utoipa::path(
    tag = TAG,
    summary = "Approve or reject an AI-proposed personal skill",
    request_body = SkillProposalDecision,
    responses((status = 200, body = RestResponse<SkillProposalView>)),
)]
#[post("/my/ai-agent-session/skill-proposal-decision")]
pub async fn decide_ai_agent_skill_proposal(
    body: web::Json<SkillProposalDecision>,
) -> Result<HttpResponse, DeskSignalError> {
    let view = crate::agent_skills::decide_proposal(crate::db::get_db(), &body)
        .await
        .map_err(|error| match error {
            ProposalDecisionError::NotFound => DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Skill proposal is no longer pending; refresh the conversation",
            ),
            ProposalDecisionError::Conflict => DeskSignalError::new_custom_error(
                DeskErrorCode::REVISION_CONFLICT,
                "Skill proposal changed; refresh and retry",
            ),
            ProposalDecisionError::Backend(_) => DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Skill proposals are unavailable",
            ),
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(view)))
}
