//! A message's turn: `@role:` routing, the in-process chat turn
//! (`execute_chat_turn`) and the orchestrator turn (one orchestrator per sender).

use super::*;
use tracing::Instrument;

impl TelegramSession {
    // -------------------------------------------------------------------
    // Chat routing + execution
    // -------------------------------------------------------------------

    pub(super) async fn route_and_chat(&mut self, msg: InboundMessage, sender_id: &str) {
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

    pub(super) async fn execute_chat_turn(
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
                    &agent.skill_registry,
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
        // single-agent `ChatRuntimeService` turn: the planner calls its own
        // engine, each step is a `run-agent` child.
        //
        // Orchestrator dispatch:
        //   - default agent message → always through the orchestrator
        //     (when one is configured)
        //   - `@role:`-prefixed message → by default bypasses the
        //     orchestrator (explicit routing = "talk to this agent
        //     directly"), UNLESS `orchestrator.route_explicit_agents = true`
        //     which flips the toggle so the planner sees everything.
        let explicit_route = target_agent_id != self.default_agent_id;
        let route_through_orchestrator = self.planner_llm.is_some()
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
        let state = self
            .user_states
            .entry(state_key)
            .or_insert_with(crate::application::chat::service::create_chat_loop_state);

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
            tool_executor: sanitized_executor.as_deref(),
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

        let result = chat_runtime
            .process_user_text(state, &user_content)
            .instrument(crate::bootstrap::trace::turn_span(Some(
                &super::telegram_session_id(sender_id),
            )))
            .await;
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
    // Orchestrator dispatch
    // -------------------------------------------------------------------

    /// `sender`'s orchestrator, built on first use; `None` when no
    /// `[orchestrator]` is configured. Each sender gets its own session id
    /// (`telegram_session_id`), so planner messages and recall stay per
    /// sender. Its bus events are logged at `info`.
    pub(super) fn orchestrator_for(
        &mut self,
        sender: &str,
    ) -> Option<Arc<crate::application::orchestrator::Orchestrator>> {
        if let Some(o) = self.orchestrators.get(sender) {
            return Some(Arc::clone(o));
        }
        let llm = self.planner_llm.clone()?;
        let session_id = telegram_session_id(sender);
        let orch = Arc::new(crate::bootstrap::orchestrator::build_orchestrator(
            &self.config,
            llm,
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

    pub(super) async fn execute_orchestrator_turn(
        &mut self,
        sender: &Recipient,
        user_content: &str,
    ) {
        let orchestrator = match self.orchestrator_for(&sender.peer_id) {
            Some(o) => o,
            None => return,
        };

        self.turn_cancel.store(false, Ordering::Relaxed);

        let _ = self.pipe.send_chat_action(sender).await;
        let typing_pipe = Arc::clone(&self.pipe);
        let typing_sender = sender.clone();
        let typing_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let _ = typing_pipe.send_chat_action(&typing_sender).await;
            }
        });

        // `/stop` sets `turn_cancel` from the dispatcher while this turn
        // runs: dropping the turn future aborts the plan's steps and kills
        // their `run-agent` children (`Orchestrator::handle`).
        let final_output = tokio::select! {
            out = orchestrator.handle(user_content.to_string()) => out,
            () = cancel_requested(&self.turn_cancel) => "⏹ Stopped.".to_string(),
        };
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
}
