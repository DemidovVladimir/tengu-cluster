# AGENTS.md — Tengu-Cluster Project Guide for AI Assistants

> **Read this first, every session, before touching any code.**
> This project is "vibe-coded" — there's a real underlying doctrine but the
> implementation has accumulated layers and naming choices that aren't
> obvious from the file tree alone. The docs below are not optional.
>
> Twin file: `CLAUDE.md` carries the same substance for Claude.
> Keep the two in sync — a change to one is a change to both.

---

## What this project is

> **Rust only.** No JavaScript / TypeScript / Python source in this repo —
> not even test-fixture generators or one-off probes. Produce goldens
> outside the repo (e.g. with the bot's own libraries) and commit only the
> resulting fixtures (`tests/fixtures/**`). Existing non-Rust files are
> infra only: `deploy/*.sh`, `Makefile`, `Dockerfile*`, test shell fixtures.
>
> **One exception — the Studio web page (operator, 2026-10-08):** HTML, CSS
> and vanilla JavaScript may live under `web/studio/` only, for the Tengu
> Studio frontend: no build step, no TypeScript, no CDN, no telemetry, embedded
> in the binary with `include_str!`. It only draws what the Rust API serves —
> every rule (risk, scope, schedule, legality, health, colour from status) is
> computed in Rust. Never for tools, adapters, runtime, tests, fixtures or
> probes. `tests/language_policy.rs` enforces where each language may live
> (`docs/` keeps its static site).

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

| Beyond chat | What | Doc |
|---|---|---|
| `tengu run --sandbox <s>` | long-running: `[feeds.*]`, `[decision_loops.*]` (Jev picks the action, tools execute), lease, heartbeat, `doctor --live` | `docs/runtime-2026-09-30.md` |
| `tengu studio --sandbox <s>` | local browser UI (`--features studio`): the validated graph, live + replayed runs (`tengu trace`), health; Play / Stop only where `[studio] control` allows it (never W1-bound / hardened) | `docs/studio-2026-10-08.md` |
| `sandboxes/xmarket` | paper trading desk: every order tool runs the `[risk]` gate inside it ($100 budget) | `docs/xmarket-tracker-2026-09-29.md` § 0 |
| `sandboxes/xlab` | research on backfilled history: `tengu history`, `tengu backtest`, Jev replayed | `docs/xlab-2026-10-01.md` |
| `lineage/` · `tengu lineage` · `tengu evidence` | research lineage: experiments, variants, Experience, capabilities, generations (W1 frozen; a sandbox binds one with `[generation]`), evidence vault + forward grading | `docs/lineage-2026-10-06.md` |

All network traffic is **Tor by default** (`[egress] network = "tor"`, the
Arti + lyrebird-rs proxy from `make tor`); a sandbox opts out with
`network = "open"`. See `docs/egress-2026-09-16.md`.

The doctrine is **LLM = heart, Open Brain + Karpathy LLM Wiki = brain,
tools = hands**. Open Brain is live agent memory; the Karpathy LLM Wiki is
compiled, reviewed knowledge. The planner picks; subagents execute. The
boundary is enforced in code (`run_turn_with_system` strips
tools/memory/grounding on the planner-side LLM call).

## Code layout — hexagonal (2026-09-23)

| Layer | Path | Holds | May import |
|---|---|---|---|
| domain | `src/domain/` | plain data + pure policy (messages, plan, `ToolScope`, secrets redaction, metrics records, observations, decisions, risk / paper / backtest math, calendars) | nothing else |
| ports | `src/ports/` | traits: `Engine`, `Tool`, `MemoryService`, `RecallStore`, `Planner`, `WorkerHandle`, `Clock`, `ObservationStore`, `PaperLedger`, `MarketDataStore`, … | domain, config |
| config | `src/config/` | TOML schema, validation, paths | domain |
| application | `src/application/` | use cases: chat turn + tool loop, orchestrator, memory manager, skills, tool dispatch, decision loops, runtime (`tengu run`), paper gate + fill, backtests | domain, ports, config |
| outbound adapters | `src/adapters/outbound/` | engines, **tools**, memory stores, MCP client, egress, secrets, subprocess runner, SQLite stores (observations, ledger, `market.db`), backfill, Hyperliquid / Solana clients | all but inbound/bootstrap |
| bootstrap | `src/bootstrap/` | composition root: tool executor, memory, orchestrator, sandbox, decision loops, runtime | all but inbound |
| inbound adapters | `src/adapters/inbound/` | CLI (`cli/`), TUI, Telegram, webhooks, `tengu run`, MCP bridge, eval/evolve | everything |

Enforced by `tests/layering_lint.rs`. **Where does X live / how do I add Y** → `docs/code-map.md` (+ `docs/code-map.html`, interactive knowledge graph).

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

Read in this order. Do not skip. **Start with `docs/code-map.md`** when you
only need to know where something lives or how to extend it (tools, engines,
config, channels) — it is the index into everything below.

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
   `src/adapters/outbound/tools/agentic_memory/`,
   `src/application/orchestrator/shared_files.rs`, or the planner recall lanes.
6. **`docs/IMPLEMENTATION_PLAN.md`** — phase-by-phase roadmap. Many phases
   are now done; the doc tracks what shipped and what didn't.
7. **`docs/comparison-2026-04-26.md`** + **`.svg`** — Tengu vs Hermes Agent
   vs PI/Cowork. Useful when deciding "should we add feature X" — often
   already exists in one of the other two and informs design.
8. **`docs/context-management-2026-04-27.{md,svg,html}`** — canonical
   reference for everything that shapes what an LLM sees: 7 layers,
   ~25 mechanisms (token primitives, per-flow lifecycle, per-turn
   assembly, inner tool loop, MCP bridge cap, subagent IPC, Open Brain /
   LLM Wiki hygiene, file chunking). Read this BEFORE touching `prompt_budget.rs`,
   `application/chat/flow.rs`, `application/chat/tool_loop.rs::collect_engine_response`,
   `application/chat/service.rs::process_user_text`, or any of the `LimitsConfig`
   knobs. The .html is interactive (search mechanisms, walk a turn,
   symptom → cause lookup); the .md is the canonical narrative.
   Two focused companions slice this further:
   - `docs/context-cutting-flow-2026-04-27.{svg,html}` — Layer 3 deep-dive
     (the inner tool loop).
   - `docs/compression-flow-2026-04-27.{md,svg}` — Layer 5 deep-dive
     (`compress_and_store` step protocol).
9. **`docs/typed-observations-2026-09-24.md`** — typed tool results
    (`Observation` envelope), the observation cache
    (`<workspace>/.tengu/observations.db`), decision-loop `world` /
    `requires`, the 11 Solana LP observe/plan tools (args, keys, TTLs, hosts, knobs)
    and the 5 write tools (§ Write tools: modes, signer, lease, fence).
    Read BEFORE touching `domain/observation.rs`, `application/observe.rs`,
    `application/decision_loop/`, `adapters/outbound/tools/solana/`,
    `adapters/outbound/solana/`, `domain/lp/`, `domain/solana_tx.rs`,
    `domain/solana_write.rs` or `config/solana.rs`.
10. **`docs/xmarket-tracker-2026-09-29.md`** (+ `docs/xmarket-prd-2026-09-29.md`,
    `docs/xmarket-gaps-2026-09-29.md`) — the `xmarket` sandbox: the operator's
    PRD with its 2026-09-30 addendum (decisions and rules), and the backlog
    (185 items, milestones E0, M0–M8, M3b), plus
    `docs/xmarket-build-plan-2026-09-30.md` — how to execute it (waves, gates,
    the weekend sandbox, a kickoff prompt). **§ 0 Start here** of the tracker
    holds the rules for every task and the definition of done. Read BEFORE any xmarket work: `tengu run`, feeds,
    `[risk]` / paper trading, Hyperliquid / Robinhood / news tools, or the
    MCP-bridge parity items.
11. **`docs/xlab-2026-10-01.md`** — the `xlab` sandbox (operator PRD v0.5: Architect +
    TypeSafe JEV + deterministic layer + risk), **history first**: the market-data
    warehouse `<state dir>/market.db` + backfill (HL candles / funding,
    GeckoTerminal, the HL S3 archive), strategy specs (the Architect's level-2
    capabilities: data, never code), the pure backtest engine (`domain/backtest/`,
    time-integrity checks), Jev replayed on history (gate arm + decision cache),
    tools `market_history` / `backtest`. Read BEFORE touching
    `domain/{marketdata*,backtest/,canonical.rs}`, `ports/market_data.rs`,
    `adapters/outbound/{market_data.rs,backfill/,decision_cache.rs,tools/xlab/}`,
    `application/backtest/`, `config/backtest.rs` or `sandboxes/xlab`.
13. **`TENGU_HANDOFF.md` + `TENGU_ROADMAP.md`** — operator intent (W1 → W2 → evolution,
    gated phases, STOP at operator reviews) — and **`docs/lineage-2026-10-06.md`**:
    the `lineage/` registry (one TOML file per record), `tengu evidence` (vault,
    grade, regrade, coverage), generation binding. State: P0–P5 done, W1 frozen,
    STOP at Operator Review #1 (`docs/w1-review-2026-10-06.md`). Read BEFORE
    touching `lineage/`, `domain/{lineage/,evidence*.rs,xm/grade.rs,xm/regrade.rs}`,
    `{config,ports,application,adapters/outbound}/{lineage,evidence}*`, a
    sandbox's `[generation]` or any W1-pinned section.

---

## REQUIRED updates after non-trivial changes

**Every code change updates the visual tutorial (`docs/tutorial/`, operator rule 2026-10-07).**
`docs/tutorial/sources.toml` maps each feature page to the source paths it explains.
In the same commit as any change under `src/`: re-read the changed code, update
every page whose `sources` lists a changed file or one of its parent directories,
and bump that page's `Checked against the code on <date>` footer. A new, moved or
deleted source file also changes `sources.toml`; a new feature gets a page + a
`assets/nav.js` entry. `cargo test --test tutorial_map` fails on an unmapped `src/`
file, a dead path or a broken page; Claude Code also gets a reminder from the
`PostToolUse` hook in `.claude/settings.json`. Page rules: `docs/tutorial/AUTHORING.md`.

If you touched code that affects architecture, file structure, or the per-turn
flow, audit these for staleness **before declaring done**:

| File | When to update |
|---|---|
| `docs/SESSION_HANDOFF.md` | Almost always — mark the items you closed, add new opens, update the "TL;DR" if behaviour changed |
| `docs/architecture-2026-04-27.md` | If you changed the per-turn flow, added/removed a subsystem, or moved a file's responsibilities |
| `docs/architecture-2026-04-27.svg` | If you changed the per-turn flow OR added/removed a file in a subsystem panel |
| `docs/architecture-2026-04-27.html` | Same triggers as the svg + md. The STEPS / FLOWS / SUBSYSTEM_GROUPS / SUBSYSTEMS / FILE_MAP arrays in the inline `<script>` need to stay in sync |
| `docs/agentic-memory-*-2026-05-13.md` + `src/adapters/outbound/tools/agentic_memory/mod.rs` doc-comment | If you changed the memory schema, the `agentic_memory` tool API / operations, the recall lanes, or the Open Brain ↔ LLM Wiki split |
| `src/application/orchestrator/shared_files.rs` doc-comment | If you changed the `TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` shape, or who writes/reads them |
| `docs/comparison-2026-04-26.md` + `.svg` | If your change affects how Tengu compares to Hermes or PI on memory/skills/tools/routing or the trading runtime axes (typed decisions, runtime, money safety, research on history, engine parity) |
| `docs/skill-research-2026-04-28.md` | If the skill-lifecycle plan, gap inventory, or learning-platform A1/A2/A3 decisions change. |
| `docs/context-management-2026-04-27.{md,svg,html}` | If you changed any of the ~25 context-shaping mechanisms (anything in `prompt_budget.rs`, `application/chat/flow.rs`, `application/chat/tool_loop.rs::collect_engine_response`, `application/chat/service.rs::process_user_text`, the `LimitsConfig` / `MemoryConfig` defaults, the `compress_and_store` protocol, or `rag/cleanup.rs`). The .html keeps inline JS arrays — keep them in sync with the .md. |
| `AGENTS.md` (this file) **and** `CLAUDE.md` (its twin) | If you added/removed a top-level subsystem, changed the doctrine, or added a new "required reading" doc. Update both — they must not drift. |
| `docs/code-map.md` + `docs/code-map.html` (inline `GRAPH` data) | If you added/moved/removed a file, port, layer, tool, engine, config section, or changed an extension recipe. Keep the two in sync. |
| `docs/tutorial/<slug>.html` + `docs/tutorial/sources.toml` | **Every code change** (rule above): pages whose `sources` cover the changed paths; `sources.toml` when files move; `cargo test --test tutorial_map`. |
| `tests/layering_lint.rs` | If you added a layer or changed who may import whom (`RULES` / `FORBIDDEN`). |
| `docs/tools.md` | If you changed the tool catalog, tool gating, `[[mcp_servers]]` handling, or how agents get tools. |
| `sandboxes/*/config.toml` + `config.example.toml` | If you changed `AgentConfig` / `LimitsConfig` / `EgressConfig` (`src/config/mod.rs`, `src/adapters/outbound/egress.rs`), document the field in the struct doc-comment and update every sandbox + the example |
| `docs/webhooks-2026-05-11.md` | If you changed `src/adapters/inbound/webhooks.rs`, `WebhookConfig`, or the request/response shape. Canonical operator doc for the webhook listener. |
| `docs/typed-observations-2026-09-24.md` | If you changed the `Observation` envelope, the observation store / `observe()`, decision-loop `world` / `requires` / typed history, a Solana tool's args, key, TTL, hosts or knobs, or a write tool's send rules (signer, `config/solana.rs`, lease / pending / fence). |
| `docs/xmarket-tracker-2026-09-29.md` (+ PRD addendum) | If you worked on an xmarket item: tick it (✅ + commit), update § 0 "Where to begin" when the next step changes, and keep the conventions, decisions and PRD addendum current when a rule changes. |
| `docs/xlab-2026-10-01.md` | If you changed `market.db` / backfill sources, the strategy-spec kinds, the engine's fill / cost / funding / time rules, the Jev gate arm, the run-dir files or the xlab tools. Keep its "as built" rows current. |
| `lineage/` + `docs/lineage-2026-10-06.md` | If you ran an experiment or added a variant, episode, incident or capability (one record file), changed a W1-pinned section (that is a new generation, never an edit of W1 — `tengu lineage verify --pins`), or changed the registry / evidence code. Preregister with `tengu lineage seal` before any outcome. |
| `docs/egress-2026-09-16.md` + `src/adapters/outbound/egress.rs` doc-comment | If you added a network path (new HTTP client, subprocess, engine, channel) or changed `EgressConfig`, the audit record shape, `docker-compose.tor.yml`, `deploy/tor/` or the Makefile `NETWORK` switch. Canonical operator doc for Tor / host allowlist / audit. |
| `skills/orchestrator/SKILL.md` | If you changed what the planner can output OR added a new prompt block (e.g. cross-session recall) |
| `skills/orchestrator/plan_schema.json` | If you changed the plan JSON shape (e.g. added `Step.compose` for C→B fallback) |
| `src/domain/metrics.rs` doc-comments | If you changed `MetricsRecord` shape, added a new `MetricsKind`, or moved the global sink semantics. The header doctrine block sells the design — keep it accurate. |
| `docs/studio-2026-10-08.md` + `TENGU_STUDIO_PLAN.md` § 8 | If you changed `tengu studio` (routes, guard, control, `[studio]`, the page in `web/studio/`) or the trace envelope: keep the operator doc true and tick the § 8 row (status, evidence, commit). |

**Rule of thumb:** if a future agent opening this project would learn the
"wrong" thing from a doc, the doc is stale. Fix it in the same commit that
made the doc stale, not later.

---

## How to read context/token metrics

Every successful planner call, `run-agent` step turn, embedding, wiki-compiler
and Jev decisions call emits a `MetricsRecord` (see `src/domain/metrics.rs`);
a failed call and an in-process chat turn (TUI / Telegram / webhook agent
without a plan) emit none. Three surfaces:

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

## How to add a new tool (one catalog row)

1. `src/adapters/outbound/tools/<name>/mod.rs`: `impl Tool` (`ports::tool`), a `ToolPlugin`, `tool_defs()`. `execute` calls `ctx.scope.check_*` within its first 30 lines, or says `// scope: pure-compute` (`tests/scope_lint.rs`).
2. One `ToolEntry` row in `catalog()` (`src/adapters/outbound/tools/mod.rs`) — drives in-process registration, the MCP bridge, and the advertised tool list.
3. Opt-in only: also add the name to `src/domain/tools.rs::WORKSPACE_TOOLS` (config validation; `catalog_tests` fail if you forget).
4. **Works under every engine — `openrouter`, `local`, `claude_code` — no exceptions (operator rule, 2026-09-30).** OpenRouter and local run tools in-process; Claude Code reaches them through `tengu mcp-bridge`, which must behave the same: everything the tool reads (sandbox config sections, stores under the workspace or `<TENGU_HOME>/state`, secrets, scopes, the call id) must reach the bridge. Keep the input schema in the subset all three accept and the result within a local model's context window. A tool is done when its schema lint (`tools/schema_lint.rs`, runs over every catalog row), bridge conformance case and live engine-matrix smoke pass (a tool set in `tests/engine_matrix.rs` — `every_catalog_tool_has_a_live_leg` fails CI without one; milestone E0 in `docs/xmarket-tracker-2026-09-29.md`, open items in the gotcha below).

No Rust needed for HTTP APIs (skill + `http_request`) or existing tool servers (`[[mcp_servers]]`). Full recipe + agent config: `docs/tools.md`, `docs/code-map.md`. `SkillPlugin` / `McpPlugin` stay outside the catalog (registered in `bootstrap/tools.rs::build_tool_executor`).

## How to add an inference engine / extend config

| Task | Where | Recipe |
|---|---|---|
| New engine (e.g. another provider) | `src/adapters/outbound/engines/<name>.rs` + `build_engine` match in `engines/mod.rs` | `docs/code-map.md` § Add an engine |
| New config field / section | `src/config/mod.rs` (struct + `#[serde(default)]` + `Default`) → validation in `validation_errors` | `docs/code-map.md` § Extend config |
| Built-in defaults | `impl Default for Config` + `default_*` fns in `src/config/mod.rs`; commented example `config.example.toml` | — |

## How to add a new agent

1. Add an `[agents.<name>]` block to `sandboxes/<name>/config.toml` (or the
   base config) with a `description` — that is what makes it routable.
2. Restart `tengu chat` — `TENGU_PLANNER_REGISTRY.md` is regenerated on planner turns.

Fields (same `AgentConfig` as every in-process agent, `src/config/mod.rs`):
`engine`, `model`, `description`, `example_queries`, `tools` (allow-list on
every surface — chat, plan steps, bridge; workspace-tool names opt in),
`skill_packages` (`skills` alias),
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
   grounding all stripped on its turn (see `bootstrap::orchestrator::run_turn_with_system`).
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
   overrides the base spec's skills/tools wholesale — in a hardened sandbox
   (`[risk]` / Solana signer) it may only narrow them
   (`bootstrap::tools::compose_agent`; a widening compose fails the step).
   This keeps the contract simple even if it costs some convenience.

4. **Fail-soft on memory operations, hard on plan-shape errors.** Open Brain
   unavailable → log warn, continue with file registry + recent history.
   Planner output is parsed tolerantly (`planner.rs::parse_verdict`: raw
   JSON → fenced → embedded JSON → non-empty prose becomes a `Direct` reply
   with a warn); an empty reply fails the turn — the planner call is not
   retried. A failing plan step runs up to `max_attempts_per_step` times
   (default 3; it waits 1 s, then 3 s, 9 s for any later attempt), then
   the plan is replanned up to `max_replans` times (default 2). The split
   is deliberate: memory degradation should not break a session, but a plan
   you can't parse means the model is broken.

---

## Key gotchas (compiled from SESSION_HANDOFF + scars)

- **Generations are frozen (2026-10-06, `docs/lineage-2026-10-06.md`)** —
  `sandboxes/xlab` and `xmarket-weekend` carry `[generation] id = "W1"`: every
  config load checks their tools, feeds and strategy kinds against
  `lineage/generations/W1.toml` (an opt-in tool needs a capability — closed
  world) and recomputes its `config:` / `spec:` pins, so editing a pinned
  section (library specs, costs, splits, universes, `[risk]`, `[paper]`, rule W,
  the Jev loop, agent models) fails the load (`pin_drift`). The W1 lock covers
  W1.toml and every capability record it lists — a binding added at the same
  version fails the load too (`frozen_manifest_changed`); `verify --pins` wants
  every listed sandbox to keep its `[generation]` (`sandbox_unbound`). W2 work
  goes in new sandboxes bound to a new generation; `lineage/locks.toml` is
  append-only. Evidence: `tengu evidence snapshot` copies into a read-only vault
  `<TENGU_HOME>/state/evidence/<id>/` (a non-empty `-wal` is refused —
  checkpoint first); read old ledgers / day files there, never through the
  store adapters (they migrate or purge on open). Phase 6+ of
  `TENGU_ROADMAP.md` waits for the operator's APPROVE.
- **History first (operator rule 2026-10-01)** — answer trading / strategy
  questions by backfilling public history and backtesting it (`tengu history
  backfill`, `tengu backtest`, sandbox `xlab`, `docs/xlab-2026-10-01.md`) — never
  make the operator wait days for live recording. Live recording only for data
  with no historical source (e.g. executable xyz weekend books), said so, never a
  blocking step. HL serves the newest 5 000 bars per interval (1h ≈ 208 days) and
  funding since listing; its S3 archive (`hyperliquid-archive`, requester pays)
  has per-minute contexts + L2 books for main-dex perps only (no `xyz:*`); HL keeps
  no-trade hours as flat bars (`n = 0`, stale price). Engine time integrity: a bar
  is observable only at its close; `data_asof_ms ≤ decided_at_ms` is asserted and
  `domain/backtest/checks.rs` proves no move after t (bars scaled, deleted or cut;
  funding; ctx) changes a decision, an admission or a closed trade at or before t
  (15m across DST, 1h, 4h, 1d, split worlds); P&L is a linear perp's simple return,
  never ln; capped admissions never read whether a later bar exists. Jev on history:
  the replay loop runs with `history = 0` and every answer is cached by the full
  sha256 of (model, state, questions) — Jev answers vary call to call, the cache
  makes reruns identical; pin the Jev build in the gate loop. Holdout discipline
  (the Architect's `backtest` tool): a `split` runs its in-sample half only;
  `holdout: true` shows both halves and appends to
  `<state dir>/backtests/holdout-reads.jsonl` (`holdout read #n for this spec`) —
  never show the model a holdout uncounted; its runs are read by run id
  (`backtest` `run_id`), never by path (the state dir is outside every fs root).
  The operator's `tengu backtest --split` counts its reads too (`via = "cli"`).
  Every report records `data_through_ms` (the newest row it read); `tengu backtest
  --data-through <it>` reruns on the same data after `market.db` grew.
  Judge a Jev gate with `tengu evidence evaluate <run dir>` (rules · Jev · HOLD on
  the same candidates): Jev is PROVEN only when it beats rules and HOLD per
  candidate — W1's is UNPROVEN (`docs/p6-decision-evaluation-2026-10-08.md`).
- **Strategy ranking (2026-10-08, `docs/strategy-ranking-automation-2026-10-08.md`)** —
  a ranking contract is a lineage record `lineage/rankings/<id>.toml`, sealed by
  the operator (`tengu lineage seal ranking:<id>`) before any ranking: unsealed =
  verify Warn `ranking_unsealed`. The publisher (`application/ranking/`, behind
  `tengu ranking run|show` and the opt-in tool `strategy_ranking`) reloads the
  registry on every run and refuses `contract_unsealed` / `contract_changed` —
  never seal a contract or run a ranking on `~/.tengu/state/xlab` yourself (SR-1:
  the operator's call). `[strategy_ranking] registry, contracts`
  (`config/strategy_ranking.rs`) lists a sandbox's contracts; `sandboxes/xlab-w2`
  (unbound) runs the daily + weekend contracts by the feeds `history_refresh` →
  `strategy_ranking_daily` / `strategy_ranking_weekend` on the private
  `xl_ranker`. Output `<state>/strategy-rankings/<contract id>/<date>/` +
  `latest.{md,json}`; a published date (COMPLETE or INCOMPLETE) is never
  rewritten — an INCOMPLETE one reruns only after its date dir is deleted. Rules
  arms only (`NOT_GATED`), never a split. Retention (`keep_runs`) keeps every run
  the `[strategy_ranking]` registry cites (bound or not) and every run a
  `latest.json` names. `lineage/rankings/` fails a pre-2026-10-08 binary's W1
  load (unknown registry dir): keep it out of a checkout a frozen weekend binary
  reads.
- **`workspace_tools` is a narrow allow-list** — only the opt-in tool names
  in `domain/tools.rs::WORKSPACE_TOOLS` (memory and skill-lifecycle tools,
  the Solana, Hyperliquid and xmarket families — read the list there, don't
  copy it); anything else fails config validation, and listing one in
  `tools` opts it in too. `manage_skill` is the canonical unified skill
  write API (see `outbound/tools/manage_skill/`); `agentic_memory` is the
  Postgres-backed Open Brain memory tool (`postgres_memory` feature).
- **One agent schema (2026-09-18)** — `agents/*.toml` and `AgentSpec` are
  gone. A subagent is an `[agents.<name>]` block with a `description`;
  `bootstrap::tools::subagent_config` merges workspace-tool names found in
  `tools` into `workspace_tools` (filtered by `WORKSPACE_TOOLS`).
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
  bridge (the agent's folded scopes loaded from `TENGU_CONFIG`;
  `TENGU_BRIDGE_SCOPES` only as its fallback) use the configured scope for
  each tool and `permissive_scope` only for tools with no entry. A configured scope is deny-by-default per field: an
  `http_request` scope with empty `fs_roots` denies multipart file uploads.
  Subprocess children get their own workspace added to every inherited
  scope's `fs_roots` (`grant_workspace_root`) — except a deny-all scope (every
  field empty, e.g. xmarket's `[default_scopes.write_file]`), which stays a
  deny (`tengu doctor --engines` skips an agent whose own scopes deny the
  smoke tools — jev-exec's architect). `ToolScope::check_env_read` honours
  the `"*"` wildcard like `net_hosts` / `shell_bins`. `shell_bins` gates a
  command's first command word only (`domain::scope::shell_command_binary`:
  leading `NAME=value` skipped, so `DEK=… node` checks `node`) — a guard
  rail, not a sandbox: `;`, pipes, `$( )` and `node -e` run unchecked.
- **Config resolution** — `--sandbox <name>` replaces the base config
  wholesale; otherwise `--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`.
  The `run-agent` child gets the same file: `--sandbox` travels over IPC and
  `main` pins `TENGU_CONFIG` to the resolved path so `-c/--config` reaches
  children and the MCP bridge too, and `load_sandbox_or` re-pins it to the
  absolute sandbox file; each takes `[agents.<name>]` from it. `Config` is
  `deny_unknown_fields` (2026-09-30): an unknown or misspelled top-level key
  (`[rsik]`) fails the load — a new section must be a `Config` field first
  (`config::risk::tests` loads every `sandboxes/*/config.toml`).
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
  `run_turn_with_system` in `bootstrap/` sets `tools = []`,
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
  each surface resolves the id ONCE (`bootstrap::orchestrator::resolve_session_id`:
  env override `TENGU_SESSION_ID` > fresh UUID — `tengu chat` once per
  process, `tengu telegram` once per sender (override + `-<sender>`), `tengu
  eval` once per row; `tengu webhooks`
  mints `webhook-<name>-<uuid>` per request) and hands it to `build_orchestrator`,
  which passes the same string to `RagPlanner::new(...)` AND
  `SubprocessRunner::new(sandbox_name, session_id, agents)`.
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
  `"openrouter"`, `"local"` or `"claude_code"` (required field, no default);
  the same block serves in-process chat and `run-agent` steps.
  `model` slug format depends on engine: OpenRouter wants
  `anthropic/claude-sonnet-4-6`; Claude Code wants the bare `claude-sonnet-4-6`
  (building with `--features claude_code` is required); `local` sends the
  server's own id verbatim.
- **`engine = "local"` = any OpenAI-compatible server on this host
  (2026-09-23)** — Unsloth (`unsloth run`, default `http://127.0.0.1:8888`,
  key in `$UNSLOTH_API_KEY`), Ollama, llama.cpp, vLLM. Optional
  `[agents.<n>.local] base_url` / `api_key_env`. Own engine
  `engines/local.rs` (`LocalEngine`); direct connection, never via the
  `[egress]` proxy. Set
  `limits.context_window` — the 1_000_000 default is wrong for local models
  (load warns) and must equal the served window (Ollama:
  `OLLAMA_CONTEXT_LENGTH`). Local agents get each tool result capped at 1/8
  of the window; a typed row above that cap is compacted (`data` → a pointer
  to the store key), one that fits arrives whole
  (`Engine::tool_result_char_cap`, `Observation::compact_text`);
  `base_url` may end in `/v1`. Guide: `docs/engine-backends.md` § Local.
- **Open-network sandboxes** — `lping`, `jev-exec` and `unlimited` (RPC, market APIs, latency)
  and `control-loop-lab` (Jev only) run `network = "open"`; `xmarket` (M0 stage) and
  `xmarket-weekend` run `open` with `allow_hosts = ["api.hyperliquid.xyz"]`, `xlab` with
  `allow_hosts = ["api.hyperliquid.xyz", "api.geckoterminal.com"]`, `xlab-w2` with those two
  and `www.sec.gov`, `data.sec.gov`, `soe` with `www.sec.gov`, `data.sec.gov`,
  `api.ted.europa.eu`, and must stay switchable to Tor
  (every transport through `egress.rs`). `tor-check`, `storage-test` and the
  base config run over Tor.
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
- **Adding a new tool: ONE catalog row (2026-09-23)** — `catalog()` in
  `adapters/outbound/tools/mod.rs`; `register_catalog` (in-process executor
  AND MCP bridge) and `advertised_defs` both read it. Opt-in names also go in
  `domain/tools.rs::WORKSPACE_TOOLS` (config validation + `subagent_config` +
  bridge filter). `SkillPlugin` / `McpPlugin` are registered outside the
  catalog; the bridge registers `McpPlugin` only for servers the Claude Code
  engine names (`TENGU_BRIDGE_MCP_SERVERS`: names only, each taken from the
  bridge's loaded config — no `${VAR}`-expanded value in the temp file).
- **`[[mcp_servers]]` tools are named `{server}__{tool}` (2026-09-23)** — was
  `{server}.{tool}`; model APIs reject `.`. They reach plan-step subagents
  (both engines), in-process TUI/Telegram agents, webhook and eval agents. A
  tool whose schema leaves the engine subset (`tools/schema_lint.rs`) is
  dropped at discovery with a warn naming server, tool and rule
  (`mcp_client::linted_tool`, 2026-10-01).
- **Layering is lint-enforced (2026-09-23)** — `tests/layering_lint.rs` fails
  when `domain` / `ports` / `config` / `application` import an adapter, or
  outbound imports inbound/bootstrap. Need something from an adapter in a use
  case? Add a port in `src/ports/`, implement it in `adapters/outbound/`, wire
  it in `src/bootstrap/`.
- **`compress_and_store` with Claude Code subagents** — served through the
  bridge since 2026-10-01: a `run-agent` step's bridge writes the `summary`
  to the step's `TENGU_BRIDGE_SUMMARY_FILE`, read back as the IPC summary
  (+ the `agentic_memory` write with `postgres_memory`), and answers
  `stored — stop now`; the engine then ends the CLI run once that round's
  calls are answered (as the in-process loop stops after its round). A subagent that just
  stops still passes: the Phase 5c middle-ground protocol makes the final
  assistant text the IPC summary, the `run-agent` backstop
  (`try_persist_agentic_step_summary`) captures it into Postgres
  `agentic_memory` for within-session recall, and the
  `model finished without calling compress_and_store` warn is emitted.
- **Memory backend** — `agentic_memory` (Postgres + pgvector, behind the
  `postgres_memory` feature) is the only durable runtime memory: planner
  user-message recall, within-session step-output recall, replan cross-plan
  recall, and subagent summary capture all read/write it. The legacy Qdrant
  `rag/` facade and the `qdrant` cargo feature were removed in Phase 6
  (2026-05-14). The only built-in `VectorStore` is now the disk-backed bincode
  store. Every Open Brain reader and writer (planner recall, step summaries,
  webhook output, the tool, `capture` included) embeds with
  `text-embedding-3-small` (1536-dim, `domain::memory::DEFAULT_EMBEDDING_MODEL`)
  because the Postgres schema hardcodes `vector(1536)`; `[memory]
  embedding_model` only picks the workspace vector store's model. A
  wrong-dimension vector warns and falls back to text-only writes / FTS-only
  recall (fail-soft).
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
  config). JSONL audit at `<TENGU_HOME>/logs/egress.jsonl` (`agent` /
  `session` / `call_id`: a `tengu run` feed or loop call names its own —
  `egress::AttributedExecutor` — else the process env). `tengu doctor
  --tor` verifies the exit; `make tor` runs the proxy (`deploy/tor/`: Arti +
  lyrebird-rs from `../lyrebird-rs`). Unit tests must not call
  `egress::install` (process-global).
- **MCP bridge tool names are prefixed `mcp__tengu-tools__<name>`** — when
  Claude Code calls a tengu tool through the bridge, the model sees
  `mcp__tengu-tools__persistent_store`, not bare `persistent_store`. Skills
  that say "check that tool X is in your tool list" should look for both
  forms or just attempt the call and read the error.
- **Every tool must work under every engine — `openrouter`, `local`,
  `claude_code` (operator rule 2026-09-30, no exceptions)** — step 4 of "How
  to add a new tool". E0 is closed (`x-engine-parity-audit`, 2026-10-01):
  every catalog tool, shell skills and `[[mcp_servers]]` proxies pass the
  schema lint, a bridge conformance case and a live engine-matrix leg
  (`every_catalog_tool_has_a_live_leg` fails CI for a tool in no set; gap
  list in the tracker's W1 notes). `tengu mcp-bridge` loads `TENGU_CONFIG`
  (absolute) and runs tools as `[agents.<TENGU_BRIDGE_AGENT>]` — folded
  scopes, `AgentConfig::sandbox` sections, `no_shell_fallback`, `[memory]`,
  shell skills (`skill_packages` + the requested names) — behind
  `SanitizedToolExecutor` (text, observations and errors redacted, on every
  surface — vault values, `TENGU_MASTER_PASSWORD` and env vars named
  `*_API_KEY` `*_SECRET` `*_TOKEN` `*_PASSWORD` `*_PRIVATE_KEY`, `.env` too;
  never a public on-chain id: `domain::secrets::is_env_secret`), call id
  `mcp:<nonce>:<id>`; no config / unknown agent → default `main` +
  `TENGU_BRIDGE_SCOPES` with a warn. A `run-agent` step's engine
  (`engines::build_step_engine`) writes `TENGU_BRIDGE_GRANT_WORKSPACE=1` +
  `TENGU_BRIDGE_SUMMARY_FILE` into the bridge env: the workspace grant, and
  `compress_and_store` served into that file (the step's IPC summary);
  elsewhere the bridge refuses it with the reason. Each bridged call gets the
  run's conversation (`TENGU_BRIDGE_TRANSCRIPT_FILE`, the engine's 0600
  transcript; `tengu tool call --transcript`) — `skill_distill` refuses a call
  with none. A `run-agent` step always has a workspace — the agent's, else a
  temp dir per step (webhook / `tool turn` one-shots: per turn;
  `bootstrap::tools::workspace_or_temp`) — for its executor, the CLI's cwd
  and the bridge; its
  results are capped and older rounds compacted as in chat, every engine;
  `limits.max_tool_rounds` counts tool calls on `claude_code`, engine turns
  elsewhere. The temp `--mcp-config`
  holds no secret value (`[[mcp_servers]]` by name) — the CLI merges its `env` over the inherited env
  (`docs/mcp-bridge.md` § Env); the engine strips a parent Claude Code
  session's env (`CLAUDECODE`, `CLAUDE_CODE_*` but auth / provider,
  `CLAUDE_PID`, `CLAUDE_EFFORT`) and passes `--strict-mcp-config`. Every
  surface honours `tools` (`bootstrap::tools::agent_base_tools`; empty = every
  base tool; `[[mcp_servers]]` tools too); in-process chat call ids are
  `chat:<turn nonce>:<round>:<i>:<provider id>`. Still open: the live `local`
  legs (the operator's PC), `agentic_memory` live (Postgres), Privy signing /
  Solana `send` legs (never run).
- **Hardened sandboxes (2026-09-30, `config/hardening.rs`)** — a `[solana]`
  signer or a `[risk]` section: every `claude_code` agent must set
  `[agents.<a>.claude_code] builtin_tools_profile = "none"` (no block =
  `editor_shell` = load error; the value is read trimmed, an unknown one is a
  load error), and `fold_default_scopes` sets `no_shell_fallback` on every
  agent (in-process and bridge fallbacks run no shell; no shell skill loads —
  `SkillRegistry::with_shell_skills`). A `none` agent's CLI runs with
  `--setting-sources "" --disable-slash-commands --settings
  {"autoMemoryEnabled":false,"disableAllHooks":true}` (`claude_code.rs::cli_args`):
  no settings files, hooks, installed plugins, skills, CLAUDE.md / AGENTS.md
  discovery or auto-memory (CLI 2.1.286: OAuth + bridge verified live;
  `--safe-mode` drops the bridge, `--bare` OAuth). A plan step's `compose`
  may only narrow its base agent there (`run-agent` refuses a widening one).
- **Workspace writers refuse agent / CLI state (2026-10-01)** — `write_file`
  (`tools/args.rs::validate_write_path`) resolves the path first
  (`domain::scope::resolve_path`: symlinks followed, `..` applied —
  `new/../../x` used to land outside the workspace once `new` was created),
  then refuses `.tengu/`, `.claude/`, `.git/`, `skills/` at any depth and
  `CLAUDE.md` / `CLAUDE.local.md` / `AGENTS.md` / `.mcp.json` files
  (case-insensitive) in every sandbox; in a hardened one (`AgentConfig::hardened`)
  also the system-prompt files `MEMORY.md` / `USER.md` / `IDENTITY.md` /
  `PROFILE.md` / `CONTEXT.md` (`domain::scope::protected_write_in`; elsewhere
  agents keep their profile files current); `manage_skill` /
  `apply_improver_proposal` resource paths and `agentic_memory` wiki titles
  refuse the same names (`domain::scope::protected_write`).
  Not covered: Claude Code built-ins (profiles other than `none`) and
  `run_command`.
- **Telegram fails closed (2026-10-01)** — `tengu telegram` refuses to start
  without an allow-list (`[telegram] allowed_users` + `TENGU_TELEGRAM_ALLOWED_USERS`);
  an unlisted sender gets "Unauthorized.". A private agent (no `description`,
  not `default` — the exec-tool / signing owners) is never `@<id>:` /
  `@<role>:`-routable, never the default and not listed in `/agents` or the
  team block (`telegram.rs::telegram_reachable`). `[telegram] tool_approvals`
  / `approve_only` are NOT implemented — no tool call waits for an approval,
  on any engine or surface; they still load and
  `Config::load` warns naming them (`TelegramConfig::approvals_warning`).
- **Sandbox sections reach tools via `AgentConfig::sandbox` (2026-09-30)** —
  `config/sections.rs::SandboxSections` (one `Arc` per config, set by
  `fold_default_scopes`): the sandbox name (`<name>` of the file
  `sandboxes/<name>/config.toml` however loaded, else `default` — the paper
  ledger's account owner), `[xmarket]` state dir (`<TENGU_HOME>/state/<state>`,
  install-wide stores), `[risk]` / `[paper]` (every field required,
  `config/risk.rs`), `[xmarket.calendars.*]`, `[rate_limits.<name>]`,
  `[recorder]`. A new section a tool reads goes there — never a new
  `#[serde(skip)]` field on `AgentConfig`. Every surface (in-process,
  `run-agent`, loops, `tengu run`, bridge) sees the same values.
- **`tengu run --sandbox <s>` (2026-09-30, `docs/runtime-2026-09-30.md`)** —
  one runner per sandbox and one owner per `[xmarket]` ledger (leases
  `runtime:<s>` + `state:<dir>` in `<state dir>/runtime.db`, TTL 30 s;
  `tengu webhooks` takes the same — a second process exits 1; each ledger
  account records the sandbox that first wrote it, another sandbox's tools are
  refused `account_owner_mismatch`); every `[decision_loops.*]` built once
  behind `LoopDispatch` (one event per loop at a time, `[runtime]
  max_decisions_in_flight`; past `max_queued_per_loop` waiting events, 64 by
  default, one more is refused with a warn — webhook 429); `/webhooks/:name` with `--features webhooks`;
  SIGINT/SIGTERM drain ≤ `shutdown_grace_secs`; heartbeat
  `<state dir>/run-<s>.json` + `loop/1` / `feed/1` rows; `tengu doctor
  --sandbox <s> --live` is the healthcheck for a `tengu run` container (the
  shipped image runs `tengu telegram` + plain `tengu doctor`). `tengu risk resume` may
  also ask for a secret (`TENGU_RISK_RESUME_SECRET_FILE`, 0600; a terminal is
  not a human). Budgets: `[rate_limits.<name>]`
  (`outbound/rate_limit.rs`, per process; unconfigured = unlimited); backoff
  `domain/backoff.rs`; HTTP errors classify through `outbound/http_class.rs`;
  Hyperliquid `POST /info` via `outbound/hyperliquid/info.rs` (`500 null` ⇒
  not applicable). Time: `domain/tz.rs` (NY / Paris DST), `domain/calendar.rs`
  (NYSE holidays, weekend window), `ports/clock.rs`.
- **Tengu Studio (2026-10-09, `docs/studio-2026-10-08.md`, tracker
  `TENGU_STUDIO_PLAN.md` § 8)** — the server needs `cargo build --features
  studio` (not default; without it `tengu studio` fails naming the flag;
  `tengu studio graph` and `tengu trace` work in every build). Loopback only;
  the token is printed once, in the URL fragment; any request but GET / HEAD,
  on any path, needs the `X-Studio-Token` header + its own `Origin` +
  `Sec-Fetch-Site: same-origin`; the server logs to `tengu.log` like `tengu run`.
  Play = `inbound::run::start_session` inside the Studio process (the `tengu
  run` lease: a CLI run and Studio refuse each other), Stop = the SIGINT
  drain; control only with `[studio] control = true` (only `control-loop-lab`)
  or `--allow-control`, never `[generation]`-bound or hardened (a load error).
  The page draws what Rust serves; its assets are `include_str!` — rebuild
  after a `web/studio/` edit. **Lab `TENGU_HOME` isolation:** `export
  TENGU_HOME="$HOME/tengu-lab/home"` before any lab `tengu` — dotenvy loads the
  nearest `.env` up from the cwd and never overrides an exported var, so a
  `tengu` started in a worktree under `.claude/worktrees/` otherwise inherits
  the main checkout's `.env` (`TENGU_HOME=~/.tengu`, the weekend run's live
  state). `--sandbox <name>` is cwd-relative (`sandboxes/<name>/config.toml`,
  `bootstrap/sandbox.rs`): run from the repo / worktree root. The visual editor
  is a design only (`docs/studio-editor-design-2026-10-08.md`; no save until
  Operator Review #3).
- **Decision loops (2026-09-24)** — `[decision_loops.<name>]`
  (`config/decision_loop.rs`) runs a System One model (`~typesafe/jev-latest`
  via OpenRouter `/api/alpha/decisions`, `outbound/decisions.rs`) that picks
  the next action + its argument slots; existing tools execute it through the
  loop agent's executor (same scopes/egress as a `run-agent` child).
  Jev returns typed choices, never text or tool-call JSON — it cannot be an
  `engine`; slots also BIND one value (2026-10-06: `{ event = "/x" }`,
  `{ from, path }`, `{ observation, path }` — no question, unresolved ⇒ the
  action is illegal; exact amounts from `/data/…`), so a higher-order agent
  hands values via the event and one tool's output feeds the next (lping
  `hedge_exec` / `lp_exec`). A `sequence = ["a", "b?"]` makes the loop offer
  one step at a time (`?` skipped when it cannot run; a failed / refused step
  halts), and `tengu decide --map <file|->` runs an Architect's JSON execution
  map that can only NARROW a loop (`config/execution_map.rs`, skill
  `execution-map`; audit `trigger = "map:<sha256>"`). `dry_run` defaults to
  true (a dry-run write no longer ends the event); low confidence (`act_at`) escalates to
  the orchestrator. Triggers: webhook endpoint `loop = "<name>"` (Helius uses
  `auth_header_env`, not HMAC), `tengu decide` or `tengu run`. History is
  in-process; tool-call ids are `{loop}:{session_id}:{t}` (→ `ToolCtx.call_id`;
  exec tools key idempotency on a `client_order_id` arg, else `call_id`; an
  arg never starts with `exit:` `fade:` `fade-shadow:` `feed:` `mcp:` `chat:`,
  and a replay asking for another order is refused — `client_order_id_conflict`).
  Jev retries once on 429 / 5xx and has a 30 s circuit breaker. Audit in
  `<TENGU_HOME>/logs/decisions.jsonl` — one `write_all` per line, a line for a
  failed Jev call (`outcome = "error"`), `ts_ms` / `latency_ms` / `sandbox` /
  `act_at` (incl. `args`, `ok`, `output`);
  `tengu chat` on a config with `[decision_loops]` tails it and shows each
  decision of those loops as a System bubble. Plan:
  `docs/decision-loop-plan-2026-09-24.md`.
- **Typed observations + cache (2026-09-24)** — a tool may return
  `ToolOutput.observation` (`domain/observation.rs`); `execute_typed` carries
  it (the default wraps `execute`; `PluginToolExecutor` and
  `SanitizedToolExecutor` override it). Typed tools cache through
  `application/observe.rs::observe()` in `<workspace>/.tengu/observations.db`
  (open it with `outbound::observations::open_observation_store(workspace,
  &agent.sandbox)` — with `[recorder]` on it also appends every live result,
  `Error` and ttl-0 rows too, to `<state dir>/history/<YYYYMMDD>.db`; read
  with `tengu history range|asof`): key `<schema>:<subject>` with full ids,
  slot-monotonic, `Error` rows never cached, `max_age_secs = 0` forces a live
  read. Failed reads are
  `Field::Error` / `ObsStatus`, never 0; `features` ≤ 32 scalars; line 1 of
  `render_text` ≤ 200 chars with full ids. Decision loops read rows via
  `world` (never fetched) and gate actions with `requires`. The 11 Solana
  tools (`tools/solana/`) are opt-in; IO tools need
  `[default_scopes.<tool>]` with `fs_roots` = the workspace (store), their `net_hosts`, and
  `env_reads = ["SOLANA_RPC_URL"]` — without it the public RPC is used
  silently; the RPC URL is never rendered (host only). `hedge_decide` /
  `lp_decide` knobs are all required (no defaults); `commit` defaults to
  false. Doc: `docs/typed-observations-2026-09-24.md`.
- **Solana write tools (2026-09-29, phase 6b)** — `solana_close_token_accounts`,
  `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position`,
  `jup_perps_order` (opt-in rows; runner `tools/solana/write_common.rs`,
  pipeline `outbound/solana/send.rs`). `mode = "simulate"` (default) is
  keyless. `mode = "send"` needs BOTH `[solana] signer_key_file` (0600, key =
  the `wallet` arg) AND `wallets = ["<full pubkey>"]` in that agent's own
  scope for the tool — never in `[default_scopes]`, only on an agent with no
  `description`, not `default`, no webhook `agent`. With a signer,
  `Config::load` refuses `claude_code` agents unless `builtin_tools_profile =
  "none"` (`config/hardening.rs`), `[[mcp_servers]]`, any scope granting
  `shell_bins`, and a key inside any fs root / workspace (`config/solana.rs`);
  the permissive fallback then runs no shell. Sends are
  serialized per wallet by a lease in `<TENGU_HOME>/state/solana-writes.db`
  (+ pending record resolved before the next send, + write fence that makes
  older `lp_snapshot` rows `stale_input`) — it cannot see the TS bot, so
  never sign with a wallet the bot runs. Doc:
  `docs/typed-observations-2026-09-24.md` § Write tools.

---

## Open items still on the list

See `docs/SESSION_HANDOFF.md` for the running list. **Local data to clean up** (not in git; delete a group only on the operator's word): `docs/SESSION_HANDOFF.md` § Local data to clean up later. State 2026-10-08: `TENGU_ROADMAP.md` P0–P5 done (evidence vault +
forward grading, the lineage registry, W1 frozen + generation binding);
Operator Review #1 = APPROVE (`docs/w1-review-2026-10-06.md` § Verdict); Phases 6–11
done 2026-10-08 (`docs/p{6,7,8,9,10}-*-2026-10-08.md`): no W2 change beats rule W, W1
kept — next: forward evidence every weekend (Review #2 not reached). Before that, 2026-10-02: xmarket W1 +
its gate done, `xlab` built. Next, in order: the operator decisions
(`docs/xmarket-tracker-2026-09-29.md` § 0 + W1 notes) → W2
(`docs/xmarket-build-plan-2026-09-30.md`: status, waves, W2 kickoff prompt at its
end), judged on history first (`docs/xlab-2026-10-01.md` § 12 Next). The weekend
run is optional; the live `local` engine legs run on the operator's PC.
As of 2026-05-14, the
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
   `cargo run --release -- chat --sandbox lping` (add `--features
   postgres_memory` to exercise Open Brain recall), type "what is the BTC
   price?", and follow the logs. The flow is in §1 of
   `docs/architecture-2026-04-27.md`.
2. **Open `docs/index.html` (the docs hub) or an HTML explorer in a browser** —
   visual first: `docs/tutorial/index.html` (one animated page per feature);
   searchable: `docs/code-map.html` (where X lives, recipes),
   `docs/architecture-2026-04-27.html` (walk a turn),
   `docs/context-management-2026-04-27.html` (what an LLM sees); interactive:
   `docs/context-cutting-flow-2026-04-27.html` (the inner tool loop). Type the
   symbol you're looking for; it'll surface which subsystem owns it.
3. **`SESSION_HANDOFF.md` "Active gotchas" section** — most weird symptoms
   are documented there with the fix.
4. **If you're about to add a new abstraction layer** — stop and check
   whether the change can be expressed in TOML / SKILL.md / config instead.
   The doctrine prefers data changes over code changes.

---

*Last updated 2026-10-09 (Tengu Studio: `tengu studio` — graph, trace, live + replay, Play / Stop for the lab; operator doc `docs/studio-2026-10-08.md`, editor design only — "Beyond chat" row, REQUIRED updates row, gotcha; before that 2026-10-08 Operator Review #1 = APPROVE → Phase 6; before that 2026-10-07 visual tutorial `docs/tutorial/` — one animated page per feature, built from the code — and the rule that every code change updates its pages: REQUIRED updates + `tests/tutorial_map.rs` + `.claude/settings.json` hook; before that 2026-10-06 TENGU_ROADMAP P0–P5: `tengu evidence` vault + grade + regrade, the `lineage/` registry + `tengu lineage`, W1 frozen and `[generation]`-bound — gotcha above, required reading 13; before that 2026-10-02 docs refresh: "What this project is" names `tengu run`, the xmarket paper desk and xlab; the layer table lists the new ports / stores; "stuck" starts at `docs/index.html` + the HTML explorers; open items = operator decisions → W2; before that 2026-10-01 xlab: history-first sandbox for the operator's PRD v0.5 — market.db + backfill (HL, GeckoTerminal, HL S3 archive), strategy specs, the pure backtest engine with time-integrity checks, Jev replayed on history with a decision cache, tools `market_history` / `backtest`, operator rule "history first" — gotcha above; before that W1 gate passed — weekend-path, money-safety and engine-parity reviews fixed: ledger fixes (exit backoff, shadow paper-only, replay fingerprints), batch 2 (step temp workspace + bridge transcript, local rows whole under the cap, eval bridge + redaction, kept venue facts + funding owed, opportunity side/strategy, state-dir lease + ledger owners, Telegram approval keys warn); before that W1-gate safety fixes, access: a deny-all scope stays a deny in `run-agent`, hardened `compose` only narrows, writers refuse `.tengu/` / `.claude/` / `CLAUDE.md` / `AGENTS.md` and resolve `..`, Telegram fails closed without an allow-list, a `none` claude_code agent runs without settings / hooks / plugins — gotchas above; before that `x-engine-parity-audit`: E0 closed — every catalog tool, shell skills and `[[mcp_servers]]` proxies on every engine; chat honours `tools`; the bridge serves shell skills and a run-agent step's `compress_and_store`; tool errors redacted on every surface — gotchas above; before that 2026-09-30 xmarket W1 wave A landed: bridge parity + hardened sandboxes + schema lint + local-model fit, `AgentConfig::sandbox` sections, `[risk]` / `[paper]` / `[rate_limits]` / `[recorder]` / `[runtime]` / `[xmarket]`, `Config` `deny_unknown_fields`, `tengu run` + `doctor --live`, history recorder — gotchas above; before that the operator rules: every tool must work under every engine — `openrouter`, `local`, `claude_code` — no exceptions; build plan `docs/xmarket-build-plan-2026-09-30.md` — "How to add a new tool" step 4 + gotcha; previously 2026-09-29 Solana write tools + local key signer + signing-sandbox rules — `docs/typed-observations-2026-09-24.md` § Write tools; previously 2026-09-24 typed observations + observation cache + Solana LP read tools; previously 2026-09-23 hexagonal layout — `src/{domain,ports,config,application,adapters/{inbound,outbound},bootstrap}`, one tool catalog, `docs/code-map.{md,html}`; previously 2026-09-18 Tor-by-default egress, single sandbox config — `agents/` removed, deploy/tor = Arti + lyrebird-rs; previously 2026-09-12 audit pass, 2026-05-14 agentic-memory MVP — Open Brain Postgres + pgvector
behind `postgres_memory`; planner registry moved to file-backed
`TENGU_PLANNER_REGISTRY.md`; doctrine is now "Open Brain + Karpathy LLM Wiki =
brain"). If you're reading this in the future and the companion doc filenames
have rolled forward (e.g. `architecture-2026-05-12.md`), update the references
in this file too. Stale references in `AGENTS.md` are the worst kind of stale —
and remember to mirror every change into `CLAUDE.md`.*
