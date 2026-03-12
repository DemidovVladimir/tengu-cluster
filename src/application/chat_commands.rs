//! Runtime slash-command handlers for chat flows.

use crate::domain::chat::{ChatLoopState, FlowCompactionPolicy};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::Lens;

/// Cached engine metadata for slash commands (avoids needing &dyn Engine on the UI thread).
pub(crate) struct EngineInfo {
    pub context_window: usize,
    pub diagnostics: tengu_core::EngineDiagnostics,
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
    agent_config: &tengu_core::config::AgentConfig,
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
            let mut lines = vec![
                "Session Stats".into(),
                "─────────────────────────────".into(),
                format!(" Input tokens:  {}", state.total_input_tokens),
                format!(" Output tokens: {}", state.total_output_tokens),
                format!(
                    " Total:         {}",
                    state.total_input_tokens + state.total_output_tokens
                ),
            ];
            if state.tokens_saved > 0 {
                lines.push(String::new());
                lines.push(" Saved by refiner:".into());
                lines.push(format!(
                    "   Prompt compression: -{} tokens",
                    state.tokens_saved
                ));
            }
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
