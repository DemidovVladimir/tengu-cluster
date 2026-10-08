//! Software Opportunity Engine (SOE) — pure domain: no IO, clock, network or
//! LLM (`docs/soe-2026-10-08.md`). Grows per phase: O1 holds the values,
//! records, economics, gates, capability fit and the ranking primitives; O0
//! the eval-case format; O3 the typed Architect / Critic data, the
//! allocation (the one portfolio builder), the memo, the forecast log, the
//! Operator Review #2 packet and the cycle's ops. The operator's parameters
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
//! | `economics.rs` | `scenarios`: downside / base / upside `ScenarioMetrics` (`Metric` known or the fields it lacks, `Payback`), `expected_loss`, `inputs_sha256`, `ECONOMICS_VERSION` |
//! | `gates.rs` | `gates`: PRD § 7.2 hard gates → `GateVerdict` (`PASS` / `HOLD` / `REJECT`, `GateFailure` codes), `CitedRecord` (what a cited source record shows at `as_of`), `next_information` |
//! | `matching.rs` | `fit`: how the profile's capabilities cover `requires_skills` at the decision (`active_at`, `FitLevel` `PROVEN` `CLAIMED` `STALE` `MISSING` · `UNSTATED`) |
//! | `rank.rs` | `assess` (verdict, figures, fit, the eight `RankKey` values), `rank` by the profile's `rank_order`, `explain_order`, `rank_moves`, `perturb` / `sensitivity`; week blocks: `current_versions`, `ranked_row`, `gated_rows`, `week_next_information`, `hold_week`, `unallocated_week` (`tengu soe portfolio` until it moves to `allocate`) |
//! | `eval.rs` | `EvalCase` (`soe.eval_case/1`: dated, synthetic, profile-bound) + `run_case` (expected vs answered, no look-ahead, a valid `HOLD` week) |
//! | `observe.rs` | O3 Observe: an O2 `EvidencePacket` → `EvidenceIndex` (per record: event, class, url, times, independent confirmations) + `cited_views` (`CitedRecord`s; a single non-primary origin is a trigger) |
//! | `proposal.rs` | O3 Architect: `ProposalDraft` (model-written: opportunity, per-input `Basis` `FACT` / `INFERENCE`, novelty, forecast) → `MechanismProposal` (`soe.mechanism_proposal/1`, `Provenance` stamped by the tool); `COMPUTED_KEYS` refused; `check` against the packet; `unsupported` |
//! | `challenge.rs` | O3 Critic: `ChallengeDraft` → `Challenge` (`soe.challenge/1`); `apply` merges only toward the conservative side (`WIDEN` per-field `direction`, `UNKNOWN`, `BLOCK_GATE` holds) |
//! | `allocate.rs` | O3 Allocate — the one portfolio builder (C4): gate verdicts → `REPRICE` / `REJECT` / `DILIGENCE` / `CHEAP_TEST` / `HOLD`, `PASS` ranked by the profile's order and filled under weekly hours + tranche (`CONTINUE_ACTIVE` / `CHEAP_TEST` / `HOLD`); `decide_week` runs observe → check → challenge → assess → allocate |
//! | `memo.rs` | O3 Report: `render_memo` — verdict, facts (confirmations), inference (fenced), computed (portfolio fields only), unsupported claims, unknowns, challenges, next information |
//! | `forecast.rs` | O4 forecast log: `ForecastItem`, `Forecast` (`soe.forecast/1`, frozen with the portfolio), hash-chained `LogLine`s (`verify_chain`), `resolve` (evidence after the freeze only), integer `calibration` |
//! | `review.rs` | O4 Operator Review #2: `CycleGrade` (`soe.cycle_grade/1`), `ReviewPacket::of` (cycles, calibration, misses, ops, sources, O6A / O6B lanes), roadmap § 14 `stop_flags` |
//! | `ops.rs` | O4 cycle ops: `StageRun`s, source failures, token totals, `Cost` (unknown without prices) |
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
//! | A model writes data, never a computed field | proposals and challenges are drafts (`COMPUTED_KEYS` refused); economics, gates, rank keys, confirmations and actions are computed here; a Critic only makes a candidate worse |

pub(crate) mod allocate;
pub(crate) mod challenge;
pub(crate) mod economics;
pub(crate) mod episode;
pub(crate) mod eval;
pub(crate) mod experiment;
pub(crate) mod forecast;
pub(crate) mod gates;
pub(crate) mod matching;
pub(crate) mod memo;
pub(crate) mod observe;
pub(crate) mod opportunity;
pub(crate) mod ops;
pub(crate) mod portfolio;
pub(crate) mod profile;
pub(crate) mod proposal;
pub(crate) mod rank;
pub(crate) mod record;
pub(crate) mod review;
pub(crate) mod risk;
pub(crate) mod value;
