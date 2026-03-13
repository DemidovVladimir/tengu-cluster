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
- `tools/`: standalone infrastructure (services, CLIs) used by agents via skills and workspace primitives (not workspace members, no code changes to tengu-cluster required).

## Current Applied Runtime Slices

### Domain Layer (`src/domain/`)

| File | Purpose |
|------|---------|
| `chat.rs` | Chat state machine, flow key resolution, history limits, compaction policy |
| `usage.rs` | Token usage accounting (turn snapshots, session totals) |
| `tool_policy.rs` | Tool risk-level/approval policy catalog |
| `skill.rs` | Skill markdown parsing (classic + frontmatter API), validation, rendering, agent skill filtering |
| `agent_role.rs` | Fleet agent roles — dynamic string wrapper (any non-empty role name) |
| `task.rs` | Task lifecycle model (status machine, retry logic) |
| `memory.rs` | Memory entry types (with metadata `HashMap<String, String>`), cosine similarity, token budgeting |
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
| `workspace_tools_catalog.rs` | Built-in workspace primitive definitions: read_file, list_directory, write_file, run_command |
| `tool_use_service.rs` | Tool execution with policy check, activity publishing, and approval gate |
| `skill_catalog.rs` | Skill loading (`LoadedSkillSet`), validation, per-agent filtering, context fragment collection |
| `task_orchestrator.rs` | Task lifecycle service (create/assign/complete/retry) |
| `memory_service.rs` | Memory application service (embed→store with metadata, embed→search→budget recall, metadata-filtered recall, forget) |
| `skill_registry.rs` | Mutable skill registry with hot-reload, enable/disable |

### Adapter Layer (`src/adapters/`)

| File | Purpose |
|------|---------|
| `channel_runtime.rs` | Shared channel runtime helpers: tool/executor/prompt rebuilding, per-workspace memory init (`resolve_memory_store_path`, `resolve_qdrant_collection`), output truncation, agent routing, message chunking, state factories — all channel adapters delegate here |
| `tui/mod.rs` | Full-screen TUI with cursive, interactive tool approval dialog, delegates to channel_runtime + application services |
| `workspace_tools.rs` | Filesystem tool execution adapter (read_file, list_directory, write_file, run_command) |
| `flow_store.rs` | JSON-based flow persistence |
| `engine_factory.rs` | Engine construction from config (routes to OpenRouter, Anthropic, OpenAI, Ollama, HuggingFace, Claude Code) |
| `doctor_probe.rs` | Provider connectivity diagnostics |
| `system_prompt.rs` | System prompt builder — generates tool listing dynamically from ToolDef metadata |
| `composite_tool_executor.rs` | Vec-based composite executor routing (open for arbitrary tool executors) |
| `skill_source.rs` | Filesystem skill.md discovery |
| `skill_tool_executor.rs` | Skill execution via shell |
| `shell_executor.rs` | Local shell command execution |
| `task_store.rs` | In-memory task store implementing TaskStorePort |
| `orchestrator.rs` | Fleet orchestrator: per-agent engine/tools wiring, interactive stdin task dispatch, role-based routing, shared per-workspace memory (via `build_memory_handle`), JoinSet parallel batch execution, auto-summarize topic overviews, RAG planner recall |
| `embedding.rs` | OpenRouter embedding API adapter (EmbeddingPort) — produces `Vec<f32>` vectors |
| `memory_store.rs` | Disk-backed vector store with brute-force cosine similarity and bincode persistence (MemoryStorePort) |
| `qdrant_memory_store.rs` | Qdrant-backed vector store via gRPC, ANN cosine search (MemoryStorePort, `--features qdrant`) |
| `memory_tool_executor.rs` | Memory tool definitions + execution bridge (owns `remember` ToolDef with optional metadata parameter, sync→async via dedicated runtime) |
| `tool_ui.rs` | Shared generic UI helpers for tool approval dialogs and activity summaries (no tool name matching) |
| `secret_store.rs` | AES-256-GCM encrypted secrets vault (PBKDF2 key derivation, rpassword prompting) |
| `telegram_runtime.rs` | Headless Telegram bot adapter: TelegramPipe → ChatRuntimeService, inline keyboard approval, typing indicator, file attachments, inline inter-agent data passing, auto-summarize topic overviews, RAG planner recall, delegates to channel_runtime for shared logic |

### Channel Adapters (`crates/tengu-channels/`)

| Module | Purpose |
|--------|---------|
| `cli/` | Local stdin/stdout pipe |
| `telegram/` | Telegram bot pipe via teloxide: message handling, file download, inline keyboard approval callbacks (feature-gated) |

## Composability Principles

The system is designed for maximum composability — adding new capabilities should never require code changes to tengu-cluster:

- **Skills** (knowledge/instructions) — drop a `SKILL.md` file into `skills/`, it's immediately available. No code changes.
- **Tools** (infrastructure/capability) — standalone services, CLIs, or libraries in `tools/`. Agents interact with them via skills and workspace primitives. No code changes.
- **Channels** (communication) — telegram, TUI, future slack/discord/email. Isolated via port traits. Application and domain layers are channel-agnostic.
- **4 Workspace Primitives** — `read_file`, `list_directory`, `write_file`, `run_command`. These are the stable "syscall" layer through which agents interact with all external tools.

Each subsystem (memory, skills, etc.) owns its own tool definitions. The system prompt tool listing is generated dynamically from `ToolDef` metadata. Approval dialogs work generically from risk level and description, not hardcoded tool names.

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
- `tests/hex_architecture_enforcement.rs` (5 tests)

Enforced invariants:
- Domain files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- Application files contain no `reqwest`, `cursive`, `std::fs`, `tokio::process`
- `main.rs` uses hexagonal modules, no direct HTTP/process probing
- Module layout exists (`domain/`, `application/`, `adapters/`)

CI/local tests must stay green for architecture guardrails.
