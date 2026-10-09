# xmarket risk + paper — operator reference (2026-09-30)

The `[risk]` gate, the paper ledger and the kill switch of an xmarket sandbox. Schema: `src/config/risk.rs` (every field required, § 7 #3 budget) + `src/config/hardening.rs` (load rules). Code: gate `src/domain/xm/risk.rs`, halts `src/domain/xm/risk_state.rs`, exec orders `src/domain/xm/exec.rs` + `src/adapters/outbound/tools/xm/exec_common.rs` (`run_exec`), ledger closure `src/application/paper.rs::decide`, ledger `src/ports/paper.rs` + `src/adapters/outbound/paper_store.rs`, tools `src/adapters/outbound/tools/xm/`, CLI `src/adapters/inbound/cli/risk.rs`. Verdict audit: § Audit (`risk-audit-verdicts`); funding and the venue facts closes keep: § Funding and kept facts (`domain/xm/ledger.rs::Position::settle_funding`, `domain/xm/exec.rs::order_venue_facts`); exit rules: § Exits (`x-exit-rules`, `src/domain/xm/exits.rs` + `src/adapters/outbound/tools/xm/exits.rs`); weekend fade: § Weekend fade (`x-weekend-fade-strategy`, `src/domain/xm/weekend_fade.rs` + `src/adapters/outbound/tools/xm/weekend_fade.rs`).

## Load rules (`Config::load`, any violation fails it)

| Rule | Why |
|---|---|
| A `[risk]` sandbox is hardened like a Solana signer (`config/hardening.rs`, one code path): `claude_code` agents only with `builtin_tools_profile = "none"`; no `[[mcp_servers]]`; no scope grants `shell_bins` (tools without a scope run no shell, in-process and in the bridge) | nothing outside tengu scopes runs (convention 12) |
| `<TENGU_HOME>/state` (no overlap either way), `kill_switch_file` and the config file itself outside every `fs_roots` and agent `workspace` (symlinks resolved) | `read_file` / `write_file` cannot edit `ledger.db`, delete the kill-switch file or lift a limit for the next load |
| `[xmarket]` present; every agent sets `workspace` (absolute or `~/…`); feed, loop and xmarket-tool agents share one (`config/xmarket.rs`, `docs/runtime-2026-09-30.md` § State layout) | the ledger lives in the state dir; an agent without a workspace works in the process cwd, which may hold `<TENGU_HOME>/state` |
| Exec tools (`paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade` — `domain/tools.rs::XM_EXEC_TOOLS`) only on a private agent: no `description`, not `default`, no webhook endpoint's `agent`; a loop action running one is not `read_only` | neither the planner, a chat user nor a webhook reaches an order tool; loops and the operator's `@<agent>` chat do |
| `[default_scopes.sign_and_send_transaction]` and `[default_scopes.sign_message]` present without `wallets`; no agent scope grants one | Privy signing stays off (a tool without a scope gets the permissive fallback's `default` wallet) |

## Gate enforcement — `run_exec` (every exec tool, in-process and through the bridge)

| Step | Rule | Refusal (tool error, nothing written) |
|---|---|---|
| Config | `[risk]` + `[paper]`, the ledger (`[xmarket]`) | `risk_config_missing` · `state_dir_missing` · `ledger_unavailable` |
| Agent | the caller is private again at call time (defence in depth: `run-agent` already refuses a `compose` that widens a routable agent in a `[risk]` sandbox, `bootstrap::tools::compose_agent`) | `exec_agent_not_private` |
| Gate ↔ account (review #5) | the `[risk]` gate only on the `[risk]` account; the shadow gate (no budget) only with the paper engine's `PaperFills` proof (`[risk] mode = "paper"`: a live engine gets none) and never on the `[risk]` account | `gate_account_mismatch` · `shadow_not_paper` |
| Owner (review #13) | the account belongs to the sandbox whose tools first wrote it (§ Ledger: owners); another sandbox's call is refused before any book read | `account_owner_mismatch` |
| Key | `client_order_id` = the arg, else `ToolCtx.call_id` (loop `{loop}:{session}:{t}`, feed `feed:<name>:<slot>:<i>`, bridge / `tengu tool call` `mcp:<process nonce>:<JSON-RPC id>`, in-process chat `chat:<turn nonce>:<round>:<i>:<provider id>`); 1–256 chars, no whitespace; never random. An arg never starts with a reserved prefix — `exit:`, `fade:`, `fade-shadow:`, `feed:`, `mcp:`, `chat:` (ids the exit rules, the weekend fade and the call paths make; review #11) | `no_client_order_id` · `invalid_client_order_id` |
| Replay | an order stored under the key ⇒ its `paper_fill/1` row (`replayed`): no latency, no book read, nothing written — only when the request asks for the same order: its fingerprint (`tool account instrument close` · `… side notional USD`, stored with the order) must match; orders stored before the column are not checked | `client_order_id_conflict` |
| Rows (never fetched) | `mkt_ctx/1` of the open positions, of closed ones owing funding, and of the order (and hedge) instrument; `mkt_instrument/1` of the instrument (HL perp, `sz_decimals`, the paper fee = `hl_ctx`'s `taker_fee_bps` rule) — a reduce-only order falls back to the facts kept with its position when that row is missing, older than them or partial (§ Funding and kept facts); the `opportunity` row | `missing:mkt_instrument` (read `hl_ctx` first; a close only without kept facts) |
| Hedge (review #8) | an entry (not reduce-only) whose strategy — the order's `strategy`, else its opportunity row's (the one the gate's `hedge` rule judges) — is in `require_hedge_for`: no exec path places the hedge leg yet, so it is refused rather than sent naked (the gate's `hedge` rule only checks that the leg could trade; hedge placement is a later item); exits and closes go on | `hedge_not_supported` |
| Funding | every hour owed or due, at a fresh `mkt_ctx/1` rate + oracle, before the order; inside `place` the order's instrument settles first at the size held (§ Funding and kept facts) | — |
| Order | `[paper] order_types`, well-formed; a close = the whole position, reduce-only; a reduce-only order's IOC bound ≤ 500 bps (`exec::MAX_EXIT_SLIPPAGE_BPS`, review #9: an arg above it is refused, a configured or default bound cut to it) | `order_type` · `invalid_order` · `no_position` |
| Latency + book | sleep `latency_ms ± jitter`, then a live `l2Book` (`hl_book/1` recorded + stored); a failed read = the gate's `missing:book` | — |
| Gate + fill + write | kill-switch probe, then one `BEGIN IMMEDIATE`: the funding the order's instrument owes settled at the size held (§ Funding and kept facts), value at marks (day roll), the file probed again for an entry (review #7: a `touch` while the order waited for the lock still denies it; present at either probe = present; a reduce-only order keeps the first probe), `evaluate`, allowed ⇒ fill that book against the position read inside the transaction; a deny writes one verdict row only, besides that funding (each verdict row also a `risk.jsonl` line — § Audit) | — |

Underlying: the position's; none yet ⇒ the instrument id itself (asset exposure nets per instrument until the catalog, M1). A reduce-only close with no book after the latency is allowed degraded but rejected `stale_book` by the fill.

| Call-id source (the key without a `client_order_id` arg) | Unique across | State |
|---|---|---|
| decision loop `{loop}:{session}:{t}` · feed `feed:<name>:<slot>:<i>` | events and restarts · slots (a retried slot replays, never doubles) | ok |
| bridge / `tengu tool call` `mcp:<process nonce>:<JSON-RPC id>` | CLI sessions and processes | ok |
| in-process chat (`@<agent>`, `tengu tool turn` on OpenRouter / local): `chat:<turn nonce>:<round>:<i>:<provider id>` (`application/chat/tool_loop.rs::chat_call_id`; the messages keep the provider's id) | processes, turns, rounds and calls | ok — fixed 2026-10-01 (`x-engine-parity-audit`); was the model's own tool-call id, unique only as far as the provider made it |

## Exec tools (`risk-paper-tools`, `src/adapters/outbound/tools/xm/paper.rs`)

| Tool | Args (* required) | Row |
|---|---|---|
| `paper_order` | `instrument`* (full id), `side`* buy / sell, `notional_usd`*, `kind`* market / limit, `limit_px` (limit only), `tif` ioc, `reduce_only`, `max_slippage_bps`* (< 10 000; ≤ 500 with `reduce_only`), `strategy` (§21 type; a `require_hedge_for` one is refused `hedge_not_supported`), `hedge_instrument`, `opportunity` (row key; the row must back this order — side, strategy, size), `client_order_id` (no reserved prefix), `exit_at_ms` (position deadline for the exit rules) | `paper_fill/1:<account>:<client_order_id>` |
| `paper_close` | `instrument` or `all = true`; `max_slippage_bps`* (≤ 500); `client_order_id` (no reserved prefix) — reduce-only market IOC of the whole position, same gate | `paper_fill/1` · all: `paper_close/1:<account>:<client_order_id>` (legs `<client_order_id>:<instrument>`, each replayed by its own id) |
| `xm_exits` (§ Exits) | `max_slippage_bps` (≤ 500; default `[risk] max_slippage_bps`, cut to 500) — closes every due position of the `[risk]` account, same gate | `xm_exits/1:<account>`; each close its own `paper_fill/1` |
| `xm_weekend_fade` (§ Weekend fade) | none — one step of rule W's window per call: capped fades through the gate, shadow fades through the shadow gate, shadow exits | `xm_weekend/1:<anchor date>`; each order its own `paper_fill/1` |
| `paper_positions` (not an exec tool) | `account` (default `[risk] account`) | `paper_positions/1:<account>` (2 s): funding booked first (every owed and due hour — closed positions owing hours too — at the fresh `mkt_ctx/1` rate + oracle; past hours at the current rate); marks fresh or omitted, never 0; `exit_at_ms` per position |

| Setup | Value |
|---|---|
| Agent | a private block: no `description`, not `default`, no webhook `agent`; `tools = ["hl_ctx", "paper_order", "paper_close", "paper_positions", "risk_status", "xm_exits", "xm_weekend_fade"]` |
| Scopes | `fs_roots` = the workspace (store); `paper_order` / `paper_close` / `xm_exits` / `xm_weekend_fade` also `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]` |
| Reached by | decision loops and feeds (their agent), `@<agent>` chat, `tengu tool call` / `tool turn` — never the planner, a `run-agent` step or a webhook |
| Engines | all three (convention 20): conformance cases for each tool; live legs `openrouter_*_xm`, `claude_code_xm` run the set through `tengu tool turn` on the fixtures' private `xm_*` agents |

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
| `max_orders_per_min`, `max_open_orders` | `order_rate` (entries — orders that are not reduce-only — stored in the last 60 s), `open_orders` | `order_rate` skipped: exits never count and are never rate-limited (review #3) · `open_orders` same |
| `max_data_age_ms.book` / `.ctx` | `book_age`, `ctx_age` — a row stamped more than 1 s after now (a clock step) is stale, never age 0 (`ledger::stamp_age_ms`, review #12; also marks, funding rates, the opportunity row's TTL, the fade's prices) | waived |
| — | `market_status` (listed, a book; at the OI cap only if not growing; a Hyperliquid row without `at_oi_cap` — the cap read failed — only if not growing, else `missing:at_oi_cap`, judged again next call instead of a stored fill refusal) | skipped |
| `min_edge_bps` | `min_edge`: the order's opportunity row (key names the instrument, within its TTL) backs this order (review #8) — its `side` is the order's, its `strategy` the order's when the order names one, `edge_after_costs_bps` ≥ `min_edge_bps`, and the order (`order_notional`'s size) ≤ its `max_notional_usd` when it carries one | skipped |
| `max_slippage_bps`, `min_depth_usd` | `depth`, `slippage` | skipped |
| `max_order_notional_usd` … `max_net_exposure_usd`, `max_leverage` | `order_notional`, `position_notional`, `asset_exposure`, `venue_exposure`, `gross_exposure`, `net_exposure`, `leverage` — after the fill | skipped |
| `require_hedge_for`, `max_skew_ms` | `hedge`, `skew` — for the order's strategy, else its opportunity row's; such an entry is refused `hedge_not_supported` before the gate until hedge legs are placed (§ Gate enforcement) | skipped |

Missing input ⇒ deny `missing:<field>` (`kill_switch`, `mark`, `equity`, `day_start_equity`, `book`, `ctx`, `at_oi_cap`, `edge_after_costs_bps`, `opportunity_side`, `opportunity_strategy` (the order names a strategy, the row none), `opportunity_max_notional_usd` (present but not a number > 0), `lifecycle`, `hedge_book`, `hedge_ctx`). At the limit passes; limit + 1e-6 fails. A waived exit is allowed with rule `allow_reduce_degraded` (§ 7 #7).

| Opportunity row feature (writers set all; any schema: `xm_weekend_signal/1`, `xm_compare/1` from `risk-calc-tools`) | Gate needs it | Gate |
|---|---|---|
| `edge_after_costs_bps` | always (`missing:edge_after_costs_bps`) | ≥ `min_edge_bps` |
| `side` (`buy` · `sell`) | always (`missing:opportunity_side`) | = the order's side |
| `strategy` (§21 type) | when the order names one (`missing:opportunity_strategy`) | = the order's `strategy`; an order naming none trades the row's (`require_hedge_for`) |
| `max_notional_usd` | no (malformed ⇒ `missing:opportunity_max_notional_usd`) | the order's size ≤ it |

## Ledger — `<TENGU_HOME>/state/<xmarket.state>/ledger.db`

| Table | Holds |
|---|---|
| `accounts` · `cash` | several accounts (weekend: capped + shadow), created with `[paper] initial_cash_usd`, `sandbox` = the owner (below; added on open, NULL until claimed) · journal deposit / fill / funding + running balance |
| `positions` · `fills` | per (account, instrument) incl. `exit_at_ms`, the kept venue facts (`sz_decimals`, `taker_fee_bps`, `maker_fee_bps`, `facts_at_ms`) and a fired TP / SL (`exit_trigger`, `exit_trigger_opened_ms`, `exit_trigger_ms`) · VWAP fill per filled / partial order |
| `funding` · `funding_owed` | one HL payment per (account, instrument, hour): rate, oracle, the size held at the hour · an hour settled without a fresh rate: the size held then, until a rate books it |
| `orders` | allowed orders, `UNIQUE (account, client_order_id)`: a retry returns the stored result, writes nothing; `fingerprint` (added on open, NULL on older rows) = what the order asked for — a retry asking for another order is refused |
| `risk_decisions` · `risk_state` | every verdict (checks, headroom, trips, intent, context digest, call id, exec tool, session id — § Audit) · halt + UTC day + day-start equity |

`place` = funding settled + gate + fill + write in one `BEGIN IMMEDIATE`; a deny writes the verdict, the funding settled (and a changed risk state) only. Columns and tables added later reach an older `ledger.db` on open (`ALTER TABLE … ADD COLUMN`, `CREATE TABLE IF NOT EXISTS`, one transaction; a binary from before them leaves them alone).

## Funding and kept facts (reviews #6, #10)

| Rule | Detail |
|---|---|
| Size | an hour's funding = the size held at that hour × oracle × the HL 1 h rate (positive ⇒ longs pay); hours are settled before any fill changes the size: `place` settles the order's instrument first, inside its transaction |
| Rate | a fresh `mkt_ctx/1` row (`max_data_age_ms.ctx`; a row stamped > 1 s ahead is stale): the hour is booked. None: the hour is recorded owed (`funding_owed`: hour + size), never dropped — also through a full close or a flip |
| Booking owed | the next settlement with a fresh rate books every owed hour at that rate (HL's per-hour history is not read), then the due ones; every exec call, `paper_positions`, `xm_exits` (every 15 s) and `xm_weekend_fade` settle the account's open positions and closed ones owing hours. Without a rate nothing is written outside `place`: the hours stay due |
| Until booked | equity omits owed funding (cents on the budget); a weekend-fade name owing hours stays `closing` without a P&L; `tengu risk status` lists them |
| Kept facts | every sent order priced from a `mkt_instrument/1` row keeps its `sz_decimals`, fee schedule and the row's time with the position (never an older row's over newer ones) |
| Using them | a reduce-only order (`xm_exits`, `paper_close`, a shadow exit) fills on them when the row is missing (store unreadable, purged after 7 days), older than them, or partial; the `mkt_ctx/1` row still gives the listing and OI cap (none ⇒ `open`, the book decides). Entries always need the row. A position from before the columns has none: its close needs the row |

| Owners (review #13) | Rule |
|---|---|
| Who | the tools' ledger handle writes as its sandbox: `<name>` of the config file `sandboxes/<name>/config.toml` — however loaded: `--sandbox`, `-c`, the MCP bridge's `TENGU_CONFIG` — else `default` (`config/paths.rs::sandbox_of_config_file` → `SandboxSections::owner`) |
| Claim | every write (`open_account`, `place`, `update_risk_state`, `accrue_funding`) claims an account with no owner yet — new, or stored before the column — inside its transaction |
| Refuse | an account another sandbox owns: `account_owner_mismatch`, nothing written (replays, `risk_status` rolls and funding included); reads never check |
| Operator | `tengu risk` opens the ledger without an owner: status, halt and resume always work and claim nothing; `risk status` prints each account's owner |
| First line | the `state:<dir>` lease (`docs/runtime-2026-09-30.md` § Single-runner lease): two sandboxes on one `[xmarket] state` never run at once |
| Same sandbox, other path | the name comes from the file's directory: a config mounted elsewhere (Docker's `/opt/tengu/config.toml`) writes as `default` and is refused on accounts a `--sandbox <name>` run owns — run the sandbox's own file (`--sandbox <name>`; the image carries `/opt/tengu/sandboxes/`), or, as the operator with the run stopped, `sqlite3 <state dir>/ledger.db "UPDATE accounts SET sandbox = '<name>' WHERE account = '<account>'"` |
| Older binaries | a binary from before 2026-10-01 (the frozen weekend `tengu-6fcb455`) neither checks nor claims; a newer one adds the column on open (additive) and claims on its first write |

## Audit — every verdict (`risk-audit-verdicts`)

| Record | Where | Kept |
|---|---|---|
| verdict row — canonical | `ledger.db` `risk_decisions` (written by `place`, deny or allow) | never pruned |
| mirror line | `<TENGU_HOME>/logs/risk.jsonl`: one line per verdict row, written after the commit with one `write_all` (concurrent processes never tear a line); a replay writes neither; a failed write only warns | `tengu prune` deletes `logs/` |
| loop outcome | `decisions.jsonl` `result = {outcome: "refused", action, rule}` when a typed exec result carries `risk = deny` (`StepOutcome::Refused`, counted apart from `executed`; the loop goes on); the TUI feed shows `refused by the risk gate: <rule>` | as `decisions.jsonl` |

| `risk.jsonl` field | Value |
|---|---|
| `ts_ms` · `decision_id` | the row's time and `risk_decisions.id` |
| `account` · `client_order_id` · `instrument` | full ids |
| `call_id` | `ToolCtx.call_id` — loop `{loop}:{session}:{t}` (= that step's `call_id` in `decisions.jsonl`), feed `feed:<name>:<slot>:<i>`, bridge / `tengu tool call` `mcp:<nonce>:<id>` |
| `session_id` · `tool` | `TENGU_SESSION_ID` of the process (a `run-agent` child, its bridge), else null — loop and feed sessions are inside `call_id` · the exec tool (`paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade`) |
| `verdict` · `rule` · `class` · `degraded` · `trips` · `checks` · `headroom` | allow / deny, the first failed rule, entry / exit, every check with its values and limits |
| `intent` · `context` | the judged `OrderIntent` · the digest of row keys, ages and values the gate read |
| `fill` | null when denied; else `order_id` (joins `orders` and `fills`), `status`, `reason`, `filled_qty`, `avg_px`, `fee_usd`, `slippage_bps`, `exit_at_ms` |

Join a loop step to its verdict: `jq -c 'select(.call_id == "<call id>")' <TENGU_HOME>/logs/decisions.jsonl <TENGU_HOME>/logs/risk.jsonl`; refusals per rule: `jq -r 'select(.verdict == "deny") | .rule' risk.jsonl | sort | uniq -c`.

## Exits — `xm_exits` (`x-exit-rules`)

`[risk.exits]` (every key required, unknown keys refused): `take_profit_bps`, `stop_loss_bps` (finite > 0), `max_hold_secs` (> 0). The exec tool `xm_exits` books the account's funding, then checks every open position of the `[risk]` account (`domain/xm/exits.rs::exit_due`) and closes the due ones through `run_exec` — reduce-only market IOC of the whole position, sized inside the ledger transaction, the gate inside (an exit passes halted / on stale data under `allow_reduce_degraded`).

| Reason (first due wins) | Due when | Price |
|---|---|---|
| `deadline` | the position's `exit_at_ms` ≤ now — set by the order that opened it (`paper_order exit_at_ms`, a strategy such as the weekend fade) | none |
| `max_hold` | `opened_ms` + `max_hold_secs` ≤ now | none |
| a fired `stop_loss` / `take_profit` | it fired for this opening (`positions.exit_trigger`, review #6): due until the position is flat — a later stale mark, or a price back inside the band, never cancels it | none |
| `stop_loss` · `take_profit` | P&L (side-signed `(px − entry) / entry`, bps; fees and funding not counted) ≤ −`stop_loss_bps` · ≥ `take_profit_bps`; recorded as fired before the close | a `mkt_ctx/1` mark within `max_data_age_ms.ctx` (never fetched); stale or missing (`n_stale_marks`) ⇒ the mid of the live book the close reads after its latency (`n_book_marks`, `book_mid`): fires ⇒ closed, else nothing placed; a failed, one-sided or stale (> `max_data_age_ms.book`) book judges nothing this run |

| Detail | Rule |
|---|---|
| Id | `exit:<account>:<instrument>:<reason>:<opened_ms>` (full ids); when that id's order is stored but left the position open (rejected, e.g. `stale_book`; partial) the next attempt uses the first unstored `…:<n>`; a denial stores nothing, so the same id is judged again. A retry after a crash never closes twice: the close is sized from the position inside the transaction (flat ⇒ nothing to close) |
| Retry (review #3) | the latest stored attempt decides (`domain/xm/exits.rs::exit_retry`, read from the ledger — a restart keeps it): none, filled or partial ⇒ the next attempt now; rejected for a reason that may pass (`FillReason::is_transient`: `stale_book`, liquidity, missing data, OI cap, halts) ⇒ `backoff` — the next attempt 15 s, 30 s, 1 min, 2 min, 4 min, 8 min, then every 15 min after the latest (rejections in a row); rejected for a final reason (`delisted`, `invalid_order`, `order_type`, `MinTradeNtl`, `Tick`, `ReduceOnly`) ⇒ `stuck`: never placed again — one WARN log line when it happens, the stored rejection + its verdict stay the marker, the operator decides |
| Row `xm_exits/1:<account>` (ttl 0) | features `n_open`, `n_due`, `n_closed`, `n_failed` (`backoff` and `stuck` included), `n_stale_marks`, `n_book_marks`, `n_stuck`; `data` per open position: full id, qty, entry, `opened_ms`, `exit_at_ms`, mark (or `book_mid`), P&L bps, reason, `triggered_ms` (a TP / SL fired), status (`held` · `filled` · `partial` · `rejected` · `denied` · `flat` · `error` · `backoff` · `stuck`), id + attempt, gate rule, fill, `next_attempt_ms`; `ok` nothing failed · `partial` a close failed or a mark was stale · `error` every due close failed |
| Audit | each close is its own `paper_fill/1` row and verdict (`tool = xm_exits`, the feed's call id); a book that fired nothing writes nothing |
| Needs | the `hl_ctx` feed in the same workspace (one store) for `mkt_ctx/1` marks younger than `max_data_age_ms.ctx` — run it at least that often (15 s, convention 14) — and `mkt_instrument/1` rows; with them gone (store unreadable or purged, `hl_ctx` down) a close fills on the facts kept at the entry and TP / SL are judged on the live book (`l2Book`, scope `net_hosts`) |
| Gate on an exit | skips caps, `min_edge`, depth / slippage, `market_status`, `order_rate` (exits never count toward `max_orders_per_min` and are never denied by it); waives `kill_switch` / `halted` / `book_age` / `ctx_age` under `allow_reduce_degraded` (recorded); still checks `intent`, `account`, `reduce_only`, `open_orders`. IOC bound ≤ 500 bps (`exec::MAX_EXIT_SLIPPAGE_BPS`; a `[risk.exits]` key may replace the constant after the weekend run) |
| Feed | `[feeds.xm_exits] kind = "tool"`, `agent` = the private exec agent (its `tools` list `xm_exits`), `tool = "xm_exits"`, `every_secs = 15`, `required = true` — no LLM, no Jev (`config.example.toml`); an `error` row reports the feed `down` until a run closes them |

## Weekend fade — `xm_weekend_fade` (`x-weekend-fade-strategy`)

Rule W, fixed before the data ([`xmarket-feasibility-2026-09-30.md`](xmarket-feasibility-2026-09-30.md)). Knobs `[xmarket.weekend_fade]` (`src/config/xmarket.rs`: every key required, load rules there).

| Piece | Rule |
|---|---|
| Window | a break of the `calendar` (an `exchange` row: NYSE) that holds a Saturday and a Sunday — a single mid-week holiday is not one: anchor 20:00 on the last trading day before it, entry 18:00 on its last non-trading day (Sun; Mon for a Monday holiday), exit 09:00 on the next trading day; New York wall clock, DST-safe |
| Prices | `mkt_ctx/1` mid, else mark — the anchor from the recorder's history as of the anchor (≤ `anchor_max_age_secs` old), the entry from the store (≤ `entry_max_age_secs` old at the call) |
| Signal | s = ln(P_entry / P_anchor); fade = −sign(s); eligible = not in `exclude`, both prices, s ≠ 0 |
| Shadow ledger | every eligible name, `shadow_notional_usd`, account `shadow_account` (opens with `shadow_initial_cash_usd`) |
| Capped ledger | the `capped_top_n` largest \|s\| ≥ `min_abs_signal_bps` (ties by full id), `capped_notional_usd` each, the `[risk]` account |
| Offline replay | `weekend_fade::replay` over 5 m candles reproduces the feasibility golden of 2026-09-26 → 09-28 bit for bit: 74 names (`xyz:KIOXIA` excluded, split halt), mean net +95.4585 bps at 3.8 bps round trip, 53 positive, capped `xyz:CRCL`, `xyz:SMSN`, `xyz:MINIMAX`, `xyz:MSTR` (`tests/fixtures/xmarket/`) |

| Ledger | Gate | Entry id | Exit |
|---|---|---|---|
| capped | the `[risk]` gate, every rule; opportunity = the name's `xm_weekend_signal/1` row (`edge_after_costs_bps` = `expected_edge_bps`, example 23 = half the in-sample +46), strategy `overreaction`; the row backs that fade only (review #8): its `side`, `strategy = overreaction`, `max_notional_usd` = `capped_notional_usd` — another order naming it (the other side, another strategy, larger) is denied `min_edge` | `fade:<account>:<full id>:<anchor date>` | `xm_exits`, reason `deadline` (the position's `exit_at_ms`) — never this tool |
| shadow | `risk::evaluate_shadow`: intent, account, kill switch, halt, reduce-only, venue, book age as the gate; an entry on an unknown OI-cap state denied `missing:at_oi_cap`; every budget rule `Skipped` "shadow: measurement only"; no trips (the kill-switch file denies entries while present, leaves no sticky halt). Paper only and never the `[risk]` account (review #5: `shadow_not_paper` · `gate_account_mismatch`) | `fade-shadow:<shadow account>:<full id>:<anchor date>` | this tool after the exit: reduce-only IOC (bound ≤ 500 bps), `exit:<shadow account>:<full id>:deadline:<opened_ms>` (`…:<n>` after a stored rejected / partial attempt), the exits' retry rule (§ Exits: `backoff`, `stuck`) |

Both: market IOC bound `max_slippage_bps`, `exit_at_ms` = the exit, the fill on the post-latency live book, one `place()` per order (verdict row + `risk.jsonl` line, tool `xm_weekend_fade`, the feed's call id). Anchor date = the window's last trading day (`YYYY-MM-DD`, New York). A capped pick whose book fails the gate (`[risk] max_slippage_bps`, `min_depth_usd`) is denied and not replaced by the next name — the risk policy; the shadow ledger measures every eligible name at the `max_slippage_bps` IOC bound.

| Entry attempts (both ledgers) | Rule |
|---|---|
| Ids | attempt 1 = the entry id above; attempt n ≥ 2 = `<entry id>:<n>` (`weekend_fade::fade_attempt_id`, the exits' numbering) |
| Latest attempt | found in the ledger by scanning the ids (`exits::first_unstored`); it is the fade's outcome in the row |
| Never placed again | a fill or a partial fill; a final rejection: size, lot and tick rules (`MinTradeNtl`, `Tick`, `ReduceOnly`), `invalid_order`, `order_type`, `delisted` |
| Placed again as the next attempt, while now < entry + `entry_lateness_max_secs` | a transient rejection (`FillReason::is_transient`, one list in `domain/xm/paper.rs`): missing data (`missing:mid` — a one-sided book —, `missing:oracle`, `missing:at_oi_cap`, `bad_book`), book age (`stale_book`), liquidity (`MarketOrderNoLiquidity`, `IocCancel`), a price bound (`Oracle`), a venue state (the OI cap, `market_halted`, `market_closed`); after the lateness it stays the outcome |
| Gate denial | stores nothing: the same attempt id is judged again next call (within the lateness) |
| `position_open` | only for a position the fade's ids did not open (the account holds the name, no fill under its attempts): not faded |
| Idempotency | two callers place the same attempt id; the ledger keeps one (`UNIQUE (account, client_order_id)` inside one `BEGIN IMMEDIATE`), the other replays it; attempt n + 1 only follows a stored rejection of n, so at most one attempt per fade fills |
| Outcomes from the ledger | every call reads the fades not filled in the row from the ledger — inside the lateness, after it and in the previous window's row — so a row save lost to a crash or a concurrent call still counts the fills, closes and P&L |

| Phase (`xm_weekend/1:<anchor date>`) | When | The call |
|---|---|---|
| `waiting` | before the entry | reports `next_entry_s` |
| `entered` | entry ≤ now < exit, snapshot kept | first call: the snapshot (prices, signals, the capped set, each name's ledger bases) kept in the store by compare-and-swap before any order (one per window across processes; never recomputed), then the capped fades (largest \|s\| first), then the shadow fades (4 at a time); later calls within `entry_lateness_max_secs` place the next attempt where the table above says so; no eligible name ⇒ an `error` row, nothing kept, the next call tries again. Kept only complete: while a name (not excluded, with an anchor) has no entry row fresher than `entry_max_age_secs` (none, or older: a restart), until entry + 120 s (at most half the lateness), the call keeps nothing (an `error` row) and the next one reads again; from then on those names are left out `stale` (`n_stale`). A fresh row without a usable price is `missing_entry` and holds nothing back |
| `missed_entry` | no snapshot by entry + `entry_lateness_max_secs` | nothing: no late entry |
| `closing` | after the exit, a filled name still open — or flat but owing funding hours (settled at a stale rate, § Funding and kept facts) | closes the due shadow positions; the capped ones wait for `xm_exits` |
| `closed` | every filled name flat with its funding booked | P&L per name and ledger = (realized − fees − funding) now − the base before the entry, USD and bps of the entry notional; `shadow_pnl_usd`, `shadow_mean_net_bps`, `capped_pnl_usd`, `capped_mean_net_bps` |

Every call also books both accounts' hourly funding (closed positions owing hours too). It returns the previous window's row while that one closes and the next waits, else the current window's; both are stored (TTL 120 s).

| Setup | Value |
|---|---|
| Feeds (`config.example.toml`) | `[feeds.xm_weekend_fade]` `kind = "tool"`, the private exec agent, `every_secs = 60`, `required = true`; `[feeds.hl_ctx]` `args = { dex = "xyz" }` every 60 s (prices + the `mkt_instrument/1` rows every fill needs); `[feeds.xm_exits]` every 15 s |
| `[recorder]` | records `mkt_ctx/1` from before Friday 20:00 New York; `anchor_max_age_secs` ≥ its `heartbeat_secs` (load rule) |
| `[risk]` | `instruments_allow` ⊇ the universe not excluded (load rule); `capped_top_n` × `capped_notional_usd` ≤ `max_gross_exposure_usd` (load rule) — leave headroom: marks move between fills, and the gate checks `max_leverage` against equity after fees (4 × $25 on $100 cash needs `max_leverage` > 1); `max_net_exposure_usd` ≥ the capped total (fades often share a side); `max_orders_per_min` ≥ 4 (the entries at once; exits never count since review #3 — the frozen weekend binary `tengu-6fcb455` still counts them: ≥ 8 for it, the weekend sandbox has 12); `max_data_age_ms.ctx` ≥ the `hl_ctx` interval + jitter; `require_hedge_for` without `overreaction` (load rule); `[risk.exits]` `max_hold_secs` above the 15 h hold (entry 18:00 → exit 09:00 on the next trading day, holidays and DST included) with wide TP / SL, so the deadline closes |

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
| `tengu risk status [--sandbox s] [--account a]` | per account: owner sandbox, halt, day start, cash, positions (full ids, exit deadlines, a fired TP / SL), funding owed (hours, since when), entries last 60 s (exits never count), last 5 verdicts, kill-switch file | read-only; never creates the ledger |
| `tengu risk halt [--account a]` | `operator` halt (default account `[risk] account`) | stdin + stdout a TTY; refused under `TENGU_AGENT_IPC` / `TENGU_AGENT_NAME` |
| `tengu risk resume [--account a]` | the operator types the account name — and, with the resume guard, the secret; clears any halt | same + refused while the kill-switch file exists |
| tool `risk_status` (opt-in) | row `risk_state/1:<[risk] account>` (TTL 2 s); marks from fresh `mkt_ctx/1` rows (never fetched: missing ⇒ `partial`, numbers omitted); each read rolls the UTC day + records trips | no `[risk]` ⇒ `risk_config_missing`; scope `fs_roots` = the workspace; another sandbox's account ⇒ `account_owner_mismatch` |

**A terminal is not a human (review #16).** `halt` / `resume` only check that stdin and stdout are terminals and that no agent env is set. Inside tengu that holds: a hardened sandbox gives no agent a shell (no `shell_bins` scope, no-shell fallback, `claude_code` built-ins off). An agent *outside* tengu with a shell, running as the operator's user, could run `script -q /dev/null tengu risk resume` (a pseudo-terminal) and type the account name.

| Resume guard (optional, `src/adapters/inbound/cli/risk.rs`) | Rule |
|---|---|
| On | `TENGU_RISK_RESUME_SECRET_FILE` names a file (`~/` expanded) in the operator's shell; unset or empty = off (default) |
| Resume | after the account name, asks for the file's content (typed without echo, compared trimmed, in constant time); a mismatch changes nothing |
| Refused before any prompt | the file is missing, not a regular file, readable by group / other (must be 0600), over 4 KiB, not UTF-8 or empty, or inside an agent's `fs_roots` / `workspace` (symlinks resolved); messages never carry the content |
| Limit | stops an outside agent only while it cannot read the file: one whose file access is confined (a sandboxed CLI, a container without the path). An agent with an unrestricted shell as the operator's OS user can read a 0600 file — run such agents as another user, or not beside a live ledger. `halt` is never guarded |
