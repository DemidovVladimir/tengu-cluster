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

use crate::token::estimate_tokens_approx_min1;
use types::{
    DeliveryOptions, InboundMessage, MediaPayload, Message, ModelInfo, Recipient, StreamEvent,
    ToolDef,
};

// ---------------------------------------------------------------------------
// Engine — the AI backend powering an agent
// ---------------------------------------------------------------------------

/// Runtime-discoverable engine capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCapabilities {
    /// Maximum supported context window in tokens.
    pub context_window: usize,
    /// Whether the backend supports runtime tool-calling.
    pub supports_tool_use: bool,
    /// Whether the backend can emit incremental streamed output.
    pub supports_streaming: bool,
    /// Whether workspace/file operations are handled natively by backend.
    pub manages_own_workspace: bool,
}

/// Runtime diagnostics metadata surfaced by engines for status/doctor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDiagnostics {
    /// Stable engine identifier (for example: `ollama`).
    pub engine_id: String,
    /// Provider-specific configured model identifier, if available.
    pub configured_model: Option<String>,
    /// Provider endpoint/base URL, if applicable (for example local HTTP host).
    pub endpoint: Option<String>,
    /// Transport flavor used by the backend (for example `http-ndjson`).
    pub transport: Option<String>,
    /// Capability snapshot reported by this backend.
    pub capabilities: EngineCapabilities,
}

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
    /// Whether this engine supports incremental text streaming.
    fn supports_streaming(&self) -> bool {
        false
    }
    /// Runtime capability contract used by status/diagnostics layers.
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            context_window: self.context_window(),
            supports_tool_use: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
            manages_own_workspace: self.manages_own_workspace(),
        }
    }
    /// Runtime diagnostics metadata used by `status` and `doctor` commands.
    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: self.available_models().first().map(|m| m.id.clone()),
            endpoint: None,
            transport: None,
            capabilities: self.capabilities(),
        }
    }
    /// List of models exposed by this engine.
    fn available_models(&self) -> Vec<ModelInfo>;

    /// Execute one model turn and return a stream of response events.
    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
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

/// Result of applying a hard prompt-budget guard to a tool output.
pub struct GuardedToolOutput {
    /// Guarded output ready for prompt insertion.
    pub output: ToolOutput,
    /// Approximate tokens before guard was applied.
    pub original_tokens: usize,
    /// Approximate tokens after guard was applied.
    pub output_tokens: usize,
    /// Whether truncation was required.
    pub truncated: bool,
}

impl ToolOutput {
    /// Guard tool output against oversized prompt contribution.
    ///
    /// This keeps tool-call payloads bounded before they are appended back into
    /// model context. If content exceeds `max_tokens`, output is truncated and a
    /// marker footer is appended with original/max token metadata.
    pub fn enforce_token_limit(&self, max_tokens: u32) -> GuardedToolOutput {
        let max_tokens = (max_tokens as usize).max(1);
        let original_tokens = estimate_tokens_approx_min1(&self.content);
        if original_tokens <= max_tokens {
            return GuardedToolOutput {
                output: ToolOutput {
                    content: self.content.clone(),
                    is_error: self.is_error,
                },
                original_tokens,
                output_tokens: original_tokens,
                truncated: false,
            };
        }

        let suffix = format!(
            "\n\n[tool-output-truncated original_tokens={} max_tokens={}]",
            original_tokens, max_tokens
        );
        let suffix_tokens = estimate_tokens_approx_min1(&suffix);
        let body_tokens_budget = max_tokens.saturating_sub(suffix_tokens).max(1);
        let mut body_chars_budget = body_tokens_budget.saturating_mul(4);

        let mut guarded_content = format!(
            "{}{}",
            truncate_chars(&self.content, body_chars_budget),
            suffix
        );
        while estimate_tokens_approx_min1(&guarded_content) > max_tokens && body_chars_budget > 4 {
            body_chars_budget = body_chars_budget.saturating_sub(4);
            guarded_content = format!(
                "{}{}",
                truncate_chars(&self.content, body_chars_budget),
                suffix
            );
        }

        GuardedToolOutput {
            output: ToolOutput {
                content: guarded_content.clone(),
                is_error: self.is_error,
            },
            original_tokens,
            output_tokens: estimate_tokens_approx_min1(&guarded_content),
            truncated: true,
        }
    }
}

/// Truncate string by character count to avoid splitting invalid UTF-8 boundaries.
fn truncate_chars(content: &str, max_chars: usize) -> String {
    content.chars().take(max_chars).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    struct DefaultCapsEngine;

    #[async_trait]
    impl Engine for DefaultCapsEngine {
        fn id(&self) -> &str {
            "default-caps"
        }

        fn context_window(&self) -> usize {
            4_096
        }

        fn supports_tool_use(&self) -> bool {
            false
        }

        fn manages_own_workspace(&self) -> bool {
            false
        }

        fn available_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn run(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _context: &EngineContext,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
            Ok(Box::pin(stream::iter(vec![StreamEvent::Done])))
        }
    }

    struct StreamingCapsEngine;

    #[async_trait]
    impl Engine for StreamingCapsEngine {
        fn id(&self) -> &str {
            "streaming-caps"
        }

        fn context_window(&self) -> usize {
            8_192
        }

        fn supports_tool_use(&self) -> bool {
            true
        }

        fn manages_own_workspace(&self) -> bool {
            true
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn available_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn run(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _context: &EngineContext,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
            Ok(Box::pin(stream::iter(vec![StreamEvent::Done])))
        }
    }

    #[test]
    fn engine_capabilities_default_streaming_is_false() {
        let engine = DefaultCapsEngine;
        let caps = engine.capabilities();

        assert_eq!(
            caps,
            EngineCapabilities {
                context_window: 4_096,
                supports_tool_use: false,
                supports_streaming: false,
                manages_own_workspace: false,
            }
        );
    }

    #[test]
    fn engine_capabilities_reflect_overrides() {
        let engine = StreamingCapsEngine;
        let caps = engine.capabilities();

        assert_eq!(
            caps,
            EngineCapabilities {
                context_window: 8_192,
                supports_tool_use: true,
                supports_streaming: true,
                manages_own_workspace: true,
            }
        );
    }

    #[test]
    fn engine_diagnostics_default_shape_is_stable() {
        let engine = DefaultCapsEngine;
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "default-caps");
        assert_eq!(diagnostics.configured_model, None);
        assert_eq!(diagnostics.endpoint, None);
        assert_eq!(diagnostics.transport, None);
        assert_eq!(diagnostics.capabilities.context_window, 4_096);
        assert!(!diagnostics.capabilities.supports_streaming);
    }

    struct CustomDiagnosticsEngine;

    #[async_trait]
    impl Engine for CustomDiagnosticsEngine {
        fn id(&self) -> &str {
            "custom-diagnostics"
        }

        fn context_window(&self) -> usize {
            16_384
        }

        fn supports_tool_use(&self) -> bool {
            true
        }

        fn manages_own_workspace(&self) -> bool {
            false
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn diagnostics(&self) -> EngineDiagnostics {
            EngineDiagnostics {
                engine_id: self.id().to_string(),
                configured_model: Some("test-model".to_string()),
                endpoint: Some("https://example.test".to_string()),
                transport: Some("http-json".to_string()),
                capabilities: self.capabilities(),
            }
        }

        fn available_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn run(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _context: &EngineContext,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
            Ok(Box::pin(stream::iter(vec![StreamEvent::Done])))
        }
    }

    #[test]
    fn engine_diagnostics_can_be_overridden_by_provider() {
        let engine = CustomDiagnosticsEngine;
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "custom-diagnostics");
        assert_eq!(diagnostics.configured_model.as_deref(), Some("test-model"));
        assert_eq!(
            diagnostics.endpoint.as_deref(),
            Some("https://example.test")
        );
        assert_eq!(diagnostics.transport.as_deref(), Some("http-json"));
        assert!(diagnostics.capabilities.supports_tool_use);
        assert!(diagnostics.capabilities.supports_streaming);
    }

    #[test]
    fn tool_output_guard_passthrough_when_within_budget() {
        let output = ToolOutput {
            content: "short output".to_string(),
            is_error: false,
        };
        let guarded = output.enforce_token_limit(128);

        assert!(!guarded.truncated);
        assert_eq!(guarded.output.content, "short output");
        assert_eq!(guarded.original_tokens, guarded.output_tokens);
    }

    #[test]
    fn tool_output_guard_truncates_and_marks_payload() {
        let output = ToolOutput {
            content: "x".repeat(4_000),
            is_error: true,
        };
        let guarded = output.enforce_token_limit(128);

        assert!(guarded.truncated);
        assert!(guarded.output.content.contains("[tool-output-truncated"));
        assert!(guarded.output_tokens <= 128);
        assert!(guarded.output.is_error);
    }

    #[test]
    fn tool_output_guard_handles_zero_budget_with_safe_floor() {
        let output = ToolOutput {
            content: "x".repeat(1_000),
            is_error: false,
        };
        let guarded = output.enforce_token_limit(0);

        assert!(guarded.truncated);
        assert!(guarded.output_tokens >= 1);
    }
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
