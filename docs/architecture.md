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

- **Gateable** — the user decides which tools exist for each agent (`ToolAllowList`, today built from tool definitions)
- **Scopeable** — the user decides what each tool is allowed to touch (`ToolScope`, default-deny)

MCP servers extend the hands without touching Rust. Adding a tool never requires adding Rust code beyond a new plugin file or a new `[[mcp_servers]]` entry.

### The no-compromise corollary

If work during any phase is tempted to add Rust code that encodes *policy* — when to delegate, how to retry, what to prioritize, how to format output, when to ask for clarification — that code is a skill, not Rust. The test is: *does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?* If the answer is "markdown file," it's a skill.

This rule is the single sentence every PR reviewer checks against. A violation is not a style issue; it is a doctrine violation and blocks the PR.

---

## Tool Access Control

Two mechanisms coexist. They answer different questions:

| Question | Mechanism | Where |
|----------|-----------|-------|
| Does this tool exist at all for this agent? | `ToolAllowList` (coarse, binary) | `src/adapters/types.rs` |
| When the tool runs, what can it touch? | `ToolScope` (fine, default-deny) | `src/adapters/ports.rs` |

Order of checks:
1. Tool not in allow-list → tool is not registered. Done.
2. Tool in allow-list, no scope entry → tool is not registered (same effect as #1).
3. Tool in allow-list, scope present → tool is registered with that scope.

At runtime, every tool's execute body calls `scope.check_*()` as its first line.

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
- `compute_base_tools()` — workspace + platform + memory + cache tools for the outer loop
- `compute_bridge_tools()` — same set but for the MCP bridge when `manages_own_workspace = true`
- `build_tool_executor()` — composite executor wiring workspace, HTTP, crypto, cache, skill executors

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
| `tool_builder.rs` | Tool definitions + workspace executor |
| `composite_tool_executor.rs` | Composite executor dispatching to sub-executors |
| `http_tool_executor.rs` | `http_request` tool executor |
| `crypto_tool_executor.rs` | Crypto tools (sign, wallet, ABI encode) |
| `cache_tool_executor.rs` | `shared_cache` tool executor (SQLite) |
| `shell_executor.rs` | Shell command execution |
| `skill_builder.rs` | Skill parsing, registry, system prompt building |

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
