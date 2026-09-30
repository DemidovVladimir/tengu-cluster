# Typed observations + Solana LP tools (2026-09-24)

Typed tool results that one envelope serves to the LLM (text), decision loops (`features`) and a TTL cache. First users: 10 Solana LP read tools and 5 write tools (§ Write tools). Branch `feature/decision-loop`. Open items: `docs/SESSION_HANDOFF.md`. Loop config: `docs/decision-loop-plan-2026-09-24.md`.

## Envelope

| Piece | Where | Rule |
|---|---|---|
| `ToolOutput { text, observation }` | `ports/tool.rs` | `None` = legacy text tool; `ToolOutput::observed(obs, now)` sets `text = obs.render_text(now)` |
| `ToolExecutor::execute_typed` | `ports/engine.rs` | default wraps `execute` (no observation); overridden by `PluginToolExecutor` (passes it through) and `SanitizedToolExecutor` (redacts headline, `errors[].message`, string features, `data`) |
| `Observation` | `domain/observation.rs` | `key = <schema>:<subject>` (full ids joined by `:`), `schema = name/N`, `tool`, `observed_at_ms`, `slot?`, `ttl_ms`, `source` live \| cache \| stream, `status`, `errors[]`, `headline`, `features`, `data` (typed payload) |
| `features` | same | ≤ 32 keys (`MAX_FEATURES`); number \| bool \| string ≤ 64 chars; missing = omitted, never 0; ids go in `data` |
| `render_text` | same | line 1 = `{headline} \| {status} {age}s slot={slot} {source}`, ≤ 200 chars with full ids; too long ⇒ suffix moves to line 2 (ids never cut); then features, one line per error, `data` (omitted whole > 16 000 chars) |
| `compact_text(text)` | same | local engines only (`Engine::tool_result_char_cap`, `chat/tool_loop.rs::fit_tool_result`, also in `run-agent`): the tool's text with the `data` line → `data: <n> bytes in observation <key>` (full key); line 1, features, errors and appended notes (`lp_decide` commit) unchanged; text without that data line passes through. OpenRouter / Claude Code / decision loops see `render_text` as before |
| `Field<T>` / `ObsStatus` | same | field = ok \| absent \| error; row = ok \| partial \| absent \| error. Failed reads never become 0 (bot BUG-023) |
| `ErrorClass` | same | quota_exhausted · rate_limited · auth_required · timeout · transient · decode · not_applicable · fatal |

## Cache

| Piece | Rule |
|---|---|
| Port | `ports/observation.rs::ObservationStore` — `get` (`Err` on a row that does not parse), `get_many` (skips it), `put -> bool`, `put_if_unchanged(obs, expected_observed_at_ms) -> bool` (compare-and-swap; default impl never writes) |
| Store | `outbound/observations.rs::SqliteObservationStore` → `<workspace>/.tengu/observations.db`, table `observations`, WAL + `busy_timeout` 5000, rows > 7 days purged on open |
| `put` | `Error` rows ignored; slot-monotonic (an older slot never overwrites; rows without a slot always replace). `put_if_unchanged`: one conditional statement, atomic across processes |
| `application/observe.rs::observe()` | fresh row (`status != error`, age ≤ min(ttl, `max_age_secs`)) ⇒ served with `source = cache`; else fetch, store if usable and ttl > 0. Store failure ⇒ warn + live read (doctrine #4) |
| `max_age_secs` | optional arg on every typed tool; `0` forces a live read |
| `acct/1:<pubkey>` | raw account rows (`outbound/solana/accounts.rs::fetch_accounts`, 60 s): reused when fresh, one `getMultipleAccounts` for the rest. **Phase-5 seam**: a stream writing these rows makes builders RPC-free |
| `dlmm_discovery/1:<wallet>:<pool>` | position discovery (gPA): 60 s found / 10 s empty, and an empty row is reused only within the caller's max age (the bot's 300 s is safe only for the process that opens the positions) / errors never stored. Position reads pinned ≥ the discovery's slot; a discovered key that does not value ⇒ exposure `Error`, never 0 (`outbound/solana/plan.rs`, `dlmm::flag_unvalued`) |
| `lp_state/1:<wallet>:<pool>` | controller state (regime, timers, re-entry anchor, last hedge action); 7 days; written only with `commit = true`, compare-and-swap on the version read (changed since ⇒ NOT committed, `features.commit = conflict`); unreadable ⇒ the decide tools block `invalid_read`, never overwrite |

## Decision-loop use (`config/decision_loop.rs`, `application/decision_loop/`)

| Field | Effect |
|---|---|
| `world = { alias = "<key>" }`, `world_max_age_secs` (30) | read from the store every step, never fetched; stale / missing / error entries carry no numbers (`world.rs`) |
| action `requires = { alias = secs }` | action offered only while each alias is usable and that fresh |
| slot `{ observation = alias, items, value, top }` | candidates from a fresh world entry (`FromObservation`) |
| typed result | history gets `decision_value` (or the reducer over `decision_root`: `/data/...`, `/features/...`), `ok = status != error`, `obs = {key, status, source, age_s, slot}` |
| executor | loop tools run through `SanitizedToolExecutor` with the process `SecretRegistry` (`bootstrap/decision.rs`) |

## Solana tools (`adapters/outbound/tools/solana/`, opt-in, one `solana` plugin)

RPC = `$SOLANA_RPC_URL` if the scope may read it, else `https://api.mainnet-beta.solana.com`; rendered as host only (the URL may hold a key).

| Tool | Args (* required) | Key | TTL | Sources | Hosts |
|---|---|---|---|---|---|
| `sol_price` | `mint` (wSOL), `pool`, `pyth_feed_id` | `price_oracle/1:<mint>`; with `pool`: `price_oracle/1:<mint>:<pool>` | 10 s | Jupiter price v3; Pyth only with `pyth_feed_id`; pool active price via RPC | `lite-api.jup.ag` (+ `hermes.pyth.network`, + RPC) |
| `dlmm_pools` | `query`*, `limit` 1-50 (10), `min_tvl_usd`, `sort` fee_tvl_24h \| tvl \| volume_24h | `dlmm_pools/1:<query>\|<sort>\|<limit>\|<min_tvl>` | 60 s | datapi `/pools` page 100; `apr` = daily fee/TVL % ⇒ `fee_tvl_24h_pct` | `dlmm.datapi.meteora.ag` |
| `dlmm_pool` | `pool`* | `dlmm_pool/1:<pool>` | 5 s | LbPair, then mints + reserves + bin arrays (active ± 50) | RPC |
| `dlmm_positions` | `wallet`*, `pool`*, `positions`, `min_context_slot` | `dlmm_positions/1:<wallet>:<pool>` | 10 s (explicit `positions` ⇒ not stored) | discovery + the same two reads + position bin arrays | RPC |
| `jup_perps` | `wallet`* | `jup_perps/1:<wallet>` | 5 s | long/short PDAs, SOL + USDC custody, JLP pool; oracle = price row ≤ 30 s else Jupiter inline | RPC, `lite-api.jup.ag` |
| `solana_wallet` | `wallet`*, `mints` (wSOL + USDC, ≤ 32) | `solana_wallet/1:<wallet>` | 5 s | `getBalance`, `getTokenAccountsByOwner` × 2 programs, mints + ATAs | RPC |
| `solana_tx` | `signature`* | `solana_tx/1:<signature>` | 2 s; 1 day once finalized | `getSignatureStatuses` (history), `getTransaction` | RPC |
| `lp_snapshot` | `wallet`*, `pool`*, `positions`, `min_context_slot` | `lp_snapshot/1:<wallet>:<pool>` | 10 s (explicit `positions` ⇒ not stored) | ONE planned read (`plan::read_pool`: pool + positions + perps + wallet keys) + `price_oracle/1:<base mint>` row (≤ 10 s and ≤ the caller's max age, so snapshot + price age fits `max_snapshot_age_secs`; else Jupiter inline + row written) + pending keeper request from `lp_state` | RPC, `lite-api.jup.ag` |
| `hedge_decide` | `wallet`*, `pool`*, `knobs`*, `lp_knobs` (optional, = `lp_decide`'s knobs), `commit` (false) | `hedge_decide/1:<wallet>:<pool>` | never cached | store only: `lp_snapshot` + `lp_state` (+ price samples when `lp_knobs` given: storm latch / imbalance freeze computed this cycle); missing / stale / explicit-`positions` (`args`) snapshot ⇒ action `blocked`, guard `stale_input`; unreadable `lp_state`, any `positions` / `discovery` read error, or discovery count ≠ valued positions ⇒ guard `invalid_read` | none |
| `lp_decide` | `wallet`*, `pool`*, `knobs`*, `commit` (false) | `lp_decide/1:<wallet>:<pool>` | never cached | store only: + `price_oracle` samples; missing / stale / `args` snapshot ⇒ verdict `blocked`; unreadable `lp_state` or position / discovery read errors ⇒ `paused` `invalid_read` | none |

Every Solana tool's first check is `ctx.scope.check_fs_write(workspace)` (the store), so its scope needs `fs_roots` = the workspace; `net_hosts` as above; `env_reads = ["SOLANA_RPC_URL"]` or the public RPC is used silently. Working scopes: `sandboxes/lping/config.toml`.

## Knobs (all REQUIRED, `deny_unknown_fields`, no defaults)

| Tool | Knobs | Production values |
|---|---|---|
| `hedge_decide` | `target_delta_sol`, `delta_threshold_sol`, `band_bins`, `bin_count`, `cap_mult`, `max_notional_usd`, `min_collateral_ratio`, `target_collateral_ratio`, `carry_cap_bps`, `cooldown_ms`, `lp_input` (live \| midpoint), `include_wallet_sol`, `min_wallet_sol`, `rent_reserve_sol`, `max_divergence_bps`, `max_snapshot_age_secs`, `trend_confirm_ms`, `no_lp_grace_ms` | `[decision_loops.hedge_watch]` in `sandboxes/lping/config.toml` (sources cited in TOML comments) |
| `lp_decide` | `imbalance_threshold`, `bin_count`, `storm_pct_5m`, `trend_confirm_ms`, `reentry_confirm_ms`, `reentry_tol_frac`, `max_divergence_bps`, `max_snapshot_age_secs`, `min_wallet_sol`, `rent_reserve_sol` | same file, `decide_lp` action |

## Pure policy (`domain/lp/`, no IO, `now_ms` is an input)

| File | Holds | Proof |
|---|---|---|
| `hedge.rs` | port of the bot hedge controller (`simulator/src/hedge.rs` ← `hedgeController.ts:94-375`) + `auto_band_sol` | 1027/1027 production vectors, `cargo test --bin tengu lp::hedge` |
| `gates.rs` | re-entry gate, storm hysteresis, trend + regime confirm, composition / imbalance, wallet 50/50, bin math (70-bin cap), DLMM fee rate, swap oracle gate | ported bot tests |
| `snapshot.rs` | `compose_snapshot`, `decide_hedge` / `decide_lp` (gates first, then `hedge::decide` unchanged), `LpControllerState` | gate tables |
| `dlmm.rs`, `perps.rs`, `wallet.rs`, `market.rs` | decoders + builders over one `AccountSet` / fetched JSON | SDK / Anchor / live-fixture goldens |

## Write tools (phase 6b, 2026-09-29)

Opt-in, one catalog row each. Runner `tools/solana/write_common.rs`; pipeline `outbound/solana/send.rs`; encoders `domain/solana_tx.rs`, `domain/lp/{dlmm_ix,perps_ix}.rs`. Result = `write/1:<tool>:<wallet>:<started_ms>` (`domain/solana_write.rs`, ttl 0 — never cached). Every tool: `mode` = `simulate` (default: keyless simulation as the wallet) \| `send`. Reads are live, never cached.

| Tool | Args (* required) | Builds | Refuses when |
|---|---|---|---|
| `solana_close_token_accounts` | `wallet`*, `keep_mints` | SPL `CloseAccount`, 8 / tx, independent batches | — (`noop` when nothing is empty) |
| `jupiter_swap` | `wallet`*, `input_mint`*, `output_mint`*, `amount`*, `oracle_gate_bps`* | Ultra `/order` → simulate as-is → sign our slot → `/execute` | send on a non-SOL↔USDC pair; worst fill (`otherAmountThreshold`) vs oracle > gate or no oracle; gasless order; open keeper request |
| `dlmm_open_position` | `wallet`*, `pool`*, `amount_x`*, `amount_y`*, `bin_count`* (≤ 70), `strategy`*, `max_active_bin_slippage`*, `min_wallet_sol`*, `max_new_bin_arrays`* (≤ 2), `max_divergence_bps`*, `allow_existing` | missing bin arrays → `initialize_position` → ATAs → wrap → `add_liquidity_by_strategy2` → unwrap | a position already in the pool; SOL < legs + rent + 0.005 + `min_wallet_sol`; token leg > ATA; pool vs oracle divergence; Token-2022; too many new bin arrays |
| `dlmm_close_position` | `wallet`*, `pool`*, `position`*, `arm_reentry`* | remove → claim fee → claim rewards → `close_position_if_empty` → unwrap; ≤ 70-bin chunks, split when > 1232 B | not the owner / other pool / foreign fee owner / Token-2022 |
| `jup_perps_order` | `wallet`*, `pool`*, `side`*, `action`* (increase \| decrease \| close), `size_usd`, `collateral`, `slippage_bps`*, `max_notional_usd`* | keeper market request (long: wrap / wSOL ATA kept open) | no oracle; open or unreadable keeper request; no position (decrease / close); post-order size > cap; funds |

| Send rule | Detail |
|---|---|
| Signer | `[solana] signer_key_file` (0600; solana-keygen JSON or base58; errors never echo content) + the tool's scope `wallets = ["<full pubkey>"]` on ONE agent + key = `wallet` |
| Signing sandbox (`config/solana.rs`, `config/hardening.rs`) | `claude_code` agents only with `builtin_tools_profile = "none"` (the CLI always runs `--strict-mcp-config`), no `[[mcp_servers]]`, no scope with `shell_bins` (fallback runs no shell), key outside every fs root / workspace; a wallet grant only on an agent with no `description`, not `default`, no webhook `agent`, never in `[default_scopes]`; a `read_only` write action must set `mode = "simulate"` |
| Lease | `<TENGU_HOME>/state/solana-writes.db`, `wallet:<address>`, 150 s, renewed per tx; held ⇒ `lease_held`. The TS bot is invisible to it — never sign with a wallet the bot runs |
| Pending record | written before submit; the next send resolves it first: landed ⇒ fence, expired ⇒ cleared, in flight ⇒ refused `pending_unresolved` |
| Own transactions | simulate (fail / no units ⇒ never sent) → CU `min(1.4M, ⌈units × 1.1⌉)` → price (Helius estimate on a `helius` host, clamp [1 000, 5 000 000] µL/CU, else 1 000) → `sendTransaction` once; a JSON-RPC error = not sent; a transport failure ⇒ one resend of the same bytes |
| Jupiter swap | `/execute` re-POSTed with the same body within Jupiter's 2-minute idempotency window; codes −1/−2/−3/−1002/−1003/−1004 = not sent; else polled |
| Confirm | `getSignatureStatuses` until confirmed; block height > `lastValidBlockHeight` + a history lookup ⇒ `expired`; 120 s ⇒ `unconfirmed` (record kept) |
| After landing | fence = landing slot; stale cache rows removed; `lp_snapshot` pins reads to the fence; decide tools treat an older snapshot as `stale_input`; `merge_lp_state` (CAS × 3): close + `arm_reentry` ⇒ `reentry`, perps ⇒ `last_hedge_action` (the `PendingRequest` guard = request-aware cooldown) |
| Keeper requests | unexecuted and ≤ 300 s old (or unreadable) ⇒ DLMM tools keep the wSOL account open; perps orders and SOL-leg swaps refuse |

| `WriteStatus` | Observation |
|---|---|
| `simulated` / `confirmed` / `noop` | ok |
| `unconfirmed` / `partial` | partial — do not retry blindly |
| `refused` / `sim_failed` / `failed` / `expired` | error |

Verified: goldens vs web3.js / spl-token / Meteora SDK 1.9.7 / anchor 0.29 (`tests/fixtures/solana/tx/golden.json`; generator not kept — Rust-only repo); fake-cluster pipeline tests; live keyless mainnet simulations (`cargo test --bin tengu -- --ignored live_`): Ultra swap, DLMM open, DLMM close of a real 46-bin position, perps short increase. No live `send` yet.

## Market rows (xmarket, `domain/market.rs`, 2026-09-30)

Instrument id = `<venue>:<native id verbatim>` (`docs/xmarket-tracker-2026-09-29.md` conventions 1–2): venues `hyperliquid`, `robinhood`, `binance-spot`, `binance-usdm`, `bybit-spot`, `bybit-linear`, `okx-spot`, `okx-swap`, `coinbase`, `coinbase-intx`, `ref:<MIC>`. Never shortened: `hyperliquid:xyz:TSLA`, `hyperliquid:@151`, `robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d`, `ref:XNAS:TSLA` (`InstrumentId` rejects unknown venues).

| Key | Row | Status |
|---|---|---|
| `mkt_instrument/1:<id>` | `kind` perp \| spot \| outcome · `listing` listed \| delisted \| not_found · `dex`, `asset_id`, `category` (stocks · etf · indices · commodities · fx · rates · preipo · crypto; HL `stock` / `FX` folded, unknown ⇒ none) · `quote_ccy` USDC \| USDT \| USDH \| USDE \| USD · `sz_decimals`, `max_leverage`, `margin_mode` normal \| no_cross \| strict_isolated, `only_isolated`, `oi_cap_usd`, `at_oi_cap`, `deployer_fee_scale`, `growth_mode` · `underlying {listing, ticker, ratio (decimal string), fx_converted}` | not_found ⇒ absent · a failed source (`errors`, field left unknown) ⇒ partial |
| `mkt_ctx/1:<id>` | `Field<f64>` mark, oracle, index, mid, bid, ask, last, impact_bid / impact_ask, prev_day, premium, funding_1h (per hour), oi_base, vol_24h_usd · funding_interval_h, next_funding_ms · instrument facts copied from `mkt_instrument/1` · `no_book` | delisted / not_found (HL `200 null`) ⇒ absent · no price and a failed read ⇒ error · a failed field or `no_book` (HL null premium / midPx / impactPxs) ⇒ partial |

| `mkt_ctx/1` feature (31 keys) | Rule — an input missing ⇒ key omitted, never 0 |
|---|---|
| `mark` … `last`, `impact_bid` / `impact_ask`, `funding_1h`, `oi_base`, `vol_24h_usd` | field values |
| `basis_bps` | (mark − ref) / ref · 1e4, ref = oracle, else index |
| `spread_bps` / `impact_spread_bps` | (ask − bid) / mid · 1e4 |
| `premium_bps` · `funding_apr_pct` · `next_funding_s` | premium · 1e4 · funding_1h · 8760 · 100 · next_funding_ms − as_of_ms |
| `oi_usd` · `oi_cap_used_pct` · `change_24h_pct` | oi_base · mark · oi_usd / oi_cap_usd · 100 · (mark / prev_day − 1) · 100 |
| `oracle_eq_mark` | mark = oracle (HL: no book, off-hours, delisted) |
| `max_leverage`, `only_isolated`, `delisted`, `session`, `category`, `growth_mode`, `taker_fee_bps`, `at_oi_cap` | instrument / calendar facts |

Order books (`hl_book/1:<id>`) live in `domain/book.rs`; `rh_quote/1:<id>`, `rh_dex_quote/1:<id>` follow the same id rule.

## Review fixes (2026-09-25)

| Rule | Behaviour |
|---|---|
| No-LP grace (bot BUG-011) | clock starts at the first no-LP read (`lp_state.no_lp_since_ms`); a fresh/missing state holds for `no_lp_grace_ms`; position observations are recorded before any gate and persisted with `commit = true` even on gated evaluations |
| Decision-loop `FromHistory` slots | read only the CURRENT event's history entries; `state.history` still shows earlier events as context |
| Slot candidate descriptions | never cut inside a value: whole trailing fields are dropped to fit 300 chars; the slot's `value` field is always kept |

