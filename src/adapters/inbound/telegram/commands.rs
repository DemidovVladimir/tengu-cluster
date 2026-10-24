//! Slash commands: `/stop`, `/project`, `/agents`, `/reset`, `/purge`,
//! `/reload`, a skill's command (`handle_skill_cmd`) and the chat commands
//! `handle_chat_command` serves.

use super::*;

impl TelegramSession {
    // -------------------------------------------------------------------
    // Slash commands
    // -------------------------------------------------------------------

    pub(super) async fn handle_slash_command(&mut self, msg: &InboundMessage, sender_id: &str) {
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
        let state = self
            .user_states
            .entry(state_key)
            .or_insert_with(crate::application::chat::service::create_chat_loop_state);
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
    // /project — switch workspace to a named project subfolder
    // -------------------------------------------------------------------

    pub(super) async fn handle_project(
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

    pub(super) async fn handle_agents(&self, sender: &Recipient) -> Result<()> {
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

    pub(super) async fn handle_reset(&mut self, sender: &Recipient, sender_id: &str) -> Result<()> {
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

    pub(super) async fn handle_purge(&mut self, sender: &Recipient, sender_id: &str) -> Result<()> {
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
            let p = crate::config::paths::WS_ATTACHMENTS.open(ws);
            if p.exists() {
                match std::fs::remove_dir_all(&p) {
                    Ok(()) => lines.push(format!("Cleaned {}", p.display())),
                    Err(e) => lines.push(format!("Failed to clean {}: {}", p.display(), e)),
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

    pub(super) async fn handle_reload(
        &mut self,
        sender: &Recipient,
        active_aid: &str,
    ) -> Result<()> {
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
            &agent.skill_registry,
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

    pub(super) async fn handle_skill_cmd(
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
                    &agent.skill_registry,
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
        let stack = agent.workspace.as_ref().and_then(|ws| {
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
        // The stack advertises the `[[mcp_servers]]` tools found on connect.
        if let Some(stack) = &stack {
            agent.current_tools = stack.tools.clone();
        }
        let sanitized_executor = stack.map(|s| s.executor);

        self.turn_cancel.store(false, Ordering::Relaxed);

        let state_key = format!("{}:{}", sender_id, active_aid);
        self.user_state_last_active
            .insert(state_key.clone(), std::time::Instant::now());
        let state = self
            .user_states
            .entry(state_key)
            .or_insert_with(crate::application::chat::service::create_chat_loop_state);

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
            tool_executor: sanitized_executor.as_deref(),
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
