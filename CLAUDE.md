# CLAUDE.md — Tengu-Cluster Project Guide for AI Assistants

> **Read this first, every session, before touching any code.**
> This project is "vibe-coded" — there's a real underlying doctrine but the
> implementation has accumulated layers and naming choices that aren't
> obvious from the file tree alone. The docs below are not optional.
>
> Twin file: `AGENTS.md` carries the same substance for non-Claude assistants.
> Keep the two in sync — a change to one is a change to both.

---

## What this project is

Tengu-Cluster is a multi-agent harness in **Rust**. Single binary. The user runs
`tengu chat --sandbox <name>` (or `tengu telegram --sandbox <name>`) and types
messages into a TUI / Telegram chat. The harness:

1. Loads `sandboxes/<name>/config.toml` — **the one config file**: every
   agent (`[agents.<name>]`), which one is the planner, models, MCP servers,
   scope rules, `[egress]` network policy.
2. Builds `RagPlanner` (legacy name) — an LLM planner that picks which agent
   should handle the message from root `TENGU_PLANNER_REGISTRY.md` (generated
   from the `[agents.*]` blocks that carry a `description`, plus skills/tools,
   and loaded into the planner prompt).
3. Dispatches plan steps to `SubprocessRunner`, which spawns
   `tengu run-agent` as a child process for each step. The child re-loads the
   same sandbox config, takes `[agents.<step.agent>]`, runs an LLM-with-tools
   loop until it calls `compress_and_store`, then writes the result to
   Postgres `agentic_memory` when `postgres_memory` is enabled.

All network traffic is **Tor by default** (`[egress] network = "tor"`, the
Arti + lyrebird-rs proxy from `make tor`); a sandbox opts out with
`network = "open"`. See `docs/egress-2026-09-16.md`.

The doctrine is **LLM = heart, Open Brain + Karpathy LLM Wiki = brain,
tools = hands**. Open Brain is live agent memory; the Karpathy LLM Wiki is
compiled, reviewed knowledge. The planner picks; subagents execute. The
boundary is enforced in code (`run_turn_with_system` strips
tools/memory/grounding on the planner-side LLM call).

---

## Output style — read this before writing ANY doc or response

The user does not have time to read walls of prose. Default to terse, scannable, table-driven output. Applies to docs, chat replies, and commit messages.

| Rule | Applied as |
|---|---|
| Tables over prose | If it can be a table, it must be. |
| One-line decisions, not justifications | Cite a §-ref instead of recapping reasoning. |
| Cap new docs at one screen | Unless the user explicitly asks for depth. |
| Cut "why this matters" / "honest read" / recap preambles | The table is the explanation. |
| Bullets, not paragraphs | Three sentences in a row → convert. |
| Use existing pointers | Don't re-explain `skill_distill` if a § already does. |

**Test**: would a busy operator skim this in 30 seconds and know what to do next? If not, cut more.

Why this rule exists: an earlier draft of `docs/skill-research-2026-04-28.md` ballooned past 700 lines of prose. The user pushed back twice. Don't make them push back again.

---

## REQUIRED reading before any code changes

Read in this order. Do not skip.

1. **`docs/architecture-2026-04-27.md`** — the line-by-line architecture
   walkthrough. Five-sentence TL;DR up top, then §1 walks the seven steps from
   prompt to reply, §2 lists every file in every subsystem, §5 has a
   "trace one turn end-to-end before changing anything" recipe.
2. **`docs/architecture-2026-04-27.svg`** — the picture. Companion to the
   walkthrough. Open both side by side.
3. **`docs/architecture-2026-04-27.html`** — interactive explorer. Tabs for
   walking a turn step-by-step, expanding subsystem cards, searching the file
   map. Use this when you don't remember which file owns what.
4. **`docs/SESSION_HANDOFF.md`** — running state log. What landed last session,
   what's open, known gotchas. Always read the top section.
5. **`docs/agentic-memory-prd-2026-05-13.md`**, **`-implementation-2026-05-13.md`**,
   **`-examples-2026-05-13.md`** — canonical spec for the Open Brain + Karpathy
   LLM Wiki memory subsystem. PRD = product decisions + correction log;
   implementation = phase plan, schema, tool API, Tengu touchpoints; examples =
   workflows + MVP setup. Read these BEFORE touching
   `src/adapters/plugins/agentic_memory/`,
   `src/adapters/orchestrator/shared_files.rs`, or the planner recall lanes.
6. **`REDESIGN.md`** — original v2 design brief. The "why" behind the doctrine.
7. **`docs/IMPLEMENTATION_PLAN.md`** — phase-by-phase roadmap. Many phases
   are now done; the doc tracks what shipped and what didn't.
8. **`docs/comparison-2026-04-26.md`** + **`.svg`** — Tengu vs Hermes Agent
   vs PI/Cowork. Useful when deciding "should we add feature X" — often
   already exists in one of the other two and informs design.
9. **`docs/context-management-2026-04-27.{md,svg,html}`** — canonical
   reference for everything that shapes what an LLM sees: 7 layers,
   ~25 mechanisms (token primitives, per-flow lifecycle, per-turn
   assembly, inner tool loop, MCP bridge cap, subagent IPC, Open Brain /
   LLM Wiki hygiene, file chunking). Read this BEFORE touching `prompt_budget.rs`,
   `flow_builder.rs`, `engine_builder.rs::collect_engine_response`,
   `chat_builder.rs::process_user_text`, or any of the `LimitsConfig`
   knobs. The .html is interactive (search mechanisms, walk a turn,
   symptom → cause lookup); the .md is the canonical narrative.
   Two focused companions slice this further:
   - `docs/context-cutting-flow-2026-04-27.{svg,html}` — Layer 3 deep-dive
     (the inner tool loop).
   - `docs/compression-flow-2026-04-27.{md,svg}` — Layer 5 deep-dive
     (`compress_and_store` step protocol).

---

## REQUIRED updates after non-trivial changes

If you touched code that affects architecture, file structure, or the per-turn
flow, audit these for staleness **before declaring done**:

| File | When to update |
|---|---|
| `docs/SESSION_HANDOFF.md` | Almost always — mark the items you closed, add new opens, update the "TL;DR" if behaviour changed |
| `docs/architecture-2026-04-27.md` | If you changed the per-turn flow, added/removed a subsystem, or moved a file's responsibilities |
| `docs/architecture-2026-04-27.svg` | If you changed the per-turn flow OR added/removed a file in a subsystem panel |
| `docs/architecture-2026-04-27.html` | Same triggers as the svg + md. The SUBSYSTEMS / STEPS / FILE_MAP arrays in the inline `<script>` need to stay in sync |
| `docs/agentic-memory-*-2026-05-13.md` + `src/adapters/plugins/agentic_memory/mod.rs` doc-comment | If you changed the memory schema, the `agentic_memory` tool API / operations, the recall lanes, or the Open Brain ↔ LLM Wiki split |
| `src/adapters/orchestrator/shared_files.rs` doc-comment | If you changed the `TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` shape, or who writes/reads them |
| `docs/comparison-2026-04-26.md` + `.svg` | If your change affects how Tengu compares to Hermes or PI on memory/skills/tools/routing |
| `docs/skill-research-2026-04-28.md` | If the skill-lifecycle plan, gap inventory, or learning-platform A1/A2/A3 decisions change. |
| `docs/context-management-2026-04-27.{md,svg,html}` | If you changed any of the ~25 context-shaping mechanisms (anything in `prompt_budget.rs`, `flow_builder.rs`, `engine_builder.rs::collect_engine_response`, `chat_builder.rs::process_user_text`, the `LimitsConfig` / `MemoryConfig` defaults, the `compress_and_store` protocol, or `rag/cleanup.rs`). The .html keeps inline JS arrays — keep them in sync with the .md. |
| `CLAUDE.md` (this file) **and** `AGENTS.md` (its twin) | If you added/removed a top-level subsystem, changed the doctrine, or added a new "required reading" doc. Update both — they must not drift. |
| Inline `mod.rs` doc comments in `src/adapters/memory/` and `src/adapters/rag/` | If you changed the layering between memory/ (low-level) and rag/ (legacy facade) |
| `sandboxes/*/config.toml` + `config.example.toml` | If you changed `AgentConfig` / `LimitsConfig` / `EgressConfig` (`src/adapters/config.rs`, `src/adapters/egress.rs`), document the field in the struct doc-comment and update every sandbox + the example |
| `docs/webhooks-2026-05-11.md` | If you changed `src/adapters/webhook_builder.rs`, `WebhookConfig`, or the request/response shape. Canonical operator doc for the webhook listener. |
| `docs/egress-2026-09-16.md` + `src/adapters/egress.rs` doc-comment | If you added a network path (new HTTP client, subprocess, engine, channel) or changed `EgressConfig`, the audit record shape, `docker-compose.tor.yml`, `deploy/tor/` or the Makefile `NETWORK` switch. Canonical operator doc for Tor / host allowlist / audit. |
| `skills/orchestrator/SKILL.md` | If you changed what the planner can output OR added a new prompt block (e.g. cross-session recall) |
| `skills/orchestrator/plan_schema.json` | If you changed the plan JSON shape (e.g. added `Step.compose` for C→B fallback) |
| `src/adapters/metrics.rs` doc-comments | If you changed `MetricsRecord` shape, added a new `MetricsKind`, or moved the global sink semantics. The header doctrine block sells the design — keep it accurate. |

**Rule of thumb:** if a future Claude opening this project would learn the
"wrong" thing from a doc, the doc is stale. Fix it in the same commit that
made the doc stale, not later.

---

## How to read context/token metrics

Every LLM and embedding call emits a `MetricsRecord` (see
`src/adapters/metrics.rs`). Three surfaces:

1. **Always-on tracing** — `RUST_LOG=tengu=info` prints one structured line
   per call: `kind=`, `agent=`, `model=`, `prompt_tokens=`,
   `completion_tokens=`, `latency_ms=`, `prompt_chars=`. Per-layer breakdown
   (system / roster / cross_session / history / user_message) goes to `debug`
   so the info line stays narrow.
2. **TUI bottom panel** — opt-in via `TENGU_TUI_METRICS=1`. Renders a
   compact aggregated status line as a System bubble after every call. The
   in-process aggregator (`AggregatorState`) absorbs records even when the
   panel is off.
3. **OrchestratorEvent::MetricsRecorded** — the same record bridged onto the
   orchestrator bus (subscribers like `eval_builder` consume it directly).

Subagent records cross the IPC boundary in `AgentIpcOutput.metrics` (new
field; `default + skip_serializing_if = Vec::is_empty` keeps the payload
byte-compatible). The parent's `SubprocessRunner` re-emits each one on the
global metrics sink so the TUI sees a unified stream.

## How to add a new tool (single source of truth — Phase 7.7)

1. Create the plugin module at `src/adapters/plugins/<name>/mod.rs` (and `tool_defs()` returning a `Vec<ToolDef>`).
2. If it should be available everywhere (in-process planner-side AND Claude Code subagents via the MCP bridge), add ONE registration line to `channel_runtime::register_core_plugins`. That's it. The bridge picks it up automatically.
3. If the tool is workspace-tools opt-in (like `shared_cache`/`persistent_store`/`skill_distill`/`agentic_memory`), add it to `channel_runtime::WORKSPACE_TOOLS_ALLOWLIST` and to the `valid_workspace_tools` list in `config.rs`.

**Do NOT** add registration lines to both `build_tool_executor` and `build_bridge_executor`. That duplication is the whole reason for `register_core_plugins`. The exceptions (registered outside the helper) are `SkillPlugin` and `McpPlugin` — both have inputs the bridge can't sensibly provide.

## How to add a new agent

1. Add an `[agents.<name>]` block to `sandboxes/<name>/config.toml` (or the
   base config) with a `description` — that is what makes it routable.
2. Restart `tengu chat` — `TENGU_PLANNER_REGISTRY.md` is regenerated on planner turns.

Fields (same `AgentConfig` as every in-process agent, `src/adapters/config.rs`):
`engine`, `model`, `description`, `example_queries`, `tools` (subprocess
allow-list; workspace-tool names opt in), `skill_packages` (`skills` alias),
`workspace`, `workspace_tools`, `scopes`, `limits.max_tool_rounds` (turn cap
per step), `limits.step_timeout_secs` (wall clock per step, default 600),
`identity`, `claude_code`. There is no separate subagent schema and no
`agents/` directory.

## How to add a new skill

1. Drop a directory under `skills/<name>/` (or `~/.tengu/skills/<name>/`, or `<workspace>/.tengu/skills/<name>/` — three-tier scanner with shadowing).
2. Add `SKILL.md` with YAML frontmatter (`name`, `description`).
3. Restart `tengu chat` — `TENGU_PLANNER_REGISTRY.md` is regenerated on planner turns.

## Doctrine — keep this when changing code

These are not preferences. They're load-bearing.

1. **LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands.**
   The planner LLM has exactly one job: emit plan JSON. Tools, memory,
   grounding all stripped on its turn (see `channel_runtime::run_turn_with_system`).
   Open Brain stores live events/context in Postgres; the Karpathy LLM Wiki
   compiles stable knowledge into Markdown. Subagents do the actual work in
   their own subprocess with their own tools.

2. **Behaviour changes via TOML and SKILL.md, not Rust.** If a feature
   *can* be expressed by editing `sandboxes/*/config.toml` (agents, scopes,
   egress) or `skills/*/SKILL.md`, do that. Adding new Rust types or trait methods is a last
   resort. The root `TENGU_PLANNER_REGISTRY.md` is regenerated on planner
   turns, so TOML/skill edits take effect without rebuilds.

3. **Composition over wholesale.** Per-agent scopes override
   `default_scopes` wholesale (NOT field-merged). Per-step `Step.compose`
   overrides the base spec's skills/tools wholesale. This keeps the contract
   simple even if it costs some convenience.

4. **Fail-soft on memory operations, hard on plan-shape errors.** Open Brain
   unavailable → log warn, continue with file registry + recent history.
   JSON parse error on plan output → hard error, retried up to 3x. The split
   is deliberate: memory degradation should not break a session, but a plan
   you can't parse means the model is broken.

---

## Key gotchas (compiled from SESSION_HANDOFF + scars)

- **`workspace_tools` is a narrow allow-list** —
  `agentic_memory`, `shared_cache`, `persistent_store`, `skill_distill`,
  `apply_improver_proposal`, `manage_skill`. Anything else fails config
  validation (`config.rs::valid_workspace_tools`). `manage_skill` is the
  canonical unified skill write API (see `plugins/manage_skill/`);
  `agentic_memory` is the Postgres-backed Open Brain memory tool
  (`postgres_memory` feature).
- **One agent schema (2026-09-18)** — `agents/*.toml` and `AgentSpec` are
  gone. A subagent is an `[agents.<name>]` block with a `description`;
  `channel_runtime::subagent_config` merges workspace-tool names found in
  `tools` into `workspace_tools` (filtered by `WORKSPACE_TOOLS_ALLOWLIST`).
  `limits.max_tool_rounds` is the per-step turn cap and
  `limits.step_timeout_secs` the per-step wall clock — the parent
  `SubprocessRunner` reads both from the agent block (the old
  `max_turns`/`timeout_secs` spec fields were never wired). Only blocks WITH
  a `description` may run as plan steps (runner + child both check).
  `AgentConfig` is `deny_unknown_fields`: a typo or an old spec key
  (`max_turns`; `skills` is fine — it is an alias) fails config load.
- **Per-tool scopes are ENFORCED at runtime (2026-09-12)** —
  `[default_scopes.<tool>]` / `[agents.<id>.scopes.<tool>]` are folded into
  `AgentConfig.scopes` at `Config::load` (`fold_default_scopes`; per-agent wins
  wholesale; `~` in `fs_roots` expanded). `build_tool_executor` and the MCP
  bridge (`TENGU_BRIDGE_SCOPES`, exported by `ClaudeCodeEngine::with_scopes`)
  use the configured scope for each tool and `permissive_scope` only for tools
  with no entry. A configured scope is deny-by-default per field: an
  `http_request` scope with empty `fs_roots` denies multipart file uploads —
  `sandboxes/aura/config.toml` sets `fs_roots = ["~/aura-workspace"]` for that
  reason. Subprocess children get their own workspace added to every inherited
  scope's `fs_roots` (`grant_workspace_root`). `ToolScope::check_env_read`
  honours the `"*"` wildcard like `net_hosts` / `shell_bins`.
- **Config resolution** — `--sandbox <name>` replaces the base config
  wholesale; otherwise `--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`.
  The `run-agent` child gets the same file: `--sandbox` travels over IPC and
  `main` pins `TENGU_CONFIG` to the resolved path so `-c/--config` reaches
  children and the MCP bridge too; it then takes `[agents.<name>]` from it.
  Docker mounts `./config.toml` (or `sandboxes/<name>/config.toml` via
  `make up SANDBOX=<name>`) as `TENGU_CONFIG`; `make` derives `NETWORK` from
  that file's `[egress] network`.
- **The accepted plan reaches subagents via IPC, not the file** —
  `AgentIpcInput.plan_state` carries the rendered plan per session
  (`shared_files::set_active_plan`, keyed by the shared `session_id`);
  `run_agent_subprocess` prefers it and only falls back to reading root
  `TENGU_PLAN.md` for old parents. `TENGU_PLAN.md` is a debug artifact —
  concurrent webhook / Telegram sessions no longer race on it.
- **`run-agent` children export `TENGU_SESSION_ID` and `TENGU_AGENT_NAME`** —
  `agentic_memory` `capture` stamps both when the LLM omits `session_id` /
  `agent`, so subagent captures are session-scoped.
- **`tengu doctor` exits non-zero when any agent engine fails to build** —
  it is the Docker HEALTHCHECK. A config with `engine = "claude_code"` agents
  needs an image built with `claude_code` in `TENGU_FEATURES` or the
  container reports unhealthy.
- **IPC boundary test is Rust** — `cargo test --test run_agent_ipc`
  (`tests/run_agent_ipc.rs`) replaced `scripts/test-runner.sh`. Pre-loop
  failures (missing `TENGU_AGENT_IPC`, bad stdin JSON, unknown agent) exit
  non-zero with the anyhow chain on stderr and empty stdout.
- **`compress_and_store` is appended IMPLICITLY** — never list it in an
  agent's `tools`. The runner appends it itself for every subagent.
- **Planner LLM call strips tools/memory/grounding** —
  `run_turn_with_system` in `channel_runtime.rs` sets `tools = []`,
  `tool_executor = None`, `memory_manager = None`,
  `suppress_grounding_nudge = true` when `system_override.is_some()`.
- **`engine = "rag"` is the only orchestrator engine and now the default.**
  `OrchestratorConfig.engine` defaults to `"rag"` (was `"static"`, which
  silently disabled orchestration); any other value fails config validation.
  The name is historical; current routing is file-registry + Open Brain
  memory, not retrieval-first memory.
- **Planner registry is file-backed** — `RagPlanner` regenerates root
  `TENGU_PLANNER_REGISTRY.md` from the `[agents.*]` blocks with a
  `description` (`shared_files::routable_agents`), skills, and core tool
  definitions before planner calls (`orchestrator/shared_files.rs`).
  Editing the sandbox config and restarting `tengu chat` is sufficient to
  pick up the change. The planner LLM is asked to use its own judgement reading
  each description — there is no similarity score gate anymore.
- **`TENGU_PLAN.md` carries the accepted plan to subagents** — `replan.rs::drive`
  overwrites root `TENGU_PLAN.md` on every accepted plan/replan;
  `run_agent_subprocess` reads it into the subagent system prompt at startup.
  Both files are runtime artifacts — gitignored, never hand-edited.
- **Sandbox config crosses the IPC boundary (Phase 7.2)** —
  `Config.sandbox_name` (`#[serde(skip)]`) is set by `load_sandbox_or` and
  threaded through `SubprocessRunner` → `AgentIpcInput.sandbox_config` so the
  child re-resolves the same `sandboxes/<name>/config.toml` and inherits the
  parent's scopes/secrets/MCP servers. Without this, `http_request`
  scope-denies in the child even when the parent's sandbox allows it.
- **TUI debug panel is opt-in via `TENGU_TUI_RAG_DEBUG=1`** — when on,
  `OrchestratorEvent::RagQueried` renders as a compact System bubble showing
  top-3 hits per planner call.
- **TUI metrics panel is opt-in via `TENGU_TUI_METRICS=1`** — when on,
  every `OrchestratorEvent::MetricsRecorded` updates the in-process
  `AggregatorState` and a compact one-liner is pushed as a System bubble
  (`metrics: tok in/out 4.5k/812 · last planner (1.2k) · session 5.6k · researcher 4.0k`).
  Aggregator absorbs events even when the panel is OFF — flipping it on
  mid-session shows real numbers, not zero. Tracing baseline
  (`RUST_LOG=tengu=info` → one `metrics` info line per call) is always-on.
- **Per-context-layer metric attribution is approximate** — `MetricsLayer`
  only sees the strings the planner builds (system / roster / cross_session
  / history / recall / failure / user_message). Engine-side framing
  (Anthropic `<thinking>` blocks, OpenAI tool-call schema, function-calling
  message wrappers) isn't counted. Treat layers as relative attribution
  for "which planner block bloated this turn", not absolute byte-perfect
  accounting against `prompt_tokens`.
- **Metrics records cross the IPC boundary via `AgentIpcOutput.metrics`** —
  added 2026-04-28. Old child binaries produce JSON without the field;
  serde's `#[serde(default)]` gives the parent an empty vec. Re-emission
  happens in `SubprocessRunner::run_step` for both the Ok and Failed paths,
  so a partial subagent run still surfaces its consumed tokens.
- **`session_id` is unified between planner and runner (Fix B 2026-05-09)** —
  `channel_runtime::build_orchestrator` resolves the id ONCE
  (env override `TENGU_SESSION_ID` > fresh UUID), passes the same string to
  `RagPlanner::new(...)` AND `SubprocessRunner::new(sandbox_name, session_id)`.
  Planner messages and subagent step summaries share the same session key,
  so `RagPlanner::session_output_recall_block` can filter to it on the next
  turn. `SubprocessRunner::default()` still mints a fresh UUID for standalone
  test / CLI use.
- **Within-session output recall is opt-in via a config knob** —
  `[memory] within_session_output_top_k = N` in `sandboxes/<name>/config.toml`.
  Default 0 = off (back-compat — the planner prompt is unchanged). With
  `postgres_memory`, `RagPlanner::plan` injects a `## Recent step outputs
  (this session)` block from Postgres `agentic_memory` filtered by the shared
  `session_id`. Recommended `3–5`.
- **`[agents.<name>].engine` selects the subagent engine (Phase 7.3)** —
  `"openrouter"` or `"claude_code"` (required field, no default); the same
  block serves in-process chat and `run-agent` steps.
  `model` slug format depends on engine: OpenRouter wants
  `anthropic/claude-sonnet-4-6`; Claude Code wants the bare `claude-sonnet-4-6`.
  Building with `--features claude_code` is required.
- **`sandboxes/aura` is `network = "open"`** — Molecule / Privy / Beach block
  Tor exits. Every other sandbox and the base config run over Tor.
- **Don't put a Claude Code agent in the planner role.** The Claude Code CLI
  has tool access via MCP at engine-construction time, so the planner-side
  tool stripping (intended for OpenRouter's per-turn `tools = []`) doesn't
  prevent the planner LLM from calling tools instead of emitting plan JSON.
  Result: planner bypasses orchestration. **Use OpenRouter for the planner
  agent and Claude Code for subagents** (mixed-mode is the verified pattern).
- **`parse_verdict` has a Phase 7.4 prose-fallback** — when the planner LLM
  emits non-JSON (Claude Code being conversational), the harness wraps the
  whole text as a `Direct { response }` verdict with a warn log. No more
  `System error: orchestrator initial call failed`. Watch for the warn line
  to know when this is firing.
- **Adding a new tool: ONE place (Phase 7.7)** —
  `channel_runtime::register_core_plugins`. Both the in-process executor and
  the MCP bridge call this single helper. Don't add registration lines to
  `build_tool_executor` or `build_bridge_executor` directly. The exceptions
  (registered outside) are `SkillPlugin` (needs a SkillRegistry the bridge
  can't construct) and `McpPlugin` (the bridge would create double-hop
  routing).
- **Adding a workspace-tool opt-in: ONE constant (Phase 7.7)** —
  `channel_runtime::WORKSPACE_TOOLS_ALLOWLIST`. Both `agent_config_from_spec`
  and the bridge filter against it. Keep `config.rs::valid_workspace_tools`
  in sync — a name missing there fails config validation.
- **`compress_and_store` reliability with Claude Code subagents** — Claude
  Code subagents don't reliably call `compress_and_store` as their final
  action; they just stop. The Phase 5c middle-ground protocol forgives this —
  the final assistant text becomes the IPC summary. With `postgres_memory`,
  that final text is also captured into Postgres `agentic_memory` by the
  `run-agent` backstop (`try_persist_agentic_step_summary`) so the durable
  row exists for within-session recall on the next user turn. The original
  `model finished without calling compress_and_store` warn is still emitted.
- **Memory backend** — `agentic_memory` (Postgres + pgvector, behind the
  `postgres_memory` feature) is the only durable runtime memory: planner
  user-message recall, within-session step-output recall, replan cross-plan
  recall, and subagent summary capture all read/write it. The legacy Qdrant
  `rag/` facade and the `qdrant` cargo feature were removed in Phase 6
  (2026-05-14). The only built-in `VectorStore` is now the disk-backed bincode
  store. Embedding model is pinned to `text-embedding-3-small` (1536-dim,
  `memory::vector::embedder::DEFAULT_EMBEDDING_MODEL`) because the Postgres
  schema hardcodes `vector(1536)`; a wrong-dimension vector now warns and
  falls back to text-only writes / FTS-only recall (fail-soft).
- **No write-landed diagnostic CLI yet** — `tengu memory inspect` was removed
  with the Qdrant path. To check whether a write landed, query Postgres
  directly against `TENGU_MEMORY_DATABASE_URL` or run the ignored
  `postgres_*_smoke` tests. A Postgres-native inspect CLI is a tracked
  follow-up.
- **All network traffic goes through `egress.rs`, and the default is Tor
  (2026-09-18)** — no `[egress]` section means `network = "tor"`:
  `socks5h://127.0.0.1:9050` (`TENGU_TOR_PROXY` overrides), `route_llm_api =
  true`, fail-closed. `network = "open"` = direct. Build HTTP clients for
  tools with `egress::policy().tool_client` (redirects off — `http_request`
  follows them itself so every hop is re-checked), LLM provider clients with
  `llm_api_client`, MCP-http with `mcp_client`; spawn shells via
  `shell_command`. The Claude Code CLI gets `HTTPS_PROXY` (`claude_cli_env`)
  and teloxide gets a reqwest 0.11 client (`reqwest011`, 30s/60s timeouts), both via the HTTP CONNECT form of the proxy
  (Arti serves CONNECT on the SOCKS port). A bare `reqwest::Client::builder()`
  on a runtime path bypasses Tor. The *resolved* policy crosses to
  `run-agent` / `mcp-bridge` as `TENGU_EGRESS` (wins over the child's
  config). JSONL audit at `<TENGU_HOME>/logs/egress.jsonl`. `tengu doctor
  --tor` verifies the exit; `make tor` runs the proxy (`deploy/tor/`: Arti +
  lyrebird-rs from `../lyrebird-rs`). Unit tests must not call
  `egress::install` (process-global).
- **MCP bridge tool names are prefixed `mcp__tengu-tools__<name>`** — when
  Claude Code calls a tengu tool through the bridge, the model sees
  `mcp__tengu-tools__persistent_store`, not bare `persistent_store`. Skills
  that say "check that tool X is in your tool list" should look for both
  forms or just attempt the call and read the error.

---

## Open items still on the list

See `docs/SESSION_HANDOFF.md` for the running list. As of 2026-05-14, the
agentic-memory migration (Open Brain Postgres + pgvector behind
`postgres_memory`) has landed all six phases: the `agentic_memory` plugin, the
schema, the planner/runner recall replacement, the LLM Wiki compiler
(`compile_wiki`), the standalone MCP server (`tengu agentic-memory-server`),
and the Phase 6 cleanup that removed the legacy Qdrant `rag/` module, the
`qdrant` feature, and the `tengu registry` / `tengu memory inspect` CLIs.
Smaller gaps remain (real `lint`, `[agentic_memory]` config section,
`propose_behavior` operation, `memory_claims`/`memory_links` graph tables,
chunked oversize summaries, a Postgres-native inspect CLI) — all tracked in
the handoff. The 2026-09-12 audit pass (see handoff top section) removed the
orphaned `rag/` + `vector/qdrant.rs` files, made scopes enforce at runtime,
and rewrote the run docs (README, Makefile, Dockerfile, compose, installer).

---

## What to do when you're stuck

1. **Trace one turn end-to-end before changing anything.** Run
   `cargo run --release -- chat --sandbox aura` (add `--features
   postgres_memory` to exercise Open Brain recall), type "what is the BTC
   price?", and follow the logs. The flow is in §1 of
   `docs/architecture-2026-04-27.md`.
2. **Open `docs/architecture-2026-04-27.html` in a browser** — the file map
   tab is searchable. Type the symbol you're looking for; it'll surface
   which subsystem owns it.
3. **`SESSION_HANDOFF.md` "Active gotchas" section** — most weird symptoms
   are documented there with the fix.
4. **If you're about to add a new abstraction layer** — stop and check
   whether the change can be expressed in TOML / SKILL.md / config instead.
   The doctrine prefers data changes over code changes.

---

*Last updated 2026-09-18 (Tor-by-default egress, single sandbox config — `agents/` removed, deploy/tor = Arti + lyrebird-rs; previously 2026-09-12 audit pass, 2026-05-14 agentic-memory MVP — Open Brain Postgres + pgvector
behind `postgres_memory`; planner registry moved to file-backed
`TENGU_PLANNER_REGISTRY.md`; doctrine is now "Open Brain + Karpathy LLM Wiki =
brain"). If you're reading this in the future and the companion doc filenames
have rolled forward (e.g. `architecture-2026-05-12.md`), update the references
in this file too. Stale references in `CLAUDE.md` are the worst kind of stale —
and remember to mirror every change into `AGENTS.md`.*
