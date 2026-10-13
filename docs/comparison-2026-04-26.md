# Tengu-Cluster vs Hermes Agent vs PI/Cowork — 2026-04-26 Snapshot

> **Superseded note (2026-05-14, extended 2026-09-18):** Tengu memory/routing has moved from the
> old vector-registry snapshot to Open Brain live memory + Karpathy LLM Wiki
> compiled Markdown + file-backed planner registry. Since 2026-09-18: one config
> per sandbox (`agents/*.toml` + `AgentSpec` deleted — a subagent is an
> `[agents.<name>]` block with a `description`), Tor-by-default egress
> (`[egress] network = "tor"`, code-enforced in `src/adapters/outbound/egress.rs`), no
> Qdrant / `rag/`. Tengu facts below are corrected in place; the
> "What landed today" table is historical.
>
> **2026-10-02:** Tengu gained trading-runtime axes (typed observations,
> System One decision loops with Jev, an in-tool risk gate + paper ledger,
> history-first backtesting, engine parity through the MCP bridge) — new
> section [Trading runtime axes](#trading-runtime-axes-added-2026-10-02). For
> Hermes and PI/Cowork it states only what this snapshot already establishes;
> anything else is marked "not covered here".
>
> **2026-10-08:** Tengu column refreshed against the code — research lineage +
> forward evidence (`tengu lineage`, `tengu evidence`), sealed strategy
> rankings (`tengu ranking`, tool `strategy_ranking`), the source evidence
> layer (`tengu sources`, tool `source_evidence`) and the offline Software
> Opportunity Engine (`tengu soe`); tool and engine-matrix counts.
>
> **2026-10-09:** Tengu column refreshed again — the SOE weekly cycle (stage
> agents + `soe_*` tools, replay, grades), the execution trace (`tengu trace`)
> and Tengu Studio (`tengu studio`, a loopback browser UI) — § Observability.
>
> Companion to `docs/tengu-analysis.html` (the deep version). This doc is the
> obsidian-friendly **state snapshot** taken at the end of the 2026-04-26
> session that landed per-example vectors, TUI debug panel, and durable
> user-message persistence. See also: `docs/comparison-2026-04-26.svg` for
> the visual.

---

## TL;DR per project

| | Tengu-Cluster | Hermes Agent | PI / Cowork |
|---|---|---|---|
| **Language** | Rust | Python | TypeScript + Claude Agent SDK |
| **Origin** | Vibe-coded v2 redesign | Nous Research, mature | Anthropic, this product |
| **Surfaces** | TUI (cursive), Telegram, webhooks listener; long-running `tengu run` (feeds + decision loops); Tengu Studio (`tengu studio`, a loopback browser page, default build); CLIs `tengu decide`, `tengu backtest`, `tengu history`, `tengu ranking`, `tengu lineage`, `tengu evidence`, `tengu sources`, `tengu soe`, `tengu trace` | CLI, Telegram, Discord, Slack, WhatsApp, Signal | Desktop app, browser ext |
| **Models** | OpenRouter (any) + `local` (any OpenAI-compatible server: Ollama, llama.cpp, vLLM, Unsloth) + Claude Code engine; Jev (`~typesafe/jev-latest`) as the decision model of `[decision_loops]` | 200+ providers, multi-backend terminal | Sonnet / Opus / Haiku |
| **Persistence** | Open Brain Postgres + Karpathy LLM Wiki Markdown + file registry; SQLite stores for typed observations, the paper ledger, the market-data warehouse and the source store (`sources.db`); frozen SOE run dirs; JSONL execution traces; `lineage/` TOML registry + read-only evidence vaults | SQLite + FTS5 + Honcho dialectic model | CLAUDE.md + plain-text memory files |
| **Network** | Tor by default (`[egress] network = "tor"`: Arti + lyrebird-rs proxy, host allow/deny ceiling, JSONL audit — code-enforced, fail-closed; `"open"` per sandbox) | not a first-class feature | platform-managed |
| **Strength** | Doctrine clarity (LLM=heart, Open Brain / Karpathy LLM Wiki=brain, tools=hands) | Breadth: platforms, scheduler, self-improving skills | UX: artifacts, computer-use, MCP marketplace |
| **Weakness** | Agent-layer features partial / vibecoded gaps | Heavier deployment surface, Python perf | No vector memory; no autonomous skill evolution |

---

## Memory model

### Tengu

Three stores (2026-09-18 state — the 2026-04-26 Qdrant collections are gone):

- `TENGU_PLANNER_REGISTRY.md` — Markdown roster rendered from the `[agents.<name>]` blocks that carry a `description` (+ `example_queries`), skills, core + MCP tools. Regenerated every planner turn (`shared_files::render_registry`); no embeddings, no reindex step.
- Postgres `agentic_memory` (pgvector + FTS, feature `postgres_memory`) — user messages (`RagPlanner::persist_user_message` on every `plan()`), step summaries (`compress_and_store` + the `run-agent` backstop), `promote` → `compile_wiki` for the Karpathy LLM Wiki. Read back by three planner lanes: cross-session (`[memory] cross_session_msg_top_k`), within-session (`within_session_output_top_k`), cross-plan on `replan()` (`cross_plan_top_k`).
- Disk bincode vector store (`adapters/outbound/memory/disk_vector.rs`) — in-process `memory_ingest` / `memory_search` / `persistent_store`; the only built-in `VectorStore`.

Plus an in-memory ring buffer in `RagPlanner` for within-session "recent user messages" (`session_recent_n`). Lost on restart.

**What's missing**: LLM summarisation of old sessions; a Postgres-native inspect CLI (`tengu memory inspect` was removed with Qdrant).

### Hermes

SQLite database with FTS5 full-text search across all sessions. LLM-summarisation pass condenses old conversations into a compact, queryable form. **Honcho** provides a dialectic model of the user (preferences, projects, tone). Periodic memory nudges prompt the agent to persist facts that should outlive the current session.

**What it does that Tengu doesn't**: continuous self-improving user-model, automatic summarisation, queryable across years.

### PI / Cowork

Plain Markdown files in a memory directory. The `consolidate-memory` skill periodically merges duplicates and prunes the index. CLAUDE.md anchors per-project context.

**What it does that the others don't**: human-readable memory the user can grep, edit, version-control directly.

**What it lacks**: vector recall. The agent can't ask "what did I learn about X six months ago that's semantically similar to this question?"

---

## Skills + self-adjustment

| | Tengu | Hermes | PI/Cowork |
|---|---|---|---|
| **Discovery** | scanner, first name wins: managed `~/.tengu/skills`, workspace `.tengu/skills`, workspace `skills/`, then the cwd's `skills/` (`skills::registry::skill_directories`) | Skills Hub + agentskills.io standard | Plugin marketplace + per-session skill list |
| **Trigger** | Planner LLM reads `TENGU_PLANNER_REGISTRY.md` (descriptions + `example_queries`) and picks | Description-match + slash command | Description-match (slash commands) |
| **Creation** | tools `skill_distill`, `manage_skill`, `view_skill` (the orchestrator skill routes lifecycle verbs to a `learning-agent` — no shipped sandbox defines one); `tengu skill install` | Autonomous skill creation after complex tasks | `skill-creator` skill, manual / LLM-assisted |
| **Self-improvement** | `tengu skill evolve` (metric-gated, `[skill_lifecycle] improver_agent`, approval gate — only a commented sample in `config.example.toml`) — no post-turn auto-trigger | live in-use refinement | none |
| **Standard** | own format (SKILL.md + frontmatter) | agentskills.io | SKILL.md + frontmatter (close to Hermes) |

The big gap for Tengu here: **Hermes has a closed learning loop** — it creates skills from experience, refines them as it uses them, and persists the refinements. Tengu has the *building blocks* (`skill_distill`, `manage_skill` / `view_skill`, `tengu skill evolve` with metric gate + approval, the skill scanner) but nothing triggers them after a task. Closing that loop is one of the highest-leverage open items.

PI/Cowork takes a different path: rather than autonomous evolution, it leans on a curated **plugin marketplace** so users adopt vetted skills.

---

## Tools

### Tengu
- `PluginToolExecutor` over a plugin registry; tools are namespaced and scoped. One catalog row per tool (`adapters/outbound/tools/mod.rs::catalog()`): 50 tools in the default build (`tengu tool list`: 14 default + 36 opt-in), + `agentic_memory` with `postgres_memory`.
- `compute_base_tools` returns workspace + memory + http + crypto + `skill_resource` + `view_skill` plus the opt-ins named in `domain/tools.rs::WORKSPACE_TOOLS` (37 names: memory / skill lifecycle, Solana read + write, Hyperliquid, xmarket risk / paper, xlab incl. `strategy_ranking`, `source_evidence`, SOE `soe_view` / `soe_propose` / `soe_challenge`); `[agents.<name>].tools` narrows it on every surface (`bootstrap::tools::agent_base_tools`).
- Every catalog tool works under `openrouter`, `local` and `claude_code` (the latter through `tengu mcp-bridge`): schema lint, bridge conformance, live engine matrix — § Trading runtime axes.
- `compress_and_store` is implicitly appended to every subagent — never list it in `[agents.<name>].tools`.
- Real MCP bridge via `outbound/mcp_client/client.rs::tools/list`; the planner registry lists core tool defs plus enumerated MCP server tools (`<server>__<tool>`, once per `RagPlanner`). Item 6.6 closed 2026-09-12.
- Every network path goes through `egress.rs`: Tor by default (`socks5h://127.0.0.1:9050`, fail-closed), `allow_hosts` / `deny_hosts` / `https_only` re-checked on each redirect hop, JSONL audit; children inherit the resolved policy via `TENGU_EGRESS`.

### Hermes
- 40+ tools, **6 terminal backends** (local, Docker, SSH, Daytona, Singularity, Modal). Daytona and Modal give you serverless persistence — agent environment hibernates between sessions.
- Built-in **cron scheduler** with delivery to any platform.
- Voice memo transcription as a first-class input.
- Subagents spawn isolated parallel workstreams.
- Python RPC tool calls (Hermes lets the agent write a script that calls tools, collapsing multi-step pipelines into a single context-cheap turn).

### PI / Cowork
- Built-ins: Read, Write, Edit, Bash, Grep, Glob, NotebookEdit.
- MCP marketplace + connector ecosystem (Atlassian, Notion, Linear, Slack, GitHub, Asana, etc.).
- Chrome extension and computer-use for desktop control; tiered access (read / click / full).
- Visual artifacts as a first-class deliverable.

---

## Routing model

The biggest architectural difference. All three solve the same problem (which tool/skill/agent should run for this user message) very differently.

- **Tengu** — file-registry-first. `RagPlanner` (historical name) regenerates `TENGU_PLANNER_REGISTRY.md` from the `[agents.<name>]` blocks with a `description`, skills and core + MCP tools, prepends Open Brain recall blocks, and asks a "planner" LLM (using `skills/orchestrator/SKILL.md` as system prompt, no tools/memory/grounding) to emit plan JSON. No similarity score or gate — the LLM reads each description + `example_queries` and picks; C→B `compose` fallback when nothing fits; unknown agent names fail fast in `SubprocessRunner`.
- **Hermes** — single-agent loop with rich tool choice. The model decides which tool to call; subagents are spawned by the model itself when parallelism helps. No upstream router.
- **PI / Cowork** — two-tier: the orchestration model picks slash-commands / skills via description-match; the `Agent` tool spawns subagents. `AskUserQuestion` is used to gate ambiguity rather than guess.

Each is suited to its product's context. Tengu's registry + planner design wins for a fixed roster of specialised agents; Hermes wins for a single rich agent that grows; PI wins for breadth without lock-in.

---

## Trading runtime axes (added 2026-10-02)

What Tengu gained 2026-09-24 → 10-09. Hermes / PI cells hold only what this snapshot already says (§ Tools, § TL;DR); "not covered here" = this doc has no evidence either way.

| Axis | Tengu | Hermes | PI / Cowork |
|---|---|---|---|
| Typed tool results | ✅ `Observation` envelope (≤ 32 scalar features, line 1 ≤ 200 chars with full ids, failed reads never 0) + TTL cache `<workspace>/.tengu/observations.db`; `[recorder]` day files — `docs/typed-observations-2026-09-24.md` | not covered here | not covered here |
| Decision model ("System One") | ✅ `[decision_loops.*]`: Jev picks the next action + argument slots from a typed menu, existing tools execute it, low confidence (`act_at`) escalates to the orchestrator, `dry_run` by default, audit `logs/decisions.jsonl` — `docs/decision-loop-plan-2026-09-24.md` | not covered here (this snapshot lists the agent loop + model-spawned subagents) | not covered here |
| Long-running runtime | ✅ `tengu run`: `[feeds.*]` on a UTC grid / local windows, single-runner lease, heartbeat, `tengu doctor --live`, graceful drain — `docs/runtime-2026-09-30.md` | built-in cron scheduler with delivery to any platform (§ Tools) | not covered here |
| Money safety | ✅ `[risk]` gate inside every exec tool (gate + fill + ledger write in one transaction, fail closed), kill-switch file, exit rules (`xm_exits`), paper ledger filled against live L2 books; only private agents hold exec tools — `docs/xmarket-risk-paper-2026-09-30.md` | not covered here | not covered here |
| Research on history | ✅ `market.db` warehouse + backfill (HL candles / funding, GeckoTerminal, HL S3 archive), strategy-spec DSL (six kinds — data, never code), pure backtest engine (costs, funding, `[risk]` caps, time-integrity checks), Jev replayed on history with a decision cache, holdout reads counted; `tengu evidence evaluate` scores rules · Jev · HOLD on the same candidates (verdict PROVEN / UNPROVEN / REJECTED); sealed strategy rankings (`[strategy_ranking]`: freshness → backtest → evaluate → rank → publish under a lease, no LLM; `tengu ranking`, tool `strategy_ranking`) — `docs/xlab-2026-10-01.md`, `docs/strategy-ranking-automation-2026-10-08.md` | not covered here | not covered here |
| Research lineage + forward evidence | ✅ `lineage/` registry (one TOML file per family, variant, experiment, evidence record, episode, incident, capability, generation, ranking contract; `tengu lineage verify / trace / seal`, preregistration sealed in append-only `locks.toml`); generation binding (`[generation]`: a pinned section edited ⇒ the config load fails, `pin_drift`); `tengu evidence snapshot / verify / grade / regrade / coverage` over read-only vaults `<TENGU_HOME>/state/evidence/<id>/` — `docs/lineage-2026-10-06.md` | not covered here | not covered here |
| Source evidence | ✅ `[sources]` registry (SEC EDGAR, EU TED): operator-run fetches into an append-only `sources.db`, as-of evidence packets (`captured` / `knowable`), retention purges, a runtime kill switch; agents read only (`source_evidence`); `tengu soe`: offline economics + hard gates (no LLM), and a weekly cycle where Architect / Critic stage agents write drafts through `soe_propose` / `soe_challenge` while the pure domain decides every figure and rank (frozen run dirs, replay with counted holdout reads, grades; no contact, spend or publish) — `docs/source-evidence-2026-10-08.md`, `docs/soe-2026-10-08.md` | not covered here | not covered here |
| Engine parity | ✅ every catalog tool on `openrouter`, `local` and `claude_code` (through `tengu mcp-bridge`, run as the sandbox agent): schema lint, a bridge conformance case per tool, live engine matrix (19 tool sets; `local` legs on the operator's PC) — `docs/mcp-bridge.md`, `docs/engine-backends.md` | 200+ providers, 6 terminal backends (§ TL;DR, § Tools); per-tool parity checks not covered here | one model vendor (Sonnet / Opus / Haiku); not applicable |
| Agent-to-agent (A2A) interop | ✅ A2A 1.0.1 both ways (2026-10-10): the opt-in `a2a` tool calls `[a2a.remotes.<name>]` harnesses (JSON-RPC 1.0 / 0.3, HTTP+JSON 1.0; host-pinned, through egress), `tengu a2a serve` serves the planner and chosen agents (JSON-RPC 1.0 + 0.3, SSE, bearer token); interop checked against the official `a2a-sdk` — `docs/a2a-2026-10-10.md` | not covered here (check current docs) | not covered here (check current docs) |

Net: these axes are Tengu's xmarket / xlab work. The snapshot holds no evidence that Hermes or PI ship equivalents — which is not evidence that they lack them; check their current docs before deciding "build vs borrow".

---

## What landed today (2026-04-26)

Historical — `src/adapters/rag/` was deleted 2026-05-14; per-example vectors are now `example_queries` lines in the Markdown registry.

| # | Item | Files | Purpose |
|---|---|---|---|
| 1 | Per-example vectors | `src/adapters/rag/indexer.rs`, `src/adapters/rag/query.rs` | Each `agents/*.toml::example_queries` entry indexed as its own vector. Embed bare query text (tight cosine), store snippet = full description (planner sees full context after dedup). Over-fetch margin in `query::search_registry` bumped 4× → 6× for the wider per-agent vector count. Expected score lift on BTC query: 0.355 → 0.5+. |
| 2 | TUI debug panel | `src/adapters/inbound/tui/mod.rs` | Renders `OrchestratorEvent::RagQueried` as a single compact System bubble: `rag-{phase} "{query}" → researcher(0.55) tool/http_request(0.32) ...`. Top-3 of top-10 hits. Off by default; opt in via `TENGU_TUI_RAG_DEBUG=1`. |
| 3 | Durable user-message persistence | `src/application/orchestrator/planner.rs`, `src/adapters/rag/{mod.rs,query.rs}` | `RagPlanner` mints `session_id` (`TENGU_SESSION_ID` override or fresh UUID). `plan()` writes the user message to `agentic_memory user events` with `MemoryKind::Message` via `RagStore::store_memory`. Fail-soft on errors. New `RagStore::search_messages` is forward-compat for the next commit (prompt-side read-back). |

Carryover from earlier this session (already committed):

- `c3fe7fd2be0f91fe7f4ffb2b41088622c536d2ee` — Phase 6.1 full: `OrchestratorEvent::RagQueried` variant + bus plumbing.
- `e248d82ead03d7e99eae098137f09405aa61cd6b` — registry recall: example_queries per agent + threshold realism.
- `7d1fa552b2d9e8bb52590d3aaf4ffaa79674b7c4` — auto-reindex `tengu_registry` (Qdrant) on first rag-mode chat turn. Superseded by the file-backed `TENGU_PLANNER_REGISTRY.md` registry.

---

## What's still open — prioritised

The smaller-effort polish items are at the top; structural work below.

### Polish

1. **6.4 read-back hydration** — **Done**: `[memory] cross_session_msg_top_k` drives the planner cross-session lane over Postgres `agentic_memory`. Original note: The durable writes shipped today; nothing reads them yet. One-line wiring of `RagStore::search_messages` into the planner prompt (with a config knob `memory.cross_session_msg_top_k`, default 0 = off) lights up cross-session semantic recall. Per-example vectors and the persistence design make this a small commit.
2. **6.2 content-hash dedup** — **Moot**: the registry is a rendered Markdown file, nothing is embedded. Original note: Auto-reindex now hits the embedding API ~29 times per chat startup with per-example vectors, up from ~6. Cheap absolute cost, but redundant when nothing changed. Hash description text in payload extras; skip re-embed when unchanged. Caveat noted in earlier sessions: registry uses UUID-on-write IDs, so dedup needs an upfront scroll OR a switch to deterministic IDs.
3. **6.3 filter-based TTL purge** — **Moot**: `ttl_days` / `cleanup.rs` left with the Qdrant path (2026-09-12). Original note: Default `ttl_days = 0` (never purge). When set > 0, `cleanup.rs::ttl_cleanup` logs a TODO; needs filter-based delete via direct legacy vector DB client.

### Architectural completeness

4. **6.6 real MCP tool indexing** — **Done 2026-09-12**: registry TOOLS section = core tool defs + MCP server tools. Original note: Replace `placeholder_tools()` (6 hardcoded) with `compute_base_tools()` for real compiled-in tools plus enumerated MCP server tools via `outbound/mcp_client/client.rs::tools/list`. Touches the registry CLI subcommand.
5. **6.7 C→B unknown-agent fallback (B half)** — **Done**: `Step.compose` (`plan.rs`, `plan_schema.json`, SKILL.md "C → B fallback"). Original note: REDESIGN.md §11 (file removed 2026-10-07; in git history). When all Open Brain / Karpathy LLM Wiki hits score below the threshold, today the SKILL.md tells the planner to ask the user. The B half — compose generic agent on user confirmation, run for one turn — isn't implemented. Multi-turn UX, deserves its own session.
6. **session_id sharing with `SubprocessRunner`** — **Done 2026-05-09** (Fix B): `build_orchestrator` resolves one id for `RagPlanner` + `SubprocessRunner`. Original note: The open question from the original handoff is still open. Today the planner and child subprocess each mint their own UUID. Pass via IPC env or stdin payload to unify the conversation across the entire orchestration.

### Cleanup (do last)

7. **7.1 delete legacy** — **Done**: `roster.rs` and the `static` engine are gone; `engine = "rag"` is the only value. Original note: Drop `src/application/orchestrator/roster.rs`, the `engine = "static"` branch in `OrchestratorAgentPlanner` and `build_orchestrator`, the `ChatWorker` static-mode branch. Make `engine = "rag"` the only path. Requires confidence legacy planner mode is bulletproof for every sandbox you care about — needs full manual checklist run on each sandbox + Telegram + cancel/replan flows.

---

## Observability — context/token telemetry

Added 2026-04-28 to Tengu; included here so the comparison stays current.

### Tengu

First-class. `src/domain/metrics.rs` records one `MetricsRecord` per LLM call (planner + each subagent inner-loop turn, the wiki compiler, each Jev decision) and per embedding call. Three surfaces: `RUST_LOG=tengu=info` baseline (always-on), in-process broadcast bus (`OnceLock<broadcast::Sender>`), and `OrchestratorEvent::MetricsRecorded` re-broadcast on the event bus. Per-call telemetry includes prompt/completion/total tokens, prompt chars + bytes, response chars, latency, and (for the planner) per-context-layer breakdown (`system` / `roster` / `cross_session` / `history` / `user_message`, plus `session_recall` on `plan`, `recall` / `failure` on `replan`). Subagent records cross the IPC boundary in `AgentIpcOutput.metrics`. TUI bubble opt-in via `TENGU_TUI_METRICS=1`.

Since 2026-10-09, runtime execution is observable too:

| Surface | What |
|---|---|
| Execution trace | every `tengu run` / `tengu decide` / `tengu webhooks` process (and, with `[orchestrator]`, each `tengu chat` / `tengu telegram` process and `tengu eval` row) writes one JSONL recording `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl` (`domain/trace.rs`): `event_id` = `<run_id>:<seq>`, session / correlation / parent ids, workflow `node_id`, status, payload redacted + bounded; `tengu trace runs` / `show [--follow]`. A webhook request is a `trigger.webhook` root over its loop or plan events; planner / subagent steps are `plan.*` / `step.*` with each step's `run-agent` tool calls under it |
| Tengu Studio | `tengu studio --sandbox <s>` (default build): a loopback browser page over the validated workflow graph, live + replayed runs, the `tengu doctor --live` health; Play / Stop only where `[studio] control` allows it (never `[generation]`-bound or hardened); the browser draws, Rust decides — `docs/studio-2026-10-08.md` |

### Hermes

`UsageRecord` per LLM call (input/output/total tokens, latency, model). Persisted to SQLite alongside the session log. Strong on persistence, weak on per-context-layer attribution — Hermes doesn't decompose the prompt into components since its assembly pipeline is more linear than Tengu's roster + recall + history layering.

### PI / Cowork

Token counts surface in the conversation transcript (Cowork's UI shows them inline) but there is no public per-context-layer breakdown or programmatic API for cost analysis. The closed-source equivalent of Tengu's `MetricsRecord` is reportedly internal; users see a final "session cost" rather than a turn-by-turn ledger.

### Net comparison

Tengu now has the most granular *attribution* (per-context-layer planner breakdown is unique to it). Hermes has the strongest *durability* (SQLite). PI has the smoothest *UX* (inline cost in the chat pane). The follow-up to close the durability gap would be a JSONL writer subscribing to the metrics bus (not built; `docs/SESSION_HANDOFF.md` no longer has the 2026-04-28 "Open follow-ups" list); the execution trace (2026-10-09) is durable, but it records runtime events, not the `MetricsRecord` stream. Tengu Studio adds a visual run view; this snapshot holds no evidence of an equivalent in Hermes or PI.

---

## What can be improved (analytical)

Beyond the open items above, three architectural directions where Tengu lags behind one of the other two:

1. **Borrow Hermes's closed learning loop.** Tengu has `skill_distill`, `manage_skill` / `view_skill`, `tengu skill evolve` and the skill scanner — the building blocks of in-use refinement — but no orchestrator wiring that says "after this complex task, propose a skill update." Adding a post-turn hook that runs `skill_distill` against successful long sessions (gated on user confirmation, like PI's `AskUserQuestion`) would close the same loop without going fully autonomous.
2. **Borrow Hermes's session search + summarisation.** With durable msg persistence shipped today, the data is now there. A follow-up that runs LLM summarisation on session-old messages and writes back into `agentic_memory step outputs` would make Tengu's recall comparable to Hermes's FTS5 + summary path, while keeping the vector-first design.
3. **Borrow PI's MCP marketplace shape.** Tengu has the MCP bridge but no UX for discovering or installing servers. A `tengu mcp install <name>` subcommand backed by a tiny registry (even just a JSON manifest) would let users adopt connectors without editing TOML.

These are all additive — they don't conflict with the doctrine ("LLM = heart, Open Brain / Karpathy LLM Wiki = brain, tools = hands"). They just fill in the agent-layer features Tengu's `tengu-analysis.html` flagged as missing.

---

*Last updated 2026-04-26 with a 2026-04-28 follow-up adding the Observability section above, a 2026-09-18 fact pass (banner at top), a 2026-10-02 pass (§ Trading runtime axes; Tengu rows of the TL;DR and § Tools), a 2026-10-08 code check (lineage + evidence, strategy ranking, source evidence, counts, skill rows) and a 2026-10-09 code check (SOE weekly cycle, execution trace, Tengu Studio, `WORKSPACE_TOOLS` count). The "What landed today" section is the 2026-04-26 snapshot and is intentionally not amended in place — see `SESSION_HANDOFF.md` for everything since. Companion files: `comparison-2026-04-26.svg` for the diagram, `SESSION_HANDOFF.md` for the day-to-day handoff doc, `tengu-analysis.html` for the deep historical analysis.*
