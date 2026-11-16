//! One iterative pass over the parsed tree before rendering.
//!
//! Collects the title, publication time, `<base href>` and content-root
//! candidates; drops non-content tags, comments and processing instructions;
//! replaces images with their alt text; keeps at most `MAX_HTML_SIBLINGS`
//! children per element and prunes subtrees deeper than `MAX_HTML_DEPTH`
//! (tree-builder reparenting can deepen subtrees after insertion, and the
//! renderer recurses per level). `<blockquote>` and `<li>` nested more than
//! `MAX_QUOTE_LIST_NESTING` deep are unwrapped into their parent: the
//! renderer prefixes every line once per such level, so deep nesting would
//! multiply the output by the depth. Adjacent same-name inline elements are
//! merged here in linear time with the same rule `htmd` applies, so its
//! per-removal merge never runs. Links are made absolute afterwards.

use std::cell::{Cell, RefCell};
use std::mem;
use std::rc::Rc;

use html5ever::tendril::StrTendril;
use html5ever::{LocalName, local_name, ns};
use markup5ever_rcdom::{Handle, Node, NodeData};

use super::{MAX_HTML_DEPTH, MAX_HTML_SIBLINGS, MAX_QUOTE_LIST_NESTING};
use crate::readable::{clamp_published_at, clamp_title};

const SKIPPED_TAGS: [&str; 15] = [
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed", "nav",
    "aside", "button", "select", "input", "textarea",
];

/// `<meta>` name/property/itemprop values that carry the publication time.
const PUBLISHED_TIME_KEYS: [&str; 10] = [
    "article:published_time",
    "datepublished",
    "date",
    "pubdate",
    "publishdate",
    "publish-date",
    "dc.date",
    "dc.date.issued",
    "dcterms.issued",
    "dcterms.created",
];

/// Elements whose rendering prefixes every contained line.
const QUOTE_LIST_TAGS: [&str; 2] = ["blockquote", "li"];

/// htmd's block element set (CommonMark HTML blocks); block elements are never merged.
const BLOCK_TAGS: [&str; 66] = [
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "pre",
    "script",
    "search",
    "section",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

pub(super) struct Cleaned {
    /// Content root to render: first `<main>`, else the only `<article>`, else `<body>`.
    pub(super) root: Handle,
    pub(super) title: Option<String>,
    pub(super) published_at: Option<String>,
    pub(super) depth_pruned: bool,
    pub(super) siblings_truncated: bool,
}

enum Step {
    Visit {
        node: Handle,
        depth: usize,
        /// `<blockquote>`/`<li>` ancestors including `node`.
        quote_list_level: usize,
    },
    MergeChildren(Handle),
}

#[derive(Default)]
struct Walk {
    title: Option<String>,
    published_at: Option<String>,
    base_href: Option<String>,
    main: Option<Handle>,
    articles: Vec<Handle>,
    body: Option<Handle>,
    anchors: Vec<Handle>,
    depth_pruned: bool,
    siblings_truncated: bool,
}

pub(super) fn clean(document: &Handle, base_url: &url::Url) -> Cleaned {
    let mut walk = Walk::default();
    let mut stack = vec![Step::Visit {
        node: document.clone(),
        depth: 0,
        quote_list_level: 0,
    }];
    while let Some(step) = stack.pop() {
        match step {
            Step::Visit {
                node,
                depth,
                quote_list_level,
            } => {
                walk.inspect(&node);
                walk.filter_children(&node, depth, quote_list_level);
                stack.push(Step::MergeChildren(node.clone()));
                for child in node.children.borrow().iter().rev() {
                    if matches!(child.data, NodeData::Element { .. }) {
                        stack.push(Step::Visit {
                            node: child.clone(),
                            depth: depth + 1,
                            quote_list_level: quote_list_level
                                + usize::from(is_html_element(child, &QUOTE_LIST_TAGS)),
                        });
                    }
                }
            }
            Step::MergeChildren(node) => merge_inline_runs(&node),
        }
    }

    let base = walk
        .base_href
        .as_deref()
        .and_then(|href| base_url.join(href.trim()).ok())
        .filter(is_http)
        .unwrap_or_else(|| base_url.clone());
    for anchor in &walk.anchors {
        absolutize_href(anchor, &base);
    }

    let root = match (walk.main, walk.articles.len()) {
        (Some(main), _) => main,
        (None, 1) => walk.articles.swap_remove(0),
        _ => match walk.body {
            Some(body) => {
                body.children
                    .borrow_mut()
                    .retain(|child| !is_html_element(child, &["header", "footer"]));
                body
            }
            None => document.clone(),
        },
    };
    Cleaned {
        root,
        title: walk.title,
        published_at: walk.published_at,
        depth_pruned: walk.depth_pruned,
        siblings_truncated: walk.siblings_truncated,
    }
}

impl Walk {
    /// Records metadata and content-root candidates from an element.
    fn inspect(&mut self, node: &Handle) {
        let NodeData::Element { name, attrs, .. } = &node.data else {
            return;
        };
        if name.ns != ns!(html) {
            return;
        }
        let attrs = attrs.borrow();
        let attr = |wanted: LocalName| {
            attrs
                .iter()
                .find(|attribute| attribute.name.local == wanted)
                .map(|attribute| &*attribute.value)
        };
        match name.local {
            local_name!("title") if self.title.is_none() => {
                let mut text = String::new();
                for child in node.children.borrow().iter() {
                    if let NodeData::Text { contents } = &child.data {
                        text.push_str(&contents.borrow());
                    }
                }
                self.title = clamp_title(&text);
            }
            local_name!("meta") if self.published_at.is_none() => {
                let is_published_time = [
                    attr(local_name!("property")),
                    attr(local_name!("name")),
                    attr(local_name!("itemprop")),
                ]
                .into_iter()
                .flatten()
                .any(|key| {
                    PUBLISHED_TIME_KEYS
                        .iter()
                        .any(|wanted| key.trim().eq_ignore_ascii_case(wanted))
                });
                if is_published_time {
                    self.published_at = attr(local_name!("content")).and_then(clamp_published_at);
                }
            }
            local_name!("base") if self.base_href.is_none() => {
                self.base_href = attr(local_name!("href")).map(str::to_owned);
            }
            local_name!("main") if self.main.is_none() => self.main = Some(node.clone()),
            local_name!("article") => self.articles.push(node.clone()),
            local_name!("body") if self.body.is_none() => self.body = Some(node.clone()),
            local_name!("a") if attr(local_name!("href")).is_some() => {
                self.anchors.push(node.clone());
            }
            _ => {}
        }
    }

    /// Drops non-content children, turns images into alt text, unwraps
    /// over-nested quotes and list items, enforces the sibling limit and
    /// prunes children below the depth limit.
    fn filter_children(&mut self, node: &Handle, depth: usize, quote_list_level: usize) {
        let mut children = node.children.borrow_mut();
        if children.is_empty() {
            return;
        }
        if depth >= MAX_HTML_DEPTH {
            self.depth_pruned = true;
            children.clear();
            return;
        }
        let mut pending = mem::take(&mut *children);
        pending.reverse();
        while let Some(child) = pending.pop() {
            if children.len() == MAX_HTML_SIBLINGS {
                self.siblings_truncated = true;
                break;
            }
            match &child.data {
                NodeData::Text { .. } => children.push(child),
                NodeData::Element { name, attrs, .. } => {
                    if SKIPPED_TAGS.contains(&&*name.local) {
                        continue;
                    }
                    if name.ns == ns!(html) && name.local == local_name!("img") {
                        let alt = attrs
                            .borrow()
                            .iter()
                            .find(|attribute| attribute.name.local == local_name!("alt"))
                            .map(|attribute| attribute.value.trim().to_owned())
                            .filter(|alt| !alt.is_empty());
                        if let Some(alt) = alt {
                            children.push(text_node(node, &alt));
                        }
                        continue;
                    }
                    if quote_list_level >= MAX_QUOTE_LIST_NESTING
                        && is_html_element(&child, &QUOTE_LIST_TAGS)
                    {
                        let mut grandchildren = mem::take(&mut *child.children.borrow_mut());
                        for grandchild in &grandchildren {
                            grandchild.parent.set(Some(Rc::downgrade(node)));
                        }
                        grandchildren.reverse();
                        pending.append(&mut grandchildren);
                        continue;
                    }
                    children.push(child);
                }
                NodeData::Document
                | NodeData::Doctype { .. }
                | NodeData::Comment { .. }
                | NodeData::ProcessingInstruction { .. } => {}
            }
        }
    }
}

fn text_node(parent: &Handle, text: &str) -> Handle {
    Rc::new(Node {
        parent: Cell::new(Some(Rc::downgrade(parent))),
        children: RefCell::new(Vec::new()),
        data: NodeData::Text {
            contents: RefCell::new(StrTendril::from_slice(text)),
        },
    })
}

/// Merges runs of adjacent combinable inline elements (see [`combinable`]).
fn merge_inline_runs(node: &Handle) {
    let mut children = node.children.borrow_mut();
    if children.len() < 2 {
        return;
    }
    let old = mem::take(&mut *children);
    for child in old {
        if let Some(previous) = children.last()
            && combinable(previous, &child)
        {
            let (NodeData::Text { contents: into }, NodeData::Text { contents: from }) = (
                &previous.children.borrow()[0].data,
                &child.children.borrow()[0].data,
            ) else {
                unreachable!("combinable elements hold a single text child");
            };
            into.borrow_mut().push_tendril(&from.borrow());
            continue;
        }
        children.push(child);
    }
}

/// htmd's merge rule: two non-block elements other than `<a>` with the same
/// name (`b`/`strong` and `i`/`em` count as equal), equal attributes and a
/// single text child each.
fn combinable(first: &Node, second: &Node) -> bool {
    let (
        NodeData::Element {
            name: first_name,
            attrs: first_attrs,
            template_contents: first_template,
            mathml_annotation_xml_integration_point: first_mathml,
        },
        NodeData::Element {
            name: second_name,
            attrs: second_attrs,
            template_contents: second_template,
            mathml_annotation_xml_integration_point: second_mathml,
        },
    ) = (&first.data, &second.data)
    else {
        return false;
    };
    let (first_local, second_local) = (&*first_name.local, &*second_name.local);
    let same_name = first_name == second_name
        || matches!(
            (first_local, second_local),
            ("b", "strong") | ("strong", "b") | ("i", "em") | ("em", "i")
        );
    same_name
        && first_local != "a"
        && !BLOCK_TAGS.contains(&first_local)
        && single_text_child(first)
        && single_text_child(second)
        && first_template.borrow().is_none()
        && second_template.borrow().is_none()
        && *first_attrs.borrow() == *second_attrs.borrow()
        && first_mathml == second_mathml
}

fn single_text_child(node: &Node) -> bool {
    let children = node.children.borrow();
    children.len() == 1 && matches!(children[0].data, NodeData::Text { .. })
}

fn is_html_element(node: &Node, names: &[&str]) -> bool {
    matches!(&node.data, NodeData::Element { name, .. }
        if name.ns == ns!(html) && names.contains(&&*name.local))
}

fn is_http(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Rewrites `href` to an absolute http(s) URL, or removes it so only the
/// link text is rendered.
fn absolutize_href(anchor: &Node, base: &url::Url) {
    let NodeData::Element { attrs, .. } = &anchor.data else {
        return;
    };
    let mut attrs = attrs.borrow_mut();
    let Some(index) = attrs
        .iter()
        .position(|attribute| attribute.name.local == local_name!("href"))
    else {
        return;
    };
    match base.join(attrs[index].value.trim()).ok().filter(is_http) {
        Some(url) => attrs[index].value = StrTendril::from_slice(url.as_str()),
        None => {
            attrs.remove(index);
        }
    }
}
