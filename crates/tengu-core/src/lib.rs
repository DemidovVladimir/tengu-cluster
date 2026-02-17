//! Core contracts shared by all Tengu components.
//!
//! This crate defines the runtime-neutral traits (`Engine`, `Pipe`, `Refiner`, `Tool`),
//! common configuration models, routing helpers, and shared transport types.
//!
//! Potential use case:
//! Implement a new provider/channel crate by depending only on these traits and shared types.

pub mod config;
pub mod routing;
pub mod token;
pub mod types;

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::str::FromStr;

use types::{
    DeliveryOptions, InboundMessage, MediaPayload, Message, ModelInfo, Recipient, StreamEvent,
    ToolDef,
};

// ---------------------------------------------------------------------------
// Engine — the AI backend powering an agent
// ---------------------------------------------------------------------------

pub struct EngineContext {
    /// Optional workspace path associated with the current request.
    pub workspace: Option<std::path::PathBuf>,
    /// Fully assembled system prompt for the current turn.
    pub system_prompt: Option<String>,
}

#[async_trait]
pub trait Engine: Send + Sync {
    /// Stable engine identifier (for example: `ollama`).
    fn id(&self) -> &str;
    /// Maximum supported context window in tokens.
    fn context_window(&self) -> usize;
    /// Whether this engine can issue tool calls.
    fn supports_tool_use(&self) -> bool;
    /// Whether this engine manages workspace access internally.
    fn manages_own_workspace(&self) -> bool;
    /// List of models exposed by this engine.
    fn available_models(&self) -> Vec<ModelInfo>;

    /// Execute one model turn and return a stream of response events.
    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
    // TODO(epic-backend-telemetry): Standardize backend diagnostic metadata
    // surfaced to runtime for cost/latency tracking.
}

// ---------------------------------------------------------------------------
// Pipe — a messaging platform connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PipeCapabilities {
    /// Supports binary/media outbound delivery.
    pub supports_media: bool,
    /// Supports incremental streamed response delivery.
    pub supports_streaming: bool,
    /// Supports threaded conversation targets.
    pub supports_threading: bool,
    /// Supports reaction operations in the channel.
    pub supports_reactions: bool,
    /// Maximum text payload length accepted by this pipe.
    pub max_text_length: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessPolicy {
    /// Unknown senders require explicit approval.
    Approval,
    /// Only listed identities can send requests.
    Allowlist(Vec<String>),
    /// Accept all inbound messages.
    Open,
    /// Disable inbound message handling.
    Disabled,
}

pub struct PipeContext {
    /// Runtime channel where inbound messages are published by the pipe.
    pub inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
}

#[async_trait]
pub trait Pipe: Send + Sync {
    /// Stable pipe identifier (for example: `cli`, `telegram`).
    fn id(&self) -> &str;
    /// Human-readable display name.
    fn display_name(&self) -> &str;
    /// Access policy enforced for this channel.
    fn access_policy(&self) -> AccessPolicy;
    /// Capability declaration for runtime planning.
    fn capabilities(&self) -> PipeCapabilities;

    /// Connect the channel and start publishing inbound messages.
    async fn connect(&self, ctx: PipeContext) -> anyhow::Result<()>;
    /// Gracefully disconnect the channel.
    async fn disconnect(&self) -> anyhow::Result<()>;
    /// Send text to a resolved recipient.
    async fn send_text(
        &self,
        target: &Recipient,
        text: &str,
        opts: &DeliveryOptions,
    ) -> anyhow::Result<()>;
    /// Send media payload to a resolved recipient.
    async fn send_media(&self, target: &Recipient, media: &MediaPayload) -> anyhow::Result<()>;
    // TODO(epic-channel-ack): Add optional delivery ack/result contract.
}

// ---------------------------------------------------------------------------
// Refiner — optional prompt optimization layer
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Refiner: Send + Sync {
    /// Compress user input before it enters the prompt.
    async fn compress(&self, input: &str) -> anyhow::Result<String>;

    /// Produce embedding vector for retrieval/ranking use-cases.
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Summarize content to fit an approximate max-token target.
    async fn summarize(&self, content: &str, max_tokens: u32) -> anyhow::Result<String>;

    /// Estimated memory footprint used by this refiner implementation.
    fn memory_footprint(&self) -> usize;
    // TODO(epic-refiner-observability): Add optional quality/latency stats hooks.
}

// ---------------------------------------------------------------------------
// Tool — a capability available to an agent
// ---------------------------------------------------------------------------

pub struct ToolContext {
    /// Workspace root available to the tool.
    pub workspace: std::path::PathBuf,
    /// Agent identity invoking the tool.
    pub agent_id: String,
}

pub struct ToolOutput {
    /// Tool textual output sent back to the model/runtime.
    pub content: String,
    /// Whether this output represents a tool execution error.
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable tool name exposed to models.
    fn name(&self) -> &str;
    /// Human-readable tool description.
    fn description(&self) -> &str;
    /// JSON Schema describing accepted tool parameters.
    fn parameters_schema(&self) -> serde_json::Value;
    /// Execute the tool for the provided JSON parameters and context.
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput>;
    // TODO(epic-tool-security): Add per-tool policy metadata (risk level, approval requirements).
}

// ---------------------------------------------------------------------------
// Lens — user-controlled precision mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    /// Lowest-cost context mode.
    Eco,
    /// Balanced context mode.
    Standard,
    /// Highest-fidelity context mode.
    Precise,
}

impl Lens {
    /// Return canonical string representation for config/CLI output.
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
