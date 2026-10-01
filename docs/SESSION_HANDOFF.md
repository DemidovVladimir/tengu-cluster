# SESSION_HANDOFF.md — Tengu-Cluster running state log

> Restored 2026-05-14. The file was dropped from the tree during the
> agentic-memory rework branch; `CLAUDE.md` / `AGENTS.md` still list it as
> required reading, so it is back. Keep the top section current.

---

## Resume here (Thu 2026-10-01 evening — xlab: history first)

Operator 2026-10-01: "why do I have to wait 2 days" — answer from history, never block on live recording (CLAUDE.md gotcha "History first"). Built the same day on `feature/xmarket` (not pushed): sandbox `xlab` for the PRD v0.5 harness, doc `docs/xlab-2026-10-01.md` (§ 14 = first results).

| Topic | State |
|---|---|
| Commits | `b19c915` prep · `18221e8` Jev on a clock + decision cache · `d1154b1` sandbox + skill · `e7cd063` market.db + backfill · `9645fc0` engine · `6b7a0e0` `market_history` · `90242ef` gate arm · `d46639b` `tengu backtest` · `b8bb075` CLAUDE / AGENTS / tracker · `02f0c61` `backtest` tool · `2750f78` gate wiring + splits + entry liquidity + capped ranking · `f7baf25` Architect tools · `e6b0075` split CIs + skill call shapes |
| Data | `~/.tengu/state/xlab/market.db`: 79 instruments (75 xyz + BTC / ETH / SOL / HYPE), 1h bars from 2026-03-07, funding from 2026-03-01, Sept HL-archive ctx (crypto); re-run `tengu history backfill --sandbox xlab --instruments @crypto,@xyz_stocks --interval 1h --from 2026-03-01 --funding` to extend (resumes) |
| Results | rule W +51.3 bps (n 1,500, CI +15.5 … +82.5); liquid-entry variant holdout +75.4 (CI +7.4 … +136.2); placebo −58.9; every other library strategy no-go; Jev gate on W: +32.8 vs rules, CI spans 0, p(take) uninformative (Brier 0.35) — `docs/xlab-2026-10-01.md` § 14 |
| Weekend run | optional now: it adds only executable xyz weekend books (no archive has them). The frozen-binary rules below still hold if it runs; the throwaway sampler (pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid`) is a sleeping shell — `kill <pid>` to drop it |
| Next | `docs/xlab-2026-10-01.md` § 12: HL-archive L2 import (crypto executable books), info layer on history (EDGAR / news as `event_window` events), `ask_architect` escalation replay, capability lifecycle store, forward paper of a validated spec; local engine legs of `market_history` / `backtest` on the operator's PC |

---

## Previous: Resume here (Thu 2026-10-01 ~09:00 ET — W1 done: 34 items + the W1 gate passed; weekend run Fri, then operator decisions + W2)

| Topic | State |
|---|---|
| **Weekend run (operator action)** | Start `tengu run --sandbox xmarket-weekend` **≤ Fri 2026-10-02 19:30 New York = Sat 01:30 on this Mac (Europe/Berlin)**: tmux pane, foreground (the vault prompt suspends a `&` job), the frozen W1-gate binary `~/.cache/tengu-xm.noindex/weekend/tengu-6fcb455` started under `caffeinate -i -s` (runbook). Keep the Mac on AC power (`-s` holds only on AC), lid open, online until Mon 2026-10-05 10:00 New York (16:00 Berlin). Runbook + timeline: top of `sandboxes/xmarket-weekend/config.toml`; soak results: `docs/runtime-2026-09-30.md` § Weekend run. The throwaway sampler (pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid`) records the same weekend independently |
| Operator rules | 2026-10-01: workflows / parallel agents allowed for speed — keep ≤ 3 at once, each build in its own `~/.cache/tengu-xm.noindex/agents/<label>` (cloned, then `cargo clean -p tengu-cluster`), deleted after; before (2026-09-30): **one agent at a time, low priority** (the claude process runs `renice 15` + `taskpolicy -b`; every build `CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR` under `~/.cache/tengu-xm.noindex/` — Spotlight skips `.noindex`); **never start a local model on this Mac** (Ollama server stopped; the live `local` engine check runs later on the operator's Windows gaming PC over the LAN) |
| Why | 6 parallel agents + gemma4 (10 GB) + Spotlight indexing ~30 GB of build output made the Mac lag badly |
| Merged since wave A | `x-bridge-conformance-test` 7f02717 (hidden `tengu tool list|call`; 46 cases now), `risk-paper-fill-engine` 7859383, `hl-ctx-tool` ea0b05b, `hl-book-tool` 1e6c372 (`tools/hyperliquid/book.rs::fresh_book` = the paper engine's live book read, wired by `risk-gate-enforcement`), `risk-gate-domain` 81e0065, `risk-paper-ledger-store` b71d1be (`ports/paper.rs`: `place(PlaceRequest, Decide)` = gate + fill + write in one `BEGIN IMMEDIATE`), `risk-kill-switch` ea69033 (`tengu risk status|halt|resume`, `risk_status`; 48 conformance cases), `rt-scheduler` 24a7dd3 (`[feeds.<n>]` tool / tick feeds, windows + DST-safe `at` ticks; `tengu run` refuses a feed whose tool its agent cannot run), `x-engine-matrix-smoke` 0ad160d (live 2026-09-30: gemini-2.5-flash-lite, claude-haiku-4.5 on OpenRouter and claude-haiku-4-5 on the Claude CLI × workspace / hyperliquid / xm tool sets — 9/9 green, ≈ $0.06; `tengu doctor --engines`; the local leg runs only on the operator's Windows PC via `TENGU_MATRIX_LOCAL_BASE_URL`), `risk-gate-enforcement` 5dd0a96 (exec tools refuse unless the caller is a private agent; bridge call ids `mcp:<uuid>:<id>`), `risk-paper-tools` 4209d64 (`paper_order` / `paper_close` / `paper_positions`; hidden `tengu tool turn`; live matrix xm set green), `risk-audit-verdicts` 1cd924d (`logs/risk.jsonl` mirror, `StepOutcome::Refused`), `x-exit-rules` 845cf25 (`[risk.exits]`, exec tool `xm_exits` via a `kind = "tool"` feed) + ebd3d17 (docs), `x-shared-workspace-and-state-layout` 5e03e86 (one xmarket workspace, `[risk]` needs `[xmarket]`, state layout, prune spares `state/`), `x-weekend-fade-strategy` fbb3246 (rule W as exec tool `xm_weekend_fade`: capped + shadow ledgers, 60 s idempotent steps, golden replay of 2026-09-26 bit-exact), `x-weekend-sandbox` bc51087 (`sandboxes/xmarket-weekend`: floor profile, one private agent, 5 required feeds, recorder; golden replay through the sandbox config; 30-min live soak green — 0 WARN / ERROR, HL weight ≤ 190 / min of 1200, SIGTERM drain 0.32 s), `ops-sandbox-config` a27f9b5 (`sandboxes/xmarket` M0 stage: planner `xm`, routable `xm_architect`, private `xm_executor`; HL ctx + books recorded, exits, daily risk roll; 5-min smoke green), `x-engine-parity-audit` 1f52df0 (E0 closed: every catalog tool + a shell skill + an `[[mcp_servers]]` proxy pass lint + conformance (57 cases) + live matrix on gemini-2.5-flash-lite / claude-haiku-4.5 / Claude CLI, 12 / 12 sets each; leads 1–12 fixed — parent-session env stripped, explicit bridge workspace grant, OpenRouter failed-turn retry + `native_finish_reason`, bridge `compress_and_store`, no secrets in `--mcp-config`, shell skills bridged, chat honours `tools`, chat call ids namespaced, Privy under the egress ceiling; open: the live `local` column on the operator's PC) |
| Wave B worktrees | all merged and removed · local-leg commands for the Windows PC: `~/.tengu/state/xmarket/research/parity_audit_leads.md` |
| W1 gate (passed 2026-10-01) | Three read-only adversarial reviews — weekend path (→ 6fcb455, frozen weekend binary + 30-min soak 2 green), money safety (→ 0fd620b access, 270f23e ledger, batch 2), engine parity / doctrine (→ batch 2) — then batch 2 in parallel worktrees: 8e35c28, 78f6f6f, b0e3e31, e50483c, 7e8cfb0, b389d06 (tracker W1 notes). Final checks on 11900f7: fmt, lints, conformance, run_agent_ipc, mcp_bridge_external, offline matrix, 1328 unit tests, `cargo check --all-features --all-targets`; live engine matrix on 11900f7: 39 / 39 legs green on gemini / haiku / Claude CLI (one gemini wording flake passed on a rerun; `local` + Postgres legs skipped) |
| Operator decisions (open) | Tracker W1 note "operator decisions": aura `editor_shell` built-ins with the full env; aura wallet tools vs. signing without approval; aura `learning-agent` without a workspace; weekend capped book limits; Docker owner name |
| Next | Fri ≤ 19:30 ET the weekend run (row above; the FROZEN binary — never run a newer one on `~/.tengu/state/xmarket-weekend/` before Mon 10:00 ET: it would add ledger columns mid-run) → Mon: analyse the weekend (fade rows, both ledgers, the sampler) → operator decisions → W2 (inputs: VPS + SSH alias, dedicated OpenRouter key) · the live `local` legs when the Windows PC is ready |
| Weekend replay inputs (outside the repo) | `~/.tengu/state/xmarket/research/replay-2026-09-26/`: `universe.txt` (75 xyz single stocks = HL `stocks` minus 17 ETFs, STRC preferred, OURA pre-IPO; delisted IBIDEN out), `weekend_2026-09-26_candles.json` (anchor / entry / exit 5m candles), `weekend_2026-09-26_golden.json` (74 names ex KIOXIA: mean net +95.5 bps, 53 positive, capped CRCL / SMSN / MINIMAX / MSTR). Also there: all 128 xyz markets' 5m candles (`c5m_all_xyz_2026-09-25_to_28.tgz`), `perpCategories.json`, annotations summary; if lost, re-fetch 5m candles for 2026-09-25 18:00 → 09-28 16:00 UTC (HL keeps ~17 days of 5m bars) |

## Current (2026-09-30, W1 in progress): xmarket wave A merged on `feature/xmarket`

| State | Detail |
|---|---|
| Branch | `feature/xmarket` (local, not pushed); one commit per tracker item; progress + deviations: tracker § 0 "Where to begin" + "W1 notes" |
| Landed (wave A, 16 items) | E0: `x-bridge-parity`, `x-claude-code-hardening`, `x-tool-schema-lint`, `x-local-model-fit` (+ `risk-exec-idempotency-ids`) · M0: `risk-config-schema`, `ops-audit-atomic-write`, `hl-market-schema`, `rt-backoff-budget`, `hl-info-client`, `risk-calc-costs`, `risk-paper-ledger-domain`, `rt-daemon`, `rt-health` · M1: `kg-calendars`, `ops-history-recorder` |
| Running (wave B) | `hl-ctx-tool`, `hl-book-tool`, `risk-gate-domain`, `risk-paper-ledger-store`, `risk-kill-switch`, `risk-paper-fill-engine`, `rt-scheduler`, `x-bridge-conformance-test`, `x-engine-matrix-smoke` |
| Next | wave C: `risk-gate-enforcement`, `risk-paper-tools`, `risk-audit-verdicts`, `x-exit-rules`, `x-engine-parity-audit`, `ops-sandbox-config`, `x-shared-workspace-and-state-layout`; wave D: `x-weekend-fade-strategy`, `x-weekend-sandbox` + 30-min soak by Fri 2026-10-02 18:00 ET |
| New commands | `tengu run [--sandbox <s>]` (lease, heartbeat, drain), `tengu doctor --sandbox <s> --live`, `tengu history range|asof <key>` — `docs/runtime-2026-09-30.md` |
| Verified live | Claude CLI 2.1.285 merges the `--mcp-config` env (secrets reach the bridge by inheritance); without `--strict-mcp-config` it loaded 28 operator MCP servers (144 tools), with it none; all 38 tool schemas accepted by gemini-2.5-flash-lite, claude-haiku-4.5, gpt-4o-mini (OpenRouter) and parsed by Ollama 0.24; a haiku turn through the bridge returned a vault secret as `[REDACTED]` |
| How waves run | `Workflow` with `isolation: "worktree"` (worktrees start from `main` — agents `git merge --ff-only <base>` first); each agent builds in its own `CARGO_TARGET_DIR` cloned (APFS `cp -Rc`) from `~/.cache/tengu-xm/seed` — a shared `target/` let a stale test binary from a deleted worktree run (`tests/code_map.rs` "NotFound"); coordinator cherry-picks one item at a time, adds code-map rows + regenerates the html, ticks the tracker, runs the item gate |
| Weekend data | throwaway sampler pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid` (records Fri 19:30 ET → Mon 10:00 ET); 5m candles of the 2026-09-26 weekend for all 128 xyz markets saved outside the repo for the replay fixture |

## Next session (set 2026-09-30): build xmarket — start with the build plan

| Read | Why |
|---|---|
| `docs/xmarket-build-plan-2026-09-30.md` | **First.** The operator's mandate (build the full PRD scope; every tool works 100 % under `openrouter`, `local` and `claude_code`; a separate `xmarket-weekend` sandbox; validate and fix until it runs smoothly), "Before the first item", waves W1–W9 with gates (W1 = E0 engine parity + the weekend-run slice, deadline **Fri 2026-10-02 18:00 ET**), the engine validation matrix, operator inputs, and the kickoff prompt to paste |
| `docs/xmarket-tracker-2026-09-29.md` **§ 0 Start here** | Rules for every task (R1–R13), definition of done; § 1 milestones (E0 first); § 5 backlog (185 items) |
| `docs/xmarket-prd-2026-09-29.md` (addendum at the top) | Operator decisions: build everything, engine parity for every tool, paper first, $100 budget + `[risk]` caps, 24/7 `tengu run` on the operator's VPS, network `open` (switchable), no legal gates |
| `docs/xmarket-feasibility-2026-09-30.md` | Evidence, attached as a warning: verdict re-scope (cross-venue convergence fails after costs); the operator kept the full plan. Holdout: weekend fade passes on 53 new names, post-earnings rule not confirmed. Weekend order books are being recorded Fri 2026-10-02 → Mon 10-05 in `~/.tengu/state/xmarket/research/weekend-2026-10-02/` (throwaway sampler, pid in `sampler.pid`) — analyse them on Monday |
| `docs/xmarket-gaps-2026-09-29.md` | Per-item detail — read the entry before starting an item |

The xmarket section further down (2026-09-29 / 2026-09-30 rows) lists what is decided and what the operator still has to provide.

---

## TL;DR — current state (2026-09-29): Solana write tools (phase 6b)

Branch `feature/decision-loop` (not merged). Doc: **`docs/typed-observations-2026-09-24.md` § Write tools**. Plan: `/Users/vladimirdemidov/.claude/plans/enchanted-waddling-reef.md` (reviewed: 5 high findings folded in).

| Area | Change |
|---|---|
| Tools | 5 opt-in rows (`tools/solana/write_{tokens,swap,dlmm,perps}.rs`): `solana_close_token_accounts`, `jupiter_swap` (Ultra), `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order`; `mode` = simulate (default, keyless) \| send; result `write/1` (`domain/solana_write.rs`) |
| Wire format | `domain/solana_tx.rs` (legacy compile + serialize, legacy / v0 parse, System / SPL / ATA / ComputeBudget ix), `domain/lp/{dlmm_ix,perps_ix}.rs` — byte-for-byte goldens from the bot's own libraries (`tests/fixtures/solana/tx/golden.json`) |
| Signer | `ports/solana_signer.rs`; `outbound/solana/signer.rs` = `ed25519-dalek` over `[solana] signer_key_file` (0600, no-echo errors). Send only with the tool scope's `wallets = ["<pubkey>"]` on one non-routable agent |
| Signing sandbox | `config/solana.rs`: no `claude_code`, no `[[mcp_servers]]`, no scope with `shell_bins` (runtime: permissive fallback runs no shell — `AgentConfig::no_shell_fallback`), key outside every fs root; grant rules; read_only write actions must simulate |
| Send pipeline | `outbound/solana/send.rs` + `writes_store.rs` (`<TENGU_HOME>/state/solana-writes.db`): lease per wallet, pending record before submit (resolved first next time), one-attempt `sendTransaction` (JSON-RPC error = not sent), confirm / expire by `lastValidBlockHeight`, write fence; `Submitter` seam (RPC or Ultra `/execute`) |
| State | fence pins `lp_snapshot` and makes older snapshots `stale_input` in the decide tools; `merge_lp_state` (CAS × 3): close ⇒ `reentry`, perps ⇒ `last_hedge_action` (request-aware cooldown); open keeper requests keep wSOL open / refuse SOL-leg orders |
| Verified live (keyless) | mainnet simulations: Ultra 0.01 SOL→USDC (48 640 CU), DLMM open 20 bins (163 105 CU), DLMM close of a real 46-bin position (356 418 CU), perps short increase (98 233 CU) — `cargo test --bin tengu -- --ignored live_` |
| lping | new `[agents.lp_executor]` (no `description`) runs both loops; write tools simulate-only (no signer, no grant); `lp_watch.open_position` uses the real schema, `mode = "simulate"`; `http_request` `env_reads` tightened to `SOLANA_RPC_URL`; "Signing" how-to at the end of the config |

| Open | Detail |
|---|---|
| First live send | not done: needs a DEDICATED wallet (never the bot's `F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S` — the lease cannot see the TS bot) + the operator's go; order: close token accounts → ~$1 swap → tiny DLMM open + close → minimum perps order |
| Human approval | `TelegramConfig.tool_approvals` / `approve_only` are parsed but never read — no approval gate exists |
| Token-2022 pools | refused by the DLMM write tools (transfer-hook slices not built) |
| Privy signer | second `SolanaSigner` impl possible later |
| Loop write actions | `hedge_watch` has no write actions yet; `lp_watch.open_position` stays `dry_run` + `mode = "simulate"` |

## Previous TL;DR (2026-09-24): typed observations + Solana LP read tools

Branch `feature/decision-loop` (not merged). Subsystem doc: **`docs/typed-observations-2026-09-24.md`**. Loop doc: `docs/decision-loop-plan-2026-09-24.md`; sandbox: `docs/lping-2026-09-24.md`.

| Area | Change |
|---|---|
| Typed observations | `ToolOutput.observation` + `ToolExecutor::execute_typed` (default wraps `execute`); `domain/observation.rs` envelope (features ≤ 32 scalars, line 1 ≤ 200 with full ids, `Field<T>` / `ObsStatus` — failed reads never 0); `ports/observation.rs` + `outbound/observations.rs` (`<workspace>/.tengu/observations.db`, slot-monotonic, `Error` rows never stored, 7-day purge); `application/observe.rs::observe()` |
| Decision loop | `world` + `world_max_age_secs`, action `requires`, `FromObservation` slots, typed results + `HistoryEntry.obs`; loop tools wrapped in `SanitizedToolExecutor` (`build_decision_loop(.., secrets)`) |
| Review (2026-09-25) | 6-lens adversarial review: 31 findings, 11 refuted, 20 confirmed (5 medium, 15 low; none reachable with money — dry-run, no signer) and all fixed with regression tests: empty discovery bounded by caller max age (TTL 10 s), explicit-`positions` snapshots never stored under the canonical key, share > bin supply ⇒ incomplete, oracle reuse ≤ 10 s, `lp_state` compare-and-swap commits + unreadable ⇒ block, position/discovery read errors ⇒ `invalid_read`, grace clock per bot, in-cycle storm/freeze via `lp_knobs`, current-event `FromHistory`, JSON-RPC error text scrubbed, ids never cut in slot descriptions, base58 length pre-check. Open: cross-tool slot mixing of cached `acct/1` rows (share ≤ supply catches only part); architecture `.svg` has no decision-loop panel |
| Solana tools | 10 opt-in tools (`tools/solana/`): `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `hedge_decide`, `lp_decide`. Pure: `domain/solana.rs` (hand-rolled base58 + PDA), `domain/lp/{dlmm,perps,wallet,market,gates,hedge,snapshot}.rs`. IO: `outbound/solana/{rpc,accounts,http_json,plan}.rs` |
| Policy port | `domain/lp/hedge.rs` = bot hedge controller, 1027/1027 production vectors (`cargo test --bin tengu lp::hedge`); `gates.rs` = re-entry, storm, trend/regime confirm, composition, wallet 50/50, 70-bin range cap |
| Crates | `base64 = "0.22"` direct; `curve25519-dalek ~4.1` dev-dependency only (off-curve cross-check). No bs58 / solana-sdk / anchor / borsh |
| lping | `lp_watch` on typed tools (`sol_price`, `dlmm_pools`, `requires = { price = 30 }`); new `hedge_watch` (`lp_snapshot` → `hedge_decide` / `lp_decide`, `commit = true`, production knobs cited in TOML); both `dry_run` |
| Egress | new hosts: `api.mainnet-beta.solana.com` (or `$SOLANA_RPC_URL`'s host), `lite-api.jup.ag`, `dlmm.datapi.meteora.ag`, `hermes.pyth.network`; RPC URL never rendered (host only) — `docs/egress-2026-09-16.md` |

| Open | Detail |
|---|---|
| Phase 5 — push feed | Yellowstone gRPC → `acct/1:<pubkey>` rows (slot-monotonic put) + heartbeat row `stream/1:<name>` so unchanged accounts count as fresh; subscription set = union of `lp_snapshot` `data.watch`; trigger via `DecisionLoop::handle_event`; `egress::grpc_channel` |
| ~~Phase 6b — writes~~ | LANDED 2026-09-29 — see the TL;DR above |
| Pyth 401 | Hermes (and the benchmarks mirror) answer 401 → Pyth only with `pyth_feed_id` (`auth_required` error, Partial row); default oracle is Jupiter-only (`degraded = true` unless a pool cross-check is given) |
| ATA-only balances | `solana_wallet.balances` and `lp_snapshot.wallet_balances` count the mint's ATA only; tokens in other accounts appear only in `token_accounts` rows |
| Extended positions | > 70 bins decode fully (SDK-verified on a 164-bin position) but raise `ExtendedPosition`; a missing bin array ⇒ `complete = false` (amounts are a floor, row Partial); farming rewards + Token-2022 transfer fees not modelled; new ranges capped at 70 bins |
| Two-read slot skew | `plan::read_pool` = 2 GMAs (2nd pinned ≥ 1st slot) + reused `acct/1` rows up to their max age → `LpSnapshot.slot` (min) ≠ `slot_max` is possible; a lagging public node answers `-32016` (retried once) |
| Hedge knobs `trend_confirm_ms` + `no_lp_grace_ms` | LANDED: clamp-regime confirm for `lp_input = "midpoint"` and bot BUG-011 grace (counts from the first no-LP read, `LpControllerState.no_lp_since_ms`; no re-entry wait ⇒ action `none` "no-LP grace"; `0` = off). `hedge_decide` optional `lp_knobs` computes storm / imbalance freeze in-cycle (hedge_watch passes them) |
| Non-USDC-quote storm | `lp_decide` storm samples come from the USD `price_oracle` row; for a pool whose quote is not USDC they are dropped → `move_5m_pct = None`, storm never fires |
| Cache | no cross-process single-flight (WAL prevents corruption, not duplicate RPC on a miss); `acct/1` rows hold base64 data (bin array ≈ 13.5 KB), only the 7-day purge bounds growth |
| `sol_price` keys | pool-aware: `price_oracle/1:<mint>` vs `price_oracle/1:<mint>:<pool>` — a `world` alias must name the key the loop's `sol_price` call writes |

### 2026-09-29 — `sandboxes/jev-exec` (Claude architect → Jev executor)

| Change | Detail |
|---|---|
| Sandbox | `sandboxes/jev-exec/config.toml`: architect (in-process `claude_code`, subscription) whose only usable tool is `run_command` → `tengu decide --loop executor`; executor = Jev loop over `http_request` / `list_directory`. Verified live end to end — `docs/decision-loop-plan-2026-09-24.md` § Jev as an architect's hands |
| `tengu decide` | prints `history` (args + reduced result per step) — `DecisionLoop::history()` |
| TUI fix | direct (no-orchestrator) turns passed `bridge_tools: None` and the normal rebuild never set them → an in-process `claude_code` agent in `tengu chat` had NO tengu tools (only `/skill` commands did). Both paths now pass the bridge tools |
| Jev decision feed | audit lines gain `ok` + `output`; `tengu chat` on a config with `[decision_loops]` shows each decision of those loops live (`jev executor #1 · crypto_price (1.00) → executed` + slots, args, output). `view::hide_thinking` removed the LAST bubble, not the spinner — any mid-turn System bubble (feed, orchestrator events) was lost; now removes the indicator by index |
| Vault prompt fix | a `tengu` started by a tool (`run_command` → `tengu decide`) re-prompted `Master password:` on `/dev/tty` while the TUI owned it → "Engine stream timed out — no data for 120s". The first `tengu` to open the vault now sets `TENGU_SECRETS_LOADED` (loaded key names, set even on failure; forwarded to the Claude Code bridge); descendants inherit the secrets, register them for redaction, never prompt |

| Open | Detail |
|---|---|
| Plugin MCP leak | `claude -p` also loads the user's global Claude Code plugin MCP servers into every `claude_code` agent (outside tengu scopes/egress). Candidate fix: `--strict-mcp-config` in `engines/claude_code.rs` |
| Hand-off paths | `--sandbox` is cwd-relative and `TENGU_CONFIG` is not forwarded to the bridge → jev-exec hardcodes `~/development/tengu-cluster`. A `decide` tool or `--sandbox` resolution from `$TENGU_HOME` would remove it |
| Jev args | slots are enumerated only; a `FromEvent` slot source would let the architect pass values |

### 2026-09-29 — `xmarket` PRD + gap tracker (planning only, no code)

| Doc | What |
|---|---|
| `docs/xmarket-prd-2026-09-29.md` | Operator PRD, verbatim: event-driven cross-market trading intelligence (news / X / EDGAR + Hyperliquid HIP-3 + Robinhood Chain → Jev → risk gate → paper) |
| `docs/xmarket-tracker-2026-09-29.md` | The backlog: 179 items in M0–M8 + M3b (live pilot), 20 conventions, accounts + operator setup, decisions, risks, verified facts |
| `docs/xmarket-gaps-2026-09-29.md` | Per-item research notes (files, API shapes, no-Rust options, evidence) — look up by id |

| Open | Detail |
|---|---|
| M0 not started | thin paper slice running 24/7 on the operator's Hetzner / Hostinger VPS (Docker): bridge parity first, then `tengu run` + HL reads + EDGAR 8-K → one Jev loop → risk gate ($100 budget) → paper fill → exit rules → audit (40 items); M3 is a go / no-go edge check; M3b = live pilot on a $100 Hyperliquid sub-account after an M3 go |
| Operator decisions (2026-09-30) | Operator in Kazakhstan, no legal or regulatory gate in the plan (venue choice is the operator's; only effect: Kazakh connections cannot reach Coinbase, OKX, … → deploy outside); `network = "open"`, switchable later (convention 17); one OpenRouter key for Jev + LLM at $40 / day. `SEC_USER_AGENT` set in `.env` and verified 2026-09-30. Budget $100, paper first then real (M3b), server = operator's Hetzner or Hostinger VPS. Pending operator action: create the xmarket OpenRouter key; pick the server + SSH alias (tracker § 6, operator setup) |
| Doc contradiction | OpenRouter over Tor: `docs/egress-2026-09-16.md` says reachable, `docs/lping-2026-09-24.md` says blocked — `hl-tor-probe` (M1) settles it |
| Claude Code rule (2026-09-30) | Every tool must work under `engine = "claude_code"`, no exceptions — CLAUDE.md / AGENTS.md step 4 + gotcha, `docs/tools.md` step 5, tracker convention 20. The bridge is not at parity today (default config + `main` agent, empty `SecretRegistry`, `no_shell = false`, no `TENGU_CONFIG` / `--strict-mcp-config`): M0 items `x-bridge-parity`, `x-claude-code-hardening`, `x-bridge-conformance-test` |
| ~~Stale gotcha~~ fixed 2026-09-30 | CLAUDE.md / AGENTS.md said only aura runs `open`; now they list aura, lping, jev-exec, unlimited (and the planned xmarket) |

---

## Previous TL;DR (2026-09-23): hexagonal layout

Branch `refactor/hexagonal` (not merged). Plan + per-phase status: `docs/hexagonal-plan-2026-09-23.md`. Start any "where is / how do I" question at **`docs/code-map.md`** (+ interactive `docs/code-map.html`).

| Area | Change |
|---|---|
| Layout | `src/{domain,ports,config,application,bootstrap}` + `src/adapters/{inbound,outbound}`; `main.rs` is 14 lines. Old `channel_runtime.rs` / `engine_builder.rs` / `types.rs` / `tool_plugin.rs` / `plugins/` / `*_builder.rs` are gone — every doc path was rewritten |
| Enforcement | `tests/layering_lint.rs` (layer rules, 0 exceptions), `tests/code_map.rs` (code map lists every file, graph block current — regen `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`), `tests/scope_lint.rs` (now actually scans `outbound/tools` + `mcp_client`) |
| Tools | One `ToolEntry` row in `adapters/outbound/tools/mod.rs::catalog()`; opt-in names in `domain/tools.rs::WORKSPACE_TOOLS`. Guide: `docs/tools.md` |
| New ports | `Embedding`, `MemoryService`, `RecallStore` (`ports/memory.rs`), `ToolDirectory` (`ports/tool.rs`); `SecretRegistry` → `domain/secrets.rs` |
| MCP | `[[mcp_servers]]` tools are `{server}__{tool}` and reach plan-step subagents (both engines) + in-process TUI/Telegram agents (see rows below) |

| Open | Detail |
|---|---|
| ~~No local-model engine~~ added 2026-09-23 | `engine = "local"` (+ optional `[agents.<n>.local] base_url`, `api_key_env`; defaults Unsloth `http://127.0.0.1:8888` / `UNSLOTH_API_KEY`). Own engine `engines/local.rs` (`LocalEngine`), direct connection, not part of `[egress]`; keyless = no auth header. Tests: `engines::local::tests::*` (keyless + tool call, bearer, unreachable server), `local_engine_parses_with_and_without_block`. Live: `run-agent` on Ollama `gemma4:latest`, Tor down → `status=ok`. Not tried against a real Unsloth install (not installed here). |
| ~~`[[mcp_servers]]` tools invisible to plan-step subagents~~ fixed 2026-09-23 | `build_subprocess_tool_executor` now advertises the executor's MCP tools (through `tools`); Claude Code engine passes servers to the bridge (`TENGU_BRIDGE_MCP_SERVERS` + forwarded `$VAR`s), bridge registers `McpPlugin`. Names `{server}.{tool}` → `{server}__{tool}` (providers reject `.`). Tests: `mcp_client` fake-server test, `subprocess_executor_advertises_mcp_server_tools`, `claude_code` bridge-config test, `tests/mcp_bridge_external.rs` (real `tengu mcp-bridge` ↔ `tests/fixtures/fake_mcp_server.sh`). Not tested against a live LLM. |
| ~~In-process Claude Code agents (TUI/Telegram) don't see `[[mcp_servers]]` tools~~ fixed 2026-09-23 | `bootstrap::tools::with_mcp_bridge_tools` lists the servers once at agent setup and appends `{server}__{tool}` to the bridge list; `ChatRuntimeService.mcp_servers` / `ChatTurnInputs.mcp_servers` reach `EngineContext`. Test: `in_process_claude_code_agent_gets_mcp_tools_and_servers`. |
| ~~Flaky test `learner_state::tests::save_is_atomic_concurrent`~~ fixed 2026-09-23 | Real bug: temp names were `.<name>.tmp-<nanos>`; macOS clocks tick in µs, so concurrent writers collided and the losing `rename` failed. Now a UUID suffix (`evolve.rs::unique_suffix`) in `learner_state::save`, `manage_skill`, `skill_distill`, evolve apply, `tengu skill install`. Regression test `concurrent_saves_never_collide` (8×100; failed 3/3 before). |
| ~~Webhook agents on `engine = "claude_code"` get no bridge tools~~ fixed 2026-09-23 | `adapters/inbound/webhooks.rs` passed `bridge_tools: None` (no tengu or MCP tools). Now `bridge_inputs` hands the executor's tool list (catalog + skills + `{server}__{tool}`) and `[[mcp_servers]]` to Claude Code engines. Test `claude_code_webhook_agent_gets_bridge_tools_and_mcp_servers`. |

---

## Previous TL;DR (2026-09-18)

Tor-by-default egress, one config per sandbox, deploy/tor = Arti + lyrebird-rs.
Everything below is **uncommitted on `main`** (on top of the 2026-09-12 /
2026-09-16 work, also uncommitted). `cargo check --all-features`, `cargo fmt
--check`, scoped tests, `tests/run_agent_ipc.rs`, `tests/scope_lint.rs` pass.

| Area | Change |
|---|---|
| `[egress]` default = Tor | `EgressConfig.network = "tor" \| "open"` (default `tor`). `resolved()`: tor → `proxy` = `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050`, `route_llm_api = true`; open → direct. Explicit values win; `route_llm_api` is now `Option<bool>`. Children receive the *resolved* config via `TENGU_EGRESS`. `warn_if_proxy_unreachable` (parent only, after sandbox resolution). `tengu doctor` prints `network`. |
| Claude Code + Telegram over Tor | `claude_code_profile` no longer refuses `route_llm_api`; the CLI child gets `HTTPS_PROXY`/`HTTP_PROXY` = HTTP CONNECT form of the proxy (`EgressPolicy::http_connect_proxy`, `claude_cli_env`; Arti serves CONNECT on the SOCKS port). `TelegramPipe::build_bot` builds teloxide's reqwest 0.11 client itself (`reqwest011` alias, 2026-09-19). |
| One agent schema | `agents/*.toml` + `src/adapters/agents/` (`AgentSpec`) **deleted**. `AgentConfig` gained `description` (presence = planner-routable), `example_queries`, `tools`, alias `skills` → `skill_packages`; `LimitsConfig.step_timeout_secs` (default 600). `shared_files::routable_agents` feeds `render_registry`; `RagPlanner::new(.., agents, ..)`; `SubprocessRunner::new(sandbox, session, agents)` — fail-fast on unknown agent, per-step `max_tool_rounds` / `step_timeout_secs` (the old spec `max_turns`/`timeout_secs` were never wired — parent always sent 20 / 180s). `run_agent_subprocess` loads the parent config first, then `[agents.<name>]`; `bootstrap::tools::subagent_config` replaces `agent_config_from_spec`; subagents now run with their real `limits` and per-agent `claude_code` profile. `skill_doctor(&config, ..)`. Also fixed: `tools = [...]` on `[agents.*]` used to be silently dropped (no field). |
| Decision loops (2026-09-24) | Branch `feature/decision-loop`. `[decision_loops.<name>]` = Jev (`~typesafe/jev-latest`, OpenRouter `/api/alpha/decisions`) picks action + arg slots, existing tools execute via the loop agent's executor. Files: `domain/decision.rs`, `ports/decision.rs`, `config/decision_loop.rs`, `application/decision_loop/`, `outbound/decisions.rs`, `bootstrap/decision.rs`, `cli/decide.rs`; webhooks gained `loop` + `auth_header_env` (Helius). `MetricsKind::Decision`; audit `<TENGU_HOME>/logs/decisions.jsonl`. Verified live (`tengu decide`, webhook 401/202/escalation). Open: history lost on restart, gRPC feed (phase 5), Solana LP tools + signing (phase 6). Plan: `docs/decision-loop-plan-2026-09-24.md` |
| lping sandbox (2026-09-24) | New `sandboxes/lping/config.toml` placeholder: `lping` planner + `crypto_researcher` (OpenRouter, `http_request`), `network = "open"`, webhook `solana_events` disabled. Plan + open gaps (webhook `Authorization`-header auth for Helius, stream consumer, Jev decisions gate, Solana tools): `docs/lping-2026-09-24.md` |
| Sandboxes | `aura`: `[egress] network = "open"` (Molecule/Privy/Beach block Tor); `[agents.aura]` = planner + DeSci subagent (description, tools, `step_timeout_secs = 720`); new `[agents.researcher]`, `[agents.learning-agent]` (from the deleted specs). `storage-test`: description/tools, `model = "claude-sonnet-4-6"`. `unlimited`: explicit `[egress] network = "tor"`. `config.example.toml` documents `[egress]` + a subagent example. |
| deploy/tor | `deploy/snowflake/` (Go lyrebird + socat, fixed-IP subnet) and `refresh-snowflake-bridges.sh` deleted. `deploy/tor/Dockerfile` = Arti 2.6.0 (`--features http-connect`) + lyrebird-rs built from the BuildKit named context `lyrebird-rs` (`../lyrebird-rs`, `LYREBIRD_RS_SRC` = dir or git URL). `arti.toml`: managed transport (`path = /usr/local/bin/lyrebird`, `run_on_startup = true`, protocols obfs4 + snowflake), Tor Browser bridge lines (7 obfs4 + 2 snowflake from lyrebird-rs `tools/arti-e2e/bridges-*.txt`), `tor-state` volume. `compose.yml`: one `tor` service on `127.0.0.1:9050`, healthcheck = `check.torproject.org` `IsTor:true`. `docker-compose.tor.yml` includes it and sets `TENGU_TOR_PROXY=socks5h://tor:9050`. |
| Makefile | `NETWORK=tor` (default) / `open` selects the compose file set for `up`/`up-memory`/`down`/`logs`/`status`/`doctor`/`clean`/`build`. `tor`, `tor-down`, `tor-logs`, `tor-bridges` (calls lyrebird-rs `bridges.sh`). Removed `up-tor`, `down-tor`, `tor-native`, `tor-native-down`. `LYREBIRD_RS_DIR` (default `../lyrebird-rs`) exported as `LYREBIRD_RS_SRC`. |
| Docker sandbox (2026-09-19) | `make up` had no way to pick a sandbox (always `./config.toml`). Now `SANDBOX=<name>` → `TENGU_CONFIG_FILE=sandboxes/<name>/config.toml`, bind-mounted by compose at `/opt/tengu/config.toml`; `NETWORK` defaults to that file's `[egress] network` (explicit `NETWORK=` still overrides); `check-config` guard on `up`/`up-memory` (a missing file used to make Docker create a `config.toml/` directory); `down --remove-orphans`; `make down-all` (both compose file sets + standalone `make tor`); `make chat` = `run --rm tengu chat` with the same wiring (README's `docker compose run tengu chat --sandbox aura` could never work: missing `./config.toml` becomes a directory, aura is `claude_code`). Open: `install.sh` / `cloud-init.yml` take no sandbox; image has no Claude Code CLI (`aura` can't run in Docker); `~` in sandbox paths = `/root` (not persisted). |
| Telegram over Tor (2026-09-19) | Docker `make up SANDBOX=unlimited` crash-looped: `GetMe` timed out. teloxide 0.10 defaults are 5s connect / 17s total and it does not extend the total for the 10s long poll; `api.telegram.org` over Tor measured 11–15s (sometimes >10s to connect). `TelegramPipe::build_bot` now builds the reqwest 0.11 client itself (`reqwest011` alias in `Cargo.toml`, `telegram` feature; lockfile gains only the dep edge) — proxy + 30s connect / 60s total; open network keeps teloxide defaults. `TELOXIDE_PROXY` no longer used. `sandboxes/unlimited` gained `[telegram] allowed_users` (empty rejected every message). |
| unlimited: LLM off Tor (2026-09-20) | OpenRouter is Cloudflare-fronted -- 403 "Just a moment..." on Tor exit IPs. `sandboxes/unlimited` set `route_llm_api = false` (network stays `tor`): LLM API direct, `http_request`/shell still proxied. Works for NATIVE `chat`/`telegram`. Does NOT work for Docker `make up SANDBOX=unlimited`: the container is on the internal `tor-front` net, so "direct" has no route to OpenRouter (`doctor` `IsTor=true`/`llm api: direct` only tests the tool client, not an LLM call). Docker + this sandbox would need `network = "open"` or a second internet-capable network on the tengu service. |
| Docker/installer | `Dockerfile` no longer copies `agents/`; `.dockerignore` no longer excludes `sandboxes/` (the previous `COPY sandboxes` could not have worked). `install.sh`: `TENGU_NETWORK`, `LYREBIRD_RS_SRC`/`LYREBIRD_RS_REPO`, clones lyrebird-rs, drives `make up NETWORK=…`; `cloud-init.yml` clones lyrebird-rs to `/opt/lyrebird-rs`, systemd uses `make up`/`make down`. |
| `http_request` | Hop-0 egress + `net_hosts` gate moved from `PreparedRequest::from_args` into `execute` (audit record on denial unchanged); `tests/scope_lint.rs` passes again. |
| Repo cleanup | Root `index/features/howto/memory/context.html` deleted (`docs/*.html` are the maintained pages); `Adaptive_AI_Learning_Marketplace_PRD.md` + `tengu/ideas/*` → `docs/ideas/`; `docs/configs/tui-memory-smoke.toml` deleted; empty `memory/`, `scripts/`, `.worktrees/` removed; `.githooks/pre-commit` now `cargo fmt --all --check` (it pointed at a script that did not exist). |
| Docs | README, `docs/configuration.md`, `docs/egress-2026-09-16.md`, `CLAUDE.md`/`AGENTS.md`, `config.example.toml` rewritten by hand; every other doc/diagram/skill/inline comment audited and fixed in place by the 2026-09-18 workflow (11 auditors, 65 files). `docs/architecture-v2.md` deleted (condensed duplicate of `REDESIGN.md`); superseded banners on `docs/architecture.md`, `docs/harness-architecture.md`, comparison/radar pages, every `docs/superpowers/*`. |

### Review findings applied (2026-09-18 workflow: 4 lenses × 3 skeptics, 22 raised, 20 confirmed)

| Finding | Fix |
|---|---|
| `-c/--config` never reached `run-agent` children (they resolve `$TENGU_CONFIG`) | `main` pins `TENGU_CONFIG` to the resolved path right after clap; verified with a live child run |
| `AgentConfig` lost the strict schema `AgentSpec` had | `#[serde(deny_unknown_fields)]` on `AgentConfig` + tests (`max_turns`/`timeout_secs`/`sandbox` rejected, `skills` alias, blank `description`, `step_timeout_secs = 0`) — all three sandboxes, `~/.tengu/config.toml` and `config.example.toml` still load |
| Non-routable blocks (no `description`) could still be dispatched | `run_step` and the child both filter on `description.is_some()`; error lists routable agents |
| Child used raw `toml::from_str` (no env substitution / validation) and swallowed parse errors | `Config::load` with an `error!` log; built-in defaults only when the file is absent |
| Child used `workspace = "~/…"` unexpanded | `expand_tilde` before scopes/memory/engine |
| aura subagent silently moved from OpenRouter (4 tools) to Claude Code with builtin Bash/Write | `[agents.aura.claude_code] builtin_tools_profile = "read_only"` (planner never needed builtins; subagent keeps the old no-shell contract) |
| `tengu skill doctor` could not see sandbox agents | `skill doctor --sandbox <name>` |
| Telegram `set_var(TELOXIDE_PROXY)` per message from tokio workers | `Bot` built once in `TelegramPipe::new` (`build_bot`), cloned per send |
| `http_connect_proxy` broke IPv6 proxy hosts | authority built from `host_str()` (brackets kept) + test |
| Startup probe checked only the first resolved address | tries every address (reqwest semantics) |
| MCP stdio / shells got `NO_PROXY=""` while MCP http exempted loopback | one `LOOPBACK_NO_PROXY` for all proxied children |
| Standalone `tengu mcp-bridge` ignored the operator's `[egress]` (forced Tor) | loads `$TENGU_CONFIG` / `<TENGU_HOME>/config.toml` `[egress]` when `TENGU_EGRESS` is absent |
| `make doctor` always passed `--tor` (fails under `NETWORK=open`) | `--tor` only under `NETWORK=tor` |
| `make tor` + `make up` both published 127.0.0.1:9050 | `deploy/tor/compose.internal.yml` (`ports: !reset []`) merged via the include path list in `docker-compose.tor.yml` |
| Relative `LYREBIRD_RS_SRC` resolved against `deploy/tor/`; `tor-bridges` ignored it | `LYREBIRD_RS_DIR` is the single knob (abspath unless `://`), exported as `LYREBIRD_RS_SRC` |
| cloud-init unit `TimeoutStartSec=120` < Tor bootstrap; ufw/bind comments implied 7080 is reachable under Tor | `TimeoutStartSec=900`; comments scoped to `NETWORK=open` |
| rustfmt failing on the hoisted `http_request` gate; scope_lint regex needs `scope.check_` on one line | formatted; gate kept on one line |
| Deleted `AgentSpec` tests had no successors; new paths untested | `config::` (3), `shared_files::registry_lists_routable_agents_only`, `channel_runtime::subagent_config_merges_workspace_tool_optins_from_tools`, `runner::run_step_fails_fast_on_unknown_agent`, `egress::` (+3) |
| Stale help text / comments (`agents/*.toml`, `agent_config_from_spec`, `tengu_outputs`, `RagStore`, Qdrant, `tengu.toml`, missing spec paths) | swept across `src/` (inline-comment auditor + follow-up); `skills/orchestrator/plan_schema.json` wording; `skills/orchestration-e2e/evals/config.toml` workers gained `description` so the eval can route |

### Verified (2026-09-18)

| Check | Result |
|---|---|
| `make tor` (Arti + lyrebird-rs image, obfs4 managed transport) | container healthy; Arti log `[pt lyrebird] connected`, `guard [… via obfs4 …] is usable` |
| `curl --socks5-hostname 127.0.0.1:9050 https://check.torproject.org/api/ip` | `IsTor:true` |
| `curl -x http://127.0.0.1:9050 …` (HTTP CONNECT — the Claude CLI / teloxide path) | `IsTor:true` |
| `tengu doctor --sandbox unlimited --tor` and `tengu doctor --tor` on a config with no `[egress]` | `network: tor`, `llm api: via proxy`, `tor: IsTor=true`, exit 0 |
| OpenRouter `GET /api/v1/models` and `api.telegram.org` through the proxy | HTTP 200 / 302 |
| `docker compose … config` for `deploy/tor/compose.yml`, base, base + tor override | resolves; `lyrebird-rs` context = `/Users/…/lyrebird-rs`, `TENGU_TOR_PROXY` set on tengu |
| CI gate (final) | `cargo fmt --all --check` clean; `cargo test --all-features` = 375 unit + 4 `run_agent_ipc` + 2 `scope_lint`, 0 failed; `cargo clippy --all-features` = same 40 pre-existing warnings as before this session, none new |
| Strict schema | `tengu doctor --sandbox {aura,storage-test,unlimited}`, `tengu status` on `~/.tengu/config.toml`, `config.example.toml` all load under `deny_unknown_fields` |
| `--config` propagation | child spawned with only `TENGU_CONFIG` (as pinned by the parent) resolves `[agents.foo]` from that file and builds its engine |
| `make -n doctor` / `NETWORK=open` / `LYREBIRD_RS_DIR=../foo` / `LYREBIRD_RS_DIR=https://…` | expand as intended; `bash -n deploy/install.sh` ok |
| Docker | `tengu-cluster:latest` builds (bakes `sandboxes/` + `skills/`, no `agents/`); `tengu-tor:latest` builds; merged compose config: `tor` has no host port inside the project, `tengu` no ports, `TENGU_TOR_PROXY` set |

### Open after this pass

| Item | Note |
|---|---|
| lyrebird-rs on GitHub | **Closed 2026-09-18** — pushed to `DemidovVladimir/lyrebird-rs` at `082ea0254fd057c617fdad86158129d0ec78aaec`; a fresh clone matches the local checkout, and `LYREBIRD_RS_DIR=https://github.com/DemidovVladimir/lyrebird-rs.git` builds `tengu-tor` from the git context (every lyrebird layer a content cache hit against the local build). `install.sh` / `cloud-init.yml` clones now work. |
| Claude Code CLI proxying is env-based | `HTTPS_PROXY` (advisory); network-enforced only under Docker `make up`. |
| Docker `make up` end-to-end | both images build and the merged compose config is right; a full Telegram session over Tor was not exercised in this pass. |
| `sandboxes/unlimited` model | The working tree changed `moonshotai/kimi-k3` → `qwen/qwen3.8-27b` (+ identity name) **before** this session (already modified at session start); the 900 s timeout / 32k cap comments still mention kimi-k3. Confirm or revert that hunk before committing. |
| `skills/orchestration-e2e/evals/config.toml` | Pre-existing: `workspace_tools` lists `http_request` / `memory_search` / `memory_ingest`, which `Config::validate` rejects (`tengu status` on it fails with 5 issues); `tengu eval` loads it through its own path. Also `skill_distill` seeds `evals/config.toml` with the calling agent's engine while `eval_builder::load_eval_config` refuses `claude_code` (auditor finding, not fixed). |
| Skill tier precedence | `shared_files::scan_skill_summaries` (registry) is project-first while `skill_builder::skill_directories` / `view_skill` are managed-first (auditor finding, not fixed). |
| `adapters::outbound::engines::build_planner_engine` | `#[allow(dead_code)]`, only consumer of `[claude_code] timeout_secs`; delete or wire (auditor finding). |
| Local Docker leftovers | `tengu-snowflake:latest` image from the deleted stack is still in the local Docker cache (`docker rmi tengu-snowflake:latest`). |
| Secrets in git history | unchanged from 2026-09-12 — rotate + purge. |

---

## Previous state (2026-09-12)

Audit-and-fix pass over the uncommitted agentic-memory tree (two Workflow
runs: 6 finders + 6 skeptics, then 4 fix clusters + 1 verifier). Everything
below is **uncommitted on `main`**; every feature combo compiles, scoped
tests pass, `cargo fmt --check` is clean.

| Area | Change |
|---|---|
| Scopes | `[default_scopes]` / `[agents.*.scopes]` are now **enforced** (were parsed, never used). `Config::fold_default_scopes` at load; `resolve_tool_scopes` in `build_tool_executor` + MCP bridge via `TENGU_BRIDGE_SCOPES` (`ClaudeCodeEngine::with_scopes`); children get their own workspace in `fs_roots` (`grant_workspace_root`). `check_env_read` honours `"*"`. `sandboxes/aura/config.toml` gained `fs_roots` for `http_request`. |
| agentic_memory | `execute` gates on `env_reads` for `TENGU_MEMORY_DATABASE_URL` (scope_lint passes); wrong-dim embeddings fail-soft on event insert + hybrid recall; `capture` defaults `session_id` / `agent` from `TENGU_SESSION_ID` / `TENGU_AGENT_NAME`; `pg_trgm` dropped from `ensure_schema`; DDL runs once per process (`OnceCell`). |
| Plan hand-off | `AgentIpcInput.plan_state` (per-session, `shared_files::set_active_plan`) is the source of truth; `TENGU_PLAN.md` is a debug artifact + fallback. Fixes the cross-session race for webhooks / Telegram. |
| Planner registry | Write failure of `TENGU_PLANNER_REGISTRY.md` is fail-soft (roster kept in memory). TOOLS section lists MCP server tools again (`<server>__<tool>` since 2026-09-23, enumerated once per `RagPlanner`). |
| Config | `OrchestratorConfig.engine` defaults to `"rag"` and is validated; dead `MemoryConfig` qdrant/backend/vector_size/embedding_provider/ttl_days fields + `[rag]` removed; `AgentConfig.requires` removed; `AgentSpec` is `deny_unknown_fields` + engine validated; `skill_lifecycle.fixture_runner_agent` optional; `TENGU_CONFIG` env honoured (`--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`). |
| CLI | `tengu doctor` exits non-zero on engine build failure (Docker HEALTHCHECK). `run-agent` exports `TENGU_AGENT_NAME`. |
| Dead code | `channel_runtime::build_vector_stack`, `VectorStore::delete_older_than`, `src/adapters/rag/**`, `src/application/memory/vector/qdrant.rs`, `__deltest.tmp` removed. `regex-lite` dropped; `rust-version = "1.78"`. |
| Tests | `tests/run_agent_ipc.rs` (Rust) replaces `scripts/test-runner.sh`; `tests/memory_search_tool.rs` (grep-test) and `scripts/phase-0-checks.sh` deleted. CI: single `quality.yml` (fmt, check --all-features, clippy advisory, test). |
| Run docs | README rewritten (real CLI table, features, mixed engines, Postgres memory, webhooks); Makefile `up-memory`/`native-memory` replace qdrant targets; Dockerfile bakes `agents/` + `sandboxes/`, exposes 7080 (webhooks); compose/installer/cloud-init use `postgres-memory` + real GitHub URL; `docs/configuration.md` has the env-var table; `config.example.toml` matches the real schema; root HTML pages de-Qdrant'd (`rag.html` deleted). |
| `unlimited` sandbox (2026-09-13) | `sandboxes/unlimited/config.toml` — direct-agent bench on OpenRouter DeepSeek (`unlimited`=v4-flash default, `pro`=v4-pro, `r1`=r1-0528; Telegram `@pro:` routing). Verified through `tengu run-agent` on all three models — recipe + results in `sandboxes/unlimited/BENCH.md`. |
| `tengu prune --hard` (2026-09-14) | `prune.rs::plan_prune` now takes `PruneOptions` (was 3 positional args). New `--hard` flag: with `--sandbox`, **empties the workspace root entirely** — enumerates every direct child (`.tengu/`, `memory/`, and any arbitrary agent-created dir like `image_payload/`) so the allow-list gap no longer leaves generated folders behind; keeps the root dir itself so it's reusable. Soft prune unchanged (allow-list + `scaffold.project.directories`). Never touches `sandboxes/<name>/config.toml`; does not clear Postgres `agentic_memory` (that's `make clean`). 2 unit tests in `prune.rs`; `docs/configuration.md` Reset section updated. |
| OpenRouter body-read timeout + output cap → config (2026-09-16) | Two `[limits]` knobs now drive the OpenRouter engine (were hardcoded / dead). **`request_timeout_secs`** (default 600) is the reqwest total timeout, body read included — `stream: false` means the body arrives only when generation ends, so slow reasoning models (`kimi-k3`) previously hit the 120s wall as `error decoding response body`. **`max_output_tokens_per_turn`** is now actually sent: set → `max_tokens: <n>`, unset → field **omitted** (model/provider default; no more synthetic `context÷8` send-ceiling). The `context÷8` formula survives only as a budget-reservation estimate when unset (`OpenRouterEngine::max_output_tokens_per_turn`, prompt-budget/TUI only). Body-read errors print the full cause via anyhow `{:#}`. Wiring: `build_openrouter_engine_with_limits(model, ctx, timeout, cap_opt)`; `build_engine` passes `agent_config.limits.{request_timeout_secs,max_output_tokens_per_turn}`; planner/eval use `config::default_request_timeout_secs()` + `None`. Tests: `engine_builder::tests::{body_read_failure_surfaces_source_chain, max_tokens_omitted_when_unset_sent_when_configured, budget_reservation_reflects_configured_cap}`. Note: `engine.run().await` sits outside the `stream_event_timeout_secs` idle loop, so `request_timeout_secs` is the only limit on a non-streaming call, and cancel isn't checked while it waits. |
| `[egress]` — Tor / host allowlist / audit (2026-09-16; **superseded by the 2026-09-18 section above**: Tor is now the default, `deploy/snowflake` + `tor-native`/`up-tor` are gone) | New `src/adapters/outbound/egress.rs`, process-wide policy (`install`; `TENGU_EGRESS` to `run-agent` + `mcp-bridge`). `proxy` (socks5h, reqwest `socks` feature) on the tool client (http_request, crypto), MCP-http, and — with `route_llm_api` — OpenRouter/embeddings/wiki compiler. `allow_hosts`/`deny_hosts`/`https_only` ceiling on every `http_request` hop (redirects now followed manually; credentials dropped cross-origin — previously reqwest auto-followed redirects past `net_hosts`). `run_command`/shell skills: URL-literal guard + proxy env; `shell_network = "isolated"` wraps `sh` in macOS `sandbox-exec` (only the proxy port). Claude Code `editor_shell` → `editor` under a proxy; `route_llm_api` refuses `claude_code`. JSONL audit per hop / network-looking shell command. `build_tool_executor` lost its `shared_http_client` arg (all callers passed `None`). `OpenRouterEngine::new` returns `Result`. `tengu doctor [--sandbox] [--tor]`. Docker: `deploy/tor/compose.yml` = **Arti 2.6.0** (SOCKS + HTTP CONNECT on 9050, internal networks only) **→ Snowflake** (`deploy/snowflake/`, lyrebird 0.8.1 pinned by tag+commit, `socat`-exposed unmanaged PT at 10.213.47.2:9150; `[bridges] enabled = true`, official Tor Browser 15.0.23 lines). `make tor-native` (adds `tor-port` → 127.0.0.1:9050) / `make up-tor` (`docker-compose.tor.yml` includes the stack; tengu on internal `tor-front`) / `make tor-bridges` (signed-bundle refresh). Arti deliberately not embedded (see egress doc). Upkeep: bump Arti + lyrebird pins and bridge lines with Tor Browser releases. Verified against real Tor — `docs/egress-2026-09-16.md`. Open: Linux `isolated` shell (use Docker override); `http_request` sends no `User-Agent`. |
| Secrets | `.env.example` values blanked. **The old values are still in git history (`0266655`, `d9037e3`) — rotate Molecule, Beach, Alchemy and the Qdrant Cloud JWT, then purge history.** |

### Decisions left to the maintainer

| Item | Options |
|---|---|
| Planner agent on `claude_code` (`sandboxes/aura/config.toml` `[agents.aura]`) | Doctrine says OpenRouter for the planner; the sandbox keeps `claude_code` for the subscription. Comment now states the trade-off; value unchanged. |
| `[hub]` config + `HubConfig` | Nothing listens on it (only `tengu status` prints it). Remove the struct, or keep as a placeholder. Container ports now point at the webhook listener. |
| `allowed_users = ['848344935']` in aura sandbox | Personal Telegram id in a tracked file; `${TELEGRAM_ALLOWED_USER}` substitution is available. |
| `build_tool_executor` is sync and drives MCP connect via `futures::executor::block_on`; the TUI calls it outside a tokio context | Latent panic if a sandbox sets `mcp_servers` and runs `tengu chat`. Make it `async` (all other callers already are). |
| `OrchestratorChatPort::run_orchestrator_turn` + `memory/injector.rs` + `MemoryProvider::prefetch` | Dead path (only `run_orchestrator_turn_with_system` is live). Delete in a follow-up. |
| Postgres connection per memory call | DDL is now once per process; connections are still per call. Pool if it shows up in latency. |

---

## Previous state (2026-05-14)

Runtime memory has been **replaced**: Qdrant-RAG → **Open Brain** (Postgres +
pgvector) + **Karpathy LLM Wiki** (compiled Markdown). New work is uncommitted
on `main`. Planner routing also moved off Qdrant — it is now file-backed
(`TENGU_PLANNER_REGISTRY.md` + `TENGU_PLAN.md`). All six implementation-doc
phases have landed, including the Phase 6 cleanup that fully removed the
legacy Qdrant `rag/` module, the `qdrant` cargo feature, and the
`tengu registry` / `tengu memory inspect` CLIs. Smaller gaps remain (below).

**Compile-verified 2026-09-12** (all feature combos, scoped tests). Postgres
smoke tests and the end-to-end turn were NOT run in that pass — see the
verification block.

---

## What landed (agentic-memory rework — uncommitted)

| Area | Change |
|---|---|
| Plugin | `src/adapters/outbound/tools/agentic_memory/mod.rs` — Postgres store + `agentic_memory` tool (`capture`/`recall`/`ingest_source`/`promote`/`compile_wiki`/`lint`) + free fns for planner/runner recall |
| Schema | `memory_events`, `memory_sources`, `memory_chunks`, `memory_promotions` (created under an advisory lock by `ensure_schema`); `vector` extension (`pg_trgm` dropped 2026-09-12 — nothing used it); FTS + HNSW indexes |
| Planner | `orchestrator/planner.rs` — `RagPlanner` de-Qdrant'd: registry loaded from `TENGU_PLANNER_REGISTRY.md`; recall lanes (cross-session / within-session / cross-plan) read Postgres `agentic_memory` under `postgres_memory` |
| Shared files | `orchestrator/shared_files.rs` — generates `TENGU_PLANNER_REGISTRY.md`, writes/reads `TENGU_PLAN.md` (plan state → subagent prompt) |
| Runner | `main.rs` — subagent summaries captured to Postgres (`try_persist_agentic_step_summary`) on both the `compress_and_store` path and the no-call backstop; subagent prompt loads `TENGU_PLAN.md` |
| Wiring | `bootstrap/` — `agentic_memory` registered in `register_catalog`, added to `WORKSPACE_TOOLS` + `compute_base_tools`/`advertised_defs`; `build_orchestrator` no longer gated on `qdrant` |
| Config / build | `config.rs` `valid_workspace_tools` += `agentic_memory`; `Cargo.toml` `tokio-postgres` + `postgres_memory` feature; `docker-compose.yml` `postgres-memory` profile (pgvector/pg16) |
| Docs | `docs/agentic-memory-{prd,implementation,examples}-2026-05-13.md`; architecture / comparison / context-management docs rewritten for the new model |

## What landed (this session — 2026-05-14, finish-code + Phases 4/5/6 + doc reconciliation)

| Area | Change |
|---|---|
| `.gitignore` | Ignore `TENGU_PLANNER_REGISTRY.md`, `TENGU_PLAN.md`, `/.codex/`, `**/.tengu/agentic-memory/raw/` (the compiled `wiki/` stays tracked — human-reviewable per PRD) |
| `agentic_memory/mod.rs` (chunks + recall) | `ingest_source` now embeds chunks (`memory_chunks.embedding` populated, fail-soft text-only fallback); agent-facing `recall` is now hybrid (pgvector → FTS) instead of FTS-only; tool schema gained `session_id`/`agent`/`role`/`reason`/`evidence` properties (the dispatch code already read them); new `env_embedder` / `embed_query` helpers; `insert_chunks` / `PostgresMemoryStore::recall` signatures gained params (one call site + one ignored smoke test updated) |
| `agentic_memory/mod.rs` (Phase 4 wiki compiler) | `compile_wiki` rewritten: runs an LLM over promoted memories → cited Markdown page (`[mem:<kind>/<id>]` inline + a deterministic `## Sources` footer). New module-scope helpers `compile_wiki_prompt` / `render_wiki_page_llm` / `wiki_compiler_model` / `chat_complete` (a minimal direct-OpenRouter chat call, the chat-side sibling of `Embedder`). Fail-soft: no `OPENROUTER_API_KEY` or an API error falls back to the old deterministic bullet dump. Model via `TENGU_WIKI_COMPILER_MODEL` env (default `anthropic/claude-sonnet-4-6`). |
| `metrics.rs` | New `MetricsKind::WikiCompiler` variant (+ `as_str` arm) so the wiki-compiler LLM call emits a `MetricsRecord` like every other LLM/embedding call. |
| `mcp_bridge.rs` + `main.rs` (Phase 5 MCP server) | New `tengu agentic-memory-server` subcommand — a standalone MCP stdio server exposing **only** `agentic_memory` to non-Tengu agents. Refactor: extracted `serve_mcp_stdio` (shared by `run_mcp_bridge` + the new `run_agentic_memory_mcp_server`); `handle_initialize` gained a `server_name` param (`tengu-tools` vs `tengu-agentic-memory`). CLI: new `Commands::AgenticMemoryServer` variant + early-return (stderr-only tracing, JSON-clean stdout) + feature-gated match arms, mirroring `McpBridge` / `RunAgent`. Operator doc: `docs/mcp-bridge.md`. |
| Phase 6 — full Qdrant removal (~10 files) | `adapters/inbound/webhooks.rs` persist repointed from `rag::RagStore` → `agentic_memory::write_step_summary_with_embedding` (the landmine: `webhooks` had an undeclared `qdrant` dep, so `--features webhooks` alone never compiled). `compress_and_store.rs` gutted to just `definition()` (Qdrant plugin/handler/`write_summary` gone). `src/adapters/rag/` orphaned — `pub mod rag` removed from `adapters/mod.rs`. `tengu registry` + `tengu memory inspect` CLIs deleted (`Commands` variants, `RegistryAction`/`MemoryAction` enums, all four command fns, dispatch arms, `try_persist_step_summary` + its call sites). `memory/vector.rs` qdrant `VectorStore` impl orphaned; `bootstrap/` `build_vector_stack_async` is disk-only and `resolve_qdrant_collection` removed. `Cargo.toml`: `qdrant` feature + `qdrant-client` dep gone. `docker-compose.yml`: qdrant service/profile/volume gone. `.env.example` / `config.example.toml`: qdrant blocks replaced with Postgres-memory equivalents. |
| `CLAUDE.md` | Was stale (pre-rework: "RAG = brain", Qdrant, auto-reindex). Rewritten to match `AGENTS.md` substance + new required-reading entry for the agentic-memory docs + corrected stuck-recipe |
| `AGENTS.md` | Fixed find-replace corruption — a blanket `Claude`→`Codex` had mangled code identifiers (`engine = "Codex"`, `Codex-sonnet-4-6`, `--features Codex`). Restored to `claude_code` / `claude-sonnet-4-6` / `--features claude_code`. `CLAUDE.md` + `AGENTS.md` are now twins |
| `docs/SESSION_HANDOFF.md` | Restored (this file) |

---

## Open items

### Phase 4–6 (per `docs/agentic-memory-implementation-2026-05-13.md`)

| Item | State |
|---|---|
| Phase 4 — wiki compiler | **Landed** (2026-05-14). `compile_wiki` LLM-synthesises a cited Markdown page from promoted memories; deterministic fallback when no LLM is available. Not yet exercised end-to-end against a real LLM — see verification block. |
| Phase 5 — MCP surface | **Landed** (2026-05-14). `tengu agentic-memory-server` is a standalone MCP stdio server exposing only `agentic_memory`; non-Tengu agents (ChatGPT/Codex/Claude) wire it into their MCP client config. Not yet exercised against a real external client — see verification block. |
| Phase 6 — migration cleanup | **Landed** (2026-05-14). Full Qdrant removal: `rag/` module orphaned, `qdrant` feature + `qdrant-client` dep gone, `compress_and_store` Qdrant plugin gone, `tengu registry` / `tengu memory inspect` CLIs gone, qdrant `VectorStore` orphaned, webhook persist migrated to `agentic_memory`. **Not compiled** — see verification block. Orphaned files (`src/adapters/rag/*.rs`, `src/application/memory/vector/qdrant.rs`) are still physically present — `git rm` them (the sandbox couldn't unlink). |

### Smaller gaps

| Item | Note |
|---|---|
| `lint` is shallow | Returns counts only — no duplicate / contradiction / stale-page / missing-citation detection (PRD R6). |
| `propose_behavior` | In the implementation-doc Tool API table; not in the operation enum. Deliberately deferred per the PRD "Correction" (MVP = capture/recall/ingest/promote/compile/lint). |
| `[agentic_memory]` config section | Implementation doc "Config Sketch" (enabled / raw_root / wiki_root / recall knobs) is **not** implemented. Plugin uses `TENGU_MEMORY_DATABASE_URL` env + hardcoded `RAW_ROOT`/`WIKI_ROOT` consts. |
| Graph tables | `memory_claims` / `memory_links` are "planned" in the doc; `ensure_schema` does not create them. Promotion → claim/link flow not built. |
| `capture` session scoping | **Closed 2026-09-12** — `capture` falls back to `TENGU_SESSION_ID` / `TENGU_AGENT_NAME` exported by the `run-agent` child. |
| Embedding dim coupling | Schema hardcodes `vector(1536)`; embedder pinned to `DEFAULT_EMBEDDING_MODEL` (`text-embedding-3-small`). Since 2026-09-12 a non-1536 vector warns and degrades to text-only on every path (event insert, hybrid recall, chunks). |
| Chunk embedding throughput | `insert_chunks` embeds chunks sequentially (one API call each). Batching is a follow-up for large sources. |
| Postgres inspect CLI | `tengu memory inspect` was removed with the Qdrant path. No Postgres-native "did the write land?" diagnostic yet — query the DB directly or run the `postgres_*_smoke` tests. |
| Orphaned files | **Closed 2026-09-12** — `git rm`'d. |
| Schema file | `.tengu/agentic-memory/AGENTS.md` (data-layers table) is not created or read by anything yet. |

---

## Phase 6 — completed (compile-verify still required)

Full Qdrant removal landed across ~10 files (see the "what landed" table). Key
notes for whoever runs the compiler next:

- **`rag/` is cleanly isolated** — it was already fully behind
  `#[cfg(feature = "qdrant")]`, so nothing outside it referenced `RagStore`
  except `webhook_builder`, `compress_and_store`, and `main.rs`'s CLI — all
  handled. A post-removal grep for `crate::adapters::rag` / `QdrantVectorStore`
  / `qdrant_client` in compiled code is clean.
- **The landmine, fixed** — `webhook_builder::persist_webhook_output` used
  `rag::RagStore` directly while gated only on `webhooks`, so
  `cargo build --features webhooks` never actually compiled. It now writes via
  `agentic_memory` under `#[cfg(feature = "postgres_memory")]`, with a
  graceful no-op (warn) when that feature is off.
- **Vestigial `MemoryConfig` qdrant fields and the orphaned `rag/` +
  `vector/qdrant.rs` files** — both removed 2026-09-12 (compiler-verified).

---

## Verify before declaring done (run on a machine with cargo + Docker)

```sh
# 1. Build — every feature combo must compile (Phase 6 removed the `qdrant`
#    feature entirely; `webhooks` was the latent-break combo).
cargo build                                        # default (openrouter, telegram)
cargo build --features postgres_memory             # new memory path
cargo build --features webhooks,postgres_memory    # the Phase 6 landmine combo
cargo build --features claude_code,postgres_memory # mixed-engine

# 2. Unit tests (no DB needed)
cargo test --features postgres_memory agentic_memory

# 3. Postgres + pgvector smoke (DB needed)
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo test --features postgres_memory postgres_capture_and_recall_smoke -- --ignored
cargo test --features postgres_memory postgres_vector_recall_smoke      -- --ignored

# 4. End-to-end — trace one turn, confirm recall block appears
#    sandbox config needs [memory] within_session_output_top_k = 3 (or similar)
RUST_LOG=tengu=info cargo run --release --features postgres_memory -- chat --sandbox aura

# 5. Wiki compiler (Phase 4) — via an agent opted into tools = ["agentic_memory"]:
#    capture + promote a memory, then run compile_wiki, then inspect the page.
#    agentic_memory(operation="capture", kind="preference", content="...")
#    agentic_memory(operation="promote", target_kind="event", target_id="<id>")
#    agentic_memory(operation="compile_wiki", title="...")
#    -> expect .tengu/agentic-memory/wiki/<slug>.md with inline [mem:...] cites
#       + a ## Sources footer. Tool result reports mode=llm (or mode=fallback
#       if OPENROUTER_API_KEY is unset — that path must still write a page).

# 6. Standalone MCP server (Phase 5) — handshake + tools/list over stdio
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory \
    cargo run --features postgres_memory -- agentic-memory-server
#    -> id=1: result.serverInfo.name == "tengu-agentic-memory"
#    -> id=2: result.tools[0].name == "agentic_memory"
#    (server exits cleanly when stdin closes; build a release binary for real use)
```

Watch for: `agentic_memory: ...` warn lines (embed/connection/LLM fail-soft),
`persisted user message to agentic_memory`, `agentic_memory: backstop wrote
final_text summary to Postgres`, and one `metrics` line with
`kind=wiki_compiler` per `compile_wiki` call. After ingesting a source,
confirm `memory_chunks.embedding` is non-null for at least one row.

---

## Active gotchas

The compiled gotcha list lives in `CLAUDE.md` / `AGENTS.md` ("Key gotchas").
Rework-specific call-outs:

- **`TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` are generated** — regenerated
  every planner turn / accepted plan. Now gitignored. Don't hand-edit; don't
  commit.
- **`postgres_memory` is off by default** — without it, the planner recall
  lanes compile to `String::new()` and `agentic_memory` is not registered.
  The harness still runs (file registry + in-memory history); it just has no
  durable cross-session memory.
- **`agentic_memory` module is fully feature-gated** — everything in
  `src/adapters/outbound/tools/agentic_memory/` only compiles under `postgres_memory`.
- **`tengu agentic-memory-server` reuses the `mcp-bridge` machinery** — it is
  NOT a Claude-specific protocol; `mcp_bridge.rs` implements standard MCP
  (JSON-RPC 2.0 stdio), it was just originally built for the `claude_code`
  engine. `run_mcp_bridge` and `run_agentic_memory_mcp_server` share
  `serve_mcp_stdio`; the bridge path is byte-identical post-refactor.
- **MCP clients replace the env, not extend it** — when an external client
  spawns `tengu agentic-memory-server`, only the keys in its `env` block are
  visible. Forward `OPENROUTER_API_KEY` alongside `TENGU_MEMORY_DATABASE_URL`
  or `recall`/`ingest_source`/`compile_wiki` silently run in their degraded
  fail-soft modes. Same trap `adapters/outbound/engines/claude_code.rs` documents for the bridge.
