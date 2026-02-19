//! Streaming event model emitted by `Engine::run`.
//!
//! Potential use case:
//! Handle text deltas, usage stats, and tool-call lifecycle in one unified event loop.

use serde::{Deserialize, Serialize};

/// Incremental events produced during model generation.
///
/// Runtime fixture expectation today:
/// - Success path: zero-or-more non-terminal events, optional `Usage`, then `Done`.
/// - Failure path: terminal `Error`.
/// Strict cross-provider terminal guarantees are tracked as post-MVP work (`E10-T2`).
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
    ///
    /// Contract: values are cumulative within the turn. Runtime should keep the
    /// latest snapshot and apply it once after the turn completes.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },

    /// Terminal event indicating successful completion.
    Done,

    /// Terminal event indicating generation failure.
    Error { message: String },
}
