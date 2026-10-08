//! Software Opportunity Engine (SOE) — pure domain: no IO, clock, network or
//! LLM (`docs/soe-2026-10-08.md`). Grows per phase: O1 adds the records,
//! economics, gates and ranking beside `value`. The operator's parameters
//! never live in the repo; they come from the signed private profile at run
//! time.
//!
//! | File | Holds |
//! |---|---|
//! | `value.rs` | exact money (`Minor`, `Money`, `Currency`), `Bps`, FX (`FxRate`, `Converted`), rounding (`Flow`), unknown-safe `Est` + `Assumption`, `Scenario`, `SchemaTag` |
//!
//! | Rule | Why |
//! |---|---|
//! | Money is integer minor units from decimal text, never `f64` | a threshold rule holds exactly at its boundary (the trading desk's `f64` + tolerance does not) |
//! | Unknown stays unknown | a missing price, tax, hour or probability is never 0 or "safe" |
//! | Rounding never flatters a candidate | inflows floor, outflows ceil |
//! | No built-in operator parameter | PRD § 14: only the signed private profile sets caps, targets, rates and the rank order |
//! | Every record carries `schema = "soe.<record>/1"` | an unknown version is refused, never guessed |
//! | Provenance is `domain/source/` (O2); imports go `soe` → `source`, never back | one provenance type for every domain |

pub(crate) mod value;
