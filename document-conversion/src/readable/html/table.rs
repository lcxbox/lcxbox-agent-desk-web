//! Markdown table rendering without column padding.
//!
//! htmd pads every cell to its column's widest cell, so one wide cell times
//! many rows multiplies the output. This handler emits the same pipe-table
//! shape with unpadded cells; tables without header cells, and tables inside
//! cells, fall back to htmd's handling (block content).

use htmd::Element;
use htmd::element_handler::{HandlerResult, Handlers};
use markup5ever_rcdom::{Handle, NodeData};

use super::parent_of;

pub(super) fn table_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    if !has_header_cells(element.node) || is_inside_cell(element.node) {
        return handlers.fallback(element);
    }
    let mut captions = Vec::new();
    let mut rows = Vec::new();
    for child in element.node.children.borrow().iter() {
        match element_name(child) {
            Some("caption") => {
                if let Some(caption) = handlers.handle(child) {
                    captions.push(normalize_cell(&caption.content));
                }
            }
            Some("thead" | "tbody" | "tfoot") => {
                for row in child.children.borrow().iter() {
                    if element_name(row) == Some("tr") {
                        rows.push(row_cells(handlers, row));
                    }
                }
            }
            Some("tr") => rows.push(row_cells(handlers, child)),
            _ => {}
        }
    }
    rows.retain(|row| !row.is_empty());
    if rows.is_empty() {
        return handlers.fallback(element);
    }

    let mut markdown = String::from("\n\n");
    for caption in captions.iter().filter(|caption| !caption.is_empty()) {
        markdown.push_str(caption);
        markdown.push('\n');
    }
    for (index, row) in rows.iter().enumerate() {
        markdown.push('|');
        for cell in row {
            markdown.push(' ');
            markdown.push_str(cell);
            markdown.push_str(" |");
        }
        markdown.push('\n');
        if index == 0 {
            markdown.push('|');
            markdown.push_str(&" --- |".repeat(row.len()));
            markdown.push('\n');
        }
    }
    markdown.push('\n');
    Some(markdown.into())
}

fn row_cells(handlers: &dyn Handlers, row: &Handle) -> Vec<String> {
    row.children
        .borrow()
        .iter()
        .filter(|cell| matches!(element_name(cell), Some("td" | "th")))
        .map(|cell| {
            handlers
                .handle(cell)
                .map(|result| normalize_cell(&result.content))
                .unwrap_or_default()
        })
        .collect()
}

fn normalize_cell(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "&#124;")
}

fn element_name(node: &Handle) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&name.local),
        _ => None,
    }
}

/// Whether the table has `<th>` or `<thead>` outside nested tables.
fn has_header_cells(table: &Handle) -> bool {
    let mut stack: Vec<Handle> = table.children.borrow().iter().cloned().collect();
    while let Some(node) = stack.pop() {
        match element_name(&node) {
            Some("th" | "thead") => return true,
            Some("table") => {}
            _ => stack.extend(node.children.borrow().iter().cloned()),
        }
    }
    false
}

fn is_inside_cell(node: &Handle) -> bool {
    let mut current = parent_of(node);
    while let Some(parent) = current {
        if matches!(element_name(&parent), Some("td" | "th")) {
            return true;
        }
        current = parent_of(&parent);
    }
    false
}
