//! `TreeSink` wrapper that enforces the HTML parse budget around `RcDom`.
//!
//! Nodes are counted where they are created (elements, comments, processing
//! instructions) and text only when a new text node is created rather than
//! merged into the preceding one. Insertion depth is measured by walking the
//! parent chain of the insertion point. Once a limit is reached the sink
//! stops mutating the tree: every insertion, reparenting, removal and
//! attribute merge becomes a no-op, while `create_*` still returns fresh
//! nodes so the tree builder keeps working until the caller stops feeding.
//!
//! Insertions into a parent that already holds `MAX_HTML_SIBLINGS` children
//! are dropped without stopping the parse: RcDom locates a node among its
//! siblings by linear scan (foster parenting, removal), so unbounded sibling
//! lists would make those operations quadratic.

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ExpandedName, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use super::{MAX_HTML_DEPTH, MAX_HTML_NODES, MAX_HTML_SIBLINGS, parent_of};

/// Budget state shared between the sink (owned by the parser) and the caller.
#[derive(Debug, Default)]
pub(super) struct ParseBudget {
    nodes: Cell<usize>,
    exceeded: Cell<bool>,
    siblings_dropped: Cell<bool>,
}

impl ParseBudget {
    pub(super) fn exceeded(&self) -> bool {
        self.exceeded.get()
    }

    pub(super) fn siblings_dropped(&self) -> bool {
        self.siblings_dropped.get()
    }

    #[cfg(test)]
    pub(super) fn nodes(&self) -> usize {
        self.nodes.get()
    }

    /// Charges `units` against the node budget; returns false once it is exhausted.
    fn charge(&self, units: usize) -> bool {
        let nodes = self.nodes.get().saturating_add(units);
        self.nodes.set(nodes);
        if nodes > MAX_HTML_NODES {
            self.exceeded.set(true);
        }
        !self.exceeded.get()
    }
}

pub(super) struct CountingSink {
    dom: RcDom,
    budget: Rc<ParseBudget>,
}

impl CountingSink {
    pub(super) fn new(budget: Rc<ParseBudget>) -> Self {
        Self {
            dom: RcDom::default(),
            budget,
        }
    }

    /// Admits a node inserted under `parent`; trips the budget when it would
    /// sit deeper than `MAX_HTML_DEPTH` and drops it when `parent` is full.
    fn admit_under(&self, parent: &Handle) -> bool {
        if self.budget.exceeded() {
            return false;
        }
        if depth(parent) >= MAX_HTML_DEPTH {
            self.budget.exceeded.set(true);
            return false;
        }
        if parent.children.borrow().len() >= MAX_HTML_SIBLINGS {
            self.budget.siblings_dropped.set(true);
            return false;
        }
        true
    }

    /// Charges a text insertion only when it created a new text node.
    fn charge_new_text(&self, parent: &Handle, children_before: usize) {
        if parent.children.borrow().len() > children_before {
            self.budget.charge(1);
        }
    }
}

/// Number of ancestors of `node` (the document has depth 0), counted up to
/// `MAX_HTML_DEPTH`.
fn depth(node: &Handle) -> usize {
    let mut depth = 0;
    let mut current = parent_of(node);
    while let Some(parent) = current {
        depth += 1;
        if depth >= MAX_HTML_DEPTH {
            break;
        }
        current = parent_of(&parent);
    }
    depth
}

impl TreeSink for CountingSink {
    type Handle = Handle;
    type Output = RcDom;
    type ElemName<'a>
        = ExpandedName<'a>
    where
        Self: 'a;

    fn finish(self) -> RcDom {
        self.dom
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> Handle {
        self.dom.get_document()
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        self.dom.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Handle {
        self.budget.charge(1);
        self.dom.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> Handle {
        self.budget.charge(1);
        self.dom.create_comment(text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Handle {
        self.budget.charge(1);
        self.dom.create_pi(target, data)
    }

    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        if !self.admit_under(parent) {
            return;
        }
        let children_before = parent.children.borrow().len();
        let is_text = matches!(child, NodeOrText::AppendText(_));
        self.dom.append(parent, child);
        if is_text {
            self.charge_new_text(parent, children_before);
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        if parent_of(element).is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        if !self.budget.exceeded() {
            self.dom
                .append_doctype_to_document(name, public_id, system_id);
        }
    }

    fn get_template_contents(&self, target: &Handle) -> Handle {
        self.dom.get_template_contents(target)
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.dom.same_node(x, y)
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.dom.set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        let Some(parent) = parent_of(sibling) else {
            return;
        };
        if !self.admit_under(&parent) {
            return;
        }
        let children_before = parent.children.borrow().len();
        let is_text = matches!(new_node, NodeOrText::AppendText(_));
        self.dom.append_before_sibling(sibling, new_node);
        if is_text {
            self.charge_new_text(&parent, children_before);
        }
    }

    /// Merging scans the existing attributes, so the work is charged to the
    /// node budget; repeated `<html>`/`<body>` tags cannot grow it unboundedly.
    fn add_attrs_if_missing(&self, target: &Handle, attrs: Vec<Attribute>) {
        let existing = match &target.data {
            NodeData::Element { attrs, .. } => attrs.borrow().len(),
            _ => 0,
        };
        if self.budget.charge(existing + attrs.len()) {
            self.dom.add_attrs_if_missing(target, attrs);
        }
    }

    fn remove_from_parent(&self, target: &Handle) {
        if !self.budget.exceeded() {
            self.dom.remove_from_parent(target);
        }
    }

    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        if !self.budget.exceeded() {
            self.dom.reparent_children(node, new_parent);
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        self.dom.is_mathml_annotation_xml_integration_point(handle)
    }

    /// RcDom's implementation walks the whole `<select>` subtree for every
    /// option, which is quadratic; selected-content cloning is not needed for
    /// text extraction.
    fn maybe_clone_an_option_into_selectedcontent(&self, _option: &Handle) {}
}
