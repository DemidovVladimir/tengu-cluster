//! Core types shared by all Tengu components.
//!
//! Consolidates all shared data types, traits, and enums: messages, engine
//! contracts, capability/tool types, memory types, and chat session state.

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
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
// Engine — the AI backend powering an agent
// ---------------------------------------------------------------------------

/// Runtime-discoverable engine capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub context_window: usize,
    pub max_output_tokens_per_turn: u32,
    pub supports_tool_use: bool,
    pub supports_streaming: bool,
    pub manages_own_workspace: bool,
}

/// Runtime diagnostics metadata surfaced by engines for status/doctor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDiagnostics {
    pub engine_id: String,
    pub configured_model: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub capabilities: EngineCapabilities,
}

pub struct EngineContext {
    pub workspace: Option<std::path::PathBuf>,
    pub system_prompt: Option<String>,
    /// Tools to expose via MCP bridge (used by Claude Code engine).
    pub bridge_tools: Option<Vec<ToolDef>>,
    /// Maximum tool call rounds before killing the session.
    /// Enforced inside the Claude Code NDJSON reader (the outer
    /// `collect_engine_response` loop already caps OpenRouter rounds).
    pub max_tool_rounds: Option<u32>,
    /// Maximum chars per MCP bridge tool result. Passed to the bridge
    /// subprocess via `TENGU_BRIDGE_MAX_RESULT_CHARS`.
    pub max_mcp_result_chars: Option<u32>,
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn max_output_tokens_per_turn(&self) -> u32 {
        ((self.context_window() / 8).clamp(256, 16_384)) as u32
    }
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    fn supports_streaming(&self) -> bool {
        false
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            context_window: self.context_window(),
            max_output_tokens_per_turn: self.max_output_tokens_per_turn(),
            supports_tool_use: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
            manages_own_workspace: self.manages_own_workspace(),
        }
    }
    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: self.available_models().first().map(|m| m.id.clone()),
            endpoint: None,
            transport: None,
            capabilities: self.capabilities(),
        }
    }
    fn available_models(&self) -> Vec<ModelInfo>;

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
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
    pub(crate) fn new(
        name: &str,
        description: &str,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub agent_id: String,
    pub created_at_epoch_s: u64,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct MemorySearchResult {
    pub entry: MemoryEntry,
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Chat / flow session types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptAssemblyReport {
    pub system_tokens: usize,
    pub history_tokens: usize,
    pub dropped_history_messages: usize,
    pub reserved_output_tokens: usize,
    pub output_token_cap: usize,
    pub total_input_budget: usize,
    pub flow_budget_remaining: usize,
    pub compaction_applied: bool,
    pub compacted_messages: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryAssembly {
    pub messages: Vec<Message>,
    #[allow(dead_code)] // read in tests only
    pub used_tokens: usize,
    #[allow(dead_code)] // read in tests only
    pub dropped_messages: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlowCompactionPolicy {
    pub threshold_tokens: u64,
    pub keep_turns: usize,
    pub summary_max_tokens: u32,
}

/// Mutable per-session runtime state for chat loop execution.
#[derive(Debug, Clone)]
pub(crate) struct ChatLoopState {
    pub messages: Vec<Message>,
    pub active_flow_key: Option<String>,
    pub manual_session_id: Option<String>,
    pub flow_token_usage: u64,
    pub active_lens: Lens,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub last_prompt_report: Option<PromptAssemblyReport>,
}

impl ChatLoopState {
    pub(crate) fn reset_for_new_session(&mut self) {
        self.manual_session_id = Some(uuid::Uuid::new_v4().to_string());
        self.active_flow_key = None;
        self.messages.clear();
        self.flow_token_usage = 0;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
    }
}

