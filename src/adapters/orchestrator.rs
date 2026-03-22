//! Fleet orchestrator: LLM-driven subagent spawning (OpenClaw-compatible).
//!
//! Instead of a central planner with Plan/DAG/EventBus, the main agent decides
//! when to spawn subagents via `sessions_spawn` and `sessions_fan_out` tools.
//! The orchestrator boots all agents, creates a SubagentToolExecutor, gives it
//! to the main/default agent, and runs an interactive chat loop.

use crate::adapters::channel_runtime;
use crate::adapters::engine_builder::{build_engine, ToolExecutor};
use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::skill_builder::{FileSystemSkillSource, SkillRegistry};
use crate::adapters::ports::{ToolActivityPort, ToolApprovalPort};
use crate::adapters::approval::DenyByDefaultApproval;
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::subagent_builder::{
    AgentRuntime, SubagentInfo, SubagentToolExecutor,
};
use crate::adapters::engine_builder::{
    collect_engine_response, SanitizedToolExecutor,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use crate::adapters::config::Config;
use crate::adapters::types::{
    AgentRole, Message, Role, ToolCall, ToolDef,
};
use crate::adapters::EngineContext;
use tracing::info;

/// No-op tool activity adapter for fleet agents — logs are sufficient.
struct LogToolActivity;

impl ToolActivityPort for LogToolActivity {
    fn publish_tool_activity(&self, call: &ToolCall) {
        tracing::debug!(tool = %call.name, "Fleet agent tool call");
    }
}

struct NoopRuntimeToolExecutor;

impl ToolExecutor for NoopRuntimeToolExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        anyhow::bail!("No tools available (agent has no workspace): {}", call.name)
    }
}

/// Boot the orchestrator: build agents, create SubagentToolExecutor, run main agent chat.
///
/// The main agent gets sessions_spawn/sessions_fan_out/subagents tools and decides
/// autonomously when to delegate work to other agents.
pub(crate) async fn boot_orchestrator(
    config: &Config,
    secret_registry: Arc<SecretRegistry>,
) -> Result<()> {
    let orch_config = config.orchestrator.clone().unwrap_or_default();

    if !orch_config.enabled {
        info!("Orchestrator disabled in config, skipping boot");
        return Ok(());
    }

    // Apply workspace scaffold if configured.
    crate::adapters::scaffold::maybe_apply_scaffold(config);

    // Find first agent's workspace for per-sandbox memory scoping.
    let first_workspace: Option<std::path::PathBuf> = config.agents.values().find_map(|ac| {
        ac.workspace
            .as_ref()
            .map(|p| crate::adapters::tool_builder::expand_tilde(p))
    });

    // Build shared memory handle.
    let memory_handle: Option<Arc<MemoryServiceHandle>> = tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("memory init runtime");
        channel_runtime::build_memory_handle(&config.memory, &rt, first_workspace.as_deref())
    });

    let mut worker_runtimes: HashMap<String, Arc<AgentRuntime>> = HashMap::new();
    let mut role_to_agent: HashMap<String, String> = HashMap::new();
    let mut agent_info: Vec<SubagentInfo> = Vec::new();
    let mut main_agent_id: Option<String> = None;
    let mut agent_list: Vec<(String, String, String)> = Vec::new();

    let session_registry = Arc::new(channel_runtime::SessionRegistry::new());
    let deny_approval: Arc<dyn ToolApprovalPort> = Arc::new(DenyByDefaultApproval);
    let log_activity: Arc<dyn ToolActivityPort> = Arc::new(LogToolActivity);

    let shared_http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .ok();

    // Build worker agent runtimes (all agents except the main/default one).
    for (agent_id, agent_config) in &config.agents {
        let role_str = match &agent_config.role {
            Some(r) => r,
            None => continue,
        };
        let role: AgentRole = match role_str.parse() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, error = %e, "Skipping agent with invalid role");
                continue;
            }
        };

        let engine = match build_engine(agent_id, agent_config) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, error = %e, "Failed to build engine, skipping");
                continue;
            }
        };

        let workspace = agent_config
            .workspace
            .as_ref()
            .map(|ws_raw| crate::adapters::tool_builder::expand_tilde(ws_raw));

        // Worker agents (sub-agents) get standard tools minus session/subagent tools.
        // OpenClaw pattern: "sub-agents receive the full tool set minus session tools."
        let (system_prompt_str, tools, tool_executor): (
            String,
            Vec<ToolDef>,
            Arc<dyn ToolExecutor>,
        ) = if let Some(ref ws) = workspace {
            let base_tools = channel_runtime::compute_base_tools_subagent(
                true,
                memory_handle.is_some(),
                &agent_config.workspace_tools,
            );
            let skill_source = FileSystemSkillSource::new(ws.clone());
            let base_reserved: Vec<String> =
                base_tools.iter().map(|t| t.def.name.clone()).collect();
            let mut skill_registry = SkillRegistry::new(base_reserved)
                .with_allowlist(Some(agent_config.skill_packages.clone()));
            skill_registry.reload(&skill_source);

            let current_tools =
                channel_runtime::rebuild_tools(&base_tools, &skill_registry);
            // Apply per-agent tool allow/deny policies (OpenClaw-compatible).
            let current_tools = channel_runtime::apply_tool_policies(
                current_tools,
                &agent_config.tool_allow,
                &agent_config.tool_deny,
            );
            let tool_defs = channel_runtime::tool_defs(&current_tools);
            // Sub-agents use minimal prompt mode (OpenClaw-compatible).
            let prompt = crate::adapters::skill_builder::build_subagent_system_prompt(
                agent_config,
                &tool_defs,
            );
            // Sub-agents don't get session tools (OpenClaw-compatible).
            let tool_executor = channel_runtime::build_tool_executor(
                ws,
                &current_tools,
                &skill_registry,
                &memory_handle,
                &secret_registry,
                deny_approval.clone(),
                log_activity.clone(),
                None,
                shared_http_client.as_ref(),
            )
            .map(|executor| Arc::new(executor) as Arc<dyn ToolExecutor>)
            .unwrap_or_else(|| Arc::new(NoopRuntimeToolExecutor));

            (prompt, tool_defs, tool_executor)
        } else {
            let prompt =
                crate::adapters::skill_builder::build_subagent_system_prompt(agent_config, &[]);
            (prompt, vec![], Arc::new(NoopRuntimeToolExecutor))
        };

        let tool_count = tools.len();
        role_to_agent.insert(role_str.clone(), agent_id.clone());
        agent_list.push((
            role_str.clone(),
            agent_id.clone(),
            agent_config.engine.clone(),
        ));

        session_registry.register(channel_runtime::SessionInfo {
            agent_id: agent_id.clone(),
            agent_name: agent_config
                .identity
                .name
                .clone()
                .unwrap_or_else(|| agent_id.clone()),
            role: Some(role_str.clone()),
            model: agent_config.model.clone(),
            active: true,
        });

        let identity_name = agent_config
            .identity
            .name
            .clone()
            .unwrap_or_else(|| agent_id.clone());
        let instructions = agent_config
            .identity
            .instructions
            .as_deref()
            .unwrap_or("AI assistant");
        let truncated = if instructions.len() > 200 {
            let mut end = 200;
            while end > 0 && !instructions.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &instructions[..end])
        } else {
            instructions.to_string()
        };

        agent_info.push(SubagentInfo {
            agent_id: agent_id.clone(),
            name: identity_name,
            role: Some(role_str.clone()),
            model: agent_config.model.clone(),
            instructions_preview: truncated,
        });

        worker_runtimes.insert(
            agent_id.clone(),
            Arc::new(AgentRuntime {
                engine,
                tools,
                tool_executor,
                system_prompt: system_prompt_str,
                workspace,
                task_token_budget: Some(agent_config.limits.max_tokens_per_flow as u32),
                max_tool_rounds: agent_config.limits.max_tool_rounds,
            }),
        );

        info!(
            agent_id = %agent_id,
            role = %role,
            engine = %agent_config.engine,
            tools = tool_count,
            memory = memory_handle.is_some(),
            "Registered fleet agent"
        );

        if main_agent_id.is_none() {
            main_agent_id = Some(agent_id.clone());
        }
    }

    if worker_runtimes.is_empty() {
        anyhow::bail!("No agents with roles configured for orchestration");
    }

    // Build SubagentToolExecutor with all worker runtimes (OpenClaw-compatible).
    let subagent_config = &orch_config.subagents;
    // Push-based announce callback (OpenClaw-compatible): log completions.
    let announce_cb: crate::adapters::subagent_builder::CompletionCallback =
        std::sync::Arc::new(|run_id, agent, output, success| {
            if success {
                let preview = if output.len() > 200 { &output[..200] } else { output };
                info!(
                    run_id = %run_id,
                    agent = %agent,
                    output_preview = %preview,
                    "Subagent completed (push-announce)"
                );
            } else {
                info!(
                    run_id = %run_id,
                    agent = %agent,
                    error = %output,
                    "Subagent failed (push-announce)"
                );
            }
        });
    let subagent_executor = Arc::new(
        SubagentToolExecutor::new(
            worker_runtimes.clone(),
            agent_info.clone(),
            Arc::clone(&secret_registry),
            role_to_agent.clone(),
            subagent_config.max_concurrent as usize,
        )
        .with_depth(0) // Main agent is depth 0.
        .with_max_spawn_depth(subagent_config.max_spawn_depth)
        .with_max_children_per_agent(subagent_config.max_children_per_agent)
        .with_completion_callback(announce_cb),
    );

    // Determine main agent: use the first agent (or one with 'default' flag).
    let default_agent_id = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default && ac.role.is_some())
        .map(|(id, _)| id.clone())
        .or(main_agent_id)
        .expect("No main agent found");

    let main_config = config.agents.get(&default_agent_id).unwrap();

    // Build main agent with subagent tools.
    let main_engine = build_engine(&default_agent_id, main_config)?;
    let main_workspace = main_config
        .workspace
        .as_ref()
        .map(|ws_raw| crate::adapters::tool_builder::expand_tilde(ws_raw));

    let (main_prompt, main_tools, main_tool_executor) = if let Some(ref ws) = main_workspace {
        // Main agent gets subagent tools in addition to standard tools.
        let base_tools = channel_runtime::compute_base_tools_ext(
            true,
            memory_handle.is_some(),
            &main_config.workspace_tools,
            true,  // has_subagents
            true,  // has_session_tools
        );
        let skill_source = FileSystemSkillSource::new(ws.clone());
        let base_reserved: Vec<String> =
            base_tools.iter().map(|t| t.def.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(main_config.skill_packages.clone()));
        skill_registry.reload(&skill_source);

        let current_tools =
            channel_runtime::rebuild_tools(&base_tools, &skill_registry);
        // Apply per-agent tool allow/deny policies (OpenClaw-compatible).
        let current_tools = channel_runtime::apply_tool_policies(
            current_tools,
            &main_config.tool_allow,
            &main_config.tool_deny,
        );

        // Build system prompt with team info.
        let mut prompt = channel_runtime::rebuild_system_prompt(
            main_config,
            true,
            &skill_registry,
            &current_tools,
        );

        // Inject team roster into system prompt (OpenClaw-compatible).
        prompt.push_str("\n\n## Team & Sub-Agents\n\n");
        prompt.push_str("You are the lead agent. You can spawn subagents to delegate work.\n\n");
        prompt.push_str("### Tools\n");
        prompt.push_str("- `agents_list` — discover available agents\n");
        prompt.push_str("- `sessions_spawn` — spawn a subagent (blocking or async)\n");
        prompt.push_str("- `sessions_fan_out` — spawn multiple subagents in parallel\n");
        prompt.push_str("- `subagents` — list, info, result, or kill subagent runs\n\n");
        prompt.push_str("### Spawn modes\n");
        prompt.push_str("- **blocking** (default): waits for subagent to finish, returns full result\n");
        prompt.push_str("- **async** (`blocking: false`): returns immediately with run_id, use `subagents result <id>` to get output later\n\n");
        prompt.push_str("### Available agents\n");
        for info in &agent_info {
            let role = info.role.as_deref().unwrap_or("none");
            prompt.push_str(&format!(
                "- **{}** (role: `{}`, model: `{}`): {}\n",
                info.name, role, info.model, info.instructions_preview
            ));
        }

        let tool_defs = channel_runtime::tool_defs(&current_tools);
        let tool_executor = channel_runtime::build_tool_executor_full(
            ws,
            &current_tools,
            &skill_registry,
            &memory_handle,
            &secret_registry,
            deny_approval.clone(),
            log_activity.clone(),
            None,
            shared_http_client.as_ref(),
            Some(Arc::clone(&session_registry)),
            Some(Arc::clone(&subagent_executor)),
        )
        .map(|executor| Arc::new(executor) as Arc<dyn ToolExecutor>)
        .unwrap_or_else(|| Arc::new(NoopRuntimeToolExecutor));

        (prompt, tool_defs, tool_executor)
    } else {
        let prompt =
            crate::adapters::skill_builder::build_system_prompt(main_config, false, &[]);
        (prompt, vec![], Arc::new(NoopRuntimeToolExecutor) as Arc<dyn ToolExecutor>)
    };

    // Print fleet banner (OpenClaw-compatible).
    println!();
    println!("  TENGU FLEET");
    println!("  ─────────────────────────────────────");
    println!("  Lead: {} ({})", default_agent_id, main_config.engine);
    println!("  Agents: {}", agent_list.len());
    for (role, aid, eid) in &agent_list {
        println!("    [{:>20}]  {} ({})", role, aid, eid);
    }
    println!(
        "  Memory: {}",
        if memory_handle.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Execution: LLM-driven subagent spawning");
    println!(
        "  Subagents: depth={}, concurrent={}, per-agent={}",
        subagent_config.max_spawn_depth,
        subagent_config.max_concurrent,
        subagent_config.max_children_per_agent
    );
    println!("  ─────────────────────────────────────");
    println!();
    println!("  The lead agent decides when to delegate work.");
    println!("  Type your goal and the agent will orchestrate.");
    println!("  /fleet — Show agents | /quit — Exit");
    println!();

    // Stdin reader on a separate thread.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = tx.send(line.trim().to_string());
                }
                Err(_) => break,
            }
        }
    });

    // Interactive chat loop with the main agent.
    let mut messages: Vec<Message> = Vec::new();

    loop {
        print!("> ");
        use std::io::Write;
        std::io::stdout().flush().ok();

        let input = match rx.recv() {
            Ok(line) => line,
            Err(_) => break,
        };

        if input.is_empty() {
            continue;
        }

        match input.as_str() {
            "/quit" | "/exit" => {
                println!("Shutting down fleet.");
                break;
            }
            "/fleet" => {
                println!();
                println!("  Fleet Agents");
                println!("  ─────────────────────────────────────");
                for (role, aid, eid) in &agent_list {
                    println!("    [{:>20}]  {} ({})", role, aid, eid);
                }
                println!("  ─────────────────────────────────────");
                println!();
                continue;
            }
            "/clear" => {
                messages.clear();
                println!("  Conversation cleared.");
                continue;
            }
            _ => {}
        }

        // Add user message.
        messages.push(Message {
            role: Role::User,
            content: input,
            tool_call_id: None,
            tool_calls: None,
        });

        let context = EngineContext {
            workspace: main_workspace.clone(),
            system_prompt: Some(main_prompt.clone()),
        };

        let sanitized = SanitizedToolExecutor::new(
            main_tool_executor.as_ref(),
            &secret_registry,
        );

        let result = collect_engine_response(
            main_engine.as_ref(),
            &messages,
            &main_tools,
            &context,
            Some(&sanitized),
            None,
            None,
            None,
            None,
        )
        .await;

        match result {
            Ok(response) => {
                if !response.text.is_empty() {
                    println!();
                    println!("{}", response.text);
                    println!();
                }
                // Add assistant response to history.
                messages.push(Message {
                    role: Role::Assistant,
                    content: response.text,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            Err(e) => {
                println!("  Error: {}", e);
            }
        }
    }

    Ok(())
}
