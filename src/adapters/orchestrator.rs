//! Adapter wiring for fleet orchestrator bootstrap and task dispatch.
//!
//! Uses JoinSet for parallel batch execution: independent tasks within a
//! batch run concurrently, while batches execute sequentially to honor
//! task dependencies.

use crate::adapters::channel_runtime;
use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::engine_factory::build_engine;
use crate::adapters::memory_store::DiskVectorMemoryStore;
use crate::adapters::memory_tool_executor::MemoryServiceHandle;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::task_store::InMemoryTaskStore;
use crate::adapters::workspace_tools;
use crate::application::engine_runtime::{
    collect_engine_response, SanitizedToolExecutor, ToolExecutor,
};
use crate::application::ports::{TaskStorePort, ToolActivityPort, ToolApprovalPort};
use crate::application::skill_registry::SkillRegistry;
use crate::application::task_orchestrator::TaskOrchestratorService;
use crate::application::task_planner;
use crate::domain::agent_role::AgentRole;
use crate::domain::approval::DenyByDefaultApproval;
use crate::domain::capability::parse_capability_set;
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::task::TaskResult;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tengu_core::config::Config;
use tengu_core::types::{Message, Role, ToolCall, ToolDef};
use tengu_core::{Engine, EngineContext};
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
}

/// Result of a completed task, fed as context to dependent tasks.
struct StepResult {
    role: String,
    task: String,
    output: String,
    success: bool,
}

/// Max chars of each previous step's output to include in context for the next step.
const MAX_STEP_CONTEXT_CHARS: usize = 3000;

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

    // Build shared memory handle for all fleet agents.
    let memory_handle: Option<Arc<MemoryServiceHandle>> = if config.memory.enabled {
        match std::env::var("OPENROUTER_API_KEY") {
            Ok(api_key) => {
                let store_path_str = config.memory.store_path.replace(
                    "~",
                    &dirs_next::home_dir().unwrap_or_default().to_string_lossy(),
                );
                match DiskVectorMemoryStore::new(std::path::Path::new(&store_path_str)) {
                    Ok(store) => {
                        let embedding = OpenRouterEmbeddingAdapter::new(
                            api_key,
                            config.memory.embedding_model.clone(),
                        );
                        info!("Shared memory store initialized for fleet");
                        Some(Arc::new(MemoryServiceHandle {
                            embedding: Arc::new(embedding),
                            store: Arc::new(store),
                        }))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to init shared memory store");
                        None
                    }
                }
            }
            Err(_) => {
                tracing::warn!("OPENROUTER_API_KEY not set, fleet memory disabled");
                None
            }
        }
    } else {
        None
    };

    let task_store = InMemoryTaskStore::new();
    let mut agent_runtimes: HashMap<String, Arc<AgentRuntime>> = HashMap::new();
    let mut role_to_agent: HashMap<String, String> = HashMap::new();
    let mut agent_descriptions: HashMap<String, String> = HashMap::new();
    let mut planner_agent_id: Option<String> = None;
    // (role_key, agent_id, engine_id) for banner display.
    let mut agent_list: Vec<(String, String, String)> = Vec::new();

    let deny_approval: Arc<dyn ToolApprovalPort> = Arc::new(DenyByDefaultApproval);
    let log_activity: Arc<dyn ToolActivityPort> = Arc::new(LogToolActivity);

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
        let engine = match build_engine(agent_id, agent_config) {
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
            .map(|ws_raw| workspace_tools::expand_tilde(ws_raw));

        let (system_prompt_str, tools, tool_executor): (
            String,
            Vec<ToolDef>,
            Arc<dyn ToolExecutor>,
        ) = if let Some(ref ws) = workspace {
            let capabilities = parse_capability_set(&agent_config.capabilities)
                .expect("agent capabilities should validate");
            let base_tools = channel_runtime::compute_base_tools(
                true,
                memory_handle.is_some(),
                &agent_config.capabilities,
            );
            let skill_source = FileSystemSkillSource::new(ws.clone());
            let base_reserved: Vec<String> =
                base_tools.iter().map(|t| t.def.name.clone()).collect();
            let mut skill_registry = SkillRegistry::new(base_reserved)
                .with_allowlist(Some(agent_config.skill_packages.clone()));
            skill_registry.reload(&skill_source);

            let current_tools =
                channel_runtime::rebuild_tools(&base_tools, &skill_registry, &capabilities);
            let prompt = channel_runtime::rebuild_system_prompt(
                agent_config,
                true,
                &skill_registry,
                &current_tools,
            );
            let tool_defs = channel_runtime::tool_defs(&current_tools);
            let tool_executor = channel_runtime::build_tool_executor(
                ws,
                &current_tools,
                &skill_registry,
                &memory_handle,
                &secret_registry,
                deny_approval.clone(),
                log_activity.clone(),
                None,
            )
            .map(|executor| Arc::new(executor) as Arc<dyn ToolExecutor>)
            .unwrap_or_else(|| Arc::new(NoopRuntimeToolExecutor));

            (prompt, tool_defs, tool_executor)
        } else {
            let prompt =
                crate::adapters::system_prompt::build_system_prompt(agent_config, false, &[]);
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
        let identity_name = agent_config
            .identity
            .name
            .as_deref()
            .unwrap_or(agent_id.as_str());
        let brief = agent_config
            .identity
            .instructions
            .as_deref()
            .and_then(|s| s.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("AI assistant");
        agent_descriptions.insert(role_str.clone(), format!("{} — {}", identity_name, brief));
        if planner_agent_id.is_none() {
            planner_agent_id = Some(agent_id.clone());
        }
    }

    if agent_runtimes.is_empty() {
        anyhow::bail!("No agents with roles configured for orchestration");
    }

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
    println!("  Execution: parallel (JoinSet batches)");
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

    let orchestrator = TaskOrchestratorService {
        store: &task_store,
        max_retries: orch_config.max_retries,
    };
    let mut task_counter: u64 = 0;

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
                print_task_history(&task_store)?;
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
            orchestrator.create_task(task_id.clone(), description.clone(), role.clone())?;
            orchestrator.assign_task(&task_id, &agent_id)?;

            println!("  Task {} → {} ({})", task_id, agent_id, role.label());

            let runtime = agent_runtimes.get(&agent_id).unwrap();
            let result = execute_agent_task(runtime, &description, &secret_registry).await;

            match result {
                Ok(output) => {
                    println!();
                    println!("  ── {} ({}) ──", agent_id, role.label());
                    println!("{}", output);
                    println!("  ── end ──");
                    println!();

                    orchestrator.complete_task(&task_id, TaskResult { output })?;
                }
                Err(e) => {
                    println!("  Task failed: {}", e);
                }
            }
        } else {
            // ── Plan-and-execute: decompose goal into parallel task batches ──
            let pid = match planner_agent_id {
                Some(ref id) => id.clone(),
                None => {
                    println!("  No agents available for planning.");
                    continue;
                }
            };
            let planner_runtime = match agent_runtimes.get(&pid) {
                Some(rt) => rt,
                None => {
                    println!("  Planner agent not found.");
                    continue;
                }
            };

            println!("  Planning...");

            let tasks = match task_planner::generate_plan(
                planner_runtime.engine.as_ref(),
                &input,
                &agent_descriptions,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    println!("  Failed to generate plan: {}", e);
                    continue;
                }
            };

            let batches = match task_planner::resolve_execution_order(&tasks) {
                Ok(b) => b,
                Err(e) => {
                    println!("  Bad plan: {}", e);
                    continue;
                }
            };

            println!();
            println!(
                "  Execution Plan ({} tasks, {} batches):",
                tasks.len(),
                batches.len()
            );
            for (bi, batch) in batches.iter().enumerate() {
                let labels: Vec<String> = batch
                    .iter()
                    .map(|&idx| format!("[{}] {}", tasks[idx].role, tasks[idx].task))
                    .collect();
                if batch.len() > 1 {
                    println!("    Batch {} (parallel):", bi + 1);
                } else {
                    println!("    Batch {}:", bi + 1);
                }
                for label in &labels {
                    println!("      {}", label);
                }
            }
            println!();

            let mut step_results: Vec<StepResult> = Vec::new();

            for (bi, batch) in batches.iter().enumerate() {
                if batch.len() > 1 {
                    let roles: Vec<&str> =
                        batch.iter().map(|&idx| tasks[idx].role.as_str()).collect();
                    println!(
                        "  Batch {}/{} — parallel: {}",
                        bi + 1,
                        batches.len(),
                        roles.join(", ")
                    );
                } else {
                    println!("  Batch {}/{}...", bi + 1, batches.len());
                }

                // Spawn all tasks in this batch concurrently via JoinSet.
                let mut set = tokio::task::JoinSet::new();

                for &task_idx in batch {
                    let plan_task = &tasks[task_idx];
                    let agent_id = match role_to_agent.get(&plan_task.role) {
                        Some(id) => id.clone(),
                        None => {
                            println!("  No agent for role '{}', skipping", plan_task.role);
                            step_results.push(StepResult {
                                role: plan_task.role.clone(),
                                task: plan_task.task.clone(),
                                output: "Skipped — no agent for role".into(),
                                success: false,
                            });
                            continue;
                        }
                    };

                    let runtime = Arc::clone(agent_runtimes.get(&agent_id).unwrap());
                    let sr = Arc::clone(&secret_registry);
                    let prompt = build_step_context(&input, &plan_task.task, &step_results);
                    let role = plan_task.role.clone();
                    let task_desc = plan_task.task.clone();

                    // Track in task store.
                    task_counter += 1;
                    let task_id = format!("task-{}", task_counter);
                    let agent_role: AgentRole =
                        role.parse().unwrap_or_else(|_| "unknown".parse().unwrap());
                    orchestrator.create_task(task_id.clone(), task_desc.clone(), agent_role)?;
                    orchestrator.assign_task(&task_id, &agent_id)?;

                    let tid = task_id.clone();
                    set.spawn(async move {
                        let result = execute_agent_task(&runtime, &prompt, &sr).await;
                        (tid, agent_id, role, task_desc, result)
                    });
                }

                // Collect results from this batch.
                while let Some(join_result) = set.join_next().await {
                    let (task_id, agent_id, role, task_desc, outcome) =
                        join_result.map_err(|e| anyhow::anyhow!("Task panicked: {}", e))?;

                    match outcome {
                        Ok(output) => {
                            println!();
                            println!("  ── {} ({}) ──", agent_id, role);
                            println!("{}", output);
                            println!("  ── end ──");
                            println!();

                            orchestrator.complete_task(
                                &task_id,
                                TaskResult {
                                    output: output.clone(),
                                },
                            )?;

                            step_results.push(StepResult {
                                role,
                                task: task_desc,
                                output,
                                success: true,
                            });
                        }
                        Err(e) => {
                            println!("  {} ({}) failed: {}", agent_id, role, e);
                            step_results.push(StepResult {
                                role,
                                task: task_desc,
                                output: format!("Failed: {}", e),
                                success: false,
                            });
                        }
                    }
                }
            }

            let succeeded = step_results.iter().filter(|r| r.success).count();
            println!(
                "  Plan completed: {}/{} tasks successful",
                succeeded,
                step_results.len()
            );
        }
    }

    Ok(())
}

/// Execute a single task using an agent's engine and tools.
async fn execute_agent_task(
    runtime: &AgentRuntime,
    task_description: &str,
    secret_registry: &SecretRegistry,
) -> Result<String> {
    let messages = vec![Message {
        role: Role::User,
        content: task_description.to_string(),
        tool_call_id: None,
        tool_calls: None,
    }];

    let context = EngineContext {
        workspace: runtime.workspace.clone(),
        system_prompt: Some(runtime.system_prompt.clone()),
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
    )
    .await?;

    Ok(response.text)
}

/// Build an enriched prompt for a task with context from completed steps.
fn build_step_context(goal: &str, current_task: &str, previous_results: &[StepResult]) -> String {
    let mut ctx = format!(
        "## Overall Goal\n{}\n\n## Your Task\n{}\n",
        goal, current_task
    );

    if !previous_results.is_empty() {
        ctx.push_str("\n## Results from Previous Steps\n\n");
        for (i, r) in previous_results.iter().enumerate() {
            ctx.push_str(&format!("### Step {}: {} — {}\n", i + 1, r.role, r.task));
            if r.success {
                let mut end = r.output.len().min(MAX_STEP_CONTEXT_CHARS);
                while end < r.output.len() && !r.output.is_char_boundary(end) {
                    end -= 1;
                }
                if r.output.len() > MAX_STEP_CONTEXT_CHARS {
                    ctx.push_str(&r.output[..end]);
                    ctx.push_str("\n...(truncated)\n\n");
                } else {
                    ctx.push_str(&r.output);
                    ctx.push('\n');
                }
            } else {
                ctx.push_str(&format!("(failed: {})\n\n", r.output));
            }
        }
    }

    ctx
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

fn print_task_history(store: &InMemoryTaskStore) -> Result<()> {
    let tasks = store.load_all_tasks()?;
    println!();
    if tasks.is_empty() {
        println!("  No tasks yet.");
    } else {
        println!("  Task History");
        println!("  ─────────────────────────────────────");
        for task in &tasks {
            let status = match task.status {
                crate::domain::task::TaskStatus::Pending => "pending",
                crate::domain::task::TaskStatus::InProgress => "in-progress",
                crate::domain::task::TaskStatus::Completed => "completed",
                crate::domain::task::TaskStatus::Failed => "failed",
            };
            let agent = task.assigned_agent.as_deref().unwrap_or("-");
            println!(
                "    {} [{}] {} → {} | {}",
                task.id.0,
                status,
                task.role.label(),
                agent,
                if task.description.len() > 60 {
                    format!("{}...", &task.description[..57])
                } else {
                    task.description.clone()
                },
            );
        }
        println!("  ─────────────────────────────────────");
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_input_valid() {
        let (role, desc) = parse_task_input("qa: review the auth module").unwrap();
        assert_eq!(role.key(), "qa");
        assert_eq!(desc, "review the auth module");
    }

    #[test]
    fn parse_task_input_backend() {
        let (role, desc) = parse_task_input("backend_engineer: implement caching").unwrap();
        assert_eq!(role.key(), "backend_engineer");
        assert_eq!(desc, "implement caching");
    }

    #[test]
    fn parse_task_input_hyphenated() {
        let (role, _) = parse_task_input("integration-master: wire up API").unwrap();
        assert_eq!(role.key(), "integration_master");
    }

    #[test]
    fn parse_task_input_no_colon() {
        assert!(parse_task_input("just some text").is_none());
    }

    #[test]
    fn parse_task_input_empty_description() {
        assert!(parse_task_input("qa:").is_none());
        assert!(parse_task_input("qa:   ").is_none());
    }

    #[test]
    fn parse_task_input_any_role_is_valid() {
        let (role, desc) = parse_task_input("unknown_role: do something").unwrap();
        assert_eq!(role.key(), "unknown_role");
        assert_eq!(desc, "do something");
    }

    #[test]
    fn parse_task_input_empty_role_part() {
        assert!(parse_task_input(" : do something").is_none());
    }

    #[test]
    fn build_step_context_first_step() {
        let ctx = build_step_context("build a site", "design the layout", &[]);
        assert!(ctx.contains("Overall Goal"));
        assert!(ctx.contains("build a site"));
        assert!(ctx.contains("design the layout"));
        assert!(!ctx.contains("Previous Steps"));
    }

    #[test]
    fn build_step_context_with_prior_results() {
        let prior = vec![StepResult {
            role: "designer".into(),
            task: "create tokens".into(),
            output: "Primary: #2563eb, font: Inter".into(),
            success: true,
        }];
        let ctx = build_step_context("build a site", "implement UI", &prior);
        assert!(ctx.contains("Results from Previous Steps"));
        assert!(ctx.contains("Primary: #2563eb"));
    }

    #[test]
    fn build_step_context_truncates_long_output() {
        let long_output = "x".repeat(MAX_STEP_CONTEXT_CHARS + 500);
        let prior = vec![StepResult {
            role: "backend".into(),
            task: "build API".into(),
            output: long_output,
            success: true,
        }];
        let ctx = build_step_context("goal", "next task", &prior);
        assert!(ctx.contains("(truncated)"));
        assert!(ctx.len() < MAX_STEP_CONTEXT_CHARS + 1000);
    }
}
