# xmarket risk + paper — operator reference (2026-09-30)

The `[risk]` gate, the paper ledger and the kill switch of an xmarket sandbox. Schema: `src/config/risk.rs` (every field required, § 7 #3 budget). Code: gate `src/domain/xm/risk.rs`, halts `src/domain/xm/risk_state.rs`, ledger `src/ports/paper.rs` + `src/adapters/outbound/paper_store.rs`, tool `src/adapters/outbound/tools/xm/`, CLI `src/adapters/inbound/cli/risk.rs`. Extended by `risk-gate-enforcement`, `risk-paper-tools`, `risk-audit-verdicts`, `x-exit-rules`.

## `[risk]` → gate rules (`evaluate`, first failing rule = the verdict's `rule`)

| `[risk]` field(s) | Rule code | Reduce-only exit |
|---|---|---|
| — | `intent` (qty, notional > 0; full ids; underlying = the open position's) · `account` | same |
| `kill_switch_file` · (stored halt) | `kill_switch` (present ⇒ trips `file`) · `halted` | waived with `allow_reduce_degraded` |
| `venues`, `instruments_allow`, `instruments_deny` (M0 permission; `min_lifecycle` from M1) | `venue`, `instrument` | pass |
| `daily_loss_limit_usd`, `total_loss_limit_usd` | `daily_loss`, `total_loss` — a breach trips the halt | not gated (still trips) |
| `max_orders_per_min`, `max_open_orders` | `order_rate`, `open_orders` | same |
| `max_data_age_ms.book` / `.ctx` | `book_age`, `ctx_age` | waived |
| — | `market_status` (listed, a book; at the OI cap only if not growing) | skipped |
| `min_edge_bps` | `min_edge`: `edge_after_costs_bps` of the order's opportunity row (key names the instrument, within its TTL) | skipped |
| `max_slippage_bps`, `min_depth_usd` | `depth`, `slippage` | skipped |
| `max_order_notional_usd` … `max_net_exposure_usd`, `max_leverage` | `order_notional`, `position_notional`, `asset_exposure`, `venue_exposure`, `gross_exposure`, `net_exposure`, `leverage` — after the fill | skipped |
| `require_hedge_for`, `max_skew_ms` | `hedge`, `skew` | skipped |

Missing input ⇒ deny `missing:<field>` (`kill_switch`, `mark`, `equity`, `day_start_equity`, `book`, `ctx`, `edge_after_costs_bps`, `lifecycle`, `hedge_book`, `hedge_ctx`). At the limit passes; limit + 1e-6 fails. A waived exit is allowed with rule `allow_reduce_degraded` (§ 7 #7).

## Ledger — `<TENGU_HOME>/state/<xmarket.state>/ledger.db`

| Table | Holds |
|---|---|
| `accounts` · `cash` | several accounts (weekend: capped + shadow), created with `[paper] initial_cash_usd` · journal deposit / fill / funding + running balance |
| `positions` · `fills` · `funding` | per (account, instrument) incl. `exit_at_ms` · VWAP fill per filled / partial order · one HL payment per (account, instrument, hour) |
| `orders` | allowed orders, `UNIQUE (account, client_order_id)`: a retry returns the stored result, writes nothing |
| `risk_decisions` · `risk_state` | every verdict (checks, headroom, trips, intent, context digest, call id) · halt + UTC day + day-start equity |

`place` = gate + fill + write in one `BEGIN IMMEDIATE`; a deny writes the verdict (and a changed risk state) only.

## Halts (§ 7 #8)

| Reason | Trips when | Clears |
|---|---|---|
| `daily_loss` | day-start equity − equity > `daily_loss_limit_usd` | next 00:00 UTC, or resume |
| `total_loss` / `operator` | initial cash − equity > `total_loss_limit_usd` / `tengu risk halt` | `tengu risk resume` only |
| `file` | `kill_switch_file` present — every gate call and `risk_status` read | `tengu risk resume`, once the file is gone |

Halted ⇒ entries deny, reduce-only exits pass. Day-start equity = the first valuation of the UTC day (gate call or `risk_status`); unknown ⇒ entries deny `missing:day_start_equity`. A loss still over its limit re-trips after a resume. No-code lever: `touch <kill_switch_file>`.

## `tengu risk` and `risk_status`

| Command / tool | Does | Guard |
|---|---|---|
| `tengu risk status [--sandbox s] [--account a]` | per account: halt, day start, cash, positions (full ids, exit deadlines), orders last 60 s, last 5 verdicts, kill-switch file | read-only; never creates the ledger |
| `tengu risk halt [--account a]` | `operator` halt (default account `[risk] account`) | stdin + stdout a TTY; refused under `TENGU_AGENT_IPC` / `TENGU_AGENT_NAME` |
| `tengu risk resume [--account a]` | the operator types the account name; clears any halt | same + refused while the kill-switch file exists |
| tool `risk_status` (opt-in) | row `risk_state/1:<[risk] account>` (TTL 2 s); marks from fresh `mkt_ctx/1` rows (never fetched: missing ⇒ `partial`, numbers omitted); each read rolls the UTC day + records trips | no `[risk]` ⇒ `risk_config_missing`; scope `fs_roots` = the workspace |
