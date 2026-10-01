//! Backtests — history-first research (xlab, `docs/xlab-2026-10-01.md`
//! § 5–7). Pure: series come in (`domain/marketdata.rs`), trades, statistics
//! and the report go out; the store, the run dir and Jev are outside
//! (`application/backtest/`).
//!
//! | File | Holds |
//! |---|---|
//! | `costs.rs` | the cost model a spec or `[backtest.costs]` sets: taker fee, half-spread (fixed / Abdi–Ranaldo / archive ctx), slippage, funding on or off |

pub(crate) mod costs;
