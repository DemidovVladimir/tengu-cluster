# Decision loop — plan (2026-09-24)

Jev-driven control loop for `sandboxes/lping`. Context + probes: `docs/lping-2026-09-24.md`. Typed tools, `world`, cache: `docs/typed-observations-2026-09-24.md`.

## Status (2026-09-24, merged: phases 1–6b #18, A #27, B #29)

| Phase | State | Verified |
|---|---|---|
| 1 Domain + Jev client | ✅ | unit tests on the probed wire shape; `#[ignore]` live test |
| 2 Loop use case | ✅ | fake-engine tests: history slots, caps, dry-run, escalate, illegal choice |
| 3 Config + `tengu decide` | ✅ | live: `fetch_pools` ran → Jev picked pool `HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR` (highest APR, TVL > $100k), 2 SOL → `dry_run`; ~$0.00014/decision |
| 4 Helius trigger | ✅ `auth_header_env` + endpoint `loop` | live listener: wrong header → 401; Helius-shaped POST → 202 → loop → escalated at 0.79 → planner turn started |
| 5 gRPC feed | ⏳ | — (seam ready: a stream writes `acct/1:<pubkey>` rows) |
| 6a Typed observe / plan tools + observation cache | ✅ | 11 Solana tools, `world` / `requires` / `FromObservation`, `obs` meta in history; `lp_watch` + `hedge_watch` use them (`sandboxes/lping`) |
| 6b Write tools + signing | ✅ built, send not exercised live | 5 write tools, simulate by default (`docs/typed-observations-2026-09-24.md` § Write tools); live keyless mainnet simulations of swap / open / close / perps order; `lp_watch.open_position` uses the real schema (`mode = "simulate"`); loops run as `lp_executor` (no `description`) |
| A Bindings + execution chains (2026-10-06, `feature/jev-chains`) | ✅ simulate, live | slots bind one value (`{event = "/x"}`, `{from, path}`, `{observation, path}`) — no question to Jev, unresolved ⇒ action illegal; a dry-run write no longer ends the event; `hedge_decide` `data.order` = `jup_perps_order` args. Live (Jev + mainnet, keyless): `hedge_exec` snapshot → decide_hedge → order (simulated, 1 tx) → hold; `lp_exec` snapshot → plan_swap → swap → refresh, and snapshot → plan_swap → open (simulated, 1 tx) → hold |
| B Execution maps + `sequence` (2026-10-06, `feature/execution-map`) | ✅ simulate, live | `sequence = ["a", "b?", …]` in a loop (TOML or map): each step offers only the next action + terminals; optional steps that cannot run are skipped; a failed / refused step halts. `tengu decide --map <file\|->`: an Architect's JSON run of one loop that can only narrow it (`config/execution_map.rs`, skill `execution-map`); refused maps list every reason; audit `trigger = "map:<sha256>"`, map kept in `<TENGU_HOME>/logs/maps/`. Live: lp_exec map snapshot → plan_swap → open → hold (twice), swap route → swap → refresh (gate) → Jev declined open (simulated balances unchanged); hedge_exec map snapshot → decide_hedge → hold (no order) and order simulated with forced knobs |
| Runtime + trace (2026-09-30 `tengu run`; 2026-10-08 #44) | ✅ | `tengu run` builds every loop once behind `LoopDispatch` (tick feeds `kind = "tick"`, webhook loop endpoints, Studio's send-event); each step writes trace events (`observation.read`, `jev.*`, `action.*`, `tool.*`) into the run's recording; reference sandbox `control-loop-lab` (`docs/runtime-2026-09-30.md` § Trace, `docs/control-loop-lab-2026-10-08.md`) |

Deviations from the sketch below: history is in-process (not `shared_cache`); a dry-run loop may name write tools not built yet; `tengu decide` never escalates (no orchestrator) — `tengu webhooks` and `tengu run` (a `webhooks` build with `[orchestrator]`) do. 6b: signer = local key file (user decision 2026-09-29); no Telegram approval gate (`TelegramConfig.tool_approvals` is not wired) — the gates are the per-agent wallet grant, `mode` default simulate and the loop's `dry_run`; no devnet run — verified by keyless mainnet simulation instead; `dlmm_claim_fees` dropped (close claims).

### Jev as an architect's hands — `sandboxes/jev-exec` (2026-09-29, experiment)

| Piece | How |
|---|---|
| Architect | in-process `claude_code` agent (subscription, `claude-opus-5-5`), builtins `none`; every tengu tool scope-denied except `run_command` → `tengu` |
| Hand-off | `run_command`: `tengu -c <sandbox config> decide --loop executor --event - <<'EOF' {"task": "..."} EOF` — one task per call |
| Executor | `[decision_loops.executor]`: `crypto_price` (Coinbase), `fx_rate` (Frankfurter), `list_workspace`, `done`; `escalate = false`, `act_at = 0.7` |
| Result back | `tengu decide` now prints `history` (args + reduced result per step) next to `outcomes` |
| Verified | "1 BTC in EUR?" → architect sent 2 tasks in parallel → Jev `crypto_price(BTC-USD)` / `fx_rate(EUR)` at confidence 1.0, then `done`; ~0.3–0.6 s per decision. Architect's own `http_request` → `host 'api.coinbase.com' not in allowed net_hosts []` |
| Limits | Jev only chooses — args are enumerated slots or bindings (`{ event = "/x" }` carries a value the task sets, 2026-10-06; no free text); `run_command` scope checks the first command word only (leading `NAME=value` skipped, 2026-10-01). Fixed since: `tengu decide` logs to stderr, stdout is the JSON alone (`cli/mod.rs::stdout_is_data`); the Claude CLI runs `--strict-mcp-config` (the tengu bridge is its only MCP server) and, with `builtin_tools_profile = "none"`, `--setting-sources ""` (no user plugins, hooks or settings) — `engines/claude_code.rs::cli_args` |

## Shape

```
trigger (tengu run tick feed | webhook | tengu decide [--map] | Studio event | gRPC later)
        ──► world = observation-store rows (never fetched)
        ──► DecisionState {goal, world, history[N]}      (application/decision_loop)
        ──► Jev: next_action + one choice per arg slot   (ports/decision.rs → outbound/decisions.rs)
        ──► threshold ── ≥ act_at   → ToolExecutor::execute_typed (existing tools + scopes)
                      ── < act_at   → escalate: orchestrator one-shot (planner → crypto_researcher)
                      ── skip/noop  → log only
        ──► reduce(result) → history (ring buffer) + audit JSONL + MetricsRecord + trace events
```

Key decision: **Jev picks, existing tools execute.** No new tool runtime — actions are rows mapping to catalog tools, so scopes, egress and `scope_lint` apply unchanged.

## Config (behaviour in TOML, doctrine #2)

```toml
# Shape of sandboxes/lping (the file is the source of truth).
[decision_loops.lp_watch]
goal    = "Pick the best SOL/USDC DLMM pool for a new position; max 2 SOL"
agent   = "lp_executor"                      # tools + scopes + workspace (+ its observations.db); private (no description)
dry_run = true                               # write actions logged, never run
act_at  = 0.8                                # min confidence; below → escalate
history = 8
world   = { price = "price_oracle/1:So11111111111111111111111111111111111111112" }
world_max_age_secs = 30

[decision_loops.lp_watch.actions.refresh_price]
description = "Refresh the SOL/USD oracle price"
tool = "sol_price"                           # typed: history gets features + obs meta
read_only = true

[decision_loops.lp_watch.actions.fetch_pools]
description = "Fetch Meteora DLMM SOL/USDC pools"
tool = "dlmm_pools"
read_only = true
args = { query = "SOL-USDC", min_tvl_usd = 100000 }
reduce = { pools = "/data/pools/*/{address,name,tvl_usd,fee_tvl_24h_pct,bin_step}" }

[decision_loops.lp_watch.actions.open_position]
description = "Open a DLMM LP position in one of the fetched pools"
tool = "dlmm_open_position"                  # 6b write tool; dry_run: logged only (mode stays "simulate" even with dry_run = false)
requires = { price = 30 }                    # offered only while `price` is usable and ≤ 30 s old
slots = { pool = { from = "fetch_pools", items = "/pools/*", value = "address", top = 5 }, size_sol = [0.5, 1, 2, 3] }
caps  = { size_sol = 2.0 }
```

| Concept | Rule |
|---|---|
| Slot candidates | static list, `{from, items, value, top}` over the latest `from` action's reduced result, `{observation, items, value, top}` over a fresh `world` entry, or `{event = "<list path>", value, top}` over the event; code labels them `pool_1..n`, maps back to full values |
| Bindings (2026-10-06) | `{from, path}`, `{observation, path}`, `{event = "<path>"}`: ONE value, one candidate, filled without a question. Nothing there (missing, `null`, `[]`, a failed `Field`) ⇒ no candidate ⇒ the action is illegal — a chain advances only on real values. Caps apply. Bind exact amounts from `/data/...` (features are rounded to 6 significant digits). Event values are untrusted: caps, the tool's own checks and the agent's scopes (wallet grants) still hold |
| Reducer | JSON-pointer projection per action (no Rust per tool). Typed tools: default = `decision_value` (`{status, age_s, slot, source, features}`); a reducer addresses `decision_root` (`/data/...`, `/features/...`) |
| `world` | alias → observation key; read from the store every step, never fetched; stale / missing / error entries carry no numbers |
| `requires` | alias → max age secs; the action is offered only while each alias is usable and that fresh |
| `ok` | typed: `status != error`; text: HTTP status / parse (`reduce::parse_tool_output`) |
| Executor | loop agent's tools wrapped in `SanitizedToolExecutor` (process `SecretRegistry`) — text and observations redacted before history, audit, Jev |
| Tool-call id | `{loop}:{session_id}:{t}` → the tool's `ToolCtx.call_id`; never repeats across events or restarts (one session id per event) — the idempotency key of exec tools (`client_order_id` arg, else `call_id`) |
| Caps | re-checked in code after Jev answers; violation = skip + audit |
| `dry_run` | non-`read_only` actions are logged (history `result = "dry_run"`), not executed; the event goes on (2026-10-06) — a slot bound to the dry-run step has no candidate. To exercise writes keylessly: `dry_run = false` + `mode = "simulate"` literal in the args (lping `hedge_exec` / `lp_exec`) |
| `sequence` (2026-10-06) | `["action", "action?"]`; each step offers Jev the next action + the terminal actions only; `?` = skipped when not legal; a required step that is not legal, or a failed (`ok != true`) / refused step, halts the chain. `max_steps` > sequence length. A slot the args never reference is a pure gate (lping `refresh`: `swapped = {from = "swap", path = "/status"}` — legal only after a successful swap) |
| Execution map (2026-10-06) | JSON `{loop, event, goal, sequence, actions, caps, max_steps, act_at, dry_run}` — narrows only (subset of actions, tighter caps, lower `max_steps`, higher `act_at`, dry-run on, goal appended); tools / args / slot sources / `world` / agent / model untouchable; the result passes `validation_errors`; `tengu decide --map`. Hand-off from an architect today: `run_command` → `tengu decide --map -` (non-hardened sandboxes only — a signer sandbox refuses shell scopes; a private map tool is the next step) |
| Escalation | below `act_at` with `escalate = true` (default) and an `Escalator` (`webhooks::orchestrator_escalator`, only with `[orchestrator]` in `tengu webhooks` / `tengu run`): one `run_one_shot` turn, message = the proposed action, args, confidence + the state JSON, session = the event's (`webhook-<endpoint>-<uuid>`, `<feed>:<slot ms>`); `escalate = false` or no escalator ⇒ the step stops (audit `escalated`) |
| Jev call failures (`outbound/decisions.rs`, 2026-09-30) | HTTP 429 / 5xx / connect error ⇒ one retry after 0.5–1 s (`Retry-After` ≤ 5 s honoured, longer ⇒ no retry; `domain/backoff.rs::next_delay`); timeout / other 4xx / unparseable ⇒ no retry; 3 consecutive failed calls open a 30 s circuit (fail fast, no request), then calls pass again |
| Clock + replay (2026-10-01, xlab gate arm — `docs/xlab-2026-10-01.md` § 7) | `DecisionLoop::with_clock`: `world` / typed-result ages, audit `ts` / `ts_ms` and metrics time from a `Clock` (none = wall; `latency_ms` stays real). `bootstrap::decision::build_replay_loop`: terminal actions only (a tool action or `world` is refused), no tools / store / escalator (`escalate = false`), a `SimClock` set to each decision instant, audit to the run's `decisions.jsonl` (`trigger = "backtest"`); `decide_terminal` → `Verdict` (action, confidence, p per action, below `act_at`, outcome); Jev via `CachedDecisionEngine` (`<state dir>/backtests/decision-cache.db`, key = sha256 of the canonical request; offline = a miss fails); replay loops keep no history (`history = 0`: the request is the event alone) |
| Gate arm (2026-10-01, xlab — `docs/xlab-2026-10-01.md` § 7) | `application/backtest/gate.rs::run_gate`: one `decide_terminal` per rule candidate (the first `--max-decisions` by seq, the rest counted as cut), K workers each with its own replay loop + `SimClock` on one queue (`bootstrap::decision::build_gate`), results by seq and identical for any K; verdict → `take` / `skip` / `ask_architect` / `unsure` / `rejected` / `error` (`domain/backtest/gate.rs`); `gate_arms`: rules vs jev arms, calibration, paired diff |

## Phases (one branch, `feature/decision-loop`)

| # | Deliverable | Files | Verify |
|---|---|---|---|
| 1 | Domain types + Jev client | `domain/decision.rs` (State, HistoryEntry, Question, Answer), `ports/decision.rs` (`DecisionEngine`), `outbound/decisions.rs` (via `egress::llm_api_client`) | unit tests on recorded JSON; `#[ignore]` live probe |
| 2 | Loop use case | `application/decision_loop/{mod,slots,reduce,world}.rs` — state build, ring buffer, slot resolution, threshold, dry-run, execute via the `ToolExecutor` port | unit tests with a fake `DecisionEngine` + fake tool |
| 3 | Config + CLI | `DecisionLoopConfig` in `config/decision_loop.rs` (+ validation: tool exists, slots non-empty, 0<act_at≤1); `tengu decide --sandbox <n> [--loop <name>]` in `inbound/cli/`; wiring in `bootstrap/decision.rs` | `tengu decide --sandbox lping` ticks, logs decisions, dry-run |
| 4 | Push triggers | webhook `auth_header_env` mode (Helius); endpoint `loop = "<name>"` feeds the loop instead of the planner | signed + header-auth tests |
| 5 | gRPC feed | Yellowstone client behind `solana_stream` feature (tonic); `egress` gains `grpc_channel` (open network only at first) | subscribe to Meteora DLMM program, events → loop |
| 6a | Typed read tools ✅ | observation envelope + cache (`domain/observation.rs`, `ports/observation.rs`, `outbound/observations.rs`, `application/observe.rs`), `world.rs`, `tools/solana/*`, `domain/lp/*` | `tengu decide --sandbox lping --loop lp_watch` twice < 60 s → second read `obs.source = cache` |
| 6b ✅ | Write tools + signing | `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open/close_position`, `jup_perps_order`; `SolanaSigner` port (local key file); `WriteResult`, wallet lease + pending record + write fence, simulate by default | goldens + fake cluster + live keyless simulations; first `send` on a dedicated wallet only with the operator's go |

Phases 1–3 are the usable core (polling loop, dry-run). 4–6 build on it.

## Observability

| Surface | What |
|---|---|
| `MetricsKind::Decision` | new variant; tokens + cost + latency per Jev call |
| `<TENGU_HOME>/logs/decisions.jsonl` | one line per decisions call, written with one `write_all` (concurrent loops / processes never interleave): `ts` (s) + `ts_ms`, `loop`, `sandbox`, `session_id`, `t`, `call_id` (`{loop}:{session_id}:{t}` when the step ran a tool, else null — the key an exec tool's risk verdict carries: `ledger.db` `risk_decisions`, `logs/risk.jsonl`), `decision_id`, `model`, `act_at`, `latency_ms`, answers + probabilities, `usage`, `result` (action taken / skipped / escalated / rejected / `refused` + `rule` when a typed result carries `features.risk = "deny"` — `StepOutcome::Refused`, the loop goes on), `args`, `ok` + `output` (the history value: reduced, redacted); typed results carry `obs` (`key`, `status`, `source` live \| cache, `age_s`, `slot`); `trigger` (`map:<sha256>`, `backtest`), `runtime_id` + `run_id` when the process records a trace (`tengu run`, `tengu decide`; not `tengu webhooks`). A failed call (timeout, 402, 5xx) writes `result = {outcome: "error", reason}` with null answers, then the error propagates |
| Execution trace (2026-10-08) | `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`: per step `observation.read` per `world` alias, `jev.completed` / `jev.failed`, `action.selected` / `.completed` / `.refused` / `.escalated` / `.rejected`, `tool.started` / `.completed` / `.failed` (`TracedExecutor`); read with `tengu trace` or Tengu Studio — `docs/runtime-2026-09-30.md` § Trace |
| TUI decision feed | `tengu chat` on a config with `[decision_loops]` tails the audit (300 ms) and shows each decision of those loops, from any process, as a System bubble — `decision_loop::render_audit` (+ `call <id>` for a step that ran a tool); a failed call as `jev <loop> #<t> · decide failed → error: <reason>`, a risk refusal as `… → refused by the risk gate: <rule>` |
| `agentic_memory` | executed actions + escalations (durable, recallable by planner) |

## Docs to update when landing

Done for phases 1–4, 6a and 6b. Phase 5: `docs/egress-2026-09-16.md` (`grpc_channel`), `docs/typed-observations-2026-09-24.md` (stream rows).

## Open questions

| # | Question | Default if unanswered |
|---|---|---|
| 1 | v1 trigger: polling tick or Helius webhook? | polling (no provider needed) |
| 2 | Jev as a chat `Engine` instead (tool `enum` params → slots)? | no — separate port; chat history ≠ normalised state |
| 3 | ~~Signing: Privy Solana wallets or local keypair?~~ | decided 2026-09-29: local key file (`[solana] signer_key_file`); Privy can be a second `SolanaSigner` impl later |
