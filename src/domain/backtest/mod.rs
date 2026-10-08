//! Backtests — history-first research (xlab, `docs/xlab-2026-10-01.md`
//! § 5–7). Pure: series come in (`domain/marketdata.rs`), trades, statistics
//! and the report go out; the store, the run dir and Jev are outside
//! (`application/backtest/`). Flow: `spec::StrategySpec::from_value` →
//! `engine::candidates` (→ the Jev gate, outside) → `engine::simulate` per
//! arm → `report::BacktestReport`.
//!
//! | File | Holds |
//! |---|---|
//! | `spec.rs` | strategy specs — six kinds, `deny_unknown_fields`, bounds naming the field — and `SplitSpec` (`time:` / `instruments:`) |
//! | `engine.rs` | `candidates` (checks, then `kinds.rs`) and `simulate` (research arm / `[risk]`-capped arm): `MarketData`, `RunParams`, `Candidate`, `Trade`, `RiskCaps`, `ArmResult` |
//! | `kinds.rs` | the decisions of each of the six kinds, as-of t, with skips and data notes |
//! | `fills.rs` | cost per side (fee, half-spread fixed / Abdi–Ranaldo / ctx, slippage), funding over a hold, the path-dependent exits, the pair spread series |
//! | `costs.rs` | the cost model a spec or `[backtest.costs]` sets: taker fee, half-spread (fixed / Abdi–Ranaldo / archive ctx), slippage, funding on or off |
//! | `features.rs` | features as-of t (returns, vol, volume ratio, trades, funding APR, half-spread, hour of week) for the Jev gate |
//! | `gate.rs` | the Jev gate arm's pure half: the event a candidate shows Jev, `GateClass` of a verdict, p(take), `GateSummary` (counts, cache, cost, calibration, jev − rules) + its `report.md` section and row features |
//! | `evaluation.rs` | decision evaluation (Phase 6): one row per candidate of a gated run — rules · Jev · HOLD on the same candidates, paired per-candidate CIs, the Jev verdict (`tengu evidence evaluate`) |
//! | `stats.rs` | `Summary` (mean / median / t / hit, USD, drawdown, Sharpe, seeded cluster-bootstrap CI, robustness), paired arm difference, calibration |
//! | `report.rs` | `BacktestReport` = `report.json` + row `backtest/1:<run id>`, `report.md`, compact CLI / tool text |
//! | `testkit.rs` · `checks.rs` | tests only: fixtures; time integrity (§ 39) + the rule W golden |

#[cfg(test)]
pub(crate) mod checks;
pub(crate) mod costs;
pub(crate) mod engine;
pub(crate) mod evaluation;
pub(crate) mod features;
pub(crate) mod fills;
pub(crate) mod gate;
pub(crate) mod kinds;
pub(crate) mod report;
pub(crate) mod spec;
pub(crate) mod stats;
#[cfg(test)]
pub(crate) mod testkit;
