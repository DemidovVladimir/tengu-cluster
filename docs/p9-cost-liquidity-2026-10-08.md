# Phase 9 — cost / liquidity (2026-10-08)

`TENGU_ROADMAP.md` Phase 9, gate G9: does execution / liquidity information improve rule W's candidate selection? Sources: the recorded weekend's `mkt_ctx/1` (fees) and `hl_book/1` (75 names, books at rule W's entry Sun 18:00 ET and exit Mon 09:00 ET; vault `w1-2026-10-06`), xlab backtests in `sandboxes/xlab-w2`.

## Observed vs assumed

| Item | W1 assumed | Observed 2026-10-04 / 05 |
|---|---|---|
| Taker fee | 0.9 bps every name | 0.9 for 90 stocks (growth mode); **9.0** for `hyperliquid:xyz:BMNR`, `hyperliquid:xyz:MSTR`, `hyperliquid:xyz:PURRDAT`, `hyperliquid:xyz:STRC` (D8) |
| $100 order cost per side (half-spread + impact) | 1.0 bps | median 1.70 entry / 1.41 exit; mean 4.52 / 3.75; worst `hyperliquid:xyz:CVX` 71.0, SOFTBANK 17.9, QNT 14.9 |
| Abdi–Ranaldo from 48 × 1h bars (history's only spread proxy) | — | mean 3.48 vs book 4.09 bps, **corr 0.098** — no per-name signal: cost-aware ranking cannot be tested point-in-time on history |

**Cost model v2** (`sandboxes/xlab-w2`, 75 per-name `[backtest.costs]`): observed fee + the measured $100 cost per side, mean 4.13 bps. The spread part was observed once and is assumed over history. W1's flat-cost results stay untouched. Rule W under v2: all names +50.45 → **+45.84** bps (CI [+11.0, +77.1]); top 4 +147.57 → **+142.65** (CI [+46.1, +243.3]).

## Engine knobs (`weekend_window`; absent ⇒ W1's spec hashes unchanged)

| Knob | Rule |
|---|---|
| `stop_loss_bps` | exit at the first 1h close that is that many bps against the fade (`ExitPlan::Bars`, the `move_trigger` walk); else the window exit |
| `rank_by = "net_of_cost"` | `top_n` ranks by \|s\| − 2 × the side cost at the decision (fee + half-spread + slippage); a name without a cost ranks last; needs `top_n` |

## Result (top 4, cost model v2, decisions 2026-03-01 → 2026-10-03, development)

| Spec | mean net bps | 95 % CI | Sharpe | capped drawdown |
|---|---|---|---|---|
| baseline | +142.65 | [+46.1, +243.3] | 3.69 | $9.67 |
| close stop 300 | **+163.14** | [+76.0, +251.5] | **4.60** | **$7.27** |
| close stop 500 | +142.90 | [+51.3, +237.8] | 3.88 | $8.47 |
| net-of-cost ranking | +143.38 | [+48.0, +243.2] | 3.70 | $9.67 |
| stop 500 + net-of-cost | +143.63 | [+51.8, +239.2] | 3.88 | $8.47 |

Verdict: **INCONCLUSIVE**. The 300 bps stop helps in development and lowers drawdown. But the CIs overlap, and the best level moves with the method (the hand-rolled intrabar sweep read 500 > 300). Net-of-cost ranking rarely changes the top 4. Record: `lineage/experiments/xlab.weekend_fade_top4.exits_costs.2026-10-08.toml`, 4 variants `weekend_fade.top4.{close_stop300, close_stop500, net_of_cost, close_stop500.net_of_cost}`.

## G9

| Check | Result |
|---|---|
| Reproducible cost model | PASS: v2 is config, every value from a named vault file |
| Observed vs assumption distinguishable | PASS: fee observed; spread observed once, assumed over history; marked in the config, records and here |
| Old results intact | PASS: W1 records and xlab's flat costs unchanged |
| Development / holdout discipline | PASS: development only; forward weekends are the holdout |
| Eligible for W2 | the 300 bps close stop looked so on development evidence, then **failed the forward weekend** (−40.57 vs +131.04 bps, `docs/p10-w2-candidate-2026-10-08.md`). Net-of-cost ranking: no |

Open: D11 (late funding from HL `fundingHistory`) — a new opt-in tool; ≈ $0.015 a weekend; not built.
