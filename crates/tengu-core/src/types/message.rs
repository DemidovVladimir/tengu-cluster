//! Core message and envelope types used throughout runtime.
//!
//! Potential use case:
//! Convert inbound channel messages into one normalized shape consumable by any engine backend.

use serde::{Deserialize, Serialize};

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
    /// Optional policy metadata used by runtime governance/approval checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<ToolPolicyMetadata>,
}

/// Coarse risk level assigned to a tool definition.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolRiskLevel {
    /// Read-only or low-impact operations.
    #[default]
    Low,
    /// Potentially mutating operations with bounded impact.
    Medium,
    /// High-impact operations (for example shell or external side effects).
    High,
}

/// Runtime tool-governance metadata attached to tool definitions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPolicyMetadata {
    /// Declared tool risk tier.
    pub risk_level: ToolRiskLevel,
    /// Whether this tool requires explicit approval by default.
    pub requires_approval: bool,
}

impl Default for ToolPolicyMetadata {
    fn default() -> Self {
        Self {
            risk_level: ToolRiskLevel::Low,
            requires_approval: false,
        }
    }
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
