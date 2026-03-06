# Architecture Policy (Mandatory)

This repository uses **hexagonal architecture** as a hard requirement.
This is not optional.

## Layers

1. Domain
- Pure business rules and invariants.
- No UI/network/filesystem/provider code.
- No dependency on adapters.

2. Application
- Use-case orchestration.
- Depends on domain + ports only.
- No direct infrastructure calls.

3. Ports
- Inbound/outbound interfaces (traits).
- Stable contracts between application and adapters.

4. Adapters
- TUI, filesystem, backend/provider glue, Telegram, task store.
- Implement ports and translate external systems.

## Dependency Direction

- Adapters -> Ports
- Application -> Domain + Ports
- Domain -> (no adapter dependencies)

Forbidden:
- Application importing UI crates (`cursive`) or direct filesystem execution.
- TUI/business entrypoints directly calling low-level execution bypassing use-case services.
- Domain importing infrastructure/provider code.

## Mandatory Rules for New Work

1. New feature starts with a use case in `application`.
2. Every external dependency is accessed through a `port`.
3. Concrete infrastructure is implemented in `adapters`.
4. Business policies live in `domain`.
5. PRs that bypass this structure should be rejected.

## Project Mapping

- `crates/tengu-core`: shared kernel/domain contracts and core types.
- `crates/tengu-backends`: outbound adapters for provider APIs (OpenRouter, Anthropic, OpenAI, Ollama, HuggingFace, Claude Code).
- `crates/tengu-channels`: inbound/outbound channel adapters (CLI, Telegram).
- `crates/tengu-optimizer`: prompt refinement (compression, embedding, summarization).
- `src/domain`: runtime domain policies/state.
- `src/application`: runtime use-case orchestration.
- `src/adapters`: runtime infrastructure adapters (engine factory, probes, flow-store, workspace tools, task-store, Telegram, TUI, memory).
- `tools/`: standalone CLI binaries invoked by agents via `run_command` (not workspace members).

## Current Applied Runtime Slices

### Domain Layer (`src/domain/`)

| File | Purpose |
|------|---------|
| `chat.rs` | Chat state machine, flow key resolution, history limits, compaction policy |
| `usage.rs` | Token usage accounting (turn snapshots, session totals) |
| `tool_policy.rs` | Tool risk-level/approval policy catalog |
| `skill.rs` | Skill markdown parsing (classic + frontmatter API), validation, rendering, agent skill filtering |
| `agent_role.rs` | Fleet agent roles (QA, BackendEngineer, IntegrationMaster) |
| `task.rs` | Task lifecycle model (status machine, retry logic) |
| `memory.rs` | Memory entry types, cosine similarity, token budgeting |
| `secret_registry.rs` | Secret value registry for output redaction (pure, no I/O) |

### Application Layer (`src/application/`)

| File | Purpose |
|------|---------|
| `ports.rs` | Port traits: FlowStore, ToolActivity, ToolApproval, ToolExecution, SkillSource, Shell, TaskStore, Embedding, MemoryStore |
| `chat_commands.rs` | Slash command handler (/help, /cost, /context, /engine, /reset, /purge, /reload, /skills, /enable, /disable, /eco, /standard, /precise, /theme, /dark, /light) |
| `chat_runtime.rs` | Per-turn chat orchestration: memory recall, prompt budget, engine call, token budget warning (80% threshold), hard limit enforcement |
| `engine_runtime.rs` | Engine turn loop with multi-round tool-call chaining (up to 15 rounds), XML tool call fallback, tool result truncation, optional tool result observer callback |
| `prompt_budget.rs` | Context window budgeting and history assembly |
| `flow_compaction.rs` | Flow compaction orchestration |
| `flow_policy.rs` | Compaction policy resolution from config |
| `workspace_tools_catalog.rs` | Built-in tool definitions: read_file, list_directory, write_file, run_command, remember |
| `tool_use_service.rs` | Tool execution with policy check, activity publishing, and approval gate |
| `skill_catalog.rs` | Skill loading (`LoadedSkillSet`), validation, per-agent filtering, context fragment collection |
| `task_orchestrator.rs` | Task lifecycle service (create/assign/complete/retry) |
| `fleet_runtime.rs` | In-memory fleet agent registry, scheduling, per-agent prompt + tools |
| `heartbeat.rs` | Periodic heartbeat loop for stall detection |
| `memory_service.rs` | Memory application service (embed→store, embed→search→budget recall, forget) |
| `skill_registry.rs` | Mutable skill registry with hot-reload, enable/disable |

### Adapter Layer (`src/adapters/`)

| File | Purpose |
|------|---------|
| `tui/mod.rs` | Full-screen TUI with cursive, interactive tool approval dialog, delegates to application services |
| `workspace_tools.rs` | Filesystem tool execution adapter (read_file, list_directory, write_file, run_command) |
| `flow_store.rs` | JSON-based flow persistence |
| `engine_factory.rs` | Engine construction from config (routes to OpenRouter, Anthropic, OpenAI, Ollama, HuggingFace, Claude Code) |
| `doctor_probe.rs` | Provider connectivity diagnostics |
| `system_prompt.rs` | Workspace system prompt file loading + skill context injection |
| `composite_tool_executor.rs` | Vec-based composite executor routing (open for arbitrary tool executors) |
| `skill_source.rs` | Filesystem skill.md discovery |
| `skill_tool_executor.rs` | Skill execution via shell |
| `shell_executor.rs` | Local shell command execution |
| `task_store.rs` | In-memory task store implementing TaskStorePort |
| `orchestrator.rs` | Fleet orchestrator: per-agent engine/tools wiring, interactive stdin task dispatch, role-based routing, shared memory |
| `embedding.rs` | OpenRouter embedding API adapter (EmbeddingPort) — produces `Vec<f32>` vectors |
| `memory_store.rs` | Disk-backed vector store with brute-force cosine similarity and bincode persistence (MemoryStorePort) |
| `qdrant_memory_store.rs` | Qdrant-backed vector store via gRPC, ANN cosine search (MemoryStorePort, `--features qdrant`) |
| `memory_tool_executor.rs` | Memory tool execution bridge (sync→async via dedicated runtime) |
| `secret_store.rs` | AES-256-GCM encrypted secrets vault (PBKDF2 key derivation, rpassword prompting) |
| `telegram_runtime.rs` | Headless Telegram bot adapter: TelegramPipe → ChatRuntimeService, inline keyboard approval, typing indicator, file attachments, message chunking, tool result observer |
| `event_bus.rs` | In-process EventBus implementation (tokio channels) |
| `tool_bridge.rs` | Bridges core `Tool` trait to `ToolExecutionPort` for pluggable tools |

### Channel Adapters (`crates/tengu-channels/`)

| Module | Purpose |
|--------|---------|
| `cli/` | Local stdin/stdout pipe |
| `telegram/` | Telegram bot pipe via teloxide: message handling, file download, inline keyboard approval callbacks (feature-gated) |

### Core Events (`crates/tengu-core/src/events.rs`)

| Event | Purpose |
|-------|---------|
| `InboundTurnReceived` | Inbound message received |
| `FlowResolved` | Flow key resolved |
| `PromptAssembled` | Prompt budget assembled |
| `EngineTurnStarted` | Engine turn started |
| `EngineTurnCompleted` | Engine turn completed |
| `EngineTurnFailed` | Engine turn failed |
| `FlowCompacted` | Flow history compacted |
| `TaskAssigned` | Task assigned to fleet agent |
| `TaskCompleted` | Task completed by agent |
| `TaskFailed` | Task failed |
| `HeartbeatTick` | Periodic heartbeat tick |
| `AgentStatusReport` | Agent status broadcast |

## Tool Approval Flow

Tools with `requires_approval: true` go through the `ToolApprovalPort` before execution:

- **TUI mode**: Interactive dialog with tool name, description, and argument preview. User confirms with keyboard.
- **Telegram mode**: Inline keyboard message with Approve/Deny buttons. 60-second timeout (auto-deny). Callback queries update the original message with approval status.

The approval port is sync (`fn request_tool_approval(&self, call: &ToolCall) -> Result<bool>`). Telegram bridges async→sync via `block_in_place` + `Handle::block_on`.

## Token Budget System

Each conversation flow tracks cumulative token usage (`flow_token_usage`). Two thresholds:

1. **80% warning** — `ChatRuntimeService` returns a `system_notice` alerting the user of remaining budget.
2. **100% hard limit** — further requests are blocked until `/reset`.

Configured via `agents.<id>.limits.max_tokens_per_flow` (default: 100,000).

## Enforcement

Automated checks exist in:
- `tests/hex_architecture_enforcement.rs` (18 tests, including feature-gated)

Enforced invariants:
- Domain files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- Application files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- TUI delegates to `ChatRuntimeService` / `ToolUseService` (no direct engine/tool calls)
- `main.rs` uses hexagonal modules, no direct HTTP/process probing
- Orchestrator modules exist at correct layer boundaries
- `InMemoryTaskStore` implements `TaskStorePort`
- `DiskVectorMemoryStore` implements `MemoryStorePort`
- `QdrantMemoryStore` adapter exists and implements `MemoryStorePort`
- `OpenRouterEmbeddingAdapter` implements `EmbeddingPort`
- Memory modules exist at correct layer boundaries
- Qdrant does not leak into domain or application layers
- Telegram runtime adapter delegates to `ChatRuntimeService` (no direct engine calls)
- Skill registry exists at application boundary

CI/local tests must stay green for architecture guardrails.
