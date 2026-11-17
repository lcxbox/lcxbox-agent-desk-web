//! Bounded windows of a UTF-8 text file: whole-line ranges, continuation
//! inside a long line, and literal search with context lines.
//!
//! Lines are counted by `\n`; a final line feed adds no line, and a CRLF
//! line keeps its `\r`. A window ends at a line boundary when it can; a line
//! longer than the budget is cut at a character boundary and the read reports
//! where to continue.
use desk_agent_protocol::computer_use::{TextReadMatch, TextReadSelection};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use memchr::memmem;

use super::error;

/// Most context lines a search returns around a match.
pub(super) const MAX_CONTEXT_LINES: u32 = 10;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TextWindow {
    pub content: String,
    pub start_line: u64,
    pub end_line: u64,
    pub truncated: bool,
    pub next_start_byte: Option<u64>,
    pub matches: Vec<TextReadMatch>,
}

pub(super) fn total_lines(text: &str) -> u64 {
    let breaks = memchr::memchr_iter(b'\n', text.as_bytes()).count() as u64;
    breaks + u64::from(!text.is_empty() && !text.ends_with('\n'))
}

pub(super) fn select(
    text: &str,
    selection: &TextReadSelection,
    budget: usize,
) -> Result<TextWindow, AgentError> {
    match selection {
        TextReadSelection::Whole => lines(text, 1, u64::MAX, budget),
        TextReadSelection::Lines {
            start_line,
            max_lines,
        } => {
            let max_lines = max_lines.unwrap_or(u64::MAX);
            if *start_line == 0 || max_lines == 0 {
                return Err(invalid("start_line and max_lines start at 1"));
            }
            let total = total_lines(text);
            if *start_line > total.max(1) {
                return Err(invalid(format!(
                    "start_line {start_line} is past the end of the file, which has {total} lines"
                )));
            }
            lines(text, *start_line, max_lines, budget)
        }
        TextReadSelection::Bytes { start_byte } => {
            let start = usize::try_from(*start_byte).unwrap_or(usize::MAX);
            if start >= text.len() || !text.is_char_boundary(start) {
                return Err(invalid(
                    "start_byte must be a next_start_byte returned by an earlier read of this file version",
                ));
            }
            let line = line_of(text, start);
            let mut window = from_offset(text, start, line, u64::MAX, budget);
            window.start_line = line;
            Ok(window)
        }
        TextReadSelection::Search {
            query,
            case_sensitive,
            context_lines,
            max_matches,
        } => search(
            text,
            query,
            *case_sensitive,
            (*context_lines).min(MAX_CONTEXT_LINES),
            *max_matches,
            budget,
        ),
    }
}

/// Lines from `start_line`, at most `max_lines` of them.
fn lines(
    text: &str,
    start_line: u64,
    max_lines: u64,
    budget: usize,
) -> Result<TextWindow, AgentError> {
    if text.is_empty() {
        return Ok(TextWindow::default());
    }
    let offset = if start_line == 1 {
        0
    } else {
        memchr::memchr_iter(b'\n', text.as_bytes())
            .nth(usize::try_from(start_line - 2).unwrap_or(usize::MAX))
            .map(|position| position + 1)
            .ok_or_else(|| invalid("start_line is past the end of the file"))?
    };
    Ok(from_offset(text, offset, start_line, max_lines, budget))
}

/// Whole lines from `offset`, the first of which is line `line`; a first
/// line longer than the budget is cut at a character boundary.
fn from_offset(text: &str, offset: usize, line: u64, max_lines: u64, budget: usize) -> TextWindow {
    let mut window = TextWindow {
        start_line: line,
        end_line: line,
        ..TextWindow::default()
    };
    let mut cursor = offset;
    let mut taken = 0_u64;
    while cursor < text.len() && taken < max_lines {
        let end = memchr::memchr(b'\n', &text.as_bytes()[cursor..])
            .map_or(text.len(), |position| cursor + position + 1);
        let room = budget - window.content.len();
        if end - cursor > room {
            window.truncated = true;
            if taken == 0 {
                let mut cut = cursor + room;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                window.content.push_str(&text[cursor..cut]);
                window.next_start_byte = Some(cut as u64);
                taken = 1;
            }
            break;
        }
        window.content.push_str(&text[cursor..end]);
        cursor = end;
        taken += 1;
    }
    window.end_line = line + taken.saturating_sub(1);
    if taken == 0 {
        window.start_line = 0;
        window.end_line = 0;
    }
    window
}

/// 1-based line of a byte offset.
fn line_of(text: &str, offset: usize) -> u64 {
    memchr::memchr_iter(b'\n', &text.as_bytes()[..offset]).count() as u64 + 1
}

/// Byte range of the line containing `offset`, without its line feed.
fn line_bounds(text: &str, offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let start = memchr::memrchr(b'\n', &bytes[..offset]).map_or(0, |position| position + 1);
    let end =
        memchr::memchr(b'\n', &bytes[offset..]).map_or(text.len(), |position| offset + position);
    (start, end)
}

fn search(
    text: &str,
    query: &str,
    case_sensitive: bool,
    context_lines: u32,
    max_matches: u32,
    budget: usize,
) -> Result<TextWindow, AgentError> {
    if query.is_empty() {
        return Err(invalid("query must not be empty"));
    }
    if max_matches == 0 {
        return Err(invalid("max_matches starts at 1"));
    }
    // ASCII case folding keeps byte offsets identical to the original.
    let (haystack, needle) = if case_sensitive {
        (
            std::borrow::Cow::Borrowed(text),
            std::borrow::Cow::Borrowed(query),
        )
    } else {
        (
            std::borrow::Cow::Owned(text.to_ascii_lowercase()),
            std::borrow::Cow::Owned(query.to_ascii_lowercase()),
        )
    };
    let mut window = TextWindow::default();
    let mut used = 0_usize;
    let mut line = 1_u64;
    let mut counted_to = 0_usize;
    let mut last_line = 0_u64;
    for position in memmem::find_iter(haystack.as_bytes(), needle.as_bytes()) {
        line += memchr::memchr_iter(b'\n', &text.as_bytes()[counted_to..position]).count() as u64;
        counted_to = position;
        // One entry per line.
        if line == last_line {
            continue;
        }
        if window.matches.len() as u32 == max_matches {
            window.truncated = true;
            break;
        }
        last_line = line;
        let (mut start, mut end) = line_bounds(text, position);
        for _ in 0..context_lines {
            if start == 0 {
                break;
            }
            start = line_bounds(text, start - 1).0;
        }
        for _ in 0..context_lines {
            if end >= text.len() {
                break;
            }
            end = line_bounds(text, end + 1).1;
        }
        let room = budget.saturating_sub(used);
        if room == 0 {
            window.truncated = true;
            break;
        }
        let mut snippet_end = end;
        if snippet_end - start > room {
            snippet_end = start + room;
            while !text.is_char_boundary(snippet_end) {
                snippet_end -= 1;
            }
            window.truncated = true;
        }
        let snippet = text[start..snippet_end].to_owned();
        used += snippet.len();
        window.matches.push(TextReadMatch {
            line,
            byte_offset: position as u64,
            text: snippet,
        });
    }
    Ok(window)
}

fn invalid(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, selection: TextReadSelection, budget: usize) -> TextWindow {
        select(text, &selection, budget).unwrap()
    }

    #[test]
    fn line_counts_ignore_a_final_line_feed_and_keep_carriage_returns() {
        assert_eq!(total_lines(""), 0);
        assert_eq!(total_lines("one"), 1);
        assert_eq!(total_lines("one\n"), 1);
        assert_eq!(total_lines("one\r\ntwo"), 2);
        let window = read("one\r\ntwo", TextReadSelection::Whole, 1024);
        assert_eq!(window.content, "one\r\ntwo");
        assert_eq!((window.start_line, window.end_line), (1, 2));
    }

    #[test]
    fn line_ranges_stop_at_line_boundaries() {
        let text = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
        let window = read(
            &text,
            TextReadSelection::Lines {
                start_line: 3,
                max_lines: Some(2),
            },
            1024,
        );
        assert_eq!(window.content, "line 3\nline 4\n");
        assert_eq!(
            (window.start_line, window.end_line, window.truncated),
            (3, 4, false)
        );
        // A budget that holds two lines truncates at the line boundary.
        let window = read(&text, TextReadSelection::Whole, 15);
        assert_eq!(window.content, "line 1\nline 2\n");
        assert!(window.truncated && window.next_start_byte.is_none());
        assert_eq!(window.end_line, 2);
        assert!(
            select(
                &text,
                &TextReadSelection::Lines {
                    start_line: 11,
                    max_lines: Some(1)
                },
                1024
            )
            .is_err()
        );
    }

    #[test]
    fn a_long_line_continues_by_byte_at_a_character_boundary() {
        let text = format!("short\n{}\nafter\n", "界".repeat(100));
        let window = read(
            &text,
            TextReadSelection::Lines {
                start_line: 2,
                max_lines: Some(1),
            },
            100,
        );
        assert!(window.truncated);
        let next = window.next_start_byte.unwrap();
        assert_eq!(window.content.len(), 99);
        assert!(text.is_char_boundary(next as usize));
        let rest = read(&text, TextReadSelection::Bytes { start_byte: next }, 1024);
        assert_eq!(rest.start_line, 2);
        assert_eq!(
            format!("{}{}", window.content, rest.content),
            text["short\n".len()..]
        );
        assert!(
            select(
                &text,
                &TextReadSelection::Bytes {
                    start_byte: next + 1
                },
                1024
            )
            .is_err()
        );
    }

    #[test]
    fn search_returns_lines_with_context_and_stops_at_its_limits() {
        let text = (1..=20)
            .map(|n| format!("row {n} {}\n", if n % 5 == 0 { "Needle" } else { "hay" }))
            .collect::<String>();
        let search = |case_sensitive, max_matches, budget| {
            read(
                &text,
                TextReadSelection::Search {
                    query: "needle".into(),
                    case_sensitive,
                    context_lines: 1,
                    max_matches,
                },
                budget,
            )
        };
        assert!(search(true, 50, 4096).matches.is_empty());
        let window = search(false, 50, 4096);
        assert_eq!(
            window.matches.iter().map(|m| m.line).collect::<Vec<_>>(),
            vec![5, 10, 15, 20]
        );
        assert_eq!(window.matches[0].text, "row 4 hay\nrow 5 Needle\nrow 6 hay");
        assert_eq!(
            &text[window.matches[1].byte_offset as usize..][..6],
            "Needle"
        );
        assert!(window.content.is_empty() && !window.truncated);
        let limited = search(false, 2, 4096);
        assert_eq!(limited.matches.len(), 2);
        assert!(limited.truncated);
        let cut = search(false, 50, 20);
        assert!(cut.truncated);
        assert!(cut.matches.iter().map(|m| m.text.len()).sum::<usize>() <= 20);
    }

    #[test]
    fn a_large_file_reads_its_first_window() {
        let text = "0123456789\n".repeat(16 * 1024 * 1024 / 11);
        let window = read(&text, TextReadSelection::Whole, 64 * 1024);
        assert!(window.truncated);
        assert!(window.content.len() <= 64 * 1024);
        assert!(window.content.ends_with('\n'));
        assert_eq!(total_lines(&text), (16 * 1024 * 1024 / 11) as u64);
    }
}
