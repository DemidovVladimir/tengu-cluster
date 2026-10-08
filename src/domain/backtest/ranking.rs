//! Strategy ranking (`docs/strategy-ranking-automation-2026-10-08.md` SR-2 /
//! SR-3): the runs of one ranking date under a ranking contract
//! (`domain/lineage/ranking.rs`) in, one ranking out — per cohort, weakest
//! to strongest. Pure: the coordinator (`application/ranking/`, SR-5) reads
//! the run dirs, the registry and the clock, and writes the files.
//!
//! | Step | Rule |
//! |---|---|
//! | [`RunFacts::from_report`] | a `report.json`, the file's sha256, its state and the contract's arm → what a ranking reads: the arm's `Summary`, the window, the cohort identity, whether a split read the holdout; [`RunFacts::run`] = `run:<state>/<run id>` (what retention keeps) |
//! | [`StrategyStanding::of`] | the registry on a spec hash: the variants carrying it; `evidence_tier` = the highest result class of their PASS experiments (validity ≠ `INVALID_FOR_STRATEGY_INFERENCE`, class ≠ `NONE`), else `NONE`; `verdict` = their weakest status ([`RunVerdict`]), `UNREGISTERED` when none |
//! | [`select`] | each [`RunInput`] → chosen, or [`Rejected`] with the first reason of the table below; one run per strategy and cohort (the newest run id) |
//! | [`rank`] | per cohort, rows ascending by the rating tuple, then [`TieBreak`]; rank 1 = the weakest; status `INCOMPLETE` when a listed strategy failed under `on_missing = INCOMPLETE`, or none was evaluated |
//! | Rating tuple | one integer per `[rating] order` term: `evidence_tier` / `verdict` as ordinals ([`tier_ordinal`], [`RunVerdict::ordinal`]), a number as `round(x / quantum)`; a leading `-` negates |
//! | [`Ranking::content_sha256`] | canonical sha256 of the ranking without `run`, `report_sha256`, `generated_at_ms`: a rerun on the same data (new run ids) hashes the same |
//! | [`Ranking::render_markdown`] | `ranking.md`: the header, one table per cohort weakest → strongest, the ineligible / failed / dropped lists — ids whole |
//! | [`Ranking::render_compact`] | a few lines (`tengu ranking run`, a tool result): status, window, rows weakest → strongest, the failed / ineligible / dropped reasons — ids whole |
//!
//! | Reason (check order) | List | When |
//! |---|---|---|
//! | `not_in_contract` | dropped | the strategy is not in `strategies` |
//! | `failed:<stage>` · `stale` · `missing` | failed | the coordinator failed at `<stage>` · an instrument's data is older than `[freshness]` allows · a listed strategy has no input |
//! | `holdout_present` | ineligible | a split (the report's, or an arm's halves): the holdout was read |
//! | `arm_missing` | ineligible | the report has no arm of the contract's name |
//! | `cohort_unknown:<field>` | ineligible | a cohort field without a value: a report written before 2026-10-08 has no identity (`generation` first); `data_through_ms` absent |
//! | `cohort_mismatch:<field>` | ineligible | `from_ms` ≠ the contract's `from`; `to_ms` or `data_through_ms` ≠ the cutoff; the evidence class ≠ the contract's |
//! | `superseded_run` | dropped | a newer run of the strategy in the same cohort is the one judged (an ineligible newer run is not replaced by an older one) |
//! | `excluded_status` | ineligible | a variant in `exclude_status` |
//! | `insufficient_trades` · `insufficient_periods` · `funding_incomplete` | ineligible | `n` < `min_trades` · `n_periods` < `min_periods` · `funding_incomplete` > `max_funding_incomplete` |
//! | `metric_missing:<key>` · `non_finite:<key>` | ineligible | a rated key without a value (never read as 0) · NaN or ±∞ |
//!
//! Evaluation is `NOT_GATED` on every row: a ranking runs the rules arms
//! only (no Jev gate, `RANKED_ARMS`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::backtest::report::BacktestReport;
use crate::domain::backtest::stats::Summary;
use crate::domain::canonical::canonical_sha256;
use crate::domain::evidence::EvidenceClass;
use crate::domain::lineage::experiment::{Validity, VerdictValue};
use crate::domain::lineage::query::{enum_name, variants_by_hash};
use crate::domain::lineage::ranking::{
    CohortField, MissingPolicy, RankingContract, RatingKey, RatingTerm, TieBreak,
};
use crate::domain::lineage::value::Locator;
use crate::domain::lineage::variant::VariantStatus;
use crate::domain::lineage::Registry;
use crate::domain::marketdata::fmt_time;
use crate::domain::tz::Zone;

/// `ranking.json`'s schema tag.
pub const SCHEMA: &str = "strategy_ranking/1";

/// The cohort value of `generation` in a report of an unbound sandbox.
pub const UNBOUND: &str = "NONE";

/// Fields that change on a rerun over the same data:
/// [`Ranking::content_sha256`] leaves them out at every depth.
const VOLATILE: [&str; 3] = ["run", "report_sha256", "generated_at_ms"];

/// What a row's evaluation column says: the rules arms only, no Jev gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Evaluation {
    NotGated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RankingStatus {
    Complete,
    Incomplete,
}

/// The weakest status of a run's variants; `UNREGISTERED` when none
/// carries its spec hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunVerdict {
    Rejected,
    Unregistered,
    Inconclusive,
    Superseded,
    Registered,
    Active,
    Surviving,
}

impl RunVerdict {
    /// The rating ordinal: `REJECTED` 0 < `UNREGISTERED` = `INCONCLUSIVE` =
    /// `SUPERSEDED` 1 < `REGISTERED` = `ACTIVE` 2 < `SURVIVING` 3.
    pub fn ordinal(self) -> i64 {
        match self {
            RunVerdict::Rejected => 0,
            RunVerdict::Unregistered | RunVerdict::Inconclusive | RunVerdict::Superseded => 1,
            RunVerdict::Registered | RunVerdict::Active => 2,
            RunVerdict::Surviving => 3,
        }
    }
}

impl From<VariantStatus> for RunVerdict {
    fn from(s: VariantStatus) -> Self {
        match s {
            VariantStatus::Registered => RunVerdict::Registered,
            VariantStatus::Active => RunVerdict::Active,
            VariantStatus::Surviving => RunVerdict::Surviving,
            VariantStatus::Rejected => RunVerdict::Rejected,
            VariantStatus::Inconclusive => RunVerdict::Inconclusive,
            VariantStatus::Superseded => RunVerdict::Superseded,
        }
    }
}

/// `evidence_tier` as a rating ordinal: `NONE` 0, then the ladder
/// `DEVELOPMENT` 1 … `LIVE_PRODUCTION` 5.
pub fn tier_ordinal(c: EvidenceClass) -> i64 {
    match c {
        EvidenceClass::None => 0,
        EvidenceClass::Development => 1,
        EvidenceClass::Holdout => 2,
        EvidenceClass::ForwardPaper => 3,
        EvidenceClass::LiveMicro => 4,
        EvidenceClass::LiveProduction => 5,
    }
}

/// The registry's word on one spec hash (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyStanding {
    /// Every variant carrying the hash → its status.
    pub variants: BTreeMap<String, VariantStatus>,
    pub evidence_tier: EvidenceClass,
    pub verdict: RunVerdict,
}

impl StrategyStanding {
    /// Module table: the standing of `spec_sha256` in `reg`.
    pub fn of(reg: &Registry, spec_sha256: &str) -> Self {
        let variants: BTreeMap<String, VariantStatus> = variants_by_hash(reg)
            .get(spec_sha256)
            .map(|vs| vs.iter().map(|v| (v.id.clone(), v.status)).collect())
            .unwrap_or_default();
        let evidence_tier = reg
            .experiments
            .values()
            .filter(|x| {
                variants.contains_key(&x.variant)
                    && x.verdict.value == VerdictValue::Pass
                    && x.validity != Some(Validity::InvalidForStrategyInference)
            })
            .flat_map(|x| x.results.iter().map(|r| r.class))
            .filter(|c| *c != EvidenceClass::None)
            .max_by_key(|c| tier_ordinal(*c))
            .unwrap_or(EvidenceClass::None);
        let verdict = variants
            .values()
            .map(|s| RunVerdict::from(*s))
            .min_by_key(|v| (v.ordinal(), *v))
            .unwrap_or(RunVerdict::Unregistered);
        Self {
            variants,
            evidence_tier,
            verdict,
        }
    }
}

/// What a ranking reads of one run (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFacts {
    pub strategy: String,
    pub kind: String,
    pub interval: String,
    /// The state dir's name (`xlab`): the run is `run:<state>/<run id>`.
    pub state: String,
    pub run_id: String,
    /// sha256 hex of the `report.json` bytes read.
    pub report_sha256: String,
    pub spec_sha256: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub data_through_ms: Option<i64>,
    pub generation: Option<String>,
    pub instruments_sha256: Option<String>,
    pub costs_sha256: Option<String>,
    /// A split ran (the report's, or an arm's halves): the holdout was read.
    pub holdout: bool,
    /// The contract's arm, and its summary (`None`: the report has no such arm).
    pub arm: String,
    pub summary: Option<Summary>,
}

impl RunFacts {
    /// Module table: `report` (read from `run:<state>/<run id>`, file sha256
    /// `report_sha256`) as seen by a ranking of `arm`.
    pub fn from_report(
        report: &BacktestReport,
        state: &str,
        report_sha256: &str,
        arm: &str,
    ) -> Self {
        Self {
            strategy: report.strategy.clone(),
            kind: report.kind.clone(),
            interval: report.interval.clone(),
            state: state.to_string(),
            run_id: report.run_id.clone(),
            report_sha256: report_sha256.to_string(),
            spec_sha256: report.spec_sha256.clone(),
            from_ms: report.from_ms,
            to_ms: report.to_ms,
            data_through_ms: report.data_through_ms,
            generation: report.generation.clone(),
            instruments_sha256: report.instruments_sha256.clone(),
            costs_sha256: report.costs_sha256.clone(),
            holdout: report.split.is_some() || report.arms.values().any(|a| a.split.is_some()),
            arm: arm.to_string(),
            summary: report.arms.get(arm).map(|a| a.summary.clone()),
        }
    }

    /// `run:<state>/<run id>`.
    pub fn run(&self) -> String {
        Locator::Run {
            state: self.state.clone(),
            run_id: self.run_id.clone(),
            file: None,
        }
        .to_string()
    }

    fn evidence_class(&self) -> EvidenceClass {
        if self.holdout {
            EvidenceClass::Holdout
        } else {
            EvidenceClass::Development
        }
    }

    /// `field`'s value, `None` when the report does not say. A report with
    /// its identity (2026-10-08 on) and no generation is unbound
    /// ([`UNBOUND`]); one without it cannot tell.
    fn cohort_value(&self, field: CohortField) -> Option<String> {
        let identity = self.instruments_sha256.is_some() && self.costs_sha256.is_some();
        match field {
            CohortField::Generation => identity.then(|| {
                self.generation
                    .clone()
                    .unwrap_or_else(|| UNBOUND.to_string())
            }),
            CohortField::EvidenceClass => Some(enum_name(&self.evidence_class())),
            CohortField::Arm => Some(self.arm.clone()),
            CohortField::InstrumentsSha256 => self.instruments_sha256.clone(),
            CohortField::CostsSha256 => self.costs_sha256.clone(),
            CohortField::Interval => Some(self.interval.clone()),
            CohortField::FromMs => Some(self.from_ms.to_string()),
            CohortField::ToMs => Some(self.to_ms.to_string()),
            CohortField::DataThroughMs => self.data_through_ms.map(|t| t.to_string()),
        }
    }
}

/// One strategy's input to a ranking date.
#[derive(Debug, Clone, PartialEq)]
pub enum RunInput {
    /// A finished run and the registry's word on its spec
    /// ([`RunInput::of`]).
    Run {
        facts: Box<RunFacts>,
        standing: StrategyStanding,
    },
    /// No run: the coordinator failed at `stage` (`backtest`, `evaluate` …).
    Failed {
        strategy: String,
        stage: String,
        error: String,
    },
    /// No run: an instrument's newest bar is older than `[freshness]` allows.
    Stale { strategy: String, detail: String },
}

impl RunInput {
    /// A finished run's input.
    pub fn of(facts: RunFacts, standing: StrategyStanding) -> Self {
        RunInput::Run {
            facts: Box::new(facts),
            standing,
        }
    }

    pub fn strategy(&self) -> &str {
        match self {
            RunInput::Run { facts, .. } => &facts.strategy,
            RunInput::Failed { strategy, .. } | RunInput::Stale { strategy, .. } => strategy,
        }
    }

    fn run(&self) -> Option<String> {
        match self {
            RunInput::Run { facts, .. } => Some(facts.run()),
            _ => None,
        }
    }
}

/// A cohort: [`CohortField`] → its value (sha256s and ids whole, ms as
/// integers).
pub type CohortKey = BTreeMap<CohortField, String>;

/// Which list of a [`Ranking`] a rejection goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// Evaluated, not ranked.
    Ineligible,
    /// A listed strategy without a run: what `on_missing` reads.
    Failed,
    /// A run that never competed: not in the contract, or superseded.
    Dropped,
}

/// A run or strategy left out, and why (module table).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rejected {
    pub strategy: String,
    /// `run:<state>/<run id>`; none for a strategy without a run.
    pub run: Option<String>,
    pub reason: String,
    pub detail: String,
}

impl Rejected {
    fn new(strategy: &str, run: Option<String>, reason: impl Into<String>, detail: String) -> Self {
        Self {
            strategy: strategy.to_string(),
            run,
            reason: reason.into(),
            detail,
        }
    }

    pub fn bucket(&self) -> Bucket {
        let r = self.reason.as_str();
        if r.starts_with("failed:") || r == "stale" || r == "missing" {
            Bucket::Failed
        } else if r == "not_in_contract" || r == "superseded_run" {
            Bucket::Dropped
        } else {
            Bucket::Ineligible
        }
    }
}

/// A run that passed every check, with its cohort and rating tuple.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub facts: RunFacts,
    pub summary: Summary,
    pub standing: StrategyStanding,
    pub key: CohortKey,
    /// One integer per `[rating] order` term (module table).
    pub rating: Vec<i64>,
}

/// [`select`]'s result.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// The contract's `from` (ms).
    pub from_ms: Option<i64>,
    pub cutoff_ms: i64,
    pub chosen: Vec<Chosen>,
    /// Sorted by strategy, run, reason.
    pub rejected: Vec<Rejected>,
}

/// `(reason, detail)` of a refused run.
type Refusal = (String, String);

/// The run's cohort under `c`, or the first refusal of the table up to
/// `cohort_mismatch`.
fn cohort_of(c: &RankingContract, cutoff_ms: i64, f: &RunFacts) -> Result<CohortKey, Refusal> {
    if f.holdout {
        return Err((
            "holdout_present".into(),
            "a split run read the holdout: a ranking reads development evidence only".into(),
        ));
    }
    if f.summary.is_none() {
        return Err((
            "arm_missing".into(),
            format!("the report has no `{}` arm", f.arm),
        ));
    }
    let mut key = CohortKey::new();
    for field in &c.cohort {
        let Some(v) = f.cohort_value(*field) else {
            return Err((
                format!("cohort_unknown:{}", enum_name(field)),
                "the report does not record it (written before 2026-10-08?)".into(),
            ));
        };
        key.insert(*field, v);
    }
    let ms =
        |t: Option<i64>| t.map_or_else(|| "none".to_string(), |t| format!("{t} ({})", fmt_time(t)));
    if Some(f.from_ms) != c.start_ms() {
        return Err((
            "cohort_mismatch:from_ms".into(),
            format!(
                "from {} ≠ the contract's {}",
                ms(Some(f.from_ms)),
                ms(c.start_ms())
            ),
        ));
    }
    if f.to_ms != cutoff_ms {
        return Err((
            "cohort_mismatch:to_ms".into(),
            format!(
                "to {} ≠ the cutoff {}",
                ms(Some(f.to_ms)),
                ms(Some(cutoff_ms))
            ),
        ));
    }
    match f.data_through_ms {
        None => {
            return Err((
                "cohort_unknown:data_through_ms".into(),
                "the report does not record it (written before 2026-10-08?)".into(),
            ))
        }
        Some(t) if t != cutoff_ms => {
            return Err((
                "cohort_mismatch:data_through_ms".into(),
                format!(
                    "data through {} ≠ the cutoff {}",
                    ms(Some(t)),
                    ms(Some(cutoff_ms))
                ),
            ))
        }
        Some(_) => {}
    }
    if f.evidence_class() != c.evidence_class {
        return Err((
            "cohort_mismatch:evidence_class".into(),
            format!(
                "{} ≠ the contract's {}",
                enum_name(&f.evidence_class()),
                enum_name(&c.evidence_class)
            ),
        ));
    }
    Ok(key)
}

/// A number rating key's value in `s`; `None` for the ordinal keys.
fn metric(s: &Summary, key: RatingKey) -> Option<f64> {
    match key {
        RatingKey::EvidenceTier | RatingKey::Verdict => None,
        RatingKey::Ci95LoBps => s.ci95_lo_bps,
        RatingKey::MeanNetBps => s.mean_net_bps,
        RatingKey::MedianNetBps => s.median_net_bps,
        RatingKey::HitRate => s.hit_rate,
        RatingKey::Sharpe => s.sharpe,
        RatingKey::TStat => s.t_stat,
        RatingKey::Best2PeriodsShare => s.best2_periods_share,
        RatingKey::MaxDrawdownBps => s.max_drawdown_bps,
        RatingKey::MeanExBest5Bps => s.mean_ex_best5_bps,
    }
}

/// `round(x / quantum)` (saturating at the i64 bounds).
fn quantize(x: f64, quantum: f64) -> i64 {
    (x / quantum).round() as i64
}

/// The rating tuple of an eligible run, or the first eligibility refusal
/// (module table, from `excluded_status`).
fn rate(c: &RankingContract, s: &Summary, st: &StrategyStanding) -> Result<Vec<i64>, Refusal> {
    let e = &c.eligibility;
    if let Some((id, status)) = st
        .variants
        .iter()
        .find(|(_, s)| e.exclude_status.contains(s))
    {
        return Err((
            "excluded_status".into(),
            format!("variant `{id}` is {}", enum_name(status)),
        ));
    }
    if (s.n as u64) < e.min_trades {
        return Err((
            "insufficient_trades".into(),
            format!("n {} < min_trades {}", s.n, e.min_trades),
        ));
    }
    if (s.n_periods as u64) < e.min_periods {
        return Err((
            "insufficient_periods".into(),
            format!("n_periods {} < min_periods {}", s.n_periods, e.min_periods),
        ));
    }
    if s.funding_incomplete as u64 > e.max_funding_incomplete {
        return Err((
            "funding_incomplete".into(),
            format!(
                "{} trades without every funding hour > max_funding_incomplete {}",
                s.funding_incomplete, e.max_funding_incomplete
            ),
        ));
    }
    let mut tuple = Vec::with_capacity(c.rating.order.len());
    for term in &c.rating.order {
        let v = match term.key {
            RatingKey::EvidenceTier => tier_ordinal(st.evidence_tier),
            RatingKey::Verdict => st.verdict.ordinal(),
            key => {
                let name = key.name();
                let Some(x) = metric(s, key) else {
                    return Err((
                        format!("metric_missing:{name}"),
                        format!("no {name} in the `{}` arm's summary", c.arm),
                    ));
                };
                if !x.is_finite() {
                    return Err((format!("non_finite:{name}"), format!("{name} = {x}")));
                }
                quantize(x, c.rating.quantum)
            }
        };
        tuple.push(if term.negate { v.saturating_neg() } else { v });
    }
    Ok(tuple)
}

/// Module table: the runs of one ranking date (cutoff `cutoff_ms`) checked
/// against `contract`; each run chosen or rejected with its first reason.
pub fn select(contract: &RankingContract, cutoff_ms: i64, runs: &[RunInput]) -> Selection {
    let listed: BTreeSet<&str> = contract.strategies.iter().map(String::as_str).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut rejected = Vec::new();
    let mut groups: BTreeMap<(String, CohortKey), Vec<(&RunFacts, &StrategyStanding)>> =
        BTreeMap::new();
    for input in runs {
        let strategy = input.strategy();
        if !listed.contains(strategy) {
            rejected.push(Rejected::new(
                strategy,
                input.run(),
                "not_in_contract",
                format!("`{strategy}` is not one of the contract's strategies"),
            ));
            continue;
        }
        seen.insert(strategy);
        match input {
            RunInput::Failed { stage, error, .. } => rejected.push(Rejected::new(
                strategy,
                None,
                format!("failed:{stage}"),
                error.clone(),
            )),
            RunInput::Stale { detail, .. } => {
                rejected.push(Rejected::new(strategy, None, "stale", detail.clone()))
            }
            RunInput::Run { facts, standing } => match cohort_of(contract, cutoff_ms, facts) {
                Err((reason, detail)) => {
                    rejected.push(Rejected::new(strategy, Some(facts.run()), reason, detail))
                }
                Ok(key) => groups
                    .entry((facts.strategy.clone(), key))
                    .or_default()
                    .push((&**facts, standing)),
            },
        }
    }
    for s in contract
        .strategies
        .iter()
        .filter(|s| !seen.contains(s.as_str()))
    {
        rejected.push(Rejected::new(
            s,
            None,
            "missing",
            "no run and no failure for this listed strategy".into(),
        ));
    }
    let mut chosen = Vec::new();
    for ((_, key), mut group) in groups {
        group.sort_by(|a, b| b.0.run_id.cmp(&a.0.run_id));
        group.dedup_by(|a, b| a.0.run_id == b.0.run_id);
        let (&(facts, standing), older) = group
            .split_first()
            .expect("a cohort group holds at least one run");
        for (f, _) in older {
            rejected.push(Rejected::new(
                &f.strategy,
                Some(f.run()),
                "superseded_run",
                "a newer run of the strategy in the same cohort is the one judged".into(),
            ));
        }
        let summary = facts
            .summary
            .clone()
            .expect("cohort_of refuses a run without the arm");
        match rate(contract, &summary, standing) {
            Err((reason, detail)) => rejected.push(Rejected::new(
                &facts.strategy,
                Some(facts.run()),
                reason,
                detail,
            )),
            Ok(rating) => chosen.push(Chosen {
                facts: facts.clone(),
                summary,
                standing: standing.clone(),
                key,
                rating,
            }),
        }
    }
    rejected.sort();
    Selection {
        from_ms: contract.start_ms(),
        cutoff_ms,
        chosen,
        rejected,
    }
}

/// What the coordinator stamps on a ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct RankingStamp {
    /// `pins::toml_digest` of the contract file (= its `[[sealed]]` row).
    pub contract_sha256: String,
    /// The ranking date in the contract's zone.
    pub date: NaiveDate,
    pub generated_at_ms: i64,
}

/// One ranked run (module table); `rank` 1 = the weakest of its cohort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedRow {
    pub rank: usize,
    pub strategy: String,
    pub kind: String,
    pub spec_sha256: String,
    /// `run:<state>/<run id>`.
    pub run: String,
    pub report_sha256: String,
    /// The variants carrying `spec_sha256`.
    pub variants: Vec<String>,
    pub evidence_tier: EvidenceClass,
    pub verdict: RunVerdict,
    pub evaluation: Evaluation,
    pub n: usize,
    pub n_periods: usize,
    pub mean_net_bps: Option<f64>,
    pub median_net_bps: Option<f64>,
    pub ci95_bps: Option<[f64; 2]>,
    pub hit_rate: Option<f64>,
    pub sharpe: Option<f64>,
    pub t_stat: Option<f64>,
    pub max_drawdown_bps: Option<f64>,
    pub best2_periods_share: Option<f64>,
    pub mean_ex_best5_bps: Option<f64>,
    pub data_through_ms: Option<i64>,
    /// The compared tuple, one integer per `rating_order` term.
    pub rating: Vec<i64>,
}

impl RankedRow {
    fn of(rank: usize, ch: Chosen) -> Self {
        let s = &ch.summary;
        Self {
            rank,
            run: ch.facts.run(),
            strategy: ch.facts.strategy,
            kind: ch.facts.kind,
            spec_sha256: ch.facts.spec_sha256,
            report_sha256: ch.facts.report_sha256,
            variants: ch.standing.variants.keys().cloned().collect(),
            evidence_tier: ch.standing.evidence_tier,
            verdict: ch.standing.verdict,
            evaluation: Evaluation::NotGated,
            n: s.n,
            n_periods: s.n_periods,
            mean_net_bps: s.mean_net_bps,
            median_net_bps: s.median_net_bps,
            ci95_bps: s.ci95_lo_bps.zip(s.ci95_hi_bps).map(|(lo, hi)| [lo, hi]),
            hit_rate: s.hit_rate,
            sharpe: s.sharpe,
            t_stat: s.t_stat,
            max_drawdown_bps: s.max_drawdown_bps,
            best2_periods_share: s.best2_periods_share,
            mean_ex_best5_bps: s.mean_ex_best5_bps,
            data_through_ms: ch.facts.data_through_ms,
            rating: ch.rating,
        }
    }
}

/// One cohort's rows, weakest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CohortRanking {
    pub key: CohortKey,
    pub rows: Vec<RankedRow>,
}

/// `ranking.json` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranking {
    pub schema: String,
    pub contract: String,
    pub contract_sha256: String,
    pub sandbox: String,
    pub date: NaiveDate,
    pub tz: String,
    /// Decisions over `[from_ms, cutoff_ms)`; `data_through_ms` = the cutoff.
    pub from_ms: Option<i64>,
    pub cutoff_ms: i64,
    pub arm: String,
    pub evidence_class: EvidenceClass,
    pub evaluation: Evaluation,
    pub on_missing: MissingPolicy,
    pub rating_order: Vec<RatingTerm>,
    pub quantum: f64,
    pub tie_break: TieBreak,
    pub generated_at_ms: i64,
    pub status: RankingStatus,
    /// Ordered by cohort key.
    pub cohorts: Vec<CohortRanking>,
    pub ineligible: Vec<Rejected>,
    pub failed: Vec<Rejected>,
    pub dropped: Vec<Rejected>,
}

/// Module table: `sel` ranked under `contract`, stamped by the coordinator.
pub fn rank(contract: &RankingContract, stamp: &RankingStamp, sel: Selection) -> Ranking {
    let mut by_key: BTreeMap<CohortKey, Vec<Chosen>> = BTreeMap::new();
    for ch in sel.chosen {
        by_key.entry(ch.key.clone()).or_default().push(ch);
    }
    let cohorts: Vec<CohortRanking> = by_key
        .into_iter()
        .map(|(key, mut rows)| {
            rows.sort_by(|a, b| {
                a.rating
                    .cmp(&b.rating)
                    .then_with(|| match contract.rating.tie_break {
                        TieBreak::StrategyAsc => (&a.facts.strategy, &a.facts.run_id)
                            .cmp(&(&b.facts.strategy, &b.facts.run_id)),
                    })
            });
            CohortRanking {
                key,
                rows: rows
                    .into_iter()
                    .enumerate()
                    .map(|(i, ch)| RankedRow::of(i + 1, ch))
                    .collect(),
            }
        })
        .collect();
    let (mut ineligible, mut failed, mut dropped) = (Vec::new(), Vec::new(), Vec::new());
    for r in sel.rejected {
        match r.bucket() {
            Bucket::Ineligible => ineligible.push(r),
            Bucket::Failed => failed.push(r),
            Bucket::Dropped => dropped.push(r),
        }
    }
    let evaluated = !cohorts.is_empty() || !ineligible.is_empty();
    let status =
        if !evaluated || (contract.on_missing == MissingPolicy::Incomplete && !failed.is_empty()) {
            RankingStatus::Incomplete
        } else {
            RankingStatus::Complete
        };
    Ranking {
        schema: SCHEMA.to_string(),
        contract: contract.id.clone(),
        contract_sha256: stamp.contract_sha256.clone(),
        sandbox: contract.sandbox.clone(),
        date: stamp.date,
        tz: contract.tz.clone(),
        from_ms: sel.from_ms,
        cutoff_ms: sel.cutoff_ms,
        arm: contract.arm.clone(),
        evidence_class: contract.evidence_class,
        evaluation: Evaluation::NotGated,
        on_missing: contract.on_missing,
        rating_order: contract.rating.order.clone(),
        quantum: contract.rating.quantum,
        tie_break: contract.rating.tie_break,
        generated_at_ms: stamp.generated_at_ms,
        status,
        cohorts,
        ineligible,
        failed,
        dropped,
    }
}

fn strip_volatile(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for k in VOLATILE {
                o.remove(k);
            }
            o.values_mut().for_each(strip_volatile);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_volatile),
        _ => {}
    }
}

fn opt(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:.decimals$}"))
}

fn signed(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:+.decimals$}"))
}

fn rejected_table(m: &mut String, title: &str, what: &str, rows: &[Rejected]) {
    let _ = writeln!(m, "\n## {title}\n");
    if rows.is_empty() {
        let _ = writeln!(m, "None.");
        return;
    }
    let _ = writeln!(m, "{what}\n");
    let _ = writeln!(m, "| Strategy | Reason | Detail | Run |\n|---|---|---|---|");
    for r in rows {
        let _ = writeln!(
            m,
            "| `{}` | `{}` | {} | {} |",
            r.strategy,
            r.reason,
            r.detail.replace('|', "\\|"),
            r.run.as_ref().map_or("—".to_string(), |r| format!("`{r}`"))
        );
    }
}

impl Ranking {
    /// Module table: what a rerun on the same data reproduces.
    pub fn content_sha256(&self) -> String {
        let mut v = serde_json::to_value(self).expect("a ranking serializes (string map keys)");
        strip_volatile(&mut v);
        canonical_sha256(&v)
    }

    /// A few lines for a terminal or a tool result: status, window, each
    /// cohort's rows weakest → strongest (`rank strategy ci95_lo mean n run`,
    /// ids whole), then the failed / ineligible / dropped strategies with
    /// their reasons.
    pub fn render_compact(&self) -> String {
        let mut m = String::new();
        let _ = writeln!(
            m,
            "strategy ranking `{}` {}: {} · contract sha256 {}",
            self.contract,
            self.date,
            enum_name(&self.status),
            self.contract_sha256
        );
        let _ = writeln!(
            m,
            "decisions {} → {} (cutoff, data through it) · arm {} · {}",
            self.from_ms.map_or("—".to_string(), fmt_time),
            fmt_time(self.cutoff_ms),
            self.arm,
            enum_name(&self.evaluation)
        );
        if self.cohorts.is_empty() {
            let _ = writeln!(m, "no run ranked");
        }
        let n = self.cohorts.len();
        for (i, c) in self.cohorts.iter().enumerate() {
            let _ = writeln!(m, "cohort {} of {n}, weakest → strongest:", i + 1);
            for r in &c.rows {
                let _ = writeln!(
                    m,
                    "  {}. {}  ci95_lo {}  mean {}  n {}  {}",
                    r.rank,
                    r.strategy,
                    signed(r.ci95_bps.map(|[lo, _]| lo), 2),
                    signed(r.mean_net_bps, 2),
                    r.n,
                    r.run
                );
            }
        }
        for (title, rows) in [
            ("failed", &self.failed),
            ("ineligible", &self.ineligible),
            ("dropped", &self.dropped),
        ] {
            if !rows.is_empty() {
                let list: Vec<String> = rows
                    .iter()
                    .map(|r| format!("{} {}", r.strategy, r.reason))
                    .collect();
                let _ = writeln!(m, "{title} ({}): {}", rows.len(), list.join(", "));
            }
        }
        m
    }

    /// `ranking.md` (module table).
    pub fn render_markdown(&self) -> String {
        let mut m = String::new();
        let local = |ms: i64| {
            Zone::parse(&self.tz).map_or_else(
                || fmt_time(ms),
                |z| format!("{} {}", z.to_local(ms).format("%Y-%m-%d %H:%M"), self.tz),
            )
        };
        let _ = writeln!(
            m,
            "# Strategy ranking `{}` — {}\n",
            self.contract, self.date
        );
        let _ = writeln!(m, "| Ranking | |\n|---|---|");
        let order: Vec<String> = self.rating_order.iter().map(|t| format!("`{t}`")).collect();
        for (k, v) in [
            (
                "Contract",
                format!("`{}` · sha256 `{}`", self.contract, self.contract_sha256),
            ),
            ("Status", enum_name(&self.status)),
            (
                "Sandbox · arm",
                format!(
                    "{} · `{}` ({} evidence, no split)",
                    self.sandbox,
                    self.arm,
                    enum_name(&self.evidence_class)
                ),
            ),
            (
                "Decisions",
                format!(
                    "{} → {} (cutoff {}; data through the cutoff)",
                    self.from_ms.map_or("—".to_string(), fmt_time),
                    fmt_time(self.cutoff_ms),
                    local(self.cutoff_ms)
                ),
            ),
            (
                "Evaluation",
                format!(
                    "{} — the rules arms only, no Jev gate",
                    enum_name(&self.evaluation)
                ),
            ),
            (
                "Rating",
                format!(
                    "{} · quantum {} · ties by {}",
                    order.join(", "),
                    self.quantum,
                    enum_name(&self.tie_break)
                ),
            ),
            (
                "Missing strategy",
                format!("{} (`on_missing`)", enum_name(&self.on_missing)),
            ),
            ("Generated", fmt_time(self.generated_at_ms)),
            ("Content sha256", format!("`{}`", self.content_sha256())),
        ] {
            let _ = writeln!(m, "| {k} | {v} |");
        }
        let _ = writeln!(
            m,
            "\nAscending: rank 1 is the weakest of its cohort. Runs compare only inside a \
             cohort (every field below equal)."
        );
        if self.cohorts.is_empty() {
            let _ = writeln!(m, "\nNo run was ranked.");
        }
        let n = self.cohorts.len();
        for (i, c) in self.cohorts.iter().enumerate() {
            let _ = writeln!(m, "\n## Cohort {} of {n}\n", i + 1);
            let _ = writeln!(m, "| Field | Value |\n|---|---|");
            for (f, v) in &c.key {
                let shown = match f {
                    CohortField::FromMs | CohortField::ToMs | CohortField::DataThroughMs => v
                        .parse::<i64>()
                        .map_or_else(|_| format!("`{v}`"), |t| format!("`{v}` ({})", fmt_time(t))),
                    _ => format!("`{v}`"),
                };
                let _ = writeln!(m, "| {} | {shown} |", enum_name(f));
            }
            let _ = writeln!(
                m,
                "\n| Rank | Strategy | Tier | Verdict | n | periods | mean net bps | 95 % CI | median | hit | Sharpe | t | max DD bps | best-2 share | mean ex best 5 | Rating | Run | Spec sha256 | Variants |"
            );
            let _ = writeln!(
                m,
                "|---:|---|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|"
            );
            for r in &c.rows {
                let ci = r.ci95_bps.map_or("—".to_string(), |[lo, hi]| {
                    format!("[{lo:+.1}, {hi:+.1}]")
                });
                let rating: Vec<String> = r.rating.iter().map(i64::to_string).collect();
                let variants = if r.variants.is_empty() {
                    "—".to_string()
                } else {
                    r.variants
                        .iter()
                        .map(|v| format!("`{v}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    m,
                    "| {} | `{}` ({}) | {} | {} | {} | {} | {} | {ci} | {} | {} | {} | {} | {} | {} | {} | [{}] | `{}` | `{}` | {variants} |",
                    r.rank,
                    r.strategy,
                    r.kind,
                    enum_name(&r.evidence_tier),
                    enum_name(&r.verdict),
                    r.n,
                    r.n_periods,
                    signed(r.mean_net_bps, 2),
                    signed(r.median_net_bps, 2),
                    opt(r.hit_rate, 2),
                    opt(r.sharpe, 2),
                    opt(r.t_stat, 2),
                    opt(r.max_drawdown_bps, 0),
                    opt(r.best2_periods_share, 2),
                    signed(r.mean_ex_best5_bps, 2),
                    rating.join(", "),
                    r.run,
                    r.spec_sha256,
                );
            }
        }
        rejected_table(
            &mut m,
            "Ineligible",
            "Evaluated, not ranked: the first reason that applied.",
            &self.ineligible,
        );
        rejected_table(
            &mut m,
            "Failed",
            "Listed strategies without a run (`on_missing` reads these).",
            &self.failed,
        );
        rejected_table(
            &mut m,
            "Dropped",
            "Runs that never competed: not in the contract, or superseded by a newer run.",
            &self.dropped,
        );
        m
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::lineage::ranking::tests::contract;
    use crate::domain::lineage::registry::tests::minimal;
    use crate::domain::lineage::variant::{Variant, VariantSpec};

    /// 2026-03-01T00:00:00Z — the test contract's `from`.
    const FROM: i64 = 1_772_323_200_000;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()
    }

    /// 2026-10-09 00:00 New York (EDT) = 04:00Z.
    fn cutoff() -> i64 {
        Zone::NewYork.at(date(), 0, 0)
    }

    fn hash(s: &str) -> String {
        crate::domain::canonical::sha256_hex(s)
    }

    /// A finished run of `strategy` on the contract's window, with its
    /// identity; `mean` / `ci_lo` set the economics.
    fn report(strategy: &str, run_id: &str, mean: f64, ci_lo: f64) -> BacktestReport {
        serde_json::from_value(json!({
            "run_id": run_id,
            "strategy": strategy,
            "kind": "weekend_window",
            "interval": "1h",
            "spec": {"name": strategy},
            "spec_sha256": hash(strategy),
            "from_ms": FROM,
            "to_ms": cutoff(),
            "data_through_ms": cutoff(),
            "instruments_sha256": "a".repeat(64),
            "costs_sha256": "b".repeat(64),
            "n_instruments": 4,
            "n_candidates": 40,
            "arms": {"research": {"n_candidates": 40, "summary": {
                "n": 40, "n_periods": 12, "mean_net_bps": mean, "median_net_bps": mean,
                "t_stat": 2.0, "hit_rate": 0.55, "net_usd": 1.0, "gross_usd": 2.0,
                "fees_usd": 0.5, "spread_usd": 0.5, "slippage_usd": 0.0, "funding_usd": 0.0,
                "funding_incomplete": 0, "max_drawdown_usd": 1.0, "max_drawdown_bps": 120.0,
                "sharpe": 1.1, "periods_per_year": 52.0, "ci95_lo_bps": ci_lo,
                "ci95_hi_bps": mean + 10.0, "bootstrap": 200, "best2_periods_share": 0.3,
                "mean_ex_best5_bps": mean - 2.0
            }}}
        }))
        .unwrap()
    }

    fn facts(strategy: &str, run_id: &str, mean: f64, ci_lo: f64) -> RunFacts {
        let r = report(strategy, run_id, mean, ci_lo);
        RunFacts::from_report(&r, "xlab", &hash(run_id), "research")
    }

    fn unregistered() -> StrategyStanding {
        StrategyStanding {
            variants: BTreeMap::new(),
            evidence_tier: EvidenceClass::None,
            verdict: RunVerdict::Unregistered,
        }
    }

    fn run(f: RunFacts) -> RunInput {
        RunInput::of(f, unregistered())
    }

    fn with(f: RunFacts, standing: StrategyStanding) -> RunInput {
        RunInput::of(f, standing)
    }

    fn contract_of(strategies: &[&str]) -> RankingContract {
        let mut c = contract();
        c.strategies = strategies.iter().map(|s| s.to_string()).collect();
        c
    }

    fn stamp(generated_at_ms: i64) -> RankingStamp {
        RankingStamp {
            contract_sha256: "c".repeat(64),
            date: date(),
            generated_at_ms,
        }
    }

    fn ranking(c: &RankingContract, runs: &[RunInput]) -> Ranking {
        rank(c, &stamp(1), select(c, cutoff(), runs))
    }

    /// `(strategy, rank)` of every cohort's rows, in order.
    fn order(r: &Ranking) -> Vec<Vec<(String, usize)>> {
        r.cohorts
            .iter()
            .map(|c| {
                c.rows
                    .iter()
                    .map(|x| (x.strategy.clone(), x.rank))
                    .collect()
            })
            .collect()
    }

    fn reasons(r: &Ranking) -> Vec<(String, String)> {
        r.ineligible
            .iter()
            .chain(&r.failed)
            .chain(&r.dropped)
            .map(|x| (x.strategy.clone(), x.reason.clone()))
            .collect()
    }

    fn row(s: &str, n: usize) -> (String, usize) {
        (s.to_string(), n)
    }

    /// A registry whose variant `var.<s>` carries `<s>`'s spec hash, with
    /// `status`; `minimal()`'s PASS backtest (HOLDOUT result) is `var`'s.
    fn registry(variants: &[(&str, VariantStatus)]) -> Registry {
        let mut reg = minimal();
        let base = reg.variants["var"].clone();
        for (s, status) in variants {
            let v = Variant {
                id: format!("var.{s}"),
                parent: "var".into(),
                status: *status,
                spec: VariantSpec {
                    sandbox: Some("ranked".into()),
                    strategy: Some(s.to_string()),
                    spec_sha256: Some(hash(s)),
                    ..VariantSpec::default()
                },
                ..base.clone()
            };
            reg.variants.insert(v.id.clone(), v);
        }
        reg
    }

    #[test]
    fn ascending_puts_the_weakest_first() {
        let c = contract_of(&["a", "b", "c"]);
        let r = ranking(
            &c,
            &[
                run(facts("a", "20261009T050000Z-a", 20.0, 5.0)),
                run(facts("b", "20261009T050000Z-b", 20.0, -3.0)),
                run(facts("c", "20261009T050000Z-c", 20.0, 12.0)),
            ],
        );
        assert_eq!(r.status, RankingStatus::Complete);
        assert_eq!(order(&r), vec![vec![row("b", 1), row("a", 2), row("c", 3)]]);
        let b = &r.cohorts[0].rows[0];
        // evidence_tier, verdict, ci95_lo_bps, mean_net_bps, -best2, -max DD.
        assert_eq!(b.rating, vec![0, 1, -300, 2000, -30, -12000]);
        assert_eq!(b.evaluation, Evaluation::NotGated);
        assert_eq!(b.run, "run:xlab/20261009T050000Z-b");
        assert_eq!(b.ci95_bps, Some([-3.0, 30.0]));
        assert!(r.ineligible.is_empty() && r.failed.is_empty() && r.dropped.is_empty());
    }

    #[test]
    fn equal_keys_break_by_strategy_id() {
        let c = contract_of(&["a", "b", "z"]);
        // Inside one quantum (0.01 bps): 5.001 and 5.004 both round to 500.
        let r = ranking(
            &c,
            &[
                run(facts("z", "20261009T050000Z-z", 20.0, 5.001)),
                run(facts("b", "20261009T050000Z-b", 20.0, 5.004)),
                run(facts("a", "20261009T050000Z-a", 20.0, 5.0)),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("a", 1), row("b", 2), row("z", 3)]]);
        let rows = &r.cohorts[0].rows;
        assert!(rows.iter().all(|x| x.rating == rows[0].rating));
    }

    #[test]
    fn ci_lower_bound_orders_before_mean() {
        let c = contract_of(&["hi_mean", "hi_ci"]);
        let r = ranking(
            &c,
            &[
                run(facts("hi_mean", "20261009T050000Z-hi_mean", 50.0, 2.0)),
                run(facts("hi_ci", "20261009T050000Z-hi_ci", 10.0, 8.0)),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("hi_mean", 1), row("hi_ci", 2)]]);
        // A negated key: the larger drawdown is the weaker.
        let mut c = contract_of(&["deep", "shallow"]);
        c.rating.order = vec!["-max_drawdown_bps".parse().unwrap()];
        let mut deep = facts("deep", "20261009T050000Z-deep", 10.0, 1.0);
        deep.summary.as_mut().unwrap().max_drawdown_bps = Some(400.0);
        let r = ranking(
            &c,
            &[
                run(facts("shallow", "20261009T050000Z-shallow", 10.0, 1.0)),
                run(deep),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("deep", 1), row("shallow", 2)]]);
        assert_eq!(r.cohorts[0].rows[0].rating, vec![-40000]);
    }

    #[test]
    fn evidence_tier_orders_before_economics() {
        // `proven`'s variant has minimal()'s PASS backtest (a HOLDOUT
        // result); its forward experiment is PENDING and lifts nothing.
        let mut reg = registry(&[("proven", VariantStatus::Surviving)]);
        reg.experiments.get_mut("bt").unwrap().variant = "var.proven".into();
        reg.experiments.get_mut("fw").unwrap().variant = "var.proven".into();
        let proven = StrategyStanding::of(&reg, &hash("proven"));
        assert_eq!(proven.evidence_tier, EvidenceClass::Holdout);
        assert_eq!(proven.verdict, RunVerdict::Surviving);
        assert_eq!(
            proven.variants,
            BTreeMap::from([("var.proven".to_string(), VariantStatus::Surviving)])
        );
        // A PASS forward lifts it; one invalid for strategy inference does not.
        let mut passed = reg.clone();
        passed.experiments.get_mut("fw").unwrap().verdict.value = VerdictValue::Pass;
        assert_eq!(
            StrategyStanding::of(&passed, &hash("proven")).evidence_tier,
            EvidenceClass::ForwardPaper
        );
        passed.experiments.get_mut("fw").unwrap().validity =
            Some(Validity::InvalidForStrategyInference);
        assert_eq!(
            StrategyStanding::of(&passed, &hash("proven")).evidence_tier,
            EvidenceClass::Holdout
        );
        let rich = StrategyStanding::of(&reg, &hash("rich"));
        assert_eq!(rich, unregistered());

        let c = contract_of(&["proven", "rich"]);
        let r = ranking(
            &c,
            &[
                with(
                    facts("proven", "20261009T050000Z-proven", 1.0, -40.0),
                    proven,
                ),
                with(facts("rich", "20261009T050000Z-rich", 90.0, 60.0), rich),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("rich", 1), row("proven", 2)]]);
        let top = &r.cohorts[0].rows[1];
        assert_eq!(
            (top.evidence_tier, top.verdict, top.variants.clone()),
            (
                EvidenceClass::Holdout,
                RunVerdict::Surviving,
                vec!["var.proven".to_string()]
            )
        );
        assert_eq!(&top.rating[..2], &[2, 3]);
    }

    #[test]
    fn a_rejected_variant_ranks_below_a_surviving_one() {
        let reg = registry(&[
            ("dead", VariantStatus::Rejected),
            ("alive", VariantStatus::Surviving),
            ("unsure", VariantStatus::Inconclusive),
        ]);
        let dead = StrategyStanding::of(&reg, &hash("dead"));
        assert_eq!(dead.verdict, RunVerdict::Rejected);
        let c = contract_of(&["dead", "alive", "unsure"]);
        let r = ranking(
            &c,
            &[
                with(facts("dead", "20261009T050000Z-dead", 80.0, 50.0), dead),
                with(
                    facts("alive", "20261009T050000Z-alive", 5.0, -10.0),
                    StrategyStanding::of(&reg, &hash("alive")),
                ),
                with(
                    facts("unsure", "20261009T050000Z-unsure", 40.0, 20.0),
                    StrategyStanding::of(&reg, &hash("unsure")),
                ),
            ],
        );
        assert_eq!(
            order(&r),
            vec![vec![row("dead", 1), row("unsure", 2), row("alive", 3)]]
        );
        // The weakest status of several variants is the run's verdict.
        let mut both = reg.clone();
        let mut twin = both.variants["var.alive"].clone();
        twin.id = "var.alive.twin".into();
        twin.status = VariantStatus::Rejected;
        both.variants.insert(twin.id.clone(), twin);
        assert_eq!(
            StrategyStanding::of(&both, &hash("alive")).verdict,
            RunVerdict::Rejected
        );
    }

    #[test]
    fn incompatible_cohorts_rank_apart() {
        let c = contract_of(&["a", "b", "c", "d"]);
        let mut b = facts("b", "20261009T050000Z-b", 30.0, 10.0);
        b.costs_sha256 = Some("e".repeat(64));
        let mut d = facts("d", "20261009T050000Z-d", 30.0, 10.0);
        d.generation = Some("W2".into());
        let r = ranking(
            &c,
            &[
                run(facts("a", "20261009T050000Z-a", 10.0, 1.0)),
                run(b),
                run(facts("c", "20261009T050000Z-c", 20.0, 2.0)),
                run(d),
            ],
        );
        assert_eq!(r.status, RankingStatus::Complete);
        // Keys order the cohorts: generation NONE < W2, then costs b… < e….
        assert_eq!(
            order(&r),
            vec![
                vec![row("a", 1), row("c", 2)],
                vec![row("b", 1)],
                vec![row("d", 1)]
            ]
        );
        assert_eq!(r.cohorts[0].key[&CohortField::Generation], UNBOUND);
        assert_eq!(r.cohorts[2].key[&CohortField::Generation], "W2");
        assert_eq!(r.cohorts[1].key[&CohortField::CostsSha256], "e".repeat(64));
        assert_eq!(
            r.cohorts[0].key[&CohortField::DataThroughMs],
            cutoff().to_string()
        );
        assert_eq!(r.cohorts[0].key.len(), c.cohort.len());
    }

    #[test]
    fn missing_ci_is_ineligible_never_zero() {
        let c = contract_of(&["no_ci", "weak"]);
        let mut no_ci = facts("no_ci", "20261009T050000Z-no_ci", 30.0, 0.0);
        no_ci.summary.as_mut().unwrap().ci95_lo_bps = None;
        let r = ranking(
            &c,
            &[
                run(no_ci),
                run(facts("weak", "20261009T050000Z-weak", -20.0, -50.0)),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("weak", 1)]]);
        assert_eq!(
            reasons(&r),
            vec![row_reason("no_ci", "metric_missing:ci95_lo_bps")]
        );
        assert_eq!(
            r.ineligible[0].run.as_deref(),
            Some("run:xlab/20261009T050000Z-no_ci")
        );
        // Ineligible is evaluated: the ranking is complete.
        assert_eq!(r.status, RankingStatus::Complete);
    }

    fn row_reason(s: &str, reason: &str) -> (String, String) {
        (s.to_string(), reason.to_string())
    }

    #[test]
    fn nan_or_infinite_input_is_ineligible() {
        let c = contract_of(&["nan", "inf", "ninf", "ok"]);
        let mut nan = facts("nan", "20261009T050000Z-nan", 1.0, 1.0);
        nan.summary.as_mut().unwrap().mean_net_bps = Some(f64::NAN);
        let mut inf = facts("inf", "20261009T050000Z-inf", 1.0, 1.0);
        inf.summary.as_mut().unwrap().ci95_lo_bps = Some(f64::INFINITY);
        let mut ninf = facts("ninf", "20261009T050000Z-ninf", 1.0, 1.0);
        ninf.summary.as_mut().unwrap().max_drawdown_bps = Some(f64::NEG_INFINITY);
        let r = ranking(
            &c,
            &[
                run(nan),
                run(inf),
                run(ninf),
                run(facts("ok", "20261009T050000Z-ok", 1.0, 1.0)),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("ok", 1)]]);
        assert_eq!(
            reasons(&r),
            vec![
                row_reason("inf", "non_finite:ci95_lo_bps"),
                row_reason("nan", "non_finite:mean_net_bps"),
                row_reason("ninf", "non_finite:max_drawdown_bps"),
            ]
        );
        // A NaN outside the rating does not matter.
        let mut c = contract_of(&["ok"]);
        c.rating.order = vec!["ci95_lo_bps".parse().unwrap()];
        let mut odd = facts("ok", "20261009T050000Z-ok", 1.0, 1.0);
        odd.summary.as_mut().unwrap().sharpe = Some(f64::NAN);
        assert_eq!(order(&ranking(&c, &[run(odd)])), vec![vec![row("ok", 1)]]);
    }

    #[test]
    fn zero_trades_is_ineligible() {
        let c = contract_of(&["empty", "thin", "short", "gappy", "retired", "ok"]);
        let mut empty = facts("empty", "20261009T050000Z-empty", 0.0, 0.0);
        let s = empty.summary.as_mut().unwrap();
        (s.n, s.n_periods, s.mean_net_bps, s.ci95_lo_bps) = (0, 0, None, None);
        let mut thin = facts("thin", "20261009T050000Z-thin", 1.0, 1.0);
        thin.summary.as_mut().unwrap().n = 19;
        let mut short = facts("short", "20261009T050000Z-short", 1.0, 1.0);
        short.summary.as_mut().unwrap().n_periods = 7;
        let mut gappy = facts("gappy", "20261009T050000Z-gappy", 1.0, 1.0);
        gappy.summary.as_mut().unwrap().funding_incomplete = 1;
        let reg = registry(&[("retired", VariantStatus::Superseded)]);
        let r = ranking(
            &c,
            &[
                run(empty),
                run(thin),
                run(short),
                run(gappy),
                with(
                    facts("retired", "20261009T050000Z-retired", 1.0, 1.0),
                    StrategyStanding::of(&reg, &hash("retired")),
                ),
                run(facts("ok", "20261009T050000Z-ok", 1.0, 1.0)),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("ok", 1)]]);
        assert_eq!(
            reasons(&r),
            vec![
                row_reason("empty", "insufficient_trades"),
                row_reason("gappy", "funding_incomplete"),
                row_reason("retired", "excluded_status"),
                row_reason("short", "insufficient_periods"),
                row_reason("thin", "insufficient_trades"),
            ]
        );
        assert_eq!(r.ineligible[0].detail, "n 0 < min_trades 20");
        assert_eq!(
            r.ineligible[2].detail,
            "variant `var.retired` is SUPERSEDED"
        );
    }

    #[test]
    fn a_report_without_identity_is_cohort_unknown() {
        let c = contract_of(&["old", "unbound", "uncut"]);
        let mut old = facts("old", "20261009T050000Z-old", 1.0, 1.0);
        (old.instruments_sha256, old.costs_sha256) = (None, None);
        let mut uncut = facts("uncut", "20261009T050000Z-uncut", 1.0, 1.0);
        uncut.data_through_ms = None;
        let r = ranking(
            &c,
            &[
                run(old),
                run(facts("unbound", "20261009T050000Z-unbound", 1.0, 1.0)),
                run(uncut),
            ],
        );
        // An identity without a generation is an unbound sandbox, ranked.
        assert_eq!(order(&r), vec![vec![row("unbound", 1)]]);
        assert_eq!(
            reasons(&r),
            vec![
                row_reason("old", "cohort_unknown:generation"),
                row_reason("uncut", "cohort_unknown:data_through_ms"),
            ]
        );
        // The window is the contract's even when the cohort leaves it out.
        let mut narrow = contract_of(&["late", "early", "cut"]);
        narrow.cohort = vec![CohortField::Interval];
        let mut late = facts("late", "20261009T050000Z-late", 1.0, 1.0);
        late.from_ms += 3_600_000;
        let mut early = facts("early", "20261009T050000Z-early", 1.0, 1.0);
        early.to_ms -= 3_600_000;
        let mut cut = facts("cut", "20261009T050000Z-cut", 1.0, 1.0);
        cut.data_through_ms = Some(cutoff() - 1);
        let r = ranking(&narrow, &[run(late), run(early), run(cut)]);
        assert_eq!(
            reasons(&r),
            vec![
                row_reason("cut", "cohort_mismatch:data_through_ms"),
                row_reason("early", "cohort_mismatch:to_ms"),
                row_reason("late", "cohort_mismatch:from_ms"),
            ]
        );
        assert_eq!(r.status, RankingStatus::Complete);
    }

    #[test]
    fn a_holdout_half_is_refused() {
        // The engine-matrix stored run: a time split, no identity.
        let r: BacktestReport = serde_json::from_str(include_str!(
            "../../../tests/fixtures/xlab/run_conf_rows/report.json"
        ))
        .unwrap();
        let f = RunFacts::from_report(&r, "xlab", &hash("x"), "capped");
        assert!(f.holdout && f.summary.is_some() && f.generation.is_none());
        assert_eq!(f.run(), "run:xlab/20261001T182112Z-conf_rows");
        let c = contract_of(&["conf_rows", "halves", "no_arm"]);
        // An arm's halves alone count too; a missing arm is its own reason.
        let mut halves = facts("halves", "20261009T050000Z-halves", 1.0, 1.0);
        halves.holdout = true;
        let mut no_arm = report("no_arm", "20261009T050000Z-no_arm", 1.0, 1.0);
        no_arm.arms.clear();
        let no_arm = RunFacts::from_report(&no_arm, "xlab", &hash("y"), "research");
        let ranked = ranking(&c, &[run(f), run(halves), run(no_arm)]);
        assert_eq!(
            reasons(&ranked),
            vec![
                row_reason("conf_rows", "holdout_present"),
                row_reason("halves", "holdout_present"),
                row_reason("no_arm", "arm_missing"),
            ]
        );
        assert!(ranked.cohorts.is_empty());
    }

    #[test]
    fn one_run_per_strategy_and_cohort() {
        let c = contract_of(&["a", "b"]);
        let mut other_costs = facts("a", "20261009T040000Z-a", 99.0, 90.0);
        other_costs.costs_sha256 = Some("e".repeat(64));
        // `b`'s newest run is ineligible: the older eligible one does not stand in.
        let mut b_new = facts("b", "20261009T060000Z-b", 5.0, 1.0);
        b_new.summary.as_mut().unwrap().n = 3;
        let r = ranking(
            &c,
            &[
                run(facts("a", "20261008T050000Z-a", 1.0, -9.0)),
                run(facts("a", "20261009T050000Z-a", 2.0, 1.0)),
                run(facts("a", "20261009T050000Z-a", 2.0, 1.0)),
                run(other_costs),
                run(facts("b", "20261009T050000Z-b", 50.0, 40.0)),
                run(b_new),
            ],
        );
        assert_eq!(order(&r), vec![vec![row("a", 1)], vec![row("a", 1)]]);
        assert_eq!(r.cohorts[0].rows[0].run, "run:xlab/20261009T050000Z-a");
        assert_eq!(r.cohorts[1].rows[0].run, "run:xlab/20261009T040000Z-a");
        let dropped: Vec<(&str, Option<&str>)> = r
            .dropped
            .iter()
            .map(|x| (x.reason.as_str(), x.run.as_deref()))
            .collect();
        assert_eq!(
            dropped,
            vec![
                ("superseded_run", Some("run:xlab/20261008T050000Z-a")),
                ("superseded_run", Some("run:xlab/20261009T050000Z-b")),
            ]
        );
        assert_eq!(reasons(&r)[0], row_reason("b", "insufficient_trades"));
    }

    #[test]
    fn failed_stale_or_missing_strategies_follow_on_missing() {
        let mut c = contract_of(&["a", "b", "c", "d"]);
        let inputs = [
            run(facts("a", "20261009T050000Z-a", 1.0, 1.0)),
            RunInput::Failed {
                strategy: "b".into(),
                stage: "backtest".into(),
                error: "no bars for hyperliquid:xyz:TSLA".into(),
            },
            RunInput::Stale {
                strategy: "c".into(),
                detail: "hyperliquid:BTC newest bar closes 2026-10-08T20:00:00Z".into(),
            },
            run(facts("zz", "20261009T050000Z-zz", 1.0, 1.0)),
        ];
        let r = ranking(&c, &inputs);
        assert_eq!(r.status, RankingStatus::Incomplete);
        assert_eq!(order(&r), vec![vec![row("a", 1)]]);
        let failed: Vec<(String, String)> = r
            .failed
            .iter()
            .map(|x| (x.strategy.clone(), x.reason.clone()))
            .collect();
        assert_eq!(
            failed,
            vec![
                row_reason("b", "failed:backtest"),
                row_reason("c", "stale"),
                row_reason("d", "missing"),
            ]
        );
        assert_eq!(r.dropped[0].reason, "not_in_contract");
        assert_eq!(
            r.dropped[0].run.as_deref(),
            Some("run:xlab/20261009T050000Z-zz")
        );
        // EXCLUDE: ranked without them, still listed.
        c.on_missing = MissingPolicy::Exclude;
        let r = ranking(&c, &inputs);
        assert_eq!(r.status, RankingStatus::Complete);
        assert_eq!(r.failed.len(), 3);
        // Nothing evaluated is never complete.
        let r = ranking(&c, &inputs[1..3]);
        assert_eq!(r.status, RankingStatus::Incomplete);
    }

    #[test]
    fn content_sha256_ignores_run_ids_and_generation_time() {
        let c = contract_of(&["a", "b", "x"]);
        let day = |prefix: &str, mean: f64| {
            vec![
                run(facts("a", &format!("{prefix}-a"), mean, 1.0)),
                run(facts("b", &format!("{prefix}-b"), 3.0, 2.0)),
                run(facts("x", &format!("{prefix}-x"), 1.0, 5.0)),
                RunInput::Failed {
                    strategy: "x".into(),
                    stage: "evaluate".into(),
                    error: "boom".into(),
                },
            ]
        };
        let first = rank(
            &c,
            &stamp(1),
            select(&c, cutoff(), &day("20261009T050000Z", 2.0)),
        );
        let rerun = rank(
            &c,
            &stamp(999_999),
            select(&c, cutoff(), &day("20261009T070000Z", 2.0)),
        );
        assert_ne!(first, rerun);
        assert_eq!(first.content_sha256(), rerun.content_sha256());
        assert_eq!(first.content_sha256().len(), 64);
        // The run ids stay in the JSON: retention keeps what latest.json names.
        let j = serde_json::to_value(&first).unwrap();
        assert_eq!(
            j["cohorts"][0]["rows"][0]["run"],
            "run:xlab/20261009T050000Z-a"
        );
        assert_eq!(j["schema"], SCHEMA);
        assert_eq!(j["date"], "2026-10-09");
        assert_eq!(j["cohorts"][0]["key"]["costs_sha256"], "b".repeat(64));
        assert_eq!(serde_json::from_value::<Ranking>(j).unwrap(), first);
        // A changed figure, or another contract digest, changes it.
        let moved = rank(
            &c,
            &stamp(1),
            select(&c, cutoff(), &day("20261009T050000Z", 2.5)),
        );
        assert_ne!(first.content_sha256(), moved.content_sha256());
        let mut other = first.clone();
        other.contract_sha256 = "d".repeat(64);
        assert_ne!(first.content_sha256(), other.content_sha256());
    }

    #[test]
    fn markdown_lists_each_cohort_weakest_first_with_ids_whole() {
        let c = contract_of(&["strong", "weak", "thin", "gone"]);
        let mut thin = facts("thin", "20261009T050000Z-thin", 1.0, 1.0);
        thin.summary.as_mut().unwrap().n = 2;
        let r = ranking(
            &c,
            &[
                run(facts("strong", "20261009T050000Z-strong", 40.0, 20.0)),
                run(facts("weak", "20261009T050000Z-weak", -5.0, -30.0)),
                run(thin),
            ],
        );
        let md = r.render_markdown();
        let at = |s: &str| md.find(s).unwrap_or_else(|| panic!("{s} not in\n{md}"));
        assert!(at("| 1 | `weak` (weekend_window)") < at("| 2 | `strong` (weekend_window)"));
        for whole in [
            "`run:xlab/20261009T050000Z-weak`".to_string(),
            format!("`{}`", hash("strong")),
            format!("sha256 `{}`", "c".repeat(64)),
            format!("`{}`", r.content_sha256()),
            format!("| instruments_sha256 | `{}` |", "a".repeat(64)),
        ] {
            at(&whole);
        }
        at("| Status | INCOMPLETE |");
        at("cutoff 2026-10-09 00:00 America/New_York");
        at("| Evaluation | NOT_GATED");
        at("| `thin` | `insufficient_trades` | n 2 < min_trades 20 | `run:xlab/20261009T050000Z-thin` |");
        at("| `gone` | `missing` |");
        assert!(at("## Ineligible") < at("## Failed") && at("## Failed") < at("## Dropped"));
        at("## Dropped\n\nNone.");
    }

    #[test]
    fn compact_lists_rows_weakest_first_and_the_reasons() {
        let c = contract_of(&["strong", "weak", "gone"]);
        let r = ranking(
            &c,
            &[
                run(facts("strong", "20261009T050000Z-strong", 40.0, 20.0)),
                run(facts("weak", "20261009T050000Z-weak", -5.0, -30.0)),
            ],
        );
        let text = r.render_compact();
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} not in\n{text}"));
        at(&format!(
            "strategy ranking `rank.t` 2026-10-09: INCOMPLETE · contract sha256 {}",
            "c".repeat(64)
        ));
        at("decisions 2026-03-01T00:00:00Z → 2026-10-09T04:00:00Z (cutoff, data through it)");
        assert!(
            at("  1. weak  ci95_lo -30.00  mean -5.00  n 40  run:xlab/20261009T050000Z-weak")
                < at("  2. strong  ci95_lo +20.00  mean +40.00  n 40")
        );
        at("failed (1): gone missing");
        assert!(!text.contains("ineligible") && !text.contains("dropped"));
    }
}
