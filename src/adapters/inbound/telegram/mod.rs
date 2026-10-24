//! Telegram adapter — pipe, commands, and runtime in one module.
//!
//! Provides the full Telegram integration:
//!
//! - **TelegramPipe** — concrete struct for chat actions, text / media sending,
//!   and teloxide-based message dispatch.
//! - **No tool approvals** — no tool call waits for an approval; the old
//!   `[telegram] tool_approvals` / `approve_only` keys fail the load.
//! - **Typing indicator** — runs as an independent `tokio::spawn` task.
//! - **File attachments** — documents and photos downloaded and saved to
//!   `{workspace}/.tengu-attachments/`.
//! - **Message chunking** — long responses split at `\n\n` boundaries, max 4000
//!   chars per chunk (Telegram's 4096 limit with safety margin).
//! - **Per-user state** — each Telegram user gets their own `ChatLoopState`.
//! - **Secret redaction** — all outbound text passes through `SecretRegistry::redact`.
//! - **Hot-reload** — skills are re-scanned on each message if files changed.
//! - **Multi-agent routing** — `@role: message` targeting; plain messages go to
//!   the orchestrator (plan, then steps). Only agents with a `description` or
//!   the `default` one are reachable: a private agent (the exec-tool and
//!   signing owners) is never routable, the default or listed.
//! - **Access control, fail closed** — `tengu telegram` refuses to start
//!   without an allow-list (`[telegram] allowed_users` +
//!   `TENGU_TELEGRAM_ALLOWED_USERS`); an unlisted sender gets "Unauthorized.".
//!
//! | File | Holds |
//! |---|---|
//! | `pipe.rs` | `TelegramPipe` (teloxide: connect, send, typing), file downloads, tool-activity log |
//! | `session.rs` | `TelegramSession` + per-agent state: build, main loop, dispatch, idle eviction |
//! | `turn.rs` | `@role:` routing, the in-process chat turn, the orchestrator turn |
//! | `commands.rs` | slash commands (`/stop`, `/project`, `/agents`, `/reset`, `/purge`, `/reload`, skill commands, the shared chat commands) |
//! | this file | access control, routing helpers, session ids, `run_telegram` |

mod commands;
mod pipe;
mod session;
#[cfg(test)]
mod tests;
mod turn;

use pipe::*;
use session::*;

use crate::adapters::outbound::engines::build_engine;
use crate::application::chat::flow::{resolve_flow_compaction_policy, resolve_history_turn_limit};
use crate::application::chat::service::{
    handle_chat_command, needs_fresh_history_grounding, ChatRuntimeService, CommandResult,
    EngineInfo,
};
use crate::application::skills::registry::{
    FileSystemSkillSource, SkillCommandMatch, SkillCommandRouter, SkillRegistry,
};
use crate::config::Config;
use crate::domain::message::{
    DeliveryOptions, InboundMessage, MediaPayload, Recipient, ToolCall, ToolDef,
};
use crate::domain::secrets::SecretRegistry;
use crate::domain::session::ChatLoopState;
use crate::ports::engine::Engine;
use crate::ports::tool_activity::ToolActivityPort;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ===========================================================================
// Constants
// ===========================================================================

/// Maximum characters per Telegram message (with safety margin).
const TELEGRAM_MAX_LEN: usize = 4000;

/// How often to sweep for idle user states.
const EVICTION_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Idle threshold before evicting a user state.
const IDLE_EVICTION_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(1800);

/// `[keys.env]` target for the Telegram route: `TELEGRAM_API_URL = "telegram"`
/// exports `<proxy>/telegram`, and then no `TELEGRAM_BOT_TOKEN` is needed.
const TELEGRAM_API_URL_ENV: &str = "TELEGRAM_API_URL";

/// The seal-proxy Telegram URL, when `[keys]` exported one.
fn sealed_api_url() -> Option<String> {
    std::env::var(TELEGRAM_API_URL_ENV)
        .ok()
        .filter(|u| crate::adapters::outbound::keys::header_mode(u).is_some())
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Build a fallback reply when the LLM produced no text but executed tools.
fn build_tool_summary_fallback(tool_outcomes: &[(String, String)]) -> String {
    let mut seen = Vec::new();
    for (name, _) in tool_outcomes {
        if !seen.contains(name) {
            seen.push(name.clone());
        }
    }
    format!(
        "Done. Completed {} tool call{}: {}",
        tool_outcomes.len(),
        if tool_outcomes.len() == 1 { "" } else { "s" },
        seen.join(", ")
    )
}

// ===========================================================================
// Helpers
// ===========================================================================

/// The Telegram user ids the bot answers: `[telegram] allowed_users` plus
/// `TENGU_TELEGRAM_ALLOWED_USERS` (comma-separated).
fn build_allowed_users(config: &Config) -> HashSet<String> {
    allowed_users(
        &config.telegram.allowed_users,
        std::env::var("TENGU_TELEGRAM_ALLOWED_USERS")
            .ok()
            .as_deref(),
    )
}

/// [`build_allowed_users`] over explicit inputs; blank entries are dropped.
fn allowed_users(configured: &[String], env: Option<&str>) -> HashSet<String> {
    configured
        .iter()
        .map(String::as_str)
        .chain(env.into_iter().flat_map(|v| v.split(',')))
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
        .map(String::from)
        .collect()
}

/// `tengu telegram` refuses to start without an allow-list: an empty one
/// must never mean "every sender".
fn require_allowed_users(allowed: &HashSet<String>) -> Result<()> {
    if allowed.is_empty() {
        anyhow::bail!(
            "tengu telegram: no allowed users — refusing to start. Set [telegram] allowed_users \
             = [\"<your Telegram user id>\"] in the config or TENGU_TELEGRAM_ALLOWED_USERS=<id>[,<id>…] \
             (@userinfobot shows your id)"
        );
    }
    Ok(())
}

/// Only a listed sender gets an answer — an empty list answers no one.
fn is_authorized(allowed: &HashSet<String>, sender: &str) -> bool {
    allowed.contains(sender)
}

/// Telegram reaches an agent with a `description` (planner-routable) or the
/// `default` one — never a private agent (neither: the exec-tool and signing
/// owners, `config/risk.rs`, `config/solana.rs`). A private agent is not
/// `@<id>:` / `@<role>:`-routable and never the default.
fn telegram_reachable(agent: &crate::config::AgentConfig) -> bool {
    agent.default || agent.description.is_some()
}

/// `@<key>:` routing keys of the reachable agents → agent id: each role
/// (lowercased, `-` → `_`, as `parse_agent_routing` reads it), then each
/// id (an id wins over another agent's role).
fn telegram_routes<'a>(
    agents: impl Iterator<Item = (&'a str, &'a crate::config::AgentConfig)>,
) -> HashMap<String, String> {
    let mut reachable: Vec<_> = agents.filter(|(_, a)| telegram_reachable(a)).collect();
    reachable.sort_by_key(|(id, _)| *id);
    let mut routes = HashMap::new();
    for (id, agent) in &reachable {
        let role = agent.role.as_deref().unwrap_or_default();
        let role = role.trim().to_lowercase().replace('-', "_");
        if !role.is_empty() {
            routes.insert(role, id.to_string());
        }
    }
    // Keyed the way `parse_agent_routing` normalises what the user types
    // (lowercase, `-` → `_`), so `@lp-exec:` reaches agent `lp-exec`.
    for (id, _) in &reachable {
        routes.insert(id.to_lowercase().replace('-', "_"), id.to_string());
    }
    routes
}

/// One sender's planner / runner session id: `TENGU_SESSION_ID` + `-` +
/// sender when that override is set (so two senders never share one),
/// else a fresh UUID.
fn telegram_session_id(sender: &str) -> String {
    session_id_for_sender(std::env::var("TENGU_SESSION_ID").ok(), sender)
}

fn session_id_for_sender(base: Option<String>, sender: &str) -> String {
    match base.filter(|s| !s.trim().is_empty()) {
        Some(base) => format!("{base}-{sender}"),
        None => uuid::Uuid::new_v4().to_string(),
    }
}

/// Log an orchestrator's progress events at `info` so operators can follow
/// planner-driven turns from the bot's log.
fn log_orchestrator_events(orch: &crate::application::orchestrator::Orchestrator) {
    let mut rx = orch.subscribe();
    tokio::spawn(async move {
        use crate::application::orchestrator::OrchestratorEvent;
        loop {
            match rx.recv().await {
                Ok(OrchestratorEvent::StepStarted { step_id, agent }) => {
                    info!(step = %step_id.0, agent = %agent, "orch StepStarted");
                }
                Ok(OrchestratorEvent::ReplanTriggered { reason }) => {
                    info!(reason = %reason, "orch ReplanTriggered");
                }
                Ok(OrchestratorEvent::PlanCompleted { cancelled, .. }) => {
                    info!(cancelled, "orch PlanCompleted");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(dropped = n, "orch event subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Resolves once `/stop` set `flag`. The dispatcher sets it from its own
/// task while a turn runs; polled every 200 ms.
async fn cancel_requested(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Where plain messages go: the `default = true` agent, else the first
/// reachable one by id; `None` when no agent is reachable.
fn telegram_default_agent<'a>(
    agents: impl Iterator<Item = (&'a str, &'a crate::config::AgentConfig)>,
) -> Option<String> {
    let mut reachable: Vec<_> = agents.filter(|(_, a)| telegram_reachable(a)).collect();
    reachable.sort_by_key(|(id, _)| *id);
    reachable
        .iter()
        .find(|(_, a)| a.default)
        .or(reachable.first())
        .map(|(id, _)| id.to_string())
}

fn sanitize_attachment_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ===========================================================================
// Entry point
// ===========================================================================

/// Run the headless Telegram bot adapter (sync — call from `block_in_place`).
///
/// Creates its own tokio runtime internally so that state containing nested
/// runtimes drops in a sync context, avoiding the "Cannot drop a runtime in a
/// context where blocking is not allowed" panic.
pub(crate) fn run_telegram(config: Config, secret_registry: Arc<SecretRegistry>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Telegram runtime");

    let (mut session, inbound_rx) = TelegramSession::build(config, secret_registry, &rt)?;
    // With `[orchestrator]`, one recording of every sender's plans — opened
    // once the session is built, so a refused start (no allow-list) leaves
    // no run without its `run.closed`.
    let trace = crate::bootstrap::trace::open_orchestration(
        &session.config,
        crate::domain::trace::RunKind::Telegram,
        &session.secret_registry,
    );
    session.trace.clone_from(&trace);
    let out = session.run(rt, inbound_rx);
    if let Some(t) = &trace {
        crate::bootstrap::trace::close(&**t, "telegram stopped", out.is_err());
    }
    out
}
