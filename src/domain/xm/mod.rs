//! xmarket domain — pure policy for the paper slice. No IO; times are inputs.
//! Ids are `<venue>:<native id>` verbatim (`hyperliquid:xyz:TSLA`) — never
//! shortened.
//!
//! | File | Holds | Item |
//! |---|---|---|
//! | `cost.rs` | HL tick / lot rounding, fee schedules (tiers, staking, HIP-3 deployer scale + growth mode), funding carry, EVM gas, taker cost of a depth walk, edge after costs | `risk-calc-costs` |
//! | `exec.rs` | exec-tool orders: `client_order_id` rule, venue facts from the market rows, the `paper_fill/1:<account>:<client_order_id>` row | `risk-gate-enforcement` |
//! | `exits.rs` | exit rules: deadline, max hold, stop-loss, take-profit (`exit_due`), the exit idempotency key, the `xm_exits/1:<account>` row | `x-exit-rules` |
//! | `ledger.rs` | paper positions, cash, average-cost P&L, mark-to-market, exposure (per underlying / venue), leverage, HL funding; `paper_positions/1:<account>` | `risk-paper-ledger-domain` |
//! | `paper.rs` | paper fill engine: market / IOC orders vs an L2 book, HL rejection codes, partial fills, latency jitter | `risk-paper-fill-engine` |
//! | `risk.rs` | the pre-trade gate: `OrderIntent` + `RiskContext` + `RiskLimits` ⇒ `RiskVerdict` (every §28 rule a `Check`, fail closed), §29 `Lifecycle` | `risk-gate-domain` |
//! | `risk_state.rs` | account risk state: halts (daily / total loss, operator, kill-switch file), UTC day roll, `risk_state/1:<account>` | `risk-kill-switch` |
//!
//! Depth walks and depth within N bps: `domain/book.rs` (venue-neutral).

pub(crate) mod cost;
pub(crate) mod exec;
pub(crate) mod exits;
pub(crate) mod ledger;
pub(crate) mod paper;
pub(crate) mod risk;
pub(crate) mod risk_state;
