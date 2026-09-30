//! xmarket domain — pure policy for the paper slice. No IO; times are inputs.
//! Ids are `<venue>:<native id>` verbatim (`hyperliquid:xyz:TSLA`) — never
//! shortened.
//!
//! | File | Holds | Item |
//! |---|---|---|
//! | `cost.rs` | HL tick / lot rounding, fee schedules (tiers, staking, HIP-3 deployer scale + growth mode), funding carry, EVM gas, taker cost of a depth walk, edge after costs | `risk-calc-costs` |
//!
//! Depth walks and depth within N bps: `domain/book.rs` (venue-neutral).

pub(crate) mod cost;
