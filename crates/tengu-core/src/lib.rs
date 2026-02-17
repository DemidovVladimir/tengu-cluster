//! Core traits and shared types for engines, pipes, tools, and refinement.
//!
//! TODO(epic-tool-loop): Wire tool calling lifecycle end-to-end in runtime.
//! TODO(epic-flow-persistence): Add flow/session manager interfaces once persistence lands.
pub mod config;
pub mod routing;
pub mod types;

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use types::{
    DeliveryOptions, InboundMessage, MediaPayload, Message, ModelInfo,
    Recipient, StreamEvent, ToolDef,
};

// ---------------------------------------------------------------------------
// Engine — the AI backend powering an agent
// ---------------------------------------------------------------------------

/// Context provided to an engine for a single turn.
pub struct EngineContext {
    pub workspace: Option<std::path::PathBuf>,
    pub system_prompt: Option<String>,
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    fn available_models(&self) -> Vec<ModelInfo>;

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

/// Capabilities a pipe can declare.
#[derive(Debug, Clone, Default)]
pub struct PipeCapabilities {
    pub supports_media: bool,
    pub supports_streaming: bool,
    pub supports_threading: bool,
    pub supports_reactions: bool,
    pub max_text_length: Option<usize>,
}

/// Access policy for inbound messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessPolicy {
    Approval,
    Allowlist(Vec<String>),
    Open,
    Disabled,
}

/// Context provided to a pipe on connect.
pub struct PipeContext {
    pub inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
}

#[async_trait]
pub trait Pipe: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn access_policy(&self) -> AccessPolicy;
    fn capabilities(&self) -> PipeCapabilities;

    async fn connect(&self, ctx: PipeContext) -> anyhow::Result<()>;
    async fn disconnect(&self) -> anyhow::Result<()>;
    async fn send_text(
        &self,
        target: &Recipient,
        text: &str,
        opts: &DeliveryOptions,
    ) -> anyhow::Result<()>;
    async fn send_media(
        &self,
        target: &Recipient,
        media: &MediaPayload,
    ) -> anyhow::Result<()>;
    // TODO(epic-channel-ack): Add optional delivery ack/result contract.
}

// ---------------------------------------------------------------------------
// Refiner — optional prompt optimization layer
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Refiner: Send + Sync {
    /// Compress a user prompt, stripping noise while preserving intent.
    async fn compress(&self, input: &str) -> anyhow::Result<String>;

    /// Generate a vector embedding for text (for knowledge store search).
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Summarize content to a compressed representation.
    async fn summarize(&self, content: &str, max_tokens: u32) -> anyhow::Result<String>;

    /// Current memory footprint of loaded models in bytes.
    fn memory_footprint(&self) -> usize;
    // TODO(epic-refiner-observability): Add optional quality/latency stats hooks.
}

// ---------------------------------------------------------------------------
// Tool — a capability available to an agent
// ---------------------------------------------------------------------------

pub struct ToolContext {
    pub workspace: std::path::PathBuf,
    pub agent_id: String,
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
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
    /// Summaries only. Cheapest.
    Eco,
    /// Summary-first mode. Auto-expand policy is planned.
    Standard,
    /// Full content always. Maximum tokens.
    Precise,
}

impl Lens {
    pub fn from_str(s: &str) -> Self {
        match s {
            "standard" => Lens::Standard,
            "precise" => Lens::Precise,
            _ => Lens::Eco,
        }
    }
}
