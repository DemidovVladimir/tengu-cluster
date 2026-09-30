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
| `ErrorClass` | same | quota_exhausted · rate_limited · auth_required · timeout · transient · decode · not_applicable · fatal. HTTP status / transport → class: `outbound/http_class.rs` (shared by every HTTP client); class → retry / park / stop: `domain/backoff.rs::next_delay`; request budgets: `[rate_limits.<name>]` (`outbound/rate_limit.rs`) |

## Cache

| Piece | Rule |
|---|---|
| Port | `ports/observation.rs::ObservationStore` — `get` (`Err` on a row that does not parse), `get_many` (skips it), `put -> bool`, `put_if_unchanged(obs, expected_observed_at_ms) -> bool` (compare-and-swap; default impl never writes), `record(obs)` (history; default no-op) |
| Store | `outbound/observations.rs::SqliteObservationStore` → `<workspace>/.tengu/observations.db`, table `observations`, WAL + `busy_timeout` 5000, rows > 7 days purged on open. Opened only via `open_observation_store(workspace, &AgentConfig.sandbox)` (§ History recorder) |
| `put` | `Error` rows ignored; slot-monotonic (an older slot never overwrites; rows without a slot always replace). `put_if_unchanged`: one conditional statement, atomic across processes |
| `application/observe.rs::observe()` | fresh row (`status != error`, age ≤ min(ttl, `max_age_secs`)) ⇒ served with `source = cache`; else fetch, `record` it (every live result, `Error` and ttl-0 too), store if usable and ttl > 0. Store failure ⇒ warn + live read (doctrine #4) |
| `max_age_secs` | optional arg on every typed tool; `0` forces a live read |
| `acct/1:<pubkey>` | raw account rows (`outbound/solana/accounts.rs::fetch_accounts`, 60 s): reused when fresh, one `getMultipleAccounts` for the rest. **Phase-5 seam**: a stream writing these rows makes builders RPC-free |
| `dlmm_discovery/1:<wallet>:<pool>` | position discovery (gPA): 60 s found / 10 s empty, and an empty row is reused only within the caller's max age (the bot's 300 s is safe only for the process that opens the positions) / errors never stored. Position reads pinned ≥ the discovery's slot; a discovered key that does not value ⇒ exposure `Error`, never 0 (`outbound/solana/plan.rs`, `dlmm::flag_unvalued`) |
| `lp_state/1:<wallet>:<pool>` | controller state (regime, timers, re-entry anchor, last hedge action); 7 days; written only with `commit = true`, compare-and-swap on the version read (changed since ⇒ NOT committed, `features.commit = conflict`); unreadable ⇒ the decide tools block `invalid_read`, never overwrite |
| `loop/1:<loop>` | `tengu run` health of a decision loop (`domain/runtime.rs::LoopHealth`), in the loop agent's store every `[runtime] heartbeat_secs`: queue depth, in flight, counts, `last_decision_age_s` (`docs/runtime-2026-09-30.md`) |
| `feed/1:<feed>` | `tengu run` health of a feed (`FeedHealth`): state connecting \| live \| backoff \| stalled \| down, `required`, `last_item_age_s`, reconnects, dropped, `last_error_class`. `observed_at_ms` = the feed's last item, so `requires = { feed = N }` gates on data age; no row before the first item |

## History recorder (`ops-history-recorder`, 2026-09-30)

Append-only time series of observations (xmarket tracker conventions 3 + 5): research, replay, the weekend clock.

| Piece | Rule |
|---|---|
| Config | `[recorder]` (`config/recorder.rs`, `deny_unknown_fields`): `enabled` (false; needs `[xmarket]`), `schemas` (`"*"` = all), `keep_data` (schemas that keep `data`; others features only), `change_only` (true), `heartbeat_secs` (300; 0 = never), `min_interval_secs = { "<schema>" = secs }`, `retention_days` (30; 0 = keep). Resolved into `AgentConfig.sandbox` (`recorder`, `history_dir`) |
| Port | `ports/history.rs::HistoryStore` — `append(&[HistoryRow])`, `range(key, from_ms, to_ms)` (half-open, oldest first), `asof(keys, t_ms, max_age_ms)` (latest row ≤ t per key, `None` when older than `max_age_ms`). `HistoryRow` = envelope minus `tool`, `ttl_ms`, `headline`; `venue_ts_ms` = `features.venue_ts_ms` |
| Store | `outbound/history_sqlite.rs::SqliteHistoryStore` → `<TENGU_HOME>/state/<xmarket.state>/history/<YYYYMMDD>.db` (UTC day of `observed_at_ms`), table `obs_history(key, schema, observed_at_ms, venue_ts_ms, slot, source, status, errors, features, data)`, unique `(key, observed_at_ms)` (re-append = no-op), WAL (`synchronous = NORMAL`) + `busy_timeout` 5000. Day files older than `retention_days` deleted on open and at each new day file; rows that old are not appended |
| Feed | `outbound/observations.rs::RecordingObservationStore` wraps the cache: rows `put` / `put_if_unchanged` wrote + every live `observe()` result via `record`. The cache still never stores `Error` / ttl-0 rows |
| Filter (per key, per process) | schema listed · each `(key, observed_at_ms)` once (`record`, then `put`) · nothing within `min_interval_secs` · `change_only`: skip a row whose status, errors, features (minus `venue_ts_ms`) and kept `data` equal the last recorded row until that row is `heartbeat_secs` old |
| Constructor | `open_observation_store(workspace, &SandboxSections)` — the only way tool plugins (`tools/solana/mod.rs`) and decision loops (`bootstrap/decision.rs`) open the store; new tools call it too. History fails to open ⇒ warn, cache only |
| CLI | `tengu history range <key> --from <rfc3339 \| ms> --to <…>` · `tengu history asof <key>… --at <…> [--max-age-secs N]` (`--sandbox`) → JSON lines, full keys; reads only (creates / deletes nothing) |

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
| Signing sandbox (`config/solana.rs`, `config/hardening.rs` — the hardened rules a `[risk]` sandbox shares) | `claude_code` agents only with `builtin_tools_profile = "none"` (the CLI always runs `--strict-mcp-config`), no `[[mcp_servers]]`, no scope with `shell_bins` (fallback runs no shell), the key, `<TENGU_HOME>/state` and the config file outside every fs root / workspace; a wallet grant only on an agent with no `description`, not `default`, no webhook `agent`, never in `[default_scopes]`; a `read_only` write action must set `mode = "simulate"` |
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

Order books (`hl_book/1:<id>`, § Hyperliquid tools) walk through `domain/book.rs`; `rh_quote/1:<id>`, `rh_dex_quote/1:<id>` follow the same id rule. Venue decimal strings parse through `domain/market.rs::{parse_decimal, decimal_field}` (malformed ⇒ `decode` field error, `null` ⇒ absent, never 0).

| HL `POST /info` reply (`outbound/hyperliquid/info.rs`) | Class a tool records |
|---|---|
| `200 null` | none — `InfoReply::Null`: HL does not know the coin / user (row `absent`) |
| `500` + body `null` | `not_applicable` — unknown dex / coin, not an outage |
| `422` (`Failed to deserialize the JSON body …`) | `fatal` — bad request `type` / shape |
| `429` | `rate_limited` + `retry_after_ms`; the `[rate_limits.hyperliquid]` bucket is drained |
| `403` | `auth_required` — "blocked (geo/WAF/Tor exit?)" |
| other `5xx` · timeout · non-JSON `200` | `transient` · `timeout` · `decode` |

## Hyperliquid tools (`adapters/outbound/tools/hyperliquid/`, opt-in, one `hyperliquid` plugin)

Host `api.hyperliquid.xyz` (`$HL_API_URL` when the scope may read it: testnet `https://api.hyperliquid-testnet.xyz`). Every request goes through `HlInfo` (egress gate, `[rate_limits.hyperliquid]` weight, the reply table above). Decoders: `domain/hl/` (pure). Coins are HL names verbatim: `ETH`, `kPEPE` (default dex), `xyz:TSLA` (HIP-3), `@151` / `PURR/USDC` (spot); ids `hyperliquid:<coin>`.

| Tool | Args (* one of, ** required) | Returns | Reads (weight) | Writes (TTL) |
|---|---|---|---|---|
| `hl_ctx` | `coins`* (1–64; `#…` outcome refused), `dex`* (`""` / `"default"` = the default dex; ignored as `""` next to `coins`), `max_age_secs` | one coin: `mkt_ctx/1:hyperliquid:<coin>`; a dex or several coins: `hl_sweep/1` | one `metaAndAssetCtxs {dex}` per needed perp dex, concurrent (20 each); `spotMetaAndAssetCtxs` for spot coins (20); side rows below through the cache | every coin of each reply: `mkt_ctx/1` (5 s) + `mkt_instrument/1` (60 s); a requested coin HL lacks ⇒ `not_found` rows |
| `hl_book` | `coin`** (one HL name, verbatim), `notional_usd` (≤ 3 numbers > 0; default `[100, 1000, 10000]`, `[]` = none), `include_trades` (default false), `max_age_secs` | `hl_book/1:hyperliquid:<coin>` | `l2Book {coin}` (2; full precision, ≤ 20 levels a side); with `include_trades` also `recentTrades {coin}` (20 + 1 per 20 trades), concurrently | that row (2 s): levels + HL `time` in `data`; a fresh cached row is re-walked for the caller's notionals (no request) |

| Side row (cached, shared by every agent of the workspace) | TTL | Source (weight) | Feeds |
|---|---|---|---|
| `hl_perp_meta/1:hyperliquid` | 1 h; 60 s when a source failed | `perpDexs` + `perpCategories` (20 + 20); only when a HIP-3 dex is read | `asset_id` (`100000 + 10000·position + index`), `oi_cap_usd` (`assetToStreamingOiCap`), `category` (HIP-3 coins only; default-dex coins stay unknown) |
| `hl_at_oi_cap/1:hyperliquid:<dex label>` | 60 s | `perpsAtOpenInterestCap {dex}` (20) | `at_oi_cap` |

Budget: a live read of one HIP-3 dex = 20, + 20 per minute per dex (at-cap), + 40 per hour (meta); the default dex needs no meta. A failed side read leaves its field unknown and puts an error on every `mkt_instrument/1` row (`partial`); `taker_fee_bps` = `hl_fee_schedule` (tier / staking from `[paper]`, else tier 0) on USDC-quoted markets only.

| `hl_sweep/1` | Rule |
|---|---|
| Key | `hl_sweep/1:hyperliquid:<dex label>` (dex sweep, cached 5 s) · `hl_sweep/1:hyperliquid:<coin>,<coin>,…` (sorted, de-duplicated; ttl 0, rebuilt from the per-coin rows) |
| `data` | `scope`, `reads` (`info`, `dex`, `weight`, `failed` class), `rows_written`, `coins` (`coin`, `status`, `mark`, `basis_bps` / `funding_apr_pct` rounded to 0.1, flags `delisted` / `not_found` / `no_book` / `at_oi_cap`), `errors` — 10.7 KB for the 128-market xyz dex; local engines get the store pointer instead (< 600 chars in all) |
| Features (14) | `n_coins`, `n_ok`, `n_partial`, `n_absent`, `n_error`, `n_delisted`, `n_not_found`, `n_no_book`, `n_at_oi_cap`, `n_reads`, `n_failed_reads`, `weight`, `rows_written`, `from_cache` |
| Status | no coin or every coin `error` ⇒ `error` (a `not_applicable` unknown dex ⇒ `absent`) · every coin absent ⇒ `absent` · a coin `error` / `partial` or a failed side read ⇒ `partial` · else `ok` |

| `hl_book/1` | Rule (`domain/hl/book.rs`; walks and depth only through `domain/book.rs`) |
|---|---|
| Status | `absent`: HL does not know the coin (`200 null` / `500 null`) or `book_empty` (both sides empty: delisted / halted) · `error`: the read failed or the reply did not decode (crossed / unordered levels, bad decimals) — class as in the reply table above, never stored · `partial`: one side empty, or the asked `recentTrades` read failed (error field `last`) · else `ok` |
| Features (≤ 27) | `bid`, `ask`, `mid`, `spread_bps` · `depth_usd_{10,50}bps_{bid,ask}` (resting within N bps of mid, visible levels only: equal to `depth_usd_<side>` ⇒ a lower bound) · `imbalance_10bps` = (bid − ask) / (bid + ask) · `depth_usd_{bid,ask}`, `{bid,ask}_levels` (all visible) · `notional_usd_k` + `buy_slip_bps_k` / `sell_slip_bps_k` (k = 1..3: taker VWAP vs mid of a walk that fills the notional; beyond the visible depth ⇒ omitted, never a partial number) · `book_empty` · `venue_ts_ms`, `book_age_ms` · `last`, `last_age_s` (`include_trades`) |
| Paper fills | `tools/hyperliquid/book.rs::fresh_book(hl, store, id, now_ms) -> Result<(Observation, L2Book), ReadError>`: `l2Book` now (never the cache), recorded + stored with the default notionals; `Err` = no book (`not_applicable`: not HL / unknown coin; else the read's class) — the live `BookSource` of `ports/book.rs` (`risk-gate-enforcement`) |

Scope per tool: `fs_roots` = the workspace (store), `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]` (example: `config.example.toml`).

## Risk + paper rows (xmarket, 2026-09-30)

Operator reference: [`xmarket-risk-paper-2026-09-30.md`](xmarket-risk-paper-2026-09-30.md) (gate rules, ledger, halts, `tengu risk`). Subject = the ledger account (`[risk] account`, `[A-Za-z0-9._-]`), in full.

| Key | Written by | Row | Status |
|---|---|---|---|
| `paper_positions/1:<account>` | `paper_positions` (opt-in, `xm` plugin, TTL 2 s); math `domain/xm/ledger.rs::PaperPositions` | features `n_positions`, `cash_usd`, `equity_usd`, `upnl_usd`, `rpnl_usd`, `fees_usd`, `funding_usd`, `gross_exposure_usd`, `net_exposure_usd`, `leverage`, `daily_pnl_usd`, `marks_stale`, `n_marks_stale`, `halted`; per-position rows (full ids, qty, entry, mark, notional, uPnL, `exit_at_ms`) in `data` | a failed / stale mark ⇒ `partial`, the numbers it feeds omitted, never 0 |
| `paper_close/1:<account>:<client_order_id>` | `paper_close` with `all = true`; TTL 0; `domain/xm/exec.rs::PaperCloseAll` | features `positions`, `filled`, `partial`, `rejected`, `denied`, `error`; `data` = one leg per open position (full id, leg id `<client_order_id>:<instrument>`, status, rule, filled qty, avg px, error) | `ok` every leg filled (or nothing open) · `partial` some · `error` none; each leg also has its own `paper_fill/1` row |
| `paper_fill/1:<account>:<client_order_id>` | the exec tools through `tools/xm/exec_common.rs::run_exec` (`risk-gate-enforcement`); TTL 0 (recorded, never cached); `domain/xm/exec.rs::PaperFillRow` | features `status` (`filled` · `partial` · `rejected` · `denied`), `risk` (`allow` · `deny`), `risk_rule`, `class`, `degraded`, `replayed`, `side`, `reduce_only`, `intent_qty`, `intent_notional_usd`, `filled_qty`, `filled_notional_usd`, `avg_px`, `mid`, `slippage_bps`, `fee_usd`, `levels_used`, `reason`, `latency_ms`, `book_age_ms`, `position_qty_after`, `equity_usd_after`, `exit_at_ms`; `data` = the gate summary (first failed check, trips, verdict row id), the judged intent, the fill (levels) | `ok` filled · `partial` · `error` denied (`errors`: `risk`) or rejected by the venue (`errors`: `fill`); fill numbers only when an order was sent, never 0 for a failed read |
| `risk_state/1:<account>` | `risk_status` (opt-in, `xm` plugin, TTL 2 s); `domain/xm/risk_state.rs::RiskStatus` | features `halted`, `reason` (`daily_loss` · `total_loss` · `operator` · `file`), `kill_switch`, `equity_usd`, `cash_usd`, `daily_pnl_usd`, `total_pnl_usd`, `loss_headroom_usd`, `day_start_equity_usd`, `gross_exposure_usd`, `net_exposure_usd`, `leverage`, `n_positions`, `marks_stale`, `orders_last_min`, `open_orders`; `data` = the stored risk state + the valued account | a missing mark, day start or kill-switch state ⇒ `partial`, numbers omitted; an unreadable kill-switch file ⇒ `halted` |
| `xm_exits/1:<account>` | `xm_exits` (exec, `x-exit-rules`); TTL 0; `domain/xm/exits.rs::XmExits` | features `n_open`, `n_due`, `n_closed`, `n_failed`, `n_stale_marks`; `data` = every open position (full id, qty, entry, `opened_ms`, `exit_at_ms`, mark, P&L bps, reason `deadline` · `max_hold` · `stop_loss` · `take_profit` or none, status, exit id + attempt, gate rule, fill) | `ok` nothing failed · `partial` a close failed or a mark was missing / stale (TP / SL not judged, the mark in `errors`) · `error` every due close failed (`errors`: `exit:<id>`) |

| Tool | Args | Returns | Reads | Writes |
|---|---|---|---|---|
| `risk_status` | none | `risk_state/1:<account>` | `<xm_state_dir>/ledger.db` (account opened on first use), `mkt_ctx/1:<id>` rows of the open positions from the store (never fetched; older than `[risk] max_data_age_ms.ctx` ⇒ stale), `kill_switch_file` | the ledger's `risk_state` (UTC day roll + trips, one transaction) and the row (2 s) |
| `paper_order` (exec) | `instrument`* (full id), `side`* buy / sell, `notional_usd`*, `kind`* market / limit, `limit_px` (limit only), `tif` ioc, `reduce_only`, `max_slippage_bps`*, `strategy` (§21 type), `hedge_instrument`, `opportunity` (row key), `client_order_id`, `exit_at_ms` | `paper_fill/1:<account>:<client_order_id>` | through `run_exec`: `mkt_ctx/1` + `mkt_instrument/1` rows (never fetched), the `opportunity` row, a live `l2Book` after the `[paper]` latency, `kill_switch_file` | one ledger transaction: verdict (+ order, fill, position, cash when allowed), the verdict then mirrored as a `<TENGU_HOME>/logs/risk.jsonl` line; the `hl_book/1` row; funding owed first |
| `paper_close` (exec) | `instrument` or `all = true`; `max_slippage_bps`*; `client_order_id` | `paper_fill/1` · `paper_close/1` (all) | as `paper_order`; a reduce-only market IOC of the whole position (exits pass halted / degraded under `allow_reduce_degraded`) | as `paper_order`, per position |
| `paper_positions` | `account` (default `[risk] account`) | `paper_positions/1:<account>` | the ledger, `mkt_ctx/1` rows of the open positions (never fetched) | funding owed at a fresh rate + oracle (every due hour, `ledger.accrue_funding`); the row (2 s) |
| `xm_exits` (exec) | `max_slippage_bps` (default `[risk] max_slippage_bps`) | `xm_exits/1:<account>` | the ledger (positions, exit deadlines), `mkt_ctx/1` rows of the open positions (never fetched), `[risk.exits]`; per due position what `paper_close` reads | per due position one `run_exec` close under `exit:<account>:<instrument>:<reason>:<opened_ms>` (`…:<n>` once that id is stored and the position still open) |

Exec tools (`paper_order`, `paper_close`, `xm_exits`) run only for a private agent (no `description`, not `default`, no webhook `agent` — load rule and call-time check); scope `fs_roots` = the workspace, `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]`. Arguments parse strictly (an unknown key is an error). Operator reference: [`xmarket-risk-paper-2026-09-30.md`](xmarket-risk-paper-2026-09-30.md) § Exec tools.

## Paper fills (xmarket `risk-paper-fill-engine`, 2026-09-30)

Engine `domain/xm/paper.rs::simulate_fill` (pure) · latency `application/paper.rs::fill_with_latency` · book port `ports/book.rs::BookSource` (tracker convention 16) · live source `tools/hyperliquid/book.rs::HlBookSource` and the gate + fill + ledger path `tools/xm/exec_common.rs::run_exec` (`risk-gate-enforcement`, row `paper_fill/1:<account>:<client_order_id>`); the replay source lands with `ops-replay-harness`. Here until `risk-docs` moves it into [`xmarket-risk-paper-2026-09-30.md`](xmarket-risk-paper-2026-09-30.md).

| Piece | Rule |
|---|---|
| Order | `PaperOrder {client_order_id, instrument (full id), side, size {qty \| notional_usd}, kind market \| limit, tif ioc, limit_px (limit only), reduce_only, max_slippage_bps, ref_mid?}` — GTC / ALO P1; AMM / RFQ venues `rh-paper-fill` (M6) |
| Inputs | `VenueRules {kind perp \| spot, sz_decimals, min_notional_usd (HL 10), oracle_band {oracle_px, max_bps}?, at_oi_cap?}` · `MarketStatus` open \| halted \| closed \| delisted · the position · `FeeSchedule` · `[risk] max_data_age_ms.book` |
| Latency | `[paper] latency_ms ± latency_jitter_ms`, uniform in an injected `rand01`: sleep on the `Clock`, THEN `fresh_book`, THEN fill THAT book (the market moves meanwhile); `ref_mid` = the pre-latency price anchor. `[paper] order_types` gates `market` / `ioc`. A failed read = `Err`, nothing filled |
| Bound, size | market = ref ± `max_slippage_bps` on the HL tick grid (buy rounded down, sell up); limit = the tighter of `limit_px` and that. Notional ⇒ size = notional / ref, rounded down to `szDecimals`; ref = `ref_mid`, else the book mid |
| Fill | `domain/book.rs` walk to the bound, taker fee ⇒ `filled` · `partial` (`bound` / `depth` — never hidden liquidity) · `rejected`; missing numbers `None`, never 0 |
| Rejections, first failing check | `invalid_order` · `market_halted` / `market_closed` / `delisted` · `stale_book` / `bad_book` · `missing:mid` · `Tick` · `Oracle` / `missing:oracle` · `ReduceOnly` · `MinTradeNtl` (size × bound < $10; a reduce-only whole-position close is exempt) · `PositionIncreaseAtOpenInterestCap` / `PositionFlipAtOpenInterestCap` / `missing:at_oi_cap` · `MarketOrderNoLiquidity` (market) / `IocCancel` (limit). HL codes verbatim; `message` leads with HL's documented error string |
| Ledger | `FillResult::ledger_fill(underlying, ts)` ⇒ `xm/ledger.rs::Fill` (qty, VWAP, fee) |
| Unconfirmed until a testnet fill (M3b) | a market order that takes nothing = `MarketOrderNoLiquidity`; reduce-only closes under $10 accepted; the oracle band width is an input (HL documents none) |
| Goldens | `tests/fixtures/xm/meta.json` `fills` (bc on `l2_xyz_TSLA.json`) |

## Review fixes (2026-09-25)

| Rule | Behaviour |
|---|---|
| No-LP grace (bot BUG-011) | clock starts at the first no-LP read (`lp_state.no_lp_since_ms`); a fresh/missing state holds for `no_lp_grace_ms`; position observations are recorded before any gate and persisted with `commit = true` even on gated evaluations |
| Decision-loop `FromHistory` slots | read only the CURRENT event's history entries; `state.history` still shows earlier events as context |
| Slot candidate descriptions | never cut inside a value: whole trailing fields are dropped to fit 300 chars; the slot's `value` field is always kept |

