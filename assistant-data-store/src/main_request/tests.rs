use super::*;

#[test]
fn queued_receipts_show_only_owner_text() {
    let owner = serde_json::json!({
        "ask": { "question": "Check the disk", "client_message_id": "m1" },
        "device_id": "",
        "targets": [],
    });
    assert_eq!(
        owner_question(&owner.to_string()).unwrap().as_deref(),
        Some("Check the disk")
    );
    let subscription = serde_json::json!({
        "issue_source": { "request_id": "issue-1", "anything": ["kept opaque"] },
        "ask": { "question": "Generated requirement" },
    });
    assert_eq!(owner_question(&subscription.to_string()).unwrap(), None);
    assert!(owner_question("{}").is_err());
}
