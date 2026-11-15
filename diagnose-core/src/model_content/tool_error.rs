//! A failed tool call's model-visible error, classified by where its text
//! comes from.
//!
//! The description always repeats the failing call's tool name, so it is
//! derived from that call's arguments. Validation text of the server adds
//! nothing else and registers under the issuing output. Text a device or an
//! external read returned keeps that result's label. Raw host or upstream
//! text never registers.

use super::{ModelArg, ModelContent, Sourced, Unverified};
use crate::chat::ToolCall;
use desk_agent_protocol::{AgentError, data_lineage::DataEnvelope};

/// What the model is told when a host error may not be shown.
const UNAVAILABLE_REASON: &str = "the tool could not complete";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Server,
    Sourced(Box<DataEnvelope>),
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    reason: String,
    origin: Origin,
}

impl ToolError {
    /// A fixed server reason.
    pub fn server(reason: &'static str) -> Self {
        Self {
            reason: reason.into(),
            origin: Origin::Server,
        }
    }

    /// The error of server validation that read only the failing call's
    /// arguments and server constants, such as argument parsing, or of a host
    /// whose model-safe errors are fixed server text. An error that is not
    /// model-safe becomes the fixed unavailable reason.
    pub fn validation(error: &AgentError) -> Self {
        if !error.safe_for_model {
            return Self::server(UNAVAILABLE_REASON);
        }
        Self {
            reason: error.message.clone(),
            origin: Origin::Server,
        }
    }

    /// An error a host returned whose text may come from a device, an
    /// upstream service or infrastructure. A model-safe one is shown as
    /// unverified text; any other becomes the fixed unavailable reason.
    pub fn host(error: &AgentError) -> Self {
        if !error.safe_for_model {
            return Self::server(UNAVAILABLE_REASON);
        }
        Self {
            reason: error.message.clone(),
            origin: Origin::Unverified,
        }
    }

    /// An error a device or external read returned under `label`.
    pub fn sourced(error: &AgentError, label: DataEnvelope) -> Self {
        if !error.safe_for_model {
            return Self::server(UNAVAILABLE_REASON);
        }
        Self {
            reason: error.message.clone(),
            origin: Origin::Sourced(Box::new(label)),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The model-visible description for `call`, issued by `issuing`, with
    /// the tool's schema guidance when the reason is about its input.
    pub fn describe(&self, call: &ToolCall, issuing: Option<&DataEnvelope>) -> ModelContent {
        let text = crate::model_input::describe_error(&call.name, &self.reason);
        let builder = ModelContent::builder().arg(&ModelArg::of_call(call, issuing, text));
        match &self.origin {
            Origin::Server => builder.build(),
            Origin::Sourced(label) => builder
                .sourced(&Sourced::new(String::new(), (**label).clone()))
                .build(),
            Origin::Unverified => builder.unverified(&Unverified::new("")).build(),
        }
    }

    /// The persisted result message of `call`, labelled from the error.
    pub fn result_message(
        &self,
        message_id: impl Into<String>,
        call: &ToolCall,
        issuing: Option<&DataEnvelope>,
    ) -> Result<crate::chat::ChatMessage, AgentError> {
        crate::chat::ChatMessage::tool_result_from(
            message_id,
            &call.id,
            &self.describe(call, issuing),
            issuing,
            crate::model_message_labels::ServerResultSource::ToolCallError,
        )
    }
}
