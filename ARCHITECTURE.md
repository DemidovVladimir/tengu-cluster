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
- `src/adapters`: runtime infrastructure adapters (engine factory/probes/flow-store/workspace fs/task-store/orchestrator).

## Current Applied Runtime Slices

### Domain Layer (`src/domain/`)

| File | Purpose | Status |
|------|---------|--------|
| `chat.rs` | Chat state machine, flow key resolution, history limits | Stable |
| `usage.rs` | Token usage accounting | Stable |
| `tool_policy.rs` | Tool risk-level/approval policy catalog | Stable |
| `skill.rs` | Skill markdown parsing (classic + frontmatter API), validation, rendering, agent skill filtering | Stable |
| `agent_role.rs` | Fleet agent roles (QA, BackendEngineer, IntegrationMaster) | New |
| `task.rs` | Task lifecycle model (status machine, retry logic) | New |
| `memory.rs` | Memory entry types, cosine similarity, token budgeting | New |
| `evm.rs` | EVM transaction request/receipt types (pure, no alloy) | New |

### Application Layer (`src/application/`)

| File | Purpose | Status |
|------|---------|--------|
| `ports.rs` | Port traits (FlowStore, ToolActivity/Approval/Execution, SkillSource, Shell, TaskStore, EmbeddingPort, MemoryStorePort, EvmPort) | Stable |
| `chat_commands.rs` | Slash command handler | Stable |
| `chat_runtime.rs` | Per-turn chat orchestration service | Stable |
| `engine_runtime.rs` | Engine turn loop with tool-call chaining | Stable |
| `prompt_budget.rs` | Context window budgeting and history assembly | Stable |
| `flow_compaction.rs` | Flow compaction orchestration | Stable |
| `flow_policy.rs` | Compaction policy resolution | Stable |
| `workspace_tools_catalog.rs` | Built-in workspace tool definitions | Stable |
| `tool_use_service.rs` | Tool execution with policy/activity/approval | Stable |
| `skill_catalog.rs` | Skill loading (`LoadedSkillSet`), validation, per-agent filtering, context fragment collection | Stable |
| `task_orchestrator.rs` | Task lifecycle service (create/assign/complete/retry) | Stable |
| `fleet_runtime.rs` | In-memory fleet agent registry, scheduling, per-agent prompt + tools | Stable |
| `heartbeat.rs` | Periodic heartbeat loop for stall detection | Stable |
| `memory_service.rs` | Memory application service (embed→store, embed→search→budget recall, forget) | Stable |

### Adapter Layer (`src/adapters/`)

| File | Purpose | Status |
|------|---------|--------|
| `tui/mod.rs` | Full-screen TUI with cursive, delegates to application services | Stable |
| `workspace_tools.rs` | Filesystem tool execution adapter | Stable |
| `flow_store.rs` | JSON-based flow persistence | Stable |
| `engine_factory.rs` | Engine construction from config (routes to OpenRouter, Anthropic, OpenAI, Ollama, HuggingFace, Claude Code) | Stable |
| `doctor_probe.rs` | Provider connectivity diagnostics | Stable |
| `system_prompt.rs` | Workspace system prompt file loading + skill context injection | Stable |
| `composite_tool_executor.rs` | Composite executor routing (workspace + skill tools) | Stable |
| `skill_source.rs` | Filesystem skill.md discovery | Stable |
| `skill_tool_executor.rs` | Skill execution via shell | Stable |
| `shell_executor.rs` | Local shell command execution | Stable |
| `task_store.rs` | In-memory task store implementing TaskStorePort | Stable |
| `orchestrator.rs` | Fleet orchestrator bootstrap wiring | Stable |
| `embedding.rs` | OpenRouter embedding API adapter (EmbeddingPort) — produces `Vec<f32>` vectors for storage and query | Stable |
| `memory_store.rs` | Disk-backed vector store with brute-force cosine similarity and bincode persistence (MemoryStorePort) | Stable |
| `qdrant_memory_store.rs` | Qdrant-backed vector store via gRPC, ANN cosine search (MemoryStorePort, `--features qdrant`) | Stable |
| `memory_tool_executor.rs` | Memory tool execution bridge (sync→async via dedicated runtime) | Stable |
| `secret_store.rs` | AES-256-GCM encrypted secrets vault (PBKDF2 key derivation, rpassword prompting) | Stable |
| `evm_signer.rs` | Alloy-based EVM signer adapter (EvmPort, `--features evm`) | New |
| `evm_tool_executor.rs` | EVM tool execution bridge (sync→async via dedicated runtime, `--features evm`) | New |
| `telegram_runtime.rs` | Headless Telegram bot adapter wiring TelegramPipe → ChatRuntimeService (`--features telegram`) | New |

### Channel Adapters (`crates/tengu-channels/`)

| Module | Purpose | Status |
|--------|---------|--------|
| `cli/` | Local stdin/stdout pipe | Stable |
| `telegram/` | Telegram bot pipe via teloxide (feature-gated) | New |

### Core Events (`crates/tengu-core/src/events.rs`)

| Event | Purpose | Status |
|-------|---------|--------|
| `InboundTurnReceived` | Inbound message received | Stable |
| `FlowResolved` | Flow key resolved | Stable |
| `PromptAssembled` | Prompt budget assembled | Stable |
| `EngineTurnStarted` | Engine turn started | Stable |
| `EngineTurnCompleted` | Engine turn completed | Stable |
| `EngineTurnFailed` | Engine turn failed | Stable |
| `FlowCompacted` | Flow history compacted | Stable |
| `TaskAssigned` | Task assigned to fleet agent | New |
| `TaskCompleted` | Task completed by agent | New |
| `TaskFailed` | Task failed | New |
| `HeartbeatTick` | Periodic heartbeat tick | New |
| `AgentStatusReport` | Agent status broadcast | New |

## Enforcement

Automated checks exist in:
- `tests/hex_architecture_enforcement.rs` (23 tests, including feature-gated)

Enforced invariants:
- Domain files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- Application files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- TUI delegates to `ChatRuntimeService` / `ToolUseService` (no direct engine/tool calls)
- `main.rs` uses hexagonal modules, no direct HTTP/process probing
- Orchestrator modules exist at correct layer boundaries
- `InMemoryTaskStore` implements `TaskStorePort`
- Legacy flat modules (`runtime_*.rs`, `flow_store.rs` at root) do not exist
- `DiskVectorMemoryStore` implements `MemoryStorePort`
- `QdrantMemoryStore` adapter exists and implements `MemoryStorePort`
- `OpenRouterEmbeddingAdapter` implements `EmbeddingPort`
- Memory modules exist at correct layer boundaries
- Qdrant does not leak into domain or application layers
- `AlloySigner` adapter exists and implements `EvmPort`
- `EvmToolExecutionAdapter` exists and implements `ToolExecutionPort`
- Alloy does not leak into domain or application layers
- EVM domain types are infrastructure-free
- Telegram runtime adapter delegates to `ChatRuntimeService` (no direct engine calls, `--features telegram`)

CI/local tests must stay green for architecture guardrails.
