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
| **Surfaces** | TUI (cursive), Telegram, webhooks listener | CLI, Telegram, Discord, Slack, WhatsApp, Signal | Desktop app, browser ext |
| **Models** | OpenRouter (any) + Claude Code engine | 200+ providers, multi-backend terminal | Sonnet / Opus / Haiku |
| **Persistence** | Open Brain Postgres + Karpathy LLM Wiki Markdown + file registry | SQLite + FTS5 + Honcho dialectic model | CLAUDE.md + plain-text memory files |
| **Network** | Tor by default (`[egress] network = "tor"`: Arti + lyrebird-rs proxy, host allow/deny ceiling, JSONL audit — code-enforced, fail-closed; `"open"` per sandbox) | not a first-class feature | platform-managed |
| **Strength** | Doctrine clarity (LLM=heart, Open Brain / Karpathy LLM Wiki=brain, tools=hands) | Breadth: platforms, scheduler, self-improving skills | UX: artifacts, computer-use, MCP marketplace |
| **Weakness** | Agent-layer features partial / vibecoded gaps | Heavier deployment surface, Python perf | No vector memory; no autonomous skill evolution |

---

## Memory model

### Tengu

Three stores (2026-09-18 state — the 2026-04-26 Qdrant collections are gone):

- `TENGU_PLANNER_REGISTRY.md` — Markdown roster rendered from the `[agents.<name>]` blocks that carry a `description` (+ `example_queries`), skills, core + MCP tools. Regenerated every planner turn (`shared_files::render_registry`); no embeddings, no reindex step.
- Postgres `agentic_memory` (pgvector + FTS, feature `postgres_memory`) — user messages (`RagPlanner::persist_user_message` on every `plan()`), step summaries (`compress_and_store` + the `run-agent` backstop), `promote` → `compile_wiki` for the Karpathy LLM Wiki. Read back by three planner lanes: cross-session (`[memory] cross_session_msg_top_k`), within-session (`within_session_output_top_k`), cross-plan on `replan()` (`cross_plan_top_k`).
- Disk bincode vector store (`memory/vector/disk.rs`) — in-process `memory_ingest` / `memory_search` / `persistent_store`; the only built-in `VectorStore`.

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
| **Discovery** | 3-tier scanner (managed `~/.tengu/skills`, workspace dotdir, workspace root) | Skills Hub + agentskills.io standard | Plugin marketplace + per-session skill list |
| **Trigger** | Planner LLM reads `TENGU_PLANNER_REGISTRY.md` (descriptions + `example_queries`) and picks | Description-match + slash command | Description-match (slash commands) |
| **Creation** | `skill_distill` tool; `learning-agent` with `view_skill` / `manage_skill`; `tengu skill install` | Autonomous skill creation after complex tasks | `skill-creator` skill, manual / LLM-assisted |
| **Self-improvement** | `tengu skill evolve` (metric-gated, improver agent, approval gate) — no post-turn auto-trigger | live in-use refinement | none |
| **Standard** | own format (SKILL.md + frontmatter) | agentskills.io | SKILL.md + frontmatter (close to Hermes) |

The big gap for Tengu here: **Hermes has a closed learning loop** — it creates skills from experience, refines them as it uses them, and persists the refinements. Tengu has the *building blocks* (`skill_distill`, `learning-agent` + `manage_skill`, `tengu skill evolve` with metric gate + approval, three-tier scanner) but nothing triggers them after a task. Closing that loop is one of the highest-leverage open items.

PI/Cowork takes a different path: rather than autonomous evolution, it leans on a curated **plugin marketplace** so users adopt vetted skills.

---

## Tools

### Tengu
- `PluginToolExecutor` over a plugin registry; tools are namespaced and scoped.
- `compute_base_tools` returns workspace + memory + http + crypto + `skill_resource` + `view_skill` plus the six-element `WORKSPACE_TOOLS` opt-ins (`agentic_memory`, `shared_cache`, `persistent_store`, `skill_distill`, `apply_improver_proposal`, `manage_skill`); `[agents.<name>].tools` narrows it per subagent.
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

## What landed today (2026-04-26)

Historical — `src/adapters/rag/` was deleted 2026-05-14; per-example vectors are now `example_queries` lines in the Markdown registry.

| # | Item | Files | Purpose |
|---|---|---|---|
| 1 | Per-example vectors | `src/adapters/rag/indexer.rs`, `src/adapters/rag/query.rs` | Each `agents/*.toml::example_queries` entry indexed as its own vector. Embed bare query text (tight cosine), store snippet = full description (planner sees full context after dedup). Over-fetch margin in `query::search_registry` bumped 4× → 6× for the wider per-agent vector count. Expected score lift on BTC query: 0.355 → 0.5+. |
| 2 | TUI debug panel | `src/adapters/inbound/tui/mod.rs` | Renders `OrchestratorEvent::RagQueried` as a single compact System bubble: `rag-{phase} "{query}" → researcher(0.55) tool/http_request(0.32) ...`. Top-3 of top-10 hits. Off by default; opt in via `TENGU_TUI_RAG_DEBUG=1`. |
| 3 | Durable user-message persistence | `src/application/orchestrator/planner.rs`, `src/adapters/rag/{mod.rs,query.rs}` | `RagPlanner` mints `session_id` (`TENGU_SESSION_ID` override or fresh UUID). `plan()` writes the user message to `agentic_memory user events` with `MemoryKind::Message` via `RagStore::store_memory`. Fail-soft on errors. New `RagStore::search_messages` is forward-compat for the next commit (prompt-side read-back). |

Carryover from earlier this session (already committed):

- `c3fe7fd` — Phase 6.1 full: `OrchestratorEvent::RagQueried` variant + bus plumbing.
- `e248d82` — registry recall: example_queries per agent + threshold realism.
- `7d1fa55` — auto-reindex `tengu_registry` (Qdrant) on first rag-mode chat turn. Superseded by the file-backed `TENGU_PLANNER_REGISTRY.md` registry.

---

## What's still open — prioritised

The smaller-effort polish items are at the top; structural work below.

### Polish

1. **6.4 read-back hydration** — **Done**: `[memory] cross_session_msg_top_k` drives the planner cross-session lane over Postgres `agentic_memory`. Original note: The durable writes shipped today; nothing reads them yet. One-line wiring of `RagStore::search_messages` into the planner prompt (with a config knob `memory.cross_session_msg_top_k`, default 0 = off) lights up cross-session semantic recall. Per-example vectors and the persistence design make this a small commit.
2. **6.2 content-hash dedup** — **Moot**: the registry is a rendered Markdown file, nothing is embedded. Original note: Auto-reindex now hits the embedding API ~29 times per chat startup with per-example vectors, up from ~6. Cheap absolute cost, but redundant when nothing changed. Hash description text in payload extras; skip re-embed when unchanged. Caveat noted in earlier sessions: registry uses UUID-on-write IDs, so dedup needs an upfront scroll OR a switch to deterministic IDs.
3. **6.3 filter-based TTL purge** — **Moot**: `ttl_days` / `cleanup.rs` left with the Qdrant path (2026-09-12). Original note: Default `ttl_days = 0` (never purge). When set > 0, `cleanup.rs::ttl_cleanup` logs a TODO; needs filter-based delete via direct legacy vector DB client.

### Architectural completeness

4. **6.6 real MCP tool indexing** — **Done 2026-09-12**: registry TOOLS section = core tool defs + MCP server tools. Original note: Replace `placeholder_tools()` (6 hardcoded) with `compute_base_tools()` for real compiled-in tools plus enumerated MCP server tools via `outbound/mcp_client/client.rs::tools/list`. Touches the registry CLI subcommand.
5. **6.7 C→B unknown-agent fallback (B half)** — **Done**: `Step.compose` (`plan.rs`, `plan_schema.json`, SKILL.md "C → B fallback"). Original note: REDESIGN §11. When all Open Brain / Karpathy LLM Wiki hits score below the threshold, today the SKILL.md tells the planner to ask the user. The B half — compose generic agent on user confirmation, run for one turn — isn't implemented. Multi-turn UX, deserves its own session.
6. **session_id sharing with `SubprocessRunner`** — **Done 2026-05-09** (Fix B): `build_orchestrator` resolves one id for `RagPlanner` + `SubprocessRunner`. Original note: The open question from the original handoff is still open. Today the planner and child subprocess each mint their own UUID. Pass via IPC env or stdin payload to unify the conversation across the entire orchestration.

### Cleanup (do last)

7. **7.1 delete legacy** — **Done**: `roster.rs` and the `static` engine are gone; `engine = "rag"` is the only value. Original note: Drop `src/application/orchestrator/roster.rs`, the `engine = "static"` branch in `OrchestratorAgentPlanner` and `build_orchestrator`, the `ChatWorker` static-mode branch. Make `engine = "rag"` the only path. Requires confidence legacy planner mode is bulletproof for every sandbox you care about — needs full manual checklist run on each sandbox + Telegram + cancel/replan flows.

---

## Observability — context/token telemetry

Added 2026-04-28 to Tengu; included here so the comparison stays current.

### Tengu

First-class. `src/domain/metrics.rs` records one `MetricsRecord` per LLM call (planner + each subagent inner-loop turn) and per embedding call. Three surfaces: `RUST_LOG=tengu=info` baseline (always-on), in-process broadcast bus (`OnceLock<broadcast::Sender>`), and `OrchestratorEvent::MetricsRecorded` re-broadcast on the event bus. Per-call telemetry includes prompt/completion/total tokens, prompt chars + bytes, response chars, latency, and (for the planner) per-context-layer breakdown (`system` / `roster` / `cross_session` / `history` / `recall` / `failure` / `user_message`). Subagent records cross the IPC boundary in `AgentIpcOutput.metrics`. TUI bubble opt-in via `TENGU_TUI_METRICS=1`.

### Hermes

`UsageRecord` per LLM call (input/output/total tokens, latency, model). Persisted to SQLite alongside the session log. Strong on persistence, weak on per-context-layer attribution — Hermes doesn't decompose the prompt into components since its assembly pipeline is more linear than Tengu's roster + recall + history layering.

### PI / Cowork

Token counts surface in the conversation transcript (Cowork's UI shows them inline) but there is no public per-context-layer breakdown or programmatic API for cost analysis. The closed-source equivalent of Tengu's `MetricsRecord` is reportedly internal; users see a final "session cost" rather than a turn-by-turn ledger.

### Net comparison

Tengu now has the most granular *attribution* (per-context-layer planner breakdown is unique to it). Hermes has the strongest *durability* (SQLite). PI has the smoothest *UX* (inline cost in the chat pane). The follow-up to close the durability gap would be a JSONL writer subscribing to the metrics bus — see `docs/SESSION_HANDOFF.md` "Open follow-ups" 2026-04-28.

---

## What can be improved (analytical)

Beyond the open items above, three architectural directions where Tengu lags behind one of the other two:

1. **Borrow Hermes's closed learning loop.** Tengu has `skill_distill`, `learning-agent` + `manage_skill`, `tengu skill evolve` and the three-tier scanner — the building blocks of in-use refinement — but no orchestrator wiring that says "after this complex task, propose a skill update." Adding a post-turn hook that runs `skill_distill` against successful long sessions (gated on user confirmation, like PI's `AskUserQuestion`) would close the same loop without going fully autonomous.
2. **Borrow Hermes's session search + summarisation.** With durable msg persistence shipped today, the data is now there. A follow-up that runs LLM summarisation on session-old messages and writes back into `agentic_memory step outputs` would make Tengu's recall comparable to Hermes's FTS5 + summary path, while keeping the vector-first design.
3. **Borrow PI's MCP marketplace shape.** Tengu has the MCP bridge but no UX for discovering or installing servers. A `tengu mcp install <name>` subcommand backed by a tiny registry (even just a JSON manifest) would let users adopt connectors without editing TOML.

These are all additive — they don't conflict with the doctrine ("LLM = heart, Open Brain / Karpathy LLM Wiki = brain, tools = hands"). They just fill in the agent-layer features Tengu's `tengu-analysis.html` flagged as missing.

---

*Last updated 2026-04-26 with a 2026-04-28 follow-up adding the Observability section above and a 2026-09-18 fact pass (banner at top). The "What landed today" section is the 2026-04-26 snapshot and is intentionally not amended in place — see `SESSION_HANDOFF.md` for everything since. Companion files: `comparison-2026-04-26.svg` for the diagram, `SESSION_HANDOFF.md` for the day-to-day handoff doc, `tengu-analysis.html` for the deep historical analysis.*
