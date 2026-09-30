# xmarket risk + paper — operator reference (2026-09-30)

The `[risk]` gate, the paper ledger and the kill switch of an xmarket sandbox. Schema: `src/config/risk.rs` (every field required, § 7 #3 budget) + `src/config/hardening.rs` (load rules). Code: gate `src/domain/xm/risk.rs`, halts `src/domain/xm/risk_state.rs`, exec orders `src/domain/xm/exec.rs` + `src/adapters/outbound/tools/xm/exec_common.rs` (`run_exec`), ledger closure `src/application/paper.rs::decide`, ledger `src/ports/paper.rs` + `src/adapters/outbound/paper_store.rs`, tools `src/adapters/outbound/tools/xm/`, CLI `src/adapters/inbound/cli/risk.rs`. Extended by `risk-paper-tools`, `risk-audit-verdicts`, `x-exit-rules`.

## Load rules (`Config::load`, any violation fails it)

| Rule | Why |
|---|---|
| A `[risk]` sandbox is hardened like a Solana signer (`config/hardening.rs`, one code path): `claude_code` agents only with `builtin_tools_profile = "none"`; no `[[mcp_servers]]`; no scope grants `shell_bins` (tools without a scope run no shell, in-process and in the bridge) | nothing outside tengu scopes runs (convention 12) |
| `<TENGU_HOME>/state` (no overlap either way), `kill_switch_file` and the config file itself outside every `fs_roots` and agent `workspace` (symlinks resolved) | `read_file` / `write_file` cannot edit `ledger.db`, delete the kill-switch file or lift a limit for the next load |
| Exec tools (`paper_order`, `paper_close` — `domain/tools.rs::XM_EXEC_TOOLS`) only on a private agent: no `description`, not `default`, no webhook endpoint's `agent`; a loop action running one is not `read_only` | neither the planner, a chat user nor a webhook reaches an order tool; loops and the operator's `@<agent>` chat do |
| `[default_scopes.sign_and_send_transaction]` and `[default_scopes.sign_message]` present without `wallets`; no agent scope grants one | Privy signing stays off (a tool without a scope gets the permissive fallback's `default` wallet) |

## Gate enforcement — `run_exec` (every exec tool, in-process and through the bridge)

| Step | Rule | Refusal (tool error, nothing written) |
|---|---|---|
| Config | `[risk]` + `[paper]`, the ledger (`[xmarket]`) | `risk_config_missing` · `state_dir_missing` · `ledger_unavailable` |
| Agent | the caller is private again at call time (a planner step's `compose.tools` could hand any tool to a routable agent) | `exec_agent_not_private` |
| Key | `client_order_id` = the arg, else `ToolCtx.call_id` (loop `{loop}:{session}:{t}`, feed `feed:<name>:<slot>:<i>`, bridge / `tengu tool call` `mcp:<process nonce>:<JSON-RPC id>`); 1–128 chars, no whitespace; never random | `no_client_order_id` · `invalid_client_order_id` |
| Replay | an order stored under the key ⇒ its `paper_fill/1` row (`replayed`): no latency, no book read, nothing written | — |
| Rows (never fetched) | `mkt_ctx/1` of the open positions + the order (and hedge) instrument; `mkt_instrument/1` of the instrument (HL perp, `sz_decimals`, the paper fee = `hl_ctx`'s `taker_fee_bps` rule); the `opportunity` row | `missing:mkt_instrument` (read `hl_ctx` first) |
| Funding | every hour the open positions owe, at a fresh `mkt_ctx/1` rate + oracle, before the order | — |
| Order | `[paper] order_types`, well-formed; a close = the whole position, reduce-only | `order_type` · `invalid_order` · `no_position` |
| Latency + book | sleep `latency_ms ± jitter`, then a live `l2Book` (`hl_book/1` recorded + stored); a failed read = the gate's `missing:book` | — |
| Gate + fill + write | kill-switch probe, then one `BEGIN IMMEDIATE`: value at marks (day roll), `evaluate`, allowed ⇒ fill that book against the position read inside the transaction; a deny writes one verdict row only | — |

Underlying: the position's; none yet ⇒ the instrument id itself (asset exposure nets per instrument until the catalog, M1). A reduce-only close with no book after the latency is allowed degraded but rejected `stale_book` by the fill.

## `paper_fill/1:<account>:<client_order_id>` (TTL 0: recorded, never cached)

| Status | When | Features that say why |
|---|---|---|
| `ok` | filled | `status = filled`, `risk = allow`, `filled_qty`, `avg_px`, `slippage_bps`, `fee_usd`, `levels_used` |
| `partial` | the IOC bound or visible depth stopped the walk | `status = partial`, `reason` (`bound` · `depth`) |
| `error` | denied by the gate · rejected by the venue | `risk = deny` + `risk_rule` · `status = rejected` + `reason` (HL code) |

Every row: `latency_ms`, `book_age_ms`, `position_qty_after`, `equity_usd_after` (fresh marks; omitted, never 0, when one fails), `replayed`; `data` = the first failed check, the judged intent, the fill levels, the verdict row id (`risk_decisions.id`). Line 1 carries status, side, the full instrument id, the fill, the verdict and the full key.

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
