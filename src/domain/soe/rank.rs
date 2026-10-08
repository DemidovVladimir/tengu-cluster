//! Ranking primitives (PRD § 7.2 "remaining candidates are ranked by …",
//! § 11 rank stability; `docs/soe-2026-10-08.md` § 5): each candidate's
//! eight rank-key values, the order the signed profile's `rank_order` gives
//! them, why one ranks above another, what moved between two rankings and
//! how sensitive a verdict is to one input — plus the pieces of a
//! `WeeklyPortfolio`. Pure and deterministic. The allocation (who gets a
//! test within the weekly hours and the tranche) is O3 `allocate`, the one
//! portfolio builder; here only weeks with nothing allocated are assembled:
//! the `HOLD` week (nothing passes) and, until O3, the unallocated week.
//!
//! | `RankKey` | Value ([`KeyValue`]) | Ranks first |
//! |---|---|---|
//! | `EVIDENCE_CONFIDENCE` | [`evidence_confidence`]: `HIGH` a supporting `FACT` and no knowable contradiction · `MEDIUM` ≥ 2 supporting events · else `LOW` | higher |
//! | `TIME_ADJUSTED_BASE` | base time-adjusted contribution (profile currency) | higher |
//! | `PAYBACK_BASE` | base cash payback (`NOT_REACHED` after every month count) | lower |
//! | `DAYS_TO_DECISIVE_EVIDENCE` | whole days from the decision to the end of `experiment.deadline`, rounded up; no experiment = unknown | lower |
//! | `REVERSIBILITY` · `DEFENSIBILITY` | `ordinal` tier | higher |
//! | `CAPABILITY_FIT` | `matching::fit` level (`PROVEN` > `CLAIMED` > `STALE` > `MISSING`) | higher |
//! | `CONCENTRATION_MAX` | the largest `risk.concentration` share (bps); none listed = unknown | lower |
//!
//! | Rule | Value |
//! |---|---|
//! | Ranked | `PASS` candidates only, by `profile.rank_order` (no built-in order), then id ascending |
//! | Unknown | ranks after every known value of its key, never as a value |
//! | Copies | supporting records count per `event_key` — the O2 as-of view gives copies one key (C6: syndication is detected there, not here) |
//! | [`explain_order`] | the first key in the profile's order on which two candidates differ (`None`: equal on all, the id decides) |
//! | [`rank_moves`] | per id whose rank changed: `ENTERED` / `LEFT` (with its verdict and gates), `CROSSED` (the candidate it swapped with nearest it, the key deciding them, both values before → after), `SHIFTED` (a candidate entered or left above it) |
//! | [`perturb`] / [`sensitivity`] | scale one input's low / base / high by `(10 000 + scale_bps) / 10 000` — rounded against the candidate, shares held to 0..=10 000 bps — and re-run economics + gates; `scale_bps` ≥ −10 000 |
//!
//! | [`PerturbInput`] | `RECURRING` | `ONE_OFF` | `ACQUISITION` | `REVENUE_SHARE` |
//! |---|---|---|---|---|
//! | `OWNER_HOURS` | `owner_hours_per_month` | + `owner_hours_total` | `owner_hours_per_month` | `owner_hours_per_month` |
//! | `CONVERSION` | `conversion` | `win_probability` | — | — |
//! | `CHURN` | `churn_per_month` | — | `churn_per_month` | — |
//! | `PRICE` | `price_per_month` | `contract_value` | — | `share` |
//!
//! | Week building block | Value |
//! |---|---|
//! | [`current_versions`] | per opportunity id, the highest version not dated after the decision (a later one is look-ahead, ignored); a repeated (id, version) is refused |
//! | [`ranked_row`] · [`gated_rows`] | a `RankedRow` with the profile's keys as text · held (`HOLD`) / rejected (`REJECT`) rows with every gate label, by id |
//! | [`week_next_information`] | the fields behind the held candidates' `HOLD` failures, by how many candidates each blocks, then by name |
//! | [`portfolio_inputs_sha256`] | canonical sha256 of every candidate's (id, version, `inputs_sha256`), by id |
//! | [`hold_week`] | the `HOLD` week: nothing ranked, every failed gate, the next information, a rationale; refused when a candidate passes (that week is the allocation's) |
//! | [`unallocated_week`] | `tengu soe portfolio` until O3: the `HOLD` week, or the `PASS` candidates ranked, each `HOLD`, nothing allocated, rationale [`NOT_ALLOCATED`] |

// Some consumers land with the O3 allocation.
#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;

use super::economics::{scenarios, Metric, Payback, Scenarios, ECONOMICS_VERSION};
use super::gates::{cited_views, gates, next_information, CitedKind, CitedRecord, GateVerdict};
use super::matching::{fit, CapabilityFit, FitLevel};
use super::opportunity::{Opportunity, RevenueModel};
use super::portfolio::{
    Allocation, GatedRow, IsoWeek, PortfolioAction, RankValue, RankedRow, WeeklyPortfolio,
};
use super::profile::{OperatorProfile, RankKey};
use super::record::{validate, Tier, Verdict};
use super::value::{
    codes, div_ceil, div_floor, Assumption, Better, Bps, Currency, Est, Minor, Money, SchemaTag,
    ValueError, BPS_FULL,
};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::{Time, TimeOrder};

const DAY_MS: i128 = 86_400_000;
const FULL: i128 = BPS_FULL as i128;

// ---------------------------------------------------------------------------
// Key values
// ---------------------------------------------------------------------------

/// One rank key's value for one candidate (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyValue {
    Tier(Tier),
    Money(Money),
    Payback(Payback),
    Days(u32),
    Share(Bps),
    Fit(FitLevel),
    /// What it lacks.
    Unknown(String),
}

impl KeyValue {
    /// The value on its own scale; `None` = unknown (ranks last).
    fn magnitude(&self) -> Option<i128> {
        match self {
            KeyValue::Tier(t) => t.level().map(i128::from),
            KeyValue::Money(m) => Some(i128::from(m.minor.0)),
            KeyValue::Payback(Payback::Months(n)) | KeyValue::Days(n) => Some(i128::from(*n)),
            KeyValue::Payback(Payback::NotReached) => Some(i128::from(u32::MAX) + 1),
            KeyValue::Share(b) => Some(i128::from(b.get())),
            KeyValue::Fit(f) => f.level().map(i128::from),
            KeyValue::Unknown(_) => None,
        }
    }

    pub fn is_known(&self) -> bool {
        self.magnitude().is_some()
    }
}

/// A closed value as serialized (`HIGH`, `PROVEN`).
fn name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

impl fmt::Display for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KeyValue::Tier(t) => f.write_str(&name(t)),
            KeyValue::Money(m) => write!(f, "{m}"),
            KeyValue::Payback(Payback::Months(1)) => f.write_str("1 month"),
            KeyValue::Payback(Payback::Months(n)) => write!(f, "{n} months"),
            KeyValue::Payback(Payback::NotReached) => f.write_str("NOT_REACHED"),
            KeyValue::Days(1) => f.write_str("1 day"),
            KeyValue::Days(d) => write!(f, "{d} days"),
            KeyValue::Share(b) => write!(f, "{b} bps"),
            KeyValue::Fit(l) => f.write_str(&name(l)),
            KeyValue::Unknown(why) => write!(f, "UNKNOWN: {why}"),
        }
    }
}

/// As its text (`"3000.00 EUR"`, `"HIGH"`, `"UNKNOWN: needs …"`).
impl Serialize for KeyValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// `a` against `b` on one key: `Less` = `a` ranks first; unknown last.
fn cmp_values(a: &KeyValue, b: &KeyValue, better: Better) -> Ordering {
    match (a.magnitude(), b.magnitude()) {
        (Some(x), Some(y)) => match better {
            Better::Higher => y.cmp(&x),
            Better::Lower => x.cmp(&y),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Module table: `EVIDENCE_CONFIDENCE` of `opp` at `as_of`, from the views
/// of the records it cites (`gates::cited_views`).
pub fn evidence_confidence(opp: &Opportunity, cited: &[CitedRecord], as_of: &Time) -> Tier {
    let knowable: Vec<&CitedRecord> = cited_views(opp, cited)
        .into_iter()
        .filter(|c| c.knowable(as_of))
        .collect();
    let contested = knowable.iter().any(|c| c.contradicted(as_of));
    let support: Vec<&&CitedRecord> = knowable.iter().filter(|c| c.supports(as_of)).collect();
    let fact = support.iter().any(|c| c.kind == CitedKind::Fact);
    let events: BTreeSet<&str> = support.iter().map(|c| c.event_key.as_str()).collect();
    if fact && !contested {
        Tier::High
    } else if events.len() >= 2 {
        Tier::Medium
    } else {
        Tier::Low
    }
}

/// Module table: `DAYS_TO_DECISIVE_EVIDENCE` of `opp` decided at `as_of`.
pub fn days_to_decisive_evidence(opp: &Opportunity, as_of: &Time) -> KeyValue {
    let Some(x) = &opp.experiment else {
        return KeyValue::Unknown("no experiment".into());
    };
    match (x.deadline.window_end(), as_of.earliest()) {
        (Some(end), Some(at)) => {
            let days = div_ceil((i128::from(end) - i128::from(at)).max(0), DAY_MS);
            KeyValue::Days(u32::try_from(days).unwrap_or(u32::MAX))
        }
        _ => KeyValue::Unknown("experiment.deadline or the decision time unknown".into()),
    }
}

fn metric_value<T>(m: &Metric<T>, known: impl FnOnce(&T) -> KeyValue) -> KeyValue {
    match m {
        Metric::Known(v) => known(v),
        Metric::Unknown { fields } => KeyValue::Unknown(format!("needs {}", fields.join(", "))),
    }
}

// ---------------------------------------------------------------------------
// One candidate
// ---------------------------------------------------------------------------

/// One candidate at one decision time: verdict, figures, fit and every rank
/// key's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assessment {
    pub id: String,
    pub version: u32,
    pub verdict: GateVerdict,
    pub scenarios: Scenarios,
    pub fit: CapabilityFit,
    /// Every `RankKey`'s value (all eight, whatever the profile's order).
    pub keys: BTreeMap<RankKey, KeyValue>,
    /// Canonical sha256 of the opportunity, the views of the records it
    /// cites and the decision time.
    pub inputs_sha256: String,
}

impl Assessment {
    pub fn value(&self, key: RankKey) -> &KeyValue {
        &self.keys[&key]
    }

    pub fn passes(&self) -> bool {
        self.verdict.verdict == Verdict::Pass
    }
}

fn as_json<T: Serialize>(what: &str, v: &T) -> Result<serde_json::Value, ValueError> {
    serde_json::to_value(v)
        .map_err(|e| ValueError::new(codes::INVALID_RECORD, format!("{what}: {e}")))
}

/// Gates, economics, fit and the rank keys of `opp` at `as_of` (`gates`'
/// refusals pass through: an unsigned profile, an unknown time, an
/// opportunity dated after the decision).
pub fn assess(
    opp: &Opportunity,
    cited: &[CitedRecord],
    profile: &OperatorProfile,
    as_of: Time,
) -> Result<Assessment, ValueError> {
    let verdict = gates(opp, cited, profile, as_of)?;
    let sc = scenarios(opp, profile)?;
    let fit = fit(opp, profile, &as_of);
    let money =
        |m: &Metric<Minor>| metric_value(m, |v| KeyValue::Money(Money::new(*v, sc.currency)));
    let keys = BTreeMap::from([
        (
            RankKey::EvidenceConfidence,
            KeyValue::Tier(evidence_confidence(opp, cited, &as_of)),
        ),
        (
            RankKey::TimeAdjustedBase,
            money(&sc.base.time_adjusted_contribution),
        ),
        (
            RankKey::PaybackBase,
            metric_value(&sc.base.payback, |p| KeyValue::Payback(*p)),
        ),
        (
            RankKey::DaysToDecisiveEvidence,
            days_to_decisive_evidence(opp, &as_of),
        ),
        (
            RankKey::Reversibility,
            KeyValue::Tier(opp.ordinal.reversibility),
        ),
        (RankKey::CapabilityFit, KeyValue::Fit(fit.level)),
        (
            RankKey::ConcentrationMax,
            opp.risk
                .concentration_max()
                .map(KeyValue::Share)
                .unwrap_or_else(|| KeyValue::Unknown("no concentration listed".into())),
        ),
        (
            RankKey::Defensibility,
            KeyValue::Tier(opp.ordinal.defensibility),
        ),
    ]);
    let inputs_sha256 = canonical_sha256(&json!({
        "as_of": as_of.to_string(),
        "opportunity": as_json("opportunity", opp)?,
        "cited": as_json("cited", &cited_views(opp, cited))?,
    }));
    Ok(Assessment {
        id: opp.id.clone(),
        version: opp.version,
        verdict,
        scenarios: sc,
        fit,
        keys,
        inputs_sha256,
    })
}

// ---------------------------------------------------------------------------
// Order and explanation
// ---------------------------------------------------------------------------

/// `a` against `b` under `order`: `Less` = `a` ranks first (module table);
/// equal on every key ⇒ id, then version, then `inputs_sha256`.
pub fn compare(a: &Assessment, b: &Assessment, order: &[RankKey]) -> Ordering {
    order
        .iter()
        .map(|k| cmp_values(a.value(*k), b.value(*k), k.better()))
        .find(|o| o.is_ne())
        .unwrap_or_else(|| {
            a.id.cmp(&b.id)
                .then(a.version.cmp(&b.version))
                .then_with(|| a.inputs_sha256.cmp(&b.inputs_sha256))
        })
}

/// The `PASS` candidates of `all`, in rank order.
pub fn rank<'a>(all: &'a [Assessment], order: &[RankKey]) -> Vec<&'a Assessment> {
    let mut out: Vec<&Assessment> = all.iter().filter(|a| a.passes()).collect();
    out.sort_by(|a, b| compare(a, b, order));
    out
}

/// Why `first` ranks above `second`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RankReason {
    /// The first key in the profile's order on which they differ; `None` =
    /// equal on every key (the values are then the ids).
    pub key: Option<RankKey>,
    pub first: String,
    pub first_value: String,
    pub second: String,
    pub second_value: String,
}

impl fmt::Display for RankReason {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "`{}` before `{}`: ", self.first, self.second)?;
        match self.key {
            Some(k) => write!(
                f,
                "{} {} vs {}",
                name(&k),
                self.first_value,
                self.second_value
            ),
            None => f.write_str("equal on every key, id ascending"),
        }
    }
}

/// The deciding key of `first` over `second`, if any.
fn deciding_key(first: &Assessment, second: &Assessment, order: &[RankKey]) -> Option<RankKey> {
    order
        .iter()
        .copied()
        .find(|k| cmp_values(first.value(*k), second.value(*k), k.better()).is_ne())
}

/// Module table: which of `a`, `b` ranks first under `order`, and why.
pub fn explain_order(a: &Assessment, b: &Assessment, order: &[RankKey]) -> RankReason {
    let (first, second) = if compare(a, b, order).is_le() {
        (a, b)
    } else {
        (b, a)
    };
    match deciding_key(first, second, order) {
        Some(k) => RankReason {
            key: Some(k),
            first: first.id.clone(),
            first_value: first.value(k).to_string(),
            second: second.id.clone(),
            second_value: second.value(k).to_string(),
        },
        None => RankReason {
            key: None,
            first: first.id.clone(),
            first_value: first.id.clone(),
            second: second.id.clone(),
            second_value: second.id.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Rank moves
// ---------------------------------------------------------------------------

/// A value before → after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub before: String,
    pub after: String,
}

impl Change {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

/// Why a candidate's rank changed (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MoveReason {
    /// Ranked now, not before: `before` is its verdict then (none: not
    /// assessed), `gates` what held it.
    Entered {
        before: Option<Verdict>,
        gates: Vec<String>,
    },
    /// Ranked before, not now.
    Left {
        after: Option<Verdict>,
        gates: Vec<String>,
    },
    /// It and `other` swapped; `key` decides between them now (before, when
    /// they tie now).
    Crossed {
        other: String,
        key: Option<RankKey>,
        value: Change,
        other_value: Change,
    },
    /// No swap: `other` entered (or left) above it.
    Shifted { other: String, entered: bool },
}

/// One candidate whose rank changed; ranks count from 1, none = not ranked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RankMove {
    pub id: String,
    pub from: Option<u32>,
    pub to: Option<u32>,
    pub reason: MoveReason,
}

/// `id → (rank, assessment)` of a ranking (the first per id).
fn positions<'a>(ranked: &[&'a Assessment]) -> BTreeMap<&'a str, (u32, &'a Assessment)> {
    let mut out = BTreeMap::new();
    for (i, a) in ranked.iter().enumerate() {
        out.entry(a.id.as_str()).or_insert((i as u32 + 1, *a));
    }
    out
}

fn by_id(all: &[Assessment]) -> BTreeMap<&str, &Assessment> {
    let mut out = BTreeMap::new();
    for a in all {
        out.entry(a.id.as_str()).or_insert(a);
    }
    out
}

/// Module table: every candidate (one assessment per id on each side)
/// whose rank differs between `before` and `after`, ranked ones first.
pub fn rank_moves(before: &[Assessment], after: &[Assessment], order: &[RankKey]) -> Vec<RankMove> {
    let (rb, ra) = (rank(before, order), rank(after, order));
    let (pb, pa) = (positions(&rb), positions(&ra));
    let (all_b, all_a) = (by_id(before), by_id(after));
    let ids: BTreeSet<&str> = pb.keys().chain(pa.keys()).copied().collect();
    let change = |b: &Assessment, a: &Assessment, k: Option<RankKey>| match k {
        Some(k) => Change {
            before: b.value(k).to_string(),
            after: a.value(k).to_string(),
        },
        None => Change {
            before: b.id.clone(),
            after: a.id.clone(),
        },
    };
    let mut moves = Vec::new();
    for id in ids {
        let (from, to) = (pb.get(id), pa.get(id));
        if from.map(|f| f.0) == to.map(|t| t.0) {
            continue;
        }
        let reason = match (from, to) {
            (None, _) => MoveReason::Entered {
                before: all_b.get(id).map(|a| a.verdict.verdict),
                gates: all_b
                    .get(id)
                    .map(|a| a.verdict.labels())
                    .unwrap_or_default(),
            },
            (_, None) => MoveReason::Left {
                after: all_a.get(id).map(|a| a.verdict.verdict),
                gates: all_a
                    .get(id)
                    .map(|a| a.verdict.labels())
                    .unwrap_or_default(),
            },
            (Some(&(f, xb)), Some(&(t, xa))) => {
                // The swapped partner nearest it now.
                let crossed = pa
                    .iter()
                    .filter_map(|(&y, &(ty, ya))| {
                        let &(fy, yb) = pb.get(y)?;
                        (y != id && (f < fy) != (t < ty)).then_some((fy, yb, ty, ya))
                    })
                    .min_by_key(|&(_, _, ty, _)| (ty.abs_diff(t), ty));
                match crossed {
                    Some((fy, yb, ty, ya)) => {
                        let (first_a, second_a) = if t < ty { (xa, ya) } else { (ya, xa) };
                        let (first_b, second_b) = if f < fy { (xb, yb) } else { (yb, xb) };
                        let key = deciding_key(first_a, second_a, order)
                            .or_else(|| deciding_key(first_b, second_b, order));
                        MoveReason::Crossed {
                            other: ya.id.clone(),
                            key,
                            value: change(xb, xa, key),
                            other_value: change(yb, ya, key),
                        }
                    }
                    None => {
                        let entered = pa
                            .iter()
                            .filter(|&(y, &(ty, _))| ty < t && !pb.contains_key(y))
                            .min_by_key(|&(_, &(ty, _))| ty)
                            .map(|(y, _)| (y.to_string(), true));
                        let left = || {
                            pb.iter()
                                .filter(|&(y, &(fy, _))| fy < f && !pa.contains_key(y))
                                .min_by_key(|&(_, &(fy, _))| fy)
                                .map(|(y, _)| (y.to_string(), false))
                        };
                        let (other, entered) = entered
                            .or_else(left)
                            .unwrap_or_else(|| (id.to_string(), false));
                        MoveReason::Shifted { other, entered }
                    }
                }
            }
        };
        moves.push(RankMove {
            id: id.to_string(),
            from: from.map(|f| f.0),
            to: to.map(|t| t.0),
            reason,
        });
    }
    moves.sort_by(|a, b| {
        (a.to.is_none(), a.to, a.from, &a.id).cmp(&(b.to.is_none(), b.to, b.from, &b.id))
    });
    moves
}

// ---------------------------------------------------------------------------
// Sensitivity
// ---------------------------------------------------------------------------

/// The inputs a sensitivity run scales (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PerturbInput {
    OwnerHours,
    Conversion,
    Churn,
    Price,
}

impl PerturbInput {
    pub const ALL: [PerturbInput; 4] = [
        PerturbInput::OwnerHours,
        PerturbInput::Conversion,
        PerturbInput::Churn,
        PerturbInput::Price,
    ];

    /// Which way the input helps the candidate (the rounding goes the other).
    fn better(self) -> Better {
        match self {
            PerturbInput::OwnerHours | PerturbInput::Churn => Better::Lower,
            PerturbInput::Conversion | PerturbInput::Price => Better::Higher,
        }
    }
}

/// Scale one input by `(10 000 + scale_bps) / 10 000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Perturbation {
    pub input: PerturbInput,
    pub scale_bps: i32,
}

/// Each input at −`scale_bps`, then +`scale_bps` (a tornado).
pub fn tornado(scale_bps: i32) -> Vec<Perturbation> {
    PerturbInput::ALL
        .iter()
        .flat_map(|&input| {
            [-scale_bps, scale_bps].map(|s| Perturbation {
                input,
                scale_bps: s,
            })
        })
        .collect()
}

/// `v` scaled, rounded against the candidate.
fn scale(v: i128, p: Perturbation) -> Result<i128, ValueError> {
    let num = v
        .checked_mul(FULL + i128::from(p.scale_bps))
        .ok_or_else(|| ValueError::new(codes::OVERFLOW, "sensitivity: a scaled input overflows"))?;
    Ok(match p.input.better() {
        Better::Higher => div_floor(num, FULL),
        Better::Lower => div_ceil(num, FULL),
    })
}

fn scale_est<T: Copy + PartialOrd + fmt::Display>(
    a: &mut Assumption<T>,
    f: impl Fn(T) -> Result<T, ValueError>,
) -> Result<(), ValueError> {
    if let Est::Range { low, base, high } = a.value {
        a.value = Est::range(f(low)?, f(base)?, f(high)?)?;
    }
    Ok(())
}

fn scale_amount(a: &mut Assumption<Minor>, p: Perturbation) -> Result<(), ValueError> {
    scale_est(a, |m| {
        i64::try_from(scale(i128::from(m.0), p)?)
            .map(Minor)
            .map_err(|_| ValueError::new(codes::OVERFLOW, "sensitivity: an amount overflows"))
    })
}

fn scale_count(a: &mut Assumption<u32>, p: Perturbation) -> Result<(), ValueError> {
    scale_est(a, |n| {
        u32::try_from(scale(i128::from(n), p)?)
            .map_err(|_| ValueError::new(codes::OVERFLOW, "sensitivity: a count overflows"))
    })
}

fn scale_share(a: &mut Assumption<Bps>, p: Perturbation) -> Result<(), ValueError> {
    scale_est(a, |b| {
        Bps::new(scale(i128::from(b.get()), p)?.clamp(0, FULL) as i64)
    })
}

/// Module table: `opp` with `p.input` scaled, and the fields scaled (none
/// when the input is not part of its revenue model).
pub fn perturb(
    opp: &Opportunity,
    p: Perturbation,
) -> Result<(Opportunity, Vec<&'static str>), ValueError> {
    if p.scale_bps < -i32::from(BPS_FULL) {
        return Err(ValueError::new(
            codes::INVALID_FIELD,
            format!(
                "scale_bps {}: at least -{BPS_FULL} (an input never goes below 0)",
                p.scale_bps
            ),
        ));
    }
    let mut o = opp.clone();
    let mut fields = Vec::new();
    let e = &mut o.economics;
    match (p.input, &mut e.revenue) {
        (PerturbInput::OwnerHours, revenue) => {
            scale_count(&mut e.owner_hours_per_month, p)?;
            fields.push("economics.owner_hours_per_month");
            if let RevenueModel::OneOff {
                owner_hours_total, ..
            } = revenue
            {
                scale_count(owner_hours_total, p)?;
                fields.push("economics.revenue.owner_hours_total");
            }
        }
        (PerturbInput::Conversion, RevenueModel::Recurring { conversion, .. }) => {
            scale_share(conversion, p)?;
            fields.push("economics.revenue.conversion");
        }
        (
            PerturbInput::Conversion,
            RevenueModel::OneOff {
                win_probability, ..
            },
        ) => {
            scale_share(win_probability, p)?;
            fields.push("economics.revenue.win_probability");
        }
        (
            PerturbInput::Churn,
            RevenueModel::Recurring {
                churn_per_month, ..
            }
            | RevenueModel::Acquisition {
                churn_per_month, ..
            },
        ) => {
            scale_share(churn_per_month, p)?;
            fields.push("economics.revenue.churn_per_month");
        }
        (
            PerturbInput::Price,
            RevenueModel::Recurring {
                price_per_month, ..
            },
        ) => {
            scale_amount(price_per_month, p)?;
            fields.push("economics.revenue.price_per_month");
        }
        (PerturbInput::Price, RevenueModel::OneOff { contract_value, .. }) => {
            scale_amount(contract_value, p)?;
            fields.push("economics.revenue.contract_value");
        }
        (PerturbInput::Price, RevenueModel::RevenueShare { share, .. }) => {
            scale_share(share, p)?;
            fields.push("economics.revenue.share");
        }
        _ => {}
    }
    Ok((o, fields))
}

/// One perturbation's effect on the base time-adjusted contribution and the
/// verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SensitivityRow {
    pub input: PerturbInput,
    pub scale_bps: i32,
    /// The fields scaled; empty: the input is not part of the model.
    pub fields: Vec<&'static str>,
    pub base_time_adjusted: Metric<Minor>,
    pub perturbed_time_adjusted: Metric<Minor>,
    pub verdict_before: Verdict,
    pub verdict_after: Verdict,
    /// Gate labels the perturbation adds · removes.
    pub gates_added: Vec<String>,
    pub gates_removed: Vec<String>,
}

/// Module table: one row per perturbation of `opp` at `as_of`.
pub fn sensitivity(
    opp: &Opportunity,
    cited: &[CitedRecord],
    profile: &OperatorProfile,
    as_of: Time,
    perturbations: &[Perturbation],
) -> Result<Vec<SensitivityRow>, ValueError> {
    let before = assess(opp, cited, profile, as_of)?;
    let labels_before: BTreeSet<String> = before.verdict.labels().into_iter().collect();
    perturbations
        .iter()
        .map(|&p| {
            let (o, fields) = perturb(opp, p)?;
            let after = assess(&o, cited, profile, as_of)?;
            let labels_after: BTreeSet<String> = after.verdict.labels().into_iter().collect();
            Ok(SensitivityRow {
                input: p.input,
                scale_bps: p.scale_bps,
                fields,
                base_time_adjusted: before.scenarios.base.time_adjusted_contribution.clone(),
                perturbed_time_adjusted: after.scenarios.base.time_adjusted_contribution.clone(),
                verdict_before: before.verdict.verdict,
                verdict_after: after.verdict.verdict,
                gates_added: labels_after.difference(&labels_before).cloned().collect(),
                gates_removed: labels_before.difference(&labels_after).cloned().collect(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// WeeklyPortfolio building blocks
// ---------------------------------------------------------------------------

/// Module table: one opportunity per id — its highest version not dated
/// after `as_of` — sorted by id.
pub fn current_versions<'a>(
    opps: &'a [Opportunity],
    as_of: &Time,
) -> Result<Vec<&'a Opportunity>, ValueError> {
    let mut seen = BTreeSet::new();
    let mut current: BTreeMap<&str, &Opportunity> = BTreeMap::new();
    for o in opps {
        if !seen.insert((o.id.as_str(), o.version)) {
            return Err(ValueError::new(
                codes::DUPLICATE,
                format!("opportunity `{}` version {} twice", o.id, o.version),
            ));
        }
        if o.as_of.order(as_of) == TimeOrder::After {
            continue;
        }
        if current
            .get(o.id.as_str())
            .map_or(true, |c| c.version < o.version)
        {
            current.insert(o.id.as_str(), o);
        }
    }
    Ok(current.into_values().collect())
}

/// A ranked row: its rank, the action the allocation gives it and the
/// profile's keys as text.
pub fn ranked_row(
    rank: u32,
    a: &Assessment,
    order: &[RankKey],
    action: PortfolioAction,
) -> RankedRow {
    RankedRow {
        rank,
        id: a.id.clone(),
        opportunity_version: a.version,
        action,
        keys: order
            .iter()
            .map(|&key| RankValue {
                key,
                value: a.value(key).to_string(),
            })
            .collect(),
    }
}

/// `(held, rejected)` rows: `HOLD` / `REJECT` actions, every gate label, by
/// id (the O3 allocation may give a held row `CHEAP_TEST` or `DILIGENCE`).
pub fn gated_rows(all: &[Assessment]) -> (Vec<GatedRow>, Vec<GatedRow>) {
    let mut sorted: Vec<&Assessment> = all.iter().filter(|a| !a.passes()).collect();
    sorted.sort_by(|a, b| (&a.id, a.version).cmp(&(&b.id, b.version)));
    let row = |a: &Assessment, action| GatedRow {
        id: a.id.clone(),
        opportunity_version: a.version,
        action,
        gates: a.verdict.labels(),
    };
    let held = sorted
        .iter()
        .filter(|a| a.verdict.verdict == Verdict::Hold)
        .map(|a| row(a, PortfolioAction::Hold))
        .collect();
    let rejected = sorted
        .iter()
        .filter(|a| a.verdict.verdict == Verdict::Reject)
        .map(|a| row(a, PortfolioAction::Reject))
        .collect();
    (held, rejected)
}

/// Module table: the next information worth buying this week.
pub fn week_next_information(all: &[Assessment]) -> Vec<String> {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for a in all.iter().filter(|a| a.verdict.verdict == Verdict::Hold) {
        for field in next_information(&a.verdict) {
            *count.entry(field).or_default() += 1;
        }
    }
    let mut out: Vec<(String, usize)> = count.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.into_iter().map(|(f, _)| f).collect()
}

/// Module table: the identity of the week's inputs.
pub fn portfolio_inputs_sha256(all: &[Assessment]) -> String {
    let mut rows: Vec<(&str, u32, &str)> = all
        .iter()
        .map(|a| (a.id.as_str(), a.version, a.inputs_sha256.as_str()))
        .collect();
    rows.sort();
    canonical_sha256(&json!(rows
        .iter()
        .map(|(id, v, h)| json!({ "id": id, "version": v, "inputs_sha256": h }))
        .collect::<Vec<_>>()))
}

/// Why the week holds; `None` when a candidate passes.
pub fn hold_rationale(all: &[Assessment]) -> Option<String> {
    if all.iter().any(Assessment::passes) {
        return None;
    }
    if all.is_empty() {
        return Some("no candidate this week".into());
    }
    let n = |v: Verdict| all.iter().filter(|a| a.verdict.verdict == v).count();
    Some(format!(
        "no candidate passed the hard gates: {} held, {} rejected",
        n(Verdict::Hold),
        n(Verdict::Reject)
    ))
}

/// What a week record names besides its candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekHead {
    pub id: String,
    pub week: IsoWeek,
    /// The decision time every assessment was made at.
    pub as_of: Time,
    /// The profile's currency.
    pub currency: Currency,
    /// The profile file's digest (`config/soe.rs` `Loaded::sha256`).
    pub profile_sha256: String,
}

/// Every assessment made at the week's decision time, in its currency.
fn same_decision(head: &WeekHead, all: &[Assessment]) -> Vec<ValueError> {
    all.iter()
        .filter(|a| a.verdict.as_of != head.as_of || a.scenarios.currency != head.currency)
        .map(|a| {
            ValueError::new(
                codes::INVALID_FIELD,
                format!(
                    "`{}` was assessed at {} in {}, the week at {} in {}",
                    a.id, a.verdict.as_of, a.scenarios.currency, head.as_of, head.currency
                ),
            )
        })
        .collect()
}

/// The week record around its rows (nothing allocated), validated.
fn week_record(
    head: WeekHead,
    all: &[Assessment],
    ranked: Vec<RankedRow>,
    hold_rationale: Option<String>,
) -> Result<WeeklyPortfolio, Vec<ValueError>> {
    let (held, rejected) = gated_rows(all);
    let week = WeeklyPortfolio {
        schema: SchemaTag::v1("weekly_portfolio").map_err(|e| vec![e])?,
        id: head.id,
        version: 1,
        week: head.week,
        as_of: head.as_of,
        currency: head.currency,
        profile_sha256: head.profile_sha256,
        inputs_sha256: portfolio_inputs_sha256(all),
        economics_version: ECONOMICS_VERSION,
        ranked,
        held,
        rejected,
        allocation: Allocation {
            owner_hours: 0,
            cash: Minor::ZERO,
        },
        hold_rationale,
        next_information: week_next_information(all),
    };
    validate(&week)?;
    Ok(week)
}

/// Module table: the `HOLD` week of `all`, validated. `Err` when a
/// candidate passes, an assessment was made at another time or in another
/// currency, or the record breaks a `WeeklyPortfolio` rule.
pub fn hold_week(head: WeekHead, all: &[Assessment]) -> Result<WeeklyPortfolio, Vec<ValueError>> {
    let mut errors: Vec<ValueError> = all
        .iter()
        .filter(|a| a.passes())
        .map(|a| {
            ValueError::new(
                codes::INVALID_FIELD,
                format!(
                    "`{}` passes the gates: a week with a passing candidate is the allocation's (O3), not a HOLD week",
                    a.id
                ),
            )
        })
        .collect();
    errors.extend(same_decision(&head, all));
    if !errors.is_empty() {
        return Err(errors);
    }
    let rationale = hold_rationale(all);
    week_record(head, all, Vec::new(), rationale)
}

/// `hold_rationale` of an [`unallocated_week`] that ranks a candidate.
pub const NOT_ALLOCATED: &str = "not allocated: every ranked candidate holds until the O3 allocation gives tests within weekly_owner_hours and max_validation_tranche";

/// Module table: the week `tengu soe portfolio` prints before O3 —
/// [`hold_week`] when nothing passes; else every `PASS` candidate ranked by
/// `order` with the action `HOLD` (nothing allocated), the held / rejected
/// rows, the next information and [`NOT_ALLOCATED`]. O3 `allocate` replaces
/// it; `Err` as [`hold_week`] (another decision time or currency, a broken
/// record).
pub fn unallocated_week(
    head: WeekHead,
    all: &[Assessment],
    order: &[RankKey],
) -> Result<WeeklyPortfolio, Vec<ValueError>> {
    if !all.iter().any(Assessment::passes) {
        return hold_week(head, all);
    }
    let errors = same_decision(&head, all);
    if !errors.is_empty() {
        return Err(errors);
    }
    let ranked = rank(all, order)
        .into_iter()
        .enumerate()
        .map(|(i, a)| ranked_row(i as u32 + 1, a, order, PortfolioAction::Hold))
        .collect();
    week_record(head, all, ranked, Some(NOT_ALLOCATED.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::canonical::canonical_json;
    use crate::domain::lineage::pins::toml_digest;
    use crate::domain::soe::economics::tests::{economics, with_economics};
    use crate::domain::soe::gates::tests::{
        at, cited, complete, passing, revenue_share, view, SIGNAL,
    };
    use crate::domain::soe::opportunity::tests::recurring;
    use crate::domain::soe::profile::tests::{synthetic, SYNTHETIC};

    /// A passing recurring automation: 10 leads × 20 % a month, churn 5 %,
    /// ramp 2 (12.065 customers in month 9), `price` a month, variable 9 %,
    /// fixed 40.00, 9 h, initial 1200.00.
    fn automation(id: &str, price: &str) -> Opportunity {
        let mut o = complete(with_economics(
            "AUTOMATE",
            &economics(
                &[
                    ("variable_cost", "900"),
                    ("fixed_costs_per_month", "\"40.00\""),
                    ("owner_hours_per_month", "9"),
                    ("ramp_months", "2"),
                ],
                "RECURRING",
                &[
                    ("leads_per_month", "10"),
                    ("conversion", "2000"),
                    ("churn_per_month", "500"),
                    ("price_per_month", price),
                    ("collection_loss", "0"),
                ],
                ["\"0.00\"", "\"900.00\"", "\"300.00\"", "\"0.00\""],
            ),
            "",
        ));
        o.id = id.into();
        o
    }

    fn assessed(o: &Opportunity) -> Assessment {
        assess(o, &cited(), &synthetic(), at()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn ids(ranked: &[&Assessment]) -> Vec<String> {
        ranked.iter().map(|a| a.id.clone()).collect()
    }

    fn head() -> WeekHead {
        WeekHead {
            id: "2026-W41".into(),
            week: "2026-W41".parse().unwrap(),
            as_of: at(),
            currency: Currency::Eur,
            profile_sha256: toml_digest(SYNTHETIC).unwrap(),
        }
    }

    #[test]
    fn key_values_of_a_passing_candidate() {
        let a = assessed(&automation("a", "\"600.00\""));
        assert!(a.passes(), "{:?}", a.verdict.failures);
        let text: Vec<(RankKey, String)> =
            a.keys.iter().map(|(k, v)| (*k, v.to_string())).collect();
        assert_eq!(
            text,
            [
                (RankKey::EvidenceConfidence, "HIGH".to_string()),
                (RankKey::TimeAdjustedBase, "5917.49 EUR".into()),
                (RankKey::PaybackBase, "4 months".into()),
                // Deadline 2026-11-15 (to its end) from 2026-10-05T12:00Z:
                // 41.5 days, rounded up.
                (RankKey::DaysToDecisiveEvidence, "42 days".into()),
                (RankKey::Reversibility, "HIGH".into()),
                (RankKey::CapabilityFit, "PROVEN".into()),
                (
                    RankKey::ConcentrationMax,
                    "UNKNOWN: no concentration listed".into()
                ),
                (RankKey::Defensibility, "MEDIUM".into()),
            ]
        );
        assert_eq!(a.inputs_sha256.len(), 64);
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["keys"]["TIME_ADJUSTED_BASE"], "5917.49 EUR");
        assert_eq!(j["fit"]["level"], "PROVEN");
    }

    #[test]
    fn syndicated_copies_count_as_one_confirmation() {
        let demand = |id: &str, event: &str| {
            view(
                &format!("synthetic:{id}:{}", "0".repeat(63) + "d"),
                event,
                CitedKind::Demand,
                "2026-10-02",
            )
        };
        let mut o = passing();
        let copies = vec![
            demand("tender-a", "synthetic:notice:tender-7"),
            demand("tender-a-copy", "synthetic:notice:tender-7"),
        ];
        o.signals = copies.iter().map(|c| c.record_id.clone()).collect();
        // Two copies of one notice: one event — low, and held by the gates.
        assert_eq!(evidence_confidence(&o, &copies, &at()), Tier::Low);
        let a = assess(&o, &copies, &synthetic(), at()).unwrap();
        assert_eq!(a.verdict.labels(), ["SINGLE_DEMAND_SIGNAL"]);
        // Two independent notices: medium, and it passes.
        let mut two = copies.clone();
        two[1].event_key = "synthetic:notice:tender-9".into();
        assert_eq!(evidence_confidence(&o, &two, &at()), Tier::Medium);
        assert!(assess(&o, &two, &synthetic(), at()).unwrap().passes());
        // A primary fact: high — unless a knowable record is contested.
        let mut with_fact = copies.clone();
        with_fact.extend(cited());
        o.signals.push(SIGNAL.into());
        assert_eq!(evidence_confidence(&o, &with_fact, &at()), Tier::High);
        with_fact[0].contradicted_at = Some("2026-10-03".parse().unwrap());
        // The rest still count: one notice + the fact = two events.
        assert_eq!(evidence_confidence(&o, &with_fact, &at()), Tier::Medium);
        // A record not yet knowable confirms nothing.
        let mut later = two.clone();
        later[1].knowable_at = "2026-10-05".parse().unwrap(); // later the same day
        assert_eq!(evidence_confidence(&o, &later, &at()), Tier::Low);
    }

    #[test]
    fn order_follows_profile_rank_order_then_id() {
        let a = automation("a", "\"600.00\"");
        let mut b = automation("b", "\"500.00\"");
        b.ordinal.defensibility = Tier::High;
        let mut c = b.clone();
        c.id = "c".into();
        let all: Vec<Assessment> = [&c, &a, &b].map(assessed).to_vec();
        // The synthetic order: evidence ties, time-adjusted decides; b and c
        // tie on every key, the id breaks it.
        let p = synthetic();
        assert_eq!(ids(&rank(&all, &p.rank_order)), ["a", "b", "c"]);
        let r = explain_order(&all[1], &all[2], &p.rank_order);
        assert_eq!(r.key, Some(RankKey::TimeAdjustedBase));
        assert_eq!(
            r.to_string(),
            "`a` before `b`: TIME_ADJUSTED_BASE 5917.49 EUR vs 4819.57 EUR"
        );
        let tie = explain_order(&all[0], &all[2], &p.rank_order);
        assert_eq!(
            (tie.key, tie.first.as_str(), tie.to_string()),
            (
                None,
                "b",
                "`b` before `c`: equal on every key, id ascending".into()
            )
        );
        // Another order, another ranking: defensibility first.
        let order = [RankKey::Defensibility, RankKey::TimeAdjustedBase];
        assert_eq!(ids(&rank(&all, &order)), ["b", "c", "a"]);
        assert_eq!(
            explain_order(&all[1], &all[2], &order).key,
            Some(RankKey::Defensibility)
        );
        // Lower is better for payback; an unknown value ranks last.
        let mut fast = automation("d", "\"500.00\"");
        fast.economics.initial.setup.value = Est::point("150.00".parse().unwrap());
        let mut none = automation("e", "\"500.00\"");
        none.ordinal.reversibility = Tier::Unknown;
        let more = [&b, &fast, &none].map(assessed);
        let payback = [RankKey::PaybackBase];
        assert_eq!(more[1].value(RankKey::PaybackBase).to_string(), "3 months");
        assert_eq!(ids(&rank(&more, &payback)), ["d", "b", "e"]);
        assert_eq!(
            ids(&rank(&more, &[RankKey::Reversibility])),
            ["b", "d", "e"]
        );
        // Only PASS candidates rank.
        let held = assessed(&complete(recurring()));
        assert!(!held.passes());
        assert_eq!(ids(&rank(&[held, more[0].clone()], &p.rank_order)), ["b"]);
    }

    #[test]
    fn price_cut_rank_move_explained_by_time_adjusted_key() {
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"500.00\""));
        let before = [assessed(&a), assessed(&b)];
        let (cut, fields) = perturb(
            &a,
            Perturbation {
                input: PerturbInput::Price,
                scale_bps: -2500,
            },
        )
        .unwrap();
        assert_eq!(fields, ["economics.revenue.price_per_month"]);
        let after = [assessed(&cut), assessed(&b)];
        let order = synthetic().rank_order;
        assert_eq!(ids(&rank(&after, &order)), ["b", "a"]);
        let moves = rank_moves(&before, &after, &order);
        // 450.00 a month: 5429.25 − ⌈488.6325⌉ − 40.00 − 630.00 = 4270.61.
        assert_eq!(
            moves,
            [
                RankMove {
                    id: "b".into(),
                    from: Some(2),
                    to: Some(1),
                    reason: MoveReason::Crossed {
                        other: "a".into(),
                        key: Some(RankKey::TimeAdjustedBase),
                        value: Change {
                            before: "4819.57 EUR".into(),
                            after: "4819.57 EUR".into()
                        },
                        other_value: Change {
                            before: "5917.49 EUR".into(),
                            after: "4270.61 EUR".into()
                        },
                    },
                },
                RankMove {
                    id: "a".into(),
                    from: Some(1),
                    to: Some(2),
                    reason: MoveReason::Crossed {
                        other: "b".into(),
                        key: Some(RankKey::TimeAdjustedBase),
                        value: Change {
                            before: "5917.49 EUR".into(),
                            after: "4270.61 EUR".into()
                        },
                        other_value: Change {
                            before: "4819.57 EUR".into(),
                            after: "4819.57 EUR".into()
                        },
                    },
                },
            ]
        );
        assert!(rank_moves(&before, &before, &order).is_empty());
        let j = serde_json::to_value(&moves[1]).unwrap();
        assert_eq!(j["reason"]["kind"], "CROSSED");
        assert_eq!(j["reason"]["key"], "TIME_ADJUSTED_BASE");
    }

    /// `a` (600.00) ranks above `b` (500.00); `p` applied to `a` drops it.
    fn a_drops_below_b(p: Perturbation, field: &str, after_value: &str) {
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"500.00\""));
        let (moved, fields) = perturb(&a, p).unwrap();
        assert_eq!(fields, [field]);
        let before = [assessed(&a), assessed(&b)];
        let after = [assessed(&moved), assessed(&b)];
        assert!(after[0].passes(), "{:?}", after[0].verdict.failures);
        let moves = rank_moves(&before, &after, &synthetic().rank_order);
        let mv = moves.iter().find(|m| m.id == "a").unwrap();
        assert_eq!((mv.from, mv.to), (Some(1), Some(2)));
        let MoveReason::Crossed {
            other, key, value, ..
        } = &mv.reason
        else {
            panic!("{mv:?}")
        };
        assert_eq!(
            (other.as_str(), *key),
            ("b", Some(RankKey::TimeAdjustedBase))
        );
        assert_eq!(
            (value.before.as_str(), value.after.as_str()),
            ("5917.49 EUR", after_value)
        );
    }

    #[test]
    fn a_new_candidate_enters_and_shifts_the_rest() {
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"500.00\""));
        let c = automation("c", "\"700.00\"");
        let before = [assessed(&a), assessed(&b)];
        let after = [assessed(&a), assessed(&b), assessed(&c)];
        let moves = rank_moves(&before, &after, &synthetic().rank_order);
        let got: Vec<(&str, Option<u32>, Option<u32>, &MoveReason)> = moves
            .iter()
            .map(|m| (m.id.as_str(), m.from, m.to, &m.reason))
            .collect();
        let shifted = MoveReason::Shifted {
            other: "c".into(),
            entered: true,
        };
        let entered = MoveReason::Entered {
            before: None,
            gates: vec![],
        };
        assert_eq!(
            got,
            [
                ("c", None, Some(1), &entered),
                ("a", Some(1), Some(2), &shifted),
                ("b", Some(2), Some(3), &shifted),
            ]
        );
    }

    #[test]
    fn churn_change_moves_rank_with_reason() {
        // Churn 5 % → 15 %: 9.058 customers in month 9 ⇒ 4275.66 time-adjusted.
        a_drops_below_b(
            Perturbation {
                input: PerturbInput::Churn,
                scale_bps: 20000,
            },
            "economics.revenue.churn_per_month",
            "4275.66 EUR",
        );
    }

    #[test]
    fn conversion_change_moves_rank_with_reason() {
        // Conversion 20 % → 15 %: 9.047 customers ⇒ 4269.66 time-adjusted.
        a_drops_below_b(
            Perturbation {
                input: PerturbInput::Conversion,
                scale_bps: -2500,
            },
            "economics.revenue.conversion",
            "4269.66 EUR",
        );
    }

    #[test]
    fn hours_increase_flips_gate_and_is_reported() {
        // Exactly at the target (3000.00 time-adjusted with 10 h). +25 %
        // hours: 12.5, rounded up to 13 ⇒ 3700.00 − 910.00 = 2790.00.
        let rows = sensitivity(
            &passing(),
            &cited(),
            &synthetic(),
            at(),
            &[
                Perturbation {
                    input: PerturbInput::OwnerHours,
                    scale_bps: 2500,
                },
                Perturbation {
                    input: PerturbInput::Churn,
                    scale_bps: 2500,
                },
            ],
        )
        .unwrap();
        let h = &rows[0];
        assert_eq!(h.fields, ["economics.owner_hours_per_month"]);
        assert_eq!(
            h.base_time_adjusted,
            Metric::Known("3000.00".parse().unwrap())
        );
        assert_eq!(
            h.perturbed_time_adjusted,
            Metric::Known("2790.00".parse().unwrap())
        );
        assert_eq!(
            (h.verdict_before, h.verdict_after),
            (Verdict::Pass, Verdict::Reject)
        );
        assert_eq!(h.gates_added, ["CONTRIBUTION_BELOW_TARGET"]);
        assert!(h.gates_removed.is_empty());
        // Churn is not part of a revenue share: nothing scaled, nothing moves.
        let c = &rows[1];
        assert!(c.fields.is_empty());
        assert_eq!(c.perturbed_time_adjusted, c.base_time_adjusted);
        assert_eq!(c.verdict_after, Verdict::Pass);
        // Ranking sees the gate flip as a LEFT move.
        let (more, _) = perturb(
            &passing(),
            Perturbation {
                input: PerturbInput::OwnerHours,
                scale_bps: 2500,
            },
        )
        .unwrap();
        let moves = rank_moves(
            &[assessed(&passing())],
            &[assessed(&more)],
            &synthetic().rank_order,
        );
        assert_eq!(
            moves[0].reason,
            MoveReason::Left {
                after: Some(Verdict::Reject),
                gates: vec!["CONTRIBUTION_BELOW_TARGET".into()]
            }
        );
        // The tornado: each input down, then up; below −100 % is refused.
        let t = tornado(2000);
        assert_eq!(t.len(), 2 * PerturbInput::ALL.len());
        assert_eq!((t[0].scale_bps, t[1].scale_bps), (-2000, 2000));
        let e = perturb(
            &passing(),
            Perturbation {
                input: PerturbInput::Price,
                scale_bps: -10001,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, codes::INVALID_FIELD);
        // A share is held at 100 %.
        let (up, _) = perturb(
            &revenue_share("\"9350.00\"", "4000", "10"),
            Perturbation {
                input: PerturbInput::Price,
                scale_bps: 25000,
            },
        )
        .unwrap();
        let RevenueModel::RevenueShare { share, .. } = &up.economics.revenue else {
            panic!()
        };
        assert_eq!(share.value, Est::point(Bps::FULL));
    }

    #[test]
    fn no_candidate_passes_gives_valid_hold_portfolio() {
        let mut h1 = complete(recurring()); // price UNKNOWN
        h1.id = "h1".into();
        let mut h2 = h1.clone();
        h2.id = "h2".into();
        let mut h3 = passing();
        h3.id = "h3".into();
        h3.economics.owner_hours_per_month.value = Est::unknown("no time log");
        let mut r = revenue_share("\"16000.00\"", "7500", "160"); // hidden labour
        r.id = "r".into();
        let all: Vec<Assessment> = [&r, &h3, &h1, &h2].map(assessed).to_vec();
        let w = hold_week(head(), &all).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(w.is_hold() && w.ranked.is_empty());
        assert_eq!(
            w.hold_rationale.as_deref(),
            Some("no candidate passed the hard gates: 3 held, 1 rejected")
        );
        let held: Vec<(&str, &str)> = w
            .held
            .iter()
            .map(|r| (r.id.as_str(), r.action.kind()))
            .collect();
        assert_eq!(held, [("h1", "HOLD"), ("h2", "HOLD"), ("h3", "HOLD")]);
        assert_eq!(
            w.held[2].gates,
            [
                "UNBOUNDED_OWNER_TIME",
                "UNKNOWN_INPUT:economics.owner_hours_per_month"
            ]
        );
        assert_eq!(
            (w.rejected[0].id.as_str(), w.rejected[0].action.kind()),
            ("r", "REJECT")
        );
        assert_eq!(w.rejected[0].gates, ["CONTRIBUTION_BELOW_TARGET"]);
        // By how many candidates each blocks (2, then 1), then by name.
        assert_eq!(
            w.next_information,
            [
                "economics.revenue.price_per_month",
                "economics.owner_hours_per_month"
            ]
        );
        assert_eq!(
            (w.allocation.owner_hours, w.allocation.cash),
            (0, Minor::ZERO)
        );
        assert_eq!(w.profile_sha256, toml_digest(SYNTHETIC).unwrap());
        // An empty week holds too; a passing candidate is not a HOLD week.
        let empty = hold_week(head(), &[]).unwrap();
        assert_eq!(
            empty.hold_rationale.as_deref(),
            Some("no candidate this week")
        );
        let mut with_pass = all.clone();
        with_pass.push(assessed(&passing()));
        let e = hold_week(head(), &with_pass).unwrap_err();
        assert!(e[0].message.contains("allocation"), "{e:?}");
        // An assessment made at another time is refused.
        let mut other = head();
        other.as_of = "2026-10-06T12:00:00Z".parse().unwrap();
        assert!(hold_week(other, &all).is_err());
    }

    #[test]
    fn unallocated_week_ranks_passing_candidates_as_hold() {
        let order = synthetic().rank_order;
        let mut h = complete(recurring()); // price UNKNOWN
        h.id = "h".into();
        let mut r = revenue_share("\"16000.00\"", "7500", "160"); // hidden labour
        r.id = "r".into();
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"500.00\""));
        let all: Vec<Assessment> = [&r, &b, &h, &a].map(assessed).to_vec();
        let w = unallocated_week(head(), &all, &order).unwrap_or_else(|e| panic!("{e:?}"));
        let ranked: Vec<(u32, &str, &str)> = w
            .ranked
            .iter()
            .map(|r| (r.rank, r.id.as_str(), r.action.kind()))
            .collect();
        assert_eq!(ranked, [(1, "a", "HOLD"), (2, "b", "HOLD")]);
        assert_eq!(w.ranked[0].keys.len(), order.len());
        assert_eq!(
            (w.held[0].id.as_str(), w.rejected[0].id.as_str()),
            ("h", "r")
        );
        assert_eq!(w.hold_rationale.as_deref(), Some(NOT_ALLOCATED));
        assert_eq!(
            (w.allocation.owner_hours, w.allocation.cash),
            (0, Minor::ZERO)
        );
        assert!(!w.is_hold());
        assert_eq!(w.inputs_sha256, portfolio_inputs_sha256(&all));
        // Nothing passes: it is the HOLD week.
        let gated = [all[0].clone(), all[2].clone()];
        assert_eq!(
            unallocated_week(head(), &gated, &order).unwrap(),
            hold_week(head(), &gated).unwrap()
        );
        // Another decision time is refused.
        let mut other = head();
        other.as_of = "2026-10-06T12:00:00Z".parse().unwrap();
        assert!(unallocated_week(other, &all, &order).is_err());
    }

    #[test]
    fn rerun_same_inputs_byte_identical_portfolio_json() {
        let opps = {
            let mut h = complete(recurring());
            h.id = "h".into();
            let mut r = revenue_share("\"16000.00\"", "7500", "160");
            r.id = "r".into();
            [h, r]
        };
        let json = |order: [usize; 2]| {
            let all: Vec<Assessment> = order.map(|i| assessed(&opps[i])).to_vec();
            canonical_json(&serde_json::to_value(hold_week(head(), &all).unwrap()).unwrap())
        };
        let first = json([0, 1]);
        assert_eq!(json([0, 1]), first);
        assert_eq!(json([1, 0]), first, "input order changes nothing");
        // The ranking is order-independent too.
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"600.00\""));
        let order = synthetic().rank_order;
        let one = [assessed(&b), assessed(&a)];
        let two = [assessed(&a), assessed(&b)];
        assert_eq!(ids(&rank(&one, &order)), ids(&rank(&two, &order)));
        // A ranked row lists the profile's keys in its order.
        let row = ranked_row(1, &two[0], &order, PortfolioAction::ContinueActive);
        assert_eq!(row.keys.len(), order.len());
        assert_eq!(
            (row.keys[1].key, row.keys[1].value.as_str()),
            (RankKey::TimeAdjustedBase, "5917.49 EUR")
        );
    }

    #[test]
    fn current_versions_one_per_id_never_later() {
        let at_t = at();
        let mut v1 = passing();
        v1.version = 1;
        v1.as_of = "2026-10-02".parse().unwrap();
        let mut v2 = passing();
        v2.version = 2;
        let mut v3 = passing();
        v3.version = 3;
        v3.as_of = "2026-10-12".parse().unwrap(); // after the decision
        let mut other = automation("b", "\"600.00\"");
        other.version = 1;
        let opps = [v3, v1, other, v2];
        let cur = current_versions(&opps, &at_t).unwrap();
        let got: Vec<(&str, u32)> = cur.iter().map(|o| (o.id.as_str(), o.version)).collect();
        assert_eq!(got, [("b", 1), ("example-automation", 2)]);
        let twice = [passing(), passing()];
        assert_eq!(
            current_versions(&twice, &at_t).unwrap_err().code,
            codes::DUPLICATE
        );
    }
}
