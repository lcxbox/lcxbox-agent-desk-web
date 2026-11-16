//! HTML → Markdown under a fixed parse budget.
//!
//! The document is parsed in UTF-8-aligned chunks through [`sink::CountingSink`],
//! cleaned in one iterative pass (metadata, links, skipped tags, sibling and
//! depth limits), and the selected content root is rendered with `htmd`.

mod clean;
mod sink;
mod table;

use std::rc::Rc;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, RcDom};

use super::{ReadableDocument, ReadableNotice};
use sink::{CountingSink, ParseBudget};

/// Nodes the parser may create (attribute merges on `<html>`/`<body>` count too).
pub(super) const MAX_HTML_NODES: usize = 100_000;
/// Maximum element nesting; deeper insertions stop the parse and deeper
/// subtrees are pruned before rendering. The renderer recurses once per
/// level (about 3.3 KiB of stack per level in debug builds).
pub(super) const MAX_HTML_DEPTH: usize = 256;
/// Children kept per element, during parsing and again after reparenting.
pub(super) const MAX_HTML_SIBLINGS: usize = 10_000;
/// `<blockquote>`/`<li>` nesting kept; deeper ones are unwrapped because
/// the renderer prefixes each line once per level.
pub(super) const MAX_QUOTE_LIST_NESTING: usize = 16;
/// Bytes fed to the parser between budget checks.
const PARSE_CHUNK_BYTES: usize = 1024;

pub(super) fn to_markdown(text: &str, base_url: &url::Url) -> ReadableDocument {
    let (dom, budget) = parse(text);
    let mut notices = Vec::new();
    if budget.exceeded() {
        notices.push(ReadableNotice::ParseBudgetExceeded);
    }
    let cleaned = clean::clean(&dom.document, base_url);
    if cleaned.depth_pruned {
        notices.push(ReadableNotice::DepthPruned);
    }
    if budget.siblings_dropped() || cleaned.siblings_truncated {
        notices.push(ReadableNotice::SiblingsTruncated);
    }
    let markdown = htmd::HtmlToMarkdown::builder()
        .add_handler(vec!["table"], table::table_handler)
        .build()
        .tree_to_markdown(&cleaned.root);
    ReadableDocument {
        markdown: collapse_blank_lines(&markdown),
        truncated: !notices.is_empty(),
        title: cleaned.title,
        published_at: cleaned.published_at,
        notices,
    }
}

/// Feeds `text` in UTF-8-aligned chunks, stopping once the budget is exceeded.
fn parse(text: &str) -> (RcDom, Rc<ParseBudget>) {
    let budget = Rc::new(ParseBudget::default());
    let mut parser = parse_document(CountingSink::new(budget.clone()), ParseOpts::default());
    let mut rest = text;
    while !rest.is_empty() && !budget.exceeded() {
        let mut end = rest.len().min(PARSE_CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        parser.process(StrTendril::from_slice(&rest[..end]));
        rest = &rest[end..];
    }
    (parser.finish(), budget)
}

fn parent_of(node: &Handle) -> Option<Handle> {
    let weak = node.parent.take();
    let parent = weak.as_ref().and_then(|weak| weak.upgrade());
    node.parent.set(weak);
    parent
}

/// Collapses runs of blank (whitespace-only) lines into one empty line.
fn collapse_blank_lines(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut previous_blank = false;
    for line in markdown.split('\n') {
        let blank = line.trim().is_empty();
        if blank && previous_blank {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        if !blank {
            output.push_str(line);
        }
        previous_blank = blank;
    }
    output.truncate(output.trim_end().len());
    output
}

#[cfg(test)]
mod tests;
