# Architecture

> Every phase spec (A, B, C, D) references this document. Every PR is reviewed against it.

Tengu is a single-binary AI agent runtime. All code lives in `src/adapters/` + `src/main.rs` — flat structure, no sub-crates.

---

## Doctrine

The Rust core exists to serve three principles. Violating any of them is a doctrine violation that blocks the PR.

### 1. LLM is the heart

It consumes tokens and emits tokens. It has no behaviour of its own — no memory, no goals, no identity, no plans. Anything that looks like "the agent did X because..." is really "the context instructed the LLM, and the LLM produced X." The Rust core never hard-codes behaviour that belongs to the model.

### 2. Context and skills are the brain

Everything the LLM knows on a given turn lives in the context window: system prompt, bootstrap files (AGENTS.md, MEMORY.md, daily logs, identity files), tool definitions, skill catalog entries, transcript history, pending tool results.

Skills are how policy reaches the brain — any strategy, workflow, playbook, plan, or "how the agent decides what to do" is a skill, and each skill materializes into context via frontmatter catalog entries (compact) and body text (loaded on demand). Orchestration. Decomposition. Delegation. Failure handling. Progress tracking. Even meta-behaviour like "how to write new skills" is a skill (`skill-creator`).

The Rust core's job is **brain assembly** — deciding which skills + bootstrap + transcript enter the context, in what order, at what compression, and what to do when the window overflows (Phase D's RAG spill). The core does not decide what the brain does with that context.

Skills evolve through `skill-creator` (create), `skill-eval` (measure), and `skill-improver` (propose edits from align reports — Phase E). The harness gets closer to the user over time because the brain does, not because the core does.

### 3. Tools and MCP are the hands and senses

They are the only way the LLM touches the world. A tool reads a file, writes a file, runs a command, signs a transaction, calls an HTTP API, spawns a subagent. Tools are:

- **Gateable** — the user decides which tools exist for each agent. The per-agent tool list is computed in `channel_runtime::compute_base_tools` + skill tools + (when enabled) `compute_subagent_tools`, and passed to `build_tool_executor` as its allow-list. Tools whose names are not in the list are not registered.
- **Scopeable** — the user decides what each tool is allowed to touch (`ToolScope`, default-deny). Every registered tool gets a per-agent scope entry; every `Tool::execute` body calls `scope.check_*()` as its first logic line, enforced structurally by `tests/scope_lint.rs`.

MCP servers extend the hands without touching Rust. Adding a tool never requires adding Rust code beyond a new plugin file or a new `[[mcp_servers]]` entry.

### The no-compromise corollary

If work during any phase is tempted to add Rust code that encodes *policy* — when to delegate, how to retry, what to prioritize, how to format output, when to ask for clarification — that code is a skill, not Rust. The test is: *does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?* If the answer is "markdown file," it's a skill.

This rule is the single sentence every PR reviewer checks against. A violation is not a style issue; it is a doctrine violation and blocks the PR.

---

## Tool Access Control

Two mechanisms coexist. They answer different questions:

| Question | Mechanism | Where |
|----------|-----------|-------|
| Does this tool exist at all for this agent? | Allow-list derived from the `tools` slice passed to `build_tool_executor` (coarse, binary) | `src/adapters/channel_runtime.rs` |
| When the tool runs, what can it touch? | `ToolScope` (fine, default-deny) | `src/adapters/ports.rs` |

Order of checks:
1. Tool not in allow-list → plugin registration skips it; tool is not callable. Done.
2. Tool in allow-list → plugin registers it; `PluginToolExecutor` gives it a scope entry.
3. At call time, `Tool::execute`'s first logic line is `ctx.scope.check_*()` — enforced by `tests/scope_lint.rs`. Tools that legitimately have no resource access (e.g. `abi_encode`) declare this with a `// scope: pure-compute` annotation.

The allow-list lives in the `tools` list the caller builds and passes in; `ToolAllowList` as a type was removed in Phase A when the registry replaced the old `ToolUseService`. `ToolScope` is the per-agent fine-grained gate (see [[configuration#Scopes]]).

---

## Core Loop

```
Channel (TUI/Telegram) → ChatRuntimeService → Engine → StreamEvent → Response
                              ↓                  ↑
                         ToolExecutor ←── collect_engine_response (tool loop)
```

1. A **channel adapter** (TUI, Telegram) receives user input
2. **ChatRuntimeService** handles memory recall, prompt budgeting, history management
3. The **engine** processes the prompt and returns `StreamEvent`s
4. **collect_engine_response** runs the outer tool loop for engines that don't manage their own tools (OpenRouter)
5. For engines that manage their own tools (Claude Code), the tool loop runs inside the engine subprocess

## Engine Backends

Tengu supports two engine backends, selectable per agent via `engine = "..."` in config:

| Backend | Transport | Tool Loop | Workspace | Feature Flag |
|---------|-----------|-----------|-----------|-------------|
| [[engine-backends#OpenRouter|OpenRouter]] | HTTP JSON | Tengu outer loop | Tengu tools | `openrouter` (default) |
| [[engine-backends#Claude Code|Claude Code]] | CLI subprocess | Claude internal | Claude native + MCP bridge | `claude_code` |

See [[engine-backends]] for detailed comparison.

## Key Abstractions

### Engine trait (`src/adapters/types.rs`)
```rust
trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    async fn run(&self, messages, tools, context) -> Stream<StreamEvent>;
}
```

The `manages_own_workspace()` flag is the key discriminator:
- `false` (OpenRouter): Tengu builds tools, runs the outer tool loop, manages workspace operations
- `true` (Claude Code): Engine handles workspace ops natively; Tengu tools are bridged via [[mcp-bridge]]

### EngineContext (`src/adapters/types.rs`)
Passed to every `engine.run()` call:
- `workspace` — filesystem path for the agent
- `system_prompt` — Tengu-composed prompt with identity, skills, memory
- `bridge_tools` — tool definitions for the [[mcp-bridge]] (Claude Code only)

### Tool Assembly (`src/adapters/channel_runtime.rs`)
- `compute_base_tools()` — static tool defs from the workspace, http, crypto, memory, cache plugins for the outer loop
- `compute_subagent_tools()` — subagent-spawn tool defs, added when the orchestrator is enabled
- `compute_bridge_tools()` — tool defs for the MCP bridge when `manages_own_workspace = true`
- `build_tool_executor()` — constructs a `ToolRegistry`, registers each plugin (workspace, skill, memory, cache, http, crypto, subagents, mcp) filtered by the caller's allow-list, and returns a `PluginToolExecutor`. Callers append `executor.additional_tool_defs(&tools)` to surface dynamically-discovered MCP proxy tools to the LLM.

### Plugin Architecture (`src/adapters/plugins/`, `src/adapters/tool_plugin.rs`)

Every tool is a small struct implementing the async `Tool` trait. Plugins (`ToolPlugin` impls) group related tools and materialize them at registry build time. The `ToolRegistry` collects all tools and dispatches calls through `PluginToolExecutor`, which builds a per-call `ToolCtx` carrying the agent's workspace, scope, shell, HTTP client, memory handle, secret registry, activity port, and subagent registry.

- **Static plugins** (tool defs known at compile time): `workspace`, `http`, `crypto`, `cache`, `memory`, `skill`, `subagents`
- **Dynamic plugin** (tool defs discovered at boot): `mcp` — connects to each `[[mcp_servers]]` entry, calls `tools/list`, registers each remote tool as `{server}.{tool_name}`

Adding a new platform tool: write a `Tool` impl under a new `plugins/<name>/` directory, expose it through a `ToolPlugin`, and register it in `channel_runtime::build_tool_executor`. No changes to the engine loop, no new executor plumbing.

## Module Map

### Core
| Module | Purpose |
|--------|---------|
| `config.rs` | TOML config schema, validation |
| `types.rs` | Engine trait, Message, ToolCall, StreamEvent, EngineContext |
| `ports.rs` | Port traits for dependency inversion |

### Engines
| Module | Purpose |
|--------|---------|
| `engine_builder.rs` | Engine factory + OpenRouter implementation |
| `claude_code_engine.rs` | Claude Code engine (feature-gated) |
| `mcp_bridge.rs` | Stdio MCP server for tool bridging |

### Runtime
| Module | Purpose |
|--------|---------|
| `chat_builder.rs` | ChatRuntimeService — per-turn orchestration |
| `channel_runtime.rs` | Shared logic for all channel adapters (tool assembly, session registry) |
| `flow_builder.rs` | Flow/session management |
| `prompt_budget.rs` | Token budget calculation |
| `token.rs` | Token counting utilities |
| `usage.rs` | Usage/cost tracking |

### Tools
| Module | Purpose |
|--------|---------|
| `tool_plugin.rs` | `Tool` / `ToolPlugin` traits, `ToolRegistry`, `PluginToolExecutor`, `ToolCtx`, `PluginCtx` |
| `plugins/workspace/` | `read_file`, `list_directory`, `write_file`, `run_command` |
| `plugins/http/` | `http_request` (async, env-var + bearer/basic auth + multipart) |
| `plugins/crypto/` | Privy wallet tools: `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` |
| `plugins/cache/` | `shared_cache` (SQLite, workspace-scoped, opt-in via `workspace_tools`) |
| `plugins/memory/` | `remember` + `persistent_store` (chunked RAG, opt-in via `workspace_tools`) |
| `plugins/skill/` | `SkillShellTool` — one struct reused per active shell skill |
| `plugins/subagents/` | `sessions_spawn`, `sessions_fan_out`, `subagents` — registered when orchestrator is enabled |
| `plugins/mcp/` | Inbound MCP client (stdio + http) — proxies each remote tool as `{server}.{tool}` |
| `tool_builder.rs` | Path validation + tool-activity UI helpers (no executors) |
| `shell_executor.rs` | `LocalShellExecutor` implementing `ShellExecutionPort` |
| `skill_builder.rs` | Skill parsing, registry, system prompt building (no tool dispatch — that lives in `plugins/skill/`) |
| `mcp_bridge.rs` | Outbound stdio MCP server (exposes Tengu tools to external Claude Code) |

### Memory
| Module | Purpose |
|--------|---------|
| `memory_builder.rs` | Memory types, service, disk store, tool defs |
| `embedding.rs` | Embedding generation (OpenRouter API) |
| `qdrant_memory_store.rs` | Qdrant vector store backend (feature-gated) |

### Channel Adapters
| Module | Purpose |
|--------|---------|
| `tui/mod.rs` | Terminal UI adapter (cursive) |
| `telegram_builder.rs` | Telegram bot adapter |

### Orchestration
| Module | Purpose |
|--------|---------|
| `orchestrator.rs` | Multi-agent fleet orchestrator (CLI) |
| `event_orchestrator.rs` | Event-based orchestrator (Telegram) |
| `agent_builder.rs` | Agent worker loop (Telegram) |
| `task_builder.rs` | Task/plan management (Telegram) |

### Infrastructure
| Module | Purpose |
|--------|---------|
| `secret_builder.rs` | Encrypted secrets vault (AES-256-GCM) |
| `scaffold.rs` | Workspace directory scaffolding |
| `prune.rs` | State/cache cleanup |

## Related
- [[engine-backends]] — detailed engine comparison
- [[configuration]] — config reference
- [[mcp-bridge]] — MCP tool bridge details
- [[skills]] — skill system architecture
