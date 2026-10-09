# Tengu-Cluster — Architecture Walkthrough (2026-04-27; layout 2026-09-23; synced 2026-10-08)

> Companions: `docs/architecture-2026-04-27.svg` (the picture), `docs/architecture-2026-04-27.html` (the explorer: walk a turn, the other flows, subsystem cards, searchable file map). Five minutes: the TL;DR, §1 (the chat turn), §1b (the `tengu run`, `tengu backtest` and `tengu ranking` flows). An hour: §2's file tables. Any single file: `docs/code-map.md`. One animated page per feature: `docs/tutorial/`.

---

## TL;DR — the entire system in five sentences

A user message arrives at a channel (TUI / Telegram / webhook). The harness builds an `Orchestrator`, which holds `RagPlanner` (legacy name for the LLM planner) and a `SubprocessRunner` (which executes plan steps as child processes). The planner regenerates and loads root `TENGU_PLANNER_REGISTRY.md` (`[agents.*]` with a `description` · skills · tools) into the planner prompt, then asks an LLM to emit either a direct response or a plan referencing that roster. Each plan step spawns `tengu run-agent` as a subprocess, which re-loads the parent's config (`sandboxes/<name>/config.toml`), takes its `[agents.<name>]` block, receives the accepted plan via `AgentIpcInput.plan_state`, builds a tool executor, and runs an LLM-with-tools loop until the model calls `compress_and_store` (the implicit "I'm done" tool). Every network path — tools, LLM APIs, the Claude Code CLI, Telegram — goes through the `[egress]` policy (`egress.rs`), which is Tor by default. Step output flows back to the executor; on failure the executor calls `RagPlanner::replan`, which uses Open Brain-style Postgres `agentic_memory` cross-plan recall when `postgres_memory` is enabled.

That is the chat turn. Since 2026-09-24 the same binary also runs work with no chat turn at all:

| Path | What runs | Read |
|---|---|---|
| `tengu run --sandbox <s>` | the sandbox's long-running process: `[feeds.*]` call tools on a clock, `[decision_loops.*]` take events, webhook routes; one runner per sandbox (lease), heartbeat | §1b A · §2 runtime |
| `[decision_loops.<n>]` | Jev (a System One model) picks an action + argument slots; existing tools execute it; replayable on a `SimClock` | §2 decision loops |
| Typed tools | return an `Observation` row; rows cache in `<workspace>/.tengu/observations.db`, `[recorder]` keeps day-file history | §2 observations |
| xmarket exec tools | `[risk]` gate + paper fill + `ledger.db` write in one transaction, inside the tool | §2 xmarket |
| `tengu backtest` + xlab tools | public history in `market.db` (+ SEC filing events) → pure engine → run dir; Jev replayed with a decision cache | §1b B · §2 xlab |
| `tengu lineage` + `tengu evidence` | `lineage/` registry (one TOML per record); W1 frozen, a sandbox's `[generation]` checked at every `Config::load`; read-only evidence vault, forward grade / regrade, rules · Jev · HOLD evaluation | §2.14 |
| `tengu ranking run` + `strategy_ranking` | a sealed contract's backtests → one ranking per date, weakest first per cohort; a `[feeds.*]` of `tengu run` | §1b C · §2.15 |
| `tengu sources` + `source_evidence` | SEC EDGAR / EU TED facts → append-only `sources.db`; the as-of evidence packet (no lookahead); agents never fetch | §2.16 |
| `tengu soe` | Software Opportunity Engine, offline: gates, economics, ranking over a signed private profile | §2.17 |

**Code layout (2026-09-23):** hexagonal — `src/domain/` (data) ← `src/ports/` (traits) ← `src/application/` (use cases) ← `src/adapters/{inbound,outbound}/`, wired by `src/bootstrap/`; enforced by `tests/layering_lint.rs`. Every file, extension recipe and dependency: `docs/code-map.md` / `docs/code-map.html`.

---

## §1 — The seven steps from prompt to reply

Trace through them in order. Every step has a file you can open.

### 1. User input → Channel

The user types into the TUI (`src/adapters/inbound/tui/mod.rs`) or sends a Telegram message (`src/adapters/inbound/telegram.rs`; fails closed without an allow-list); a webhook POST (`src/adapters/inbound/webhooks.rs`, `--features webhooks`) runs a one-shot orchestrator turn with its own `session_id`. All get their wiring from `src/bootstrap/` (tool executor, memory, orchestrator); TUI and Telegram share `src/adapters/inbound/channel.rs` helpers.

### 2. Bootstrap → Orchestrator

`src/bootstrap/orchestrator.rs::build_orchestrator(config, chat_factory, memory, session_id)` is the function that wires everything together. It:
- Constructs `ChatOrchestratorPortImpl` (`application/orchestrator/wiring.rs`) over the channel's `ChatServiceFactory` (`RuntimeChatServiceFactory`) — the LLM-call abstraction the planner uses.
- Mints an `EventBus` (so the TUI can subscribe to plan progress events), installs the global metrics sink and bridges it onto the bus.
- Builds `RagPlanner` (legacy name; planner side) with its ports injected — `ToolDirectory` (`CatalogDirectory`: tool catalog + MCP `tools/list`), `RecallStore` + `Embedding` (only with `postgres_memory`) — and `SubprocessRunner` (worker side). Both receive `config.agents` (the planner renders the registry, the runner fails fast on unknown agents and reads per-step `limits.max_tool_rounds` / `limits.step_timeout_secs`) and the caller's `session_id` (`resolve_session_id`: `TENGU_SESSION_ID`, else a fresh UUID — once per process for chat / Telegram, per row for `tengu eval`; `tengu webhooks` mints `webhook-<name>-<uuid>` per request).
- Returns an `Orchestrator` that wraps them.

The default-agent dispatch (no orchestrator at all) is the fallback when `[orchestrator]` is missing from the sandbox config. `engine = "rag"` is the only orchestrator engine (Phase 7.1) — any other value fails config validation; `build_orchestrator` keeps a defensive warn + `None`. The name is historical: file-registry planner + Open Brain memory.

### 3. Planner — `RagPlanner::plan`

`src/application/orchestrator/planner.rs`. This is where the doctrine lives ("LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands"). For each user message:

1. **Embed once** — only with a `RecallStore` (`postgres_memory`) and `OPENROUTER_API_KEY`; the vector is reused by every recall lane below.
2. **Load the planner registry** — regenerate root `TENGU_PLANNER_REGISTRY.md` (`shared_files::ensure_planner_registry`) from the `[agents.*]` blocks that carry a `description` (`routable_agents`, with `example_queries`), skills, and core + MCP tool definitions, then inject it into the planner prompt.
3. **Emit `RagQueried` event** — registry entries go to the event bus + a tracing line. The TUI debug panel renders these when `TENGU_TUI_RAG_DEBUG=1`.
4. **Open Brain recall (opt-in)** — if `memory.cross_session_msg_top_k > 0`, retrieve prior user questions from Postgres `agentic_memory` and inject them as a "Cross-session message recall" block. **This happens BEFORE persisting the current message** so the just-written message can never appear in its own recall block.
5. **Persist the user message** — `persist_user_message` writes keyed by `session_id` to Postgres `agentic_memory` with `postgres_memory`.
6. **Push to in-memory buffer** — Phase 6.4 lite (`update_and_format_history`): the "Recent user messages this session" block.
7. **Within-session output recall (opt-in)** — if `memory.within_session_output_top_k > 0`, prior step outputs of this `session_id` (`session_output_recall_block`).
8. **LLM call with `skills/orchestrator/SKILL.md` as system prompt** — `run_orchestrator_turn_with_system_metered` → `RuntimeChatServiceFactory::run_turn_inner` (`bootstrap/orchestrator.rs`) passes no tools, no tool executor, no memory manager and suppresses the grounding nudge, so the LLM has exactly one job: emit plan JSON in the shape `plan_schema.json` describes. `emit_planner_metrics` records the per-layer breakdown.
9. **Parse verdict** — `parse_verdict` (4-path cascade: raw JSON, fenced, JSON inside prose, prose → `Direct`) gives `PlannerVerdict::Direct { response }` (return it to user) or `PlannerVerdict::Plan { plan }` (hand to executor). No JSON-schema validator runs in Rust; an empty output ends the turn with `System error: orchestrator initial call failed: …` (`replan.rs::drive`).

### 4. Executor — DagExecutor

`src/application/orchestrator/executor.rs`. Standard topological-sort-then-parallel pattern:
- `Plan::ready_steps` (`domain/plan.rs`) returns every step whose `depends_on` is satisfied.
- Each ready step is `tokio::spawn`'d on the worker (`SubprocessRunner`).
- Retries follow `RetryPolicy::new(max_attempts_per_step)` from `retry.rs` (default 3 attempts: at once, then after 1 s, 3 s; 9 s before any later one).
- On exhaustion, `replan.rs::drive` calls `RagPlanner::replan` with the failed step + error context. With `postgres_memory`, replan also pulls `cross_plan_top_k` hits from Postgres `agentic_memory`.
- Cancel or an exhausted step aborts every step still in flight (`abort_in_flight`; the dropped `run-agent` child is `kill_on_drop`). A step that never becomes ready (unknown dependency, cycle) is a replan, not an empty reply. Parallel leaves with no join step are joined in plan order, one `### <id> (<agent>)` section each (`Plan::leaves`).
- Every accepted plan/replan is rendered once: stored per session (`shared_files::set_active_plan`) so `SubprocessRunner` can pass it as `AgentIpcInput.plan_state`, and written to root `TENGU_PLAN.md` as a human-readable debug artifact (fallback for old parents only).

### 5. Subprocess — `SubprocessRunner::run_step`

`src/adapters/outbound/subprocess_runner.rs`. For each step:
- Fails fast when `step.agent` (or `compose.base_agent`) names no routable `[agents.<name>]` block (one with a `description`) in the parent config — no spawn; the retry policy still runs its attempts (1 s, 3 s waits) before the replan.
- Builds `AgentIpcInput` JSON from the step (agent name, goal, session_id, step_id, `max_turns` = the agent's `limits.max_tool_rounds`, optional `compose` for C→B fallback, **`sandbox_config`** so the child sees the same scopes as the parent — Phase 7.2, **`plan_state`** — the rendered plan for this session, 2026-09-12). `model` / `tools` / `skills` travel empty — the child reads them from its own `[agents.<name>]`.
- Spawns `tengu run-agent` as a child process with `TENGU_AGENT_IPC=1` env guard and `TENGU_EGRESS` (the parent's *resolved* `[egress]` policy — the child applies it verbatim, 2026-09-16).
- Pipes the JSON over stdin; reads the result JSON from stdout; kill on drop. Wall-clock cap per step = `limits.step_timeout_secs` (default 600) via `run_with_timeout`. The child's stderr log goes through the parent's log (`forward_child_log`, target `tengu::run_agent`); a failed step's error carries its last 40 lines.
- Returns `AgentIpcOutput::Ok { output, summary }` or `Failed { error, output }`, both with `metrics` and `tools` — the step's tool activity (name + ok per call; a Claude Code engine's bridged calls arrive as `StreamEvent::ToolRan`), which `tests/engine_matrix.rs` asserts on.

### 6. Subagent tool loop (in the child)

`src/adapters/inbound/cli/run_agent.rs::run_agent_subprocess` is the entry point of the child process:
1. Reads stdin, parses `AgentIpcInput`; exports `TENGU_SESSION_ID`.
2. **Phase 7.2 config resolution — FIRST** — `load_sandbox_or(input.sandbox_config, default config)`: `sandboxes/<name>/config.toml` when the parent ran with `--sandbox`, else the default chain. Scopes/secrets/MCP servers therefore match the parent. Then `egress::install(&parent_config.egress)` — `TENGU_EGRESS` wins over the file, an invalid policy aborts the child.
3. **Agent resolution** — `parent_config.agents.get(name)` where `name` = `compose.base_agent` if composed, else `input.agent_name`. Unknown name → hard error listing the configured agents. Composed runs overwrite `skill_packages` / `tools` in memory only (`bootstrap::tools::compose_agent`; in a hardened sandbox only narrowing). Exports `TENGU_AGENT_NAME`. The step's workspace (`bootstrap::tools::workspace_or_temp`): the agent's, else a `tengu-step-*` temp dir removed after the step — the executor's and the engine's (a Claude Code CLI's cwd, its bridge).
4. **System prompt assembly** — base template + each skill body (via `load_skill_body_three_tier`) + `input.plan_state` (falls back to root `TENGU_PLAN.md`) + mandatory suffix. Per-tool scopes come from the folded `[default_scopes]` plus the child's own workspace in `fs_roots` (`grant_workspace_root`; a deny-all scope stays a deny).
5. **Tool executor** — `bootstrap::tools::build_subprocess_tool_executor(&AgentConfig, ..)` builds `PluginToolExecutor` from `(base tools ∩ agent.tools) ∪ {compress_and_store} ∪ shell skills (skill_packages) ∪ [[mcp_servers]] tools (∩ tools)` over the plugin registry (`agent_base_tools` + `agent_skill_registry`); `subagent_config` first merges the workspace-tool names in `tools` into `workspace_tools`. Its HTTP client comes from `egress::policy().tool_client` (proxy, redirects off); `http_request` gates hop 0 (egress + scope) inside `execute` and re-checks every redirect hop; `run_command` runs under the `[egress]` shell mode — see §4 K.
6. **Multi-turn loop** — `run_single_engine_turn` repeatedly until the model calls `compress_and_store` OR `max_turns` (= `limits.max_tool_rounds`) exhausted. Tool results enter as in chat for every engine (`tool_loop::tool_result_content`: `max_tool_result_chars`, local window fit; older rounds compacted to line 1). Out-of-band detection of `compress_and_store` to commit step result. `engine = "claude_code"` agents get `EngineContext.bridge_tools` so the CLI sees tengu tools over the MCP bridge; the step's engine (`engines::build_step_engine`) also names the workspace grant and a summary file in the bridge env, so `compress_and_store` through the bridge lands in that file and becomes the step summary — the engine then ends the CLI run after that round.

### 7. `compress_and_store` — durable step summary

`src/adapters/outbound/tools/skill_lifecycle/compress_and_store.rs` owns only the tool *definition*. The model's "I'm done" call. `run-agent` intercepts the tool call (a Claude Code step's bridge writes it to the step's `TENGU_BRIDGE_SUMMARY_FILE`, read back after the turn), captures the summary, and exits cleanly. With `postgres_memory`, `adapters/inbound/cli/run_agent.rs::try_persist_agentic_step_summary` writes the final summary to Postgres `agentic_memory` with embeddings when available and text-only fallback otherwise; without it the summary only travels back over IPC.

Output flows back to the executor (step 4); on success the next ready step starts.

### Across all steps — metrics emission (added 2026-04-28)

Every step that calls an LLM or embedding API emits a `MetricsRecord` via `application::metrics::record()`:

| Emitter | Kind | Where |
|---|---|---|
| Planner (step 3) | `Planner` — one per plan / replan, with the layer breakdown | `planner.rs::emit_planner_metrics` |
| Subagent (step 6) | `Subagent` — one per `run_single_engine_turn`, shipped in `AgentIpcOutput.metrics`, re-emitted by `SubprocessRunner::run_step` on the parent's sink (`metrics::forward`: no second log line) | `cli/run_agent.rs` |
| Embedder | `Embedding` — reads `usage.total_tokens` | `outbound/memory/embedder.rs::embed_batch_openrouter` |
| Wiki compiler | `WikiCompiler` | `tools/agentic_memory/` (`compile_wiki`) |
| Decision loop | `Decision` — one per successful decisions call (replay: cache hits too) | `application/decision_loop/mod.rs` |

A bridge task (spawned in `build_orchestrator`) subscribes to the global metrics sink and republishes each record as `OrchestratorEvent::MetricsRecorded` on the orchestrator bus, so subscribers (TUI, eval recorder) see one unified event stream.

---

## §1b — Other flows (2026-09-30 / 2026-10-01; ranking 2026-10-08)

No user message starts these. Same tools, scopes and egress as a chat turn; no planner.

### A. `tengu run` — feed → tool → store → loop → exec tool → gate → ledger

| # | Step | Code |
|---|---|---|
| 1 | Start: leases `runtime:<s>` (+ `state:<dir>` for an `[xmarket]` state dir) in `<state dir>/runtime.db`, all or none, TTL 30 s, renewed every 10 s; held ⇒ exit 1, lost ⇒ stop as failed | `adapters/inbound/run.rs::run_runtime` → `bootstrap/runtime.rs` (`LeasePlan`, `OwnerLeases`) · `outbound/runtime_store.rs` |
| 2 | Every `[decision_loops.*]` built once behind one `LoopDispatch`; `HealthBoard` starts the heartbeat | `bootstrap/decision.rs::build_decision_loop` · `application/runtime/{loops,health}.rs` |
| 3 | Each `[feeds.<n>]` fires on its schedule (`SystemClock`): UTC grid `every_secs`, local `windows`, DST-safe `at` ticks, jitter; one run in flight; missed slots never replayed | `bootstrap/runtime.rs::start_feeds` → `application/runtime/feeds.rs::run_feed` · `domain/schedule.rs::next_fire` |
| 4 | `kind = "tool"`: the feed agent's executor runs the tool (fan-out `each`), call id `feed:<name>:<slot ms>:<i>`; egress lines name the feed | `bootstrap/decision.rs::agent_tool_executor` (`SanitizedToolExecutor`) under `egress::AttributedExecutor` |
| 5 | Typed tools read through the cache and write rows (`mkt_ctx/1`, `mkt_instrument/1`, `hl_book/1`, …); `[recorder]` appends them to day files | `application/observe.rs::observe` · `outbound/observations.rs` (`<workspace>/.tengu/observations.db`) · `outbound/history_sqlite.rs` (`<state dir>/history/<YYYYMMDD>.db`) |
| 6 | `kind = "tick"`: an event to a loop; the loop reads `world` rows (never fetched), Jev picks, `act_at` + `requires` gate, the tool runs via `execute_typed` | `LoopDispatch::submit_tracked` → `application/decision_loop/mod.rs::handle_event` · `outbound/decisions.rs::JevClient` |
| 7 | Exec tool (`paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade`): private agent only, `client_order_id` (arg, else call id), store reads, latency on the `Clock`, then a live book | `tools/xm/exec_common.rs::run_exec` · `application/paper.rs::fill_with_latency` · `tools/hyperliquid/book.rs` (`HlBookSource`) |
| 8 | `[risk]` gate + fill + ledger write in one `BEGIN IMMEDIATE`; a denial is a verdict row (+ `<TENGU_HOME>/logs/risk.jsonl`), loop outcome `Refused` | `PaperLedger::place(decide(..))` · `domain/xm/risk.rs::evaluate` · `domain/xm/paper.rs` · `outbound/paper_store.rs` (`<state dir>/ledger.db`) |
| 9 | Health: `run-<s>.json` + `loop/1` / `feed/1` rows → `tengu doctor --sandbox <s> --live`; SIGINT / SIGTERM drain ≤ `[runtime] shutdown_grace_secs` | `application/runtime/health.rs` · `domain/runtime.rs::live_verdict` · `cli/doctor.rs` |

| Note | Detail |
|---|---|
| Clock work is a tool feed | exits, the weekend fade, the daily risk roll: `kind = "tool"` — no LLM, no Jev (`kind = "tick"` always reaches a Jev loop) |
| Today | `xmarket` + `xmarket-weekend` run tool feeds only (no `[decision_loops]` there yet — tracker `jev-xmarket-loops-toml`); `xlab-w2` runs tool feeds (`history_refresh`, `strategy_ranking_daily`, `strategy_ranking_weekend` — §1b C); loops live in `lping` (`lp_watch`, `hedge_watch`, exec chains `hedge_exec`, `lp_exec`), `jev-exec`, `xlab` / `xlab-w2` (`xl_gate`, replayed by `tengu backtest --gate`) |
| Operator doc | `docs/runtime-2026-09-30.md` (§ Feeds, § Health, § Weekend run) |

### B. `tengu backtest` — market.db → resolve / prepare → candidates → [Jev gate] → evaluate → run dir

| # | Step | Code |
|---|---|---|
| 0 | Fill the warehouse: HL candles + funding, GeckoTerminal pool OHLCV, HL S3 archive asset contexts, JSON datasets, SEC EDGAR filings of `hyperliquid:xyz:<TICKER>` names (`tengu history events`, `$SEC_USER_AGENT`) → `<state dir>/market.db` (`bars`, `funding`, `ctx`, `events`, `event_coverage`); resumes | `tengu history backfill` · `events` · `import-hl-archive` · `import-json` (`cli/history.rs`) → `outbound/backfill/` (`sec.rs` + `domain/sec.rs`) → `outbound/market_data.rs` |
| 1 | `resolve`: the spec (`[backtest.strategies.<n>]` or a JSON object) parsed + validated in one error, universe `@<name>`, instruments minus `exclude`, `spec_sha256` | `application/backtest/mod.rs::resolve` / `spec_of` · `domain/backtest/spec.rs` · `domain/canonical.rs` |
| 2 | `prepare`: a kind outside the bound `[generation]` refused (`capability_unavailable`); series over the data window (bars; funding / ctx when the costs need them), cut at `--data-through` (else the newest row read = `data_through_ms`), `[backtest.splits]` applied; a spec with `labels` loads events + coverage (NEWS / UNCERTAIN / NOISE per candidate); `RunParams`, `engine::candidates` (stops past `max_candidates`) | `application/backtest/mod.rs::prepare` · `domain/backtest/{engine,kinds,labels,features}.rs` · `domain/marketdata.rs` |
| 3 | `--gate`: each candidate → the real `DecisionLoop` on a `SimClock` set to `decided_at`, terminal actions only, no tools; answers through the decision cache (`--offline` = cache only) | `application/backtest/gate.rs::run_gate` · `bootstrap/decision.rs::build_gate` · `DecisionLoop::decide_terminal` · `outbound/decision_cache.rs` |
| 4 | `evaluate`: arms `research` + `capped` (`[risk]` + `[paper]`); with the gate `rules` / `jev` / `rules_capped` / `jev_capped`; fills at bar closes, costs, funding, stats, split halves | `application/backtest/mod.rs::evaluate` · `gate::evaluate_gated` · `domain/backtest/{engine,fills,costs,stats}.rs` |
| 5 | `write_run_dir`: `<state dir>/backtests/<YYYYMMDDTHHMMSSZ>-<strategy>[-N]/` — `report.json` (`backtest/1:<run id>`, `data_through_ms`, the cohort identity: `generation`, `instruments_sha256`, `costs_sha256`), `report.md`, `trades-<arm>.jsonl`, `candidates.jsonl`, `skips.json`, the gate's `decisions.jsonl`; prunes past `[backtest] keep_runs`, never a run the lineage registry or a published ranking's `latest.json` cites | `application/backtest/run_dir.rs` · `domain/backtest/report.rs` |

| Note | Detail |
|---|---|
| The `backtest` tool | steps 1, 2, 4, 5: rules arms only — no gate (spend), no fetch; a split's holdout hidden until `holdout: true`, every read appended to `<state dir>/backtests/holdout-reads.jsonl`; `run_id` + `view` reads a stored run's rows (`tools/xlab/{run,holdout,rows}.rs`) |
| Time integrity | a bar is observable at its close; series read only as-of t; every trade records `data_asof_ms ≤ decided_at_ms` (`domain/backtest/checks.rs` perturbs everything after t) |
| No LLM | the CLI needs a key only for a live `--gate`; `market.db` is read only during a run |
| Operator doc | `docs/xlab-2026-10-01.md` (§ 4 data, § 5 specs, § 6 engine, § 7 gate, § 8 tools, § 14 results) |

### C. `tengu ranking run` — sealed contract → date → lease → backtests → rank → publish

| # | Step | Code |
|---|---|---|
| 1 | Contract: listed in `[strategy_ranking]`; registry reloaded; its newest `[[sealed]]` row `ranking:<id>` hashes the file now — else `contract_unsealed` / `contract_changed`, nothing written | `application/ranking/mod.rs::run_ranking` · `domain/lineage/ranking.rs` · `config/strategy_ranking.rs` |
| 2 | Date: the given one, else the newest local date in the contract's `tz` whose `cutoff` has passed (DST-correct); a `days` weekday — else `not_a_ranking_day` / `cutoff_not_reached` | `application/ranking/mod.rs::ranking_date` · `domain/tz.rs` |
| 3 | A published date (`COMPLETE` / `INCOMPLETE` manifest) comes back as is; else lease `ranking:<contract id>` in `runtime.db` (TTL 15 min; held ⇒ `ranking_busy`), a `RUNNING` / `FAILED` manifest resumed | `application/ranking/store.rs` · `outbound/runtime_store.rs` |
| 4 | Per strategy: freshness (newest bar ≥ cutoff − `max_lag_bars`, else `STALE`) → `prepare` (from = `from`, to = data through = the cutoff, no split) → `evaluate` (rules arms) → `write_run_dir` | `application/backtest/mod.rs` |
| 5 | `RunFacts::from_report` + standing as of the cutoff → `select` (dropped / ineligible / failed reasons) → `rank`: per cohort, weakest first | `domain/backtest/ranking.rs` |
| 6 | Publish `<state dir>/strategy-rankings/<contract>/<date>/ranking.{json,md}`; `COMPLETE` and newest ⇒ `latest.md`, then `latest.json` (commit point); manifest final | `application/ranking/store.rs` |

| Note | Detail |
|---|---|
| Callers | `tengu ranking run \| show` (`cli/ranking.rs`, an external cron) · tool `strategy_ranking` (`run` \| `latest`, `tools/xlab/rank.rs`): `xlab-w2`'s `[feeds.strategy_ranking_daily]` / `_weekend` under `tengu run` (agent `xl_ranker`) |
| No LLM, no network | `market.db` read only; no split ⇒ never a holdout read |
| `INCOMPLETE` | an error (CLI exit 1; the feed shows `down`); `latest` keeps the previous `COMPLETE` ranking |
| Operator doc | `docs/strategy-ranking-automation-2026-10-08.md` |

---

## §2 — The subsystems (file-by-file)

Paths below are relative to `src/`. §2.1–2.6 serve the chat turn; §2.7–2.13 landed 2026-09-24 → 2026-10-01; §2.14–2.17 (lineage + evidence, strategy ranking, source evidence, SOE) 2026-10-06 → 2026-10-08.

### 2.1 Memory — `application/memory/` + ports + outbound stores

Current durable memory is Postgres `agentic_memory` (`adapters/outbound/tools/agentic_memory/`, feature `postgres_memory`) plus Karpathy LLM Wiki Markdown. The layer below serves chat-side memory tools and the builtin provider.

| File | What it owns |
|---|---|
| `ports/memory.rs` | Traits: `VectorStore`, `MemoryProvider`, `Embedding`, `MemoryService` (what memory tools call), `RecallStore` (planner recall lanes) |
| `domain/memory.rs` | Shared types — `MemoryHit`, `ChunkMetadata`, `RecallHit`, `DEFAULT_EMBEDDING_MODEL` |
| `application/memory/manager.rs` | `MemoryManager` — provider holder, prefetch_all/sync_all, `impl MemoryService` |
| `application/memory/injector.rs` | Pre-turn fenced context injection. **Phase 7.1**: only `ChatOrchestratorPortImpl` calls this. |
| `application/memory/writer.rs` | Post-turn spawned non-blocking memory writes. Same Phase 7.1 note. |
| `application/memory/fencing.rs` | `<memory-context>` block helpers |
| `adapters/outbound/memory/disk_vector.rs` | Bincode store — the only `VectorStore` impl |
| `adapters/outbound/memory/embedder.rs` | OpenRouter `text-embedding-3-small` client (1536d), `impl Embedding` |
| `adapters/outbound/memory/builtin.rs` | `BuiltinMemoryProvider` — vector `prefetch` (top 5 for the agent) + `sync_turn`; its AGENTS.md / MEMORY.md / identity / daily-log loader runs only from `initialize`, which only tests call |
| `bootstrap/memory.rs` | Builds the `MemoryManager` for a workspace |

### 2.2 `[agents.*]` — agents live in the sandbox config

No `agents/` directory, no separate spec type. A subagent is an `[agents.<name>]` block with a `description`.

| Item | Where |
|---|---|
| Schema | `config/mod.rs::AgentConfig` (`deny_unknown_fields`) — one struct for in-process agents, subagents, loop / feed agents |
| Routable ⇔ `description` set | `orchestrator/shared_files.rs::routable_agents` renders those blocks (+ `example_queries`) into `TENGU_PLANNER_REGISTRY.md` |
| Private ⇔ no `description`, not `default` | the only agents that may hold an exec tool (xmarket) or a Solana `send` grant (and not a webhook endpoint's `agent`); never `@<id>:`-routable on Telegram |
| Fields | `engine` (`openrouter` \| `local` \| `claude_code`), `model`, `description`, `example_queries`, `tools` (allow-list; workspace-tool names opt in), `skill_packages` (`skills` alias), `workspace`, `workspace_tools`, `scopes`, `limits.max_tool_rounds` (turn cap per step), `limits.step_timeout_secs` (default 600), `identity`, `claude_code` |
| Sandbox sections | `AgentConfig::sandbox` (`config/sections.rs`) — `[xmarket]`, `[risk]`, `[paper]`, calendars, `[rate_limits]`, `[recorder]`, `[backtest]`, `[generation]` (resolved `GenerationScope`), `[sources]`, `[strategy_ranking]` reach tools on every surface |
| Child lookup | `adapters/inbound/cli/run_agent.rs::run_agent_subprocess` loads the parent config first, then `config.agents.get(name)` (`compose.base_agent` when composed) |
| Examples | `sandboxes/lping` (`lping` planner · `crypto_researcher` routable · `lp_executor` private), `sandboxes/xmarket` (no planner · `xm_architect` default + routable · `xm_executor` private), `sandboxes/xlab` (`xl_architect` routable · `xl_jev` private), `sandboxes/xlab-w2` (+ `xl_ranker` private, the ranking feeds' agent), `sandboxes/soe` (`soe_reader` default, `source_evidence` only) |

Edit a block + restart chat → the planner registry file is regenerated on the next planner turn. No rebuild required.

| Sandbox | Runs | Network |
|---|---|---|
| `lping` | Solana LP loops `lp_watch`, `hedge_watch` (dry run) + exec chains `hedge_exec`, `lp_exec` (bound slots, `sequence`, writes in simulate mode) | `open` |
| `jev-exec` | Claude architect → `tengu decide` → Jev executor | `open` |
| `xmarket` | paper desk, stage M0: `tengu run`, tool feeds, `[risk]` $100 | `open`, `allow_hosts` HL |
| `xmarket-weekend` | rule W on paper, tool feeds only (no LLM, no Jev); bound to generation W1 | `open`, `allow_hosts` HL |
| `xlab` | history-first research: `market.db`, `tengu backtest`, Architect; bound to generation W1 (frozen) | `open`, `allow_hosts` HL + GeckoTerminal |
| `xlab-w2` | W2 research: an unbound copy of `xlab` sharing its state dir (`xlab`) + SEC events, `[strategy_ranking]` + ranking feeds | `open`, `allow_hosts` HL + GeckoTerminal + SEC |
| `soe` | source layer (O2): `[sources]` rows (SEC EDGAR, EU TED, shipped disabled), one read-only agent; unbound | `open`, `allow_hosts` SEC + TED |
| `tor-check` · `storage-test` | Tor egress check · `persistent_store` test | `tor` |
| `unlimited` | one OpenRouter agent over Telegram (`[telegram]`) | `open` |

### 2.3 `skills/` — prompt fragments + frontmatter

Skill directories, highest priority first (a name found earlier shadows later ones):
1. `~/.tengu/skills/` (managed)
2. `<workspace>/.tengu/skills/` (workspace dotdir)
3. `<workspace>/skills/` (project)
4. the repo's `skills/` in the cwd, when it is another dir

One list for every reader: `application/skills/registry.rs::skill_directories` — the planner registry (`orchestrator/shared_files.rs::scan_skill_summaries`, frontmatter `name` + `description`, first found wins), `run-agent`'s `load_skill_body_three_tier`, `view_skill`, `skill_resource`, `apply_improver_proposal`.

`skills/orchestrator/SKILL.md` is special — used as the planner system prompt (Phase 4c). Its companion `plan_schema.json` is the JSON shape the prompt asks the planner for. `skills/xlab-research/SKILL.md` (2026-10-01) is the xlab Architect's protocol (split first, tune in-sample, one holdout read); `skills/execution-map/SKILL.md` teaches an Architect the execution-map JSON (§2.8).

### 2.4 `application/orchestrator/` — planner + executor

| File | What it owns |
|---|---|
| `mod.rs` | `Orchestrator` (top-level handle: `handle`, `subscribe`, `cancel`) |
| `planner.rs` | `RagPlanner` legacy type name (only `Planner` impl since 7.1), free fn `parse_verdict`, `cross_session_recall_block`, `session_output_recall_block`, `emit_planner_metrics` |
| `domain/plan.rs` | `Plan`, `Step`, `AgentCompose` (C→B B-half), `StepId`, `ready_steps`, `leaves` |
| `executor.rs` | `DagExecutor` — parallel `ready_steps` with cancel; aborts in-flight steps on cancel / exhaustion; a never-ready step ⇒ replan; parallel leaves joined |
| `retry.rs` | `RetryPolicy` |
| `replan.rs` | `drive()` loop — replan-on-exhausted |
| `events.rs` | `OrchestratorEvent` enum + bus (PlanCreated, StepStarted, RagQueried legacy event name, **MetricsRecorded**, etc.) |
| `wiring.rs` | `ChatOrchestratorPortImpl` (planner-side LLM turn glue). Traits `Planner`, `OrchestratorChatPort`, `WorkerHandle`, `ChatServiceFactory`, `TurnTelemetry` live in `ports/orchestration.rs` |
| `shared_files.rs` | `routable_agents` + `render_registry` → `TENGU_PLANNER_REGISTRY.md` (`ensure_planner_registry`, tool list from the `ToolDirectory` port), per-session `set_active_plan` / `active_plan` (IPC `plan_state`), `TENGU_PLAN.md` debug artifact, `scan_skill_summaries` |

### 2.5 `domain/metrics.rs` + `application/metrics.rs` — context/token observability (added 2026-04-28)

Process-wide observability layer. Every successful planner call, `run-agent` step turn, embedding, wiki-compiler and Jev decisions call emits a `MetricsRecord` (an in-process chat turn emits none) with token counts, latency, and (for the planner) per-context-layer breakdown. Three surfaces — tracing baseline (`RUST_LOG=tengu=info`), in-process broadcast bus, and `OrchestratorEvent::MetricsRecorded` for the TUI/eval recorder.

| Item | What it is |
|---|---|
| `MetricsRecord` | Per-call telemetry: ts, session_id, kind, agent, model, prompt/completion/total tokens, prompt chars+bytes, response chars, latency, layer breakdown, optional step_id. Serde-friendly so it crosses the subagent IPC boundary. |
| `MetricsKind` | `Planner` (one per user turn) / `Subagent` (one per inner-loop turn) / `Embedding` (one per OpenRouter `/embeddings` call) / `WikiCompiler` (`compile_wiki`) / `Decision` (one per successful decisions call). |
| `MetricsLayer` | One row in the planner's per-context-layer breakdown. `plan`: `system`, `roster`, `cross_session`, `history`, `session_recall`, `user_message`; `replan`: `system`, `roster`, `cross_session`, `history`, `recall`, `failure`, `user_message`. Approximate — engine-side framing isn't counted. |
| `install_global_sink` / `record` / `subscribe` (`application/metrics.rs`) | Process-global broadcast sink (`OnceLock<broadcast::Sender>`). `record()` always emits a `tracing::info!` line; bus broadcast is best-effort. |
| `AggregatorState` | Lightweight in-process rollup: overall + by-agent + by-kind. Used by the TUI. |

### 2.6 `adapters/outbound/egress.rs` — network policy (2026-09-16, Tor default 2026-09-18)

`[egress]` (sandbox or base config). Installed once per process; every LLM-initiated network path goes through it. Operator doc: `docs/egress-2026-09-16.md`.

| Item | What it is |
|---|---|
| `EgressConfig` (`config/egress.rs`) | `network = "tor"` (default) \| `"open"`; `proxy` (`socks5h://` only); `route_llm_api`; `allow_hosts` / `deny_hosts`; `https_only`; `shell_network = "proxy_env"` \| `"isolated"`; `audit` / `audit_log`. Unknown keys = parse error. |
| `resolved()` | Under `tor`: `proxy` ← `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050` (the `deploy/tor/` Arti + lyrebird-rs container, `make tor`), `route_llm_api` ← true. Under `open`: direct, false. Explicit values win. |
| `install` / `policy` | Process-global. Called by `main` / `load_sandbox_or` / `run-agent` / `mcp-bridge`; children receive the *resolved* config as `TENGU_EGRESS` (`child_env`), which wins over their own file. |
| `tool_client` / `llm_api_client` / `mcp_client` | The only HTTP clients on LLM paths. Tool client: proxy on, redirects off (`http_request` follows them itself and `check_url`s every hop). LLM client proxied iff `route_llm_api`. Hyperliquid (`HlInfo`), GeckoTerminal, SEC EDGAR, EU TED, Solana RPC and Jev all ride them (SEC / TED: `check_url` per request, one audit line each). |
| `shell_command` / `guard_shell` | `run_command` + shell skills: URL literals checked + audited; `isolated` = macOS `sandbox-exec`, only the proxy port reachable. |
| `claude_cli_env` / `http_connect_proxy` / `claude_code_profile` | Claude Code CLI gets `HTTPS_PROXY` = HTTP CONNECT form of the SOCKS proxy (Arti serves CONNECT on 9050); builtin `Bash` dropped while proxied. |
| `AttributedExecutor` / `CallScope` | a `tengu run` feed's or loop's calls name their own `agent` / `session` / `call_id` on the audit lines (`<TENGU_HOME>/logs/egress.jsonl`) |
| `warn_if_proxy_unreachable` | One startup warning from the parent CLI (after sandbox resolution) when the proxy port is closed. `tengu doctor` prints the resolved network. |
| Telegram | `TelegramPipe::build_bot` builds teloxide's reqwest 0.11 client (`reqwest011` alias) through the proxy's HTTP CONNECT form, 30s connect / 60s timeout (teloxide's 5s/17s defaults time out over Tor). |

### 2.7 `tengu run` — runtime supervisor, feeds, lease, heartbeat (2026-09-30)

| File | What it owns |
|---|---|
| `adapters/inbound/run.rs` | `run_runtime`: signals (a second one exits 130), webhook routes (feature `webhooks`), `tengu run started` log line |
| `bootstrap/runtime.rs` | `LeasePlan` / `OwnerLeases`, every loop built once, `start_feeds`, `Runtime::spawn` / `shutdown` |
| `application/runtime/mod.rs` | `Supervisor` (named tasks on one stop signal; early task death fails the run), `keep_lease` |
| `application/runtime/loops.rs` | `LoopDispatch`: one event at a time per loop, `max_decisions_in_flight` across loops, ≤ `max_queued_per_loop` waiting (`Refused::QueueFull` ⇒ webhook 429); also used by `tengu webhooks` |
| `application/runtime/health.rs` | `HealthBoard`: `<state dir>/run-<s>.json`, `loop/1:<name>`, `feed/1:<name>` (`FeedWriter`) |
| `application/runtime/feeds.rs` | `run_feed`: tool / tick feeds, backoff per error class (`domain/backoff.rs`), at-tick slots retried 15 min |
| `domain/runtime.rs` · `domain/schedule.rs` | leases + `live_verdict` (pure) · `next_fire` (grid, windows, at-ticks, DST) |
| `ports/runtime.rs` · `outbound/runtime_store.rs` | `RuntimeStore` · `<state dir>/runtime.db` lease (acquire / renew / release, TTL takeover) |
| `ports/clock.rs` · `outbound/clock.rs` | `Clock` + `SimClock` · `SystemClock` |
| `config/runtime.rs` · `config/feeds.rs` | `[runtime]` (grace, in-flight, queue, heartbeat) · `[feeds.<n>]` (validated against agents' tools and loops) |
| `cli/doctor.rs` | `tengu doctor --sandbox <s> --live` (healthcheck for a `tengu run` container) |

Doc: `docs/runtime-2026-09-30.md`.

### 2.8 Decision loops (Jev) + clock + replay (2026-09-24; replay 2026-10-01)

Outside the chat turn: Jev picks actions; tools execute. Docs: `docs/decision-loop-plan-2026-09-24.md`, `docs/xlab-2026-10-01.md` § 7.

| File | What it owns |
|---|---|
| `config/decision_loop.rs` | `[decision_loops.<name>]`: actions, slots (static / `from` history / `observation` / `event` — each a list Jev picks from, or one bound value with no question; unresolved ⇒ the action is illegal), caps, `sequence = ["a", "b?"]` (one step at a time), `dry_run`, `world`, `requires`, `act_at` |
| `config/execution_map.rs` | an Architect's execution map (JSON): order, event, tighter caps — only NARROWS the TOML loop; `bootstrap/decision.rs::build_mapped_loop`, audit `trigger = "map:<sha256>"`; skill `execution-map` |
| `application/decision_loop/{mod,slots,reduce,world}.rs` | per event: read `world` → Jev decisions call → `act_at` gate → `execute_typed` → history (`obs` meta); a risk denial = outcome `Refused`; time from a `Clock` (`with_clock`); `decide_terminal` → `Verdict` (replay) |
| `domain/decision.rs` · `ports/decision.rs` | `Question` / `Answer` / `Decision`, `HistoryEntry`, `StepOutcome`, `Verdict` · `DecisionEngine`, `Escalator` |
| `bootstrap/decision.rs` | `build_decision_loop`, `agent_tool_executor` (`SanitizedToolExecutor`); replay: `build_replay_loop` (terminal-only, no history, no tools), `cached_decision_engine`, `build_gate` (K loops, each on its own `SimClock`) |
| `outbound/decisions.rs` | `JevClient` — OpenRouter `/api/alpha/decisions` (`llm_api_client`), one retry on 429 / 5xx, circuit open 30 s after 3 failures |
| `outbound/decision_cache.rs` | `CachedDecisionEngine` — key = sha256 hex of canonical `{model, state, questions}`; `<state dir>/backtests/decision-cache.db`; an offline miss is an error |
| `cli/decide.rs` | `tengu decide --sandbox <s> --loop <n> [--event f.json]` · `tengu decide --sandbox <s> --map <file\|->` |

Audit: `<TENGU_HOME>/logs/decisions.jsonl` (one `write_all` per line, failed calls too); a replay's lines go to the run's `decisions.jsonl` instead.

### 2.9 Typed observations, observation store, history recorder (2026-09-24 / 2026-09-30)

| File | What it owns |
|---|---|
| `domain/observation.rs` · `ports/observation.rs` | `Observation` envelope (`Field<T>`, `ObsStatus`, ≤ 32 features, `render_text` line 1 ≤ 200 chars with full ids, `CachePolicy`) · `ObservationStore` |
| `application/observe.rs` | `observe()` — cache-or-fetch; `Error` never cached; `max_age_secs = 0` forces a live read |
| `adapters/outbound/observations.rs` | `SqliteObservationStore` (`<workspace>/.tengu/observations.db`: slot-monotonic upsert, 7-day purge); `open_observation_store` wraps it in `RecordingObservationStore` when `[recorder]` is on |
| `ports/history.rs` · `adapters/outbound/history_sqlite.rs` | `HistoryStore` · `<state dir>/history/<YYYYMMDD>.db` UTC day files, retention sweeper; read with `tengu history range` / `asof` |
| `config/recorder.rs` | `[recorder]` — schemas, `keep_data`, `change_only` + heartbeat, `min_interval`, retention; needs `[xmarket]` |
| `domain/market.rs` · `domain/book.rs` | cross-venue rows `mkt_instrument/1`, `mkt_ctx/1` keyed `<venue>:<native id>` · venue-neutral L2 book, depth walk |
| `application/decision_loop/world.rs` | loop `world` aliases read from the store (fresh / stale / missing / error) |

Doc: `docs/typed-observations-2026-09-24.md`.

### 2.10 Tool families (opt-in names in `domain/tools.rs::WORKSPACE_TOOLS`)

| Family | Tools | Code |
|---|---|---|
| Solana LP observe / plan (11) | `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `lp_swap_plan`, `hedge_decide`, `lp_decide` | `adapters/outbound/tools/solana/` · `adapters/outbound/solana/{rpc,accounts,http_json,plan,layouts}.rs` · `domain/solana.rs`, `domain/lp/` |
| Solana write (5) | `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order` — `mode = "simulate"` default; `send` = `[solana] signer_key_file` + a wallet grant on a private agent | `tools/solana/write_*.rs` · `outbound/solana/{send,signer,writes_store}.rs` · `domain/solana_tx.rs`, `domain/solana_write.rs` · `config/solana.rs` |
| Hyperliquid (2) | `hl_ctx` (dex sweep or ≤ 64 coins → `mkt_ctx/1` + `mkt_instrument/1`), `hl_book` (`hl_book/1` L2 book) | `tools/hyperliquid/` · `outbound/hyperliquid/info.rs` (`HlInfo`, `[rate_limits.hyperliquid]`) · `domain/hl/` |
| xmarket risk / paper (6) | `risk_status`, `paper_positions`; exec tools `paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade` | `tools/xm/` (§2.11) |
| xlab (3) | `market_history` (`mkt_history/1`), `backtest` (`backtest/1:<run id>`), `strategy_ranking` (`run` \| `latest`; no row — the ranking is its files) | `tools/xlab/` (§2.12, §2.15) |
| Sources (1, read-only) | `source_evidence` (`source_asof/1`, the as-of evidence packet; agents never fetch) | `tools/sources/` (§2.16) |

Shared: `outbound/rate_limit.rs` (`[rate_limits.<name>]`, per process), `outbound/http_class.rs` (HTTP → `ErrorClass`, URL scrubber), `domain/backoff.rs`. The hidden `tengu tool list` prints every catalog name (47 in a default build: 14 default + 33 opt-in; `agentic_memory` joins with `postgres_memory`). In a `[generation]`-bound sandbox the executor and the bridge drop a tool the generation refuses (`bootstrap/tools.rs::within_generation`; `Config::load` refuses listing one).

### 2.11 xmarket — risk gate, paper ledger, fill engine, kill switch, exits, weekend fade (2026-09-30 / 2026-10-01)

| File | What it owns |
|---|---|
| `domain/xm/risk.rs` | `evaluate`: `OrderIntent` + `RiskContext` + `RiskLimits` ⇒ `RiskVerdict`, every rule a `Check`, fail closed (`missing:<field>`); `evaluate_shadow` for shadow accounts |
| `domain/xm/risk_state.rs` | halts (`daily_loss` clears 00:00 UTC; `total_loss` / `operator` / `file` only by resume), day roll, `risk_state/1` |
| `domain/xm/paper.rs` · `application/paper.rs` | fill engine (market / IOC on an L2 book, HL reject codes) · `fill_with_latency` (sleep on the `Clock`, then read the book, then fill) + `decide` (the closure `place` runs) |
| `domain/xm/{exec,ledger,cost}.rs` | order ids + `order_fingerprint`, kept venue facts · positions, funding settled at the size held · HL fees, rounding, edge after costs |
| `ports/paper.rs` · `adapters/outbound/paper_store.rs` | `PaperLedger::place` (funding + gate + fill + write, one transaction, idempotent per `client_order_id`) · `<state dir>/ledger.db`, verdicts mirrored to `logs/risk.jsonl`, account owners (`account_owner_mismatch`) |
| `tools/xm/exec_common.rs` | `run_exec` — the gate inside every exec tool (private agent, ids, store reads, latency, TP / SL on the live book) |
| `domain/xm/exits.rs` · `tools/xm/exits.rs` | `[risk.exits]` deadline / max hold / take-profit / stop-loss, exit ids + retry rule · `xm_exits` |
| `domain/xm/weekend_fade.rs` · `tools/xm/weekend_fade.rs` | rule W: window (DST-safe, holidays), signals, capped + shadow ledgers, golden replay · `xm_weekend_fade` (one idempotent step per call) |
| `config/risk.rs` · `config/xmarket.rs` · `config/hardening.rs` · `config/sections.rs` | `[risk]` / `[paper]` / `[risk.exits]` (every field required) · `[xmarket]` state dir + layout, calendars, `[xmarket.weekend_fade]`, load rules · hardened-sandbox rules · `SandboxSections` |
| `domain/{tz,calendar}.rs` | New York / Paris DST clock · exchange sessions, holidays, weekend window |
| `cli/risk.rs` | `tengu risk status` / `halt` / `resume` (halt and resume: operator at a terminal only; `[risk] kill_switch_file` blocks resume) |

Docs: `docs/xmarket-risk-paper-2026-09-30.md`, `docs/xmarket-tracker-2026-09-29.md` (§ 0, W1 notes).

### 2.12 xlab — market-data warehouse, backfill, backtest engine, gate arm, tools (2026-10-01)

| File | What it owns |
|---|---|
| `domain/marketdata.rs` · `domain/marketdata_decode.rs` · `domain/marketdata_stats.rs` | `Interval`, `Bar`, `FundingPoint`, `CtxPoint`, series observable at close, `StockSplit` · HL / Gecko / archive / JSON decoders · `mkt_history/1` statistics |
| `domain/sec.rs` | SEC EDGAR decoders (Phase 7): ticker map, submissions, the index page's `Accepted` (New York → UTC = `published_ms`), forms kept (8-K, 6-K, 10-Q, 10-K, 20-F, 40-F + amendments) |
| `ports/market_data.rs` · `adapters/outbound/market_data.rs` | `MarketDataStore` · `SqliteMarketData` over `<state dir>/market.db` (`bars`, `funding`, `ctx`, `events`, `event_coverage`) |
| `adapters/outbound/backfill/{mod,hl,gecko,hl_archive,json,sec}.rs` | HL `candleSnapshot` + `fundingHistory`, GeckoTerminal OHLCV, HL S3 `asset_ctxs` import, JSON import, SEC filings → `events` (`$SEC_USER_AGENT`, `[rate_limits.sec]`); resume (`missing_ranges`), `Retry` |
| `domain/backtest/{spec,kinds,labels,engine,fills,costs,features,stats,gate,report}.rs` | pure engine: six strategy kinds, candidates, information labels (NEWS / UNCERTAIN / NOISE from `events`), arms (research / `[risk]`-capped), fills + costs + funding, features as-of t, cluster-bootstrap stats, the gate's pure half, `report.json` / `report.md` (`checks.rs`, `testkit.rs`: tests only); `evaluation.rs` §2.14, `ranking.rs` §2.15 |
| `application/backtest/{mod,gate,run_dir}.rs` | `resolve` → `prepare` → [`run_gate`] → `evaluate` / `evaluate_gated` → `write_run_dir`; cohort identity in `report.json`; retention keeps cited runs |
| `domain/canonical.rs` | canonical JSON + sha256 hex (`spec_sha256`, decision-cache key, ranking `content_sha256`) |
| `adapters/outbound/tools/xlab/{mod,defs,history,run,holdout,rows,rank}.rs` | `market_history`, `backtest`: holdout ledger (`holdout.rs`), stored-run rows by run id (`rows.rs`); `strategy_ranking` (`rank.rs`, §2.15) |
| `adapters/inbound/cli/{history,backtest}.rs` | `tengu history backfill` / `events` / `import-hl-archive` / `import-json` / `coverage`; `tengu backtest` (`--gate`, `--max-decisions`, `--concurrency`, `--offline`, `--fetch`, `--split`, `--data-through`) |
| `config/backtest.rs` | `[backtest]`: costs per prefix, universes, strategies, splits, gate, `max_candidates`, `keep_runs` (needs `[xmarket]`) |

Sandbox `sandboxes/xlab` (bound to W1): `xl_architect` (default, routable; `market_history`, `backtest`, skill `xlab-research`), `xl_jev` (private, owns `[decision_loops.xl_gate]`). `sandboxes/xlab-w2` (unbound, same state dir): the same agents + `xl_ranker` (private; `market_history`, `strategy_ranking`). Docs: `docs/xlab-2026-10-01.md`, `docs/p7-news-labels-2026-10-08.md`.

### 2.13 Engine parity (E0) + hardening (2026-09-30 / 2026-10-01)

Every tool works under `openrouter`, `local` and `claude_code` (operator rule, no exceptions).

| Piece | Where |
|---|---|
| Schema subset lint (every catalog row, skills, `[[mcp_servers]]` at discovery) | `adapters/outbound/tools/schema_lint.rs` · `mcp_client::linted_tool` |
| Bridge conformance — same text + rows in-process and through a real `tengu mcp-bridge` | `tests/bridge_conformance.rs` + hidden `tengu tool list` / `call` / `turn` (`cli/tool.rs`) |
| Live engine matrix + smoke | `tests/engine_matrix.rs` (`every_catalog_tool_has_a_live_leg`) · `tengu doctor --engines` (`domain/engine_smoke.rs`) |
| Bridge runs tools as the agent | engine writes `TENGU_CONFIG` + `TENGU_BRIDGE_AGENT` (`outbound/bridge_env.rs`); `inbound/mcp_bridge.rs` loads that block behind `SanitizedToolExecutor`; call id `mcp:<nonce>:<id>` |
| Local model fit | `Engine::tool_result_char_cap` (`engines/local.rs`) + `Observation::compact_text` |
| Hardened `claude_code` | `--strict-mcp-config` always; `builtin_tools_profile = "none"` ⇒ no settings files, hooks, plugins, skills, CLAUDE.md (`engines/claude_code.rs::cli_args`); required in `[risk]` / signer sandboxes (`config/hardening.rs`) |

Doc: `docs/engine-backends.md` § Engine matrix, `docs/mcp-bridge.md`.

### 2.14 Research lineage + evidence (2026-10-06; Phase 6 evaluation 2026-10-08)

What an experiment's figures rest on, and which capabilities a sandbox may reach. Docs: `docs/lineage-2026-10-06.md`, `docs/p6-decision-evaluation-2026-10-08.md`.

| File | What it owns |
|---|---|
| `lineage/` (repo root) | one TOML per record: `families/`, `variants/`, `experiments/`, `episodes/`, `incidents/`, `capabilities/`, `generations/`, `evidence/`, `rankings/`; `locks.toml` append-only (`[[frozen]]`, `[[sealed]]`) |
| `domain/lineage/` | record types, `Registry::validate` (`invalid_id` … `ranking_unsealed`), queries `trace` / `family_report` / `acceptance` (the 21 Rule-W answers), pin hashes (`pins.rs`) — pure |
| `config/lineage.rs` | `load_registry`; `[generation] id, registry` — `Config::load` refuses a tool, feed tool, loop action tool or strategy kind outside the bound generation and recomputes a FROZEN one's `config:` / `spec:` pins (`pin_drift`); `GenerationScope` → `SandboxSections::generation` |
| `ports/lineage.rs` · `adapters/outbound/lineage/` | `ContractProbe` (`RepoProbe`), `EvidenceResolver` (`FsResolver`), `ResultSource` (`ReportJson`, `LedgerGrade`), `AttemptSource` (`RunDirs`) — read-only |
| `application/lineage/{verify,attempts}.rs` | `verify` (+ `--pins`, `--evidence`), `pin_status`, `attempts::scan` |
| `cli/lineage.rs` | `tengu lineage verify \| show \| trace \| family \| attempts \| report \| capabilities \| generation \| seal` — only `seal` writes (one `[[sealed]]` row) |
| `domain/evidence.rs` · `domain/evidence_coverage.rs` | `EvidenceClass`, `Provenance`, `EvidenceRecord` (a vault pinned by sha256), `tree_hash` · cadence slots → `LIVE_RECORDED` / `BACKFILLED` / `MISSING` |
| `domain/xm/grade.rs` · `domain/xm/regrade.rs` | a paper ledger → trades, totals, 12 reconciliation checks per account · rule W (or a variant) re-graded from recorded `mkt_ctx/1` + `hl_book/1` rows |
| `domain/backtest/evaluation.rs` | rules · Jev · HOLD on the same candidates of a gated run; Jev `PROVEN` / `UNPROVEN` / `REJECTED`, calibration, latency, cost, capped books |
| `ports/evidence.rs` · `adapters/outbound/evidence/` | `Vault` (the only write path), `LedgerSource`, `RecordedHistory`, `BackfillSource`, `RunDirSource` — SQLite `immutable=1`, a non-empty `-wal` refused |
| `application/evidence.rs` · `cli/evidence.rs` | `tengu evidence snapshot \| verify \| coverage \| grade \| evaluate \| regrade` — no config, no LLM, no network |

State: generation W1 `FROZEN`, bound by `sandboxes/xlab` and `sandboxes/xmarket-weekend`; W1's Jev is UNPROVEN (`docs/p6-decision-evaluation-2026-10-08.md`).

### 2.15 Strategy ranking (2026-10-08)

Flow: §1b C. Doc: `docs/strategy-ranking-automation-2026-10-08.md`.

| File | What it owns |
|---|---|
| `domain/lineage/ranking.rs` | the contract `lineage/rankings/<id>.toml`: strategies, `evidence_class` `DEVELOPMENT`, `arm`, `tz` / `cutoff` / `days` / `from`, cohort, `on_missing`, freshness, eligibility, rating; sealed with `tengu lineage seal ranking:<id>` |
| `domain/backtest/ranking.rs` | pure: `RunFacts::from_report` → `StrategyStanding::of` → `select` → `rank` (per cohort, weakest first), `content_sha256`, `ranking.md`, compact text |
| `config/strategy_ranking.rs` | `[strategy_ranking] registry, contracts` → `SandboxSections::ranking` (its cited runs kept by run-dir retention) |
| `application/ranking/{mod,store}.rs` | `run_ranking` (contract, date, lease, manifest, freshness, backtest, rank, publish) · `<state dir>/strategy-rankings/<contract>/` files, temp file + rename |
| `cli/ranking.rs` | `tengu ranking run \| show` |
| `tools/xlab/rank.rs` | `strategy_ranking` — `run` \| `latest`; lease holder = the call id |

### 2.16 Source evidence — O2 (2026-10-08)

Facts from approved sources with full provenance, read as-of t. Docs: `docs/source-evidence-2026-10-08.md`, page `docs/tutorial/source-evidence.html`.

| File | What it owns |
|---|---|
| `domain/source/` | `SourceRecord` (`source_record/1`), `Fact` (`SecFiling`, `TedNotice`, `Unparsed`, `Withdrawn`), per-class rules, the two-clock as-of view (`captured` · `knowable`), evidence packet `source_asof/1` (free text fenced), SEC / TED record builders — pure |
| `config/sources.rs` | `[sources] state` + `[sources.registry.<id>]` rows (`sec_edgar`, `ted_search`), shipped `enabled = false`; enabling needs reviewed terms + both retention periods |
| `ports/source_store.rs` · `adapters/outbound/sources/store.rs` | `SourceStore` · `SqliteSourceStore` over `<TENGU_HOME>/state/<sources.state>/sources.db` — append-only snapshots, records, coverage, cursors, purge tombstones, runtime switches; never `market.db` |
| `adapters/outbound/sources/{mod,sec,ted}.rs` | `fetch_gate` (enabled, reviewed terms, no runtime off switch — before any request); SEC via `backfill/sec.rs`' client; TED `POST /v3/notices/search`, one day at a time |
| `application/sources.rs` | `evidence_as_of`, `purge_request` |
| `cli/sources.rs` | `tengu sources list \| fetch \| import \| asof \| purge \| terms \| disable \| enable` — only `fetch` uses the network |
| `tools/sources/` | `source_evidence` — read-only; agents never fetch |

Sandbox `sandboxes/soe`: `soe_reader` (default, `source_evidence` only, Claude Code `builtin_tools_profile = "none"`), no feeds, loops or `[[mcp_servers]]`.

### 2.17 Software Opportunity Engine — SOE O0–O2 (2026-10-08)

Offline: no sandbox config, secrets, egress, LLM or fetch. Doc: `docs/soe-2026-10-08.md`, page `docs/tutorial/soe.html`.

| File | What it owns |
|---|---|
| `domain/soe/` | integer minor-unit money + unknown-safe `Est` (`value.rs`), records `soe.<record>/1`, economics (downside / base / upside), PRD § 7.2 hard gates (`PASS` / `HOLD` / `REJECT`), capability fit, ranking + the `HOLD` week, eval cases — pure; imports `domain/source/`, never the reverse |
| `config/soe.rs` | the signed private profile `<TENGU_HOME>/state/soe/operator.toml` (0600, refused inside a git tree or unsigned), opportunity + record-dir + `--cited` loaders |
| `cli/soe.rs` | `tengu soe init \| check \| portfolio \| sensitivity \| eval` — only `init` writes |

---

## §3 — Stores: Open Brain, wiki, planner files, runtime + research state

| Store | What it holds | Written by | Read by | TTL? |
|---|---|---|---|---|
| `TENGU_PLANNER_REGISTRY.md` | `[agents.*]` blocks with a `description` + skills + core & MCP tool defs | `shared_files::ensure_planner_registry` before planner calls | `RagPlanner::plan/replan` prompt context | NO |
| `TENGU_PLAN.md` | current accepted plan/replan (debug artifact; IPC `plan_state` is the source of truth) | `replan.rs::drive` | humans; old-parent fallback in `run_agent_subprocess` | overwritten |
| Postgres `agentic_memory` | Open Brain live memory: user messages + step outputs + source chunks | planner + `run-agent` + `agentic_memory` tool when `postgres_memory` is enabled | planner recall + agents via tool | none yet |
| `.tengu/agentic-memory/wiki/` | Karpathy LLM Wiki compiled Markdown | `agentic_memory compile_wiki` | agents, humans, future MCP surface | Git/history |
| Disk bincode vector store (`adapters/outbound/memory/disk_vector.rs`: `<workspace>/memory/vectors.bin`, else `[memory] store_path`, default `~/.tengu/memory/`) | chat-side `memory_ingest` / `memory_search` / `persistent_store` entries | `MemoryPlugin` tools | same tools + `BuiltinMemoryProvider` | none (per-id `delete`, `clear_all` via `/purge`; no sweep) |
| `<workspace>/.tengu/observations.db` | typed rows `<schema>:<subject>` (`acct/1`, `mkt_ctx/1`, `hl_book/1`, `paper_positions/1`, `loop/1`, `feed/1`, …) | typed tools via `observe()`, feeds, `HealthBoard` | the same tools (cache hits), loop `world`, `doctor --live` | per-row `ttl_ms`; purged after 7 days |
| `<state dir>/history/<YYYYMMDD>.db` | recorded observation history (`[recorder]`) | `RecordingObservationStore` | `tengu history range` / `asof` | `[recorder]` retention |
| `<state dir>/ledger.db` | paper accounts, positions, orders, fills, funding, risk verdicts, account owners | exec tools via `PaperLedger::place` | `risk_status`, `paper_positions`, `tengu risk status` | none |
| `<state dir>/runtime.db` · `run-<s>.json` | leases `runtime:<s>`, `state:<dir>`, `ranking:<contract id>` · heartbeat | `tengu run`, `tengu webhooks` (leases only), the ranking coordinator | the next runner · `tengu doctor --live` | lease 30 s (ranking 15 min) |
| `<state dir>/market.db` | `bars`, `funding`, `ctx`, `events` (SEC filings), `event_coverage` by full instrument id | `tengu history backfill` / `events` / imports, `market_history` `fetch` | `tengu backtest`, `backtest`, `market_history`, labels, ranking freshness, `tengu evidence coverage` / `regrade` (read-only) | none |
| `<state dir>/backtests/<run id>/` · `decision-cache.db` · `holdout-reads.jsonl` | one run's report + trades · Jev answers · every holdout read (tool and `tengu backtest --split`) | `write_run_dir` · `CachedDecisionEngine` · `tools/xlab/holdout.rs`, `cli/backtest.rs` | humans, `backtest` `run_id` reads, rankings, `tengu evidence evaluate` · gate replays · the Architect's protocol | `[backtest] keep_runs` (100; cited and latest-ranked runs kept) · none · append-only |
| `<state dir>/strategy-rankings/<contract>/` | `<date>/manifest.json`, `ranking.json`, `ranking.md` · `latest.md`, `latest.json` | `application/ranking/store.rs` | `tengu ranking show`, `strategy_ranking` `latest`, run-dir retention | none (a published date never rewritten) |
| `lineage/` (repo) | the registry: one TOML per record, `locks.toml` | humans; `tengu lineage seal` appends `[[sealed]]` | `tengu lineage`, `Config::load` (`[generation]`, `[strategy_ranking]`) | git |
| `<TENGU_HOME>/state/evidence/<vault>/` | read-only copies of evidence + `MANIFEST.json` (sha256 per file) | `tengu evidence snapshot` (once, then `chmod a-w`) | `tengu evidence verify` / `grade` / `evaluate`, `tengu lineage verify --evidence` | none |
| `<TENGU_HOME>/state/<sources.state>/sources.db` | raw snapshots (sha256), source records, coverage, cursors, tombstones, kill switches | `tengu sources fetch` / `import` / `terms` / `disable` / `enable` / `purge` | `tengu sources asof`, `source_evidence` | per-row retention (`raw_retention_days`, `record_retention_days`; 0 = forever) |
| `<TENGU_HOME>/state/soe/operator.toml` | the signed private operator profile (0600) | `tengu soe init` (template), the operator | every other `tengu soe` command | none |
| `<TENGU_HOME>/logs/{egress,decisions,risk}.jsonl` | network audit · Jev calls · risk verdict mirror | `egress.rs` · decision loops · `paper_store.rs` | operators | `tengu prune` deletes `logs/` |
| `<TENGU_HOME>/state/solana-writes.db` | per-wallet send lease, pending sends, write fences | `outbound/solana/send.rs` | the next send | none |

`<state dir>` = `<TENGU_HOME>/state/<[xmarket] state>` (else `<TENGU_HOME>/state`); outside every fs root and workspace; `tengu prune` never deletes `<TENGU_HOME>/state` beyond `state/flows` (layout: `config/xmarket.rs`, `docs/runtime-2026-09-30.md` § State layout).

---

## §3.5 — Context/token metrics (2026-04-28)

Three surfaces, one record:

1. **Tracing baseline (always on)** — every `metrics::record()` writes a structured `tracing::info!` with `kind`, `agent`, `model`, `prompt_tokens`, `completion_tokens`, `latency_ms`, `prompt_chars`. Run `RUST_LOG=tengu=info` to see every LLM, embedding and Jev call inline.
2. **In-process broadcast bus** — `OnceLock<broadcast::Sender<MetricsRecord>>` in `application/metrics.rs`. Installed once by `build_orchestrator`. Subscribers (`metrics::subscribe()`) get the live stream; lagging subscribers see `RecvError::Lagged` and skip ahead (standard broadcast semantics).
3. **TUI bottom panel** — opt-in via `TENGU_TUI_METRICS=1`. Renders a compact one-liner as a System bubble: `metrics: tok in/out 4.5k/812 · last planner (1.2k) · session 5.6k · researcher 4.0k`. Aggregator absorbs records even when the panel is OFF, so flipping it on mid-session shows real numbers, not zero.

The subagent's per-turn records cross the IPC boundary in `AgentIpcOutput.metrics` (field with `default + skip_serializing_if` so older child binaries stay compatible). The parent's runner re-emits each one on the global sink.

---

## §4 — Things that surprised me reading this code

### A. The doctrine is real

"LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" isn't just a mantra. The planner LLM call in `RagPlanner::plan` gets no tools, no tool executor, no memory manager, and `suppress_grounding_nudge`, because the planner has exactly one job: emit plan JSON. See `bootstrap/orchestrator.rs::RuntimeChatServiceFactory::run_turn_inner` (reached through `run_turn_with_system`). The boundary is enforced in code.

### B. Planner and runner share one `session_id`

The caller resolves the id once (`bootstrap::orchestrator::resolve_session_id`: `TENGU_SESSION_ID` env override or fresh UUID; `tengu webhooks` mints `webhook-<name>-<uuid>` per request) and `build_orchestrator` hands the same string to both `RagPlanner` and `SubprocessRunner`, so planner messages and subagent step summaries share the same session key.

### C. Sandbox config crosses the IPC boundary (Phase 7.2)

Without this you get the bug the user hit: child loaded `~/.tengu/config.toml` not the parent's sandbox config, so HTTP scopes denied even when the parent allowed them. The fix is `Config.sandbox_name` (`#[serde(skip)]`, populated by `load_sandbox_or`), threaded through `SubprocessRunner` → `AgentIpcInput.sandbox_config` → child's config-load fallback.

### D. `compress_and_store` is implicit

Never list it in `[agents.<name>].tools` — the runner appends it to every subagent automatically. The middle-ground protocol (Phase 5c): a step is only `Failed` when no text emitted AND no `compress_and_store` called. Otherwise `Ok` with a tracing warning. Most agents are well-behaved and call it; the warn path catches the rest.

### E. There are TWO layers of recall in the planner prompt

1. **In-memory ring buffer** (Phase 6.4 lite) — last N user messages this process. Survives within one chat. Lost on restart.
2. **Open Brain block** — recall over Postgres `agentic_memory`. Survives restart. Opt-in via `memory.cross_session_msg_top_k > 0` (default 0 = off).

Both inject **before** the current turn. Different time scales — lite is "what did the user say two turns ago", full is "what did the user ask about three days ago that's similar to now". (A third, opt-in lane — `within_session_output_top_k` — recalls this session's step outputs.)

### F. C → B fallback is a two-turn dance

When no listed agent fits the request:
- **Turn 1 (C)**: planner emits `Direct { response: "I don't have a perfect match. Closest is researcher (0.18)..." }` asking the user.
- **Turn 2 (B)**: on user confirmation, planner emits `Plan` with `Step.compose` set. Child loads `[agents.<compose.base_agent>]`, OVERRIDES `skill_packages` + `tools` with the values the planner picked from the registry file. **This run only — config on disk unchanged.** In a hardened sandbox (`[risk]` / Solana signer) the override may only narrow the base (`bootstrap::tools::compose_agent`); a widening one fails the step before the engine is built.

Implementation: `domain/plan.rs::AgentCompose`, threaded through `AgentIpcInput.compose`, applied in `run_agent_subprocess`.

### G. Engines: three backends, mixed is the verified pattern (Phase 7.3, `local` 2026-09-23)

Each `[agents.<name>]` block declares its `engine`: `"openrouter"`, `"local"` (any OpenAI-compatible server on the host — Unsloth, Ollama, llama.cpp, vLLM; direct, never via the proxy) or `"claude_code"` (same field for in-process agents). Model slug format depends on engine: OpenRouter wants `anthropic/claude-sonnet-4-6`; Claude Code wants the bare `claude-sonnet-4-6` (build with `--features claude_code`); `local` sends the server's own id.

**Don't put a Claude Code agent in the planner role.** The Claude Code CLI has tool access via MCP at engine-construction time, so the planner-side tool stripping (intended for OpenRouter's per-turn `tools = []`) doesn't prevent the planner LLM from calling tools instead of emitting plan JSON. **The verified pattern is OpenRouter for the planner, Claude Code for subagents.**

### H. Claude Code subagents see tengu's tools via the MCP bridge (Phase 7.4)

`adapters/inbound/cli/run_agent.rs::run_agent_subprocess` sets `EngineContext.bridge_tools = Some(tools)` when the agent's `engine == "claude_code"`. Claude Code spawns `tengu mcp-bridge` as an MCP server, advertises tools as `mcp__tengu-tools__<name>`, and routes calls through it. The bridge has its own `ToolRegistry` (built by `build_bridge_executor`) from the same tool catalog (`register_catalog`) so it can't drift from the in-process registry. Since 2026-09-23 the engine also passes the `[[mcp_servers]]` behind any `{server}__{tool}` bridge entry (`TENGU_BRIDGE_MCP_SERVERS`, names in `adapters/outbound/bridge_env.rs`; since 2026-10-01 their names only — the bridge takes them from its loaded config); the bridge registers `McpPlugin` for them. Each bridged call sees the run's conversation (`TENGU_BRIDGE_TRANSCRIPT_FILE`, kept current by the engine), as in-process tools see the loop's messages.

Since `x-bridge-parity` (2026-09-30) the bridge runs tools as the calling agent: the engine forwards `TENGU_CONFIG` (absolute) + `TENGU_BRIDGE_AGENT`, and the bridge loads that `[agents.<name>]` block (scopes, sandbox sections, `no_shell_fallback`), redacts the process secrets and passes the JSON-RPC request id, behind a per-process nonce (`mcp:<nonce>:<id>`, so an exec tool's idempotency key never repeats across CLI sessions), as `ToolCtx.call_id` (`docs/mcp-bridge.md`). The Claude CLI merges the MCP config's `env` over its own env (verified, CLI 2.1.285), so the bridge inherits the parent's env — secrets such as `OPENROUTER_API_KEY` arrive that way, never written into the `--mcp-config` file. The engine writes only non-secret values there: the bridge contract (`TENGU_BRIDGE_*`), `TENGU_EGRESS`, `TENGU_SESSION_ID` (Open Brain `capture` stamps it), `TENGU_SECRETS_LOADED` (vault key names).

### I. parse_verdict has a prose-fallback (Phase 7.4)

When the planner LLM returns prose instead of JSON (Claude Code being conversational, even with strict instructions), `parse_verdict` falls back to wrapping the whole text as a `Direct { response }` verdict with a warn log. No more `System error: orchestrator initial call failed` for prose — the user sees the model's reply, the warn surfaces the issue. Path 4 in `parse_verdict`'s tolerant cascade.

### J. Adding a tool is a one-row edit (2026-09-23) — and three parity checks (2026-09-30)

`catalog()` in `adapters/outbound/tools/mod.rs` lists every built-in tool group as a `ToolEntry` (opt-in name, memory gate, defs, plugin). `register_catalog` (used by both `bootstrap/tools.rs::build_tool_executor` and the MCP bridge) and `advertised_defs` (the tool list the model sees) both read it. Opt-in names also live in `domain/tools.rs::WORKSPACE_TOOLS` for config validation; `catalog_tests` enforce the sync. `SkillPlugin` and `McpPlugin` stay outside the catalog. A tool is done when its schema lint, bridge conformance case and live engine-matrix leg pass (§2.13). Recipe: `docs/tools.md`.

### K. Every LLM network path goes through `egress.rs` (2026-09-16)

`[egress] network = "tor"` is the default — with no `[egress]` section every request goes through `socks5h://127.0.0.1:9050` (`TENGU_TOR_PROXY` overrides) and `route_llm_api` is on; `network = "open"` (e.g. `sandboxes/lping`, `xmarket`, `xlab`) is plain internet, optionally under an `allow_hosts` ceiling. `EgressConfig::resolved()` fills the defaults once; `egress::install` is called by `main` / `load_sandbox_or` / `run-agent` / `mcp-bridge`; children inherit the *resolved* config via `TENGU_EGRESS`. Chokepoints: `tool_client` (http_request, crypto, Hyperliquid, GeckoTerminal, SEC EDGAR, EU TED, Solana RPC), `llm_api_client` (OpenRouter, embeddings, Jev — proxied iff `route_llm_api`), `mcp_client`, `shell_command` (`sandbox-exec` under `isolated`), `guard_shell`, `claude_cli_env` (`HTTPS_PROXY` = HTTP CONNECT on the SOCKS port for the Claude Code CLI), `claude_code_profile` (drops builtin Bash under a proxy), a proxied reqwest 0.11 client for Telegram (`TelegramPipe::build_bot`). `build_tool_executor` takes no HTTP client argument, so nothing can hand in an unproxied one. Operator doc: `docs/egress-2026-09-16.md`; §2.6 has the function table.

### L. Money paths fail closed inside the tool (2026-09-30)

The `[risk]` gate is not a planner or loop step: it runs inside every exec tool (`tools/xm/exec_common.rs::run_exec`), so a feed, a Jev loop, a bridged Claude Code call and `tengu tool call` all hit the same check. Gate + fill + ledger write = one `BEGIN IMMEDIATE`; idempotent per `client_order_id` (the arg, else the call id — never random); a replay asking for another order is refused (`client_order_id_conflict`); only a private agent may hold an exec tool; an entry re-probes the kill-switch file inside the transaction.

### M. Time is a port

`ports/clock.rs::Clock`: `SystemClock` live (feed schedules, paper fill latency), `SimClock` in replay. The backtest gate runs the real `DecisionLoop` with its clock set to each candidate's `decided_at`, so the code that decides live is the code measured on history; the decision cache makes reruns identical (`--offline`).

### N. History first, as-of only (2026-10-01)

The backtest engine reads series only through an as-of view: a bar is observable at its close, funding / ctx rows at their `t_ms`; every trade records `data_asof_ms ≤ decided_at_ms`. `domain/backtest/checks.rs` scales, moves, deletes and cuts everything after a sampled t and asserts nothing at or before t changes — on 15m (the New York and Paris spring-forward switches), 1h, 4h, 1d and split worlds.

### O. One owner per state dir

`tengu run` and `tengu webhooks` take the same leases (`runtime:<sandbox>`, `state:<dir>`); a second process exits 1. Behind them, each ledger account records the sandbox that first wrote it — another sandbox's tools are refused `account_owner_mismatch`.

### P. Sealed before seen, frozen once judged (2026-10-06 / 2026-10-08)

A preregistered variant, experiment or ranking contract is sealed (`tengu lineage seal`: its digest appended to `lineage/locks.toml`) before any outcome; `seal_mismatch` fires if it changes after, and the ranking publisher refuses an unsealed or changed contract. A FROZEN generation (W1) is checked at every `Config::load` of a bound sandbox: a tool or kind outside it, or a pinned section edited (`pin_drift`), fails the load — W2 work goes in new, unbound sandboxes (`xlab-w2`). Evidence is copied once into a read-only vault and pinned by sha256; readers open SQLite `immutable=1`, never through the store adapters that migrate or purge.

### Q. Agents never fetch sources

`source_evidence` only reads `sources.db`; fetching is the operator's (`tengu sources fetch`), gated per row (`enabled`, reviewed `terms_sha256`, the runtime kill switch) before any request. Source text reaches a model fenced as untrusted data (`fence_untrusted`).

**Workflow:**

| To add | Edit | Rust? |
|---|---|---|
| Agent | `[agents.<name>]` block with a `description` in `sandboxes/<name>/config.toml` | no |
| Skill | `skills/<name>/SKILL.md` | no |
| Feed | `[feeds.<n>]` block (`kind = "tool"` or `"tick"`) | no |
| Decision loop | `[decision_loops.<n>]` block | no |
| Strategy (xlab) | `[backtest.strategies.<n>]` or a JSON spec (six kinds) | no |
| Lineage record | one `lineage/<dir>/<id>.toml` (+ `tengu lineage seal` before its outcome) | no |
| Ranking | `lineage/rankings/<id>.toml` + `tengu lineage seal ranking:<id>` + `[strategy_ranking] contracts` (+ a `[feeds.*]` calling `strategy_ranking`) | no |
| Source | a `[sources.registry.<id>]` row (`kind` = `sec_edgar` \| `ted_search`); a new kind is Rust | no |
| Tool | `tools/<name>/mod.rs` + one `ToolEntry` row in `catalog()` (+ `WORKSPACE_TOOLS` if opt-in) + conformance case + matrix set | yes |

---

## §5 — Where to start when you open the codebase tomorrow

Trace ONE turn end-to-end before changing anything:

```
cargo run --release --features postgres_memory,claude_code -- chat --sandbox lping
> what is the current BTC price in USD?
```

(`lping` is `[egress] network = "open"`; for a Tor sandbox run `make tor` first.)

Follow the call chain:

1. `adapters/inbound/tui/mod.rs` — captures the input, dispatches to the engine thread
2. `bootstrap::orchestrator::build_orchestrator` — constructs `Orchestrator`
3. `application/orchestrator/mod.rs::Orchestrator::handle` — calls `replan::drive` → `RagPlanner::plan`
4. `application/orchestrator/planner.rs::RagPlanner::plan` — load planner registry file, LLM call, parse verdict
5. `application/orchestrator/replan.rs::drive` — runs the plan, handles retry/replan
6. `adapters/outbound/subprocess_runner.rs::SubprocessRunner::run_step` — spawns child, pipes IPC
7. `adapters/inbound/cli/run_agent.rs::run_agent_subprocess` — child entry: load sandbox config → `egress::install` → `[agents.<name>]`, build tools, multi-turn loop
8. `adapters/outbound/tools/http/request.rs` — http_request hits CoinGecko through `egress::policy().tool_client`
9. `adapters/inbound/cli/run_agent.rs::try_persist_agentic_step_summary` — child intercepts `compress_and_store`, writes the durable step summary
10. Back to step 6 (child exits) → step 5 (drive() returns) → step 1 (TUI renders the reply)

The other flows and views:

| Flow | Try | Then read |
|---|---|---|
| `tengu ranking run` (no LLM) | `tengu ranking show --sandbox xlab-w2 --contract rank.xlab-w2.daily.v1` (a published ranking) · the same with `run` (one date; needs a filled `market.db`) | `cli/ranking.rs` → `application/ranking/mod.rs` → `domain/backtest/ranking.rs` |
| `tengu lineage` / `tengu evidence` (no config, no LLM) | `tengu lineage verify --pins` · `tengu lineage trace fwd.rule_w.2026-10-02` · `tengu evidence verify lineage/evidence/w1-2026-10-06.toml` | `cli/lineage.rs` → `application/lineage/verify.rs` → `domain/lineage/registry.rs` |
| `tengu backtest` (no LLM, no key) | `tengu backtest --sandbox xlab --strategy weekend_fade` (needs a filled `~/.tengu/state/xlab/market.db`: `docs/xlab-2026-10-01.md` § 10) | `cli/backtest.rs` → `application/backtest/mod.rs` → `domain/backtest/engine.rs` → the run dir |
| `tengu run` | `tengu doctor --sandbox xmarket` then `tengu run --sandbox xmarket` (runbook at the top of `sandboxes/xmarket/config.toml`); never start another binary on a state dir a weekend run holds (`docs/SESSION_HANDOFF.md`) | `inbound/run.rs` → `bootstrap/runtime.rs` → `application/runtime/feeds.rs` → `tools/xm/exec_common.rs` |

That's the whole story. Every other doc in `docs/` zooms into one of those layers.

---

*Written 2026-04-27; last synced 2026-10-08 against `main` at bc9bc12a038654ac4941c714abcfa777c99af631 (added §1b C ranking flow, §2.14–2.17: lineage + evidence, strategy ranking, source evidence, SOE; mechanics P, Q; SEC events, labels, execution maps, `xlab-w2` / `soe` sandboxes; corrected: skill directory order, executor abort + parallel leaves, child log forwarding, tool count 47, `tengu prune` scope). Before that 2026-10-02 against `feature/xmarket` at 699bb83e79e3dac2f76ca9cbef69b3573b31252d (added §1b other flows, §2.7–2.13: `tengu run`, decision loops + replay, typed observations + recorder, tool families, xmarket risk / paper, xlab, engine parity; corrected: session-id resolution, plan-schema validation, metrics kinds and layers, bridge env, `AgentCompose` path). Companion files: `architecture-2026-04-27.svg` (the picture), `architecture-2026-04-27.html` (the explorer), `code-map.md` (every file), `SESSION_HANDOFF.md` (the running state log), `comparison-2026-04-26.md` (vs Hermes / PI).*
