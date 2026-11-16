//! Production code writes the model-visible fields of a message only through
//! typed content and `ChatMessage`'s named mutators, and marks text unverified
//! only at fixed sites. Tests may build fixtures freely.

/// Writes of model-visible message fields, and text marked unverified.
const PATTERNS: &[&str] = &[
    ".data_envelope = ",
    ".text = ",
    ".image_data_url = ",
    ".stored_roots = ",
    ".text.push_str(",
    ".text.push(",
    ".data_envelope.as_mut()",
    ".data_envelope.take()",
    "Unverified::new(",
];

/// The only production sites of those patterns, as (file relative to the web workspace, pattern,
/// occurrences):
/// - `ChatMessage`'s own mutators in `chat.rs`, and typed content in
///   `model_content.rs` and `runtime_context.rs`;
/// - same-named fields of other types: compression messages, streaming text
///   buffers and the label of a model response's metadata;
/// - each site that marks text unverified.
const ALLOWED: &[(&str, &str, usize)] = &[
    (
        "assistant-data-store/src/schedule/resume_claim.rs",
        "Unverified::new(",
        1,
    ),
    ("diagnose-core/src/action_result.rs", "Unverified::new(", 1),
    ("diagnose-core/src/agent_loop.rs", ".data_envelope = ", 3),
    ("diagnose-core/src/agent_loop.rs", "Unverified::new(", 3),
    (
        "diagnose-core/src/agent_loop/assistant_data.rs",
        ".data_envelope = ",
        1,
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
    ("diagnose-core/src/chat.rs", ".data_envelope = ", 3),
    ("diagnose-core/src/chat.rs", ".image_data_url = ", 2),
    ("diagnose-core/src/chat.rs", ".stored_roots = ", 1),
    ("diagnose-core/src/chat.rs", ".text = ", 3),
    ("diagnose-core/src/model_content.rs", ".data_envelope = ", 3),
    ("diagnose-core/src/model_content.rs", ".stored_roots = ", 2),
    ("diagnose-core/src/model_content.rs", ".text = ", 2),
    ("diagnose-core/src/model_content.rs", ".text.push_str(", 6),
    (
        "diagnose-core/src/model_content/tool_error.rs",
        "Unverified::new(",
        1,
    ),
    (
        "diagnose-core/src/model_context/checkpoint.rs",
        ".text = ",
        1,
    ),
    (
        "diagnose-core/src/proposal_review/provenance.rs",
        "Unverified::new(",
        1,
    ),
    ("diagnose-core/src/runtime_context.rs", ".text.push_str(", 1),
    ("diagnose-core/src/seam.rs", "Unverified::new(", 1),
    (
        "diagnose-core/src/terminal_ai_assistant.rs",
        ".text.push_str(",
        1,
    ),
    ("signal/src/assistant_model.rs", ".data_envelope = ", 1),
    (
        "signal/src/capability_grant_store/computer_delivery.rs",
        "Unverified::new(",
        1,
    ),
    ("signal/src/model_dial.rs", ".text.push_str(", 2),
];

/// The production part of a source file: test files and the trailing test
/// module are left out. Whitespace before a `.` is removed, so a chained
/// write split across lines is still found.
fn production(path: &std::path::Path, text: &str) -> Option<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if path.components().any(|part| part.as_os_str() == "tests")
        || name.ends_with("tests.rs")
        || name.starts_with("test_")
    {
        return None;
    }
    let end = [
        "\n#[cfg(test)]\nmod tests",
        "\n#[cfg(test)]\npub(crate) mod tests",
    ]
    .iter()
    .filter_map(|marker| text.find(marker))
    .min()
    .unwrap_or(text.len());
    let mut joined = String::with_capacity(end);
    let mut pending = false;
    for character in text[..end].chars() {
        if character.is_whitespace() {
            pending = true;
            continue;
        }
        if pending && character != '.' {
            joined.push(' ');
        }
        pending = false;
        joined.push(character);
    }
    Some(joined)
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
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let Some(production) = production(&path, &text) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        for pattern in PATTERNS {
            let count = production.matches(pattern).count();
            if count > 0 {
                found.push((relative.clone(), pattern, count));
            }
        }
    }
}

#[test]
fn production_writes_message_fields_only_through_typed_content_and_mutators() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut found = Vec::new();
    for dir in [
        "diagnose-core/src",
        "signal/src",
        "assistant-data-store/src",
    ] {
        scan(&root.join(dir), root, &mut found);
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
