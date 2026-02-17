use serde::{Deserialize, Serialize};

/// Events emitted by an engine during response generation.
///
/// TODO(epic-stream-contract): Define strict ordering/terminal-event guarantees and
/// add conformance tests across backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Incremental text content.
    TextDelta { text: String },

    /// The model is requesting a tool call.
    ToolCallStart {
        id: String,
        name: String,
    },

    /// Incremental arguments for the current tool call.
    ToolCallDelta {
        id: String,
        arguments_delta: String,
    },

    /// Tool call arguments are complete.
    ToolCallEnd { id: String },

    /// Thinking/reasoning output (if supported).
    ThinkingDelta { text: String },

    /// Token usage stats for this turn.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },

    /// The response stream is complete.
    Done,

    /// An error occurred during generation.
    Error { message: String },
}
