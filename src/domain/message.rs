//! Messages, tool calls/definitions, stream events, and the precision `Lens`
//! — the vocabulary every layer speaks. Pure data, no IO.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Role of a chat message passed to model engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
}

/// Normalized chat message exchanged with engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Message role.
    pub role: Role,
    /// Message text content.
    pub content: String,
    /// Optional tool-call ID when responding to a tool invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional model-generated tool calls attached to the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Tool call emitted by a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique tool call identifier.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON arguments for the tool.
    pub arguments: serde_json::Value,
}

/// Tool definition exposed to model providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema of accepted parameters.
    pub parameters: serde_json::Value,
}

/// Provider/model metadata published to runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Provider-native model ID.
    pub id: String,
    /// Provider name.
    pub provider: String,
    /// Display name suitable for CLI/status output.
    pub display_name: String,
    /// Model context window in tokens.
    pub context_window: usize,
    /// Whether tool calls are supported.
    pub supports_tools: bool,
    /// Whether streaming responses are supported.
    pub supports_streaming: bool,
}

/// Channel-recipient identity used by pipes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    /// Pipe identifier (for example: `cli`, `telegram`).
    pub pipe_id: String,
    /// Peer/user/channel identifier.
    pub peer_id: String,
    /// Optional account/server identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Optional thread/group identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Inbound message envelope emitted by pipes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Sender identity.
    pub sender: Recipient,
    /// Text payload.
    pub content: String,
    /// Inbound timestamp.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional attached media payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<MediaPayload>>,
}

/// Raw media payload attached to an inbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPayload {
    /// MIME type of the payload.
    pub mime_type: String,
    /// Raw media bytes.
    pub data: Vec<u8>,
    /// Optional original filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Delivery options for outbound messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeliveryOptions {
    /// Optional message ID to reply to.
    pub reply_to_message_id: Option<String>,
    /// Optional parse/render mode defined by the target pipe.
    pub parse_mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

/// Incremental events produced during model generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Text delta chunk from the model.
    TextDelta { text: String },

    /// Start of a tool call emitted by the model.
    ToolCallStart { id: String, name: String },

    /// Incremental tool-call argument payload.
    ToolCallDelta { id: String, arguments_delta: String },

    /// End of the current tool call.
    ToolCallEnd { id: String },

    /// Optional thinking/reasoning text chunk.
    ThinkingDelta { text: String },

    /// Usage accounting snapshot for the current turn.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },

    /// Terminal event indicating successful completion.
    Done,

    /// Terminal event indicating generation failure.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Lens — user-controlled precision mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    Eco,
    Standard,
    Precise,
}

impl Lens {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lens::Eco => "eco",
            Lens::Standard => "standard",
            Lens::Precise => "precise",
        }
    }
}

impl FromStr for Lens {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "standard" => Lens::Standard,
            "precise" => Lens::Precise,
            "eco" => Lens::Eco,
            _ => return Err(()),
        })
    }
}

// ---------------------------------------------------------------------------
// Tool definition helpers
// ---------------------------------------------------------------------------

impl ToolDef {
    pub(crate) fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}
