//! Adapter wiring for fleet orchestrator bootstrap and task dispatch.
//!
//! Uses the event-bus architecture: agents run as persistent workers listening
//! on dedicated channels, the orchestrator dispatches tasks reactively as
//! dependencies are satisfied, and parallelism emerges from the DAG.

use crate::adapters::channel_runtime;
use crate::adapters::engine_builder::build_engine;
use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::skill_builder::{FileSystemSkillSource, SkillRegistry};
use crate::adapters::engine_builder::{
    collect_engine_response, SanitizedToolExecutor, ToolExecutor,
};
use crate::adapters::event_orchestrator::{self, OrchestratorConfig};
use crate::adapters::ports::ToolActivityPort;
use crate::adapters::secret_builder::SecretRegistry;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use crate::adapters::config::Config;
use crate::adapters::types::{
    AgentRole, AgentTaskExecutor, Message, Role, RoleDependencies,
    TaskHistory, TaskStatus, ToolCall, ToolDef,
};
use crate::adapters::{Engine, EngineContext};
use tracing::info;

/// No-op tool activity adapter for fleet agents — logs are sufficient.
struct LogToolActivity;

impl ToolActivityPort for LogToolActivity {
    fn publish_tool_activity(&self, call: &ToolCall) {
        tracing::debug!(tool = %call.name, "Fleet agent tool call");
    }
}

/// Per-agent runtime state: engine, tools, executor, system prompt.
/// Wrapped in Arc for sharing across parallel JoinSet tasks.
struct AgentRuntime {
    engine: Box<dyn Engine>,
    tools: Vec<ToolDef>,
    tool_executor: Arc<dyn ToolExecutor>,
    system_prompt: String,
    workspace: Option<std::path::PathBuf>,
    /// Per-task token budget (input + output). Derived from agent's
    /// `limits.max_tokens_per_flow` — caps runaway orchestrated tasks.
    task_token_budget: Option<u32>,
    max_tool_rounds: u32,
    max_tool_result_chars: u32,
    stream_event_timeout_secs: u64,
    compact_result_limit: u32,
}

/// Adapter implementing `AgentTaskExecutor` for real agent runtimes.
struct AgentRuntimeExecutor {
    runtime: Arc<AgentRuntime>,
    secret_registry: Arc<SecretRegistry>,
}

#[async_trait::async_trait]
impl AgentTaskExecutor for AgentRuntimeExecutor {
    async fn execute(&self, description: &str) -> std::result::Result<(String, Vec<(String, String)>), String> {
        execute_agent_task(&self.runtime, description, &self.secret_registry)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Boot the orchestrator: build agents, run interactive task dispatch.
///
/// Builds per-agent engines, tool executors with shared memory, and an interactive
/// stdin loop for task submission with role-based routing and parallel execution.
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

    // Build shared memory handle (scoped to workspace if available).
    // Uses build_memory_handle for unified backend selection (disk or Qdrant).
    let memory_handle: Option<Arc<MemoryServiceHandle>> = tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("memory init runtime");
        channel_runtime::build_memory_handle(&config.memory, &rt, first_workspace.as_deref())
    });

    let task_history = TaskHistory::new();
    let mut agent_runtimes: HashMap<String, Arc<AgentRuntime>> = HashMap::new();
    let mut role_to_agent: HashMap<String, String> = HashMap::new();
    let mut agent_descriptions: HashMap<String, String> = HashMap::new();
    let mut planner_agent_id: Option<String> = None;
    // (role_key, agent_id, engine_id) for banner display.
    let mut agent_list: Vec<(String, String, String)> = Vec::new();

    let log_activity: Arc<dyn ToolActivityPort> = Arc::new(LogToolActivity);

    // Shared HTTP client across all agents — eliminates redundant connection pools.
    let shared_http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .ok();

    // Register agents with roles, build engines and tool executors.
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

        // Build engine.
        let engine = match build_engine(agent_id, agent_config, config.claude_code.as_ref()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, error = %e, "Failed to build engine, skipping");
                continue;
            }
        };

        // Build tools and system prompt.
        let workspace = agent_config
            .workspace
            .as_ref()
            .map(|ws_raw| crate::adapters::tool_builder::expand_tilde(ws_raw));

        let (system_prompt_str, tools, tool_executor): (
            String,
            Vec<ToolDef>,
            Arc<dyn ToolExecutor>,
        ) = if let Some(ref ws) = workspace {
            let base_tools = channel_runtime::compute_base_tools(
                true,
                memory_handle.is_some(),
                &agent_config.workspace_tools,
            );
            let skill_source = FileSystemSkillSource::new(ws.clone());
            let base_reserved: Vec<String> =
                base_tools.iter().map(|t| t.name.clone()).collect();
            let mut skill_registry = SkillRegistry::new(base_reserved)
                .with_allowlist(Some(agent_config.skill_packages.clone()));
            skill_registry.reload(&skill_source);

            let current_tools =
                channel_runtime::rebuild_tools(&base_tools, &skill_registry);
            let prompt = channel_runtime::rebuild_system_prompt(
                agent_config,
                true,
                &skill_registry,
                &current_tools,
            );
            let tool_defs = current_tools.clone();
            let tool_executor = channel_runtime::build_tool_executor(
                ws,
                &current_tools,
                &skill_registry,
                &memory_handle,
                &secret_registry,
                log_activity.clone(),
                None,
                shared_http_client.as_ref(),
                Some(&config.memory),
            )
            .map(|executor| Arc::new(executor) as Arc<dyn ToolExecutor>)
            .unwrap_or_else(|| Arc::new(NoopRuntimeToolExecutor));

            (prompt, tool_defs, tool_executor)
        } else {
            let prompt =
                crate::adapters::skill_builder::build_system_prompt(agent_config, false, &[]);
            (prompt, vec![], Arc::new(NoopRuntimeToolExecutor))
        };

        let tool_count = tools.len();
        role_to_agent.insert(role_str.clone(), agent_id.clone());
        agent_list.push((
            role_str.clone(),
            agent_id.clone(),
            agent_config.engine.clone(),
        ));

        agent_runtimes.insert(
            agent_id.clone(),
            Arc::new(AgentRuntime {
                engine,
                tools,
                tool_executor,
                system_prompt: system_prompt_str,
                workspace,
                task_token_budget: Some(agent_config.limits.max_tokens_per_flow as u32),
                max_tool_rounds: agent_config.limits.max_tool_rounds,
                max_tool_result_chars: agent_config.limits.max_tool_result_chars,
                stream_event_timeout_secs: agent_config.limits.stream_event_timeout_secs,
                compact_result_limit: agent_config.limits.compact_result_limit,
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

        // Collect role descriptions for the planner.
        // Include full instructions (truncated) and dependency info.
        let identity_name = agent_config
            .identity
            .name
            .as_deref()
            .unwrap_or(agent_id.as_str());
        let instructions = agent_config
            .identity
            .instructions
            .as_deref()
            .unwrap_or("AI assistant");
        let truncated = if instructions.len() > 500 {
            let mut end = 500;
            while end > 0 && !instructions.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &instructions[..end])
        } else {
            instructions.to_string()
        };
        let mut desc = format!("{}\nInstructions: {}", identity_name, truncated);
        if !agent_config.requires.is_empty() {
            desc.push_str(&format!(
                "\nREQUIRES (must depend on): {}",
                agent_config.requires.join(", ")
            ));
        }
        agent_descriptions.insert(role_str.clone(), desc);
        if planner_agent_id.is_none() {
            planner_agent_id = Some(agent_id.clone());
        }
    }

    if agent_runtimes.is_empty() {
        anyhow::bail!("No agents with roles configured for orchestration");
    }

    // Build a dedicated planner engine if configured.
    let orch = config.orchestrator.as_ref();
    let dedicated_planner: Option<Box<dyn Engine>> = match (
        orch.and_then(|o| o.planner_engine.as_ref()),
        orch.and_then(|o| o.planner_model.as_ref()),
    ) {
        (Some(engine_type), Some(model)) => {
            match crate::adapters::engine_builder::build_planner_engine(engine_type, model, config.claude_code.as_ref()) {
                Ok(e) => {
                    tracing::info!(engine = %engine_type, model = %model, "Built dedicated planner engine");
                    Some(e)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to build planner engine, falling back to default agent");
                    None
                }
            }
        }
        _ => None,
    };

    // Print fleet banner.
    println!();
    println!("  TENGU FLEET ORCHESTRATOR");
    println!("  ─────────────────────────────────────");
    println!("  Agents: {}", agent_runtimes.len());
    for (role, aid, eid) in &agent_list {
        println!("    [{:>20}]  {} ({})", role, aid, eid);
    }
    println!(
        "  Shared memory: {}",
        if memory_handle.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Execution: event-bus (DAG dispatch)");
    println!("  ─────────────────────────────────────");
    println!();
    println!("  Commands:");
    println!("    <role>: <task>   — Direct dispatch (e.g. \"backend_engineer: add caching\")");
    println!("    <goal>           — Plan & execute across agents (parallel batches)");
    println!("    /fleet           — Show agents");
    println!("    /tasks           — Show task history");
    println!("    /quit            — Exit");
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

    let mut task_counter: u64 = 0;

    let ev_config = OrchestratorConfig {
        max_retries: orch_config.max_retries,
        task_timeout: std::time::Duration::from_secs(300),
        ..OrchestratorConfig::default()
    };

    // Main dispatch loop.
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
                print_agents(&agent_list);
                continue;
            }
            "/tasks" => {
                print_task_history(&task_history);
                continue;
            }
            _ => {}
        }

        // Try "role: task" for direct dispatch, otherwise plan-and-execute.
        if let Some((role, description)) = parse_task_input(&input) {
            // ── Direct dispatch to a specific role ──
            let agent_id = match role_to_agent.get(role.key()) {
                Some(id) => id.clone(),
                None => {
                    println!("  No agent for role: {}", role.label());
                    println!(
                        "  Available: {}",
                        role_to_agent.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                    continue;
                }
            };

            task_counter += 1;
            let task_id = format!("task-{}", task_counter);
            task_history.record(task_id.clone(), description.clone(), role.key().to_string());
            task_history.assign(&task_id, &agent_id);

            println!("  Task {} → {} ({})", task_id, agent_id, role.label());

            let runtime = agent_runtimes.get(&agent_id).unwrap();
            let result = execute_agent_task(runtime, &description, &secret_registry).await;

            match result {
                Ok((output, _tool_outcomes)) => {
                    println!();
                    println!("  ── {} ({}) ──", agent_id, role.label());
                    println!("{}", output);
                    println!("  ── end ──");
                    println!();

                    task_history.complete(&task_id);
                }
                Err(e) => {
                    println!("  Task failed: {}", e);
                }
            }
        } else {
            // ── Plan-and-execute: shared orchestration pipeline ──
            let plan_engine: &dyn Engine = dedicated_planner
                .as_deref()
                .unwrap_or_else(|| {
                    let pid = planner_agent_id.as_ref().unwrap();
                    agent_runtimes.get(pid).unwrap().engine.as_ref()
                });

            println!("  Planning...");

            // Build role dependency constraints from agent configs.
            let role_deps: RoleDependencies = config
                .agents
                .values()
                .filter_map(|ac| {
                    let role = ac.role.as_deref()?;
                    if ac.requires.is_empty() {
                        return None;
                    }
                    Some((role.to_string(), ac.requires.clone()))
                })
                .collect();

            // Phase 1: Generate and validate plan (shared with Telegram).
            let prepared = match event_orchestrator::prepare_plan(
                &input,
                plan_engine,
                &agent_descriptions,
                &role_deps,
                &memory_handle,
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    println!("  Failed: {}", e);
                    continue;
                }
            };

            println!();
            println!("  {}", prepared.summary);

            // Build executors for this plan.
            let executors: HashMap<String, Arc<dyn AgentTaskExecutor>> = agent_runtimes
                .iter()
                .map(|(id, rt)| {
                    let executor: Arc<dyn AgentTaskExecutor> = Arc::new(AgentRuntimeExecutor {
                        runtime: Arc::clone(rt),
                        secret_registry: Arc::clone(&secret_registry),
                    });
                    (id.clone(), executor)
                })
                .collect();

            let workspace_name = first_workspace
                .as_ref()
                .and_then(|ws| ws.file_name())
                .map(|n| n.to_string_lossy().to_string());

            // Phase 2: Execute plan via EventBus (shared with Telegram).
            let outcome = event_orchestrator::execute_plan(
                prepared,
                &role_to_agent,
                &executors,
                &memory_handle,
                &input,
                workspace_name.as_deref(),
                &ev_config,
            )
            .await;

            // Present results (CLI-specific).
            match outcome {
                Ok(plan_outcome) => {
                    for task_out in &plan_outcome.tasks {
                        if let Some(ref output) = task_out.output {
                            println!();
                            println!("  ── {} ──", task_out.id);
                            println!("{}", output);
                            println!("  ── end ──");
                        }
                    }

                    let succeeded = plan_outcome
                        .tasks
                        .iter()
                        .filter(|t| t.status == TaskStatus::Completed)
                        .count();
                    let failed = plan_outcome
                        .tasks
                        .iter()
                        .filter(|t| t.status == TaskStatus::Failed)
                        .count();
                    let skipped = plan_outcome
                        .tasks
                        .iter()
                        .filter(|t| t.status == TaskStatus::Skipped)
                        .count();
                    println!();
                    println!(
                        "  Plan completed: {}/{} succeeded, {} failed, {} skipped",
                        succeeded,
                        plan_outcome.tasks.len(),
                        failed,
                        skipped,
                    );
                }
                Err(e) => {
                    println!("  Orchestration failed: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Execute a single task using an agent's engine and tools.
async fn execute_agent_task(
    runtime: &AgentRuntime,
    task_description: &str,
    secret_registry: &SecretRegistry,
) -> Result<(String, Vec<(String, String)>)> {
    let messages = vec![Message {
        role: Role::User,
        content: task_description.to_string(),
        tool_call_id: None,
        tool_calls: None,
    }];

    let context = EngineContext {
        workspace: runtime.workspace.clone(),
        system_prompt: Some(runtime.system_prompt.clone()),
        bridge_tools: None,
        max_tool_rounds: Some(runtime.max_tool_rounds),
        max_mcp_result_chars: None,
    };

    let sanitized = SanitizedToolExecutor::new(runtime.tool_executor.as_ref(), secret_registry);

    let response = collect_engine_response(
        runtime.engine.as_ref(),
        &messages,
        &runtime.tools,
        &context,
        Some(&sanitized),
        None,
        None,
        runtime.task_token_budget,
        runtime.max_tool_rounds,
        runtime.max_tool_result_chars,
        runtime.stream_event_timeout_secs,
        runtime.compact_result_limit,
    )
    .await?;

    let combined = response.text;
    Ok((combined, response.tool_outcomes))
}

struct NoopRuntimeToolExecutor;

impl ToolExecutor for NoopRuntimeToolExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        anyhow::bail!("No tools available (agent has no workspace): {}", call.name)
    }
}

/// Parse "role: description" input. Returns None if format is invalid.
fn parse_task_input(input: &str) -> Option<(AgentRole, String)> {
    let colon_pos = input.find(':')?;
    let role_str = input[..colon_pos].trim();
    let description = input[colon_pos + 1..].trim();
    if description.is_empty() {
        return None;
    }
    let role: AgentRole = role_str.parse().ok()?;
    Some((role, description.to_string()))
}

fn print_agents(agent_list: &[(String, String, String)]) {
    println!();
    println!("  Fleet Agents");
    println!("  ─────────────────────────────────────");
    for (role, aid, eid) in agent_list {
        println!("    [{:>20}]  {} ({})", role, aid, eid);
    }
    println!("  ─────────────────────────────────────");
    println!();
}

fn print_task_history(history: &TaskHistory) {
    let entries = history.all();
    println!();
    if entries.is_empty() {
        println!("  No tasks yet.");
    } else {
        println!("  Task History");
        println!("  ─────────────────────────────────────");
        for entry in &entries {
            let agent = entry.assigned_agent.as_deref().unwrap_or("-");
            println!(
                "    {} [{}] {} → {} | {}",
                entry.id,
                entry.status,
                entry.role,
                agent,
                if entry.description.len() > 60 {
                    format!("{}...", &entry.description[..57])
                } else {
                    entry.description.clone()
                },
            );
        }
        println!("  ─────────────────────────────────────");
    }
    println!();
}
