//! PRD § 7.2 hard gates — run before any ranking (`docs/soe-2026-10-08.md`
//! § 5). Pure: an opportunity, the source records it cites (as the O2 as-of
//! view classes them), the signed profile and the decision time in; a
//! verdict with every failed gate out. Amounts are the profile currency's
//! (`economics::scenarios`); a boundary is the profile's value, and a figure
//! exactly at it passes.
//!
//! | Verdict | When |
//! |---|---|
//! | `REJECT` | any known violation |
//! | `HOLD` | else anything unknown, unverified or open |
//! | `PASS` | nothing failed — still no permission to act (PRD § 7.2) |
//!
//! | Code | Fails when | Outcome |
//! |---|---|---|
//! | `MECHANISM_HOLD` · `MECHANISM_REJECT` | the mechanism is `HOLD` · `REJECT` | HOLD · REJECT |
//! | `CASH_EXPOSURE_ABOVE_CAP` | the largest of initial capital (downside), Σ stage cash and the `max_loss.cash` high end > `max_cash_exposure` | REJECT |
//! | `TRANCHE_ABOVE_MAX` | a stage's cash > `max_validation_tranche` | REJECT |
//! | `CONTRIBUTION_BELOW_TARGET` · `BELOW_TARGET_WITH_PATH` | base contribution (`contribution_basis`) < `min_monthly_contribution`: when the upside reaches it and a staged experiment exists ⇒ the path HOLD, else REJECT | REJECT · HOLD |
//! | `PAYBACK_ABOVE_MAX` | base cash payback > `max_payback_months` or `NOT_REACHED` | REJECT |
//! | `DELIVERY_TOO_LONG` | ONE_OFF base `delivery_weeks` > `max_one_off_delivery_weeks` | HOLD |
//! | `UNBOUNDED_OWNER_TIME` · `UNBOUNDED_SUPPORT` · `UNBOUNDED_PAYMENT_ACCESS` · `UNBOUNDED_SCOPE` | its `risk.bounds` value `UNBOUNDED` ⇒ REJECT; `UNKNOWN` — owner time also with unknown owner hours ⇒ HOLD | REJECT · HOLD |
//! | `NO_TESTABLE_EVIDENCE` | no cited record supports it at `as_of` (`signals`), or no revenue input names evidence (`economics.revenue`) | HOLD |
//! | `SINGLE_DEMAND_SIGNAL` | the only support is one customer-demand event (PRD § 5.1: one post is not demand) | HOLD |
//! | `CONTRADICTED_EVIDENCE` | a cited record knowable at `as_of` is contradicted (ids in full) | HOLD |
//! | `LEGAL_UNRESOLVED` | a `LEGAL` `SANCTIONS` `LICENSING` `DATA` `TRANSFERABILITY` risk not `RESOLVED`; deal `ownership` / `legal` / `transition` not `VERIFIED` | HOLD |
//! | `DILIGENCE_OPEN` | another deal block not `VERIFIED` | HOLD |
//! | `DEAL_RED_FLAG` | a deal block `RED_FLAG`, or a listed `red_flags` entry | REJECT |
//! | `JURISDICTION_NOT_ALLOWED` | a `[jurisdictions]` code outside `jurisdictions_allow` | HOLD |
//! | `NO_STAGED_STOP_RULE` | the first stage with cash has no stop rule ⇒ REJECT; no experiment ⇒ HOLD | REJECT · HOLD |
//! | `DOWNSIDE_UNSTATED` | a `max_loss` term is unknown | HOLD |
//! | `UNKNOWN_INPUT:<field>` | a figure or field a gate reads is unknown (a jurisdiction `UNKNOWN`, `economics.fx`, …); under a `POST_TAX` profile a passing contribution or payback needs `economics.tax_review` (figures are pre-tax) | HOLD |
//!
//! | Evidence rule | Value |
//! |---|---|
//! | Cited | only records `Opportunity.signals` names (ids in full); a cited id with no [`CitedRecord`] supports nothing |
//! | Knowable | `knowable_at` surely not after `as_of` (a day against an instant inside it is not) |
//! | Fresh | no `valid_until`, or `as_of` surely before it — an expired listing supports nothing |
//! | Support | knowable, fresh, uncontradicted and not `TRIGGER` |
//! | Contradiction | counts unless surely after `as_of` (an unknown time counts) |
//!
//! [`gates`] refuses (`Err`) an unsigned profile, an unknown decision time and
//! an opportunity dated after it (`future_leakage`).

// Consumers land with the ranking, the eval set and `tengu soe` (O1 W5–W8).
#![allow(dead_code)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::economics::{fields, scenarios, to_currency, Metric, Payback, Scenarios};
use super::opportunity::{DiligenceStatus, Mechanism, Opportunity, RevenueModel};
use super::profile::{ContributionBasis, OperatorProfile, ProfitBasis};
use super::record::{stated, Problems, Verdict};
use super::risk::{Boundedness, RiskStatus};
use super::value::{codes, Better, Flow, Minor, Money, Side, ValueError};
use crate::domain::lineage::value::{Time, TimeOrder};

/// A gate (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateCode {
    MechanismHold,
    MechanismReject,
    CashExposureAboveCap,
    TrancheAboveMax,
    ContributionBelowTarget,
    BelowTargetWithPath,
    PaybackAboveMax,
    DeliveryTooLong,
    UnboundedOwnerTime,
    UnboundedSupport,
    UnboundedPaymentAccess,
    UnboundedScope,
    NoTestableEvidence,
    SingleDemandSignal,
    ContradictedEvidence,
    LegalUnresolved,
    DiligenceOpen,
    DealRedFlag,
    JurisdictionNotAllowed,
    NoStagedStopRule,
    DownsideUnstated,
    UnknownInput,
}

impl GateCode {
    pub const ALL: [GateCode; 22] = [
        GateCode::MechanismHold,
        GateCode::MechanismReject,
        GateCode::CashExposureAboveCap,
        GateCode::TrancheAboveMax,
        GateCode::ContributionBelowTarget,
        GateCode::BelowTargetWithPath,
        GateCode::PaybackAboveMax,
        GateCode::DeliveryTooLong,
        GateCode::UnboundedOwnerTime,
        GateCode::UnboundedSupport,
        GateCode::UnboundedPaymentAccess,
        GateCode::UnboundedScope,
        GateCode::NoTestableEvidence,
        GateCode::SingleDemandSignal,
        GateCode::ContradictedEvidence,
        GateCode::LegalUnresolved,
        GateCode::DiligenceOpen,
        GateCode::DealRedFlag,
        GateCode::JurisdictionNotAllowed,
        GateCode::NoStagedStopRule,
        GateCode::DownsideUnstated,
        GateCode::UnknownInput,
    ];

    /// As serialized (`CASH_EXPOSURE_ABOVE_CAP`).
    pub fn as_str(self) -> &'static str {
        match self {
            GateCode::MechanismHold => "MECHANISM_HOLD",
            GateCode::MechanismReject => "MECHANISM_REJECT",
            GateCode::CashExposureAboveCap => "CASH_EXPOSURE_ABOVE_CAP",
            GateCode::TrancheAboveMax => "TRANCHE_ABOVE_MAX",
            GateCode::ContributionBelowTarget => "CONTRIBUTION_BELOW_TARGET",
            GateCode::BelowTargetWithPath => "BELOW_TARGET_WITH_PATH",
            GateCode::PaybackAboveMax => "PAYBACK_ABOVE_MAX",
            GateCode::DeliveryTooLong => "DELIVERY_TOO_LONG",
            GateCode::UnboundedOwnerTime => "UNBOUNDED_OWNER_TIME",
            GateCode::UnboundedSupport => "UNBOUNDED_SUPPORT",
            GateCode::UnboundedPaymentAccess => "UNBOUNDED_PAYMENT_ACCESS",
            GateCode::UnboundedScope => "UNBOUNDED_SCOPE",
            GateCode::NoTestableEvidence => "NO_TESTABLE_EVIDENCE",
            GateCode::SingleDemandSignal => "SINGLE_DEMAND_SIGNAL",
            GateCode::ContradictedEvidence => "CONTRADICTED_EVIDENCE",
            GateCode::LegalUnresolved => "LEGAL_UNRESOLVED",
            GateCode::DiligenceOpen => "DILIGENCE_OPEN",
            GateCode::DealRedFlag => "DEAL_RED_FLAG",
            GateCode::JurisdictionNotAllowed => "JURISDICTION_NOT_ALLOWED",
            GateCode::NoStagedStopRule => "NO_STAGED_STOP_RULE",
            GateCode::DownsideUnstated => "DOWNSIDE_UNSTATED",
            GateCode::UnknownInput => "UNKNOWN_INPUT",
        }
    }
}

/// One failed gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateFailure {
    pub code: GateCode,
    /// `HOLD` or `REJECT`.
    pub outcome: Verdict,
    /// The field it is about (dotted path under the opportunity), when one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub detail: String,
}

impl GateFailure {
    /// `UNKNOWN_INPUT:<field>`, else the code — what a `WeeklyPortfolio`
    /// row lists.
    pub fn label(&self) -> String {
        match (&self.code, &self.field) {
            (GateCode::UnknownInput, Some(f)) => format!("{}:{f}", self.code.as_str()),
            _ => self.code.as_str().to_string(),
        }
    }
}

/// The gates' answer for one opportunity at one time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateVerdict {
    pub verdict: Verdict,
    pub as_of: Time,
    pub economics_version: u32,
    /// `Scenarios::inputs_sha256` of the figures the gates read.
    pub inputs_sha256: String,
    /// Rejections first, then by code and field; one per (code, field).
    pub failures: Vec<GateFailure>,
}

impl GateVerdict {
    /// Each failure's label once, in failure order.
    pub fn labels(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.failures
            .iter()
            .map(GateFailure::label)
            .filter(|l| seen.insert(l.clone()))
            .collect()
    }
}

/// The fields behind the `HOLD` failures, sorted: the next information
/// worth buying (PRD § 12).
pub fn next_information(v: &GateVerdict) -> Vec<String> {
    v.failures
        .iter()
        .filter(|f| f.outcome == Verdict::Hold)
        .filter_map(|f| f.field.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// What a cited record can establish (PRD § 5.1), as the O2 as-of view
/// classes a `domain::source` record: `trust = trigger_only` ⇒ `TRIGGER`,
/// else `customer_demand` ⇒ `DEMAND`, else `FACT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CitedKind {
    Fact,
    /// Counts only in aggregate: one demand event is not demand.
    Demand,
    /// News, social or LLM inference: starts a search, supports nothing.
    Trigger,
}

/// What the gates read of one cited source record — its time, kind,
/// freshness and contradiction, never its content. The caller builds it
/// from the O2 as-of view (the provenance type is `domain::source`'s; soe
/// never reads a store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitedRecord {
    /// `<source_id>:<native_id>:<content_hash>`, in full.
    pub record_id: String,
    /// The real-world event; copies and corrections share it.
    pub event_key: String,
    pub kind: CitedKind,
    /// When it became knowable (observed, not merely published).
    pub knowable_at: Time,
    /// When it stops holding (a listing's freshness); none = no end stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<Time>,
    /// When an unresolved contradiction of it became knowable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contradicted_at: Option<Time>,
}

impl CitedRecord {
    /// Surely knowable at `as_of` (evidence rule table).
    pub fn knowable(&self, as_of: &Time) -> bool {
        self.knowable_at.order(as_of) == TimeOrder::NotAfter
    }

    /// Not expired at `as_of`.
    pub fn fresh(&self, as_of: &Time) -> bool {
        match &self.valid_until {
            None => true,
            Some(until) => matches!(
                (as_of.latest(), until.earliest()),
                (Some(a), Some(u)) if a < u
            ),
        }
    }

    /// Contradicted at `as_of` (unless the contradiction is surely later).
    pub fn contradicted(&self, as_of: &Time) -> bool {
        self.contradicted_at
            .is_some_and(|t| t.order(as_of) != TimeOrder::After)
    }

    /// Supports a candidate at `as_of`: knowable, fresh, uncontradicted and
    /// not `TRIGGER`.
    pub fn supports(&self, as_of: &Time) -> bool {
        self.knowable(as_of)
            && self.fresh(as_of)
            && !self.contradicted(as_of)
            && self.kind != CitedKind::Trigger
    }
}

/// A list of record views (an eval case's, a `--cited` file's): record ids
/// unique, ids and event keys stated.
pub fn cited_problems(cited: &[CitedRecord], p: &mut Problems) {
    p.unique("cited.record_id", cited.iter().map(|c| &c.record_id));
    for (i, c) in cited.iter().enumerate() {
        p.text(&format!("cited[{i}].record_id"), &c.record_id);
        p.text(&format!("cited[{i}].event_key"), &c.event_key);
    }
}

/// The view of each record `opp` cites (the first given per id), in
/// `signals` order; records it does not cite are left out.
pub fn cited_views<'a>(opp: &Opportunity, cited: &'a [CitedRecord]) -> Vec<&'a CitedRecord> {
    opp.signals
        .iter()
        .filter_map(|id| cited.iter().find(|c| &c.record_id == id))
        .collect()
}

/// The failures found so far.
#[derive(Default)]
struct Run {
    out: Vec<GateFailure>,
}

impl Run {
    fn push(&mut self, outcome: Verdict, code: GateCode, field: Option<&str>, detail: String) {
        self.out.push(GateFailure {
            code,
            outcome,
            field: field.map(str::to_string),
            detail,
        });
    }

    fn hold(&mut self, code: GateCode, field: Option<&str>, detail: impl Into<String>) {
        self.push(Verdict::Hold, code, field, detail.into());
    }

    fn reject(&mut self, code: GateCode, field: Option<&str>, detail: impl Into<String>) {
        self.push(Verdict::Reject, code, field, detail.into());
    }

    /// `UNKNOWN_INPUT:<field>` for each field `what` lacks.
    fn unknown(&mut self, lacking: &[String], what: &str) {
        for f in lacking {
            self.hold(
                GateCode::UnknownInput,
                Some(f),
                format!("{what} needs it — unknown is never 0"),
            );
        }
    }

    fn finish(mut self, as_of: Time, sc: &Scenarios) -> GateVerdict {
        let mut seen = BTreeSet::new();
        self.out.retain(|f| seen.insert((f.code, f.field.clone())));
        self.out.sort_by(|a, b| {
            let key = |f: &GateFailure| (f.outcome != Verdict::Reject, f.code, f.field.clone());
            key(a).cmp(&key(b))
        });
        let verdict = if self.out.iter().any(|f| f.outcome == Verdict::Reject) {
            Verdict::Reject
        } else if self.out.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Hold
        };
        GateVerdict {
            verdict,
            as_of,
            economics_version: sc.economics_version,
            inputs_sha256: sc.inputs_sha256.clone(),
            failures: self.out,
        }
    }
}

/// The PRD § 7.2 gates of `opp` at `as_of` (module tables). `cited` may hold
/// records `opp` does not cite; they are ignored.
pub fn gates(
    opp: &Opportunity,
    cited: &[CitedRecord],
    profile: &OperatorProfile,
    as_of: Time,
) -> Result<GateVerdict, ValueError> {
    if !profile.is_signed() {
        return Err(ValueError::new(
            codes::OPERATOR_PROFILE_UNSIGNED,
            format!(
                "profile `{}`: unsigned — the gates run on signed parameters only",
                profile.id
            ),
        ));
    }
    if !as_of.is_known() {
        return Err(ValueError::new(
            codes::INVALID_TIME,
            "the decision time must be known",
        ));
    }
    if opp.as_of.order(&as_of) == TimeOrder::After {
        return Err(ValueError::new(
            codes::FUTURE_LEAKAGE,
            format!(
                "opportunity `{}` as_of {} is after the decision time {as_of}",
                opp.id, opp.as_of
            ),
        ));
    }
    let sc = scenarios(opp, profile)?;
    let mut r = Run::default();
    mechanism(opp, &mut r);
    exposure(opp, &sc, profile, &mut r)?;
    contribution(opp, &sc, profile, &mut r);
    payback(&sc, profile, &mut r);
    delivery(opp, profile, &mut r);
    bounds(opp, &mut r);
    evidence(opp, cited, &as_of, &mut r);
    legal(opp, &mut r);
    jurisdictions(opp, profile, &mut r);
    stop_rule(opp, &mut r);
    downside(opp, &mut r);
    Ok(r.finish(as_of, &sc))
}

fn mechanism(opp: &Opportunity, r: &mut Run) {
    match opp.mechanism {
        Mechanism::Hold => r.hold(
            GateCode::MechanismHold,
            Some("mechanism"),
            "the mechanism itself is HOLD",
        ),
        Mechanism::Reject => r.reject(
            GateCode::MechanismReject,
            Some("mechanism"),
            "the mechanism itself is REJECT",
        ),
        _ => {}
    }
}

fn money(p: &OperatorProfile, m: Minor) -> Money {
    Money::new(m, p.currency)
}

/// A closed value as serialized (`SELLER_CLAIM`).
fn name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn exposure(
    opp: &Opportunity,
    sc: &Scenarios,
    p: &OperatorProfile,
    r: &mut Run,
) -> Result<(), ValueError> {
    let cap = p.max_cash_exposure;
    let above = |r: &mut Run, field: &str, amount: Minor| {
        if amount > cap {
            r.reject(
                GateCode::CashExposureAboveCap,
                Some(field),
                format!("{} > max_cash_exposure {}", money(p, amount), money(p, cap)),
            );
        }
    };
    match &sc.downside.initial_capital {
        Metric::Known(v) => above(r, "economics.initial", *v),
        Metric::Unknown { fields } => r.unknown(fields, "cash exposure (initial capital)"),
    }
    let native = |r: &mut Run, field: &str, amount: Minor| -> Result<(), ValueError> {
        match to_currency(&opp.economics, p.currency, amount, Flow::Outflow)? {
            Some(v) => above(r, field, v),
            None => r.unknown(&[fields::FX.to_string()], "cash exposure"),
        }
        Ok(())
    };
    if let Some(x) = &opp.experiment {
        native(r, "experiment.stages", x.total_cash()?)?;
        for (i, stage) in x.stages.iter().enumerate() {
            let field = format!("experiment.stages[{i}].cash");
            match to_currency(&opp.economics, p.currency, stage.cash, Flow::Outflow)? {
                Some(v) if v > p.max_validation_tranche => r.reject(
                    GateCode::TrancheAboveMax,
                    Some(&field),
                    format!(
                        "stage `{}`: {} > max_validation_tranche {}",
                        stage.name,
                        money(p, v),
                        money(p, p.max_validation_tranche)
                    ),
                ),
                Some(_) => {}
                None => r.unknown(&[fields::FX.to_string()], "a stage's tranche"),
            }
        }
    }
    // An unknown max loss is DOWNSIDE_UNSTATED's.
    if let Some(high) = opp.risk.max_loss.cash.end(Side::Adverse, Better::Lower) {
        native(r, "risk.max_loss.cash", *high)?;
    }
    Ok(())
}

/// An experiment exists and its first spend carries a stop rule.
fn staged(opp: &Opportunity) -> bool {
    opp.experiment.as_ref().is_some_and(|x| {
        x.first_spend()
            .map(|(_, stage)| stage.stop_rule.is_some())
            .unwrap_or(true)
    })
}

/// A passing pre-tax figure under a `POST_TAX` profile.
fn post_tax(p: &OperatorProfile, r: &mut Run, what: &str) {
    if p.profit_basis == ProfitBasis::PostTax {
        r.hold(
            GateCode::UnknownInput,
            Some("economics.tax_review"),
            format!("{what} passes pre-tax; the profile wants POST_TAX and v1 has no tax model"),
        );
    }
}

fn contribution(opp: &Opportunity, sc: &Scenarios, p: &OperatorProfile, r: &mut Run) {
    let basis = p.contribution_basis;
    let target = p.min_monthly_contribution;
    let name = match basis {
        ContributionBasis::TimeAdjusted => "time_adjusted_contribution",
        ContributionBasis::Cash => "monthly_cash_contribution",
    };
    match sc.base.contribution(basis) {
        Metric::Unknown { fields } => r.unknown(fields, &format!("base {name}")),
        Metric::Known(base) if *base < target => {
            let upside = sc.upside.contribution(basis).known().copied();
            let reaches = upside.is_some_and(|u| u >= target);
            let detail = format!(
                "base {name} {} < min_monthly_contribution {}",
                money(p, *base),
                money(p, target)
            );
            if reaches && staged(opp) {
                r.hold(
                    GateCode::BelowTargetWithPath,
                    Some("experiment"),
                    format!(
                        "{detail}; the upside {} reaches it and a staged experiment tests it",
                        money(p, upside.unwrap_or(*base))
                    ),
                );
            } else {
                r.reject(GateCode::ContributionBelowTarget, Some(name), detail);
            }
        }
        Metric::Known(_) => post_tax(p, r, &format!("base {name}")),
    }
}

fn payback(sc: &Scenarios, p: &OperatorProfile, r: &mut Run) {
    let max = p.max_payback_months;
    match &sc.base.payback {
        Metric::Unknown { fields } => r.unknown(fields, "base payback"),
        Metric::Known(Payback::Months(n)) if *n <= max => post_tax(p, r, "base payback"),
        Metric::Known(back) => r.reject(
            GateCode::PaybackAboveMax,
            Some("payback"),
            match back {
                Payback::Months(n) => {
                    format!("base payback {n} months > max_payback_months {max}")
                }
                Payback::NotReached => {
                    format!("base payback not reached (max_payback_months {max})")
                }
            },
        ),
    }
}

fn delivery(opp: &Opportunity, p: &OperatorProfile, r: &mut Run) {
    let RevenueModel::OneOff { delivery_weeks, .. } = &opp.economics.revenue else {
        return;
    };
    let max = p.max_one_off_delivery_weeks;
    match delivery_weeks.value.base() {
        None => r.unknown(&[fields::DELIVERY_WEEKS.to_string()], "the delivery length"),
        Some(w) if *w > max => r.hold(
            GateCode::DeliveryTooLong,
            Some(fields::DELIVERY_WEEKS),
            format!("base {w} weeks > max_one_off_delivery_weeks {max}"),
        ),
        Some(_) => {}
    }
}

fn bounds(opp: &Opportunity, r: &mut Run) {
    for (f, b) in opp.risk.bounds.each() {
        let code = match f {
            "owner_time" => GateCode::UnboundedOwnerTime,
            "support" => GateCode::UnboundedSupport,
            "payment_access" => GateCode::UnboundedPaymentAccess,
            _ => GateCode::UnboundedScope,
        };
        let field = format!("risk.bounds.{f}");
        match b {
            Boundedness::Bounded => {}
            Boundedness::Unbounded => r.reject(code, Some(&field), format!("{f} is UNBOUNDED")),
            Boundedness::Unknown => r.hold(code, Some(&field), format!("{f} bound UNKNOWN")),
        }
    }
    // Owner time cannot be bounded on unknown hours.
    if opp.risk.bounds.owner_time != Boundedness::Unbounded {
        let mut hours = vec![(
            fields::OWNER_HOURS,
            opp.economics.owner_hours_per_month.value.is_known(),
        )];
        if let RevenueModel::OneOff {
            owner_hours_total, ..
        } = &opp.economics.revenue
        {
            hours.push((
                fields::OWNER_HOURS_TOTAL,
                owner_hours_total.value.is_known(),
            ));
        }
        for (field, known) in hours {
            if !known {
                r.hold(
                    GateCode::UnboundedOwnerTime,
                    Some(field),
                    "owner hours UNKNOWN: owner time is not bounded",
                );
            }
        }
    }
}

fn evidence(opp: &Opportunity, cited: &[CitedRecord], as_of: &Time, r: &mut Run) {
    let knowable: Vec<&CitedRecord> = cited_views(opp, cited)
        .into_iter()
        .filter(|c| c.knowable(as_of))
        .collect();
    let contradicted: Vec<&str> = knowable
        .iter()
        .filter(|c| c.contradicted(as_of))
        .map(|c| c.record_id.as_str())
        .collect();
    if !contradicted.is_empty() {
        r.hold(
            GateCode::ContradictedEvidence,
            Some("signals"),
            format!("contradicted at {as_of}: {}", contradicted.join(", ")),
        );
    }
    let support: Vec<&CitedRecord> = knowable
        .iter()
        .copied()
        .filter(|c| c.supports(as_of))
        .collect();
    let facts = support.iter().filter(|c| c.kind == CitedKind::Fact).count();
    let demand: BTreeSet<&str> = support
        .iter()
        .filter(|c| c.kind == CitedKind::Demand)
        .map(|c| c.event_key.as_str())
        .collect();
    // Seen from `as_of`: a record not yet knowable is no different from one
    // without a view, so the tally never hints at a later record.
    let tally = format!(
        "{} cited: {} not knowable at {as_of}, {} trigger only, {} expired, {} contradicted",
        opp.signals.len(),
        opp.signals.len().saturating_sub(knowable.len()),
        knowable
            .iter()
            .filter(|c| c.kind == CitedKind::Trigger)
            .count(),
        knowable.iter().filter(|c| !c.fresh(as_of)).count(),
        contradicted.len(),
    );
    if facts == 0 && demand.is_empty() {
        r.hold(
            GateCode::NoTestableEvidence,
            Some("signals"),
            format!("no cited record supports it — {tally}"),
        );
    } else if facts == 0 && demand.len() == 1 {
        r.hold(
            GateCode::SingleDemandSignal,
            Some("signals"),
            format!(
                "one demand event ({}) is not demand — {tally}",
                demand.iter().next().copied().unwrap_or_default()
            ),
        );
    }
    let revenue_evidenced = opp
        .economics
        .input_states()
        .iter()
        .any(|s| s.field.starts_with("economics.revenue.") && s.evidenced);
    if !revenue_evidenced {
        r.hold(
            GateCode::NoTestableEvidence,
            Some("economics.revenue"),
            "no revenue input names its evidence",
        );
    }
}

fn legal(opp: &Opportunity, r: &mut Run) {
    for (i, risk) in opp.risk.risks.iter().enumerate() {
        if risk.kind.is_legal() && risk.status != RiskStatus::Resolved {
            r.hold(
                GateCode::LegalUnresolved,
                Some(&format!("risk.risks[{i}]")),
                format!(
                    "{} risk {}: {}",
                    name(&risk.kind),
                    name(&risk.status),
                    risk.control
                ),
            );
        }
    }
    let Some(deal) = &opp.deal else {
        return;
    };
    if !deal.red_flags.is_empty() {
        r.reject(
            GateCode::DealRedFlag,
            Some("deal.red_flags"),
            deal.red_flags.join("; "),
        );
    }
    for (f, block) in deal.blocks() {
        let field = format!("deal.{f}");
        let status = block.status;
        match status {
            DiligenceStatus::Verified => {}
            DiligenceStatus::RedFlag => r.reject(
                GateCode::DealRedFlag,
                Some(&field),
                format!("{f}: RED_FLAG"),
            ),
            _ => {
                let code = if matches!(f, "ownership" | "legal" | "transition") {
                    GateCode::LegalUnresolved
                } else {
                    GateCode::DiligenceOpen
                };
                r.hold(
                    code,
                    Some(&field),
                    format!("{f}: {} — verify it", name(&status)),
                );
            }
        }
    }
}

fn jurisdictions(opp: &Opportunity, p: &OperatorProfile, r: &mut Run) {
    for (f, code) in opp.jurisdictions.each() {
        let field = format!("jurisdictions.{f}");
        let code = code.trim();
        if !stated(code) {
            r.unknown(&[field], "the jurisdiction gate");
        } else if !p.jurisdictions_allow.iter().any(|a| a == code) {
            r.hold(
                GateCode::JurisdictionNotAllowed,
                Some(&field),
                format!("`{code}` is not in jurisdictions_allow"),
            );
        }
    }
}

fn stop_rule(opp: &Opportunity, r: &mut Run) {
    let Some(x) = &opp.experiment else {
        r.hold(
            GateCode::NoStagedStopRule,
            Some("experiment"),
            "no experiment: the first material spend has no staged stop rule",
        );
        return;
    };
    if let Some((i, stage)) = x.first_spend() {
        if stage.stop_rule.is_none() {
            r.reject(
                GateCode::NoStagedStopRule,
                Some(&format!("experiment.stages[{i}].stop_rule")),
                format!(
                    "stage `{}` spends {} with no stop rule",
                    stage.name, stage.cash
                ),
            );
        }
    }
}

fn downside(opp: &Opportunity, r: &mut Run) {
    for term in opp.risk.max_loss.unstated() {
        r.hold(
            GateCode::DownsideUnstated,
            Some(&format!("risk.max_loss.{term}")),
            format!("the downside in {term} terms is UNKNOWN"),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::economics::tests::{
        economics, known, m, with_economics, VERIFIED_DEAL,
    };
    use crate::domain::soe::opportunity::tests::recurring;
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::profile::ContributionBasis;
    use crate::domain::soe::risk::{Risk, RiskKind};
    use crate::domain::soe::value::{Bps, Currency, Est, FxRate};

    /// The signal `RECURRING` cites, in full.
    pub(crate) const SIGNAL: &str =
        "sec_edgar:0000320193-26-000006:0000000000000000000000000000000000000000000000000000000000000001";

    /// A staged experiment: the first spend (750.00) under a stop rule.
    pub(crate) const EXPERIMENT: &str = r#"
hypothesis = "partners resell the integration"
metric = "paid partner seats"
threshold = { op = "GE", value = 3, unit = "seats" }
deadline = "2026-11-15"
expires_at = "2026-11-30"
max_owner_hours = 20
stop_rule = "no paid seat by the deadline"
approvals = ["CONTACT", "SPEND"]

[[stages]]
name = "interviews"
cash = 0
recoverable = 0
owner_hours = 5

[[stages]]
name = "pilot"
cash = "750.00"
recoverable = "0.00"
owner_hours = 14
stop_rule = "fewer than 3 seats sold"
"#;

    pub(crate) fn t(s: &str) -> Time {
        s.parse().unwrap()
    }

    /// The decision time of every test.
    pub(crate) fn at() -> Time {
        t("2026-10-05T12:00:00Z")
    }

    pub(crate) fn view(id: &str, event: &str, kind: CitedKind, knowable: &str) -> CitedRecord {
        CitedRecord {
            record_id: id.into(),
            event_key: event.into(),
            kind,
            knowable_at: t(knowable),
            valid_until: None,
            contradicted_at: None,
        }
    }

    /// The one fact `SIGNAL` names, knowable before the decision.
    pub(crate) fn cited() -> Vec<CitedRecord> {
        vec![view(
            SIGNAL,
            "sec:filing:0000320193-26-000006",
            CitedKind::Fact,
            "2026-10-02",
        )]
    }

    /// Jurisdictions all stated and allowed, a staged experiment.
    pub(crate) fn complete(mut o: Opportunity) -> Opportunity {
        o.jurisdictions.entity = "DE".into();
        o.jurisdictions.tax = "DE".into();
        o.experiment = Some(toml::from_str(EXPERIMENT).unwrap());
        o
    }

    /// A revenue share that passes every gate of the synthetic profile:
    /// 9350.00 × 40 % = 3740.00 − fixed 40.00 − 10 h × 70.00 = 3000.00 —
    /// exactly `min_monthly_contribution`. Initial 900.00 pays back in
    /// month 2; the downside exposure is 900.00 (max loss 1600.00).
    pub(crate) fn passing() -> Opportunity {
        revenue_share("\"9350.00\"", "4000", "10")
    }

    pub(crate) fn revenue_share(partner: &str, share: &str, hours: &str) -> Opportunity {
        complete(with_economics(
            "PARTNER_REVSHARE",
            &economics(
                &[
                    ("variable_cost", "0"),
                    ("fixed_costs_per_month", "\"40.00\""),
                    ("owner_hours_per_month", hours),
                    ("ramp_months", "1"),
                ],
                "REVENUE_SHARE",
                &[
                    ("partner_monthly_revenue", partner),
                    ("share", share),
                    ("collection_loss", "0"),
                ],
                ["\"0.00\"", "\"900.00\"", "\"0.00\"", "\"0.00\""],
            ),
            "",
        ))
    }

    /// An acquisition that passes the economics: 6000.00 a month churned
    /// 1.5 %, variable 9 %, fixed 200.00, 9 h; 15000.00 to buy it.
    pub(crate) fn acquisition(deal: &str) -> Opportunity {
        complete(with_economics(
            "ACQUIRE_TRANSFORM",
            &economics(
                &[
                    ("variable_cost", "900"),
                    ("fixed_costs_per_month", "\"200.00\""),
                    ("owner_hours_per_month", "9"),
                    ("ramp_months", "0"),
                ],
                "ACQUISITION",
                &[
                    ("asset_monthly_revenue", "\"6000.00\""),
                    ("churn_per_month", "150"),
                    ("collection_loss", "0"),
                ],
                ["\"15000.00\"", "\"0.00\"", "\"0.00\"", "\"0.00\""],
            ),
            deal,
        ))
    }

    fn run(o: &Opportunity) -> GateVerdict {
        run_with(o, &cited(), &synthetic(), at())
    }

    fn run_with(
        o: &Opportunity,
        c: &[CitedRecord],
        p: &OperatorProfile,
        when: Time,
    ) -> GateVerdict {
        gates(o, c, p, when).unwrap_or_else(|e| panic!("{e}"))
    }

    fn labels(v: &GateVerdict) -> Vec<String> {
        v.labels()
    }

    fn has(v: &GateVerdict, code: GateCode, outcome: Verdict) -> bool {
        v.failures
            .iter()
            .any(|f| f.code == code && f.outcome == outcome)
    }

    #[test]
    fn passing_candidate_passes() {
        let o = passing();
        let v = run(&o);
        assert_eq!((v.verdict, v.failures.clone()), (Verdict::Pass, vec![]));
        assert!(next_information(&v).is_empty());
        let sc = scenarios(&o, &synthetic()).unwrap();
        assert_eq!(v.inputs_sha256, sc.inputs_sha256);
        assert_eq!(v.as_of, at());
        // The verdict serializes with codes as text.
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["verdict"], "PASS");
        for c in GateCode::ALL {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }

    #[test]
    fn cash_cap_passes_at_exactly_the_cap() {
        // Downside initial = setup 900.00 + working capital high 19100.00.
        let mut o = passing();
        o.economics.initial.working_capital.value =
            Est::range(m("0.00"), m("0.00"), m("19100.00")).unwrap();
        let sc = scenarios(&o, &synthetic()).unwrap();
        assert_eq!(known(&sc.downside.initial_capital), m("20000.00"));
        assert_eq!(run(&o).verdict, Verdict::Pass);
    }

    #[test]
    fn cash_cap_rejects_one_cent_above() {
        let mut o = passing();
        o.economics.initial.working_capital.value =
            Est::range(m("0.00"), m("0.00"), m("19100.01")).unwrap();
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(labels(&v), ["CASH_EXPOSURE_ABOVE_CAP"]);
        assert_eq!(v.failures[0].field.as_deref(), Some("economics.initial"));
        assert!(
            v.failures[0]
                .detail
                .contains("20000.01 EUR > max_cash_exposure 20000.00 EUR"),
            "{}",
            v.failures[0].detail
        );
        // The max loss and the stages count too.
        let mut loss = passing();
        loss.risk.max_loss.cash = Est::range(m("0.00"), m("150.00"), m("20000.01")).unwrap();
        assert_eq!(labels(&run(&loss)), ["CASH_EXPOSURE_ABOVE_CAP"]);
        assert_eq!(
            run(&loss).failures[0].field.as_deref(),
            Some("risk.max_loss.cash")
        );
    }

    #[test]
    fn contribution_passes_at_exactly_target() {
        let o = passing();
        let sc = scenarios(&o, &synthetic()).unwrap();
        assert_eq!(known(&sc.base.time_adjusted_contribution), m("3000.00"));
        assert_eq!(run(&o).verdict, Verdict::Pass);
    }

    #[test]
    fn contribution_fails_one_cent_below() {
        // 9349.98 × 40 % = 3739.992, floored to 3739.99 ⇒ time-adjusted 2999.99.
        let o = revenue_share("\"9349.98\"", "4000", "10");
        let sc = scenarios(&o, &synthetic()).unwrap();
        assert_eq!(known(&sc.base.time_adjusted_contribution), m("2999.99"));
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(labels(&v), ["CONTRIBUTION_BELOW_TARGET"]);
        assert_eq!(
            v.failures[0].field.as_deref(),
            Some("time_adjusted_contribution")
        );
    }

    #[test]
    fn fx_amount_rounding_up_crosses_cap() {
        // In USD at 0.8 EUR: 900.00 + 24100.00 = 25000.00 USD is exactly
        // 20000.00 EUR (passes); 25000.01 USD is 20000.008 EUR, an outflow,
        // ceiled to 20000.01 (rejected) — floored it would have passed.
        let usd = |wc: &str| {
            let mut o = passing();
            o.economics.currency = Currency::Usd;
            o.economics.fx = Some(
                FxRate::new(
                    "USD/EUR".parse().unwrap(),
                    "0.8",
                    "url:https://example.org/fx".parse().unwrap(),
                    t("2026-10-01"),
                )
                .unwrap(),
            );
            o.economics.initial.working_capital.value =
                Est::range(m("0.00"), m("0.00"), m(wc)).unwrap();
            run(&o)
        };
        assert!(!usd("24100.00")
            .failures
            .iter()
            .any(|f| f.code == GateCode::CashExposureAboveCap));
        let v = usd("24100.01");
        let f = v
            .failures
            .iter()
            .find(|f| f.code == GateCode::CashExposureAboveCap)
            .unwrap();
        assert!(f.detail.starts_with("20000.01 EUR"), "{}", f.detail);
        assert_eq!(v.verdict, Verdict::Reject);
        // No rate: the exposure is unknown — held, never passed.
        let mut o = passing();
        o.economics.currency = Currency::Usd;
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Hold);
        assert!(labels(&v).contains(&"UNKNOWN_INPUT:economics.fx".to_string()));
    }

    #[test]
    fn twelve_k_revenue_160_hours_rejected_after_shadow_time() {
        // 16000.00 × 75 % = 12000.00 collected a month, 160 owner hours:
        // cash 11960.00 clears the target, but 160 h × 70.00 = 11200.00 of
        // owner time leaves 760.00 time-adjusted.
        let o = revenue_share("\"16000.00\"", "7500", "160");
        let sc = scenarios(&o, &synthetic()).unwrap();
        assert_eq!(known(&sc.base.monthly_revenue_collected), m("12000.00"));
        assert_eq!(known(&sc.base.monthly_cash_contribution), m("11960.00"));
        assert_eq!(known(&sc.base.time_adjusted_contribution), m("760.00"));
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(labels(&v), ["CONTRIBUTION_BELOW_TARGET"]);
        // On a CASH basis the same candidate passes: the shadow time rejects it.
        let mut cash = synthetic();
        cash.contribution_basis = ContributionBasis::Cash;
        assert_eq!(run_with(&o, &cited(), &cash, at()).verdict, Verdict::Pass);
    }

    #[test]
    fn unknown_price_holds_never_passes_or_rejects() {
        // The recurring example, price UNKNOWN, everything else in order.
        let o = complete(recurring());
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Hold);
        assert!(v.failures.iter().all(|f| f.outcome == Verdict::Hold));
        assert_eq!(
            labels(&v),
            ["UNKNOWN_INPUT:economics.revenue.price_per_month"]
        );
        assert_eq!(next_information(&v), ["economics.revenue.price_per_month"]);
        // The same under a CASH basis and a POST_TAX profile: still a hold.
        let mut p = synthetic();
        p.contribution_basis = ContributionBasis::Cash;
        p.profit_basis = ProfitBasis::PostTax;
        assert_eq!(run_with(&o, &cited(), &p, at()).verdict, Verdict::Hold);
    }

    #[test]
    fn unknown_hours_hold_unbounded_owner_time() {
        let mut o = passing();
        o.economics.owner_hours_per_month.value = Est::unknown("no time log yet");
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Hold);
        assert_eq!(
            labels(&v),
            [
                "UNBOUNDED_OWNER_TIME",
                "UNKNOWN_INPUT:economics.owner_hours_per_month"
            ]
        );
        assert_eq!(next_information(&v), ["economics.owner_hours_per_month"]);
        // An UNKNOWN bound holds, an UNBOUNDED one rejects.
        let mut b = passing();
        b.risk.bounds.support = Boundedness::Unknown;
        b.risk.bounds.scope = Boundedness::Unbounded;
        let v = run(&b);
        assert_eq!(v.verdict, Verdict::Reject);
        assert!(has(&v, GateCode::UnboundedScope, Verdict::Reject));
        assert!(has(&v, GateCode::UnboundedSupport, Verdict::Hold));
    }

    #[test]
    fn unverifiable_transfer_acquisition_holds_with_diligence_questions() {
        let verified = acquisition(VERIFIED_DEAL);
        assert_eq!(run(&verified).verdict, Verdict::Pass);
        let o = acquisition(
            &VERIFIED_DEAL
                .replace(
                    "ownership = { status = \"VERIFIED\"",
                    "ownership = { status = \"SELLER_CLAIM\"",
                )
                .replace(
                    "transition = { status = \"VERIFIED\"",
                    "transition = { status = \"UNVERIFIED\"",
                )
                .replace(
                    "costs = { status = \"VERIFIED\"",
                    "costs = { status = \"UNKNOWN\"",
                ),
        );
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Hold);
        assert_eq!(labels(&v), ["LEGAL_UNRESOLVED", "DILIGENCE_OPEN"]);
        // The diligence questions: the unverified blocks, sorted.
        assert_eq!(
            next_information(&v),
            ["deal.costs", "deal.ownership", "deal.transition"]
        );
        // A seller's word is never the fact: the detail names its status.
        assert!(v
            .failures
            .iter()
            .any(|f| f.detail.starts_with("ownership: SELLER_CLAIM")));
    }

    #[test]
    fn red_flag_rejects() {
        let listed = acquisition(&VERIFIED_DEAL.replace(
            "red_flags = []",
            "red_flags = [\"undisclosed chargebacks\"]",
        ));
        let v = run(&listed);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(labels(&v), ["DEAL_RED_FLAG"]);
        assert_eq!(v.failures[0].detail, "undisclosed chargebacks");
        let block = acquisition(&VERIFIED_DEAL.replace(
            "revenue_proof = { status = \"VERIFIED\"",
            "revenue_proof = { status = \"RED_FLAG\"",
        ));
        let v = run(&block);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(v.failures[0].field.as_deref(), Some("deal.revenue_proof"));
    }

    #[test]
    fn later_signal_cannot_change_verdict_at_t() {
        let o = passing();
        // At t the candidate rests on its fact; a contradiction known the
        // next day and a fact observed the same day (a day against an
        // instant inside it) cannot change the verdict at t.
        let mut later = cited();
        later[0].contradicted_at = Some(t("2026-10-06T09:00:00Z"));
        let v_t = run_with(&o, &cited(), &synthetic(), at());
        assert_eq!(v_t.verdict, Verdict::Pass);
        assert_eq!(run_with(&o, &later, &synthetic(), at()), v_t);
        // A day later the contradiction is knowable: held.
        let next = t("2026-10-06T12:00:00Z");
        let v = run_with(&o, &later, &synthetic(), next);
        // Contested, the fact no longer supports it either.
        assert_eq!(
            labels(&v),
            ["NO_TESTABLE_EVIDENCE", "CONTRADICTED_EVIDENCE"]
        );
        assert!(
            v.failures[1].detail.contains(SIGNAL),
            "{}",
            v.failures[1].detail
        );

        // Only a trigger at t; the fact arrives the same day and after.
        let mut trigger = o.clone();
        trigger.signals.push("news_wire:item-7:00ab".into());
        let triggers = vec![view(
            "news_wire:item-7:00ab",
            "news:item:7",
            CitedKind::Trigger,
            "2026-10-01",
        )];
        let mut with_fact = triggers.clone();
        with_fact.push(view(
            SIGNAL,
            "sec:filing:0000320193-26-000006",
            CitedKind::Fact,
            "2026-10-05",
        ));
        let held = run_with(&trigger, &triggers, &synthetic(), at());
        assert_eq!(labels(&held), ["NO_TESTABLE_EVIDENCE"]);
        assert_eq!(run_with(&trigger, &with_fact, &synthetic(), at()), held);
        let after = run_with(
            &trigger,
            &with_fact,
            &synthetic(),
            t("2026-10-06T00:00:00Z"),
        );
        assert_eq!(after.verdict, Verdict::Pass);
        // A decision time before the opportunity was written is refused.
        let e = gates(&o, &cited(), &synthetic(), t("2026-10-04T12:00:00Z")).unwrap_err();
        assert_eq!(e.code, codes::FUTURE_LEAKAGE);
    }

    #[test]
    fn social_only_evidence_holds() {
        let mut o = passing();
        o.signals = vec![
            "social_feed:post-1:aa".into(),
            "social_feed:post-2:bb".into(),
        ];
        let posts = vec![
            view(
                "social_feed:post-1:aa",
                "social:post:1",
                CitedKind::Trigger,
                "2026-10-01",
            ),
            view(
                "social_feed:post-2:bb",
                "social:post:2",
                CitedKind::Trigger,
                "2026-10-02",
            ),
        ];
        let v = run_with(&o, &posts, &synthetic(), at());
        assert_eq!(v.verdict, Verdict::Hold);
        assert_eq!(labels(&v), ["NO_TESTABLE_EVIDENCE"]);
        assert!(
            v.failures[0].detail.contains("2 trigger only"),
            "{}",
            v.failures[0].detail
        );
        // Nothing cited at all, and records the opportunity does not cite: the same.
        assert_eq!(
            labels(&run_with(&o, &[], &synthetic(), at())),
            ["NO_TESTABLE_EVIDENCE"]
        );
        assert_eq!(
            labels(&run_with(&o, &cited(), &synthetic(), at())),
            ["NO_TESTABLE_EVIDENCE"]
        );
        // No revenue input names evidence: held on the economics too.
        let mut bare = passing();
        if let RevenueModel::RevenueShare {
            partner_monthly_revenue,
            share,
            collection_loss,
        } = &mut bare.economics.revenue
        {
            partner_monthly_revenue.evidence.clear();
            share.evidence.clear();
            collection_loss.evidence.clear();
        }
        let v = run(&bare);
        assert_eq!(v.failures[0].field.as_deref(), Some("economics.revenue"));
        assert_eq!(labels(&v), ["NO_TESTABLE_EVIDENCE"]);
    }

    #[test]
    fn one_demand_event_holds_two_pass_and_expired_supports_nothing() {
        let mut o = passing();
        o.signals = vec!["tenders:n-1:aa".into(), "tenders:n-1-copy:bb".into()];
        let one = vec![
            view(
                "tenders:n-1:aa",
                "ted:procedure:1",
                CitedKind::Demand,
                "2026-10-01",
            ),
            view(
                "tenders:n-1-copy:bb",
                "ted:procedure:1",
                CitedKind::Demand,
                "2026-10-02",
            ),
        ];
        assert_eq!(
            labels(&run_with(&o, &one, &synthetic(), at())),
            ["SINGLE_DEMAND_SIGNAL"]
        );
        let mut two = one.clone();
        two[1].event_key = "ted:procedure:2".into();
        assert_eq!(
            run_with(&o, &two, &synthetic(), at()).verdict,
            Verdict::Pass
        );
        // A listing that expired before the decision supports nothing.
        let mut stale = cited();
        stale[0].valid_until = Some(t("2026-10-05T12:00:00Z"));
        let v = run_with(&passing(), &stale, &synthetic(), at());
        assert_eq!(labels(&v), ["NO_TESTABLE_EVIDENCE"]);
        assert!(
            v.failures[0].detail.contains("1 expired"),
            "{}",
            v.failures[0].detail
        );
        stale[0].valid_until = Some(t("2026-10-05T12:00:00.001Z"));
        assert_eq!(
            run_with(&passing(), &stale, &synthetic(), at()).verdict,
            Verdict::Pass
        );
    }

    #[test]
    fn below_target_with_bounded_path_holds() {
        // Base 8750.00 × 40 % = 3500.00 ⇒ 2760.00 < 3000.00; upside
        // 11000.00 ⇒ 4400.00 − 740.00 = 3660.00 reaches it.
        let mut o = passing();
        if let RevenueModel::RevenueShare {
            partner_monthly_revenue,
            ..
        } = &mut o.economics.revenue
        {
            partner_monthly_revenue.value =
                Est::range(m("7000.00"), m("8750.00"), m("11000.00")).unwrap();
        }
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Hold);
        assert_eq!(labels(&v), ["BELOW_TARGET_WITH_PATH"]);
        assert_eq!(next_information(&v), ["experiment"]);
        // Without a staged experiment there is no bounded path.
        let mut no_test = o.clone();
        no_test.experiment = None;
        let v = run(&no_test);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(
            labels(&v),
            ["CONTRIBUTION_BELOW_TARGET", "NO_STAGED_STOP_RULE"]
        );
        assert!(has(&v, GateCode::NoStagedStopRule, Verdict::Hold));
    }

    #[test]
    fn first_spend_without_stop_rule_rejected() {
        let mut o = passing();
        if let Some(x) = &mut o.experiment {
            x.stages[1].stop_rule = None;
        }
        let v = run(&o);
        assert_eq!(v.verdict, Verdict::Reject);
        assert_eq!(labels(&v), ["NO_STAGED_STOP_RULE"]);
        assert_eq!(
            v.failures[0].field.as_deref(),
            Some("experiment.stages[1].stop_rule")
        );
        // A stage above the tranche rejects too.
        let mut big = passing();
        if let Some(x) = &mut big.experiment {
            x.stages[1].cash = m("1500.01");
        }
        assert_eq!(labels(&run(&big)), ["TRANCHE_ABOVE_MAX"]);
        if let Some(x) = &mut big.experiment {
            x.stages[1].cash = m("1500.00");
        }
        assert_eq!(run(&big).verdict, Verdict::Pass);
    }

    #[test]
    fn remaining_gates_name_their_field() {
        let p = synthetic();
        // Jurisdiction outside the allow-list, one unknown.
        let mut o = passing();
        o.jurisdictions.customer = "US".into();
        o.jurisdictions.data = "UNKNOWN".into();
        assert_eq!(
            labels(&run(&o)),
            [
                "JURISDICTION_NOT_ALLOWED",
                "UNKNOWN_INPUT:jurisdictions.data"
            ]
        );
        // A legal risk not resolved; an unstated downside.
        let mut o = passing();
        o.risk.risks.push(Risk {
            kind: RiskKind::Licensing,
            probability: Est::range(
                Bps::new(150).unwrap(),
                Bps::new(300).unwrap(),
                Bps::new(900).unwrap(),
            )
            .unwrap(),
            impact: Est::range(m("0.00"), m("200.00"), m("900.00")).unwrap(),
            control: "terms review before the first sale".into(),
            status: RiskStatus::Unresolved,
        });
        o.risk.max_loss.dependency = "UNKNOWN".into();
        let v = run(&o);
        assert_eq!(labels(&v), ["LEGAL_UNRESOLVED", "DOWNSIDE_UNSTATED"]);
        assert_eq!(
            next_information(&v),
            ["risk.max_loss.dependency", "risk.risks[0]"]
        );
        // Mechanisms HOLD and REJECT.
        let mut h = passing();
        h.mechanism = Mechanism::Hold;
        assert_eq!(labels(&run(&h)), ["MECHANISM_HOLD"]);
        h.mechanism = Mechanism::Reject;
        assert_eq!(run(&h).verdict, Verdict::Reject);
        // Payback beyond the profile's max.
        let mut slow = passing();
        slow.economics.initial.setup.value = Est::point(m("29600.01"));
        slow.risk.max_loss.cash = Est::point(m("0.00"));
        let mut wide = p.clone();
        wide.max_cash_exposure = m("40000.00");
        // −40.00 in the ramp month, then 3700.00 a month in months 2..=9:
        // 29560.00 by month 9; 29600.01 is covered in month 10.
        let v = run_with(&slow, &cited(), &wide, at());
        assert_eq!(labels(&v), ["PAYBACK_ABOVE_MAX"]);
        assert!(v.failures[0]
            .detail
            .contains("10 months > max_payback_months 9"));
        // A one-off longer than the profile allows is held.
        let mut long = complete(with_economics(
            "HIGH_TICKET_DELIVERY",
            &economics(
                &[
                    ("variable_cost", "500"),
                    ("fixed_costs_per_month", "\"50.00\""),
                    ("owner_hours_per_month", "0"),
                    ("ramp_months", "0"),
                ],
                "ONE_OFF",
                &[
                    ("contract_value", "\"22000.00\""),
                    ("win_probability", "4500"),
                    ("delivery_weeks", "5"),
                    ("owner_hours_total", "40"),
                    ("collection_loss", "200"),
                ],
                ["\"0.00\"", "\"400.00\"", "\"200.00\"", "\"0.00\""],
            ),
            "",
        ));
        assert_eq!(
            labels(&run(&long)),
            ["DELIVERY_TOO_LONG"],
            "5 weeks > max 4"
        );
        if let RevenueModel::OneOff { delivery_weeks, .. } = &mut long.economics.revenue {
            delivery_weeks.value = Est::point(4);
        }
        assert_eq!(run(&long).verdict, Verdict::Pass);
    }

    #[test]
    fn post_tax_profile_and_unsigned_profile() {
        // Pre-tax figures cannot pass a POST_TAX profile; they can still fail it.
        let mut p = synthetic();
        p.profit_basis = ProfitBasis::PostTax;
        let v = run_with(&passing(), &cited(), &p, at());
        assert_eq!(v.verdict, Verdict::Hold);
        assert_eq!(labels(&v), ["UNKNOWN_INPUT:economics.tax_review"]);
        let below = revenue_share("\"9349.98\"", "4000", "10");
        assert_eq!(
            run_with(&below, &cited(), &p, at()).verdict,
            Verdict::Reject
        );
        // An unsigned profile and an unknown decision time are refused.
        let mut u = synthetic();
        u.signed_by = crate::domain::soe::profile::UNSIGNED.into();
        let e = gates(&passing(), &cited(), &u, at()).unwrap_err();
        assert_eq!(e.code, codes::OPERATOR_PROFILE_UNSIGNED);
        let e = gates(&passing(), &cited(), &synthetic(), Time::Unknown).unwrap_err();
        assert_eq!(e.code, codes::INVALID_TIME);
    }
}
