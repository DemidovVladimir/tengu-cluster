# Tengu-Cluster — Architecture Walkthrough (2026-04-27)

> Companion to `docs/architecture-2026-04-27.svg`. The SVG is the picture;
> this is the line-by-line read. If you only have five minutes, read the
> seven steps in §1. If you have an hour, follow the file references.

---

## TL;DR — the entire system in five sentences

A user message arrives at a channel (TUI / Telegram). The harness builds an `Orchestrator`, which holds `RagPlanner` (legacy name for the LLM planner) and a `SubprocessRunner` (which executes plan steps as child processes). The planner regenerates and loads root `TENGU_PLANNER_REGISTRY.md` (`[agents.*]` with a `description` · skills · tools) into the planner prompt, then asks an LLM to emit either a direct response or a plan referencing that roster. Each plan step spawns `tengu run-agent` as a subprocess, which re-loads the parent's config (`sandboxes/<name>/config.toml`), takes its `[agents.<name>]` block, receives the accepted plan via `AgentIpcInput.plan_state`, builds a tool executor, and runs an LLM-with-tools loop until the model calls `compress_and_store` (the implicit "I'm done" tool). Every network path — tools, LLM APIs, the Claude Code CLI, Telegram — goes through the `[egress]` policy (`egress.rs`), which is Tor by default. Step output flows back to the executor; on failure the executor calls `RagPlanner::replan`, which uses Open Brain-style Postgres `agentic_memory` cross-plan recall when `postgres_memory` is enabled.

That's it. Everything else is detail.

---

## §1 — The seven steps from prompt to reply

Trace through them in order. Every step has a file you can open.

### 1. User input → Channel

The user types into the TUI (`src/adapters/tui/mod.rs`) or sends a Telegram message (`src/adapters/telegram_builder.rs`). Both call into the same channel runtime layer.

### 2. Channel runtime → Orchestrator

`src/adapters/channel_runtime.rs::build_orchestrator` is the function that wires everything together. It:
- Constructs `ChatOrchestratorPortImpl` (the LLM-call abstraction the planner uses).
- Mints an `EventBus` (so the TUI can subscribe to plan progress events).
- Builds `RagPlanner` (legacy name; planner side) and `SubprocessRunner` (worker side). Both receive `config.agents` — the planner to render the registry, the runner for fail-fast on unknown agents and per-step `limits.max_tool_rounds` / `limits.step_timeout_secs`.
- Returns an `Orchestrator` that wraps them.

The default-agent dispatch (no orchestrator at all) is the fallback when `[orchestrator]` is missing from the sandbox config. Post Phase 7.1, `engine = "rag"` is the only supported orchestrator engine; the value is historical and now means file-registry planner + Open Brain memory.

### 3. Planner — `RagPlanner::plan`

`src/adapters/orchestrator/planner.rs`. This is where the doctrine lives ("LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands"). For each user message:

1. **Load the planner registry** — regenerate root `TENGU_PLANNER_REGISTRY.md` (`shared_files::ensure_planner_registry`) from the `[agents.*]` blocks that carry a `description` (`routable_agents`, with `example_queries`), skills, and core + MCP tool definitions, then inject it into the planner prompt.
2. **Emit `RagQueried` event** — registry entries go to the event bus + a tracing line. The TUI debug panel renders these when `TENGU_TUI_RAG_DEBUG=1`.
3. **Open Brain recall (opt-in)** — if `memory.cross_session_msg_top_k > 0`, retrieve prior user questions from Postgres `agentic_memory` when `postgres_memory` is enabled and inject them as a "Cross-session message recall" block. **This happens BEFORE persisting the current message** so the just-written message can never appear in its own recall block.
4. **Persist the user message** — `persist_user_message(user_message)` writes keyed by `session_id` to Postgres `agentic_memory` with `postgres_memory`.
5. **Push to in-memory buffer** — Phase 6.4 lite. Same data, but kept in-process for the "Recent user messages this session" prompt block.
6. **Build the prompt** — registry file + cross_session_block + history_block + the current user message.
7. **LLM call with `skills/orchestrator/SKILL.md` as system prompt** — `run_orchestrator_turn_with_system` strips tools/memory/grounding so the LLM has exactly one job: emit plan JSON. Output validated against `skills/orchestrator/plan_schema.json`.
8. **Parse verdict** — either `PlannerVerdict::Direct { response }` (return it to user) or `PlannerVerdict::Plan { steps }` (hand to executor).

### 4. Executor — DagExecutor

`src/adapters/orchestrator/executor.rs`. Standard topological-sort-then-parallel pattern:
- `Plan::ready_steps` returns every step whose `depends_on` is satisfied.
- Each ready step is `tokio::spawn`'d on the worker (`SubprocessRunner`).
- Retries follow `RetryPolicy::new(max_attempts_per_step)` from `retry.rs`.
- On exhaustion, `replan.rs::drive` calls `RagPlanner::replan` with the failed step + error context. With `postgres_memory`, replan also pulls `cross_plan_top_k` hits from Postgres `agentic_memory`.
- Every accepted plan/replan is rendered once: stored per session (`shared_files::set_active_plan`) so `SubprocessRunner` can pass it as `AgentIpcInput.plan_state`, and written to root `TENGU_PLAN.md` as a human-readable debug artifact (fallback for old parents only).

### 5. Subprocess — `SubprocessRunner::run_step`

`src/adapters/runner.rs`. For each step:
- Fails fast when `step.agent` (or `compose.base_agent`) has no `[agents.<name>]` block in the parent config — no spawn, no retries burned.
- Builds `AgentIpcInput` JSON from the step (agent name, goal, session_id, step_id, `max_turns` = the agent's `limits.max_tool_rounds`, optional `compose` for C→B fallback, **`sandbox_config`** so the child sees the same scopes as the parent — Phase 7.2, **`plan_state`** — the rendered plan for this session, 2026-09-12). `model` / `tools` / `skills` travel empty — the child reads them from its own `[agents.<name>]`.
- Spawns `tengu run-agent` as a child process with `TENGU_AGENT_IPC=1` env guard and `TENGU_EGRESS` (the parent's *resolved* `[egress]` policy — the child applies it verbatim, 2026-09-16).
- Pipes the JSON over stdin; reads the result JSON from stdout; kill on drop. Wall-clock cap per step = `limits.step_timeout_secs` (default 600) via `run_with_timeout`.
- Returns `AgentIpcOutput::Ok { output, summary }` or `Failed { error, output }`.

### 6. Subagent tool loop (in the child)

`src/main.rs::run_agent_subprocess` is the entry point of the child process:
1. Reads stdin, parses `AgentIpcInput`; exports `TENGU_SESSION_ID`.
2. **Phase 7.2 config resolution — FIRST** — `load_sandbox_or(input.sandbox_config, default config)`: `sandboxes/<name>/config.toml` when the parent ran with `--sandbox`, else the default chain. Scopes/secrets/MCP servers therefore match the parent. Then `egress::install(&parent_config.egress)` — `TENGU_EGRESS` wins over the file, an invalid policy aborts the child.
3. **Agent resolution** — `parent_config.agents.get(name)` where `name` = `compose.base_agent` if composed, else `input.agent_name`. Unknown name → hard error listing the configured agents. Composed runs overwrite `skill_packages` / `tools` in memory only. Exports `TENGU_AGENT_NAME`.
4. **System prompt assembly** — base template + each skill body (via `load_skill_body_three_tier`) + `input.plan_state` (falls back to root `TENGU_PLAN.md`) + mandatory suffix. Per-tool scopes come from the folded `[default_scopes]` plus the child's own workspace in `fs_roots`.
5. **Tool executor** — `channel_runtime::build_subprocess_tool_executor(&AgentConfig, ..)` builds `PluginToolExecutor` from `(base tools ∩ agent.tools) ∪ {compress_and_store}` over the plugin registry (http, workspace, memory, crypto, mcp, etc.); `subagent_config` first merges the workspace-tool names in `tools` into `workspace_tools`. Its HTTP client comes from `egress::policy().tool_client` (proxy, redirects off); `http_request` gates hop 0 (egress + scope) inside `execute` and re-checks every redirect hop; `run_command` runs under the `[egress]` shell mode — see §4 K.
6. **Multi-turn loop** — `run_single_engine_turn` repeatedly until the model calls `compress_and_store` OR `max_turns` (= `limits.max_tool_rounds`) exhausted. Out-of-band detection of `compress_and_store` to commit step result. `engine = "claude_code"` agents get `EngineContext.bridge_tools` so the CLI sees tengu tools over the MCP bridge.

### 7. `compress_and_store` — durable step summary

`src/adapters/plugins/skill_lifecycle/compress_and_store.rs` owns only the tool *definition*. The model's "I'm done" call. `run-agent` intercepts the tool call, captures the summary, and exits cleanly. With `postgres_memory`, `main.rs::try_persist_agentic_step_summary` writes the final summary to Postgres `agentic_memory` with embeddings when available and text-only fallback otherwise; without it the summary only travels back over IPC.

Output flows back to the executor (step 4); on success the next ready step starts.

### Across all steps — metrics emission (added 2026-04-28)

Every step that calls an LLM or embedding API emits a `MetricsRecord` via
`adapters::metrics::record()`:
- Step 3 (planner): `emit_planner_metrics` builds a record with full layer
  breakdown after the LLM responds.
- Step 6 (subagent): `run_agent_subprocess` records one per
  `run_single_engine_turn` and ships them in `AgentIpcOutput.metrics`.
  `SubprocessRunner::run_step` re-emits each on the parent's global sink.
- Embedder: every `embed_batch_openrouter` call reads `usage.total_tokens`
  and emits a record.

A bridge task (spawned in `build_orchestrator`) subscribes to the global
metrics sink and republishes each record as `OrchestratorEvent::MetricsRecorded`
on the orchestrator bus, so subscribers (TUI, eval recorder, future ones)
see one unified event stream.

---

## §2 — The six subsystems (file-by-file)

### `src/adapters/memory/` — low-level vector store

This is what Phase 7.1's doc rewrite makes explicit: `memory/` is the foundation everything else builds on. Current agentic memory uses Open Brain-style Postgres tables plus Karpathy LLM Wiki Markdown; the older vector/provider layer remains for compatibility and chat-side memory plumbing.

| File | What it owns |
|---|---|
| `vector.rs` | `VectorStore` trait — write/search/delete/clear_all/entry_count/storage_bytes/`delete_older_than` |
| `vector/disk.rs` | Bincode store — the only `VectorStore` impl |
| `vector/embedder.rs` | OpenAI `text-embedding-3-small` client (1536d) |
| `manager.rs` | `MemoryManager` — provider holder, prefetch_all/sync_all coordinator |
| `provider.rs` | `MemoryProvider` trait |
| `builtin.rs` | `BuiltinMemoryProvider` — MEMORY.md, identity, daily logs, vector |
| `injector.rs` | Pre-turn fenced context injection. **Phase 7.1**: only `ChatOrchestratorPortImpl` calls this. |
| `writer.rs` | Post-turn spawned non-blocking memory writes. Same Phase 7.1 note. |
| `fencing.rs` | `<memory-context>` block helpers |
| `context_block.rs` | Shared types — `MemoryHit`, `ChunkMetadata` |

> Durable runtime memory is the `agentic_memory` plugin (Open Brain — Postgres
> + pgvector, `postgres_memory` feature); planner routing is file-backed via
> root `TENGU_PLANNER_REGISTRY.md`. See `docs/agentic-memory-*.md`.

### `[agents.*]` — subagents live in the sandbox config

No `agents/` directory, no separate spec type. A subagent is an `[agents.<name>]` block with a `description`.

| Item | Where |
|---|---|
| Schema | `src/adapters/config.rs::AgentConfig` — one struct for in-process agents and subagents |
| Routable ⇔ `description` set | `orchestrator/shared_files.rs::routable_agents` renders those blocks (+ `example_queries`) into `TENGU_PLANNER_REGISTRY.md` |
| Subagent fields | `engine` (`openrouter` \| `claude_code`), `model`, `description`, `example_queries`, `tools` (allow-list; workspace-tool names opt in), `skill_packages` (`skills` alias), `workspace`, `scopes`, `limits.max_tool_rounds` (turn cap per step), `limits.step_timeout_secs` (default 600), `claude_code` |
| Child lookup | `main.rs::run_agent_subprocess` loads the parent config first, then `config.agents.get(name)` (`compose.base_agent` when composed) |
| Example | `sandboxes/aura/config.toml`: `aura` (DeSci pipeline), `researcher` (web/HTTP research), `learning-agent` (skill lifecycle) |

Edit a block + restart chat → the planner registry file is regenerated on the next planner turn. No rebuild required.

### `skills/` — prompt fragments + frontmatter

Three-tier scanner:
1. `~/.tengu/skills/` (managed)
2. `<workspace>/.tengu/skills/` (workspace dotdir)
3. `<workspace>/skills/` (workspace root — highest priority)

Walks all three, parses each `SKILL.md`'s YAML frontmatter (`name`, `description`), dedupes by `name`. Implementation: `orchestrator/shared_files.rs::scan_skill_summaries`.

`skills/orchestrator/SKILL.md` is special — used as the planner system prompt (Phase 4c). Its companion `plan_schema.json` is the JSON schema the planner output must validate against.

### `src/adapters/orchestrator/` — planner + executor

| File | What it owns |
|---|---|
| `mod.rs` | `Orchestrator` (top-level handle) |
| `planner.rs` | `RagPlanner` legacy type name (only `Planner` impl since 7.1), free fn `parse_verdict`, `cross_session_recall_block`, `emit_planner_metrics` |
| `plan.rs` | `Plan`, `Step`, `AgentCompose` (C→B B-half), `StepId` |
| `executor.rs` | `DagExecutor` — parallel `ready_steps` with cancel |
| `retry.rs` | `RetryPolicy` |
| `replan.rs` | `drive()` loop — replan-on-exhausted |
| `events.rs` | `OrchestratorEvent` enum + bus (PlanCreated, StepStarted, RagQueried legacy event name, **MetricsRecorded**, etc.) |
| `wiring.rs` | `ChatServiceFactory` trait + `TurnTelemetry` + `ChatOrchestratorPortImpl` (planner-side LLM turn glue) |
| `shared_files.rs` | `routable_agents` + `render_registry` → `TENGU_PLANNER_REGISTRY.md` (`ensure_planner_registry`), per-session `set_active_plan` / `active_plan` (IPC `plan_state`), `TENGU_PLAN.md` debug artifact, `enumerate_mcp_tools`, `scan_skill_summaries` |

### `src/adapters/metrics.rs` — context/token observability (added 2026-04-28)

Process-wide observability layer. Every LLM and embedding call emits a
`MetricsRecord` with token counts, latency, and (for the planner) per-context-layer
breakdown. Three surfaces — tracing baseline (`RUST_LOG=tengu=info`), in-process
broadcast bus, and `OrchestratorEvent::MetricsRecorded` for the TUI/eval recorder.

| Item | What it is |
|---|---|
| `MetricsRecord` | Per-call telemetry: ts, session_id, kind, agent, model, prompt/completion/total tokens, prompt chars+bytes, response chars, latency, layer breakdown, optional step_id. Serde-friendly so it crosses the subagent IPC boundary. |
| `MetricsKind` | `Planner` (one per user turn) / `Subagent` (one per inner-loop turn) / `Embedding` (one per OpenRouter `/embeddings` call). |
| `MetricsLayer` | One row in the planner's per-context-layer breakdown. Names: `system`, `roster`, `cross_session`, `history`, `recall`, `failure`, `user_message`. Approximate — engine-side framing isn't counted. |
| `install_global_sink` / `record` / `subscribe` | Process-global broadcast sink (`OnceLock<broadcast::Sender>`). `record()` always emits a `tracing::info!` line; bus broadcast is best-effort. |
| `AggregatorState` | Lightweight in-process rollup: overall + by-agent + by-kind. Used by the TUI. |

---

### `src/adapters/egress.rs` — network policy (2026-09-16, Tor default 2026-09-18)

`[egress]` (sandbox or base config). Installed once per process; every LLM-initiated network path goes through it. Operator doc: `docs/egress-2026-09-16.md`.

| Item | What it is |
|---|---|
| `EgressConfig` | `network = "tor"` (default) \| `"open"`; `proxy` (`socks5h://` only); `route_llm_api`; `allow_hosts` / `deny_hosts`; `https_only`; `shell_network = "proxy_env"` \| `"isolated"`; `audit` / `audit_log`. Unknown keys = parse error. |
| `resolved()` | Under `tor`: `proxy` ← `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050` (the `deploy/tor/` Arti + lyrebird-rs container, `make tor`), `route_llm_api` ← true. Under `open`: direct, false. Explicit values win. |
| `install` / `policy` | Process-global. Called by `main` / `load_sandbox_or` / `run-agent` / `mcp-bridge`; children receive the *resolved* config as `TENGU_EGRESS` (`child_env`), which wins over their own file. |
| `tool_client` / `llm_api_client` / `mcp_client` | The only HTTP clients on LLM paths. Tool client: proxy on, redirects off (`http_request` follows them itself and `check_url`s every hop). LLM client proxied iff `route_llm_api`. |
| `shell_command` / `guard_shell` | `run_command` + shell skills: URL literals checked + audited; `isolated` = macOS `sandbox-exec`, only the proxy port reachable. |
| `claude_cli_env` / `http_connect_proxy` / `claude_code_profile` | Claude Code CLI gets `HTTPS_PROXY` = HTTP CONNECT form of the SOCKS proxy (Arti serves CONNECT on 9050); builtin `Bash` dropped while proxied. |
| `warn_if_proxy_unreachable` | One startup warning from the parent CLI (after sandbox resolution) when the proxy port is closed. `tengu doctor` prints the resolved network. |
| Telegram | `TelegramPipe::build_bot` builds teloxide's reqwest 0.11 client (`reqwest011` alias) through the proxy's HTTP CONNECT form, 30s connect / 60s timeout (teloxide's 5s/17s defaults time out over Tor). |

---

## §3 — Open Brain, Wiki, And Planner Files

| Store | What it holds | Written by | Read by | TTL? |
|---|---|---|---|---|
| `TENGU_PLANNER_REGISTRY.md` | `[agents.*]` blocks with a `description` + skills + core & MCP tool defs | `shared_files::ensure_planner_registry` before planner calls | `RagPlanner::plan/replan` prompt context | NO |
| `TENGU_PLAN.md` | current accepted plan/replan (debug artifact; IPC `plan_state` is the source of truth) | `replan.rs::drive` | humans; old-parent fallback in `run_agent_subprocess` | overwritten |
| Postgres `agentic_memory` | Open Brain live memory: user messages + step outputs + source chunks | planner + `run-agent` + `agentic_memory` tool when `postgres_memory` is enabled | planner recall + agents via tool | TBD |
| `.tengu/agentic-memory/wiki/` | Karpathy LLM Wiki compiled Markdown | `agentic_memory compile_wiki` | agents, humans, future MCP surface | Git/history |
| Disk bincode vector store (`memory/vector/disk.rs`) | chat-side `memory_ingest` / `memory_search` / `persistent_store` entries | `MemoryPlugin` tools | same tools + `BuiltinMemoryProvider` | `delete_older_than` on the trait, no sweep |

---

## §3.5 — Context/token metrics (2026-04-28)

Three surfaces, one record:

1. **Tracing baseline (always on)** — every `metrics::record()` writes a
   structured `tracing::info!` with `kind`, `agent`, `model`,
   `prompt_tokens`, `completion_tokens`, `latency_ms`, `prompt_chars`. Run
   `RUST_LOG=tengu=info` to see every LLM and embedding call inline.
2. **In-process broadcast bus** — `OnceLock<broadcast::Sender<MetricsRecord>>`
   in `metrics.rs`. Installed once by `build_orchestrator`. Subscribers
   (`metrics::subscribe()`) get the live stream; lagging subscribers see
   `RecvError::Lagged` and skip ahead (standard broadcast semantics).
3. **TUI bottom panel** — opt-in via `TENGU_TUI_METRICS=1`. Renders a compact
   one-liner as a System bubble: `metrics: tok in/out 4.5k/812 · last
   planner (1.2k) · session 5.6k · researcher 4.0k`. Aggregator absorbs
   records even when the panel is OFF, so flipping it on mid-session shows
   real numbers, not zero.

The subagent's per-turn records cross the IPC boundary in
`AgentIpcOutput.metrics` (new field with `default + skip_serializing_if`
so older child binaries stay compatible). The parent's runner re-emits
each one on the global sink.

---

## §4 — Six things that surprised me reading this code

### A. The doctrine is real

"LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" isn't just a mantra. The planner LLM call in `RagPlanner::plan` strips `tools`, `tool_executor`, `memory_manager`, and `suppress_grounding_nudge` because the planner has exactly one job: emit plan JSON. See `channel_runtime.rs::run_turn_with_system`. The boundary is enforced in code.

### B. Planner and runner share one `session_id`

`channel_runtime::build_orchestrator` resolves the id once (`TENGU_SESSION_ID`
env override or fresh UUID) and passes it to both `RagPlanner` and
`SubprocessRunner`, so planner messages and subagent step summaries share the
same session key.

### C. Sandbox config crosses the IPC boundary (Phase 7.2)

Without this you get the bug the user hit: child loaded `~/.tengu/config.toml` not the parent's sandbox config, so HTTP scopes denied even when the parent allowed them. The fix is `Config.sandbox_name` (`#[serde(skip)]`, populated by `load_sandbox_or`), threaded through `SubprocessRunner` → `AgentIpcInput.sandbox_config` → child's config-load fallback.

### D. `compress_and_store` is implicit

Never list it in `[agents.<name>].tools` — the runner appends it to every subagent automatically. The middle-ground protocol (Phase 5c): a step is only `Failed` when no text emitted AND no `compress_and_store` called. Otherwise `Ok` with a tracing warning. Most agents are well-behaved and call it; the warn path catches the rest.

### E. There are TWO layers of recall in the planner prompt

1. **In-memory ring buffer** (Phase 6.4 lite) — last N user messages this process. Survives within one chat. Lost on restart.
2. **Open Brain block** — recall over Postgres `agentic_memory`. Survives restart. Opt-in via `memory.cross_session_msg_top_k > 0` (default 0 = off).

Both inject **before** the current turn. Different time scales — lite is "what did the user say two turns ago", full is "what did the user ask about three days ago that's similar to now".

### F. C → B fallback is a two-turn dance

When no listed agent fits the request:
- **Turn 1 (C)**: planner emits `Direct { response: "I don't have a perfect match. Closest is researcher (0.18)..." }` asking the user.
- **Turn 2 (B)**: on user confirmation, planner emits `Plan` with `Step.compose` set. Child loads `[agents.<compose.base_agent>]`, OVERRIDES `skill_packages` + `tools` with the values the planner picked from the registry file. **This run only — config on disk unchanged.**

Implementation: `orchestrator/plan.rs::AgentCompose`, threaded through `AgentIpcInput.compose`, applied in `run_agent_subprocess`.

### G. Mixed-engine is the verified pattern (Phase 7.3)

Each `[agents.<name>]` block declares its `engine` (`"openrouter"` or `"claude_code"`, same field for in-process agents). Set to `"claude_code"` and the subprocess builds a Claude Code engine instead. Model slug format depends on engine: OpenRouter wants `anthropic/claude-sonnet-4-6`; Claude Code wants the bare `claude-sonnet-4-6`. Build with `--features claude_code`.

**Don't put a Claude Code agent in the planner role.** The Claude Code CLI has tool access via MCP at engine-construction time, so the planner-side tool stripping (intended for OpenRouter's per-turn `tools = []`) doesn't prevent the planner LLM from calling tools instead of emitting plan JSON. **The verified pattern is OpenRouter for the planner, Claude Code for subagents.**

### H. Claude Code subagents see tengu's tools via the MCP bridge (Phase 7.4)

`main.rs::run_agent_subprocess` sets `EngineContext.bridge_tools = Some(tools)` when the agent's `engine == "claude_code"`. Claude Code spawns `tengu mcp-bridge` as an MCP server, advertises tools as `mcp__tengu-tools__<name>`, and routes calls through it. The bridge has its own `ToolRegistry` (built by `build_bridge_executor`) — Phase 7.7 consolidated it to use the shared `register_core_plugins` so it can't drift from the in-process registry again.

Two env vars are critical to forward to the bridge subprocess (Claude Code's MCP config replaces inherited env): `TENGU_SESSION_ID` (for `compress_and_store` writes) and `OPENROUTER_API_KEY` (for the embedder + memory tools).

### I. parse_verdict has a prose-fallback (Phase 7.4)

When the planner LLM returns prose instead of JSON (Claude Code being conversational, even with strict instructions), `parse_verdict` falls back to wrapping the whole text as a `Direct { response }` verdict with a warn log. No more `System error: orchestrator initial call failed` — the user sees the model's reply, the warn surfaces the issue. Path 4 in `parse_verdict`'s tolerant cascade.

### J. Adding a tool is a one-place edit (Phase 7.7)

Pre-7.7 tool registration was duplicated in `channel_runtime::build_tool_executor` AND `mcp_bridge::build_bridge_executor`; fixing one and forgetting the other caused real bugs. Now `register_core_plugins` is the single registration site for shared plugins (workspace, memory, cache, skill-lifecycle, compress_and_store, http, crypto, and feature-gated additions such as `agentic_memory`). Both the in-process executor and the MCP bridge call this same helper.

The two exceptions are `SkillPlugin` (needs a `SkillRegistry` the bridge can't construct) and `McpPlugin` (the bridge would create double-hop routing). Those stay outside the helper and are registered explicitly only in `build_tool_executor`.

### K. Every LLM network path goes through `egress.rs` (2026-09-16)

`[egress] network = "tor"` is the default — with no `[egress]` section every request goes through `socks5h://127.0.0.1:9050` (`TENGU_TOR_PROXY` overrides) and `route_llm_api` is on; `network = "open"` (e.g. `sandboxes/aura`) is plain internet. `EgressConfig::resolved()` fills the defaults once; `egress::install` is called by `main` / `load_sandbox_or` / `run-agent` / `mcp-bridge`; children inherit the *resolved* config via `TENGU_EGRESS`. Chokepoints: `tool_client` (http_request, crypto), `llm_api_client` (OpenRouter, embeddings — proxied iff `route_llm_api`), `mcp_client`, `shell_command` (`sandbox-exec` under `isolated`), `guard_shell`, `claude_cli_env` (`HTTPS_PROXY` = HTTP CONNECT on the SOCKS port for the Claude Code CLI), `claude_code_profile` (drops builtin Bash under a proxy), a proxied reqwest 0.11 client for Telegram (`TelegramPipe::build_bot`). `build_tool_executor` takes no HTTP client argument, so nothing can hand in an unproxied one. Operator doc: `docs/egress-2026-09-16.md`; §2 has the function table.

**Workflow:**

| To add | Edit | Lines |
|---|---|---|
| Agent | `[agents.<name>]` block with a `description` in `sandboxes/<name>/config.toml` | 1 block |
| Skill | `skills/<name>/SKILL.md` | 1 file |
| Tool (any) | new plugin module + 1 line in `register_core_plugins` | 2 files |
| Workspace-tool opt-in | append name to `WORKSPACE_TOOLS_ALLOWLIST` | 1 constant |

---

## §5 — Where to start when you open the codebase tomorrow

Trace ONE turn end-to-end before changing anything:

```
cargo run --release --features postgres_memory,claude_code -- chat --sandbox aura
> what is the current BTC price in USD?
```

(`aura` is `[egress] network = "open"`; for a Tor sandbox run `make tor` first.)

Follow the call chain:

1. `tui/mod.rs` — captures the input, dispatches to the engine thread
2. `channel_runtime::build_orchestrator` — constructs `Orchestrator`
3. `orchestrator/mod.rs::Orchestrator::handle` — calls `RagPlanner::plan`
4. `orchestrator/planner.rs::RagPlanner::plan` — load planner registry file, LLM call, parse verdict
5. `orchestrator/replan.rs::drive` — runs the plan, handles retry/replan
6. `runner.rs::SubprocessRunner::run_step` — spawns child, pipes IPC
7. `main.rs::run_agent_subprocess` — child entry: load sandbox config → `egress::install` → `[agents.<name>]`, build tools, multi-turn loop
8. `plugins/http/request.rs` — http_request hits CoinGecko through `egress::policy().tool_client`
9. `main.rs::try_persist_agentic_step_summary` — child intercepts `compress_and_store`, writes the durable step summary
10. Back to step 6 (child exits) → step 5 (drive() returns) → step 1 (TUI renders the reply)

That's the whole story. Every other doc in `docs/` zooms into one of those layers.

---

*Written 2026-04-27; last synced 2026-09-18 (single config: `agents/` + `AgentSpec` removed, subagents = `[agents.*]` blocks; `[egress]` Tor by default). Companion files: `architecture-2026-04-27.svg` (the picture), `architecture-2026-04-27.html` (the explorer), `SESSION_HANDOFF.md` (the running state log), `comparison-2026-04-26.md` (vs Hermes / PI).*
