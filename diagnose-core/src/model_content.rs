//! Model-visible content built only from pieces whose source is known.
//!
//! Every byte a model sees is a server constant, an argument of the model
//! call being answered, text carrying the label it was read under, or text
//! explicitly marked unverified. The label of a message is computed from the
//! pieces (see [`crate::model_message_labels::composed_envelope`]); callers
//! never choose whether a result registers.
//!
//! The piece types bound what can be written, not whether the right piece was
//! chosen: wrapping device text in [`ModelArg`] or a wrong [`Sourced`] label
//! is a programming error that review and the guard tests catch.

use crate::chat::ToolCall;
use desk_agent_protocol::{AgentError, AgentErrorKind, data_lineage::DataEnvelope};
use serde::Serialize;
use std::{collections::BTreeMap, fmt::Display};

pub mod stored;
pub use stored::{Stored, StoredOrigin, StoredRoot, StoredTextSource};
pub mod tool_error;
pub use tool_error::ToolError;

#[doc(hidden)]
pub mod __private {
    /// Implemented only by the piece impls below and the declaration macros.
    pub trait Sealed {}
}

/// What one piece adds to the label of the content it is part of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// Calls whose arguments appear, as (call id, issuing output id).
    pub(crate) args: Vec<(String, Option<String>)>,
    /// Labels of in-request data the content repeats, by envelope id.
    pub(crate) parents: BTreeMap<String, DataEnvelope>,
    /// Roots of stored text the content repeats, by envelope id.
    pub(crate) stored: BTreeMap<String, StoredRoot>,
    /// Whether any piece is text no source vouches for.
    pub(crate) unverified: bool,
}

impl Sources {
    fn merge(&mut self, other: &Sources) {
        self.args.extend(other.args.iter().cloned());
        for (id, label) in &other.parents {
            self.parents
                .entry(id.clone())
                .or_insert_with(|| label.clone());
        }
        for (id, root) in &other.stored {
            self.stored
                .entry(id.clone())
                .or_insert_with(|| root.clone());
        }
        self.unverified |= other.unverified;
    }
}

/// A value that may appear in model-visible content. Bare `String` is not a
/// piece; wrap it in the piece type naming its source.
pub trait ContentPiece: Serialize + __private::Sealed {
    fn collect(&self, sources: &mut Sources);
}

/// A server constant: literals, numbers, flags and enums declared with
/// [`const_piece!`]. It adds nothing to the label.
pub trait ConstValue: ContentPiece {}

macro_rules! const_impls {
    ($($ty:ty),* $(,)?) => {
        $(
            impl __private::Sealed for $ty {}
            impl ContentPiece for $ty {
                fn collect(&self, _: &mut Sources) {}
            }
            impl ConstValue for $ty {}
        )*
    };
}

const_impls!(&'static str, bool, u8, u16, u32, u64, usize, i32, i64);

/// Declares a serializable type, such as a status enum, as a server constant.
#[macro_export]
macro_rules! const_piece {
    ($($ty:ty),* $(,)?) => {
        $(
            impl $crate::model_content::__private::Sealed for $ty {}
            impl $crate::model_content::ContentPiece for $ty {
                fn collect(&self, _: &mut $crate::model_content::Sources) {}
            }
            impl $crate::model_content::ConstValue for $ty {}
        )*
    };
}

/// An identifier the server generated, such as a proposal, request or task id.
/// Never wrap text a model, device or owner supplied.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ServerId(String);

impl ServerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

const_piece!(ServerId);

/// Server text assembled only from literals and constant values, such as a
/// note with version numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ConstText(String);

impl ConstText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, text: &'static str) -> Self {
        self.0.push_str(text);
        self
    }

    pub fn value<T: ConstValue + Display>(mut self, value: T) -> Self {
        use std::fmt::Write;
        let _ = write!(self.0, "{value}");
        self
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ConstText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

const_piece!(ConstText);

impl<T: ContentPiece> __private::Sealed for Option<T> {}
impl<T: ContentPiece> ContentPiece for Option<T> {
    fn collect(&self, sources: &mut Sources) {
        if let Some(value) = self {
            value.collect(sources);
        }
    }
}

impl<T: ContentPiece> __private::Sealed for Vec<T> {}
impl<T: ContentPiece> ContentPiece for Vec<T> {
    fn collect(&self, sources: &mut Sources) {
        for value in self {
            value.collect(sources);
        }
    }
}

impl<T: ContentPiece> __private::Sealed for Box<T> {}
impl<T: ContentPiece> ContentPiece for Box<T> {
    fn collect(&self, sources: &mut Sources) {
        (**self).collect(sources);
    }
}

/// A value taken from the arguments of the model call being answered. Its
/// label parent is the output that issued the call. It is not
/// deserializable: it exists only through [`ModelArg::of_call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArg<T> {
    value: T,
    call_id: String,
    issuing_output: Option<String>,
}

impl<T> ModelArg<T> {
    /// Wraps `value` read from `call`'s arguments. `issuing` is the label of
    /// the model output that made the call, absent in unlabelled sessions.
    pub fn of_call(call: &ToolCall, issuing: Option<&DataEnvelope>, value: T) -> Self {
        Self {
            value,
            call_id: call.id.clone(),
            issuing_output: issuing.map(|label| label.envelope_id.clone()),
        }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    /// Derives another value from the same argument, keeping its binding.
    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> ModelArg<U> {
        ModelArg {
            value: f(&self.value),
            call_id: self.call_id.clone(),
            issuing_output: self.issuing_output.clone(),
        }
    }
}

impl<T: Serialize> Serialize for ModelArg<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<T: Serialize> __private::Sealed for ModelArg<T> {}
impl<T: Serialize> ContentPiece for ModelArg<T> {
    fn collect(&self, sources: &mut Sources) {
        sources
            .args
            .push((self.call_id.clone(), self.issuing_output.clone()));
    }
}

/// A value read under its own label in this request, such as a device read,
/// a retrieved memory or an attachment page. The label becomes a parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    value: T,
    label: DataEnvelope,
}

impl<T> Sourced<T> {
    pub fn new(value: T, label: DataEnvelope) -> Self {
        Self { value, label }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn label(&self) -> &DataEnvelope {
        &self.label
    }
}

impl<T: Serialize> Serialize for Sourced<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<T: Serialize> __private::Sealed for Sourced<T> {}
impl<T: Serialize> ContentPiece for Sourced<T> {
    fn collect(&self, sources: &mut Sources) {
        sources
            .parents
            .entry(self.label.envelope_id.clone())
            .or_insert_with(|| self.label.clone());
    }
}

/// Text no source vouches for, such as a host's raw error. Content holding
/// it never registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Unverified(String);

impl Unverified {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl __private::Sealed for Unverified {}
impl ContentPiece for Unverified {
    fn collect(&self, sources: &mut Sources) {
        sources.unverified = true;
    }
}

/// Declares a JSON receipt whose every field is a [`ContentPiece`]. A field
/// of a bare `String` type fails to compile.
///
/// ```
/// desk_diagnose_core::model_receipt! {
///     struct Receipt { status: &'static str, count: u32 }
/// }
/// ```
///
/// ```compile_fail
/// desk_diagnose_core::model_receipt! {
///     struct Leaky { status: &'static str, text: String }
/// }
/// ```
#[macro_export]
macro_rules! model_receipt {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $($(#[$field_meta:meta])* $field_vis:vis $field:ident : $ty:ty),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(::serde::Serialize)]
        $vis struct $name {
            $($(#[$field_meta])* $field_vis $field: $ty,)*
        }

        impl $crate::model_content::__private::Sealed for $name {}
        impl $crate::model_content::ContentPiece for $name {
            fn collect(&self, sources: &mut $crate::model_content::Sources) {
                $($crate::model_content::ContentPiece::collect(&self.$field, sources);)*
                let _ = sources;
            }
        }
    };
}

/// Model-visible text with the sources of every piece it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContent {
    text: String,
    sources: Sources,
}

impl ModelContent {
    /// A fixed server text.
    pub fn constant(text: &'static str) -> Self {
        Self {
            text: text.into(),
            sources: Sources::default(),
        }
    }

    /// The JSON form of a receipt declared with [`model_receipt!`].
    pub fn json<T: ContentPiece>(value: &T) -> Result<Self, AgentError> {
        let text = serde_json::to_string(value).map_err(|_| invalid_content())?;
        let mut sources = Sources::default();
        value.collect(&mut sources);
        Ok(Self { text, sources })
    }

    pub fn builder() -> ContentBuilder {
        ContentBuilder::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn sources(&self) -> &Sources {
        &self.sources
    }

    pub fn is_unverified(&self) -> bool {
        self.sources.unverified
    }

    /// Roots of the stored text this content repeats, in id order.
    pub fn stored_roots(&self) -> Vec<StoredRoot> {
        self.sources.stored.values().cloned().collect()
    }
}

/// Concatenates pieces into one [`ModelContent`].
#[derive(Debug, Default)]
pub struct ContentBuilder {
    text: String,
    sources: Sources,
}

impl ContentBuilder {
    pub fn text(mut self, text: &'static str) -> Self {
        self.text.push_str(text);
        self
    }

    pub fn value<T: ConstValue + Display>(mut self, value: T) -> Self {
        use std::fmt::Write;
        let _ = write!(self.text, "{value}");
        self
    }

    pub fn arg<T: Serialize + Display>(mut self, arg: &ModelArg<T>) -> Self {
        use std::fmt::Write;
        let _ = write!(self.text, "{}", arg.get());
        arg.collect(&mut self.sources);
        self
    }

    pub fn sourced<T: Serialize + Display>(mut self, piece: &Sourced<T>) -> Self {
        use std::fmt::Write;
        let _ = write!(self.text, "{}", piece.get());
        piece.collect(&mut self.sources);
        self
    }

    pub fn stored<T: Serialize + Display>(mut self, piece: &Stored<T>) -> Self {
        use std::fmt::Write;
        let _ = write!(self.text, "{}", piece.get());
        piece.collect(&mut self.sources);
        self
    }

    pub fn unverified(mut self, piece: &Unverified) -> Self {
        self.text.push_str(piece.as_str());
        piece.collect(&mut self.sources);
        self
    }

    /// Appends another content with all of its sources.
    pub fn content(mut self, content: &ModelContent) -> Self {
        self.text.push_str(&content.text);
        self.sources.merge(&content.sources);
        self
    }

    /// Appends a receipt's JSON form.
    pub fn json<T: ContentPiece>(self, value: &T) -> Result<Self, AgentError> {
        let content = ModelContent::json(value)?;
        Ok(self.content(&content))
    }

    pub fn build(self) -> ModelContent {
        ModelContent {
            text: self.text,
            sources: self.sources,
        }
    }
}

/// How a composed message is labelled: the model output whose call it
/// answers (absent for projections not tied to a call), the call or message
/// id its label binds, what kind of result it is, and whether it exists only
/// in one model request.
#[derive(Debug, Clone, Copy)]
pub struct Labelling<'a> {
    pub issuing: Option<&'a DataEnvelope>,
    pub call_id: &'a str,
    pub source: crate::model_message_labels::ServerResultSource,
    pub request_only: bool,
}

impl crate::chat::ChatMessage {
    /// A message whose text and label both come from `content`.
    pub fn from_content(
        role: crate::chat::ChatRole,
        message_id: impl Into<String>,
        content: &ModelContent,
        labelling: Labelling<'_>,
    ) -> Result<Self, AgentError> {
        let envelope = crate::model_message_labels::composed_envelope(
            content,
            labelling.issuing,
            labelling.call_id,
            labelling.source,
            labelling.request_only,
        )?;
        let mut message = Self::text(message_id, role, content.text());
        message.stored_roots = content.stored_roots();
        message.data_envelope = envelope;
        Ok(message)
    }

    /// The persisted result of call `call_id`, issued by `issuing`.
    pub fn tool_result_from(
        message_id: impl Into<String>,
        call_id: &str,
        content: &ModelContent,
        issuing: Option<&DataEnvelope>,
        source: crate::model_message_labels::ServerResultSource,
    ) -> Result<Self, AgentError> {
        let envelope = crate::model_message_labels::composed_envelope(
            content, issuing, call_id, source, false,
        )?;
        let mut message = Self::tool_result(message_id, call_id, content.text());
        message.stored_roots = content.stored_roots();
        message.data_envelope = envelope;
        Ok(message)
    }
}

fn invalid_content() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "model-visible content cannot be built safely".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests;
