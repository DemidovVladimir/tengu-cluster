# Code findings — 2026-10-07

Found while building the visual tutorial (`docs/tutorial/`) on `main` @ `9f0e98f`.
Each row was read in the code (file:line), not run. **Fix pass, same day:** the Kind column now also says what happened. Every `bug` and `stale` row is fixed in this checkout (each bug with a unit test, except where the row says why not) unless it reads `not a bug` or `decision`; `gap` rows stay open.

| Status | Means |
|---|---|
| fixed | changed in code (+ test) or in the doc |
| not a bug | the code is right; the row was wrong (reason given) |
| decision | needs the operator (options given) |
| open | `gap`: not built; untouched |

| Kind | Means |
|---|---|
| bug | behaviour a user would call wrong |
| gap | documented / configured feature with no code behind it |
| stale | a doc or comment that says something the code does not do |

## Orchestration (planner, subagents)

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | `[agents.<a>.identity] instructions` never reach a plan step's prompt (base + skills + plan + suffix only) | `cli/run_agent.rs:230-258` |
| bug · fixed | a plan with 2+ leaves runs every step, then replans with "plan has no single leaf"; `skills/orchestrator/SKILL.md` invites parallel steps without a join — leaves are joined now; a step that never starts (unknown `depends_on`, cycle) replans instead of an empty reply | `orchestrator/executor.rs:111-119` |
| bug · fixed | a replan does not abort in-flight sibling steps | `orchestrator/executor.rs:62`, `:87` |
| bug · fixed | `tengu chat`: `[orchestrator] agent` other than the default agent → "snapshot missing" | `tui/mod.rs:896`, `bootstrap/orchestrator.rs:134` |
| bug · fixed | child stderr is inherited: a "non-zero status" error carries an empty stderr; child logs may print over the TUI (not run) | `subprocess_runner.rs:216`, `:241-247` |
| gap · open | `SKILL.md` promises `plan_schema.json` validation + 3 retries; nothing loads the schema, nothing retries, `Plan::validate` is dead | `domain/plan.rs:69-104` |
| gap · open | `AgentIpcInput.tools` / `.skills` are never read by the child | `subprocess_runner.rs:34-37` |
| stale · fixed | `config.example.toml`: orchestrator `engine` "REQUIRED, other values disable" — defaults to `rag`, other values fail the load | `config/mod.rs:671`, `:1315` |
| stale · fixed | "largest balanced `{...}`" — the parser returns the first | `planner.rs:42` vs `:121-127` |
| stale · fixed | `session_recent_n` "reloaded from storage" — an in-memory ring per planner | `planner.rs:291-303` |
| stale · fixed | "hitting max_tool_rounds = Failed" — Ok when the model wrote any text | `run_agent.rs:73` vs `:662` |
| stale · fixed | unknown-agent fast-fail "avoids 3 retries" — skips the spawns only; attempts and waits still run | `subprocess_runner.rs:270-275` |

## Chat turn, engines, MCP bridge

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | flow compaction never fires on defaults: kept turns (24/32/40/60) always exceed the history limit (20/25/30/40) applied first | `chat/flow.rs:55-58`, `:148-151`, `service.rs:316-318` |
| bug · fixed | a chat (TUI / Telegram) `claude_code` agent without `workspace` gets no bridge, so no Tengu tools | `engines/claude_code.rs:966`, `service.rs:419` |
| gap · open | `/context` "Last prompt assembly" is never set | `service.rs:119`, `:277` |
| gap · open | `[claude_code] timeout_secs` is ignored for agents (idle timeout = `limits.stream_event_timeout_secs`) | `engines/mod.rs:136` |
| stale · fixed | `config.example.toml:86` `compaction_summary_max_tokens = 320` — computed default 4096 | `config.example.toml:86` |
| stale · fixed | `MAX_MCP_RESULT_CHARS` comment cites removed OpenRouter "2-phase pruning" | `mcp_bridge.rs:625-629` |
| stale · fixed | "Errors bypass SanitizedToolExecutor" — it redacts errors now (twice, harmless) | `mcp_bridge.rs:602`, `cli/tool.rs` |

## Config, tools, channels

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | Telegram cannot reach agent ids containing `-`: the parser maps `-` → `_`, routes are keyed verbatim | `inbound/channel.rs:33`, `:44`, `telegram.rs:2113` |
| bug · fixed | Telegram: one orchestrator + one session id per process — every sender shares planner session and recall | `telegram.rs:794` |
| bug · fixed | the base config is loaded and validated even with `--sandbox`: a broken `~/.tengu/config.toml` breaks every sandbox command | `cli/mod.rs:496` |
| bug · fixed | a misspelled name in `tools` is silently ignored (only `workspace_tools` is validated) — a load warning now, not an error: a shell skill's tool is a legal name | `bootstrap/tools.rs` |
| bug · fixed | `persistent_store` is advertised without a memory backend; its plugin is not built, so a call fails | `tools/mod.rs` |
| gap · open | `/agents` advertises `/team <goal>` — no handler; the `telegram.rs` header still describes inline approvals and `/team` | `telegram.rs:1766` |
| gap · open | `[scaffold]` is applied only by `tengu telegram` | `telegram.rs:579` |
| gap · open | plan-step tool calls show no activity line on any surface (child uses a no-op adapter) | `cli/run_agent.rs` |
| gap · open | an unset `${VAR}` stays literal in the TOML, no warning | `config/mod.rs:1550` |
| stale · fixed | CLAUDE.md: scope check on the "first line" of `execute` — the lint allows 30 lines | `tests/scope_lint.rs:86` |
| stale · fixed | `WebhookEndpointConfig` doc "Payload (JSON):" — code sends "Payload (raw body, may be JSON):" | `webhooks.rs:425`, `config/mod.rs:795` |
| stale · fixed | `resolve_tool_scopes` doc: no-shell fallback for "a `[solana]` signing sandbox" — applies to every hardened sandbox | `bootstrap/tools.rs:221-224` |
| stale · fixed | CLAUDE.md lists `sandboxes/aura` (open network, scopes example) — gone on `main`; `storage-test` has no `[egress]`, so it runs Tor | `CLAUDE.md`, `AGENTS.md` |
| not a bug | CLAUDE.md on `main` does not say this; `docs/code-map.md` already says `cli/mod.rs::run` | `cli/mod.rs:350` |

## Skills and skill lifecycle

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | `tengu eval` cannot read the `prompts.yaml` that `skill_distill` / `tengu skill seed` / `manage_skill create` write (`{schema_version, fixtures}` vs a list); `skills/german-teacher` fails | `inbound/eval.rs:409` |
| bug · fixed | `tool_assertion` always fails in `tengu eval` (no tool registry passed) | `eval.rs:1690` |
| bug · fixed | the `script` metric gets `PROMPT`, `TRANSCRIPT`, `EXPECTED_OUTCOME` as empty strings | `metric_kinds/script.rs:62` |
| bug · fixed | evolve `pick_best` protects only metrics gated at baseline — a passing metric may regress; the best cycle need not beat baseline (gate heading now "Other metrics") | `lifecycle/evolve.rs:111` |
| bug · fixed | `manage_skill` can write the managed tier (`~/.tengu/skills`) after only a workspace write check; `skill_distill` refuses it | `manage_skill/mod.rs:195`, `:956` |
| bug · fixed | which skill body is dropped over the token budget depends on HashMap order | `skills/registry.rs:705`, `:893` |
| bug · fixed | `view_skill` / `apply_improver_proposal` resolve tiers from the cwd, not the workspace | `view_skill/mod.rs:145`, `:239` |
| gap · open | `requires_bins`, `requires_env`, `os` (skill-creator docs) — no code reads them | `skills/skill-creator/SKILL.md` |
| gap · open | no compact skill catalog + on-demand read: doc / API bodies go whole into the prompt (only `resources/` on demand) | `skills/registry.rs` |
| gap · open | `[skill_lifecycle] default_rolling_window` is parsed, never read (window hard-coded 10) | `eval.rs:1373` |
| gap · open | `--concurrency` > 1 is refused | `eval.rs:1319` |
| bug · fixed | three different skill tier orders: registry, a plan step's body, the planner registry | `registry.rs:1027`, `run_agent.rs:748`, `shared_files.rs:298` |

## Memory and metrics

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | `agentic_memory` `capture` never computed a vector, so vector recall never found captures — it embeds now (fail-soft; compile-checked with `postgres_memory`, no live Postgres here) | `agentic_memory/mod.rs:310-319` |
| bug · fixed | planner embedder follows `[memory] embedding_model`, the tool's is pinned to `text-embedding-3-small`; a non-1536 model silently turns planner recall text-only — step summaries and webhook output pinned too | `agentic_memory/mod.rs`, `bootstrap/memory.rs` |
| bug · fixed | workspace recall is skipped on a plain substring match ("last", "latest", …): "Elasticsearch" skips recall too | `chat/service.rs:252-265` |
| bug · fixed | `persistent_store` `store` reads an absolute `file_path` with no `check_fs_read` | `tools/memory/persistent_store.rs:242`, `:553` |
| bug · fixed | soft `tengu prune` removes `<ws>/storage`; files live in `<ws>/.tengu/storage/`; a log line cites a nonexistent `tengu memory purge` | `prune.rs:121`, `persistent_store.rs:517` |
| bug · fixed | two processes on one `vectors.bin`: last writer wins | `memory/disk_vector.rs` |
| bug · fixed | only Telegram gave the planner a real memory manager — so only there were planner turns (the whole assembled planner prompt as the "question") written into workspace memory. Planner turns are no longer written there on any surface; cross-plan recall reads Postgres | `telegram.rs:781` |
| bug · fixed | the embedder hard-codes `https://openrouter.ai/api/v1/embeddings`, ignoring `OPENROUTER_BASE_URL` | `memory/embedder.rs` |
| bug · fixed | each subagent metrics record is logged twice (child stderr + parent re-emit) | `subprocess_runner.rs:341-362` |
| gap · open | the `AGENTS.md` / `MEMORY.md` / `USER.md` / daily-log loader runs only in tests; no daily-log writer, pre-compaction flush, identifier preservation or temporal decay exists; the prompt loads `IDENTITY.md`, `PROFILE.md`, `CONTEXT.md` only | `memory/builtin.rs:100`, `skills/registry.rs:1119` |
| gap · open | the `<memory-context>` fence has no live caller | `application/memory/mod.rs` |
| gap · open | `[memory] max_recall_tokens` (600) is never read | `config/mod.rs` |
| gap · open | the metrics sink exists only with an `[orchestrator]` (lping): `TENGU_TUI_METRICS` does nothing in xlab / xmarket / jev-exec | `bootstrap/orchestrator.rs:367-389` |
| stale · fixed | `domain/metrics.rs` lists layer `prior_plan` (never emitted); the planner emits `session_recall`, `failure` | `metrics.rs:117-124` |

## Secrets and ops

| Kind | Finding | Where |
|---|---|---|
| bug · fixed | with a vault, `tengu secret set/remove/list/change-password` ask for the master password twice unless `TENGU_MASTER_PASSWORD` is set | `cli/mod.rs:410` |
| bug · fixed | the Docker image has no Claude CLI: a `claude_code` build passed `doctor`, its agents failed at run time — `doctor` now fails when a `claude_code` agent's CLI is missing (the image still ships without it) | `Dockerfile` |
| bug · fixed | `Makefile` derives `NETWORK=open` from `network = "open"` in any section, not only `[egress]` | `Makefile:25` |
| gap · open | soft prune clears `[scaffold.project]` dirs, which no sandbox sets | `cli/mod.rs:639` |
| stale · fixed | `prune --hard` help: "root runtime artifacts" — it empties every child of each workspace root | `prune.rs:84-100` |

## Trading desk (Solana, paper) and research

| Kind | Finding | Where |
|---|---|---|
| not a bug | `[risk] min_lifecycle` maps to no rule on purpose: no catalog fills an instrument's lifecycle yet (it is `Absent` for every order, `application/paper.rs:230`), so enforcing it would refuse every order — turned on by `kg-lifecycle` | `config/risk.rs:275` |
| bug · decision | the gate waives `book_age` for a degraded exit, but `simulate_fill` rejects a stale book (`stale_book`), so `allow_reduce_degraded` cannot help such an exit. Options: (a) the paper fill accepts a stale book for a waived reduce-only exit (fills at stale prices), or (b) the gate stops waiving `book_age` (only `kill_switch`, `halted`, `ctx_age`) — the verdict then matches the fill | `domain/xm/paper.rs:551` |
| not a bug | `MinTradeNtl` checks size × IOC bound as Hyperliquid checks order value at the limit price: a $10-at-mid sell bounded below mid is refused by HL too; size a little over $10 (a reduce-only close of the whole position is exempt) | `domain/xm/risk.rs` |
| stale · fixed | `marketdata_stats.rs:15`: no-trade hours are gaps — HL keeps them as flat `n = 0` bars (`backtest/engine.rs:22`) | `domain/marketdata_stats.rs:15` |
| stale · fixed | `sandboxes/xlab` `xl_gate` sets `history = 1`; replay forces 0 | `bootstrap/decision.rs:217` |
| stale · fixed | `tools/solana/defs.rs:2`, `:244` and CLAUDE.md say "ten" Solana read tools — 11 since `lp_swap_plan` | `tools/solana/defs.rs` |
| stale · fixed | `write_swap.rs` header: `jupiter_swap`'s oracle row ≤ 10 s old — `plan::oracle_usd` accepts 30 s (`ORACLE_MAX_AGE_MS`) | `outbound/solana/plan.rs:52` |
