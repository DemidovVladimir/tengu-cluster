//! Shared no-op implementations of small ports / executors.
//!
//! These are used by surfaces that drive `collect_engine_response` outside
//! the chat / telegram loop and don't need activity logging or fallback
//! tool execution. Today: `inbound::eval` (eval runs) and `inbound/webhooks.rs`
//! (webhook one-shots). Previously each surface re-defined identical
//! `NoopActivity` and `NoopRuntimeToolExecutor` types — moved here so the
//! duplication doesn't drift if the trait signatures change. `NoopTrace` is the
//! trace sink that records nothing (`ports/trace.rs`).

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::domain::message::{Message, ToolCall};
use crate::ports::engine::ToolExecutor;
use crate::ports::tool_activity::ToolActivityPort;

/// No-op `ToolActivityPort`. Drops every event. Used by per-turn surfaces
/// (eval, webhook) that don't surface team-activity feeds.
pub(crate) struct NoopActivity;

impl ToolActivityPort for NoopActivity {
    fn publish_tool_activity(&self, _call: &ToolCall) {}
}

/// Fallback `ToolExecutor` used when `build_tool_executor` returns `None`
/// — typically because the agent has no workspace and an empty tool list
/// (e.g. the orchestrator agent on the eval / webhook paths). Any call
/// that reaches this executor is a bug at the caller's level, so we error
/// rather than silently swallowing it.
pub(crate) struct NoopRuntimeToolExecutor;

#[async_trait]
impl ToolExecutor for NoopRuntimeToolExecutor {
    async fn execute(&self, _call: &ToolCall, _messages: &[Message]) -> Result<String> {
        bail!("no-op executor: tool calls are not enabled in this run")
    }
}

/// No-op `TraceSink` (`ports/trace.rs`): records nothing, has no run id.
/// What a process gets when its trace dir cannot be written
/// (`bootstrap::trace::open_sink`), and the default of a component that was
/// handed no sink.
pub(crate) struct NoopTrace;

impl crate::ports::trace::TraceSink for NoopTrace {
    fn emit(&self, _draft: crate::domain::trace::EventDraft) -> Option<String> {
        None
    }

    fn run_id(&self) -> Option<&str> {
        None
    }
}
