//! Chat subsystem: types, slash-commands, and per-turn orchestration.
//!
//! Combines session state, history management, slash-command dispatch, and the
//! central `ChatRuntimeService` that processes a single user message through
//! flow resolution → memory recall → prompt budget → engine call → compaction.
//!
//! Token budget gates:
//! - **80% warning**: returns a `system_notice` alerting the user.
//! - **100% hard limit**: blocks further requests until `/reset`.

use anyhow::Result;

use crate::adapters::config::AgentConfig;
use crate::adapters::engine_builder::{collect_engine_response, ToolExecutor, ToolResultObserver};
use crate::adapters::flow_builder::{
    enforce_history_turn_limit, maybe_compact_flow, resolve_flow_key,
};
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::prompt_budget::{assemble_recent_history, compute_base_input_budget};
use crate::adapters::token::estimate_tokens_approx_min1;
use crate::adapters::types::{
    ChatLoopState, EngineDiagnostics, FlowCompactionPolicy, Lens, Message, Recipient, Role, ToolDef,
};
use crate::adapters::{Engine, EngineContext};

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

/// Cached engine metadata for slash commands (avoids needing &dyn Engine on the UI thread).
pub(crate) struct EngineInfo {
    pub context_window: usize,
    pub diagnostics: EngineDiagnostics,
}

/// Result of processing a slash command.
pub(crate) enum CommandResult {
    /// Command was recognized and handled.
    Handled(CommandOutput),
    /// Command was not recognized.
    NotHandled,
}

/// Output from a handled command.
pub(crate) struct CommandOutput {
    /// Lines of text to display.
    pub lines: Vec<String>,
}

/// Process one slash command.
///
/// `skill_commands` is an optional list of `(command_name, skill_name)` for dynamic `/help` output.
pub(crate) fn handle_chat_command(
    command: &str,
    state: &mut ChatLoopState,
    engine_info: &EngineInfo,
    agent_config: &AgentConfig,
    history_turn_limit: usize,
    compaction_policy: FlowCompactionPolicy,
    skill_commands: &[(String, String)],
) -> CommandResult {
    match command {
        "/eco" => {
            state.active_lens = Lens::Eco;
            CommandResult::Handled(CommandOutput {
                lines: vec!["Switched to eco lens (summaries only)".into()],
            })
        }
        "/standard" => {
            state.active_lens = Lens::Standard;
            CommandResult::Handled(CommandOutput {
                lines: vec!["Switched to standard lens (auto-expand)".into()],
            })
        }
        "/precise" => {
            state.active_lens = Lens::Precise;
            CommandResult::Handled(CommandOutput {
                lines: vec!["Switched to precise lens (full content)".into()],
            })
        }
        "/cost" => {
            let lines = vec![
                "Session Stats".into(),
                "─────────────────────────────".into(),
                format!(" Input tokens:  {}", state.total_input_tokens),
                format!(" Output tokens: {}", state.total_output_tokens),
                format!(
                    " Total:         {}",
                    state.total_input_tokens + state.total_output_tokens
                ),
            ];
            CommandResult::Handled(CommandOutput { lines })
        }
        "/context" => {
            let used: usize = state
                .messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content))
                .sum();
            let window = engine_info.context_window;
            let lens_name = state.active_lens.as_str();
            let mut lines = vec![
                format!(
                    "Context: ~{} / {} tokens ({}%)",
                    used,
                    window,
                    (used * 100) / window.max(1)
                ),
                format!("Lens: {}", lens_name),
                format!("History turn limit: {}", history_turn_limit),
                format!(
                    "Compaction: threshold={} keep_turns={} summary_max_tokens={}",
                    compaction_policy.threshold_tokens,
                    compaction_policy.keep_turns,
                    compaction_policy.summary_max_tokens
                ),
            ];
            if let Some(report) = &state.last_prompt_report {
                lines.push(String::new());
                lines.push("Last prompt assembly:".into());
                lines.push(format!("  System:    {} tokens", report.system_tokens));
                lines.push(format!("  History:   {} tokens", report.history_tokens));
                lines.push(format!(
                    "    dropped messages: {}",
                    report.dropped_history_messages
                ));
                lines.push(format!(
                    "  Reserved:  {} tokens",
                    report.reserved_output_tokens
                ));
                lines.push(format!("  Output cap: {} tokens", report.output_token_cap));
                lines.push(format!("  Budget:    {} tokens", report.total_input_budget));
                lines.push(format!(
                    "  Flow left: {} tokens",
                    report.flow_budget_remaining
                ));
                lines.push(format!(
                    "  Compaction: applied={}, compacted_messages={}",
                    report.compaction_applied, report.compacted_messages
                ));
            }
            CommandResult::Handled(CommandOutput { lines })
        }
        "/reset" => {
            state.reset_for_new_session();
            CommandResult::Handled(CommandOutput {
                lines: vec!["Flow reset and rotated to a new session.".into()],
            })
        }
        "/engine" => {
            let diagnostics = &engine_info.diagnostics;
            let caps = &diagnostics.capabilities;
            let lines = vec![
                format!("Current: {}/{}", agent_config.engine, agent_config.model),
                format!("Engine id: {}", diagnostics.engine_id),
                format!(
                    "Configured model: {}",
                    diagnostics.configured_model.as_deref().unwrap_or("n/a")
                ),
                format!(
                    "Endpoint: {}",
                    diagnostics.endpoint.as_deref().unwrap_or("n/a")
                ),
                format!(
                    "Transport: {}",
                    diagnostics.transport.as_deref().unwrap_or("n/a")
                ),
                format!("Context window: {}", caps.context_window),
                format!("Output cap: {}", caps.max_output_tokens_per_turn),
                format!("Capabilities: streaming={}", caps.supports_streaming),
            ];
            CommandResult::Handled(CommandOutput { lines })
        }
        "/help" => {
            let mut lines = vec![
                "Commands:".into(),
                "  /eco       — Eco lens (summaries)".into(),
                "  /standard  — Standard lens (auto-expand)".into(),
                "  /precise   — Precise lens (full content)".into(),
                "  /engine    — Show current engine".into(),
                "  /cost      — Token usage stats".into(),
                "  /context   — Context window usage".into(),
                "  /stop      — Cancel the current operation (Telegram)".into(),
                "  /agents    — List available agents and roles (Telegram)".into(),
                "  /reset     — Clear conversation".into(),
                "  /purge     — Clear conversation + wipe persistent memory".into(),
                "  /reload    — Re-read env vars + re-scan skills from disk".into(),
                "  /theme     — Toggle dark/light theme (Ctrl+T)".into(),
                "  /dark      — Switch to dark theme".into(),
                "  /light     — Switch to light theme".into(),
                "  /skills    — List discovered skills".into(),
                "  /enable N  — Enable a skill".into(),
                "  /disable N — Disable a skill".into(),
                "  /help      — This help".into(),
            ];
            if !skill_commands.is_empty() {
                lines.push(String::new());
                lines.push("Skill commands:".into());
                for (cmd, skill) in skill_commands {
                    lines.push(format!("  /{:<10} — from skill '{}'", cmd, skill));
                }
            }
            CommandResult::Handled(CommandOutput { lines })
        }
        _ => CommandResult::NotHandled,
    }
}

// ---------------------------------------------------------------------------
// Chat runtime service
// ---------------------------------------------------------------------------

/// Result of processing a single user message through the chat pipeline.
pub(crate) struct ChatTurnResult {
    pub assistant_text: Option<String>,
    pub system_notice: Option<String>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// Tool call outcomes from this turn (name, result).
    pub tool_outcomes: Vec<(String, String)>,
}

/// Central chat orchestration service.
pub(crate) struct ChatRuntimeService<'a> {
    pub engine: &'a dyn Engine,
    pub agent_id: &'a str,
    pub agent_config: &'a AgentConfig,
    pub history_turn_limit: usize,
    pub compaction_policy: FlowCompactionPolicy,
    pub system_prompt: String,
    pub tools: &'a [ToolDef],
    pub tool_executor: Option<&'a dyn ToolExecutor>,
    pub memory_manager: Option<&'a MemoryManager>,
    pub max_recall_entries: usize,
    #[allow(dead_code)] // reserved for future token-budget-based trimming of recall results
    pub max_recall_tokens: usize,
    pub tool_observer: Option<ToolResultObserver<'a>>,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Tools to expose via MCP bridge (Claude Code engine only).
    pub bridge_tools: Option<&'a [ToolDef]>,
}

pub(crate) fn needs_fresh_history_grounding(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    [
        "last",
        "latest",
        "most recent",
        "newest",
        "previous",
        "recently",
        "before that",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

impl<'a> ChatRuntimeService<'a> {
    pub(crate) async fn process_user_text(
        &self,
        state: &mut ChatLoopState,
        text: &str,
    ) -> Result<ChatTurnResult> {
        let compressed = text.to_string();

        let sender = Recipient {
            pipe_id: "cli".to_string(),
            peer_id: "local".to_string(),
            account_id: None,
            thread_id: None,
        };
        let flow_key = resolve_flow_key(
            self.agent_id,
            &self.agent_config.flow.scope,
            &sender,
            state.manual_session_id.as_deref(),
        );
        let switched_flow = state.active_flow_key.as_deref() != Some(flow_key.as_str());
        if switched_flow {
            state.messages = Vec::new();
            state.flow_token_usage = 0;
            state.active_flow_key = Some(flow_key.clone());
        }

        let user_message = Message {
            role: Role::User,
            content: compressed,
            tool_call_id: None,
            tool_calls: None,
        };
        state.flow_token_usage += estimate_tokens_approx_min1(&user_message.content) as u64;
        state.messages.push(user_message);
        enforce_history_turn_limit(&mut state.messages, self.history_turn_limit);

        let _ = maybe_compact_flow(
            &flow_key,
            &mut state.messages,
            &mut state.flow_token_usage,
            self.compaction_policy,
            "pre-engine",
        )
        .await;

        if state.flow_token_usage >= self.agent_config.limits.max_tokens_per_flow {
            return Ok(ChatTurnResult {
                assistant_text: None,
                system_notice: Some(
                    "Flow token limit reached. Use /reset to start a new session.".into(),
                ),
                total_input_tokens: state.total_input_tokens,
                total_output_tokens: state.total_output_tokens,
                tool_outcomes: Vec::new(),
            });
        }

        let needs_fresh_grounding = needs_fresh_history_grounding(text);

        // Recall relevant memories if a memory manager with a vector
        // backend is available.
        let memory_block = if needs_fresh_grounding {
            None
        } else if let Some(mgr) = self.memory_manager {
            let query = state
                .messages
                .last()
                .map(|m| m.content.as_str())
                .unwrap_or("");
            match mgr.search(query, self.max_recall_entries, None).await {
                Ok(hits) if !hits.is_empty() => {
                    let mut block = String::from("[Relevant memories]\n");
                    for h in &hits {
                        block.push_str(&format!("- {}\n", h.text));
                    }
                    Some(block)
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "Memory recall failed, continuing without memories");
                    None
                }
            }
        } else {
            None
        };

        let memory_tokens = memory_block
            .as_ref()
            .map(|b| estimate_tokens_approx_min1(b))
            .unwrap_or(0);

        let remaining_flow_tokens = self
            .agent_config
            .limits
            .max_tokens_per_flow
            .saturating_sub(state.flow_token_usage);
        let engine_output_cap = self.engine.max_output_tokens_per_turn() as usize;
        let base_input_budget = compute_base_input_budget(
            self.engine.context_window(),
            engine_output_cap,
            &self.system_prompt,
            remaining_flow_tokens,
        );
        let history_budget = base_input_budget.saturating_sub(memory_tokens);
        let history_assembly = assemble_recent_history(&state.messages, history_budget);

        let mut prompt_messages = Vec::new();
        if needs_fresh_grounding {
            prompt_messages.push(Message {
                role: Role::System,
                content: "For questions about last/latest/most recent history, do not rely on recalled memory summaries. Verify against the current conversation and available tools/workspace state before answering. If you cannot verify, say so clearly.".into(),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        if let Some(ref block) = memory_block {
            prompt_messages.push(Message {
                role: Role::System,
                content: block.clone(),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        prompt_messages.extend(history_assembly.messages.clone());
        if prompt_messages.is_empty() {
            return Ok(ChatTurnResult {
                assistant_text: None,
                system_notice: Some("Context budget exhausted. Use /reset to continue.".into()),
                total_input_tokens: state.total_input_tokens,
                total_output_tokens: state.total_output_tokens,
                tool_outcomes: Vec::new(),
            });
        }

        let context = EngineContext {
            workspace: self.agent_config.workspace.clone(),
            system_prompt: Some(self.system_prompt.clone()),
            bridge_tools: self.bridge_tools.map(|t| t.to_vec()),
            max_tool_rounds: Some(self.agent_config.limits.max_tool_rounds),
            max_mcp_result_chars: Some(self.agent_config.limits.max_mcp_result_chars),
        };
        let resp = collect_engine_response(
            self.engine,
            &prompt_messages,
            self.tools,
            &context,
            self.tool_executor,
            self.tool_observer,
            self.cancel,
            None, // chat runtime has its own flow-level budget enforcement
            self.agent_config.limits.max_tool_rounds,
            self.agent_config.limits.max_tool_result_chars,
            self.agent_config.limits.stream_event_timeout_secs,
            self.agent_config.limits.compact_result_limit,
        )
        .await?;

        state.total_input_tokens += resp.input_tokens_delta;
        state.total_output_tokens += resp.output_tokens_delta;
        let response_text = resp.text;
        let tool_outcomes = resp.tool_outcomes;

        if !response_text.is_empty() {
            let assistant_message = Message {
                role: Role::Assistant,
                content: response_text.clone(),
                tool_call_id: None,
                tool_calls: None,
            };
            state.flow_token_usage +=
                estimate_tokens_approx_min1(&assistant_message.content) as u64;
            state.messages.push(assistant_message);
            enforce_history_turn_limit(&mut state.messages, self.history_turn_limit);

            let _ = maybe_compact_flow(
                &flow_key,
                &mut state.messages,
                &mut state.flow_token_usage,
                self.compaction_policy,
                "post-engine",
            )
            .await;
        }

        // Warn when approaching the token limit (80% threshold).
        let max_tokens = self.agent_config.limits.max_tokens_per_flow;
        let usage_pct = if max_tokens > 0 {
            (state.flow_token_usage as f64 / max_tokens as f64 * 100.0) as u32
        } else {
            0
        };
        let budget_notice = if usage_pct >= 80 {
            let remaining = max_tokens.saturating_sub(state.flow_token_usage);
            Some(format!(
                "⚠️ Token budget: {}% used ({}/{} tokens, ~{} remaining). Use /reset to start fresh.",
                usage_pct, state.flow_token_usage, max_tokens, remaining
            ))
        } else {
            None
        };

        Ok(ChatTurnResult {
            assistant_text: if response_text.is_empty() {
                None
            } else {
                Some(response_text)
            },
            system_notice: budget_notice,
            total_input_tokens: state.total_input_tokens,
            total_output_tokens: state.total_output_tokens,
            tool_outcomes,
        })
    }
}
