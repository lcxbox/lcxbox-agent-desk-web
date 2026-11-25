//! Ordered owner questions, responses and model-visible results.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const TIMEOUT_REASON: &str = "user may be away";
pub const MAX_QUESTIONS: usize = 5;
pub const MAX_OPTIONS: usize = 4;
pub const MAX_QUESTION_CHARS: usize = 512;
pub const MAX_OPTION_CHARS: usize = 128;
pub const MAX_ANSWER_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerQuestions {
    pub questions: Vec<OwnerQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerQuestion {
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerAnswer {
    Choice { value: String },
    Text { value: String },
    Skipped,
}

/// Interact stops the deadline for an open goal prompt without submitting an answer.
/// Only a host can settle a timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerQuestionAction {
    Interact {},
    Answer { answers: Vec<OwnerAnswer> },
    Decline,
    Reply { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerQuestionResult {
    Answered { answers: Vec<OwnerAnswer> },
    Declined,
    TimedOut { reason: String },
}

impl OwnerQuestionResult {
    pub fn timed_out() -> Self {
        Self::TimedOut {
            reason: TIMEOUT_REASON.to_owned(),
        }
    }
}

fn valid_text(text: &str, max_chars: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max_chars && !text.contains('\0')
}

impl OwnerQuestions {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=MAX_QUESTIONS).contains(&self.questions.len()) {
            return Err("provide between one and five questions");
        }
        for question in &self.questions {
            if !valid_text(&question.question, MAX_QUESTION_CHARS) {
                return Err("provide a nonblank question of at most 512 characters");
            }
            if let Some(options) = &question.options {
                if !(2..=MAX_OPTIONS).contains(&options.len()) {
                    return Err("provide two to four options or omit options");
                }
                for (i, option) in options.iter().enumerate() {
                    if !valid_text(option, MAX_OPTION_CHARS)
                        || options[..i]
                            .iter()
                            .any(|prior| prior.trim() == option.trim())
                    {
                        return Err("provide distinct nonblank options of at most 128 characters");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_answers(&self, answers: &[OwnerAnswer]) -> Result<(), &'static str> {
        Self::validate_ordered_answers(&self.questions, answers)
    }

    pub fn validate_ordered_answers(
        questions: &[OwnerQuestion],
        answers: &[OwnerAnswer],
    ) -> Result<(), &'static str> {
        if questions.len() != answers.len() {
            return Err("supply one answer or skip for each question, in order");
        }
        for (question, answer) in questions.iter().zip(answers) {
            match answer {
                OwnerAnswer::Choice { value }
                    if !question
                        .options
                        .as_ref()
                        .is_some_and(|options| options.contains(value)) =>
                {
                    return Err("choice must belong to the corresponding question");
                }
                OwnerAnswer::Text { value } if !valid_text(value, MAX_ANSWER_CHARS) => {
                    return Err("provide a nonblank answer of at most 1024 characters");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// An open wait in a conversation snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerQuestionView {
    pub wait_id: String,
    pub prompt: OwnerPrompt,
    pub deadline_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerPrompt {
    Questions { questions: Vec<OwnerQuestion> },
    FollowUp { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RespondToOwnerQuestion {
    pub session: String,
    pub wait_id: String,
    pub client_request_id: String,
    pub action: OwnerQuestionAction,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    #[test]
    fn ordered_answers_preserve_skips_and_allow_free_text_for_a_choice() {
        let group: OwnerQuestions = from_value(json!({"questions":[
            {"question":"Zone?","options":["UTC","Local"]},{"question":"When?"}
        ]}))
        .unwrap();
        group.validate().unwrap();
        let answers = vec![
            OwnerAnswer::Text {
                value: "JST".into(),
            },
            OwnerAnswer::Skipped,
        ];
        group.validate_answers(&answers).unwrap();
        assert!(group.validate_answers(&answers[..1]).is_err());
        assert!(
            group
                .validate_answers(&[
                    OwnerAnswer::Skipped,
                    OwnerAnswer::Choice {
                        value: "UTC".into()
                    }
                ])
                .is_err()
        );
        assert_eq!(
            to_value(OwnerQuestionResult::Answered { answers }).unwrap(),
            json!({
                "status":"answered","answers":[{"kind":"text","value":"JST"},{"kind":"skipped"}]
            })
        );
        assert_eq!(
            to_value(OwnerQuestionResult::timed_out()).unwrap(),
            json!({
                "status":"timed_out","reason":"user may be away"
            })
        );
        assert!(from_value::<OwnerQuestionAction>(json!({"kind":"timed_out"})).is_err());
    }

    #[test]
    fn interaction_carries_no_draft_or_timeout_decision() {
        assert_eq!(
            to_value(OwnerQuestionAction::Interact {}).unwrap(),
            json!({"kind":"interact"})
        );
        assert_eq!(
            from_value::<OwnerQuestionAction>(json!({"kind":"interact"})).unwrap(),
            OwnerQuestionAction::Interact {}
        );
        assert!(
            from_value::<OwnerQuestionAction>(json!({"kind":"interact", "text":"unsent draft"}))
                .is_err()
        );
    }

    #[test]
    fn malformed_question_groups_are_rejected() {
        for questions in [
            json!([]),
            json!([{"question":" "}]),
            json!([{"question":"Q","options":[]}]),
            json!([{"question":"Q","options":["yes"," yes "]}]),
            json!(vec![json!({"question":"Q"}); 6]),
        ] {
            let group: OwnerQuestions = from_value(json!({"questions":questions})).unwrap();
            assert!(group.validate().is_err());
        }
    }
}
