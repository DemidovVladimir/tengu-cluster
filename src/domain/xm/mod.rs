//! xmarket domain — pure policy for the paper slice. No IO; times are inputs.
//! Ids are `<venue>:<native id>` verbatim (`hyperliquid:xyz:TSLA`) — never
//! shortened.
//!
//! | File | Holds | Item |
//! |---|---|---|
//! | `cost.rs` | HL tick / lot rounding, fee schedules (tiers, staking, HIP-3 deployer scale + growth mode), funding carry, EVM gas, taker cost of a depth walk, edge after costs | `risk-calc-costs` |
//! | `ledger.rs` | paper positions, cash, average-cost P&L, mark-to-market, exposure (per underlying / venue), leverage, HL funding; `paper_positions/1:<account>` | `risk-paper-ledger-domain` |
//! | `paper.rs` | paper fill engine: market / IOC orders vs an L2 book, HL rejection codes, partial fills, latency jitter | `risk-paper-fill-engine` |
//! | `risk.rs` | the pre-trade gate: `OrderIntent` + `RiskContext` + `RiskLimits` ⇒ `RiskVerdict` (every §28 rule a `Check`, fail closed), §29 `Lifecycle` | `risk-gate-domain` |
//!
//! Depth walks and depth within N bps: `domain/book.rs` (venue-neutral).

pub(crate) mod cost;
pub(crate) mod ledger;
pub(crate) mod paper;
pub(crate) mod risk;
