//! Adapter wiring for fleet orchestrator bootstrap and task dispatch.

use crate::adapters::composite_tool_executor::CompositeToolExecutionAdapter;
use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::engine_factory::build_engine;
use crate::adapters::memory_store::DiskVectorMemoryStore;
use crate::adapters::memory_tool_executor::{MemoryServiceHandle, MemoryToolExecutionAdapter};
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::skill_tool_executor::SkillToolExecutionAdapter;
use crate::adapters::system_prompt;
use crate::adapters::task_store::InMemoryTaskStore;
use crate::adapters::workspace_tools::{self, WorkspaceToolExecutionAdapter};
use crate::application::engine_runtime::{
    collect_engine_response, SanitizedToolExecutor, ToolExecutor,
};
use crate::application::fleet_runtime::{FleetAgent, FleetAgentStatus, FleetRuntimeService};
use crate::application::ports::{ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
use crate::application::skill_catalog;
use crate::application::task_orchestrator::TaskOrchestratorService;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::{
    build_memory_tools, build_workspace_tools, filter_tools_by_allowlist,
};
use crate::domain::agent_role::AgentRole;
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::task::TaskResult;
use crate::domain::tool_policy::ToolPolicyCatalog;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tengu_core::config::Config;
use tengu_core::events::EventBus;
use tengu_core::types::{Message, Role, ToolCall, ToolDef};
use tengu_core::{Engine, EngineContext};
use tracing::info;

/// Auto-approval adapter for fleet agents — always approves tool calls.
struct AutoApproval;

impl ToolApprovalPort for AutoApproval {
    fn request_tool_approval(&self, _call: &ToolCall) -> Result<bool> {
        Ok(true)
    }
}

/// No-op tool activity adapter for fleet agents — logs are sufficient.
struct LogToolActivity;

impl ToolActivityPort for LogToolActivity {
    fn publish_tool_activity(&self, call: &ToolCall) {
        tracing::debug!(tool = %call.name, "Fleet agent tool call");
    }
}

/// Per-agent runtime state: engine, tools, executor, system prompt.
struct AgentRuntime {
    engine: Box<dyn Engine>,
    tools: Vec<ToolDef>,
    tool_executor: ToolUseService,
    system_prompt: String,
    workspace: Option<std::path::PathBuf>,
}

/// A single step in a coordinated execution plan.
struct PlanStep {
    role: String,
    task: String,
}

/// Result of a completed plan step, fed as context to subsequent steps.
struct StepResult {
    role: String,
    task: String,
    output: String,
    success: bool,
}

/// Max chars of each previous step's output to include in context for the next step.
const MAX_STEP_CONTEXT_CHARS: usize = 3000;

/// Boot the orchestrator: register fleet agents, run interactive task dispatch.
///
/// Builds per-agent engines, tool executors with shared memory, and an interactive
/// stdin loop for task submission with role-based routing.
pub(crate) async fn boot_orchestrator(
    config: &Config,
    event_bus: &dyn EventBus,
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
                    &dirs_next::home_dir()
                        .unwrap_or_default()
                        .to_string_lossy(),
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
    let mut fleet = FleetRuntimeService::new();
    let mut agent_runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    let mut agent_descriptions: HashMap<String, String> = HashMap::new();
    let mut planner_agent_id: Option<String> = None;

    let shell: Arc<dyn crate::application::ports::ShellExecutionPort> =
        Arc::new(LocalShellExecutor);
    let auto_approval: Arc<dyn ToolApprovalPort> = Arc::new(AutoApproval);
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

        let (system_prompt_str, tools, tool_executor) = if let Some(ref ws) = workspace {
            let skill_source = FileSystemSkillSource::new(ws.clone());
            let mut all_tools = filter_tools_by_allowlist(
                build_workspace_tools(),
                agent_config.allowed_tools.as_deref(),
            );
            if memory_handle.is_some() {
                all_tools.extend(build_memory_tools());
            }
            let reserved: Vec<&str> = all_tools.iter().map(|t| t.name.as_str()).collect();
            let agent_skill_allowlist = agent_config.skills.as_deref();
            let loaded =
                skill_catalog::load_skills_for_agent(&skill_source, &reserved, agent_skill_allowlist);
            all_tools.extend(loaded.tool_defs);

            let skill_contexts: Vec<String> =
                loaded.context_fragments.iter().map(|f| f.body.clone()).collect();
            let prompt =
                system_prompt::build_system_prompt(agent_config, true, &skill_contexts);

            // Build composite tool executor.
            let ws_executor = WorkspaceToolExecutionAdapter::new(ws.clone())
                .with_shell(shell.clone());
            let mut composite =
                CompositeToolExecutionAdapter::new(Arc::new(ws_executor));

            // Add skill executor if there are skills.
            if !loaded.executable_skills.is_empty() {
                let skill_names: HashSet<String> =
                    loaded.executable_skills.iter().map(|s| s.name.clone()).collect();
                let skill_executor = SkillToolExecutionAdapter::new(
                    loaded.executable_skills,
                    shell.clone(),
                    ws.clone(),
                );
                composite = composite.with_executor(Arc::new(skill_executor), skill_names);
            }

            // Add memory executor if memory is available.
            if let Some(ref mh) = memory_handle {
                let mem_executor =
                    MemoryToolExecutionAdapter::new(mh.clone(), secret_registry.clone())?;
                let mem_names: HashSet<String> = ["remember".to_string()].into_iter().collect();
                composite = composite.with_executor(Arc::new(mem_executor), mem_names);
            }

            let policies = ToolPolicyCatalog::from_tools(&all_tools);
            let tool_use = ToolUseService::new(
                policies,
                log_activity.clone(),
                auto_approval.clone(),
                Arc::new(composite),
            );

            (prompt, all_tools, tool_use)
        } else {
            let prompt = system_prompt::build_system_prompt(agent_config, false, &[]);
            let noop_executor = Arc::new(NoopToolExecutor);
            let policies = ToolPolicyCatalog::from_tools(&[]);
            let tool_use = ToolUseService::new(
                policies,
                log_activity.clone(),
                auto_approval.clone(),
                noop_executor,
            );
            (prompt, vec![], tool_use)
        };

        let tool_count = tools.len();
        fleet.register_agent(
            agent_id.clone(),
            role.clone(),
            agent_config.engine.clone(),
            system_prompt_str.clone(),
            tools.clone(),
        );

        agent_runtimes.insert(
            agent_id.clone(),
            AgentRuntime {
                engine,
                tools,
                tool_executor,
                system_prompt: system_prompt_str,
                workspace,
            },
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

    let agent_count = fleet.agents().len();
    if agent_count == 0 {
        anyhow::bail!("No agents with roles configured for orchestration");
    }

    // Print fleet banner.
    println!();
    println!("  TENGU FLEET ORCHESTRATOR");
    println!("  ─────────────────────────────────────");
    println!("  Agents: {}", agent_count);
    for agent in fleet.agents() {
        println!(
            "    [{:>20}]  {} ({})",
            agent.role.label(),
            agent.agent_id,
            agent.engine_id
        );
    }
    println!(
        "  Shared memory: {}",
        if memory_handle.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  ─────────────────────────────────────");
    println!();
    println!("  Commands:");
    println!("    <role>: <task>   — Direct dispatch (e.g. \"backend_engineer: add caching\")");
    println!("    <goal>           — Plan & execute across agents");
    println!("    /fleet           — Show agent status");
    println!("    /tasks           — Show task history");
    println!("    /quit            — Exit");
    println!();

    // Stdin reader on a separate thread (engines aren't Send).
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let _ = tx.send(line.trim().to_string());
                }
                Err(_) => break,
            }
        }
    });

    let orchestrator = TaskOrchestratorService {
        store: &task_store,
        event_bus,
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
            Err(_) => break, // stdin closed
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
                print_fleet_status(&fleet);
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
            let agent_id = match fleet.find_idle_agent_for_role(role.clone()) {
                Some(agent) => agent.agent_id.clone(),
                None => {
                    println!("  No idle agent available for role: {}", role.label());
                    continue;
                }
            };

            task_counter += 1;
            let task_id = format!("task-{}", task_counter);
            let task =
                orchestrator.create_task(task_id.clone(), description.clone(), role.clone())?;
            orchestrator.assign_task(&task.id.0, &agent_id).await?;
            fleet.mark_busy(&agent_id, &task.id.0);

            println!(
                "  Task {} assigned to {} ({})",
                task_id,
                agent_id,
                role.label()
            );
            println!("  Executing...");

            let runtime = agent_runtimes.get(&agent_id).unwrap();
            let result = execute_agent_task(runtime, &description, &secret_registry).await;

            match result {
                Ok(output) => {
                    println!();
                    println!("  ── {} ({}) ──", agent_id, role.label());
                    println!("{}", output);
                    println!("  ── end ──");
                    println!();

                    orchestrator
                        .complete_task(
                            &task_id,
                            TaskResult {
                                success: true,
                                output,
                                validation_notes: None,
                            },
                        )
                        .await?;
                    fleet.mark_idle(&agent_id);
                }
                Err(e) => {
                    println!("  Task failed: {}", e);
                    fleet.mark_failed(&agent_id);
                }
            }
        } else {
            // ── Plan-and-execute: decompose goal into multi-agent steps ──
            let pid = match planner_agent_id {
                Some(ref id) => id.clone(),
                None => {
                    println!("  No agents available for planning.");
                    continue;
                }
            };
            let planner_engine = match agent_runtimes.get(&pid) {
                Some(rt) => rt.engine.as_ref(),
                None => {
                    println!("  Planner agent not found.");
                    continue;
                }
            };

            println!("  Planning...");

            let steps = match generate_plan(
                planner_engine,
                &input,
                fleet.agents(),
                &agent_descriptions,
            )
            .await
            {
                Ok(steps) => steps,
                Err(e) => {
                    println!("  Failed to generate plan: {}", e);
                    continue;
                }
            };

            println!();
            println!("  Execution Plan ({} steps):", steps.len());
            for (i, step) in steps.iter().enumerate() {
                println!("    {}. [{}] {}", i + 1, step.role, step.task);
            }
            println!();

            let mut step_results: Vec<StepResult> = Vec::new();
            for (i, step) in steps.iter().enumerate() {
                let role: AgentRole = match step.role.parse() {
                    Ok(r) => r,
                    Err(_) => {
                        println!("  Step {}: invalid role '{}', skipping", i + 1, step.role);
                        step_results.push(StepResult {
                            role: step.role.clone(),
                            task: step.task.clone(),
                            output: "Skipped — invalid role".into(),
                            success: false,
                        });
                        continue;
                    }
                };

                let agent_id = match fleet.find_idle_agent_for_role(role.clone()) {
                    Some(a) => a.agent_id.clone(),
                    None => {
                        println!(
                            "  Step {}: no idle agent for role '{}', skipping",
                            i + 1,
                            step.role
                        );
                        step_results.push(StepResult {
                            role: step.role.clone(),
                            task: step.task.clone(),
                            output: "Skipped — no idle agent".into(),
                            success: false,
                        });
                        continue;
                    }
                };

                task_counter += 1;
                let task_id = format!("task-{}", task_counter);
                orchestrator.create_task(task_id.clone(), step.task.clone(), role.clone())?;
                orchestrator.assign_task(&task_id, &agent_id).await?;
                fleet.mark_busy(&agent_id, &task_id);

                println!("  Executing step {}/{}...", i + 1, steps.len());

                let enriched_prompt =
                    build_step_context(&input, i, steps.len(), &step.task, &step_results);
                let runtime = agent_runtimes.get(&agent_id).unwrap();
                let result =
                    execute_agent_task(runtime, &enriched_prompt, &secret_registry).await;

                match result {
                    Ok(output) => {
                        println!();
                        println!(
                            "  ── {} ({}) — step {}/{} ──",
                            agent_id,
                            step.role,
                            i + 1,
                            steps.len()
                        );
                        println!("{}", output);
                        println!("  ── end ──");
                        println!();

                        orchestrator
                            .complete_task(
                                &task_id,
                                TaskResult {
                                    success: true,
                                    output: output.clone(),
                                    validation_notes: None,
                                },
                            )
                            .await?;
                        fleet.mark_idle(&agent_id);

                        step_results.push(StepResult {
                            role: step.role.clone(),
                            task: step.task.clone(),
                            output,
                            success: true,
                        });
                    }
                    Err(e) => {
                        println!("  Step {} failed: {}", i + 1, e);
                        fleet.mark_idle(&agent_id);

                        step_results.push(StepResult {
                            role: step.role.clone(),
                            task: step.task.clone(),
                            output: format!("Failed: {}", e),
                            success: false,
                        });
                    }
                }
            }

            let succeeded = step_results.iter().filter(|r| r.success).count();
            println!(
                "  Plan completed: {}/{} steps successful",
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

    // Bridge ToolUseService to ToolExecutor trait.
    let bridge = ToolUseServiceBridge(&runtime.tool_executor);
    let sanitized = SanitizedToolExecutor::new(&bridge, secret_registry);

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

/// Generate an execution plan by asking an LLM to decompose a goal into steps.
async fn generate_plan(
    engine: &dyn Engine,
    goal: &str,
    agents: &[FleetAgent],
    descriptions: &HashMap<String, String>,
) -> Result<Vec<PlanStep>> {
    let mut team_lines = String::new();
    for agent in agents {
        let key = agent.role.key();
        let desc = descriptions
            .get(key)
            .map(|s| s.as_str())
            .unwrap_or("AI assistant");
        team_lines.push_str(&format!("- {} : {}\n", key, desc));
    }

    let system = format!(
        "You are a project coordinator. Break down a goal into sequential tasks \
         for the available team members.\n\n\
         Team members:\n{}\n\
         Respond with ONLY valid JSON, no markdown fences, no extra text:\n\
         {{\"steps\":[{{\"role\":\"exact_role_key\",\"task\":\"specific actionable task\"}}]}}\n\n\
         Rules:\n\
         - Order steps logically (design before implementation, backend before frontend if APIs needed, etc.)\n\
         - Each task must be specific and actionable\n\
         - When a step depends on a prior step, mention it (e.g. \"using the design tokens from step 1\")\n\
         - Use ONLY the exact role keys listed above\n\
         - Keep to 2-6 steps",
        team_lines
    );

    let messages = vec![Message {
        role: Role::User,
        content: format!("Goal: {}", goal),
        tool_call_id: None,
        tool_calls: None,
    }];

    let context = EngineContext {
        workspace: None,
        system_prompt: Some(system),
    };

    let response =
        collect_engine_response(engine, &messages, &[], &context, None, None, None).await?;

    parse_plan_json(&response.text)
}

/// Parse a JSON execution plan from LLM output.
fn parse_plan_json(text: &str) -> Result<Vec<PlanStep>> {
    let json_str =
        extract_json(text).ok_or_else(|| anyhow::anyhow!("No JSON found in planner response"))?;
    let value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow::anyhow!("Invalid JSON: {}", e))?;
    let steps = value
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("Plan missing 'steps' array"))?;

    let mut result = Vec::new();
    for step in steps {
        let role = step
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| anyhow::anyhow!("Plan step missing 'role'"))?;
        let task = step
            .get("task")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Plan step missing 'task'"))?;
        result.push(PlanStep {
            role: role.to_string(),
            task: task.to_string(),
        });
    }

    if result.is_empty() {
        anyhow::bail!("Plan has no steps");
    }
    Ok(result)
}

/// Extract a JSON object from text that may contain markdown fences or prose.
fn extract_json(text: &str) -> Option<String> {
    // Try ```json ... ``` blocks first.
    if let Some(start) = text.find("```json") {
        let inner_start = start + 7;
        if let Some(end) = text[inner_start..].find("```") {
            return Some(text[inner_start..inner_start + end].trim().to_string());
        }
    }
    // Try ``` ... ``` blocks.
    if let Some(start) = text.find("```") {
        let inner_start = start + 3;
        if let Some(end) = text[inner_start..].find("```") {
            let inner = text[inner_start..inner_start + end].trim();
            if inner.starts_with('{') {
                return Some(inner.to_string());
            }
        }
    }
    // Try raw JSON.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if start <= end {
        Some(text[start..=end].to_string())
    } else {
        None
    }
}

/// Build an enriched prompt for a plan step with context from previous steps.
fn build_step_context(
    goal: &str,
    step_idx: usize,
    total_steps: usize,
    current_task: &str,
    previous_results: &[StepResult],
) -> String {
    let mut ctx = format!(
        "## Overall Goal\n{}\n\n## Your Task (Step {}/{})\n{}\n",
        goal,
        step_idx + 1,
        total_steps,
        current_task
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

/// Bridge from ToolUseService (application layer) to ToolExecutor trait (engine_runtime).
struct ToolUseServiceBridge<'a>(&'a ToolUseService);

impl<'a> ToolExecutor for ToolUseServiceBridge<'a> {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        self.0.execute(call)
    }
}

/// No-op tool executor for agents without a workspace.
struct NoopToolExecutor;

impl ToolExecutionPort for NoopToolExecutor {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
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

fn print_fleet_status(fleet: &FleetRuntimeService) {
    println!();
    println!("  Fleet Status");
    println!("  ─────────────────────────────────────");
    for agent in fleet.agents() {
        let status = match agent.status {
            FleetAgentStatus::Idle => "idle",
            FleetAgentStatus::Busy => "busy",
            FleetAgentStatus::Failed => "FAILED",
        };
        let task_info = agent
            .current_task_id
            .as_deref()
            .map(|t| format!(" ({})", t))
            .unwrap_or_default();
        println!(
            "    {} [{:>20}] {} — {}{} ({} tools)",
            if agent.status == FleetAgentStatus::Idle {
                "●"
            } else if agent.status == FleetAgentStatus::Busy {
                "◉"
            } else {
                "✗"
            },
            agent.role.label(),
            agent.agent_id,
            status,
            task_info,
            agent.tools.len(),
        );
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

// Import for load_all_tasks
use crate::application::ports::TaskStorePort;

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
        // With dynamic roles, any non-empty string is a valid role.
        let (role, desc) = parse_task_input("unknown_role: do something").unwrap();
        assert_eq!(role.key(), "unknown_role");
        assert_eq!(desc, "do something");
    }

    #[test]
    fn parse_task_input_empty_role_part() {
        assert!(parse_task_input(" : do something").is_none());
    }

    #[test]
    fn extract_json_raw() {
        let text = r#"{"steps":[{"role":"qa","task":"review"}]}"#;
        let json = extract_json(text).unwrap();
        assert!(json.contains("steps"));
    }

    #[test]
    fn extract_json_from_markdown_fence() {
        let text = "Here is the plan:\n```json\n{\"steps\":[{\"role\":\"qa\",\"task\":\"test\"}]}\n```\nDone.";
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        assert!(json.contains("steps"));
    }

    #[test]
    fn extract_json_from_plain_fence() {
        let text = "```\n{\"steps\":[]}\n```";
        let json = extract_json(text).unwrap();
        assert_eq!(json, "{\"steps\":[]}");
    }

    #[test]
    fn extract_json_with_surrounding_prose() {
        let text = "Sure! Here is the plan: {\"steps\":[{\"role\":\"dev\",\"task\":\"code\"}]} Hope this helps.";
        let json = extract_json(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("steps").unwrap().is_array());
    }

    #[test]
    fn extract_json_no_json() {
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn parse_plan_json_valid() {
        let text = r#"{"steps":[{"role":"designer","task":"create mockups"},{"role":"frontend_engineer","task":"implement UI"}]}"#;
        let steps = parse_plan_json(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].role, "designer");
        assert_eq!(steps[1].role, "frontend_engineer");
    }

    #[test]
    fn parse_plan_json_empty_steps() {
        let text = r#"{"steps":[]}"#;
        assert!(parse_plan_json(text).is_err());
    }

    #[test]
    fn parse_plan_json_missing_field() {
        let text = r#"{"steps":[{"role":"qa"}]}"#;
        assert!(parse_plan_json(text).is_err());
    }

    #[test]
    fn build_step_context_first_step() {
        let ctx = build_step_context("build a site", 0, 3, "design the layout", &[]);
        assert!(ctx.contains("Overall Goal"));
        assert!(ctx.contains("build a site"));
        assert!(ctx.contains("Step 1/3"));
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
        let ctx = build_step_context("build a site", 1, 3, "implement UI", &prior);
        assert!(ctx.contains("Step 2/3"));
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
        let ctx = build_step_context("goal", 1, 2, "next task", &prior);
        assert!(ctx.contains("(truncated)"));
        assert!(ctx.len() < MAX_STEP_CONTEXT_CHARS + 1000);
    }
}
