//! Exact UTF-8 text edits applied in memory, all or nothing.
//!
//! Each edit matches in the result of the edits before it. A single-match
//! edit must match exactly once, counting overlapping occurrences; a
//! `replace_all` edit replaces every non-overlapping occurrence, like
//! `str::replace`, and needs at least one. The result size is computed before
//! anything is allocated, so a small input cannot grow the file past the
//! limit. In a file whose every line break is CRLF, bare line feeds in the
//! edits are written as CRLF; otherwise matching is byte exact.
use desk_agent_protocol::AgentError;
use desk_agent_protocol::AgentErrorKind;
use desk_agent_protocol::computer_use::{MAX_TEXT_FILE_BYTES, TextEdit, TextFileChange};
use memchr::memmem;

use super::error;

/// The file content after `change`.
pub(super) fn apply(original: &str, change: &TextFileChange) -> Result<String, AgentError> {
    let edits = match change {
        TextFileChange::Write { content_utf8 } => {
            return within_limit(content_utf8.len() as u64).map(|()| content_utf8.clone());
        }
        TextFileChange::Edit { edits } => edits,
    };
    let crlf = uses_crlf(original);
    let mut text = original.to_owned();
    for (index, edit) in edits.iter().enumerate() {
        let (old, new) = if crlf {
            (to_crlf(&edit.old_text), to_crlf(&edit.new_text))
        } else {
            (edit.old_text.clone(), edit.new_text.clone())
        };
        text = apply_one(&text, &old, &new, edit, index, edits.len())?;
    }
    Ok(text)
}

fn apply_one(
    text: &str,
    old: &str,
    new: &str,
    edit: &TextEdit,
    index: usize,
    total: usize,
) -> Result<String, AgentError> {
    let label = format!("edit {} of {total}", index + 1);
    if old.is_empty() {
        return Err(invalid(format!("{label}: old_text must not be empty")));
    }
    let finder = memmem::Finder::new(old.as_bytes());
    let count = if edit.replace_all {
        finder.find_iter(text.as_bytes()).count()
    } else {
        count_overlapping(&finder, text.as_bytes())
    };
    match count {
        0 => {
            let hint = trimmed_match_line(text, old)
                .map(|line| {
                    format!(
                        " It matches line {line} only when leading and trailing whitespace is ignored; copy that text exactly."
                    )
                })
                .unwrap_or_default();
            return Err(invalid(format!(
                "{label}: old_text matched 0 locations in the current file; no edit was applied.{hint} Read the file again and request approval for the revised edits."
            )));
        }
        1 => {}
        count if !edit.replace_all => {
            return Err(invalid(format!(
                "{label}: old_text matched {count} locations; include more surrounding text so it matches exactly once, or set replace_all. No edit was applied."
            )));
        }
        _ => {}
    }
    let grown = (new.len() as i128 - old.len() as i128) * count as i128;
    let length = text.len() as i128 + grown;
    within_limit(u64::try_from(length).unwrap_or(u64::MAX))?;
    let mut result = String::with_capacity(length as usize);
    let mut cursor = 0;
    // A single-match edit has exactly one non-overlapping match here too.
    for position in finder.find_iter(text.as_bytes()) {
        result.push_str(&text[cursor..position]);
        result.push_str(new);
        cursor = position + old.len();
    }
    result.push_str(&text[cursor..]);
    Ok(result)
}

/// Occurrences counted from one byte past each match, so overlapping ones
/// count too.
fn count_overlapping(finder: &memmem::Finder<'_>, haystack: &[u8]) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(found) = finder.find(&haystack[start..]) {
        count += 1;
        start += found + 1;
        if start >= haystack.len() {
            break;
        }
    }
    count
}

fn within_limit(length: u64) -> Result<(), AgentError> {
    if length > MAX_TEXT_FILE_BYTES {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            format!(
                "the edited file would be {length} bytes, over the 16 MiB limit; no edit was applied"
            ),
            false,
        ));
    }
    Ok(())
}

/// At least one line break, and every one of them CRLF.
fn uses_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut any = false;
    for position in memchr::memchr_iter(b'\n', bytes) {
        if position == 0 || bytes[position - 1] != b'\r' {
            return false;
        }
        any = true;
    }
    any
}

/// Bare line feeds as CRLF; existing CRLF pairs stay.
fn to_crlf(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            output.push('\r');
        }
        output.push(character);
        previous = character;
    }
    output
}

/// The 1-based line where `needle` matches once with each line's leading and
/// trailing whitespace ignored.
fn trimmed_match_line(text: &str, needle: &str) -> Option<usize> {
    let wanted: Vec<&str> = needle.lines().map(str::trim).collect();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if wanted.is_empty() || wanted.len() > lines.len() {
        return None;
    }
    let mut found = None;
    for start in 0..=lines.len() - wanted.len() {
        let window = &lines[start..start + wanted.len()];
        let matches = window
            .iter()
            .zip(&wanted)
            .enumerate()
            .all(|(offset, (line, want))| {
                // The first and last needle lines may be partial lines.
                if wanted.len() == 1 {
                    line.contains(want)
                } else if offset == 0 {
                    line.ends_with(want)
                } else if offset == wanted.len() - 1 {
                    line.starts_with(want)
                } else {
                    line == want
                }
            });
        if matches {
            if found.is_some() {
                return None;
            }
            found = Some(start + 1);
        }
    }
    found
}

fn invalid(message: String) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_text: &str, new_text: &str) -> TextEdit {
        TextEdit {
            old_text: old_text.into(),
            new_text: new_text.into(),
            replace_all: false,
        }
    }

    fn edits(list: Vec<TextEdit>) -> TextFileChange {
        TextFileChange::Edit { edits: list }
    }

    #[test]
    fn edits_apply_in_order_or_not_at_all() {
        let change = edits(vec![edit("alpha", "beta"), edit("beta gamma", "delta")]);
        assert_eq!(apply("alpha gamma", &change).unwrap(), "delta");
        // The third edit fails, so the earlier two are not written either.
        let failing = edits(vec![
            edit("one", "1"),
            edit("two", "2"),
            edit("missing", "3"),
        ]);
        let error = apply("one two", &failing).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
        assert!(
            error.message.starts_with("edit 3 of 3"),
            "{}",
            error.message
        );
        assert!(error.message.contains("matched 0 locations"));
    }

    #[test]
    fn single_edits_count_overlapping_matches_and_replace_all_does_not_overlap() {
        let error = apply("aaa", &edits(vec![edit("aa", "b")])).unwrap_err();
        assert!(
            error.message.contains("matched 2 locations"),
            "{}",
            error.message
        );
        let all = TextEdit {
            replace_all: true,
            ..edit("aa", "b")
        };
        assert_eq!(
            apply("aaaaa", &edits(vec![all])).unwrap(),
            "aaaaa".replace("aa", "b")
        );
        let none = TextEdit {
            replace_all: true,
            ..edit("zz", "b")
        };
        assert!(apply("aaaaa", &edits(vec![none])).is_err());
    }

    #[test]
    fn inflating_edits_are_rejected_before_allocation() {
        let source = "x".repeat(1024 * 1024);
        let inflate = TextEdit {
            replace_all: true,
            ..edit("x", &"y".repeat(64))
        };
        let error = apply(&source, &edits(vec![inflate])).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::OutputLimitExceeded);
        let oversized = TextFileChange::Write {
            content_utf8: "z".repeat(MAX_TEXT_FILE_BYTES as usize + 1),
        };
        assert!(apply("", &oversized).is_err());
    }

    #[test]
    fn crlf_files_take_multiline_edits_written_with_line_feeds() {
        let original = "first\r\nsecond\r\nthird\r\n";
        let change = edits(vec![edit("first\nsecond", "first\ninserted\nsecond")]);
        assert_eq!(
            apply(original, &change).unwrap(),
            "first\r\ninserted\r\nsecond\r\nthird\r\n"
        );
        // A mixed file is matched byte for byte.
        let mixed = "first\r\nsecond\nthird";
        assert!(apply(mixed, &edits(vec![edit("first\nsecond", "x")])).is_err());
        assert_eq!(
            apply(mixed, &edits(vec![edit("second\nthird", "x")])).unwrap(),
            "first\r\nx"
        );
        // Edits without line breaks are unaffected.
        assert_eq!(
            apply(original, &edits(vec![edit("third", "3")])).unwrap(),
            "first\r\nsecond\r\n3\r\n"
        );
    }

    #[test]
    fn a_whitespace_only_mismatch_names_the_line_but_applies_nothing() {
        let original = "fn main() {\n    call();\n}\n";
        let error = apply(original, &edits(vec![edit("        call();", "done();")])).unwrap_err();
        assert!(error.message.contains("line 2"), "{}", error.message);
        assert!(error.message.contains("matched 0 locations"));
    }

    #[test]
    fn writes_replace_the_whole_file() {
        let change = TextFileChange::Write {
            content_utf8: "new".into(),
        };
        assert_eq!(apply("old", &change).unwrap(), "new");
    }
}
