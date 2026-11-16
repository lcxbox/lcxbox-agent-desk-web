//! Bounded server-generated feedback; model text never becomes an instruction.
use super::*;

pub const DECISION_TEMPLATE: &str = r#"Return exactly four fields, with no extra keys, wrappers, Markdown or code fences. verdict must be either "approve" or "deny". Example shape: {"verdict":"deny","reason_code":"owner_not_requested","reason":"Concrete justification","evidence_event_ids":["an actual supplied evidence ID"]}. reason_code is within_scope for approve and one of owner_not_requested, exceeds_task, narrower_alternative, wrong_target, unsafe_content, missing_fact, needs_owner for deny. Use only actual evidence supplied in the candidate. A correction changes output format only, never the owner's instruction or the frozen action."#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFormatIssue {
    JsonSyntax,
    ObjectRequired,
    UnexpectedFields,
    MissingFields,
    FieldSchema,
    FieldConstraints,
}

impl ReviewFormatIssue {
    pub fn feedback(self) -> &'static str {
        match self {
            Self::JsonSyntax => "The previous answer was not one complete valid JSON document.",
            Self::ObjectRequired => "The previous answer was not a JSON object.",
            Self::UnexpectedFields => {
                "The previous answer included fields outside the four allowed fields."
            }
            Self::MissingFields => {
                "The previous answer omitted one or more of the four required fields."
            }
            Self::FieldSchema => {
                "The previous answer had an invalid field type, verdict value, or duplicate field."
            }
            Self::FieldConstraints => {
                "The previous answer violated the reason or reference-count format constraints."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOutputIssue {
    Format(ReviewFormatIssue),
    Protocol,
    Semantics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptState {
    pub index: u32,
    pub retry_count: u32,
    pub runtime_revision: String,
    pub feedback: Option<ReviewFormatIssue>,
}
impl Default for ReviewAttemptState {
    fn default() -> Self {
        Self {
            index: 0,
            retry_count: 1,
            runtime_revision: "1".into(),
            feedback: None,
        }
    }
}
impl ReviewAttemptState {
    pub fn validate(&self) -> Result<(), &'static str> {
        let revision = self
            .runtime_revision
            .parse::<u64>()
            .map_err(|_| "invalid reviewer attempt")?;
        if self.index > self.retry_count
            || revision == 0
            || revision.to_string() != self.runtime_revision
            || (self.index == 0) != self.feedback.is_none()
        {
            return Err("invalid reviewer attempt");
        }
        Ok(())
    }
    pub fn next(&self, feedback: ReviewFormatIssue) -> Option<Self> {
        self.validate().ok()?;
        if self.index >= self.retry_count {
            return None;
        }
        Some(Self {
            index: self.index.checked_add(1)?,
            feedback: Some(feedback),
            ..self.clone()
        })
    }
    pub fn call_id(&self, candidate: &str) -> String {
        format!("{candidate}:attempt:{}", self.index)
    }
}

fn classify_json(error: &serde_json::Error, response: &str) -> ReviewFormatIssue {
    if error.is_syntax() || error.is_eof() {
        return ReviewFormatIssue::JsonSyntax;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return ReviewFormatIssue::JsonSyntax;
    };
    let Some(object) = value.as_object() else {
        return ReviewFormatIssue::ObjectRequired;
    };
    let fields = ["verdict", "reason_code", "reason", "evidence_event_ids"];
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        ReviewFormatIssue::UnexpectedFields
    } else if fields.iter().any(|key| !object.contains_key(*key)) {
        ReviewFormatIssue::MissingFields
    } else {
        ReviewFormatIssue::FieldSchema
    }
}

pub fn parse_decision_diagnosed(
    candidate: &ApprovalReviewCandidate,
    response: &str,
) -> Result<ApprovalReviewDecision, ReviewOutputIssue> {
    candidate
        .validate()
        .map_err(|_| ReviewOutputIssue::Semantics)?;
    let model: super::model_view::ModelDecision = serde_json::from_str(response)
        .map_err(|error| ReviewOutputIssue::Format(classify_json(&error, response)))?;
    // Reviewer codes are a closed set; approval has exactly one code.
    let code_matches = ApprovalReasonCode::parse(&model.reason_code).is_some_and(|code| {
        (code == ApprovalReasonCode::WithinScope) == (model.verdict == ApprovalVerdict::Approve)
    });
    if !code_matches
        || model.reason.trim().is_empty()
        || model.reason.len() > MAX_APPROVAL_REASON_BYTES
        || model.evidence_event_ids.len() > MAX_APPROVAL_REFERENCES
    {
        return Err(ReviewOutputIssue::Format(
            ReviewFormatIssue::FieldConstraints,
        ));
    }
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict: model.verdict,
        reason_code: model.reason_code,
        reason: model.reason,
        evidence_event_ids: model
            .evidence_event_ids
            .into_iter()
            .map(|cited| super::model_view::evidence_event_id(candidate, cited))
            .collect(),
    };
    decision
        .validate_for(candidate)
        .map_err(|_| ReviewOutputIssue::Semantics)?;
    Ok(decision)
}

pub fn reviewer_output_diagnosed(
    candidate: &ApprovalReviewCandidate,
    turn: &ModelTurn,
) -> Result<ApprovalReviewDecision, ReviewOutputIssue> {
    if turn.stop_reason != StopReason::EndTurn || !turn.tool_calls.is_empty() {
        return Err(ReviewOutputIssue::Protocol);
    }
    parse_decision_diagnosed(candidate, &turn.text)
}

pub fn format_feedback_overhead_bytes() -> usize {
    2 + DECISION_TEMPLATE.len()
        + CORRECTION_PREFIX.len()
        + CORRECTION_CONSTRAINTS.len()
        + [
            ReviewFormatIssue::JsonSyntax,
            ReviewFormatIssue::ObjectRequired,
            ReviewFormatIssue::UnexpectedFields,
            ReviewFormatIssue::MissingFields,
            ReviewFormatIssue::FieldSchema,
            ReviewFormatIssue::FieldConstraints,
        ]
        .into_iter()
        .map(|issue| issue.feedback().len())
        .max()
        .unwrap_or(0)
}
const CORRECTION_PREFIX: &str = "SERVER FORMAT CORRECTION: ";
const CORRECTION_CONSTRAINTS: &str = " Re-evaluate this same candidate and return a corrected decision. Either approve or deny is valid when justified. reason_code must be within_scope for approve, or one of owner_not_requested, exceeds_task, narrower_alternative, wrong_target, unsafe_content, missing_fact, needs_owner for deny; reason must contain 1..2048 UTF-8 bytes; evidence_event_ids must be an array of at most 32 strings.";

pub fn apply_format_feedback(request: &mut ModelRequest, issue: Option<ReviewFormatIssue>) {
    if let Some(system) = request.messages.first_mut() {
        let mut text = system.text.clone();
        text.push('\n');
        text.push_str(DECISION_TEMPLATE);
        if let Some(issue) = issue {
            text.push('\n');
            text.push_str(CORRECTION_PREFIX);
            text.push_str(issue.feedback());
            text.push_str(CORRECTION_CONSTRAINTS);
        }
        system.rewrite_instructions(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_codes_are_closed_and_match_the_verdict() {
        let candidate = super::super::tests::candidate();
        let owner = candidate.context.evidence[0].event_id.clone();
        let parse = |verdict: &str, code: &str| {
            let text = serde_json::json!({"verdict":verdict,"reason_code":code,
                "reason":"Concrete reason.","evidence_event_ids":[owner]})
            .to_string();
            parse_decision_diagnosed(&candidate, &text)
        };
        assert!(parse("approve", "within_scope").is_ok());
        assert!(parse("deny", "needs_owner").is_ok());
        for (verdict, code) in [
            ("approve", "needs_owner"),
            ("deny", "within_scope"),
            ("deny", "scope_too_broad"),
        ] {
            assert_eq!(
                parse(verdict, code),
                Err(ReviewOutputIssue::Format(
                    ReviewFormatIssue::FieldConstraints
                )),
                "{verdict} {code}"
            );
        }
    }

    fn good() -> serde_json::Value {
        serde_json::json!({"verdict":"approve","reason_code":"within_scope","reason":"The owner requested this exact bounded read.","evidence_event_ids":["owner-1"]})
    }

    #[test]
    fn distinguishes_correctable_shape_from_authority_and_protocol_errors() {
        let candidate = super::super::tests::candidate();
        let mut value = good();
        // Use the fixture's actual owner instruction rather than inventing evidence.
        value["evidence_event_ids"] = serde_json::json!([candidate
            .context
            .evidence
            .iter()
            .find(|e| e.trust == ApprovalEvidenceTrust::OwnerInstruction)
            .unwrap()
            .event_id]);
        assert!(parse_decision_diagnosed(&candidate, &value.to_string()).is_ok());
        value["type"] = serde_json::json!("json_object");
        assert_eq!(
            parse_decision_diagnosed(&candidate, &value.to_string()),
            Err(ReviewOutputIssue::Format(
                ReviewFormatIssue::UnexpectedFields
            ))
        );
        value.as_object_mut().unwrap().remove("type");
        value["evidence_event_ids"] = serde_json::json!(["invented-reference"]);
        assert_eq!(
            parse_decision_diagnosed(&candidate, &value.to_string()),
            Err(ReviewOutputIssue::Semantics)
        );
        for (text, issue) in [
            ("{", ReviewFormatIssue::JsonSyntax),
            ("[]", ReviewFormatIssue::ObjectRequired),
            ("{}", ReviewFormatIssue::MissingFields),
            (
                r#"{"verdict":"deny","verdict":"approve","reason_code":"x","reason":"x","evidence_event_ids":[]}"#,
                ReviewFormatIssue::FieldSchema,
            ),
        ] {
            assert_eq!(
                parse_decision_diagnosed(&candidate, text),
                Err(ReviewOutputIssue::Format(issue))
            );
        }
        let mut turn = ModelTurn {
            text: "{".into(),
            stop_reason: StopReason::MaxTokens,
            ..Default::default()
        };
        assert_eq!(
            reviewer_output_diagnosed(&candidate, &turn),
            Err(ReviewOutputIssue::Protocol)
        );
        turn.stop_reason = StopReason::EndTurn;
        assert_eq!(
            reviewer_output_diagnosed(&candidate, &turn),
            Err(ReviewOutputIssue::Format(ReviewFormatIssue::JsonSyntax))
        );
    }
    #[test]
    fn zero_one_and_large_retry_limits_use_checked_iteration() {
        let mut state = ReviewAttemptState {
            retry_count: 0,
            ..Default::default()
        };
        assert!(state.next(ReviewFormatIssue::JsonSyntax).is_none());
        state.retry_count = 1;
        let second = state.next(ReviewFormatIssue::UnexpectedFields).unwrap();
        assert_eq!(second.index, 1);
        assert!(second.validate().is_ok());
        assert!(second.next(ReviewFormatIssue::UnexpectedFields).is_none());
        state.retry_count = u32::MAX;
        state.index = u32::MAX - 1;
        state.feedback = Some(ReviewFormatIssue::JsonSyntax);
        let last = state.next(ReviewFormatIssue::JsonSyntax).unwrap();
        assert_eq!(last.index, u32::MAX);
        assert!(last.next(ReviewFormatIssue::JsonSyntax).is_none());
        assert_ne!(state.call_id("candidate"), last.call_id("candidate"));
    }
    #[test]
    fn complete_format_failures_can_correct_to_a_denial_but_no_other_failure_can_retry() {
        let candidate = super::super::tests::candidate();
        let owner = candidate
            .context
            .evidence
            .iter()
            .find(|entry| entry.trust == ApprovalEvidenceTrust::OwnerInstruction)
            .unwrap()
            .event_id
            .clone();
        let mut decision = good();
        decision["evidence_event_ids"] = serde_json::json!([owner]);
        let turn = |value: &serde_json::Value| ModelTurn {
            text: value.to_string(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        };
        let state = ReviewAttemptState::default();
        let mut bad = decision.clone();
        bad["type"] = serde_json::json!("json_object");
        let Err(ReviewOutputIssue::Format(issue)) =
            reviewer_output_diagnosed(&candidate, &turn(&bad))
        else {
            panic!("format feedback");
        };
        let corrected = state.next(issue).unwrap();
        assert!(corrected.next(issue).is_none());
        decision["verdict"] = serde_json::json!("deny");
        decision["reason_code"] = serde_json::json!("needs_owner");
        assert_eq!(
            reviewer_output_diagnosed(&candidate, &turn(&decision))
                .unwrap()
                .verdict,
            ApprovalVerdict::Deny
        );
        decision["verdict"] = serde_json::json!("approve");
        decision["reason_code"] = serde_json::json!("within_scope");
        for key in ["verdict", "reason", "evidence_event_ids"] {
            let mut wrong_type = decision.clone();
            wrong_type[key] = serde_json::json!(42);
            assert!(matches!(
                reviewer_output_diagnosed(&candidate, &turn(&wrong_type)),
                Err(ReviewOutputIssue::Format(_))
            ));
        }
        let mut protocol = turn(&bad);
        protocol.stop_reason = StopReason::MaxTokens;
        assert_eq!(
            reviewer_output_diagnosed(&candidate, &protocol),
            Err(ReviewOutputIssue::Protocol)
        );
        let mut untrusted = decision;
        untrusted["evidence_event_ids"] = serde_json::json!(["invented"]);
        assert_eq!(
            reviewer_output_diagnosed(&candidate, &turn(&untrusted)),
            Err(ReviewOutputIssue::Semantics)
        );
    }
}
