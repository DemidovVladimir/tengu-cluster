//! Ranking contract — `lineage/rankings/<id>.toml`
//! (`docs/strategy-ranking-automation-2026-10-08.md` SR-1): the preregistered
//! policy a strategy ranking follows — which strategies, which runs compare
//! (cohort), who is eligible, how rows are rated and ordered (ascending:
//! weakest first). Sealed with `tengu lineage seal ranking:<id>` before any
//! ranked outcome is seen; the publisher runs only a sealed, unchanged
//! contract. Unsealed = Warn `ranking_unsealed` (never an Error: a draft can
//! sit in the registry until the operator reviews it).
//!
//! | Field | Value |
//! |---|---|
//! | `preregistered`, `registered_at` | `true` (a contract is a preregistration) · a known time |
//! | `sandbox`, `strategies[]` | the sandbox whose `[backtest.strategies]` names them (`config/strategy_ranking.rs` checks at load) · distinct, `[a-z0-9_]{1,48}` |
//! | `evidence_class`, `arm` | `DEVELOPMENT` (no split: the holdout is never read) · `research` or `capped` (the rules arms; no Jev gate ⇒ evaluation `NOT_GATED`) |
//! | `tz`, `cutoff`, `days[]`, `from` | `America/New_York` · `Europe/Paris` · `UTC` (`domain/tz.rs`) · `"HH:MM"` local on the ranking date = each run's `to` and `data_through` · the weekdays it ranks on (`Mon` … `Sun`; none = every day) · decisions from this UTC day |
//! | `cohort[]` | the report fields two runs must share to rank together ([`CohortField`]) |
//! | `on_missing` | `INCOMPLETE`: a listed strategy failed or stale ⇒ the ranking is INCOMPLETE, `latest` kept · `EXCLUDE`: ranked without it, listed as failed |
//! | `[freshness] max_lag_bars` | every instrument's newest stored bar closes ≥ cutoff − n bars, else the strategy is STALE |
//! | `[eligibility] min_trades, min_periods, max_funding_incomplete, exclude_status[]` | below a minimum, above the funding gaps, or a variant in an excluded status ⇒ ineligible (listed, never ranked); minimums ≥ 1 |
//! | `[rating] order[], quantum, tie_break` | [`RatingTerm`]s compared lexicographically, ascending = weaker first, a leading `-` negates · numbers compared as `round(x / quantum)` (> 0) · `strategy_asc` |
//!
//! [`RankingContract::shape_errors`] holds the shape rules (`invalid_field`,
//! `validate.rs`); each message starts with the field it names.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::Weekday;
use serde::{Deserialize, Serialize};

use super::query::enum_name;
use super::value::{valid_id, EvidenceRef, Time};
use super::variant::VariantStatus;
use crate::domain::backtest::spec::valid_name;
use crate::domain::evidence::EvidenceClass;
use crate::domain::schedule::{parse_clock, parse_day};
use crate::domain::tz::Zone;

/// The arms a ranking may rank: the rules arms a backtest always runs (no
/// Jev gate — `domain/backtest/report.rs` `PRIMARY_ARM` / `CAPPED_ARM`).
pub const RANKED_ARMS: [&str; 2] = ["research", "capped"];

/// A report field two runs must share to rank in one cohort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CohortField {
    Generation,
    EvidenceClass,
    Arm,
    InstrumentsSha256,
    CostsSha256,
    Interval,
    FromMs,
    ToMs,
    DataThroughMs,
}

/// What a listed strategy that failed or went stale does to the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MissingPolicy {
    Incomplete,
    Exclude,
}

/// How equal ratings order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TieBreak {
    StrategyAsc,
}

/// A rating key: higher = stronger (a [`RatingTerm`] may negate it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RatingKey {
    /// The highest result class of PASS experiments of the run's variants.
    EvidenceTier,
    /// The weakest status of the run's variants.
    Verdict,
    Ci95LoBps,
    MeanNetBps,
    MedianNetBps,
    HitRate,
    Sharpe,
    TStat,
    Best2PeriodsShare,
    MaxDrawdownBps,
    MeanExBest5Bps,
}

impl RatingKey {
    pub const ALL: [RatingKey; 11] = [
        RatingKey::EvidenceTier,
        RatingKey::Verdict,
        RatingKey::Ci95LoBps,
        RatingKey::MeanNetBps,
        RatingKey::MedianNetBps,
        RatingKey::HitRate,
        RatingKey::Sharpe,
        RatingKey::TStat,
        RatingKey::Best2PeriodsShare,
        RatingKey::MaxDrawdownBps,
        RatingKey::MeanExBest5Bps,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RatingKey::EvidenceTier => "evidence_tier",
            RatingKey::Verdict => "verdict",
            RatingKey::Ci95LoBps => "ci95_lo_bps",
            RatingKey::MeanNetBps => "mean_net_bps",
            RatingKey::MedianNetBps => "median_net_bps",
            RatingKey::HitRate => "hit_rate",
            RatingKey::Sharpe => "sharpe",
            RatingKey::TStat => "t_stat",
            RatingKey::Best2PeriodsShare => "best2_periods_share",
            RatingKey::MaxDrawdownBps => "max_drawdown_bps",
            RatingKey::MeanExBest5Bps => "mean_ex_best5_bps",
        }
    }
}

/// One key of `[rating] order`: `"ci95_lo_bps"`, or `"-max_drawdown_bps"`
/// (negated: lower is stronger).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RatingTerm {
    pub key: RatingKey,
    pub negate: bool,
}

impl FromStr for RatingTerm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (negate, name) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let key = RatingKey::ALL
            .into_iter()
            .find(|k| k.name() == name)
            .ok_or_else(|| {
                format!(
                    "rating key `{s}`: one of {} (a leading `-` negates)",
                    RatingKey::ALL.map(RatingKey::name).join(", ")
                )
            })?;
        Ok(RatingTerm { key, negate })
    }
}

impl fmt::Display for RatingTerm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.negate {
            f.write_str("-")?;
        }
        f.write_str(self.key.name())
    }
}

impl TryFrom<String> for RatingTerm {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<RatingTerm> for String {
    fn from(t: RatingTerm) -> String {
        t.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Freshness {
    pub max_lag_bars: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub min_trades: u64,
    pub min_periods: u64,
    pub max_funding_incomplete: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_status: Vec<VariantStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rating {
    pub order: Vec<RatingTerm>,
    pub quantum: f64,
    pub tie_break: TieBreak,
}

/// `lineage/rankings/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingContract {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub preregistered: bool,
    pub registered_at: Time,
    pub sandbox: String,
    pub evidence_class: EvidenceClass,
    pub arm: String,
    pub tz: String,
    pub cutoff: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
    pub from: Time,
    pub strategies: Vec<String>,
    pub cohort: Vec<CohortField>,
    pub on_missing: MissingPolicy,
    pub freshness: Freshness,
    pub eligibility: Eligibility,
    pub rating: Rating,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}

/// The first value of `items` seen twice.
fn first_dup<T: Ord + Clone>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut seen = BTreeSet::new();
    items.into_iter().find(|x| !seen.insert(x.clone()))
}

impl RankingContract {
    /// `tz` as a zone, `None` when unsupported.
    pub fn zone(&self) -> Option<Zone> {
        Zone::parse(&self.tz)
    }

    /// `cutoff` as minutes after local midnight, `None` when not `HH:MM`.
    pub fn cutoff_minute(&self) -> Option<u32> {
        parse_clock(&self.cutoff, false)
    }

    /// `days` parsed (empty = every day); `Err` names the first bad one.
    pub fn weekdays(&self) -> Result<Vec<Weekday>, String> {
        self.days
            .iter()
            .map(|d| parse_day(d).ok_or_else(|| d.clone()))
            .collect()
    }

    /// `from` as ms (its UTC 00:00), `None` unless a UTC day.
    pub fn start_ms(&self) -> Option<i64> {
        match self.from {
            Time::Day(ms) => Some(ms),
            _ => None,
        }
    }

    /// The shape rules (module table), one message each, starting with the
    /// field it names.
    pub fn shape_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.preregistered {
            out.push(
                "preregistered: must be true — a ranking contract is a preregistration (the \
                 publisher runs only a sealed one)"
                    .to_string(),
            );
        }
        if !self.registered_at.is_known() {
            out.push("registered_at: UNKNOWN — a preregistration says when it was made".into());
        }
        if !valid_id(&self.sandbox) {
            out.push(format!("sandbox: `{}` is not a sandbox name", self.sandbox));
        }
        if self.evidence_class != EvidenceClass::Development {
            out.push(format!(
                "evidence_class: {} — a ranking reads DEVELOPMENT evidence only (no split, the \
                 holdout never read)",
                enum_name(&self.evidence_class)
            ));
        }
        if !RANKED_ARMS.contains(&self.arm.as_str()) {
            out.push(format!(
                "arm: `{}` — {} (the rules arms; no Jev gate)",
                self.arm,
                RANKED_ARMS.join(" or ")
            ));
        }
        if self.zone().is_none() {
            out.push(format!(
                "tz: `{}` — America/New_York, Europe/Paris or UTC",
                self.tz
            ));
        }
        if self.cutoff_minute().is_none() {
            out.push(format!("cutoff: `{}` — HH:MM (00:00–23:59)", self.cutoff));
        }
        match self.weekdays() {
            Err(bad) => out.push(format!("days: `{bad}` — Mon … Sun")),
            Ok(days) => {
                let mut seen = BTreeSet::new();
                if let Some(i) =
                    (0..days.len()).find(|&i| !seen.insert(days[i].num_days_from_monday()))
                {
                    out.push(format!("days: `{}` listed twice", self.days[i]));
                }
            }
        }
        if self.start_ms().is_none() {
            out.push(format!("from: `{}` — a UTC day (YYYY-MM-DD)", self.from));
        }
        if self.strategies.is_empty() {
            out.push("strategies: empty — list the strategies to rank".into());
        }
        for s in self.strategies.iter().filter(|s| !valid_name(s)) {
            out.push(format!(
                "strategies: `{s}` is not a strategy name ([a-z0-9_]{{1,48}})"
            ));
        }
        if let Some(s) = first_dup(self.strategies.iter()) {
            out.push(format!("strategies: `{s}` listed twice"));
        }
        if self.cohort.is_empty() {
            out.push("cohort: empty — name the fields runs must share".into());
        }
        if let Some(c) = first_dup(self.cohort.iter().copied()) {
            out.push(format!("cohort: `{}` listed twice", enum_name(&c)));
        }
        let e = &self.eligibility;
        if e.min_trades == 0 {
            out.push("eligibility.min_trades: 0 — at least 1".into());
        }
        if e.min_periods == 0 {
            out.push("eligibility.min_periods: 0 — at least 1".into());
        }
        let r = &self.rating;
        if r.order.is_empty() {
            out.push("rating.order: empty — list the rating keys".into());
        }
        if let Some(k) = first_dup(r.order.iter().map(|t| t.key)) {
            out.push(format!("rating.order: `{}` listed twice", k.name()));
        }
        if !(r.quantum.is_finite() && r.quantum > 0.0) {
            out.push(format!("rating.quantum: {} — a number > 0", r.quantum));
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid contract (the fixture's shape).
    pub(crate) const CONTRACT: &str = r#"
id = "rank.t"
title = "a test ranking"
preregistered = true
registered_at = "2026-10-08T12:00:00Z"
sandbox = "ranked"
evidence_class = "DEVELOPMENT"
arm = "research"
tz = "America/New_York"
cutoff = "00:00"
from = "2026-03-01"
strategies = ["rule_w", "rule_w_top4"]
cohort = ["generation", "evidence_class", "arm", "instruments_sha256", "costs_sha256", "interval", "from_ms", "to_ms", "data_through_ms"]
on_missing = "INCOMPLETE"

[freshness]
max_lag_bars = 2

[eligibility]
min_trades = 20
min_periods = 8
max_funding_incomplete = 0
exclude_status = ["SUPERSEDED"]

[rating]
order = ["evidence_tier", "verdict", "ci95_lo_bps", "mean_net_bps", "-best2_periods_share", "-max_drawdown_bps"]
quantum = 0.01
tie_break = "strategy_asc"
"#;

    pub(crate) fn contract() -> RankingContract {
        toml::from_str(CONTRACT).unwrap()
    }

    #[test]
    fn a_contract_parses_and_prints_its_rating_keys() {
        let c = contract();
        assert_eq!(c.shape_errors(), Vec::<String>::new());
        assert_eq!(c.zone(), Some(Zone::NewYork));
        assert_eq!(c.cutoff_minute(), Some(0));
        assert_eq!(c.start_ms(), Some(1_772_323_200_000));
        assert_eq!(c.weekdays(), Ok(vec![]));
        let order: Vec<String> = c.rating.order.iter().map(|t| t.to_string()).collect();
        assert_eq!(order[4], "-best2_periods_share");
        assert!(c.rating.order[4].negate);
        assert_eq!(c.rating.order[2].key, RatingKey::Ci95LoBps);
        // A key outside the list, an unknown field: parse errors.
        let bad = CONTRACT.replace("\"verdict\",", "\"pnl\",");
        let e = toml::from_str::<RankingContract>(&bad)
            .unwrap_err()
            .to_string();
        assert!(e.contains("rating key `pnl`"), "{e}");
        let extra = format!("{CONTRACT}\n[extra]\nx = 1\n");
        assert!(toml::from_str::<RankingContract>(&extra).is_err());
        // Round trip through JSON keeps the negation.
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["rating"]["order"][5], "-max_drawdown_bps");
        assert_eq!(serde_json::from_value::<RankingContract>(j).unwrap(), c);
    }
}
