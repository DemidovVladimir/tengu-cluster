# Decision loop — plan (2026-09-24)

Jev-driven control loop for `sandboxes/lping`. Context + probes: `docs/lping-2026-09-24.md`. Typed tools, `world`, cache: `docs/typed-observations-2026-09-24.md`.

## Status (2026-09-24, branch `feature/decision-loop`)

| Phase | State | Verified |
|---|---|---|
| 1 Domain + Jev client | ✅ | unit tests on the probed wire shape; `#[ignore]` live test |
| 2 Loop use case | ✅ | fake-engine tests: history slots, caps, dry-run, escalate, illegal choice |
| 3 Config + `tengu decide` | ✅ | live: `fetch_pools` ran → Jev picked pool `HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR` (highest APR, TVL > $100k), 2 SOL → `dry_run`; ~$0.00014/decision |
| 4 Helius trigger | ✅ `auth_header_env` + endpoint `loop` | live listener: wrong header → 401; Helius-shaped POST → 202 → loop → escalated at 0.79 → planner turn started |
| 5 gRPC feed | ⏳ | — (seam ready: a stream writes `acct/1:<pubkey>` rows) |
| 6a Typed read tools + observation cache | ✅ | 10 Solana tools, `world` / `requires` / `FromObservation`, `obs` meta in history; `lp_watch` + `hedge_watch` use them (`sandboxes/lping`) |
| 6b Write tools + signing | ⏳ | `open_position` names `dlmm_open_position` (allowed only while `dry_run`) |

Deviations from the sketch below: history is in-process (not `shared_cache`); a dry-run loop may name write tools not built yet; `tengu decide` never escalates (no orchestrator) — the webhook listener does.

## Shape

```
trigger (tick | webhook | gRPC)  ──► world = observation-store rows (never fetched)
        ──► DecisionState {goal, world, history[N]}      (application/decision_loop)
        ──► Jev: next_action + one choice per arg slot   (ports/decision.rs → outbound/decisions.rs)
        ──► threshold ── ≥ act_at   → ToolExecutor::execute_typed (existing tools + scopes)
                      ── < act_at   → escalate: orchestrator one-shot (planner → crypto_researcher)
                      ── skip/noop  → log only
        ──► reduce(result) → history (ring buffer) + audit JSONL + MetricsRecord
```

Key decision: **Jev picks, existing tools execute.** No new tool runtime — actions are rows mapping to catalog tools, so scopes, egress and `scope_lint` apply unchanged.

## Config (behaviour in TOML, doctrine #2)

```toml
# Shape of sandboxes/lping (the file is the source of truth).
[decision_loops.lp_watch]
goal    = "Pick the best SOL/USDC DLMM pool for a new position; max 2 SOL"
agent   = "crypto_researcher"                # tools + scopes + workspace (+ its observations.db)
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
tool = "dlmm_open_position"                  # phase 6b — not built; dry_run only
requires = { price = 30 }                    # offered only while `price` is usable and ≤ 30 s old
slots = { pool = { from = "fetch_pools", items = "/pools/*", value = "address", top = 5 }, size_sol = [0.5, 1, 2, 3] }
caps  = { size_sol = 2.0 }
```

| Concept | Rule |
|---|---|
| Slot candidates | static list, `{from, items, value, top}` over the latest `from` action's reduced result, or `{observation, items, value, top}` over a fresh `world` entry; code labels them `pool_1..n`, maps back to full values |
| Reducer | JSON-pointer projection per action (no Rust per tool). Typed tools: default = `decision_value` (`{status, age_s, slot, source, features}`); a reducer addresses `decision_root` (`/data/...`, `/features/...`) |
| `world` | alias → observation key; read from the store every step, never fetched; stale / missing / error entries carry no numbers |
| `requires` | alias → max age secs; the action is offered only while each alias is usable and that fresh |
| `ok` | typed: `status != error`; text: HTTP status / parse (`reduce::parse_tool_output`) |
| Executor | loop agent's tools wrapped in `SanitizedToolExecutor` (process `SecretRegistry`) — text and observations redacted before history, audit, Jev |
| Caps | re-checked in code after Jev answers; violation = skip + audit |
| `dry_run` | non-`read_only` actions are logged, not executed |
| Escalation | reuse `webhooks::run_one_shot` path: state JSON as the user message, session `decide-<loop>-<uuid>` |

## Phases (one branch, `feature/decision-loop`)

| # | Deliverable | Files | Verify |
|---|---|---|---|
| 1 | Domain types + Jev client | `domain/decision.rs` (State, HistoryEntry, Question, Answer), `ports/decision.rs` (`DecisionEngine`), `outbound/decisions.rs` (via `egress::llm_api_client`) | unit tests on recorded JSON; `#[ignore]` live probe |
| 2 | Loop use case | `application/decision_loop/{mod,slots,reduce,world}.rs` — state build, ring buffer, slot resolution, threshold, dry-run, execute via the `ToolExecutor` port | unit tests with a fake `DecisionEngine` + fake tool |
| 3 | Config + CLI | `DecisionLoopConfig` in `config/decision_loop.rs` (+ validation: tool exists, slots non-empty, 0<act_at≤1); `tengu decide --sandbox <n> [--loop <name>]` in `inbound/cli/`; wiring in `bootstrap/decision.rs` | `tengu decide --sandbox lping` ticks, logs decisions, dry-run |
| 4 | Push triggers | webhook `auth_header_env` mode (Helius); endpoint `loop = "<name>"` feeds the loop instead of the planner | signed + header-auth tests |
| 5 | gRPC feed | Yellowstone client behind `solana_stream` feature (tonic); `egress` gains `grpc_channel` (open network only at first) | subscribe to Meteora DLMM program, events → loop |
| 6a | Typed read tools ✅ | observation envelope + cache (`domain/observation.rs`, `ports/observation.rs`, `outbound/observations.rs`, `application/observe.rs`), `world.rs`, `tools/solana/*`, `domain/lp/*` | `tengu decide --sandbox lping --loop lp_watch` twice < 60 s → second read `obs.source = cache` |
| 6b | Write tools + signing | `dlmm_open/close_position`, `jup_perps_order`, `jupiter_swap`; `SolanaSigner` port (none exists — crypto tools are EVM-only); `WriteResult`, wallet lease, simulate by default; Telegram approval on sign | devnet first; `dry_run = false` only after |

Phases 1–3 are the usable core (polling loop, dry-run). 4–6 build on it.

## Observability

| Surface | What |
|---|---|
| `MetricsKind::Decision` | new variant; tokens + cost + latency per Jev call |
| `<TENGU_HOME>/logs/decisions.jsonl` | state hash, questions, answers + probabilities, action taken / skipped / escalated; typed results carry `obs` (`key`, `status`, `source` live \| cache, `age_s`, `slot`) |
| `agentic_memory` | executed actions + escalations (durable, recallable by planner) |

## Docs to update when landing

Done for phases 1–4 and 6a. Phase 5: `docs/egress-2026-09-16.md` (`grpc_channel`), `docs/typed-observations-2026-09-24.md` (stream rows). Phase 6b: `docs/tools.md`, `docs/typed-observations-2026-09-24.md`, `CLAUDE.md` + `AGENTS.md`.

## Open questions

| # | Question | Default if unanswered |
|---|---|---|
| 1 | v1 trigger: polling tick or Helius webhook? | polling (no provider needed) |
| 2 | Jev as a chat `Engine` instead (tool `enum` params → slots)? | no — separate port; chat history ≠ normalised state |
| 3 | Signing: Privy Solana wallets or local keypair? | decide in phase 6b (`SolanaSigner` port, both impls possible) |
