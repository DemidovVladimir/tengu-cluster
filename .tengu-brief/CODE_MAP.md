# Code Map — Multi-Agent Routing

## Critical Paths

### Plain Message Routing (Telegram)
- **Entry**: `telegram_runtime.rs:1750` — route decision (multi-agent orchestration)
- **Parser**: `channel_runtime.rs:383` — `parse_agent_routing()` (@role: and role: formats)
- **Multi-agent flag**: `telegram_runtime.rs:1249` — `is_multi_agent = agent_states.len() > 1`
- **Orchestration**: `telegram_runtime.rs:317` — `orchestrate_team_goal()`
- **Direct routing**: explicit @role: fallback to default

### Planning
- **Classify request**: `task_planner.rs:22` — `classify_request()` (LLM call, SingleAgent/MultiAgent)
- **Plan generation**: `task_planner.rs:112` — `generate_plan()` (LLM call with team descriptions + REQUIRES constraints)
- **Dependency resolution**: `task_planner.rs:164` — `resolve_execution_order()` → `Vec<Vec<usize>>`
- **Dependency repair**: `task_planner.rs:224` — `repair_plan_dependencies()` (auto-injects missing edges)
- **Dependency validation**: `task_planner.rs:290` — `validate_plan_dependencies()` (checks REQUIRES constraints)
- **JSON extraction**: `task_planner.rs:371` — `extract_json()` (handles markdown fences)
- **Plan parsing**: `task_planner.rs:397` — `parse_plan_json()` → `Vec<PlanTask>`

### Task Execution (Telegram)
- **Skill hot-reload**: in `orchestrate_team_goal()` per-agent rebuild
- **Dependency context**: inline output embedding (via `truncate_output`)
- **Chat runtime call**: `process_user_text()`
- **Auto-summarize**: after batch loop — stores `topic_overview` in memory with metadata
- **RAG planner recall**: before `generate_plan()` — `recall_filtered(kind=topic_overview, source=orchestrator)`

### Task Execution (CLI Orchestrator)
- **Boot**: `orchestrator.rs:70` — `boot_orchestrator()`
- **Memory init**: `build_memory_handle()` (per-workspace)
- **Agent runtimes**: `HashMap<String, Arc<AgentRuntime>>`
- **RAG planner recall**: before `generate_plan()` — `recall_filtered(kind=topic_overview, source=orchestrator)`
- **Parallel JoinSet**: batch execution via `tokio::task::JoinSet`
- **Step context**: `orchestrator.rs:700` — `build_step_context()` (uses shared `truncate_output`)
- **Auto-summarize**: after batch loop — stores `topic_overview` in memory with metadata

### Shared Channel Runtime
- **Tool rebuild**: `channel_runtime.rs:82` — `rebuild_tools()`
- **Prompt rebuild**: `channel_runtime.rs:93` — `rebuild_system_prompt()`
- **Executor build**: `channel_runtime.rs:117` — `build_tool_executor()`
- **Base tools**: `channel_runtime.rs:222` — `compute_base_tools()`
- **Memory path resolution**: `channel_runtime.rs:249` — `resolve_memory_store_path()`, `resolve_qdrant_collection()`
- **Memory init**: `channel_runtime.rs:288` — `build_memory_handle(workspace)` (per-workspace scoping)
- **Agent routing**: `channel_runtime.rs:383` — `parse_agent_routing()`
- **Message chunking**: `channel_runtime.rs:422` — `chunk_message()`
- **Output truncation**: `channel_runtime.rs:468` — `truncate_output()` (char-boundary-safe)

## Key Types

```
TelegramAgentState          telegram_runtime.rs:947
AgentRuntime                orchestrator.rs:47
PlanTask                    task_planner.rs:93
RoleDependencies            task_planner.rs:106
RouteDecision               task_planner.rs:13
StepResult                  orchestrator.rs (local)
ToolServiceExecutor         channel_runtime.rs
AgentRole(String)           domain/agent_role.rs
CapabilityId(String)        domain/capability.rs
EffectClass                 domain/capability.rs
RegisteredTool              domain/capability.rs
ToolPolicyCatalog           domain/tool_policy.rs
ToolResultEnvelope          domain/tool_result.rs
DenyByDefaultApproval       domain/approval.rs
```

## Config
- Agent definitions: `config.agents` → `HashMap<String, AgentConfig>`
- Role: `agent_config.role: Option<String>`
- Requires: `agent_config.requires: Vec<String>` (role dependencies)
- Capabilities: `agent_config.capabilities: Vec<String>`
- Skill packages: `agent_config.skill_packages: Vec<String>`
- Default agent: `agent_config.default: bool` (exactly one)
- DeSci sandbox: `sandboxes/desci/config.toml` (4 agents with requires dependencies)
- WebStudio sandbox: `sandboxes/webstudio/config.toml` (4 agents)
