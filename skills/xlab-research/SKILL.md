---
name: xlab-research
description: Research protocol of the xlab Architect — turn a trading hypothesis into a strategy spec, backtest it on backfilled history after costs, tune on the in-sample half only, read the holdout once, and criticise failures. History first, never live waiting.
---

# xlab research protocol

You answer trading questions from **history** (`market_history`, `backtest`). Never propose waiting for live data when history can answer; say plainly when no history source exists.

## Tools

| Tool | Use |
|---|---|
| `market_history` | what is stored for an instrument (bars, funding), summary stats over a window; `fetch = true` backfills a missing range first (Hyperliquid; Gecko needs `pool`). Configured share splits are adjusted as a backtest reads them |
| `backtest` | run a named strategy (`strategy`) or your own spec (`spec`) over `[from, to)`; with `split` it runs and shows the **in-sample half only** — the holdout stays hidden until you read it with `"holdout": true` (counted). With `run_id` it reads a stored run's rows instead (`view` periods / instruments / trades / notes) |

## Call shapes (copy them)

| Call | Arguments |
|---|---|
| tune a library strategy (in-sample only) | `{"strategy": "weekend_fade", "split": "time:2026-07-01T00:00:00Z"}` |
| tune your own spec (in-sample only) | `{"spec": {"name": "wf_strict", "kind": "weekend_window", "universe": "@xyz_stocks", "interval": "1h", "calendar": "us_equity", "direction": "fade", "min_abs_signal_bps": 100, "min_entry_trades": 100}, "split": "time:2026-07-01T00:00:00Z"}` |
| the ONE holdout read of the chosen variant | the same call + `"holdout": true` → both halves side by side, `holdout read #n for this spec` |
| a window | add `"from": "2026-04-01"`, `"to": "2026-09-01"` (date, RFC 3339 or epoch ms) |
| a run's rows | `{"run_id": "<run id from line 1>", "view": "periods"}` — or `"instruments"`, `"trades"`, `"notes"`; `"arm": "capped"`, `"limit": 20` |
| history while tuning (ends at the split) | `{"instrument": "hyperliquid:xyz:TSLA", "interval": "1h", "from": "2026-04-01", "to": "2026-07-01"}` (`market_history`; add `"fetch": true` for a missing range) |

`split` is a string (`time:<t>` or `instruments:<id>,<id>`), never an object; `strategy` and `spec` never together; `run_id` never with a run's arguments; unknown keys are errors. A tool error names every problem — fix them all in one retry. A holdout read prints the split lines: `in-sample n mean ci95 · holdout n mean ci95`. Calendar-window kinds (`weekend_window`, `daily_window` with `trading`) need the stock universe (`@xyz_stocks`) and the `us_equity` calendar; crypto trades 24/7 (use `move_trigger`, `funding_carry`, `pair_spread`).

## Protocol

| Step | Do |
|---|---|
| 1 Hypothesis | one sentence + the mechanism (who is forced to trade, why the price should revert / continue) |
| 2 Data | pick the split first (holdout = the last third). `market_history` for a few names with `to` = the split: coverage, gaps, funding present? Never read prices after the split before step 5 — that is a holdout peek |
| 3 Spec | the smallest spec that expresses it (kinds below); no code |
| 4 In-sample | `backtest` with `split` and no `holdout`: the tool runs and shows the in-sample half only. Tune at most 3–5 variants on it; count every variant you tried; read a variant's `periods` / `instruments` rows by its run id |
| 5 Holdout | ONE call: the chosen variant + `"holdout": true`; read its holdout half. The text counts holdout reads per spec (`holdout read #n`) and per split: #1 is the test. A variant changed or picked after a holdout read is fitted to it — say so, and report every read |
| 6 Robustness | the opposite `direction` (placebo, in-sample — on the holdout it is another counted read), shifted times, `mean_ex_best5_bps`, `best2_periods_share`, the run's `periods` and `instruments` rows |
| 7 Report | table: run id, n trades, periods, mean net bps, 95 % CI, hit rate, Σ USD, max drawdown — in-sample vs holdout, with the holdout read number; then the verdict |

| Verdict | When |
|---|---|
| promising | on the first holdout read: holdout mean net > 0, CI excludes 0, ≥ 30 trades over ≥ 8 periods, not carried by ≤ 2 periods, placebo ≤ 0 |
| weak | holdout mean > 0 but the CI spans 0, or carried by a few periods, or only after more than one holdout read |
| no-go | holdout mean ≤ 0, or the edge is smaller than costs |

## Critic

When a result fails, say which: no edge (gross ≈ 0) · edge eaten by costs (gross > 0, net ≤ 0) · funding drag · one regime / few periods · data gaps (skips by reason) · lookahead suspicion. Point at the run's rows: `{"run_id": "<run id>", "view": "periods"}` (which periods carried it), `"instruments"`, `"trades"` (the outliers), `"notes"` (split adjustments, missing series, skips).

## Spec kinds (`docs/xlab-2026-10-01.md` § 5)

| Kind | Key params |
|---|---|
| `weekend_window` | `calendar`, `direction` fade/follow, `min_abs_signal_bps`, `top_n`, offsets in minutes |
| `daily_window` | `days` all/weekdays/trading, `tz`, `anchor`, `entry`, `exit` (`HH:MM`), `direction`, `min_abs_signal_bps`, `top_n` |
| `move_trigger` | `lookback_bars`, `threshold_bps`, `min_volume_ratio`, `volume_baseline_bars`, `direction`, `hold_bars`, `cooldown_bars`, `take_profit_bps`, `stop_loss_bps` |
| `funding_carry` | `min_apr_pct`, `exit_apr_pct`, `hold_hours` |
| `pair_spread` | `legs` (2 ids), `lookback_bars`, `entry_z`, `exit_z`, `max_hold_bars` |
| `event_window` | `events` [{instrument, t (publication time), label}], `entry_delay_mins`, `direction`, `exit_after_mins` or `exit_at` + `tz` |

Every spec also takes `name`, `universe` (ids or `"@xyz_stocks"`, `"@crypto"`), `interval` (`1h` default for stock perps), optional `notional_usd`, `exclude`, `costs`, `min_entry_trades` (skip a candidate whose entry bar counted fewer trades — thin xyz names keep no-trade hours as flat, stale bars; skips show as `thin_entry`). A tool run holds at most 50 000 candidates: a rule that fires on every bar is refused — make it pickier (`threshold_bps`, `cooldown_bars`, `top_n`, `min_entry_trades`).

## Rules

- Levels: you create level-2 capabilities (specs). You never create or call anything that moves money.
- Time integrity: you may know what happened after a test window's start; that knowledge is a leak. Trust the backtest, decide on the holdout — read once.
- Numbers come from tool rows; cite the run id. A missing or failed read is unknown, never 0.
- Ids (instruments, run ids, hashes) always in full.
