//! `read_text_file` windows: line ranges, byte continuation inside a long line
//! and literal search. The window never takes part in read authorization: a
//! grant binds the file source, and paging one source counts as one use.
use super::*;
use desk_agent_protocol::computer_use::{MAX_TEXT_READ_OUTPUT_BYTES, TextReadSelection};

/// Window parameters of `read_text_file`, excluded from authorization binding.
pub const SELECTION_FIELDS: [&str; 7] = [
    "start_line",
    "max_lines",
    "start_byte",
    "query",
    "case_sensitive",
    "context_lines",
    "max_matches",
];
const DEFAULT_CONTEXT_LINES: u32 = 2;
const MAX_CONTEXT_LINES: u32 = 10;
const DEFAULT_MAX_MATCHES: u32 = 50;
const MAX_MATCHES: u32 = 200;
const MAX_QUERY_BYTES: usize = 1024;

/// Schema properties added to `read_text_file`. Every default is declared so
/// omitted and explicit default values compare equal.
pub fn schema_properties() -> serde_json::Value {
    serde_json::json!({
        "start_line": {"type":"integer","minimum":1,"default":1,"description":"First line (1-based) of a line window."},
        "max_lines": {"type":["integer","null"],"minimum":1,"default":null,"description":"Most lines in the window; null returns as many as fit 64 KiB."},
        "start_byte": {"type":["integer","null"],"minimum":0,"maximum":MAX_TEXT_FILE_BYTES,"default":null,"description":"Continue a line longer than the output budget from the returned next_start_byte."},
        "query": {"type":["string","null"],"minLength":1,"maxLength":MAX_QUERY_BYTES,"default":null,"description":"Literal text to find; returns matches with surrounding lines instead of content."},
        "case_sensitive": {"type":"boolean","default":true,"description":"false folds ASCII case only."},
        "context_lines": {"type":"integer","minimum":0,"maximum":MAX_CONTEXT_LINES,"default":DEFAULT_CONTEXT_LINES},
        "max_matches": {"type":"integer","minimum":1,"maximum":MAX_MATCHES,"default":DEFAULT_MAX_MATCHES}
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadArgs {
    #[serde(default)]
    pub file_result_call_id: Option<String>,
    #[serde(default)]
    pub entry_name: Option<String>,
    #[serde(default)]
    start_line: Option<u64>,
    #[serde(default)]
    max_lines: Option<u64>,
    #[serde(default)]
    start_byte: Option<u64>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    context_lines: Option<u32>,
    #[serde(default)]
    max_matches: Option<u32>,
}

fn invalid(message: &str) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false, true)
}

impl ReadArgs {
    pub(super) fn parse(call: &ToolCall) -> Result<Self, AgentError> {
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())
    }

    /// Line range, byte continuation and search are mutually exclusive.
    /// Search options at their defaults are ignored without a query.
    pub(super) fn selection(&self) -> Result<TextReadSelection, AgentError> {
        let start_line = self.start_line.unwrap_or(1);
        let lines = start_line != 1 || self.max_lines.is_some();
        let search_options = self.case_sensitive.is_some_and(|value| !value)
            || self
                .context_lines
                .is_some_and(|value| value != DEFAULT_CONTEXT_LINES)
            || self
                .max_matches
                .is_some_and(|value| value != DEFAULT_MAX_MATCHES);
        let modes = [lines, self.start_byte.is_some(), self.query.is_some()];
        if modes.iter().filter(|mode| **mode).count() > 1
            || (search_options && self.query.is_none())
        {
            return Err(invalid(
                "Use one read window: start_line/max_lines, start_byte, or query with its case_sensitive/context_lines/max_matches options. No file was read.",
            ));
        }
        if start_line == 0
            || self.max_lines == Some(0)
            || self
                .start_byte
                .is_some_and(|byte| byte > MAX_TEXT_FILE_BYTES)
            || self.context_lines.unwrap_or(0) > MAX_CONTEXT_LINES
            || self
                .max_matches
                .is_some_and(|value| value == 0 || value > MAX_MATCHES)
            || self.query.as_ref().is_some_and(|query| {
                query.is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('\0')
            })
        {
            return Err(invalid(
                "A read window parameter is out of range. No file was read.",
            ));
        }
        Ok(if let Some(query) = &self.query {
            TextReadSelection::Search {
                query: query.clone(),
                case_sensitive: self.case_sensitive.unwrap_or(true),
                context_lines: self.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES),
                max_matches: self.max_matches.unwrap_or(DEFAULT_MAX_MATCHES),
            }
        } else if let Some(start_byte) = self.start_byte {
            TextReadSelection::Bytes { start_byte }
        } else if lines {
            TextReadSelection::Lines {
                start_line,
                max_lines: self.max_lines,
            }
        } else {
            TextReadSelection::Whole
        })
    }
}

/// The window a `read_text_file` call asks for.
pub fn selection(call: &ToolCall) -> Result<TextReadSelection, AgentError> {
    ReadArgs::parse(call)?.selection()
}

/// Drop the window from a read's binding input: grants bind the source only.
pub(crate) fn strip_selection(tool: &str, value: &mut serde_json::Value) {
    if tool == crate::tool_names::READ_TEXT_FILE
        && let Some(object) = value.as_object_mut()
    {
        for field in SELECTION_FIELDS {
            object.remove(field);
        }
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Shape of a read result as version evidence. The digest is checked here
/// only when the content is the whole file.
pub(super) fn validate_version(output: &FileContentReadOutput) -> Result<(), ()> {
    let content = output.content_utf8.len() as u64;
    if !is_sha256(&output.sha256)
        || output.byte_len > MAX_TEXT_FILE_BYTES
        || content > u64::from(MAX_TEXT_READ_OUTPUT_BYTES)
        || content > output.byte_len
        || output.content_utf8.contains('\0')
        || (content == output.byte_len
            && format!("{:x}", Sha256::digest(output.content_utf8.as_bytes())) != output.sha256)
    {
        return Err(());
    }
    Ok(())
}

/// Device output for one requested window.
pub(super) fn validate_output(
    selection: &TextReadSelection,
    output: &FileContentReadOutput,
) -> Result<(), ()> {
    validate_version(output)?;
    let empty = output.content_utf8.is_empty();
    let lines_ok = if empty {
        output.start_line == 0 && output.end_line == 0
    } else {
        output.start_line >= 1
            && output.start_line <= output.end_line
            && output.end_line <= output.total_lines
    };
    if !lines_ok
        || output
            .next_start_byte
            .is_some_and(|byte| !output.truncated || byte > output.byte_len)
    {
        return Err(());
    }
    match selection {
        TextReadSelection::Search { max_matches, .. } => {
            let text: usize = output.matches.iter().map(|m| m.text.len()).sum();
            if !empty
                || output.matches.len() > *max_matches as usize
                || text > MAX_TEXT_READ_OUTPUT_BYTES as usize
                || output.matches.iter().any(|m| {
                    m.line == 0 || m.line > output.total_lines || m.byte_offset >= output.byte_len
                })
            {
                return Err(());
            }
        }
        _ if !output.matches.is_empty() => return Err(()),
        TextReadSelection::Whole => {
            if output.byte_len <= u64::from(MAX_TEXT_READ_OUTPUT_BYTES)
                && (output.truncated || output.content_utf8.len() as u64 != output.byte_len)
            {
                return Err(());
            }
        }
        TextReadSelection::Lines {
            start_line,
            max_lines,
        } => {
            if !empty
                && (output.start_line != *start_line
                    || max_lines.is_some_and(|max| output.end_line - output.start_line >= max))
            {
                return Err(());
            }
        }
        TextReadSelection::Bytes { start_byte } => {
            if *start_byte > output.byte_len {
                return Err(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::TextReadMatch;

    fn call(args: serde_json::Value) -> ToolCall {
        let mut args = args;
        crate::model_input::fill_versions(crate::tool_names::READ_TEXT_FILE, &mut args);
        ToolCall {
            id: "read".into(),
            name: crate::tool_names::READ_TEXT_FILE.into(),
            arguments_json: args.to_string(),
        }
    }

    #[test]
    fn window_parameters_map_to_one_exclusive_selection() {
        let base = serde_json::json!({"file_result_call_id":"r"});
        assert_eq!(
            selection(&call(base.clone())).unwrap(),
            TextReadSelection::Whole
        );
        // Declared defaults are the same request as omission.
        assert_eq!(
            selection(&call(serde_json::json!({"file_result_call_id":"r","start_line":1,"case_sensitive":true,"context_lines":2,"max_matches":50}))).unwrap(),
            TextReadSelection::Whole
        );
        assert_eq!(
            selection(&call(
                serde_json::json!({"file_result_call_id":"r","start_line":40,"max_lines":20})
            ))
            .unwrap(),
            TextReadSelection::Lines {
                start_line: 40,
                max_lines: Some(20)
            }
        );
        assert_eq!(
            selection(&call(
                serde_json::json!({"file_result_call_id":"r","start_byte":65536})
            ))
            .unwrap(),
            TextReadSelection::Bytes { start_byte: 65_536 }
        );
        assert_eq!(
            selection(&call(
                serde_json::json!({"file_result_call_id":"r","query":"TODO","case_sensitive":false})
            ))
            .unwrap(),
            TextReadSelection::Search {
                query: "TODO".into(),
                case_sensitive: false,
                context_lines: 2,
                max_matches: 50
            }
        );
        for conflicting in [
            serde_json::json!({"file_result_call_id":"r","start_line":3,"query":"x"}),
            serde_json::json!({"file_result_call_id":"r","start_byte":3,"max_lines":2}),
            serde_json::json!({"file_result_call_id":"r","context_lines":5}),
            serde_json::json!({"file_result_call_id":"r","query":"x","context_lines":11}),
            serde_json::json!({"file_result_call_id":"r","start_byte":MAX_TEXT_FILE_BYTES + 1}),
        ] {
            assert!(
                selection(&call(conflicting.clone())).is_err(),
                "{conflicting}"
            );
        }
    }

    #[test]
    fn window_is_not_part_of_read_binding() {
        let digest = |args: serde_json::Value| {
            crate::permission_tools::canonical_tool_permission_input_json(
                crate::tool_names::READ_TEXT_FILE,
                args,
            )
            .unwrap()
        };
        let whole = digest(serde_json::json!({"file_result_call_id":"r"}));
        assert_eq!(
            digest(serde_json::json!({"file_result_call_id":"r","start_line":9,"max_lines":5})),
            whole
        );
        assert_eq!(
            digest(serde_json::json!({"file_result_call_id":"r","query":"x"})),
            whole
        );
        assert_ne!(
            digest(serde_json::json!({"file_result_call_id":"other"})),
            whole
        );
    }

    fn output(content: &str, byte_len: u64, sha256: String) -> FileContentReadOutput {
        FileContentReadOutput {
            file: ObjectRef {
                token: "file".into(),
                snapshot_id: "snapshot".into(),
                object_kind: ObjectKind::File,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            display_name: "notes.txt".into(),
            content_utf8: content.into(),
            byte_len,
            sha256,
            total_lines: 1,
            start_line: u64::from(!content.is_empty()),
            end_line: u64::from(!content.is_empty()),
            truncated: false,
            next_start_byte: None,
            matches: vec![],
        }
    }

    #[test]
    fn whole_file_digest_is_checked_only_for_a_complete_read() {
        let sha = |text: &str| format!("{:x}", Sha256::digest(text.as_bytes()));
        let complete = output("abc", 3, sha("abc"));
        assert!(validate_output(&TextReadSelection::Whole, &complete).is_ok());
        let forged = output("abc", 3, "a".repeat(64));
        assert!(validate_version(&forged).is_err());
        // A window of a 16 MiB file carries the device digest as a version.
        let mut window = output("abc", MAX_TEXT_FILE_BYTES, "a".repeat(64));
        window.truncated = true;
        assert!(validate_output(&TextReadSelection::Whole, &window).is_ok());
        // A small file must be returned whole when no window was asked for.
        let mut partial = output("ab", 3, "a".repeat(64));
        partial.truncated = true;
        assert!(validate_output(&TextReadSelection::Whole, &partial).is_err());
        assert!(validate_output(&TextReadSelection::Bytes { start_byte: 1 }, &partial).is_ok());
        let oversized = output(&"x".repeat(65_537), MAX_TEXT_FILE_BYTES, "a".repeat(64));
        assert!(validate_version(&oversized).is_err());
        let too_large = output("abc", MAX_TEXT_FILE_BYTES + 1, "a".repeat(64));
        assert!(validate_version(&too_large).is_err());
    }

    #[test]
    fn search_output_respects_requested_match_count_and_budget() {
        let search = TextReadSelection::Search {
            query: "x".into(),
            case_sensitive: true,
            context_lines: 0,
            max_matches: 1,
        };
        let mut found = output("", 1_000, "a".repeat(64));
        found.total_lines = 10;
        found.matches = vec![TextReadMatch {
            line: 2,
            byte_offset: 5,
            text: "x".into(),
        }];
        assert!(validate_output(&search, &found).is_ok());
        found.matches.push(TextReadMatch {
            line: 3,
            byte_offset: 9,
            text: "x".into(),
        });
        assert!(validate_output(&search, &found).is_err());
        found.matches.truncate(1);
        found.matches[0].text = "x".repeat(65_537);
        assert!(validate_output(&search, &found).is_err());
        let mut lines = output("abc", 1_000, "a".repeat(64));
        lines.total_lines = 10;
        lines.matches = vec![TextReadMatch {
            line: 1,
            byte_offset: 0,
            text: "a".into(),
        }];
        assert!(validate_output(&TextReadSelection::Whole, &lines).is_err());
    }
}
