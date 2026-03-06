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
use crate::application::fleet_runtime::{FleetAgentStatus, FleetRuntimeService};
use crate::application::ports::{ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
use crate::application::skill_catalog;
use crate::application::task_orchestrator::TaskOrchestratorService;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::{build_memory_tools, build_workspace_tools};
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
            let mut all_tools = build_workspace_tools();
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
            role,
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
    println!("    <role>: <task>   — Submit task (e.g. \"qa: review auth code\")");
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

        // Parse "role: task description" format.
        let (role, description) = match parse_task_input(&input) {
            Some(parsed) => parsed,
            None => {
                println!("  Format: <role>: <task description>");
                println!("  Roles: qa, backend_engineer, integration_master");
                continue;
            }
        };

        // Find an idle agent for the role.
        let agent_id = match fleet.find_idle_agent_for_role(role) {
            Some(agent) => agent.agent_id.clone(),
            None => {
                println!("  No idle agent available for role: {}", role.label());
                continue;
            }
        };

        // Create and assign the task.
        task_counter += 1;
        let task_id = format!("task-{}", task_counter);
        let task = orchestrator.create_task(task_id.clone(), description.clone(), role)?;
        orchestrator.assign_task(&task.id.0, &agent_id).await?;
        fleet.mark_busy(&agent_id, &task.id.0);

        println!(
            "  Task {} assigned to {} ({})",
            task_id,
            agent_id,
            role.label()
        );
        println!("  Executing...");

        // Execute the task via collect_engine_response.
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
        assert_eq!(role, AgentRole::QA);
        assert_eq!(desc, "review the auth module");
    }

    #[test]
    fn parse_task_input_backend() {
        let (role, desc) = parse_task_input("backend_engineer: implement caching").unwrap();
        assert_eq!(role, AgentRole::BackendEngineer);
        assert_eq!(desc, "implement caching");
    }

    #[test]
    fn parse_task_input_hyphenated() {
        let (role, _) = parse_task_input("integration-master: wire up API").unwrap();
        assert_eq!(role, AgentRole::IntegrationMaster);
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
    fn parse_task_input_invalid_role() {
        assert!(parse_task_input("unknown_role: do something").is_none());
    }
}
