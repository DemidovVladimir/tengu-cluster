# Code Map — Multi-Agent Routing

## Critical Paths

### Plain Message Routing (Telegram)
- **Entry**: `telegram_runtime.rs:1392-1424` — route decision
- **Parser**: `channel_runtime.rs:350-379` — `parse_agent_routing()` (@role: and role: formats)
- **Multi-agent flag**: `telegram_runtime.rs:921` — `is_multi_agent = agent_states.len() > 1`
- **Orchestration**: `telegram_runtime.rs:278-620` — `orchestrate_team_goal()`
- **Direct routing**: `telegram_runtime.rs:1426-1449` — explicit @role: fallback to default

### Planning
- **Plan generation**: `task_planner.rs:28-75` — `generate_plan()` (LLM call with team descriptions)
- **Dependency resolution**: `task_planner.rs:79-120` — `resolve_execution_order()` → `Vec<Vec<usize>>`
- **JSON extraction**: `task_planner.rs:123-146` — `extract_json()` (handles markdown fences)
- **Plan parsing**: `task_planner.rs:149-194` — `parse_plan_json()` → `Vec<PlanTask>`
- **Planner prompt**: `task_planner.rs:38-57` — system prompt for task decomposition

### Task Execution (Telegram)
- **Role→agent lookup**: `telegram_runtime.rs:439-450`
- **Skill hot-reload**: `telegram_runtime.rs:471-485`
- **Executor build**: `telegram_runtime.rs:487-501`
- **Dependency context**: `telegram_runtime.rs:520-535` — outcome file paths
- **Chat runtime call**: `telegram_runtime.rs:553-571` — `process_user_text()`
- **Result handling**: `telegram_runtime.rs:574-602`

### Task Execution (CLI Orchestrator)
- **Boot**: `orchestrator.rs:69-545` — `boot_orchestrator()`
- **Agent runtimes**: `orchestrator.rs:119-250` — `HashMap<String, Arc<AgentRuntime>>`
- **Parallel JoinSet**: `orchestrator.rs:441-533` — batch execution
- **Step context**: `orchestrator.rs:582-611` — `build_step_context()` (in-memory, not file-based)

### Shared Channel Runtime
- **Tool rebuild**: `channel_runtime.rs:81-89` — `rebuild_tools()`
- **Prompt rebuild**: `channel_runtime.rs:92-109` — `rebuild_system_prompt()`
- **Executor build**: `channel_runtime.rs:116-211` — `build_tool_executor()`
- **Base tools**: `channel_runtime.rs:221-240` — `compute_base_tools()`
- **Agent routing**: `channel_runtime.rs:350-379` — `parse_agent_routing()`
- **Message chunking**: `channel_runtime.rs:389-425` — `chunk_message()`

## Key Types

```
TelegramAgentState          telegram_runtime.rs:646-662
AgentRuntime                orchestrator.rs:46-52
PlanTask                    task_planner.rs:13-22
StepResult                  orchestrator.rs (local)
ToolServiceExecutor         channel_runtime.rs
AgentRole(String)           domain/agent_role.rs
CapabilityId(String)        domain/capability.rs
EffectClass                 domain/capability.rs
RegisteredTool              domain/capability.rs
ToolPolicyCatalog           domain/tool_policy.rs
DenyByDefaultApproval       domain/approval.rs
```

## Config
- Agent definitions: `config.agents` → `HashMap<String, AgentConfig>`
- Role: `agent_config.role: Option<String>`
- Capabilities: `agent_config.capabilities: Vec<String>`
- Skill packages: `agent_config.skill_packages: Vec<String>`
- Default agent: `agent_config.default: bool` (exactly one)
- DeSci sandbox: `sandboxes/desci/config.toml` (4 agents)
- WebStudio sandbox: `sandboxes/webstudio/config.toml` (5 agents)
