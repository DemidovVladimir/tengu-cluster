# Architecture

> **Historical overview — banner refreshed 2026-10-02.** Read the canonical set instead: `docs/architecture-2026-04-27.md` (+ `.svg` picture, `.html` explorer) — the chat turn, `tengu run` (feeds, loops, lease), decision loops + replay, typed observations, the tool families, xmarket risk / paper and xlab backtests. Every file: `docs/code-map.md` (+ `.html`). Config: `docs/configuration.md`.
> The doctrine below (harness owns control flow; orchestrator = an agent with one `memory_search` tool) is the PR #6–#8 design, replaced by "LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" (`CLAUDE.md`). Paths, engines, trait locations, the plugin list and the module map were corrected in place on 2026-10-02; this page does not cover anything added since 2026-09-24.

> Historical: every implementation plan referenced this document; every PR was reviewed against it.

Tengu is a single-binary AI agent runtime. Since 2026-09-23 the code is hexagonal — `src/{domain,ports,config,application,bootstrap}` + `src/adapters/{inbound,outbound}`, no sub-crates; `tests/layering_lint.rs` enforces who may import whom.

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
| Does this tool exist at all for this agent? | Allow-list derived from the `tools` slice passed to `build_tool_executor` (coarse, binary) | `src/bootstrap/tools.rs` |
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
4. **collect_engine_response** runs the outer tool loop for engines that don't manage their own tools (OpenRouter, local)
5. For engines that manage their own tools (Claude Code), the tool loop runs inside the engine subprocess

## Engine Backends

Tengu supports three engine backends, selectable per agent via `engine = "..."` in config:

| Backend | Transport | Tool Loop | Workspace | Feature Flag |
|---------|-----------|-----------|-----------|-------------|
| [[engine-backends#OpenRouter\|OpenRouter]] | HTTP JSON | Tengu outer loop | Tengu tools | `openrouter` (default) |
| [[engine-backends#Local\|Local]] (Unsloth, Ollama, llama.cpp, vLLM) | OpenAI-compatible HTTP, direct (never via the egress proxy) | Tengu outer loop | Tengu tools | none (always built) |
| [[engine-backends#Claude Code\|Claude Code]] | CLI subprocess | Claude internal | Claude native + MCP bridge | `claude_code` |

See [[engine-backends]] for detailed comparison.

## Key Abstractions

### Engine trait (`src/ports/engine.rs`, abridged)
```rust
trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    fn tool_result_char_cap(&self) -> Option<usize>; // default None; LocalEngine fits its window
    fn available_models(&self) -> Vec<ModelInfo>;
    async fn run(&self, messages, tools, context) -> Stream<StreamEvent>;
}
```

The `manages_own_workspace()` flag is the key discriminator:
- `false` (OpenRouter, local): Tengu builds tools, runs the outer tool loop, manages workspace operations
- `true` (Claude Code): Engine handles workspace ops natively; Tengu tools are bridged via [[mcp-bridge]]

### EngineContext (`src/ports/engine.rs`)
Passed to every `engine.run()` call:
- `workspace` — filesystem path for the agent
- `system_prompt` — Tengu-composed prompt with identity, skills, memory
- `bridge_tools` — tool definitions for the [[mcp-bridge]] (Claude Code only)
- `max_tool_rounds`, `max_mcp_result_chars`, `mcp_servers` — the Claude Code engine's tool-call cap, bridge result cap and the `[[mcp_servers]]` it hands its bridge

### Tool Assembly (`src/bootstrap/tools.rs`, catalog in `src/adapters/outbound/tools/mod.rs`)
- `compute_base_tools()` — static tool defs for the outer loop; opt-in names (`src/domain/tools.rs::WORKSPACE_TOOLS`) join via `workspace_tools` or `tools`
- `advertised_defs()` (`adapters/outbound/tools/mod.rs`) — the catalog's tool defs an agent sees (memory gate + its opt-ins)
- `build_tool_executor()` — constructs a `ToolRegistry`, registers the catalog plugins through `register_catalog` (workspace, memory, cache, `agentic_memory` with feature `postgres_memory`, skill-lifecycle, manage_skill, http, crypto, skill_resource, view_skill, solana, hyperliquid, xm, xlab) plus `SkillPlugin` and `McpPlugin`, filtered by the caller's allow-list, and returns a `PluginToolExecutor`. Callers append `executor.additional_tool_defs(&tools)` to surface dynamically-discovered MCP proxy tools to the LLM.

### Skill Lifecycle (`src/application/skills/lifecycle/`, `src/adapters/outbound/tools/skill_lifecycle/`)
A harness-owned subsystem for **distillation**, **metric measurement**, and **bounded evolution** of skills. Three entry points:

- `skill_distill` (LLM-callable tool, opt-in via `workspace_tools`) — an agent authors a new skill from the current conversation. Writes `skills/<name>/{SKILL.md, evals/prompts.yaml, metrics/<scaffolds>}` atomically. Cache discipline: the new skill does NOT load into the current conversation.
- `tengu eval <skill>` (integrated into `adapters/inbound/eval.rs`) — replays `evals/prompts.yaml`, scores each row via the LLM judge AND each declared `metrics:` kind (`shell_check`, `llm_judge`, `tool_assertion`, `script`), writes rolling `metrics.json` + append-only `metrics/history.jsonl`.
- `tengu skill evolve <skill>` — bounded rewrite→rescore loop. Baseline, scratch git worktree, N cycles via the `skill-improver` agent, best-cycle selection (no regression > 0.05 on other gated metrics), user approval gate, apply-or-discard.

All three honour harness-owned doctrine: cycle counts, regression tolerance, best-cycle selection, and approval are Rust policy; LLMs only propose content. See [[skills#Metrics & Evolution]] and `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md`.

### Plugin Architecture (`src/adapters/outbound/tools/`, `src/application/tools/registry.rs`)

Every tool is a small struct implementing the async `Tool` trait (`src/ports/tool.rs`). Plugins (`ToolPlugin` impls) group related tools and materialize them at registry build time. The `ToolRegistry` collects all tools and dispatches calls through `PluginToolExecutor`, which builds a per-call `ToolCtx` carrying the agent's workspace, scope, shell, HTTP client, memory handle, secret registry, activity port, the conversation, the agent's config and the call id.

- **Catalog plugins** (one `ToolEntry` row per tool in `catalog()`): `workspace`, `memory`, `cache`, `agentic_memory` (feature `postgres_memory`), `skill_lifecycle`, `manage_skill`, `http`, `crypto`, `skill_resource`, `view_skill`, `solana`, `hyperliquid`, `xm`, `xlab`
- **Outside the catalog**: `skill` (`SkillPlugin`, shell skills) and `mcp` (`McpPlugin`, dynamic) — connects to each `[[mcp_servers]]` entry, calls `tools/list`, registers each remote tool as `{server}__{tool}` (a schema outside the engine subset is dropped at discovery)

Adding a new platform tool: write a `Tool` impl under `src/adapters/outbound/tools/<name>/`, expose it through a `ToolPlugin`, and add one `ToolEntry` row to `catalog()` in `adapters/outbound/tools/mod.rs` (+ the name in `WORKSPACE_TOOLS` if opt-in); the in-process executor and the MCP bridge both read it. A tool is done when its schema lint, bridge conformance case and live engine-matrix leg pass (`docs/tools.md`).

## Module Map

Paths are under `src/`. The full, test-enforced list is `docs/code-map.md`.

### Core
| Module | Purpose |
|--------|---------|
| `config/mod.rs` (+ `config/*.rs`) | TOML config schema, validation |
| `domain/message.rs` | Message, ToolCall, ToolDef, StreamEvent |
| `ports/` | Port traits for dependency inversion (`engine.rs`: Engine, EngineContext, ToolExecutor; `tool.rs`: Tool, ToolPlugin, ToolCtx; …) |

### Engines
| Module | Purpose |
|--------|---------|
| `adapters/outbound/engines/mod.rs` | Engine factory (`build_engine`, `build_step_engine`, `build_planner_engine`) |
| `adapters/outbound/engines/openrouter.rs` | OpenRouter engine |
| `adapters/outbound/engines/local.rs` | Local OpenAI-compatible engine |
| `adapters/outbound/engines/claude_code.rs` | Claude Code engine (feature-gated) |
| `adapters/inbound/mcp_bridge.rs` | Stdio MCP server for tool bridging |

### Runtime
| Module | Purpose |
|--------|---------|
| `application/chat/service.rs` | ChatRuntimeService — per-turn orchestration |
| `bootstrap/` | Composition root shared by all channel adapters (tool assembly, memory, orchestrator, sandbox) |
| `application/chat/flow.rs` | Flow/session management |
| `application/chat/prompt_budget.rs` | Token budget calculation |
| `domain/token.rs` | Token counting utilities |
| `domain/usage.rs` | Usage/cost tracking |

### Tools
| Module | Purpose |
|--------|---------|
| `application/tools/registry.rs` | `ToolRegistry`, `PluginToolExecutor` (traits `Tool` / `ToolPlugin` / `ToolCtx` / `PluginCtx` in `ports/tool.rs`) |
| `adapters/outbound/tools/workspace/` | `read_file`, `list_directory`, `write_file`, `run_command` |
| `adapters/outbound/tools/http/` | `http_request` (async, env-var + bearer/basic auth + multipart) |
| `adapters/outbound/tools/crypto/` | Privy wallet tools: `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` |
| `adapters/outbound/tools/cache/` | `shared_cache` (SQLite, workspace-scoped, opt-in via `workspace_tools`) |
| `adapters/outbound/tools/memory/` | `memory_ingest`, `memory_search` + `persistent_store` (chunked file store, opt-in via `workspace_tools`) |
| `adapters/outbound/tools/skill/` | `SkillShellTool` — one struct reused per active shell skill |
| `adapters/outbound/mcp_client/` | Outbound MCP client (stdio + http) — proxies each remote tool as `{server}__{tool}` |
| `adapters/outbound/tools/skill_lifecycle/` | `skill_distill`, `apply_improver_proposal` (opt-in via `workspace_tools`); `compress_and_store` definition (appended implicitly to every `run-agent` subagent) |
| `application/skills/lifecycle/` | Metric types + kinds (`shell_check`, `llm_judge`, `tool_assertion`, `script`, `description_trigger`, `dialog_replay`), rolling `metrics.json` storage, `evals/prompts.yaml` fixtures, scratch-worktree helper, evolve loop, approval gate |
| `adapters/inbound/activity.rs` | Tool-activity UI helpers (no executors) |
| `adapters/outbound/shell.rs` | `LocalShellExecutor` implementing `ShellExecutionPort` |
| `application/skills/registry.rs` | Skill parsing, registry, system prompt building (no tool dispatch — that lives in `adapters/outbound/tools/skill/`) |
| `adapters/outbound/tools/{solana,hyperliquid,xm,xlab}/` | added 2026-09-24 → 10-01 — see `docs/architecture-2026-04-27.md` §2.10 |

### Memory
| Module | Purpose |
|--------|---------|
| `application/memory/` + `ports/memory.rs` + `adapters/outbound/memory/` | `MemoryManager` + providers, `<memory-context>` fencing, `disk_vector.rs` (bincode store), `embedder.rs` (OpenRouter `text-embedding-3-small`) |
| `adapters/outbound/tools/agentic_memory/` | Open Brain — Postgres + pgvector `agentic_memory` tool + recall lanes (feature `postgres_memory`) |

### Channel Adapters
| Module | Purpose |
|--------|---------|
| `adapters/inbound/tui/mod.rs` | Terminal UI adapter (cursive) |
| `adapters/inbound/telegram.rs` | Telegram bot adapter |
| `adapters/inbound/webhooks.rs` | Inbound webhook listener (feature `webhooks`) |

### Orchestration
| Module | Purpose |
|--------|---------|
| `application/orchestrator/` | `RagPlanner` (`planner.rs`), `DagExecutor` (`executor.rs`), `replan.rs`, `retry.rs`, `events.rs`, `shared_files.rs` (`TENGU_PLANNER_REGISTRY.md` + per-session plan state), `wiring.rs` (planner LLM port); `Plan` types in `domain/plan.rs` |
| `adapters/outbound/subprocess_runner.rs` | `SubprocessRunner` — spawns `tengu run-agent` per plan step (IPC JSON over stdin/stdout) |

### Infrastructure
| Module | Purpose |
|--------|---------|
| `adapters/outbound/secrets.rs` | Encrypted secrets vault (AES-256-GCM) + `SanitizedToolExecutor` redaction |
| `adapters/outbound/egress.rs` | Network policy: Tor by default, host allowlist, shell sandbox, JSONL audit |
| `adapters/outbound/scaffold.rs` | Workspace directory scaffolding |
| `adapters/outbound/prune.rs` | State/cache cleanup (never `<TENGU_HOME>/state` beyond `state/flows`) |

## Related
- `docs/architecture-2026-04-27.md` (+ `.svg`, `.html`) — the current architecture
- `docs/code-map.md` — every source file, recipes
- [[engine-backends]] — detailed engine comparison
- [[configuration]] — config reference
- [[mcp-bridge]] — MCP tool bridge details
- [[skills]] — skill system architecture (incl. metrics, distillation, evolve)
- `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` — skill-lifecycle design spec (archived; live reference: `docs/skill-lifecycle-validation.md`)
