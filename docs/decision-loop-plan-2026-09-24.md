# Decision loop — plan (2026-09-24)

Jev-driven control loop for `sandboxes/lping`. Context + probes: `docs/lping-2026-09-24.md`.

## Status (2026-09-24, branch `feature/decision-loop`)

| Phase | State | Verified |
|---|---|---|
| 1 Domain + Jev client | ✅ | unit tests on the probed wire shape; `#[ignore]` live test |
| 2 Loop use case | ✅ | fake-engine tests: history slots, caps, dry-run, escalate, illegal choice |
| 3 Config + `tengu decide` | ✅ | live: `fetch_pools` ran → Jev picked pool `HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR` (highest APR, TVL > $100k), 2 SOL → `dry_run`; ~$0.00014/decision |
| 4 Helius trigger | ✅ `auth_header_env` + endpoint `loop` | live listener: wrong header → 401; Helius-shaped POST → 202 → loop → escalated at 0.79 → planner turn started |
| 5 gRPC feed | ⏳ | — |
| 6 Solana LP tools + signing | ⏳ | `open_position` names `meteora_open_position` (allowed only while `dry_run`) |

Deviations from the sketch below: history is in-process (not `shared_cache`); a dry-run loop may name write tools not built yet; `tengu decide` never escalates (no orchestrator) — the webhook listener does.

## Shape

```
trigger (tick | webhook | gRPC)  ──► world snapshot
        ──► DecisionState {goal, world, history[N]}      (application/decision_loop)
        ──► Jev: next_action + one choice per arg slot   (ports/decision.rs → outbound/decisions/jev.rs)
        ──► threshold ── ≥ act_at   → ToolRegistry::invoke (existing tools + scopes)
                      ── < act_at   → escalate: orchestrator one-shot (planner → crypto_researcher)
                      ── skip/noop  → log only
        ──► reduce(result) → history (ring buffer) + audit JSONL + MetricsRecord
```

Key decision: **Jev picks, existing tools execute.** No new tool runtime — actions are rows mapping to catalog tools, so scopes, egress and `scope_lint` apply unchanged.

## Config (behaviour in TOML, doctrine #2)

```toml
# Illustrative — endpoint URL + reducer paths to be confirmed against the live Meteora API.
[decision_loops.lp_sol_usdc]
model      = "~typesafe/jev-latest"
goal       = "Keep the SOL/USDC LP position in range; max 2 SOL"
trigger    = { tick_secs = 30 }            # later: webhook = "solana_events" | grpc = "..."
history    = 8                             # ring-buffer size
act_at     = 0.8                           # min confidence to act; below → escalate
escalate_to = "crypto_researcher"
dry_run    = true                          # log decisions, execute read-only actions only

[decision_loops.lp_sol_usdc.actions.fetch_pools]
tool = "http_request"
args = { method = "GET", url = "https://dlmm-api.meteora.ag/pair/all_by_groups?search={pair}" }
slots = { pair = ["SOL-USDC"] }                          # static candidates
reduce = { pools = "/groups/0/pairs/*/{address,bin_step,fees_24h,liquidity}" }
read_only = true

[decision_loops.lp_sol_usdc.actions.open_position]
tool = "meteora_open_position"                            # phase 5
slots = { pool = { from_history = "fetch_pools.pools[*].address", top = 5 }, size_sol = ["0.5","1","2"] }
caps  = { size_sol = 2.0 }
```

| Concept | Rule |
|---|---|
| Slot candidates | static list, or `from_history` path into a reduced result; code labels them `pool_1..n`, maps back to full values |
| Reducer | JSON-pointer projection per action (no Rust per tool); result trimmed to fields listed |
| Caps | re-checked in code after Jev answers; violation = skip + audit |
| `dry_run` | non-`read_only` actions are logged, not executed |
| Escalation | reuse `webhooks::run_one_shot` path: state JSON as the user message, session `decide-<loop>-<uuid>` |

## Phases (one branch, `feature/decision-loop`)

| # | Deliverable | Files | Verify |
|---|---|---|---|
| 1 | Domain types + Jev client | `domain/decision.rs` (State, HistoryEntry, Question, Answer), `ports/decision.rs` (`DecisionEngine`), `outbound/decisions/jev.rs` (via `egress::llm_api_client`) | unit tests on recorded JSON; `#[ignore]` live probe |
| 2 | Loop use case | `application/decision_loop/{mod,state,slots,reduce}.rs` — state build, ring buffer, slot resolution, threshold, dry-run, execute via `ToolRegistry` | unit tests with a fake `DecisionEngine` + fake tool |
| 3 | Config + CLI | `DecisionLoopConfig` in `config/mod.rs` (+ validation: tool exists, slots non-empty, 0<act_at≤1); `tengu decide --sandbox <n> [--loop <name>]` in `inbound/cli/`; wiring in `bootstrap/decision.rs` | `tengu decide --sandbox lping` ticks, logs decisions, dry-run |
| 4 | Push triggers | webhook `auth_header_env` mode (Helius); endpoint `loop = "<name>"` feeds the loop instead of the planner | signed + header-auth tests |
| 5 | gRPC feed | Yellowstone client behind `solana_stream` feature (tonic); `egress` gains `grpc_channel` (open network only at first) | subscribe to Meteora DLMM program, events → loop |
| 6 | Solana LP tools | `meteora_open/close_position`, balances; Solana signing (none exists — crypto tools are EVM-only); Telegram approval on sign | devnet first; `dry_run = false` only after |

Phases 1–3 are the usable core (polling loop, dry-run). 4–6 build on it.

## Observability

| Surface | What |
|---|---|
| `MetricsKind::Decision` | new variant; tokens + cost + latency per Jev call |
| `<TENGU_HOME>/logs/decisions.jsonl` | state hash, questions, answers + probabilities, action taken / skipped / escalated |
| `agentic_memory` | executed actions + escalations (durable, recallable by planner) |

## Docs to update when landing

`docs/code-map.{md,html}`, `docs/tools.md` (actions → tools), `docs/egress-2026-09-16.md` (phase 5), `config.example.toml` + `sandboxes/lping`, `src/domain/metrics.rs` doc, `SESSION_HANDOFF.md`, `CLAUDE.md` + `AGENTS.md` (new subsystem).

## Open questions

| # | Question | Default if unanswered |
|---|---|---|
| 1 | v1 trigger: polling tick or Helius webhook? | polling (no provider needed) |
| 2 | Jev as a chat `Engine` instead (tool `enum` params → slots)? | no — separate port; chat history ≠ normalised state |
| 3 | Signing: Privy Solana wallets or local keypair? | decide in phase 6 |
