//! Software Opportunity Engine (SOE) — pure domain: no IO, clock, network or
//! LLM (`docs/soe-2026-10-08.md`). Grows per phase: O1 adds the records,
//! economics, gates and ranking beside `value`. The operator's parameters
//! never live in the repo; they come from the signed private profile at run
//! time (loaded by `config/soe.rs`).
//!
//! | File | Holds |
//! |---|---|
//! | `value.rs` | exact money (`Minor`, `Money`, `Currency`), `Bps`, FX (`FxRate`, `Converted`), rounding (`Flow`), unknown-safe `Est` + `Assumption`, `Scenario`, `SchemaTag`, refusal codes |
//! | `record.rs` | `SoeRecord` (header: schema, id, version), `from_toml` / `from_json` / `validate`, `Problems`, `Tier`, `Verdict` |
//! | `profile.rs` | `OperatorProfile` (`soe.operator_profile/1`, private, signed) + `OperatorCapability`, `RankKey` |
//! | `opportunity.rs` | `Opportunity` (`soe.opportunity/1`): `Mechanism`, `RevenueModel`, `EconomicInputs`, `Jurisdictions`, `DealReview` |
//! | `risk.rs` · `experiment.rs` | `RiskAssessment` (max loss, risks, concentration, bounds) · `ExperimentSpec` (threshold, stages, `ApprovalKind`) |
//! | `episode.rs` | `OpportunityEpisode` (`soe.opportunity_episode/1`, private; lineage `Quality` / `Lesson` / `quadrant_of`) |
//! | `portfolio.rs` | `WeeklyPortfolio` (`soe.weekly_portfolio/1`, `PortfolioAction`, `IsoWeek`) · `PublicBrief` (`soe.public_brief/1`, allow-listed keys) |
//!
//! | Rule | Why |
//! |---|---|
//! | Money is integer minor units from decimal text, never `f64` | a threshold rule holds exactly at its boundary (the trading desk's `f64` + tolerance does not) |
//! | Unknown stays unknown | a missing price, tax, hour or probability is never 0 or "safe" |
//! | Rounding never flatters a candidate | inflows floor, outflows ceil |
//! | No built-in operator parameter | PRD § 14: only the signed private profile sets caps, targets, rates and the rank order; no profile field has a serde default |
//! | Every record carries `schema = "soe.<record>/1"`, `id`, `version`; unknown keys refused | an unknown version or key is refused, never guessed |
//! | Provenance is `domain/source/` (O2); imports go `soe` → `source`, never back | one provenance type for every domain: `Opportunity.signals` lists source record ids, no SOE signal type |
//! | Private records stay private | profile, episodes and portfolios live under `<TENGU_HOME>/state/soe/`; only a `PublicBrief` may leave |

pub(crate) mod episode;
pub(crate) mod experiment;
pub(crate) mod opportunity;
pub(crate) mod portfolio;
pub(crate) mod profile;
pub(crate) mod record;
pub(crate) mod risk;
pub(crate) mod value;
