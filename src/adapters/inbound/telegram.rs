//! Telegram adapter — pipe, commands, and runtime in one module.
//!
//! Provides the full Telegram integration:
//!
//! - **TelegramPipe** — concrete struct for chat actions, text / media sending,
//!   and teloxide-based message dispatch.
//! - **No tool approvals** — `[telegram] tool_approvals` / `approve_only` load
//!   but nothing waits for an approval (`Config::load` warns naming them).
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

use crate::adapters::outbound::engines::build_engine;
use crate::adapters::outbound::secrets::SanitizedToolExecutor;
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
use crate::ports::engine::ToolExecutor;
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
// TelegramPipe — low-level Telegram API wrapper
// ===========================================================================

/// Telegram bot pipe backed by teloxide.
struct TelegramPipe {
    /// Built once in `new` (see `build_bot`); `Bot` is `Arc`-backed, clones
    /// are cheap.
    bot: teloxide::Bot,
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    turn_cancel: Option<Arc<AtomicBool>>,
}

impl TelegramPipe {
    /// Bot client honouring `[egress]`. teloxide takes a reqwest 0.11 client
    /// (`reqwest011` alias). Under a proxy it goes through the proxy's HTTP
    /// CONNECT form (Arti serves CONNECT on the SOCKS port) with Tor-sized
    /// timeouts: teloxide's defaults (5s connect / 17s total, and 0.10 does
    /// not extend the total for long polls) time out over Tor, where `GetMe`
    /// takes 10–15s and each `getUpdates` is the 10s long poll plus that.
    fn build_bot(token: &str) -> Result<teloxide::Bot> {
        let mut builder = teloxide::net::default_reqwest_settings();
        if let Some(proxy) = crate::adapters::outbound::egress::policy().http_connect_proxy() {
            builder = builder
                .proxy(reqwest011::Proxy::all(proxy).context("telegram proxy url")?)
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(60));
        }
        let client = builder.build().context("building telegram http client")?;
        Ok(teloxide::Bot::with_client(token, client))
    }

    fn bot(&self) -> anyhow::Result<teloxide::Bot> {
        Ok(self.bot.clone())
    }
}

impl TelegramPipe {
    /// Create a new pipe with the given bot token and cancellation flag.
    fn new(token: String, cancel: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self {
            bot: Self::build_bot(&token)?,
            shutdown: Arc::new(Mutex::new(None)),
            turn_cancel: Some(cancel),
        })
    }

    /// Start the teloxide dispatcher and forward incoming messages to `inbound_tx`.
    async fn connect(&self, inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::requests::Requester;

        let bot = self.bot()?;
        let me = bot
            .get_me()
            .await
            .map_err(|e| anyhow::anyhow!("Invalid Telegram bot token: {}", e))?;
        info!(bot = %me.username(), "Telegram bot authenticated");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown.lock().await = Some(shutdown_tx);

        let inbound_tx = inbound_tx.clone();
        let turn_cancel = self.turn_cancel.clone();

        tokio::spawn(async move {
            type MediaGroupBuffer = Arc<Mutex<HashMap<String, MediaGroupState>>>;

            struct MediaGroupState {
                text: String,
                media: Vec<MediaPayload>,
                chat_id: String,
                sender_id: String,
            }

            let media_groups: MediaGroupBuffer = Arc::new(Mutex::new(HashMap::new()));

            let cancel_for_handler = turn_cancel.clone();
            let groups_ref = Arc::clone(&media_groups);
            let msg_handler = Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
                let tx = inbound_tx.clone();
                let cancel = cancel_for_handler.clone();
                let groups = Arc::clone(&groups_ref);
                async move {
                    let text = msg.text().or(msg.caption()).unwrap_or_default().to_string();

                    if text == "/stop" || text.starts_with("/stop@") {
                        if let Some(ref flag) = cancel {
                            flag.store(true, Ordering::Relaxed);
                        }
                        let chat_id = msg.chat.id.0;
                        let _ = bot
                            .send_message(
                                teloxide::types::ChatId(chat_id),
                                "⏹ Stopping current operation...",
                            )
                            .await;
                        return Ok(());
                    }

                    let mut media_payloads: Vec<MediaPayload> = Vec::new();

                    if let Some(doc) = msg.document() {
                        match download_telegram_file(
                            &bot,
                            &doc.file.id,
                            doc.mime_type.as_ref().map(|m| m.to_string()),
                            doc.file_name.clone(),
                        )
                        .await
                        {
                            Ok(payload) => media_payloads.push(payload),
                            Err(e) => error!("Failed to download Telegram document: {}", e),
                        }
                    }

                    if let Some(photos) = msg.photo() {
                        if let Some(photo) = photos.last() {
                            match download_telegram_file(
                                &bot,
                                &photo.file.id,
                                Some("image/jpeg".to_string()),
                                Some("photo.jpg".to_string()),
                            )
                            .await
                            {
                                Ok(payload) => media_payloads.push(payload),
                                Err(e) => error!("Failed to download Telegram photo: {}", e),
                            }
                        }
                    }

                    if text.is_empty() && media_payloads.is_empty() {
                        return Ok::<(), teloxide::RequestError>(());
                    }

                    let chat_id = msg.chat.id.0.to_string();
                    let sender_id = msg
                        .from
                        .as_ref()
                        .map(|u| u.id.0.to_string())
                        .unwrap_or_else(|| chat_id.clone());

                    if let Some(group_id) = msg.media_group_id() {
                        let group_id = group_id.to_string();
                        let is_first = {
                            let mut map = groups.lock().await;
                            let entry =
                                map.entry(group_id.clone())
                                    .or_insert_with(|| MediaGroupState {
                                        text: String::new(),
                                        media: Vec::new(),
                                        chat_id: chat_id.clone(),
                                        sender_id: sender_id.clone(),
                                    });
                            if !text.is_empty() && entry.text.is_empty() {
                                entry.text = text;
                            }
                            entry.media.extend(media_payloads);
                            entry.media.len() == 1
                        };

                        if is_first {
                            let flush_tx = tx.clone();
                            let flush_groups = Arc::clone(&groups);
                            let flush_group_id = group_id;
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                let state = flush_groups.lock().await.remove(&flush_group_id);
                                if let Some(state) = state {
                                    debug!(
                                        group_id = %flush_group_id,
                                        attachments = state.media.len(),
                                        "Flushing media group"
                                    );
                                    let inbound = InboundMessage {
                                        sender: Recipient {
                                            pipe_id: "telegram".to_string(),
                                            peer_id: state.sender_id,
                                            account_id: None,
                                            thread_id: Some(state.chat_id),
                                        },
                                        content: state.text,
                                        timestamp: chrono::Utc::now(),
                                        media: if state.media.is_empty() {
                                            None
                                        } else {
                                            Some(state.media)
                                        },
                                    };
                                    if flush_tx.send(inbound).await.is_err() {
                                        error!("Failed to forward media group to inbound channel");
                                    }
                                }
                            });
                        }
                        return Ok(());
                    }

                    debug!(
                        chat_id = %chat_id,
                        sender = %sender_id,
                        attachments = media_payloads.len(),
                        "Telegram message received"
                    );

                    let inbound = InboundMessage {
                        sender: Recipient {
                            pipe_id: "telegram".to_string(),
                            peer_id: sender_id,
                            account_id: None,
                            thread_id: Some(chat_id),
                        },
                        content: text,
                        timestamp: chrono::Utc::now(),
                        media: if media_payloads.is_empty() {
                            None
                        } else {
                            Some(media_payloads)
                        },
                    };

                    if tx.send(inbound).await.is_err() {
                        error!("Failed to forward Telegram message to inbound channel");
                    }
                    Ok(())
                }
            });

            let handler = dptree::entry().branch(msg_handler);

            let mut dispatcher = Dispatcher::builder(bot, handler)
                .enable_ctrlc_handler()
                .build();

            tokio::select! {
                _ = dispatcher.dispatch() => {}
                _ = &mut shutdown_rx => {
                    info!("Telegram pipe shutting down");
                }
            }
        });

        Ok(())
    }

    /// Signal the dispatcher to shut down gracefully.
    async fn disconnect(&self) -> Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    /// Send a "typing…" indicator to the target chat.
    async fn send_chat_action(&self, target: &Recipient) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::{ChatAction, ChatId};

        let bot = self.bot()?;
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id"))?;
        bot.send_chat_action(ChatId(chat_id), ChatAction::Typing)
            .await
            .map_err(|e| anyhow::anyhow!("send_chat_action failed: {}", e))?;
        Ok(())
    }

    /// Send a plain text message to the target chat.
    async fn send_text(
        &self,
        target: &Recipient,
        text: &str,
        _opts: &DeliveryOptions,
    ) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::ChatId;

        let bot = self.bot()?;
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Telegram chat_id"))?;

        bot.send_message(ChatId(chat_id), text)
            .await
            .map_err(|e| anyhow::anyhow!("Telegram send_text failed: {}", e))?;

        Ok(())
    }
}

/// Download a file from Telegram servers given its file_id.
async fn download_telegram_file(
    bot: &teloxide::Bot,
    file_id: &str,
    mime_type: Option<String>,
    filename: Option<String>,
) -> std::result::Result<MediaPayload, Box<dyn std::error::Error + Send + Sync>> {
    use futures::TryStreamExt;
    use teloxide::net::Download;
    use teloxide::requests::Requester;

    let tf = bot.get_file(file_id).await?;
    let bytes: Vec<u8> = bot
        .download_file_stream(&tf.path)
        .try_fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await?;

    debug!(
        file_id = %file_id,
        size = bytes.len(),
        filename = ?filename,
        "Downloaded Telegram file"
    );

    Ok(MediaPayload {
        mime_type: mime_type.unwrap_or_else(|| "application/octet-stream".to_string()),
        data: bytes,
        filename,
    })
}

// ===========================================================================
// Port adapters
// ===========================================================================

/// Shared state for the current recipient — updated before each engine turn.
type CurrentRecipient = Arc<std::sync::Mutex<Option<Recipient>>>;

/// Telegram adapter for tool progress — logs to tracing only, no chat messages.
struct TelegramToolActivityAdapter {
    agent_label: String,
}

impl ToolActivityPort for TelegramToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let (title, detail) = crate::adapters::inbound::activity::build_tool_activity_text(call);
        let mut text = format!("[{}] {}", self.agent_label, title);
        if let Some(detail) = detail {
            text.push_str(": ");
            text.push_str(&detail);
        }
        tracing::info!(
            agent = %self.agent_label,
            tool = %call.name,
            message = %text,
            "Tool activity"
        );
    }
}

fn make_tool_activity_adapter(agent_label: impl Into<String>) -> Arc<dyn ToolActivityPort> {
    Arc::new(TelegramToolActivityAdapter {
        agent_label: agent_label.into(),
    })
}

// ===========================================================================
// Per-agent runtime state
// ===========================================================================

/// Per-agent runtime state for multi-agent Telegram support.
struct TelegramAgentState {
    agent_id: String,
    agent_config: crate::config::AgentConfig,
    engine: Arc<dyn Engine>,
    engine_info: EngineInfo,
    workspace: Option<std::path::PathBuf>,
    base_tools: Vec<ToolDef>,
    bridge_base_tools: Vec<ToolDef>,
    skill_source: Option<FileSystemSkillSource>,
    skill_registry: SkillRegistry,
    current_tools: Vec<ToolDef>,
    current_bridge_tools: Vec<ToolDef>,
    current_system_prompt: String,
    advertise_workspace_tools: bool,
    history_turn_limit: usize,
    compaction_policy: crate::domain::session::FlowCompactionPolicy,
    role: Option<String>,
}

impl TelegramAgentState {
    /// Rebuild bridge tools from bridge_base_tools + skill registry.
    fn rebuild_bridge_tools(&mut self) {
        if !self.bridge_base_tools.is_empty() {
            self.current_bridge_tools = crate::bootstrap::tools::rebuild_tools(
                &self.bridge_base_tools,
                &self.skill_registry,
            );
        }
    }
}

// ===========================================================================
// TelegramSession — shared state for the message loop
// ===========================================================================

struct TelegramSession {
    // Infrastructure
    pipe: Arc<TelegramPipe>,
    turn_cancel: Arc<AtomicBool>,
    current_recipient: CurrentRecipient,

    // Config
    config: Config,
    memory_config: crate::config::MemoryConfig,
    allowed_users: HashSet<String>,
    is_multi_agent: bool,
    base_workspaces: HashMap<String, std::path::PathBuf>,
    delivery_opts: DeliveryOptions,

    // Agents
    agent_states: HashMap<String, TelegramAgentState>,
    default_agent_id: String,
    role_to_agent: HashMap<String, String>,

    // Services
    memory_manager_handle: Option<Arc<crate::application::memory::manager::MemoryManager>>,
    secret_registry: Arc<SecretRegistry>,

    // Harness-owned orchestration. Constructed when
    // `config.orchestrator.is_some()`.
    //
    // `orchestrator_snapshots` is the shared state the per-message factory
    // closure reads from. `execute_chat_turn` populates it with fresh
    // per-agent `ChatTurnInputs` after hot-reload fires, then calls
    // `orchestrator.handle`. The factory (embedded in the orchestrator)
    // looks up agent inputs by name through the same `Arc<RwLock<...>>`.
    //
    // `_memory_manager` is held so it stays alive for the lifetime of the
    // orchestrators (each holds an `Arc<MemoryManager>` internally).
    //
    // `orchestrator_factory` is `Some` when `[orchestrator]` is configured;
    // `orchestrators` holds one orchestrator per sender id, each with its
    // own session id (`orchestrator_for`), dropped with the sender's idle
    // state (`evict_idle_users`).
    orchestrator_factory: Option<Arc<dyn crate::ports::orchestration::ChatServiceFactory>>,
    orchestrators: HashMap<String, Arc<crate::application::orchestrator::Orchestrator>>,
    orchestrator_snapshots: crate::bootstrap::orchestrator::OrchestratorSnapshots,
    /// This process's recording of every sender's plans (`RunKind::Telegram`,
    /// `run_telegram`); `None` without `[orchestrator]`.
    trace: Option<Arc<dyn crate::ports::trace::TraceSink>>,
    _memory_manager: Arc<crate::application::memory::manager::MemoryManager>,

    // Per-user mutable state
    user_states: HashMap<String, ChatLoopState>,
    user_state_last_active: HashMap<String, std::time::Instant>,
    last_eviction_check: std::time::Instant,
    user_active_agent: HashMap<String, String>,

    // Routing
    skill_command_router: SkillCommandRouter,
    activity_log: Vec<crate::adapters::inbound::channel::ActivityEntry>,
}

impl TelegramSession {
    // -------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------

    fn build(
        config: Config,
        secret_registry: Arc<SecretRegistry>,
        rt: &tokio::runtime::Runtime,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<InboundMessage>)> {
        // Fail closed: an empty allow-list never means "everyone".
        let allowed_users = build_allowed_users(&config);
        require_allowed_users(&allowed_users)?;
        info!(count = allowed_users.len(), "Telegram allowed users loaded");

        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .map_err(|_| anyhow::anyhow!("TELEGRAM_BOT_TOKEN env var is required"))?;

        let memory_config = config.memory.clone();

        let first_workspace: Option<std::path::PathBuf> = config.agents.values().find_map(|ac| {
            ac.workspace
                .as_ref()
                .map(|p| crate::config::paths::expand_tilde(p))
        });

        // Memory backend (shared `Embedder` + `VectorStore`) exposed via
        // `MemoryManager`. The manager is always constructed (so the
        // orchestrator has a valid handle), but `has_memory` gates tool
        // registration on whether the vector backend actually installed.
        let memory_manager_early = crate::bootstrap::memory::build_memory_manager(
            &memory_config,
            rt,
            first_workspace.as_deref(),
        );
        let has_memory = rt.block_on(async { memory_manager_early.has_vector_backend().await });

        crate::adapters::outbound::scaffold::maybe_apply_scaffold(&config);

        // Build per-agent runtime state. Private agents are built too (the
        // orchestrator's planner may be one) but never routable or default.
        let mut agent_states: HashMap<String, TelegramAgentState> = HashMap::new();

        for (agent_id, agent_config) in &config.agents {
            let engine: Arc<dyn Engine> =
                match build_engine(agent_id, agent_config, config.claude_code.as_ref()) {
                    Ok(e) => Arc::from(e),
                    Err(e) => {
                        warn!(agent_id = %agent_id, error = %e, "Failed to build engine, skipping");
                        continue;
                    }
                };

            let engine_info = EngineInfo {
                context_window: engine.context_window(),
                diagnostics: engine.diagnostics(),
            };

            let workspace: Option<std::path::PathBuf> = agent_config
                .workspace
                .as_ref()
                .map(|p| crate::config::paths::expand_tilde(p));

            let advertise_workspace_tools =
                engine.supports_tool_use() && !engine.manages_own_workspace();
            let manages_workspace = engine.manages_own_workspace();
            let uses_tools = advertise_workspace_tools && workspace.is_some();
            // The agent's `tools` list applies here as on every surface
            // (`agent_base_tools`; empty = every base tool).
            let base_tools =
                crate::bootstrap::tools::agent_base_tools(agent_config, uses_tools, has_memory);
            let bridge_base_tools: Vec<ToolDef> = if manages_workspace && workspace.is_some() {
                rt.block_on(crate::bootstrap::tools::with_mcp_bridge_tools(
                    crate::bootstrap::tools::agent_base_tools(agent_config, true, has_memory),
                    &agent_config.tools,
                    &config.mcp_servers,
                ))
            } else {
                vec![]
            };

            let skill_source: Option<FileSystemSkillSource> = workspace
                .as_ref()
                .map(|ws| FileSystemSkillSource::new(ws.clone()));

            let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
            let mut skill_registry = SkillRegistry::new(base_reserved)
                .with_allowlist(Some(agent_config.skill_packages.clone()))
                .with_shell_skills(!agent_config.no_shell_fallback);
            if let Some(ref src) = skill_source {
                skill_registry.reload(src);
            }

            let current_tools =
                crate::bootstrap::tools::rebuild_tools(&base_tools, &skill_registry);
            let current_bridge_tools: Vec<ToolDef> = if manages_workspace {
                crate::bootstrap::tools::rebuild_tools(&bridge_base_tools, &skill_registry)
            } else {
                vec![]
            };
            let current_system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
                agent_config,
                advertise_workspace_tools,
                &skill_registry,
                &current_tools,
            );

            let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
            let compaction_policy = resolve_flow_compaction_policy(
                &agent_config.flow,
                agent_config.limits.max_tokens_per_flow,
                engine.context_window(),
                engine.max_output_tokens_per_turn() as usize,
            );

            let role = agent_config.role.clone();

            info!(
                agent_id = %agent_id,
                role = ?role,
                tools = current_tools.len(),
                bridge_tools = current_bridge_tools.len(),
                manages_workspace = manages_workspace,
                "Registered Telegram agent"
            );

            agent_states.insert(
                agent_id.clone(),
                TelegramAgentState {
                    agent_id: agent_id.clone(),
                    agent_config: agent_config.clone(),
                    engine,
                    engine_info,
                    workspace,
                    base_tools,
                    bridge_base_tools,
                    skill_source,
                    skill_registry,
                    current_tools,
                    current_bridge_tools,
                    current_system_prompt,
                    advertise_workspace_tools,
                    history_turn_limit,
                    compaction_policy,
                    role,
                },
            );
        }

        let built = || {
            agent_states
                .iter()
                .map(|(id, s)| (id.as_str(), &s.agent_config))
        };
        let role_to_agent = telegram_routes(built());
        let default_agent_id = telegram_default_agent(built()).ok_or_else(|| {
            anyhow::anyhow!(
                "no agent Telegram may reach: set `default = true` on one or give one a \
                 `description` — a private agent (neither) is never reachable from Telegram"
            )
        })?;
        let reachable = built().filter(|(_, a)| telegram_reachable(a)).count();

        let base_workspaces: HashMap<String, std::path::PathBuf> = agent_states
            .iter()
            .filter_map(|(id, a)| a.workspace.as_ref().map(|w| (id.clone(), w.clone())))
            .collect();

        // Inject team awareness into each agent's system prompt: the agents a
        // Telegram user can route to.
        if reachable > 1 {
            let mut team_block = String::from("\n\n## Team Members\n");
            team_block.push_str(
                "If a request is outside your expertise, suggest the user route to the right agent.\n",
            );
            team_block.push_str("Format: @role: message\n\n");
            let mut members: Vec<&TelegramAgentState> = agent_states
                .values()
                .filter(|s| telegram_reachable(&s.agent_config))
                .collect();
            members.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
            for state in members {
                let role_key = state.role.as_deref().unwrap_or(&state.agent_id);
                let name = state
                    .agent_config
                    .identity
                    .name
                    .as_deref()
                    .unwrap_or(&state.agent_id);
                team_block.push_str(&format!("- @{}: {}\n", role_key, name));
            }
            for state in agent_states.values_mut() {
                state.current_system_prompt.push_str(&team_block);
            }
        }

        let is_multi_agent = agent_states.len() > 1;

        info!(
            agents = agent_states.len(),
            default = %default_agent_id,
            "Telegram multi-agent setup complete"
        );

        let turn_cancel = Arc::new(AtomicBool::new(false));

        let pipe = Arc::new(TelegramPipe::new(bot_token, Arc::clone(&turn_cancel))?);
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(256);
        rt.block_on(pipe.connect(inbound_tx))?;

        let current_recipient: CurrentRecipient = Arc::new(std::sync::Mutex::new(None));

        let skill_command_router = {
            let default_agent = agent_states.get(&default_agent_id);
            default_agent
                .map(|a| SkillCommandRouter::from_registry(&a.skill_registry))
                .unwrap_or_else(|| SkillCommandRouter::from_registry(&SkillRegistry::new(vec![])))
        };

        info!("Telegram bot started — waiting for messages (Ctrl+C to stop)");

        // Harness-owned orchestration (Track A — factory-closure wiring).
        //
        // `MemoryManager` is currently left empty — see Track B (memory
        // vector port) for the provider wiring. Empty manager just means
        // no memory injection; orchestration dispatch still works.
        //
        // `orchestrator_snapshots` is an `Arc<RwLock<HashMap<agent,
        // ChatTurnInputs>>>` shared between this session and the factory
        // closure. Before each `orchestrator.handle(...)` call we populate
        // the map with fresh per-agent snapshots (after hot-reload fires);
        // the factory reads back from the same map when the DAG executor
        // spawns a step for a given agent.
        //
        // `memory_manager_early` comes from Track B's `build_memory_manager`
        // which registers `BuiltinMemoryProvider` and installs the vector
        // backend. Reusing it here means orchestrator turns get real memory
        // injection/writes through the same provider the LLM-callable tools
        // use.
        let memory_manager = memory_manager_early;
        let orchestrator_snapshots: crate::bootstrap::orchestrator::OrchestratorSnapshots =
            Arc::new(std::sync::RwLock::new(HashMap::new()));
        // One orchestrator per sender, built on first use (`orchestrator_for`):
        // each has its own session id, so one sender's planner messages and
        // recall never reach another's prompt.
        let orchestrator_factory: Option<Arc<dyn crate::ports::orchestration::ChatServiceFactory>> =
            config.orchestrator.is_some().then(|| {
                let inputs_fn = crate::bootstrap::orchestrator::snapshots_inputs_fn(Arc::clone(
                    &orchestrator_snapshots,
                ));
                Arc::new(crate::bootstrap::orchestrator::RuntimeChatServiceFactory::new(inputs_fn))
                    as Arc<dyn crate::ports::orchestration::ChatServiceFactory>
            });
        if orchestrator_factory.is_some() {
            info!("Telegram orchestration on: one orchestrator per sender, built on first message");
        }

        let session = TelegramSession {
            pipe,
            turn_cancel,
            current_recipient,
            config,
            memory_config,
            allowed_users,
            is_multi_agent,
            base_workspaces,
            delivery_opts: DeliveryOptions::default(),
            agent_states,
            default_agent_id,
            role_to_agent,
            memory_manager_handle: if has_memory {
                Some(Arc::clone(&memory_manager))
            } else {
                None
            },
            secret_registry,
            orchestrator_factory,
            orchestrators: HashMap::new(),
            orchestrator_snapshots,
            trace: None,
            _memory_manager: memory_manager,
            user_states: HashMap::new(),
            user_state_last_active: HashMap::new(),
            last_eviction_check: std::time::Instant::now(),
            user_active_agent: HashMap::new(),
            skill_command_router,
            activity_log: Vec::new(),
        };

        Ok((session, inbound_rx))
    }

    // -------------------------------------------------------------------
    // Main loop
    // -------------------------------------------------------------------

    fn run(
        mut self,
        rt: tokio::runtime::Runtime,
        mut inbound_rx: tokio::sync::mpsc::Receiver<InboundMessage>,
    ) -> Result<()> {
        rt.block_on(async {
            let ctrl_c = tokio::signal::ctrl_c();
            tokio::pin!(ctrl_c);

            loop {
                let msg = tokio::select! {
                    msg = inbound_rx.recv() => match msg {
                        Some(m) => m,
                        None => break,
                    },
                    _ = &mut ctrl_c => {
                        info!("Received Ctrl+C, shutting down Telegram bot");
                        break;
                    }
                };
                self.handle_message(msg).await;
            }

            self.pipe.disconnect().await.ok();
        });
        Ok(())
    }

    // -------------------------------------------------------------------
    // Message dispatch
    // -------------------------------------------------------------------

    async fn handle_message(&mut self, msg: InboundMessage) {
        let sender_id = msg.sender.peer_id.clone();

        self.evict_idle_users();

        // Access control — fail closed (`build` refuses an empty list too).
        if !is_authorized(&self.allowed_users, &sender_id) {
            warn!(sender = %sender_id, "Unauthorized Telegram user");
            let _ = self
                .pipe
                .send_text(&msg.sender, "Unauthorized.", &self.delivery_opts)
                .await;
            return;
        }

        if msg.content.starts_with('/') {
            self.handle_slash_command(&msg, &sender_id).await;
        } else {
            self.route_and_chat(msg, &sender_id).await;
        }
    }

    fn evict_idle_users(&mut self) {
        if self.last_eviction_check.elapsed() < EVICTION_SWEEP_INTERVAL {
            return;
        }
        let now = std::time::Instant::now();
        let idle_keys: Vec<String> = self
            .user_state_last_active
            .iter()
            .filter(|(_, &last)| now.duration_since(last) >= IDLE_EVICTION_THRESHOLD)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &idle_keys {
            self.user_states.remove(key);
            self.user_state_last_active.remove(key);
        }
        // A sender with no live state left loses its orchestrator too (state
        // keys are `<sender>:<agent>`).
        let live = &self.user_state_last_active;
        self.orchestrators.retain(|sender, _| {
            let prefix = format!("{sender}:");
            live.keys().any(|k| k.starts_with(&prefix))
        });
        if !idle_keys.is_empty() {
            info!(
                evicted = idle_keys.len(),
                remaining = self.user_states.len(),
                "Evicted idle user states"
            );
        }
        self.last_eviction_check = now;
    }

    // -------------------------------------------------------------------
    // Slash commands
    // -------------------------------------------------------------------

    async fn handle_slash_command(&mut self, msg: &InboundMessage, sender_id: &str) {
        if msg.content == "/stop" || msg.content.starts_with("/stop@") {
            self.turn_cancel.store(true, Ordering::Relaxed);
            let _ = self
                .pipe
                .send_text(&msg.sender, "⏹ Stop requested.", &self.delivery_opts)
                .await;
            return;
        }

        if msg.content.starts_with("/project") {
            let name = msg.content.trim_start_matches("/project").trim();
            let _ = self.handle_project(&msg.sender, name, sender_id).await;
            return;
        }

        if msg.content == "/agents" || msg.content.starts_with("/agents@") {
            let _ = self.handle_agents(&msg.sender).await;
            return;
        }

        let active_aid = self
            .user_active_agent
            .get(sender_id)
            .cloned()
            .unwrap_or_else(|| self.default_agent_id.clone());

        if !self.agent_states.contains_key(&active_aid) {
            return;
        }

        if msg.content == "/reset" || msg.content.starts_with("/reset@") {
            let _ = self.handle_reset(&msg.sender, sender_id).await;
            return;
        }

        if msg.content == "/purge" || msg.content.starts_with("/purge@") {
            let _ = self.handle_purge(&msg.sender, sender_id).await;
            return;
        }

        if msg.content == "/reload" {
            let _ = self.handle_reload(&msg.sender, &active_aid).await;
            return;
        }

        if let SkillCommandMatch::Matched {
            skill_name,
            command: cmd_name,
            args,
        } = self.skill_command_router.route(&msg.content)
        {
            self.user_active_agent
                .insert(sender_id.to_string(), active_aid.clone());
            let _ = self
                .handle_skill_cmd(
                    &msg.sender,
                    sender_id,
                    &active_aid,
                    &skill_name,
                    &cmd_name,
                    &args,
                )
                .await;
            return;
        }

        // Builtin command fallback (/help, /wallet, /status, etc.).
        let state_key = format!("{}:{}", sender_id, active_aid);
        self.user_state_last_active
            .insert(state_key.clone(), std::time::Instant::now());

        let agent = match self.agent_states.get(&active_aid) {
            Some(a) => a,
            None => return,
        };
        let skill_cmds = self.skill_command_router.list();
        let state = self.user_states.entry(state_key).or_insert_with(|| {
            crate::application::chat::service::create_chat_loop_state(&agent.agent_config)
        });
        let cmd_result = handle_chat_command(
            &msg.content,
            state,
            &agent.engine_info,
            &agent.agent_config,
            agent.history_turn_limit,
            agent.compaction_policy,
            &skill_cmds,
        );
        match cmd_result {
            CommandResult::Handled(output) => {
                let reply = output.lines.join("\n");
                let _ = self
                    .pipe
                    .send_text(&msg.sender, &reply, &self.delivery_opts)
                    .await;
            }
            CommandResult::NotHandled => {
                let _ = self
                    .pipe
                    .send_text(
                        &msg.sender,
                        "Unknown command. Type /help or /agents for available commands.",
                        &self.delivery_opts,
                    )
                    .await;
            }
        }
    }

    // -------------------------------------------------------------------
    // Chat routing + execution
    // -------------------------------------------------------------------

    async fn route_and_chat(&mut self, msg: InboundMessage, sender_id: &str) {
        let (routed_role, user_text) = crate::adapters::inbound::channel::parse_agent_routing(
            &msg.content,
            Some(&self.role_to_agent),
        );

        // Resolve target agent.
        let target_agent_id = if let Some(ref role_key) = routed_role {
            match self.role_to_agent.get(role_key) {
                Some(aid) => aid.clone(),
                None => {
                    let mut available: Vec<&str> = self
                        .agent_states
                        .values()
                        .filter(|a| telegram_reachable(&a.agent_config))
                        .filter_map(|a| a.role.as_deref())
                        .collect();
                    available.sort_unstable();
                    let _ = self
                        .pipe
                        .send_text(
                            &msg.sender,
                            &format!(
                                "Unknown agent role: {}\nAvailable: {}",
                                role_key,
                                available.join(", ")
                            ),
                            &self.delivery_opts,
                        )
                        .await;
                    return;
                }
            }
        } else {
            self.default_agent_id.clone()
        };

        self.user_active_agent
            .insert(sender_id.to_string(), target_agent_id.clone());

        self.execute_chat_turn(
            &msg.sender,
            sender_id,
            &target_agent_id,
            &user_text,
            msg.media.as_ref(),
        )
        .await;
    }

    async fn execute_chat_turn(
        &mut self,
        sender: &Recipient,
        sender_id: &str,
        target_agent_id: &str,
        user_text: &str,
        media: Option<&Vec<MediaPayload>>,
    ) {
        let agent = match self.agent_states.get_mut(target_agent_id) {
            Some(a) => a,
            None => return,
        };

        // Hot-reload skills.
        if let Some(ref src) = agent.skill_source {
            if agent.skill_registry.reload(src) {
                agent.current_tools = crate::bootstrap::tools::rebuild_tools(
                    &agent.base_tools,
                    &agent.skill_registry,
                );
                agent.rebuild_bridge_tools();
                agent.current_system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
                    &agent.agent_config,
                    agent.advertise_workspace_tools,
                    &agent.skill_registry,
                    &agent.current_tools,
                );
            }
        }

        // Save attached files.
        let user_content = if let (Some(ref ws), Some(media_items)) = (&agent.workspace, &media) {
            let attachments_dir = ws.join(".tengu-attachments");
            std::fs::create_dir_all(&attachments_dir).ok();

            let mut file_notes = Vec::new();
            for m in *media_items {
                let fname =
                    sanitize_attachment_filename(m.filename.as_deref().unwrap_or("attachment"));
                let path = attachments_dir.join(&fname);
                match std::fs::write(&path, &m.data) {
                    Ok(()) => {
                        info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment");
                        file_notes.push(format!(
                            "[Attached file: {} ({}, {} bytes)]",
                            path.display(),
                            m.mime_type,
                            m.data.len()
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to save Telegram attachment");
                    }
                }
            }

            if file_notes.is_empty() {
                user_text.to_string()
            } else {
                format!("{}\n{}", file_notes.join("\n"), user_text)
            }
        } else {
            user_text.to_string()
        };

        *self.current_recipient.lock().unwrap() = Some(sender.clone());

        // Orchestrator dispatch path. When orchestration is configured we
        // route the user message through `Orchestrator::handle` instead of a
        // single-agent `ChatRuntimeService` turn. The orchestrator's DAG
        // executor spawns each step and calls our factory closure to resolve
        // per-agent inputs via `orchestrator_snapshots`.
        //
        // Orchestrator dispatch:
        //   - default agent message → always through the orchestrator
        //     (when one is configured)
        //   - `@role:`-prefixed message → by default bypasses the
        //     orchestrator (explicit routing = "talk to this agent
        //     directly"), UNLESS `orchestrator.route_explicit_agents = true`
        //     which flips the toggle so the planner sees everything.
        let explicit_route = target_agent_id != self.default_agent_id;
        let route_through_orchestrator = self.orchestrator_factory.is_some()
            && (!explicit_route
                || self
                    .config
                    .orchestrator
                    .as_ref()
                    .map(|c| c.route_explicit_agents)
                    .unwrap_or(false));
        if route_through_orchestrator {
            // Release the mutable borrow on `agent` so we can build snapshots
            // for every agent below.
            let _ = agent;
            self.execute_orchestrator_turn(sender, &user_content).await;
            return;
        }

        let activity_adapter = make_tool_activity_adapter(
            agent
                .agent_config
                .identity
                .name
                .clone()
                .unwrap_or_else(|| agent.agent_id.clone()),
        );
        let current_executor = agent.workspace.as_ref().and_then(|ws| {
            crate::bootstrap::tools::build_tool_executor(
                ws,
                &agent.current_tools,
                &agent.skill_registry,
                &self.memory_manager_handle,
                &self.secret_registry,
                activity_adapter,
                Some(Arc::clone(&self.turn_cancel)),
                Some(&self.memory_config),
                &agent.agent_config,
                &self.config.mcp_servers,
            )
        });
        if let Some(ref exec) = current_executor {
            let extra = exec.additional_tool_defs(&agent.current_tools);
            if !extra.is_empty() {
                agent.current_tools.extend(extra);
            }
        }

        let sanitized_executor = current_executor.map(|e| {
            let inner: std::sync::Arc<dyn ToolExecutor> = Arc::new(e);
            SanitizedToolExecutor::new(inner, Arc::clone(&self.secret_registry))
        });

        let turn_tool_log: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let observer_secrets = Arc::clone(&self.secret_registry);
        let observer_agent_label = agent
            .agent_config
            .identity
            .name
            .clone()
            .unwrap_or_else(|| agent.agent_id.clone());
        let turn_tool_log_ref = Arc::clone(&turn_tool_log);
        let tool_result_observer = move |call: &ToolCall, result: &str| {
            if let Some(entry) = crate::adapters::inbound::channel::format_tool_for_activity(call) {
                turn_tool_log_ref.lock().unwrap().push(entry);
            }
            let redacted = observer_secrets.redact(result);
            let mut end = redacted.len().min(1500);
            while end < redacted.len() && !redacted.is_char_boundary(end) {
                end -= 1;
            }
            let truncated = if redacted.len() > 1500 {
                format!("{}…", &redacted[..end])
            } else {
                redacted
            };
            tracing::debug!(
                agent = %observer_agent_label,
                tool = %call.name,
                result = %truncated,
                "Telegram tool result"
            );
        };

        self.turn_cancel.store(false, Ordering::Relaxed);

        let state_key = format!("{}:{}", sender_id, target_agent_id);
        self.user_state_last_active
            .insert(state_key.clone(), std::time::Instant::now());
        let state = self.user_states.entry(state_key).or_insert_with(|| {
            crate::application::chat::service::create_chat_loop_state(&agent.agent_config)
        });

        if self.is_multi_agent {
            let agent_label = agent
                .agent_config
                .identity
                .name
                .as_deref()
                .unwrap_or(&agent.agent_id);
            let _ = self
                .pipe
                .send_text(sender, &format!("[{}]", agent_label), &self.delivery_opts)
                .await;
        }

        let mut turn_system_prompt = agent.current_system_prompt.clone();
        if self.is_multi_agent && !needs_fresh_history_grounding(&user_content) {
            let activity_ctx = crate::adapters::inbound::channel::build_activity_context(
                &self.activity_log,
                target_agent_id,
            );
            if !activity_ctx.is_empty() {
                turn_system_prompt.push_str(&activity_ctx);
            }
        } else if self.is_multi_agent {
            turn_system_prompt.push_str(
                "\n\n## Grounding Rule\nFor questions about last/latest/most recent work, do not answer from Recent Team Activity. Verify against current workspace files, conversation state, or tool results first.\n",
            );
        }

        let turn_tools = agent.current_tools.clone();
        let chat_runtime = ChatRuntimeService {
            engine: agent.engine.as_ref(),
            agent_id: &agent.agent_id,
            agent_config: &agent.agent_config,
            history_turn_limit: agent.history_turn_limit,
            compaction_policy: agent.compaction_policy,
            system_prompt: turn_system_prompt,
            tools: &turn_tools,
            tool_executor: sanitized_executor.as_ref().map(|e| e as &dyn ToolExecutor),
            memory_manager: self.memory_manager_handle.as_deref(),
            max_recall_entries: self.memory_config.max_recall_entries,
            max_recall_tokens: self.memory_config.max_recall_tokens,
            tool_observer: Some(&tool_result_observer),
            cancel: Some(&self.turn_cancel),
            bridge_tools: if agent.current_bridge_tools.is_empty() {
                None
            } else {
                Some(&agent.current_bridge_tools)
            },
            mcp_servers: &self.config.mcp_servers,
            suppress_grounding_nudge: false,
        };

        let _ = self.pipe.send_chat_action(sender).await;
        let typing_pipe = Arc::clone(&self.pipe);
        let typing_sender = sender.clone();
        let typing_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let _ = typing_pipe.send_chat_action(&typing_sender).await;
            }
        });

        let result = chat_runtime.process_user_text(state, &user_content).await;
        typing_handle.abort();

        // Collect activity info before releasing agent borrow.
        let agent_label_for_activity = agent
            .agent_config
            .identity
            .name
            .clone()
            .unwrap_or_else(|| agent.agent_id.clone());
        let target_agent_id_owned = target_agent_id.to_string();

        match result {
            Ok(result) => {
                if let Some(notice) = result.system_notice {
                    let _ = self
                        .pipe
                        .send_text(sender, &notice, &self.delivery_opts)
                        .await;
                }
                if let Some(ref text) = result.assistant_text {
                    let reply = self.secret_registry.redact(text);
                    for chunk in
                        crate::adapters::inbound::channel::chunk_message(&reply, TELEGRAM_MAX_LEN)
                    {
                        if let Err(e) = self
                            .pipe
                            .send_text(sender, chunk, &self.delivery_opts)
                            .await
                        {
                            error!(error = %e, "Failed to send Telegram reply chunk");
                        }
                    }
                } else if !result.tool_outcomes.is_empty() {
                    let fallback = build_tool_summary_fallback(&result.tool_outcomes);
                    let reply = self.secret_registry.redact(&fallback);
                    let _ = self
                        .pipe
                        .send_text(sender, &reply, &self.delivery_opts)
                        .await;
                } else {
                    warn!("Engine returned empty response — no text, no tool outcomes");
                    let _ = self
                        .pipe
                        .send_text(sender, "(Engine returned no response)", &self.delivery_opts)
                        .await;
                }

                if self.is_multi_agent {
                    let tools_used = match Arc::try_unwrap(turn_tool_log) {
                        Ok(mutex) => mutex.into_inner().unwrap_or_default(),
                        Err(arc) => arc.lock().unwrap().clone(),
                    };
                    let response_summary = result
                        .assistant_text
                        .as_deref()
                        .map(|t| {
                            crate::adapters::inbound::channel::truncate_summary(
                                t,
                                crate::adapters::inbound::channel::MAX_ACTIVITY_SUMMARY_CHARS,
                            )
                        })
                        .unwrap_or_default();
                    if !tools_used.is_empty() || !response_summary.is_empty() {
                        self.activity_log
                            .push(crate::adapters::inbound::channel::ActivityEntry {
                                agent_label: agent_label_for_activity,
                                agent_id: target_agent_id_owned,
                                tools_used,
                                response_summary,
                                tool_outcomes: result.tool_outcomes,
                            });
                        if self.activity_log.len()
                            > crate::adapters::inbound::channel::MAX_ACTIVITY_ENTRIES
                        {
                            self.activity_log.drain(
                                ..self.activity_log.len()
                                    - crate::adapters::inbound::channel::MAX_ACTIVITY_ENTRIES,
                            );
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Engine error");
                let _ = self
                    .pipe
                    .send_text(sender, &format!("Error: {}", e), &self.delivery_opts)
                    .await;
            }
        }
    }

    // -------------------------------------------------------------------
    // Orchestrator dispatch (per-message snapshot factory)
    // -------------------------------------------------------------------

    /// Populate `orchestrator_snapshots` with fresh per-agent inputs and
    /// dispatch `user_content` through `Orchestrator::handle`. Hot-reload is
    /// performed for every agent here so the planner can delegate to agents
    /// other than the initial target without stale skill/tool state.
    /// `sender`'s orchestrator, built on first use; `None` when no
    /// `[orchestrator]` is configured. Each sender gets its own session id
    /// (`telegram_session_id`), so planner messages and recall stay per
    /// sender. Its bus events are logged at `info`.
    fn orchestrator_for(
        &mut self,
        sender: &str,
    ) -> Option<Arc<crate::application::orchestrator::Orchestrator>> {
        if let Some(o) = self.orchestrators.get(sender) {
            return Some(Arc::clone(o));
        }
        let factory = Arc::clone(self.orchestrator_factory.as_ref()?);
        let session_id = telegram_session_id(sender);
        let orch = Arc::new(crate::bootstrap::orchestrator::build_orchestrator(
            &self.config,
            factory,
            Arc::clone(&self._memory_manager),
            session_id.clone(),
            self.trace.as_ref().and_then(|s| {
                crate::application::orchestrator::trace::OrchestratorTrace::of(s, None)
            }),
        )?);
        info!(sender, session_id = %session_id, "Telegram orchestrator built for sender");
        log_orchestrator_events(&orch);
        self.orchestrators
            .insert(sender.to_string(), Arc::clone(&orch));
        Some(orch)
    }

    async fn execute_orchestrator_turn(&mut self, sender: &Recipient, user_content: &str) {
        let orchestrator = match self.orchestrator_for(&sender.peer_id) {
            Some(o) => o,
            None => return,
        };

        // Hot-reload every agent's skill registry so snapshots capture the
        // latest tools / system prompt.
        for agent in self.agent_states.values_mut() {
            if let Some(ref src) = agent.skill_source {
                if agent.skill_registry.reload(src) {
                    agent.current_tools = crate::bootstrap::tools::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                    );
                    agent.rebuild_bridge_tools();
                    agent.current_system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
                    );
                }
            }
        }

        self.turn_cancel.store(false, Ordering::Relaxed);

        // Build a snapshot for every agent. Snapshots own all the inputs the
        // factory closure returns — so once the map is populated, the
        // orchestrator's DAG executor can spawn tasks that look up any agent
        // by name without borrowing from `self`.
        let mut snapshot_map: HashMap<String, crate::bootstrap::orchestrator::ChatTurnInputs> =
            HashMap::new();
        for (agent_id, agent) in &self.agent_states {
            let activity_adapter = make_tool_activity_adapter(
                agent
                    .agent_config
                    .identity
                    .name
                    .clone()
                    .unwrap_or_else(|| agent.agent_id.clone()),
            );
            let current_executor: Option<Arc<dyn ToolExecutor>> =
                agent.workspace.as_ref().and_then(|ws| {
                    crate::bootstrap::tools::build_tool_executor(
                        ws,
                        &agent.current_tools,
                        &agent.skill_registry,
                        &self.memory_manager_handle,
                        &self.secret_registry,
                        activity_adapter,
                        Some(Arc::clone(&self.turn_cancel)),
                        Some(&self.memory_config),
                        &agent.agent_config,
                        &self.config.mcp_servers,
                    )
                    .map(|e| {
                        let inner: Arc<dyn ToolExecutor> = Arc::new(e);
                        let sanitized: Arc<dyn ToolExecutor> = Arc::new(
                            SanitizedToolExecutor::new(inner, Arc::clone(&self.secret_registry)),
                        );
                        sanitized
                    })
                });

            let bridge_tools = if agent.current_bridge_tools.is_empty() {
                None
            } else {
                Some(agent.current_bridge_tools.clone())
            };

            // Cross-agent Recent Team Activity — preserved on the
            // orchestrator path (addresses limitation #6). Each snapshot
            // gets its own agent-scoped view (build_activity_context
            // filters out the target agent's own entries).
            let mut snapshot_system_prompt = agent.current_system_prompt.clone();
            if self.is_multi_agent {
                let activity_ctx = crate::adapters::inbound::channel::build_activity_context(
                    &self.activity_log,
                    agent_id,
                );
                if !activity_ctx.is_empty() {
                    snapshot_system_prompt.push_str(&activity_ctx);
                }
            }

            let inputs = crate::bootstrap::orchestrator::ChatTurnInputs {
                engine: Arc::clone(&agent.engine),
                agent_id: agent.agent_id.clone(),
                agent_config: Arc::new(agent.agent_config.clone()),
                history_turn_limit: agent.history_turn_limit,
                compaction_policy: agent.compaction_policy,
                system_prompt: snapshot_system_prompt,
                tools: agent.current_tools.clone(),
                tool_executor: current_executor,
                memory_manager: self.memory_manager_handle.clone(),
                max_recall_entries: self.memory_config.max_recall_entries,
                max_recall_tokens: self.memory_config.max_recall_tokens,
                bridge_tools,
                mcp_servers: self.config.mcp_servers.clone(),
                tool_observer: None,
                cancel: Some(Arc::clone(&self.turn_cancel)),
            };
            snapshot_map.insert(agent_id.clone(), inputs);
        }

        // Publish snapshots atomically.
        {
            let mut guard = match self.orchestrator_snapshots.write() {
                Ok(g) => g,
                Err(e) => {
                    error!(error = %e, "orchestrator snapshots lock poisoned");
                    let _ = self
                        .pipe
                        .send_text(
                            sender,
                            "Internal error: orchestrator state unavailable.",
                            &self.delivery_opts,
                        )
                        .await;
                    return;
                }
            };
            *guard = snapshot_map;
        }

        let _ = self.pipe.send_chat_action(sender).await;
        let typing_pipe = Arc::clone(&self.pipe);
        let typing_sender = sender.clone();
        let typing_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let _ = typing_pipe.send_chat_action(&typing_sender).await;
            }
        });

        let final_output = orchestrator.handle(user_content.to_string()).await;
        typing_handle.abort();

        let reply = self.secret_registry.redact(&final_output);
        if reply.trim().is_empty() {
            let _ = self
                .pipe
                .send_text(
                    sender,
                    "(Orchestrator returned no response)",
                    &self.delivery_opts,
                )
                .await;
            return;
        }
        for chunk in crate::adapters::inbound::channel::chunk_message(&reply, TELEGRAM_MAX_LEN) {
            if let Err(e) = self
                .pipe
                .send_text(sender, chunk, &self.delivery_opts)
                .await
            {
                error!(error = %e, "Failed to send orchestrator reply chunk");
            }
        }
    }

    // -------------------------------------------------------------------
    // /project — switch workspace to a named project subfolder
    // -------------------------------------------------------------------

    async fn handle_project(
        &mut self,
        sender: &Recipient,
        name: &str,
        sender_id: &str,
    ) -> Result<()> {
        if name.is_empty() {
            let current = self
                .agent_states
                .get(&self.default_agent_id)
                .and_then(|a| a.workspace.as_ref())
                .map(|w| w.display().to_string())
                .unwrap_or_else(|| "(no workspace)".into());
            self.pipe
                .send_text(
                    sender,
                    &format!("Current workspace: {}\n\nUsage: /project <name>", current),
                    &self.delivery_opts,
                )
                .await?;
            return Ok(());
        }

        let sanitized: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        let mut created = false;

        if let Some(base) = self.base_workspaces.values().next() {
            let project_dir = base.join(&sanitized);
            let project_template = self
                .config
                .scaffold
                .as_ref()
                .and_then(|s| s.project.as_ref());
            let project_scaffold = crate::config::ScaffoldConfig {
                root: project_dir.to_string_lossy().to_string(),
                directories: project_template
                    .map(|p| p.directories.clone())
                    .unwrap_or_default(),
                files: project_template
                    .map(|p| p.files.clone())
                    .unwrap_or_default(),
                project: None,
            };
            if let Err(e) = crate::adapters::outbound::scaffold::apply_scaffold(&project_scaffold) {
                self.pipe
                    .send_text(
                        sender,
                        &format!("Scaffold failed: {}", e),
                        &self.delivery_opts,
                    )
                    .await?;
                return Ok(());
            }
        }

        for (aid, agent) in self.agent_states.iter_mut() {
            if let Some(base) = self.base_workspaces.get(aid) {
                let project_dir = base.join(&sanitized);
                agent.workspace = Some(project_dir);
                agent.skill_source = agent
                    .workspace
                    .as_ref()
                    .map(|ws| FileSystemSkillSource::new(ws.clone()));
                if let Some(ref src) = agent.skill_source {
                    agent.skill_registry.reload(src);
                }
                agent.current_tools = crate::bootstrap::tools::rebuild_tools(
                    &agent.base_tools,
                    &agent.skill_registry,
                );
                agent.rebuild_bridge_tools();
                created = true;
            }
        }

        if created {
            let prefix = format!("{}:", sender_id);
            for (key, state) in self.user_states.iter_mut() {
                if key.starts_with(&prefix) {
                    state.reset_for_new_session();
                }
            }
            let ws_display = self
                .agent_states
                .get(&self.default_agent_id)
                .and_then(|a| a.workspace.as_ref())
                .map(|w| w.display().to_string())
                .unwrap_or_default();
            self.pipe
                .send_text(
                    sender,
                    &format!(
                        "Project '{}' created.\nWorkspace: {}\nConversations reset.",
                        sanitized, ws_display
                    ),
                    &self.delivery_opts,
                )
                .await?;
        } else {
            self.pipe
                .send_text(sender, "No workspaces configured.", &self.delivery_opts)
                .await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // /agents — list available agents
    // -------------------------------------------------------------------

    async fn handle_agents(&self, sender: &Recipient) -> Result<()> {
        let mut lines = vec!["Available agents:".to_string()];
        let mut reachable: Vec<(&String, &TelegramAgentState)> = self
            .agent_states
            .iter()
            .filter(|(_, a)| telegram_reachable(&a.agent_config))
            .collect();
        reachable.sort_by(|a, b| a.0.cmp(b.0));
        for (aid, astate) in reachable {
            let role_label = astate.role.as_deref().unwrap_or("-");
            let is_default = if *aid == self.default_agent_id {
                " [default]"
            } else {
                ""
            };
            let tool_count = astate.current_tools.len();
            lines.push(format!(
                "  {} (role: {}, {} tools){}",
                aid, role_label, tool_count, is_default
            ));
        }
        lines.push(String::new());
        lines.push("Direct: @role: message  or  role: message".to_string());
        lines.push("Auto:    plain messages  (plan & execute across agents)".to_string());
        lines.push("Project: /project <name>  (new project subfolder)".to_string());
        lines.push("Example: @backend_engineer: add rate limiting".to_string());
        lines.push("Example: summarize this paper and save the notes".to_string());
        self.pipe
            .send_text(sender, &lines.join("\n"), &self.delivery_opts)
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // /reset — clear conversation state
    // -------------------------------------------------------------------

    async fn handle_reset(&mut self, sender: &Recipient, sender_id: &str) -> Result<()> {
        let prefix = format!("{}:", sender_id);
        let mut reset_count = 0usize;
        for (key, state) in self.user_states.iter_mut() {
            if key.starts_with(&prefix) {
                state.reset_for_new_session();
                reset_count += 1;
            }
        }
        self.pipe
            .send_text(
                sender,
                &format!(
                    "Session reset — {} agent conversation(s) cleared.",
                    reset_count
                ),
                &self.delivery_opts,
            )
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // /purge — deep clear: conversations + memory + artifacts
    // -------------------------------------------------------------------

    async fn handle_purge(&mut self, sender: &Recipient, sender_id: &str) -> Result<()> {
        let prefix = format!("{}:", sender_id);
        for (key, state) in self.user_states.iter_mut() {
            if key.starts_with(&prefix) {
                state.reset_for_new_session();
            }
        }
        let mut lines = vec!["All conversations cleared.".to_string()];
        if let Some(ref mgr) = self.memory_manager_handle {
            match mgr.clear_all().await {
                Ok(()) => lines.push("Persistent memory cleared.".to_string()),
                Err(e) => lines.push(format!("Memory clear failed: {}", e)),
            }
        } else {
            lines.push("No persistent memory active.".to_string());
        }

        let ws_set: HashSet<std::path::PathBuf> = self
            .agent_states
            .values()
            .filter_map(|s| s.workspace.clone())
            .collect();
        for ws in &ws_set {
            for subdir in &[".tengu-tasks", ".tengu-attachments"] {
                let p = ws.join(subdir);
                if p.exists() {
                    match std::fs::remove_dir_all(&p) {
                        Ok(()) => lines.push(format!("Cleaned {}", p.display())),
                        Err(e) => lines.push(format!("Failed to clean {}: {}", p.display(), e)),
                    }
                }
            }
        }

        self.pipe
            .send_text(sender, &lines.join("\n"), &self.delivery_opts)
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // /reload — hot-reload skills
    // -------------------------------------------------------------------

    async fn handle_reload(&mut self, sender: &Recipient, active_aid: &str) -> Result<()> {
        let agent = match self.agent_states.get_mut(active_aid) {
            Some(a) => a,
            None => return Ok(()),
        };
        let mut lines = Vec::new();
        if let Some(ref src) = agent.skill_source {
            if agent.skill_registry.reload(src) {
                lines.push("Skills reloaded (changes detected).".to_string());
            } else {
                lines.push("Skills reloaded (no changes).".to_string());
            }
        } else {
            lines.push("No workspace — skills unavailable.".to_string());
        }
        agent.current_tools =
            crate::bootstrap::tools::rebuild_tools(&agent.base_tools, &agent.skill_registry);
        agent.rebuild_bridge_tools();
        agent.current_system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
            &agent.agent_config,
            agent.advertise_workspace_tools,
            &agent.skill_registry,
            &agent.current_tools,
        );
        self.skill_command_router = SkillCommandRouter::from_registry(&agent.skill_registry);
        lines.push(format!("{} tool(s) active.", agent.current_tools.len()));
        self.pipe
            .send_text(sender, &lines.join("\n"), &self.delivery_opts)
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // Skill slash-commands (e.g. /summarize, /translate)
    // -------------------------------------------------------------------

    async fn handle_skill_cmd(
        &mut self,
        sender: &Recipient,
        sender_id: &str,
        active_aid: &str,
        skill_name: &str,
        cmd_name: &str,
        args: &str,
    ) -> Result<()> {
        let injected = format!(
            "[System: User invoked /{cmd} {args}. Follow the instructions in the ## Commands section of the {skill} skill.]",
            cmd = cmd_name,
            args = args,
            skill = skill_name,
        );

        let agent = match self.agent_states.get_mut(active_aid) {
            Some(a) => a,
            None => return Ok(()),
        };

        if let Some(ref src) = agent.skill_source {
            if agent.skill_registry.reload(src) {
                agent.current_tools = crate::bootstrap::tools::rebuild_tools(
                    &agent.base_tools,
                    &agent.skill_registry,
                );
                agent.rebuild_bridge_tools();
                agent.current_system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
                    &agent.agent_config,
                    agent.advertise_workspace_tools,
                    &agent.skill_registry,
                    &agent.current_tools,
                );
                self.skill_command_router =
                    SkillCommandRouter::from_registry(&agent.skill_registry);
            }
        }

        *self.current_recipient.lock().unwrap() = Some(sender.clone());

        let agent_label = agent
            .agent_config
            .identity
            .name
            .clone()
            .unwrap_or_else(|| agent.agent_id.clone());
        let activity_adapter = make_tool_activity_adapter(agent_label);
        let current_executor = agent.workspace.as_ref().and_then(|ws| {
            crate::bootstrap::tools::build_tool_executor(
                ws,
                &agent.current_tools,
                &agent.skill_registry,
                &self.memory_manager_handle,
                &self.secret_registry,
                activity_adapter,
                Some(Arc::clone(&self.turn_cancel)),
                Some(&self.memory_config),
                &agent.agent_config,
                &self.config.mcp_servers,
            )
        });
        if let Some(ref exec) = current_executor {
            let extra = exec.additional_tool_defs(&agent.current_tools);
            if !extra.is_empty() {
                agent.current_tools.extend(extra);
            }
        }
        let sanitized_executor = current_executor.map(|e| {
            let inner: std::sync::Arc<dyn ToolExecutor> = Arc::new(e);
            SanitizedToolExecutor::new(inner, Arc::clone(&self.secret_registry))
        });

        self.turn_cancel.store(false, Ordering::Relaxed);

        let state_key = format!("{}:{}", sender_id, active_aid);
        self.user_state_last_active
            .insert(state_key.clone(), std::time::Instant::now());
        let state = self.user_states.entry(state_key).or_insert_with(|| {
            crate::application::chat::service::create_chat_loop_state(&agent.agent_config)
        });

        let _ = self.pipe.send_chat_action(sender).await;
        let t_pipe = Arc::clone(&self.pipe);
        let t_sender = sender.clone();
        let typing_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let _ = t_pipe.send_chat_action(&t_sender).await;
            }
        });

        let active_tools = agent.current_tools.clone();
        let chat_runtime = ChatRuntimeService {
            engine: agent.engine.as_ref(),
            agent_id: &agent.agent_id,
            agent_config: &agent.agent_config,
            history_turn_limit: agent.history_turn_limit,
            compaction_policy: agent.compaction_policy,
            system_prompt: agent.current_system_prompt.clone(),
            tools: &active_tools,
            tool_executor: sanitized_executor.as_ref().map(|e| e as &dyn ToolExecutor),
            memory_manager: self.memory_manager_handle.as_deref(),
            max_recall_entries: self.memory_config.max_recall_entries,
            max_recall_tokens: self.memory_config.max_recall_tokens,
            tool_observer: None,
            cancel: Some(&self.turn_cancel),
            bridge_tools: if agent.current_bridge_tools.is_empty() {
                None
            } else {
                Some(&agent.current_bridge_tools)
            },
            mcp_servers: &self.config.mcp_servers,
            suppress_grounding_nudge: false,
        };

        let result = chat_runtime.process_user_text(state, &injected).await;
        typing_handle.abort();

        match result {
            Ok(res) => {
                if let Some(notice) = res.system_notice {
                    let _ = self
                        .pipe
                        .send_text(sender, &notice, &self.delivery_opts)
                        .await;
                }
                if let Some(ref text) = res.assistant_text {
                    let reply = self.secret_registry.redact(text);
                    for chunk in
                        crate::adapters::inbound::channel::chunk_message(&reply, TELEGRAM_MAX_LEN)
                    {
                        let _ = self
                            .pipe
                            .send_text(sender, chunk, &self.delivery_opts)
                            .await;
                    }
                } else if !res.tool_outcomes.is_empty() {
                    let fallback = build_tool_summary_fallback(&res.tool_outcomes);
                    let _ = self
                        .pipe
                        .send_text(sender, &fallback, &self.delivery_opts)
                        .await;
                }
            }
            Err(e) => {
                let _ = self
                    .pipe
                    .send_text(sender, &format!("Error: {}", e), &self.delivery_opts)
                    .await;
            }
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<&'a str> {
        let mut keys: Vec<&str> = keys.map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn allow_list_merges_config_and_env_and_drops_blanks() {
        let set = allowed_users(&["848344935".into(), " ".into()], Some(" 1, ,2 "));
        assert_eq!(sorted(set.iter()), ["1", "2", "848344935"]);
        assert!(allowed_users(&[], None).is_empty());
        assert!(allowed_users(&["".into()], Some(" , ")).is_empty());
    }

    /// W1-gate review: an empty allow-list (config + env) refuses to start
    /// and answers no one; a listed sender is answered, anyone else is not.
    #[test]
    fn empty_allow_list_fails_closed() {
        let none = HashSet::new();
        let err = require_allowed_users(&none).unwrap_err().to_string();
        assert!(
            err.contains("no allowed users — refusing to start")
                && err.contains("TENGU_TELEGRAM_ALLOWED_USERS"),
            "{err}"
        );
        assert!(!is_authorized(&none, "848344935"));
        let one = allowed_users(&["848344935".into()], None);
        require_allowed_users(&one).unwrap();
        assert!(is_authorized(&one, "848344935"));
        assert!(!is_authorized(&one, "1"));
    }

    fn agents(toml: &str) -> HashMap<String, crate::config::AgentConfig> {
        toml::from_str::<Config>(toml).expect("config").agents
    }

    fn view(
        agents: &HashMap<String, crate::config::AgentConfig>,
    ) -> impl Iterator<Item = (&str, &crate::config::AgentConfig)> {
        agents.iter().map(|(id, a)| (id.as_str(), a))
    }

    /// Private agents (no `description`, not `default` — the exec-tool
    /// owners) are never `@`-routable from Telegram nor its default; the
    /// default and the described agents are.
    #[test]
    fn private_agents_are_never_routable_or_default() {
        let xm = agents(
            r#"
            [agents.xm]
            default = true
            engine = "openrouter"
            model = "m"
            [agents.xm_architect]
            engine = "openrouter"
            model = "m"
            role = "Architect"
            description = "research"
            [agents.xm_executor]
            engine = "openrouter"
            model = "m"
            role = "executor"
            "#,
        );
        let routes = telegram_routes(view(&xm));
        assert_eq!(sorted(routes.keys()), ["architect", "xm", "xm_architect"]);
        assert_eq!(routes["architect"], "xm_architect");
        assert_eq!(telegram_default_agent(view(&xm)).as_deref(), Some("xm"));

        // No `default`: the first described agent, never a private one
        // (even one that sorts first).
        let no_default = agents(
            r#"
            [agents.a_exec]
            engine = "openrouter"
            model = "m"
            [agents.b_research]
            engine = "openrouter"
            model = "m"
            description = "research"
            "#,
        );
        assert_eq!(
            telegram_default_agent(view(&no_default)).as_deref(),
            Some("b_research")
        );
        assert_eq!(
            sorted(telegram_routes(view(&no_default)).keys()),
            ["b_research"]
        );

        // Only private agents (sandboxes/xmarket-weekend): nothing to reach.
        let private = agents("[agents.xm_weekend]\nengine = \"openrouter\"\nmodel = \"m\"\n");
        assert_eq!(telegram_default_agent(view(&private)), None);
        assert!(telegram_routes(view(&private)).is_empty());

        // Ids with `-` or capitals: reached by what the user types, which
        // `parse_agent_routing` lowercases and turns `-` into `_`.
        let dashed = agents(
            "[agents.Lp-exec]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"lp\"\n",
        );
        let routes = telegram_routes(view(&dashed));
        let (role, msg) = crate::adapters::inbound::channel::parse_agent_routing(
            "@Lp-exec: open 0.5 SOL",
            Some(&routes),
        );
        assert_eq!(routes[role.as_deref().unwrap()], "Lp-exec");
        assert_eq!(msg, "open 0.5 SOL");
        let (implicit, _) =
            crate::adapters::inbound::channel::parse_agent_routing("lp-exec: hi", Some(&routes));
        assert_eq!(implicit.as_deref(), Some("lp_exec"));
    }

    /// Two senders never share a planner session: an override gets the sender
    /// appended, no override gives each sender its own UUID.
    #[test]
    fn each_sender_gets_its_own_session_id() {
        assert_eq!(session_id_for_sender(Some("ops".into()), "111"), "ops-111");
        assert_ne!(
            session_id_for_sender(Some("ops".into()), "111"),
            session_id_for_sender(Some("ops".into()), "222")
        );
        let (a, b) = (
            session_id_for_sender(None, "111"),
            session_id_for_sender(Some("  ".into()), "222"),
        );
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "a UUID: {a}");
    }
}
