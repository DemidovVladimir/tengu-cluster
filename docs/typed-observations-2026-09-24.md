# Typed observations + Solana LP tools (2026-09-24)

Typed tool results that one envelope serves to the LLM (text), decision loops (`features`) and a TTL cache. First users: 10 Solana LP tools. Branch `feature/decision-loop`. Open items: `docs/SESSION_HANDOFF.md`. Loop config: `docs/decision-loop-plan-2026-09-24.md`.

## Envelope

| Piece | Where | Rule |
|---|---|---|
| `ToolOutput { text, observation }` | `ports/tool.rs` | `None` = legacy text tool; `ToolOutput::observed(obs, now)` sets `text = obs.render_text(now)` |
| `ToolExecutor::execute_typed` | `ports/engine.rs` | default wraps `execute` (no observation); overridden by `PluginToolExecutor` (passes it through) and `SanitizedToolExecutor` (redacts headline, `errors[].message`, string features, `data`) |
| `Observation` | `domain/observation.rs` | `key = <schema>:<subject>` (full ids joined by `:`), `schema = name/N`, `tool`, `observed_at_ms`, `slot?`, `ttl_ms`, `source` live \| cache \| stream, `status`, `errors[]`, `headline`, `features`, `data` (typed payload) |
| `features` | same | ≤ 32 keys (`MAX_FEATURES`); number \| bool \| string ≤ 64 chars; missing = omitted, never 0; ids go in `data` |
| `render_text` | same | line 1 = `{headline} \| {status} {age}s slot={slot} {source}`, ≤ 200 chars with full ids; too long ⇒ suffix moves to line 2 (ids never cut); then features, one line per error, `data` (omitted whole > 16 000 chars) |
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
| `hedge_decide` | `wallet`*, `pool`*, `knobs`*, `commit` (false) | `hedge_decide/1:<wallet>:<pool>` | never cached | store only: `lp_snapshot` + `lp_state`; missing / stale / explicit-`positions` (`args`) snapshot ⇒ action `blocked`, guard `stale_input`; unreadable `lp_state` ⇒ guard `invalid_read` | none |
| `lp_decide` | same | `lp_decide/1:<wallet>:<pool>` | never cached | store only: + `price_oracle` samples; missing / stale / `args` snapshot ⇒ verdict `blocked`; unreadable `lp_state` ⇒ `paused` `invalid_read` | none |

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

## Phase 6 seam (spec only)

`LeaseStore { acquire(resource, holder, ttl_ms, now_ms) -> Lease; release(resource, holder) }`, `Lease { resource, holder, acquired_at_ms, expires_at_ms, granted, current_holder }` — single writer per wallet (`wallet:<address>`), SQLite `leases` table beside `observations`. Write tools return `WriteResult<D>` (mode plan \| simulate \| send, default simulate) and re-run the pure gate before sending.
