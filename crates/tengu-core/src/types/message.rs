use serde::{Deserialize, Serialize};

/// Role of a message in a model conversation.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Normalized chat message passed to engines.
///
/// TODO(epic-message-schema): Expand content model beyond flat text for richer
/// multimodal/tool-safe structured payloads.
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Tool call emitted by a model.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Tool definition exposed to model backends.
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Backend/model metadata surfaced to runtime.
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub display_name: String,
    pub context_window: usize,
    pub supports_tools: bool,
    pub supports_streaming: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Addressable recipient identity for a channel/pipe.
pub struct Recipient {
    pub pipe_id: String,
    pub peer_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Inbound user message envelope produced by a `Pipe`.
pub struct InboundMessage {
    pub sender: Recipient,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<MediaPayload>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Raw media payload attached to an inbound message.
///
/// TODO(epic-media-storage): Add externalized media references for large payloads
/// to avoid keeping full blobs in memory.
pub struct MediaPayload {
    pub mime_type: String,
    pub data: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Delivery options used by channel adapters for outbound sends.
pub struct DeliveryOptions {
    pub reply_to_message_id: Option<String>,
    pub parse_mode: Option<String>,
}

impl Default for DeliveryOptions {
    fn default() -> Self {
        Self {
            reply_to_message_id: None,
            parse_mode: None,
        }
    }
}
