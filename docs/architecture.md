# Architecture

> **Superseded (2026-09-18):** PR #6–#8-era doc. Current architecture: `docs/architecture-2026-04-27.md` (+ `.svg` / `.html`); config: `docs/configuration.md`.
> Doctrine below (harness owns control flow; orchestrator = agent with one `memory_search` tool) was replaced by "LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" — see `CLAUDE.md`.

> Historical: every implementation plan referenced this document; every PR was reviewed against it.

Tengu is a single-binary AI agent runtime. All code lives in `src/adapters/` + `src/main.rs` — flat structure, no sub-crates.

---

## Doctrine

Three principles, checked against every PR:

### 1. The harness owns control flow

Orchestration, routing, memory retrieval, memory writes, retries, replans, cancellation, cache discipline, turn lifecycle — all of it is Rust code. No skill teaches these behaviours to an LLM, because no LLM is asked to decide them.

### 2. Agents are narrow LLM workers

An agent is an `AgentConfig` entry: a name, a system prompt, a model, a tool list, optional memory scope. Agents do not know about other agents. Agents do not decompose user requests. Agents do not spawn subagents. Each agent's conversation is a single stable system prompt + a user message + tool calls — the shape prompt caching demands.

### 3. The orchestrator is "just an agent with one tool"

The orchestrator is not a Rust class with baked-in planning logic. It is an `AgentConfig` entry like any other worker, with exactly one tool (`memory_search`) and a system prompt that teaches it to emit structured JSON plans. What makes it the orchestrator is its position in the runtime: it runs first, its output drives the DAG executor, and workers never invoke it back.

### The no-compromise corollary

If something in the codebase tries to encode per-user routing preferences, per-workflow templates, or task-specific retry strategies, stop. Those are config or agent system-prompt concerns, not Rust code. The harness owns *mechanism*, not *policy intent*. The test is: *does a non-engineer user need to change this behaviour by editing a config file (`tengu.toml`) or by filing a PR?* If the answer is "config file," it's config. If the answer is "PR," it's Rust code.

This rule is the single sentence every PR reviewer checks against. A violation is not a style issue; it is a doctrine violation and blocks the PR.

---

## Tool Access Control

Two mechanisms coexist. They answer different questions:

| Question | Mechanism | Where |
|----------|-----------|-------|
| Does this tool exist at all for this agent? | Allow-list derived from the `tools` slice passed to `build_tool_executor` (coarse, binary) | `src/bootstrap/` |
| When the tool runs, what can it touch? | `ToolScope` (fine, default-deny) | `src/domain/scope.rs` |

Order of checks:
1. Tool not in allow-list → plugin registration skips it; tool is not callable. Done.
2. Tool in allow-list → plugin registers it; `PluginToolExecutor` gives it a scope entry.
3. At call time, `Tool::execute`'s first logic line is `ctx.scope.check_*()` — enforced by `tests/scope_lint.rs`. Tools that legitimately have no resource access (e.g. `abi_encode`) declare this with a `// scope: pure-compute` annotation.

The allow-list lives in the `tools` list the caller builds and passes in; `ToolAllowList` as a type was removed in Phase A when the registry replaced the old `ToolUseService`. `ToolScope` is the per-agent fine-grained gate (see [[configuration]]).

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

### Engine trait (`src/domain/message.rs`)
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

### EngineContext (`src/domain/message.rs`)
Passed to every `engine.run()` call:
- `workspace` — filesystem path for the agent
- `system_prompt` — Tengu-composed prompt with identity, skills, memory
- `bridge_tools` — tool definitions for the [[mcp-bridge]] (Claude Code only)

### Tool Assembly (`src/bootstrap/`)
- `compute_base_tools()` — static tool defs from the workspace, http, crypto, memory, cache, and skill-lifecycle plugins for the outer loop (`skill_distill`, `shared_cache`, `persistent_store` are opt-in per agent via `workspace_tools`)
- `advertised_defs()` — tool defs for the MCP bridge when `manages_own_workspace = true`
- `build_tool_executor()` — constructs a `ToolRegistry`, registers the core plugins through `register_catalog` (agentic_memory, workspace, memory, cache, http, crypto, skill-lifecycle) plus `SkillPlugin` and `McpPlugin`, filtered by the caller's allow-list, and returns a `PluginToolExecutor`. Callers append `executor.additional_tool_defs(&tools)` to surface dynamically-discovered MCP proxy tools to the LLM.

### Skill Lifecycle (`src/application/skills/lifecycle/`, `src/adapters/outbound/tools/skill_lifecycle/`)
A harness-owned subsystem for **distillation**, **metric measurement**, and **bounded evolution** of skills. Three entry points:

- `skill_distill` (LLM-callable tool, opt-in via `workspace_tools`) — an agent authors a new skill from the current conversation. Writes `skills/<name>/{SKILL.md, evals/prompts.yaml, metrics/<scaffolds>}` atomically. Cache discipline: the new skill does NOT load into the current conversation.
- `tengu eval <skill>` (integrated into `adapters/inbound/eval.rs`) — replays `evals/prompts.yaml`, scores each row via the LLM judge AND each declared `metrics:` kind (`shell_check`, `llm_judge`, `tool_assertion`, `script`), writes rolling `metrics.json` + append-only `metrics/history.jsonl`.
- `tengu skill evolve <skill>` — bounded rewrite→rescore loop. Baseline, scratch git worktree, N cycles via the `skill-improver` agent, best-cycle selection (no regression > 0.05 on other gated metrics), user approval gate, apply-or-discard.

All three honour harness-owned doctrine: cycle counts, regression tolerance, best-cycle selection, and approval are Rust policy; LLMs only propose content. See [[skills#Metrics & Evolution]] and `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md`.

### Plugin Architecture (`src/adapters/outbound/tools/`, `src/application/tools/registry.rs`)

Every tool is a small struct implementing the async `Tool` trait. Plugins (`ToolPlugin` impls) group related tools and materialize them at registry build time. The `ToolRegistry` collects all tools and dispatches calls through `PluginToolExecutor`, which builds a per-call `ToolCtx` carrying the agent's workspace, scope, shell, HTTP client, memory handle, secret registry, and activity port.

- **Static plugins** (tool defs known at compile time): `workspace`, `http`, `crypto`, `cache`, `memory`, `skill`, `skill_lifecycle`, `agentic_memory` (feature `postgres_memory`)
- **Dynamic plugin** (tool defs discovered at boot): `mcp` — connects to each `[[mcp_servers]]` entry, calls `tools/list`, registers each remote tool as `{server}.{tool_name}`

Adding a new platform tool: write a `Tool` impl under a new `plugins/<name>/` directory, expose it through a `ToolPlugin`, and add one registration line in `adapters::outbound::tools::register_catalog` (the in-process executor and the MCP bridge both call it). No changes to the engine loop, no new executor plumbing.

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
| `adapters/outbound/engines/mod.rs` | Engine factory + OpenRouter implementation |
| `adapters/outbound/engines/claude_code.rs` | Claude Code engine (feature-gated) |
| `mcp_bridge.rs` | Stdio MCP server for tool bridging |

### Runtime
| Module | Purpose |
|--------|---------|
| `application/chat/service.rs` | ChatRuntimeService — per-turn orchestration |
| `bootstrap/` | Shared logic for all channel adapters (tool assembly, session registry) |
| `application/chat/flow.rs` | Flow/session management |
| `prompt_budget.rs` | Token budget calculation |
| `token.rs` | Token counting utilities |
| `usage.rs` | Usage/cost tracking |

### Tools
| Module | Purpose |
|--------|---------|
| `application/tools/registry.rs` | `Tool` / `ToolPlugin` traits, `ToolRegistry`, `PluginToolExecutor`, `ToolCtx`, `PluginCtx` |
| `outbound/tools/workspace/` | `read_file`, `list_directory`, `write_file`, `run_command` |
| `outbound/tools/http/` | `http_request` (async, env-var + bearer/basic auth + multipart) |
| `outbound/tools/crypto/` | Privy wallet tools: `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` |
| `outbound/tools/cache/` | `shared_cache` (SQLite, workspace-scoped, opt-in via `workspace_tools`) |
| `outbound/tools/memory/` | `memory_ingest`, `memory_search` + `persistent_store` (chunked file store, opt-in via `workspace_tools`) |
| `outbound/tools/skill/` | `SkillShellTool` — one struct reused per active shell skill |
| `outbound/mcp_client/` | Inbound MCP client (stdio + http) — proxies each remote tool as `{server}__{tool}` |
| `outbound/tools/skill_lifecycle/` | `skill_distill`, `apply_improver_proposal` (opt-in via `workspace_tools`); `compress_and_store` definition (appended implicitly to every `run-agent` subagent) |
| `skill_lifecycle/` | Metric types + 4 kinds (`shell_check`, `llm_judge`, `tool_assertion`, `script`), rolling `metrics.json` storage, `evals/prompts.yaml` fixtures, scratch-worktree helper, evolve loop, approval gate |
| `adapters/inbound/activity.rs` | Path validation + tool-activity UI helpers (no executors) |
| `adapters/outbound/shell.rs` | `LocalShellExecutor` implementing `ShellExecutionPort` |
| `application/skills/registry.rs` | Skill parsing, registry, system prompt building (no tool dispatch — that lives in `outbound/tools/skill/`) |
| `mcp_bridge.rs` | Outbound stdio MCP server (exposes Tengu tools to external Claude Code) |

### Memory
| Module | Purpose |
|--------|---------|
| `memory/` | `MemoryManager` + providers, `<memory-context>` fencing, `vector/disk.rs` (bincode store), `vector/embedder.rs` (OpenRouter `text-embedding-3-small`) |
| `outbound/tools/agentic_memory/` | Open Brain — Postgres + pgvector `agentic_memory` tool + recall lanes (feature `postgres_memory`) |

### Channel Adapters
| Module | Purpose |
|--------|---------|
| `tui/mod.rs` | Terminal UI adapter (cursive) |
| `adapters/inbound/telegram.rs` | Telegram bot adapter |

### Orchestration
| Module | Purpose |
|--------|---------|
| `orchestrator/` | `RagPlanner` (`planner.rs`), `DagExecutor` (`executor.rs`), `replan.rs`, `retry.rs`, `plan.rs`, `events.rs`, `shared_files.rs` (`TENGU_PLANNER_REGISTRY.md` + per-session plan state), `wiring.rs` (planner LLM port) |
| `runner.rs` | `SubprocessRunner` — spawns `tengu run-agent` per plan step (IPC JSON over stdin/stdout) |

### Infrastructure
| Module | Purpose |
|--------|---------|
| `adapters/outbound/secrets.rs` | Encrypted secrets vault (AES-256-GCM) |
| `scaffold.rs` | Workspace directory scaffolding |
| `prune.rs` | State/cache cleanup |

## Related
- [[engine-backends]] — detailed engine comparison
- [[configuration]] — config reference
- [[mcp-bridge]] — MCP tool bridge details
- [[skills]] — skill system architecture (incl. metrics, distillation, evolve)
- `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` — skill-lifecycle design spec (archived; live reference: `docs/skill-lifecycle-validation.md`)
