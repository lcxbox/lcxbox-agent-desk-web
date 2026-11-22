//! Owner-only review of persisted exact file inputs. No references or grants
//! are minted here; dispatch remains responsible for current authorization.
use desk_agent_protocol::computer_use::{TextEdit, TextFileChange, TextFileMutationOperation};
use desk_diagnose_core::chat::ChatMessage;
use serde::Serialize;
use utoipa::ToSchema;

/// One exact text change awaiting approval. An update carries either `edits`
/// or `write`; a restore carries `restore`; a deletion carries neither.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextFileConfirmationDto {
    pub file_name: String,
    /// Result the change is based on: the file version an update or deletion
    /// requires, or the operation a restore undoes.
    pub source_result_call_id: String,
    /// Version an update or deletion requires.
    pub expected_sha256: Option<String>,
    pub operation: TextFileMutationOperation,
    /// Exact edits, applied in order to the result of the previous edit.
    pub edits: Option<Vec<TextEdit>>,
    /// Complete new content of a full rewrite.
    pub write: Option<String>,
    pub restore: Option<TextFileRestoreDto>,
    pub one_shot: bool,
    /// The device keeps the previous state; the change can be restored.
    pub recoverable: bool,
}

/// What a restore puts back.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextFileRestoreDto {
    /// Operation whose previous state is restored.
    pub original_operation: TextFileMutationOperation,
    /// When the restored backup was taken.
    pub backup_created_at_unix_ms: u64,
    /// The current file is not the version that operation produced; the card
    /// must warn that it will be overwritten.
    pub overrides_non_ai_version: bool,
    /// The file must exist now (false: a deleted file is recreated).
    pub file_expected_to_exist: bool,
}

pub(super) fn project(
    tool: &str,
    canonical: Option<&str>,
    messages: &[ChatMessage],
) -> Option<TextFileConfirmationDto> {
    let review = desk_diagnose_core::provider_preflight::text_file::review_exact_input(
        tool, canonical, messages,
    )?;
    let (edits, write) = match review.change {
        Some(TextFileChange::Edit { edits }) => (Some(edits), None),
        Some(TextFileChange::Write { content_utf8 }) => (None, Some(content_utf8)),
        None => (None, None),
    };
    Some(TextFileConfirmationDto {
        file_name: review.file_name,
        source_result_call_id: review.source_result_call_id,
        expected_sha256: review.expected_sha256,
        operation: review.operation,
        edits,
        write,
        restore: review.restore.map(|restore| TextFileRestoreDto {
            original_operation: restore.original_operation,
            backup_created_at_unix_ms: restore.backup_created_at_unix_ms,
            overrides_non_ai_version: restore.overrides_non_ai_version,
            file_expected_to_exist: matches!(
                restore.expected_current,
                desk_agent_protocol::computer_use::RestoreExpectation::Sha(_)
            ),
        }),
        one_shot: true,
        recoverable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::chat::ChatRole;
    use sha2::{Digest, Sha256};

    fn fixture() -> (Vec<ChatMessage>, serde_json::Value) {
        let digest = format!("{:x}", Sha256::digest("原文".as_bytes()));
        let text = serde_json::json!({"ReadContext":{"FileContentRead":{
            "file":{"token":"file","snapshot_id":"snapshot","object_kind":"file","expires_at":"2099-01-01T00:00:00Z"},
            "display_name":"notes.txt","content_utf8":"原文","byte_len":6,"sha256":digest,
            "total_lines":1,"start_line":1,"end_line":1,"truncated":false,"next_start_byte":null,"matches":[]
        }}}).to_string();
        let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Update the selected file".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "model".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let mut receipt = ChatMessage::tool_result("receipt", "read-call", text.clone());
        receipt.data_envelope = desk_diagnose_core::model_message_labels::named_envelope(
            &desk_diagnose_core::model_content::ModelContent::builder()
                .unverified(&desk_diagnose_core::model_content::Unverified::new(&text))
                .build(),
            owner.data_envelope.as_ref(),
            "read-call",
            "read_text_file",
            false,
        )
        .unwrap();
        receipt
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = "file.content".into();
        let proposal = ChatMessage::agent_tool_calls(
            "proposal",
            "",
            vec![desk_diagnose_core::chat::ToolCallRef {
                id: "read-call".into(),
                name: "read_text_file".into(),
                arguments_json: "{}".into(),
            }],
        );
        (
            vec![proposal, receipt],
            serde_json::json!({"file_result_call_id":"read-call","expected_sha256":digest,
            "change":{"kind":"edit","edits":[{"old_text":"原文","new_text":"新文","replace_all":false}]}}),
        )
    }

    #[test]
    fn exact_file_review_carries_edits_or_write_and_uses_verified_source_name() {
        let (messages, mut input) = fixture();
        let review = project("update_text_file", Some(&input.to_string()), &messages).unwrap();
        assert_eq!(review.file_name, "notes.txt");
        assert!(review.one_shot && review.recoverable);
        assert_eq!(
            review.edits,
            Some(vec![TextEdit {
                old_text: "原文".into(),
                new_text: "新文".into(),
                replace_all: false
            }])
        );
        assert!(review.write.is_none() && review.restore.is_none());
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(json["edits"][0]["old_text"], "原文");
        assert_eq!(json["sourceResultCallId"], "read-call");
        input["change"] = serde_json::json!({"kind":"write","content_utf8":""});
        let write = project("update_text_file", Some(&input.to_string()), &messages).unwrap();
        assert_eq!(write.write.as_deref(), Some(""));
        assert!(write.edits.is_none());
        input.as_object_mut().unwrap().remove("change");
        let deleted = project("delete_text_file", Some(&input.to_string()), &messages).unwrap();
        assert_eq!(deleted.operation, TextFileMutationOperation::Delete);
        assert!(deleted.edits.is_none() && deleted.write.is_none());
    }

    #[test]
    fn missing_or_forged_file_review_is_not_approvable() {
        let (mut messages, input) = fixture();
        assert!(project("update_text_file", None, &messages).is_none());
        assert!(project("delete_text_file", Some(&input.to_string()), &messages).is_none());
        assert!(project("update_text_file", Some(&input.to_string()), &[]).is_none());
        let mut empty_edit = input.clone();
        empty_edit["change"]["edits"][0]["old_text"] = "".into();
        assert!(project("update_text_file", Some(&empty_edit.to_string()), &messages).is_none());
        messages[1].role = ChatRole::User;
        assert!(project("update_text_file", Some(&input.to_string()), &messages).is_none());
        messages[1].role = ChatRole::Tool;
        messages[1].text.push(' ');
        assert!(project("update_text_file", Some(&input.to_string()), &messages).is_none());
    }
}
