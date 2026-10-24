//! `TelegramSession` — the shared state of the message loop — and the
//! per-agent runtime state: build, main loop, dispatch, idle eviction.

use super::*;

// ===========================================================================
// Per-agent runtime state
// ===========================================================================

/// Per-agent runtime state for multi-agent Telegram support.
pub(super) struct TelegramAgentState {
    pub(super) agent_id: String,
    pub(super) agent_config: crate::config::AgentConfig,
    pub(super) engine: Arc<dyn Engine>,
    pub(super) engine_info: EngineInfo,
    pub(super) workspace: Option<std::path::PathBuf>,
    pub(super) base_tools: Vec<ToolDef>,
    pub(super) bridge_base_tools: Vec<ToolDef>,
    pub(super) skill_source: Option<FileSystemSkillSource>,
    pub(super) skill_registry: SkillRegistry,
    pub(super) current_tools: Vec<ToolDef>,
    pub(super) current_bridge_tools: Vec<ToolDef>,
    pub(super) current_system_prompt: String,
    pub(super) history_turn_limit: usize,
    pub(super) compaction_policy: crate::domain::session::FlowCompactionPolicy,
    pub(super) role: Option<String>,
}

impl TelegramAgentState {
    /// Rebuild bridge tools from bridge_base_tools + skill registry.
    pub(super) fn rebuild_bridge_tools(&mut self) {
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

pub(super) struct TelegramSession {
    // Infrastructure
    pub(super) pipe: Arc<TelegramPipe>,
    pub(super) turn_cancel: Arc<AtomicBool>,
    pub(super) current_recipient: CurrentRecipient,

    // Config
    pub(super) config: Config,
    pub(super) memory_config: crate::config::MemoryConfig,
    pub(super) allowed_users: HashSet<String>,
    pub(super) is_multi_agent: bool,
    pub(super) base_workspaces: HashMap<String, std::path::PathBuf>,
    pub(super) delivery_opts: DeliveryOptions,

    // Agents
    pub(super) agent_states: HashMap<String, TelegramAgentState>,
    pub(super) default_agent_id: String,
    pub(super) role_to_agent: HashMap<String, String>,

    // Services
    pub(super) memory_manager_handle:
        Option<Arc<crate::application::memory::manager::MemoryManager>>,
    pub(super) secret_registry: Arc<SecretRegistry>,

    // Harness-owned orchestration. `planner_llm` is `Some` when
    // `[orchestrator]` is configured; `orchestrators` holds one orchestrator
    // per sender id, each with its own session id (`orchestrator_for`),
    // dropped with the sender's idle state (`evict_idle_users`).
    //
    // `_memory_manager` is held so it stays alive for the session.
    pub(super) planner_llm: Option<crate::application::orchestrator::planner::PlannerLlm>,
    pub(super) orchestrators: HashMap<String, Arc<crate::application::orchestrator::Orchestrator>>,
    /// This process's recording of every sender's plans (`RunKind::Telegram`,
    /// `run_telegram`); `None` without `[orchestrator]`.
    pub(super) trace: Option<Arc<dyn crate::ports::trace::TraceSink>>,
    pub(super) _memory_manager: Arc<crate::application::memory::manager::MemoryManager>,

    // Per-user mutable state
    pub(super) user_states: HashMap<String, ChatLoopState>,
    pub(super) user_state_last_active: HashMap<String, std::time::Instant>,
    pub(super) last_eviction_check: std::time::Instant,
    pub(super) user_active_agent: HashMap<String, String>,

    // Routing
    pub(super) skill_command_router: SkillCommandRouter,
    pub(super) activity_log: Vec<crate::adapters::inbound::channel::ActivityEntry>,
}

impl TelegramSession {
    // -------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------

    pub(super) fn build(
        config: Config,
        secret_registry: Arc<SecretRegistry>,
        rt: &tokio::runtime::Runtime,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<InboundMessage>)> {
        // Fail closed: an empty allow-list never means "everyone".
        let allowed_users = build_allowed_users(&config);
        require_allowed_users(&allowed_users)?;
        info!(count = allowed_users.len(), "Telegram allowed users loaded");

        // With `TELEGRAM_API_URL` from `[keys]` the Worker holds the token
        // and `build_bot` sends a placeholder; a local token is ignored. A
        // config that routes Telegram through the proxy never falls back to
        // a local token when the session is missing — it refuses to start.
        let via_proxy = config.keys.enabled() && config.keys.env.contains_key(TELEGRAM_API_URL_ENV);
        let bot_token = match sealed_api_url() {
            Some(_) => crate::adapters::outbound::keys::PLACEHOLDER.to_string(),
            None if via_proxy => anyhow::bail!(
                "[keys] routes Telegram through the seal proxy but no session is set up — see the [keys] warning above; refusing to use a local TELEGRAM_BOT_TOKEN"
            ),
            None => std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| {
                anyhow::anyhow!(
                    "TELEGRAM_BOT_TOKEN env var is required (or route it through the seal proxy: [keys.env] TELEGRAM_API_URL = \"telegram\")"
                )
            })?,
        };

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
            let current_system_prompt =
                crate::bootstrap::tools::rebuild_system_prompt(agent_config, &skill_registry);

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

        let memory_manager = memory_manager_early;
        // One orchestrator per sender, built on first use (`orchestrator_for`):
        // each has its own session id, so one sender's planner messages and
        // recall never reach another's prompt. The planner calls its own
        // engine (`planner_llm`); steps are `run-agent` children.
        let planner_llm = crate::bootstrap::orchestrator::planner_llm(&config)?;
        if planner_llm.is_some() {
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
            planner_llm,
            orchestrators: HashMap::new(),
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

    pub(super) fn run(
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

    pub(super) async fn handle_message(&mut self, msg: InboundMessage) {
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

    pub(super) fn evict_idle_users(&mut self) {
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
}
