//! Fixed call sites that write model-visible message fields or mark text
//! unverified.
//!
//! Model-visible text and its label come from [`super::ModelContent`]. The
//! sites below write a message's text, image, tool calls or label directly,
//! or name text as unverified, for projections, egress and recovery that
//! predate typed content. A new site fails this test: build the message from
//! typed content instead, or review the site and update the list.

/// Patterns that bypass typed content.
const PATTERNS: &[&str] = &[
    ".data_envelope = ",
    ".text = ",
    ".image_data_url = ",
    ".tool_calls = ",
    "Unverified::new(",
    "String::leak",
    "unlabelled_outside_assistant",
    "project_text",
];

/// (file relative to the web workspace, pattern, occurrences).
const ALLOWED: &[(&str, &str, usize)] = &[
    (
        "assistant-data-store/src/main_request/coordination.rs",
        ".data_envelope = ",
        3,
    ),
    (
        "assistant-data-store/src/main_request/coordination/report_reads.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "assistant-data-store/src/schedule/proposal.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "assistant-data-store/src/schedule/resume_claim.rs",
        "Unverified::new(",
        1,
    ),
    ("diagnose-core/src/action_result.rs", ".data_envelope = ", 2),
    ("diagnose-core/src/action_result.rs", ".tool_calls = ", 1),
    ("diagnose-core/src/action_result.rs", "Unverified::new(", 2),
    ("diagnose-core/src/agent_loop.rs", ".data_envelope = ", 4),
    ("diagnose-core/src/agent_loop.rs", ".image_data_url = ", 4),
    ("diagnose-core/src/agent_loop.rs", ".text = ", 2),
    ("diagnose-core/src/agent_loop.rs", "Unverified::new(", 7),
    (
        "diagnose-core/src/agent_loop/assistant_data/current.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/agent_loop/phases.rs",
        ".data_envelope = ",
        3,
    ),
    (
        "diagnose-core/src/agent_loop/phases.rs",
        "Unverified::new(",
        1,
    ),
    (
        "diagnose-core/src/agent_loop/read_call.rs",
        "Unverified::new(",
        1,
    ),
    (
        "diagnose-core/src/agent_loop/retrieval_accounting.rs",
        ".data_envelope = ",
        1,
    ),
    ("diagnose-core/src/ai_assistant.rs", ".text = ", 2),
    (
        "diagnose-core/src/ai_assistant/read_projection.rs",
        ".data_envelope = ",
        3,
    ),
    (
        "diagnose-core/src/ai_assistant/read_projection.rs",
        ".text = ",
        1,
    ),
    (
        "diagnose-core/src/approval_egress.rs",
        ".data_envelope = ",
        16,
    ),
    (
        "diagnose-core/src/approval_egress.rs",
        ".image_data_url = ",
        1,
    ),
    ("diagnose-core/src/approval_egress.rs", ".text = ", 1),
    ("diagnose-core/src/assistant_data_model.rs", ".text = ", 1),
    (
        "diagnose-core/src/browser_model_ids.rs",
        ".data_envelope = ",
        2,
    ),
    ("diagnose-core/src/browser_model_ids.rs", ".text = ", 1),
    (
        "diagnose-core/src/browser_model_ids.rs",
        "Unverified::new(",
        2,
    ),
    ("diagnose-core/src/chat.rs", ".image_data_url = ", 1),
    (
        "diagnose-core/src/command_completion.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "diagnose-core/src/command_completion.rs",
        "Unverified::new(",
        1,
    ),
    ("diagnose-core/src/context_usage.rs", ".text = ", 2),
    (
        "diagnose-core/src/conversation_attachment/delivery.rs",
        ".data_envelope = ",
        5,
    ),
    (
        "diagnose-core/src/conversation_attachment/delivery.rs",
        ".text = ",
        4,
    ),
    (
        "diagnose-core/src/conversation_attachment/model_read.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "diagnose-core/src/conversation_attachment/model_read.rs",
        ".text = ",
        1,
    ),
    (
        "diagnose-core/src/conversation_image.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/conversation_image.rs",
        ".image_data_url = ",
        1,
    ),
    ("diagnose-core/src/conversation_title.rs", ".text = ", 2),
    (
        "diagnose-core/src/directory_review.rs",
        ".data_envelope = ",
        4,
    ),
    ("diagnose-core/src/directory_review.rs", ".text = ", 1),
    (
        "diagnose-core/src/directory_tools.rs",
        ".data_envelope = ",
        1,
    ),
    ("diagnose-core/src/document_model_ids.rs", ".text = ", 1),
    (
        "diagnose-core/src/document_preview.rs",
        ".data_envelope = ",
        1,
    ),
    ("diagnose-core/src/goal.rs", ".tool_calls = ", 1),
    ("diagnose-core/src/goal_review.rs", "Unverified::new(", 1),
    ("diagnose-core/src/image_input.rs", ".data_envelope = ", 4),
    ("diagnose-core/src/image_input.rs", ".image_data_url = ", 1),
    (
        "diagnose-core/src/issue_authority/work_source.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/issue_authority/workset.rs",
        ".data_envelope = ",
        2,
    ),
    ("diagnose-core/src/model_content.rs", ".data_envelope = ", 3),
    ("diagnose-core/src/model_content.rs", ".text = ", 2),
    (
        "diagnose-core/src/model_content/tool_error.rs",
        "Unverified::new(",
        1,
    ),
    (
        "diagnose-core/src/model_context/checkpoint.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/model_context/checkpoint.rs",
        ".image_data_url = ",
        1,
    ),
    (
        "diagnose-core/src/model_context/checkpoint.rs",
        ".text = ",
        2,
    ),
    (
        "diagnose-core/src/model_context/checkpoint/egress.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "diagnose-core/src/model_controls.rs",
        ".data_envelope = ",
        1,
    ),
    ("diagnose-core/src/model_controls.rs", ".text = ", 2),
    ("diagnose-core/src/model_egress.rs", ".data_envelope = ", 12),
    ("diagnose-core/src/model_egress.rs", ".text = ", 2),
    ("diagnose-core/src/model_egress.rs", ".tool_calls = ", 2),
    ("diagnose-core/src/model_egress.rs", "Unverified::new(", 1),
    (
        "diagnose-core/src/model_egress/issue_reply.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/model_egress/lineage.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/model_message_labels.rs",
        ".data_envelope = ",
        4,
    ),
    (
        "diagnose-core/src/model_message_labels.rs",
        "Unverified::new(",
        5,
    ),
    (
        "diagnose-core/src/permission_resume.rs",
        ".data_envelope = ",
        4,
    ),
    (
        "diagnose-core/src/proposal_review/projection.rs",
        ".data_envelope = ",
        5,
    ),
    (
        "diagnose-core/src/proposal_review/projection.rs",
        ".text = ",
        4,
    ),
    (
        "diagnose-core/src/proposal_review/provenance.rs",
        ".data_envelope = ",
        6,
    ),
    (
        "diagnose-core/src/proposal_review/provenance.rs",
        ".text = ",
        1,
    ),
    (
        "diagnose-core/src/proposal_review/provenance.rs",
        "Unverified::new(",
        2,
    ),
    (
        "diagnose-core/src/provider_preflight/batch_document.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "diagnose-core/src/provider_preflight/text_file.rs",
        ".data_envelope = ",
        6,
    ),
    (
        "diagnose-core/src/provider_preflight/text_file.rs",
        ".text = ",
        1,
    ),
    (
        "diagnose-core/src/provider_preflight/text_file.rs",
        "Unverified::new(",
        4,
    ),
    ("diagnose-core/src/result_model_ids.rs", ".text = ", 9),
    (
        "diagnose-core/src/runtime_context.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/schedule/published_input.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/schedule/review_result.rs",
        ".data_envelope = ",
        4,
    ),
    ("diagnose-core/src/schedule/review_result.rs", ".text = ", 2),
    ("diagnose-core/src/seam.rs", "Unverified::new(", 1),
    ("diagnose-core/src/session.rs", ".data_envelope = ", 8),
    ("diagnose-core/src/session.rs", ".image_data_url = ", 2),
    ("diagnose-core/src/session.rs", ".text = ", 2),
    ("diagnose-core/src/skill_scan.rs", ".data_envelope = ", 1),
    ("diagnose-core/src/skills.rs", ".data_envelope = ", 3),
    (
        "diagnose-core/src/subagent/creation.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "diagnose-core/src/subagent/creation.rs",
        ".tool_calls = ",
        2,
    ),
    (
        "diagnose-core/src/subagent/device_wait.rs",
        ".data_envelope = ",
        2,
    ),
    ("diagnose-core/src/subagent/device_wait.rs", ".text = ", 1),
    (
        "diagnose-core/src/subagent/projection.rs",
        ".data_envelope = ",
        6,
    ),
    ("diagnose-core/src/subagent/projection.rs", ".text = ", 1),
    (
        "diagnose-core/src/subagent/projection.rs",
        "Unverified::new(",
        4,
    ),
    ("diagnose-core/src/ui_model_ids.rs", ".text = ", 10),
    ("diagnose-core/src/ui_references.rs", ".data_envelope = ", 3),
    ("diagnose-core/src/ui_references.rs", ".text = ", 14),
    ("diagnose-core/src/ui_references.rs", "Unverified::new(", 2),
    (
        "diagnose-core/src/visual_evidence.rs",
        ".data_envelope = ",
        1,
    ),
    ("diagnose-core/src/visual_evidence.rs", ".text = ", 1),
    (
        "signal/src/agent_run_event_store.rs",
        ".data_envelope = ",
        3,
    ),
    ("signal/src/agent_session_store.rs", ".data_envelope = ", 2),
    (
        "signal/src/agent_subagent_store/lifecycle.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "signal/src/agent_subagent_store/main_tools.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "signal/src/agent_subagent_store/notification.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "signal/src/agent_subagent_store/parent_wait.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "signal/src/agent_subagent_store/parent_wait.rs",
        ".text = ",
        1,
    ),
    (
        "signal/src/ai_assistant_orchestrator.rs",
        ".data_envelope = ",
        2,
    ),
    ("signal/src/assistant_model.rs", ".data_envelope = ", 1),
    (
        "signal/src/capability_grant_store.rs",
        ".data_envelope = ",
        2,
    ),
    (
        "signal/src/capability_grant_store/computer_delivery.rs",
        ".data_envelope = ",
        1,
    ),
    (
        "signal/src/capability_grant_store/computer_delivery.rs",
        ".text = ",
        1,
    ),
    (
        "signal/src/capability_grant_store/computer_delivery.rs",
        "Unverified::new(",
        1,
    ),
    (
        "signal/src/controller/ai_assistant_session.rs",
        ".data_envelope = ",
        3,
    ),
    (
        "signal/src/controller/ai_assistant_session.rs",
        ".image_data_url = ",
        1,
    ),
    ("signal/src/model_egress_store.rs", ".data_envelope = ", 1),
    ("signal/src/remote_tool_edge.rs", ".image_data_url = ", 1),
];

fn is_test_file(path: &std::path::Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    path.components().any(|part| part.as_os_str() == "tests")
        || name.ends_with("tests.rs")
        || name == "test_host.rs"
}

fn scan(
    dir: &std::path::Path,
    root: &std::path::Path,
    found: &mut Vec<(String, &'static str, usize)>,
) {
    let mut entries = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            scan(&path, root, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") && !is_test_file(&path) {
            let text = std::fs::read_to_string(&path).unwrap();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for pattern in PATTERNS {
                let count = text.matches(pattern).count();
                if count > 0 {
                    found.push((relative.clone(), pattern, count));
                }
            }
        }
    }
}

#[test]
fn writes_that_bypass_typed_content_stay_at_their_fixed_sites() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut found = Vec::new();
    for crate_dir in [
        "diagnose-core/src",
        "signal/src",
        "assistant-data-store/src",
    ] {
        scan(&root.join(crate_dir), root, &mut found);
    }
    found.sort();
    let allowed = ALLOWED
        .iter()
        .map(|(path, pattern, count)| (path.to_string(), *pattern, *count))
        .collect::<Vec<_>>();
    let added = found
        .iter()
        .filter(|site| !allowed.contains(site))
        .collect::<Vec<_>>();
    let removed = allowed
        .iter()
        .filter(|site| !found.contains(site))
        .collect::<Vec<_>>();
    assert!(
        added.is_empty() && removed.is_empty(),
        "sites changed; added or grown: {added:#?}; removed or shrunk: {removed:#?}"
    );
}
