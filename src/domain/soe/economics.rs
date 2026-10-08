//! Deterministic economics (PRD § 7.1; `docs/soe-2026-10-08.md` § 5): the
//! downside, base and upside of one opportunity from its input ranges, in
//! the profile's currency. Integer arithmetic only (`i128` inside, [`Minor`]
//! out) — no `f64`, no `powi`; every division rounds against the candidate
//! (`value::Flow`). An unknown input makes each figure that needs it
//! [`Metric::Unknown`] naming the input's field — never 0.
//!
//! | Figure ([`ScenarioMetrics`]) | Rule |
//! |---|---|
//! | run-rate month `H` | `profile.max_payback_months`, counted from the decision; the monthly figures are month `H`'s |
//! | `monthly_revenue_collected` | nothing during the ramp (`H ≤ ramp` ⇒ 0) · RECURRING: a customer base from the month after the ramp, `n₁ = leads × conversion`, `nⱼ₊₁ = nⱼ × (1 − churn) + leads × conversion` (thousandths of a customer, floored each month), × price · ACQUISITION: the asset's revenue, churned monthly from month 1 · REVENUE_SHARE: partner revenue × share · each × (1 − collection loss) · ONE_OFF: contract × win × (1 − loss) per occupied month, occ = ⌈weeks × 12 / 52⌉ ≥ 1 |
//! | `monthly_cash_contribution` | revenue − revenue × `variable_cost` − `fixed_costs_per_month` (ONE_OFF: (E − E × variable − fixed × occ) / occ) |
//! | `owner_hours_per_month` · `owner_time_cost` | `owner_hours_per_month` (+ ONE_OFF ⌈`owner_hours_total` / occ⌉) · those hours × `shadow_hourly_rate` |
//! | `time_adjusted_contribution` | cash contribution − owner time cost |
//! | `initial_capital` | acquisition + setup + validation + working capital |
//! | `payback` · `payback_time_adjusted` | the first month the running sum of the month's cash · time-adjusted contribution covers the initial capital; fixed costs and owner hours run from month 1, revenue after the ramp; ONE_OFF stops after ramp + occ; none within [`PAYBACK_SCAN_MONTHS`] ⇒ `NOT_REACHED`; no initial capital ⇒ 0. The cash one is a native ratio (needs no rate) |
//! | `expected_loss` ([`Scenarios`]) | Σ probability × impact per risk: low ends floored .. high ends ceiled — a range, never a point |
//! | currency | native = `economics.currency`; another profile currency converts through `economics.fx` (`<native>/<profile>`; inflows floor, outflows ceil); no such rate ⇒ every money figure needs `economics.fx` |
//! | basis | `PRE_TAX` — v1 has no tax model; the gates hold a `POST_TAX` profile's passing figures |
//! | scenarios | downside = each input's adverse end, upside its favorable end ([`Est::pick`]); a ONE_OFF figure takes the delivery length that is worse (downside) · better (upside) for that figure |
//! | `inputs_sha256` | canonical sha256 of [`ECONOMICS_VERSION`] + `economics` + `risk` + the profile knobs read (`currency`, `shadow_hourly_rate`, `max_payback_months`) |

// Consumers land with the gates, ranking and `tengu soe` (O1 W4–W7).
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::iter;

use serde::{Serialize, Serializer};
use serde_json::json;

use super::opportunity::{EconomicInputs, Opportunity, RevenueModel};
use super::profile::{ContributionBasis, OperatorProfile, ProfitBasis};
use super::risk::RiskAssessment;
use super::value::{
    codes, div_ceil, div_floor, Assumption, Better, Bps, Currency, Est, Flow, FxRate, Minor, Money,
    Scenario, Side, ValueError, BPS_FULL,
};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::UNKNOWN;

/// The formula version: a new formula is a new version (and a new hash).
pub const ECONOMICS_VERSION: u32 = 1;
/// The payback search horizon in months.
pub const PAYBACK_SCAN_MONTHS: u32 = 600;

/// Customers are counted in thousandths.
const MILLI: i128 = 1_000;
const FULL: i128 = BPS_FULL as i128;

/// Input fields as `EconomicInputs::input_states` names them.
pub mod fields {
    pub const LEADS: &str = "economics.revenue.leads_per_month";
    pub const CONVERSION: &str = "economics.revenue.conversion";
    pub const CHURN: &str = "economics.revenue.churn_per_month";
    pub const PRICE: &str = "economics.revenue.price_per_month";
    pub const COLLECTION_LOSS: &str = "economics.revenue.collection_loss";
    pub const CONTRACT_VALUE: &str = "economics.revenue.contract_value";
    pub const WIN_PROBABILITY: &str = "economics.revenue.win_probability";
    pub const DELIVERY_WEEKS: &str = "economics.revenue.delivery_weeks";
    pub const OWNER_HOURS_TOTAL: &str = "economics.revenue.owner_hours_total";
    pub const ASSET_REVENUE: &str = "economics.revenue.asset_monthly_revenue";
    pub const PARTNER_REVENUE: &str = "economics.revenue.partner_monthly_revenue";
    pub const SHARE: &str = "economics.revenue.share";
    pub const VARIABLE_COST: &str = "economics.variable_cost";
    pub const FIXED_COSTS: &str = "economics.fixed_costs_per_month";
    pub const OWNER_HOURS: &str = "economics.owner_hours_per_month";
    pub const RAMP: &str = "economics.ramp_months";
    pub const ACQUISITION: &str = "economics.initial.acquisition";
    pub const SETUP: &str = "economics.initial.setup";
    pub const VALIDATION: &str = "economics.initial.validation";
    pub const WORKING_CAPITAL: &str = "economics.initial.working_capital";
    pub const FX: &str = "economics.fx";
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// A computed figure, or the input fields it lacks (sorted) — never 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metric<T> {
    Known(T),
    Unknown { fields: Vec<String> },
}

impl<T> Metric<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Metric::Known(v) => Some(v),
            Metric::Unknown { .. } => None,
        }
    }

    /// The lacking fields (empty when known).
    pub fn fields(&self) -> &[String] {
        match self {
            Metric::Known(_) => &[],
            Metric::Unknown { fields } => fields,
        }
    }
}

impl<T> From<Need<T>> for Metric<T> {
    fn from(n: Need<T>) -> Self {
        match n {
            Ok(v) => Metric::Known(v),
            Err(lacking) => Metric::Unknown {
                fields: lacking.into_iter().map(str::to_string).collect(),
            },
        }
    }
}

/// `Known` → the value; `Unknown` → `"UNKNOWN: needs <field>, …"`.
impl<T: Serialize> Serialize for Metric<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Metric::Known(v) => v.serialize(s),
            Metric::Unknown { fields } => {
                s.collect_str(&format_args!("{UNKNOWN}: needs {}", fields.join(", ")))
            }
        }
    }
}

/// Months to payback; `NotReached` orders after every month count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Payback {
    Months(u32),
    NotReached,
}

impl Serialize for Payback {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Payback::Months(n) => s.serialize_u32(*n),
            Payback::NotReached => s.serialize_str("NOT_REACHED"),
        }
    }
}

/// An expected-loss range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LossRange {
    pub low: Minor,
    pub high: Minor,
}

/// One scenario's figures (module table), money in the profile currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScenarioMetrics {
    pub scenario: Scenario,
    pub monthly_revenue_collected: Metric<Minor>,
    pub monthly_cash_contribution: Metric<Minor>,
    pub owner_hours_per_month: Metric<u32>,
    pub owner_time_cost: Metric<Minor>,
    pub time_adjusted_contribution: Metric<Minor>,
    pub initial_capital: Metric<Minor>,
    pub payback: Metric<Payback>,
    pub payback_time_adjusted: Metric<Payback>,
    /// ONE_OFF only: the months the engagement occupies at the scenario's
    /// delivery end (the longest for the downside).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occupied_months: Option<Metric<u32>>,
}

impl ScenarioMetrics {
    /// The contribution the profile's rule compares.
    pub fn contribution(&self, basis: ContributionBasis) -> &Metric<Minor> {
        match basis {
            ContributionBasis::TimeAdjusted => &self.time_adjusted_contribution,
            ContributionBasis::Cash => &self.monthly_cash_contribution,
        }
    }
}

/// The three scenarios of one opportunity (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scenarios {
    pub economics_version: u32,
    pub inputs_sha256: String,
    /// The profile's currency: every money figure is in it.
    pub currency: Currency,
    /// `economics.currency`.
    pub native_currency: Currency,
    pub basis: ProfitBasis,
    pub run_rate_month: u32,
    pub downside: ScenarioMetrics,
    pub base: ScenarioMetrics,
    pub upside: ScenarioMetrics,
    /// In the profile currency (low floored, high ceiled).
    pub expected_loss: Metric<LossRange>,
}

impl Scenarios {
    pub fn get(&self, s: Scenario) -> &ScenarioMetrics {
        match s {
            Scenario::Downside => &self.downside,
            Scenario::Base => &self.base,
            Scenario::Upside => &self.upside,
        }
    }
}

// ---------------------------------------------------------------------------
// Unknown-safe plumbing
// ---------------------------------------------------------------------------

/// A value, or the input fields it lacks.
type Need<T> = Result<T, BTreeSet<&'static str>>;

/// The scenario's end of an input, or its field when unknown.
fn pick<T: Copy>(a: &Assumption<T>, s: Scenario, better: Better, field: &'static str) -> Need<T> {
    a.value
        .pick(s, better)
        .copied()
        .ok_or_else(|| BTreeSet::from([field]))
}

fn bps(n: Need<Bps>) -> Need<i128> {
    n.map(|b| b.get() as i128)
}

fn amount(n: Need<Minor>) -> Need<i128> {
    n.map(|m| m.0 as i128)
}

/// Collects what a computation lacks. Each closure below gets all its
/// inputs in one tuple before it uses any, so an unknown figure names every
/// missing input.
#[derive(Default)]
struct Ctx {
    lacking: BTreeSet<&'static str>,
}

impl Ctx {
    fn get<T: Copy>(&mut self, n: &Need<T>) -> Option<T> {
        self.get_ref(n).copied()
    }

    fn get_ref<'a, T>(&mut self, n: &'a Need<T>) -> Option<&'a T> {
        match n {
            Ok(v) => Some(v),
            Err(f) => {
                self.lacking.extend(f.iter().copied());
                None
            }
        }
    }
}

/// Runs `f`: `Ok(None)` (an input lacking) becomes the lacking fields.
fn need<T>(
    f: impl FnOnce(&mut Ctx) -> Result<Option<T>, ValueError>,
) -> Result<Need<T>, ValueError> {
    let mut c = Ctx::default();
    let v = f(&mut c)?;
    Ok(match v {
        Some(v) if c.lacking.is_empty() => Ok(v),
        _ => Err(c.lacking),
    })
}

/// A computed `i128` as a [`Metric`] of [`Minor`].
fn as_metric(n: &Need<i128>) -> Result<Metric<Minor>, ValueError> {
    Ok(need(|c| c.get(n).map(minor).transpose())?.into())
}

fn overflow(what: &str) -> ValueError {
    ValueError::new(codes::OVERFLOW, format!("economics: {what} overflows"))
}

fn mul(a: i128, b: i128) -> Result<i128, ValueError> {
    a.checked_mul(b).ok_or_else(|| overflow("a product"))
}

fn add(a: i128, b: i128) -> Result<i128, ValueError> {
    a.checked_add(b).ok_or_else(|| overflow("a sum"))
}

fn minor(v: i128) -> Result<Minor, ValueError> {
    i64::try_from(v)
        .map(Minor)
        .map_err(|_| overflow("an amount"))
}

/// How a native amount reaches the profile currency.
#[derive(Clone, Copy)]
struct Fx<'a> {
    native: Currency,
    rate: Option<&'a FxRate>,
}

impl Fx<'_> {
    fn convert(self, native: i128, flow: Flow) -> Result<i128, ValueError> {
        match self.rate {
            None => Ok(native),
            Some(r) => Ok(r
                .convert(Money::new(minor(native)?, self.native), flow)?
                .minor
                .0 as i128),
        }
    }
}

/// The rate `economics.fx` gives for `native → target`, or its field.
fn fx_for(e: &EconomicInputs, target: Currency) -> Need<Fx<'_>> {
    let native = e.currency;
    if native == target {
        return Ok(Fx { native, rate: None });
    }
    match &e.fx {
        Some(r) if r.pair.base == native && r.pair.quote == target => Ok(Fx {
            native,
            rate: Some(r),
        }),
        _ => Err(BTreeSet::from([fields::FX])),
    }
}

/// `native` (in `e.currency`) in `target`, rounded for `flow`; `None` when
/// `economics.fx` gives no `<native>/<target>` rate.
pub fn to_currency(
    e: &EconomicInputs,
    target: Currency,
    native: Minor,
    flow: Flow,
) -> Result<Option<Minor>, ValueError> {
    match fx_for(e, target) {
        Ok(fx) => Ok(Some(minor(fx.convert(native.0 as i128, flow)?)?)),
        Err(_) => Ok(None),
    }
}

/// The profile knobs economics reads, and the rate.
struct Knobs<'a> {
    /// `shadow_hourly_rate`, profile currency.
    rate: i128,
    /// The run-rate month `H`.
    horizon: u32,
    fx: Need<Fx<'a>>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// The three scenarios of `opp` under `profile` (module table). `Err` only
/// on arithmetic overflow; an unknown input is a [`Metric::Unknown`].
pub fn scenarios(opp: &Opportunity, profile: &OperatorProfile) -> Result<Scenarios, ValueError> {
    let e = &opp.economics;
    let k = Knobs {
        rate: profile.shadow_hourly_rate.0 as i128,
        horizon: profile.max_payback_months.max(1),
        fx: fx_for(e, profile.currency),
    };
    let native_loss = match expected_loss(&opp.risk)? {
        Metric::Known(r) => Ok(r),
        Metric::Unknown { fields } => Err(fields),
    };
    let loss = match native_loss {
        Ok(r) => need(|c| {
            let Some(fx) = c.get(&k.fx) else {
                return Ok(None);
            };
            Ok(Some(LossRange {
                low: minor(fx.convert(r.low.0 as i128, Flow::Inflow)?)?,
                high: minor(fx.convert(r.high.0 as i128, Flow::Outflow)?)?,
            }))
        })?
        .into(),
        Err(fields) => Metric::Unknown { fields },
    };
    Ok(Scenarios {
        economics_version: ECONOMICS_VERSION,
        inputs_sha256: inputs_sha256(opp, profile)?,
        currency: profile.currency,
        native_currency: e.currency,
        basis: ProfitBasis::PreTax,
        run_rate_month: k.horizon,
        downside: scenario(e, &k, Scenario::Downside)?,
        base: scenario(e, &k, Scenario::Base)?,
        upside: scenario(e, &k, Scenario::Upside)?,
        expected_loss: loss,
    })
}

/// Σ probability × impact per risk, native currency: low ends floored, high
/// ends ceiled. Unknown when any risk's probability or impact is.
pub fn expected_loss(risk: &RiskAssessment) -> Result<Metric<LossRange>, ValueError> {
    let mut lacking = Vec::new();
    let (mut low, mut high) = (0i128, 0i128);
    for (i, r) in risk.risks.iter().enumerate() {
        match (&r.probability, &r.impact) {
            (
                Est::Range {
                    low: p_lo,
                    high: p_hi,
                    ..
                },
                Est::Range {
                    low: i_lo,
                    high: i_hi,
                    ..
                },
            ) => {
                low = add(
                    low,
                    div_floor(mul(p_lo.get() as i128, i_lo.0 as i128)?, FULL),
                )?;
                high = add(
                    high,
                    div_ceil(mul(p_hi.get() as i128, i_hi.0 as i128)?, FULL),
                )?;
            }
            (p, imp) => {
                if !p.is_known() {
                    lacking.push(format!("risk.risks[{i}].probability"));
                }
                if !imp.is_known() {
                    lacking.push(format!("risk.risks[{i}].impact"));
                }
            }
        }
    }
    if !lacking.is_empty() {
        return Ok(Metric::Unknown { fields: lacking });
    }
    Ok(Metric::Known(LossRange {
        low: minor(low)?,
        high: minor(high)?,
    }))
}

/// Module table: `inputs_sha256`.
pub fn inputs_sha256(opp: &Opportunity, profile: &OperatorProfile) -> Result<String, ValueError> {
    let as_json = |what: &str, v: Result<serde_json::Value, serde_json::Error>| {
        v.map_err(|e| ValueError::new(codes::INVALID_RECORD, format!("{what}: {e}")))
    };
    Ok(canonical_sha256(&json!({
        "economics_version": ECONOMICS_VERSION,
        "economics": as_json("economics", serde_json::to_value(&opp.economics))?,
        "risk": as_json("risk", serde_json::to_value(&opp.risk))?,
        "profile": {
            "currency": profile.currency,
            "shadow_hourly_rate": profile.shadow_hourly_rate,
            "max_payback_months": profile.max_payback_months,
        },
    })))
}

// ---------------------------------------------------------------------------
// One scenario
// ---------------------------------------------------------------------------

/// The inputs every model shares, picked for one scenario (native money).
struct Common {
    variable: Need<i128>,
    fixed: Need<i128>,
    hours: Need<u32>,
    ramp: Need<u32>,
    initial: Need<i128>,
}

impl Common {
    fn pick(e: &EconomicInputs, s: Scenario) -> Result<Common, ValueError> {
        let lower = Better::Lower;
        let parts = [
            amount(pick(&e.initial.acquisition, s, lower, fields::ACQUISITION)),
            amount(pick(&e.initial.setup, s, lower, fields::SETUP)),
            amount(pick(&e.initial.validation, s, lower, fields::VALIDATION)),
            amount(pick(
                &e.initial.working_capital,
                s,
                lower,
                fields::WORKING_CAPITAL,
            )),
        ];
        let initial = need(|c| {
            let got: Vec<Option<i128>> = parts.iter().map(|p| c.get(p)).collect();
            let mut sum = 0i128;
            for part in got {
                let Some(part) = part else {
                    return Ok(None);
                };
                sum = add(sum, part)?;
            }
            Ok(Some(sum))
        })?;
        Ok(Common {
            variable: bps(pick(&e.variable_cost, s, lower, fields::VARIABLE_COST)),
            fixed: amount(pick(
                &e.fixed_costs_per_month,
                s,
                lower,
                fields::FIXED_COSTS,
            )),
            hours: pick(&e.owner_hours_per_month, s, lower, fields::OWNER_HOURS),
            ramp: pick(&e.ramp_months, s, lower, fields::RAMP),
            initial,
        })
    }
}

fn scenario(e: &EconomicInputs, k: &Knobs, s: Scenario) -> Result<ScenarioMetrics, ValueError> {
    let common = Common::pick(e, s)?;
    let (higher, lower) = (Better::Higher, Better::Lower);
    let stream = match &e.revenue {
        RevenueModel::OneOff {
            contract_value,
            win_probability,
            delivery_weeks,
            owner_hours_total,
            collection_loss,
        } => {
            let deal = OneOffDeal {
                contract: amount(pick(contract_value, s, higher, fields::CONTRACT_VALUE)),
                win: bps(pick(win_probability, s, higher, fields::WIN_PROBABILITY)),
                loss: bps(pick(collection_loss, s, lower, fields::COLLECTION_LOSS)),
                hours_total: pick(owner_hours_total, s, lower, fields::OWNER_HOURS_TOTAL),
            };
            let weeks = |w: Option<&u32>| -> Need<u32> {
                w.copied()
                    .ok_or_else(|| BTreeSet::from([fields::DELIVERY_WEEKS]))
            };
            let at = |w: Need<u32>| one_off(&deal, &common, k, w, s);
            return match s {
                Scenario::Base => at(weeks(delivery_weeks.value.base())),
                Scenario::Downside | Scenario::Upside => {
                    let est = &delivery_weeks.value;
                    let short = at(weeks(est.end(Side::Favorable, lower)))?;
                    let long = at(weeks(est.end(Side::Adverse, lower)))?;
                    Ok(combine(short, long, s))
                }
            };
        }
        RevenueModel::Recurring {
            leads_per_month,
            conversion,
            churn_per_month,
            price_per_month,
            collection_loss,
        } => {
            let leads = pick(leads_per_month, s, higher, fields::LEADS).map(i128::from);
            let conv = bps(pick(conversion, s, higher, fields::CONVERSION));
            let churn = bps(pick(churn_per_month, s, lower, fields::CHURN));
            let price = amount(pick(price_per_month, s, higher, fields::PRICE));
            let loss = bps(pick(collection_loss, s, lower, fields::COLLECTION_LOSS));
            need(|c| {
                let (Some(leads), Some(conv), Some(churn), Some(price), Some(loss)) = (
                    c.get(&leads),
                    c.get(&conv),
                    c.get(&churn),
                    c.get(&price),
                    c.get(&loss),
                ) else {
                    return Ok(None);
                };
                Ok(Some(Stream::Recurring {
                    won: div_floor(mul(mul(leads, conv)?, MILLI)?, FULL),
                    keep: FULL - churn,
                    price,
                    collect: FULL - loss,
                }))
            })?
        }
        RevenueModel::Acquisition {
            asset_monthly_revenue,
            churn_per_month,
            collection_loss,
        } => {
            let first = amount(pick(
                asset_monthly_revenue,
                s,
                higher,
                fields::ASSET_REVENUE,
            ));
            let churn = bps(pick(churn_per_month, s, lower, fields::CHURN));
            let loss = bps(pick(collection_loss, s, lower, fields::COLLECTION_LOSS));
            need(|c| {
                let (Some(first), Some(churn), Some(loss)) =
                    (c.get(&first), c.get(&churn), c.get(&loss))
                else {
                    return Ok(None);
                };
                Ok(Some(Stream::Decay {
                    first,
                    keep: FULL - churn,
                    collect: FULL - loss,
                }))
            })?
        }
        RevenueModel::RevenueShare {
            partner_monthly_revenue,
            share,
            collection_loss,
        } => {
            let partner = amount(pick(
                partner_monthly_revenue,
                s,
                higher,
                fields::PARTNER_REVENUE,
            ));
            let share = bps(pick(share, s, higher, fields::SHARE));
            let loss = bps(pick(collection_loss, s, lower, fields::COLLECTION_LOSS));
            need(|c| {
                let (Some(partner), Some(share), Some(loss)) =
                    (c.get(&partner), c.get(&share), c.get(&loss))
                else {
                    return Ok(None);
                };
                Ok(Some(Stream::Flat {
                    collected: div_floor(mul(mul(partner, share)?, FULL - loss)?, FULL * FULL),
                }))
            })?
        }
    };
    run_rate(&stream, &common, k, s)
}

/// How revenue arrives month by month (module table).
enum Stream {
    /// Thousandths of a customer won a month, retention (bps), price,
    /// collected share (bps).
    Recurring {
        won: i128,
        keep: i128,
        price: i128,
        collect: i128,
    },
    /// The asset's month-1 revenue, retention, collected share.
    Decay {
        first: i128,
        keep: i128,
        collect: i128,
    },
    /// Collected revenue a month.
    Flat { collected: i128 },
}

impl Stream {
    /// Native collected revenue of months 1..=`months` (module table).
    fn revenue(&self, ramp: u32, months: u32) -> Result<Vec<i128>, ValueError> {
        let mut out = Vec::with_capacity(months as usize);
        // Recurring: customers (thousandths); Decay: the asset's revenue.
        let mut base = match self {
            Stream::Decay { first, .. } => *first,
            _ => 0,
        };
        for month in 1..=months {
            let active = month > ramp;
            let revenue = match *self {
                Stream::Recurring {
                    won,
                    keep,
                    price,
                    collect,
                } if active => {
                    base = add(div_floor(mul(base, keep)?, FULL), won)?;
                    div_floor(mul(mul(base, price)?, collect)?, MILLI * FULL)
                }
                Stream::Decay { keep, collect, .. } => {
                    if month > 1 {
                        base = div_floor(mul(base, keep)?, FULL);
                    }
                    if active {
                        div_floor(mul(base, collect)?, FULL)
                    } else {
                        0
                    }
                }
                Stream::Flat { collected } if active => collected,
                _ => 0,
            };
            out.push(revenue);
        }
        Ok(out)
    }
}

/// `revenue − ⌈revenue × variable⌉ − fixed`, native.
fn cash_of(revenue: i128, variable: i128, fixed: i128) -> Result<i128, ValueError> {
    Ok(revenue - div_ceil(mul(revenue, variable)?, FULL) - fixed)
}

/// The first month the running sum covers `initial` (module table).
fn payback(
    monthly: impl IntoIterator<Item = Result<i128, ValueError>>,
    initial: i128,
) -> Result<Payback, ValueError> {
    if initial <= 0 {
        return Ok(Payback::Months(0));
    }
    let mut sum = 0i128;
    for (i, c) in monthly.into_iter().enumerate() {
        sum = add(sum, c?)?;
        if sum >= initial {
            return Ok(Payback::Months(i as u32 + 1));
        }
    }
    Ok(Payback::NotReached)
}

/// RECURRING · ACQUISITION · REVENUE_SHARE: figures at the run-rate month.
fn run_rate(
    stream: &Need<Stream>,
    common: &Common,
    k: &Knobs,
    s: Scenario,
) -> Result<ScenarioMetrics, ValueError> {
    let months = k.horizon.max(PAYBACK_SCAN_MONTHS);
    let h = k.horizon as usize - 1;
    let revenue: Need<Vec<i128>> = need(|c| {
        let (Some(stream), Some(ramp)) = (c.get_ref(stream), c.get(&common.ramp)) else {
            return Ok(None);
        };
        stream.revenue(ramp, months).map(Some)
    })?;
    // Native cash contribution, months 1..=months.
    let cash: Need<Vec<i128>> = need(|c| {
        let (Some(revenue), Some(v), Some(f)) = (
            c.get_ref(&revenue),
            c.get(&common.variable),
            c.get(&common.fixed),
        ) else {
            return Ok(None);
        };
        revenue
            .iter()
            .map(|r| cash_of(*r, v, f))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    })?;
    let owner_cost: Need<i128> = need(|c| {
        let Some(hours) = c.get(&common.hours) else {
            return Ok(None);
        };
        mul(hours as i128, k.rate).map(Some)
    })?;
    let revenue_h: Need<i128> = need(|c| {
        let (Some(revenue), Some(fx)) = (c.get_ref(&revenue), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(revenue[h], Flow::Inflow).map(Some)
    })?;
    let cash_h: Need<i128> = need(|c| {
        let (Some(cash), Some(fx)) = (c.get_ref(&cash), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(cash[h], Flow::Inflow).map(Some)
    })?;
    let time_adjusted: Need<i128> = need(|c| {
        let (Some(cash), Some(cost)) = (c.get(&cash_h), c.get(&owner_cost)) else {
            return Ok(None);
        };
        Ok(Some(cash - cost))
    })?;
    let initial_capital: Need<i128> = need(|c| {
        let (Some(initial), Some(fx)) = (c.get(&common.initial), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(initial, Flow::Outflow).map(Some)
    })?;
    let cash_payback = need(|c| {
        let (Some(cash), Some(initial)) = (c.get_ref(&cash), c.get(&common.initial)) else {
            return Ok(None);
        };
        payback(cash.iter().map(|v| Ok(*v)), initial).map(Some)
    })?;
    let ta_payback = need(|c| {
        let (Some(cash), Some(cost), Some(initial), Some(fx)) = (
            c.get_ref(&cash),
            c.get(&owner_cost),
            c.get(&initial_capital),
            c.get(&k.fx),
        ) else {
            return Ok(None);
        };
        let monthly = cash
            .iter()
            .map(|v| Ok(fx.convert(*v, Flow::Inflow)? - cost));
        payback(monthly, initial).map(Some)
    })?;

    Ok(ScenarioMetrics {
        scenario: s,
        monthly_revenue_collected: as_metric(&revenue_h)?,
        monthly_cash_contribution: as_metric(&cash_h)?,
        owner_hours_per_month: common.hours.clone().into(),
        owner_time_cost: as_metric(&owner_cost)?,
        time_adjusted_contribution: as_metric(&time_adjusted)?,
        initial_capital: as_metric(&initial_capital)?,
        payback: cash_payback.into(),
        payback_time_adjusted: ta_payback.into(),
        occupied_months: None,
    })
}

/// A ONE_OFF deal's inputs, picked for one scenario (native money).
struct OneOffDeal {
    contract: Need<i128>,
    win: Need<i128>,
    loss: Need<i128>,
    hours_total: Need<u32>,
}

/// ⌈weeks × 12 / 52⌉, at least 1.
fn occupied(weeks: u32) -> u32 {
    (div_ceil(weeks as i128 * 12, 52) as u32).max(1)
}

/// Months 1..=`ramp` at `during_ramp`, then `occ` months at `during`; the
/// engagement then ends.
fn engagement(
    ramp: u32,
    occ: i128,
    during_ramp: i128,
    during: i128,
) -> impl Iterator<Item = Result<i128, ValueError>> {
    iter::repeat(during_ramp)
        .take(ramp as usize)
        .chain(iter::repeat(during).take(occ as usize))
        .map(Ok)
}

/// ONE_OFF figures at one delivery length (module table).
fn one_off(
    d: &OneOffDeal,
    common: &Common,
    k: &Knobs,
    weeks: Need<u32>,
    s: Scenario,
) -> Result<ScenarioMetrics, ValueError> {
    let occ: Need<i128> = weeks.map(|w| occupied(w) as i128);
    // Expected collected revenue of the whole engagement, native.
    let expected: Need<i128> = need(|c| {
        let (Some(contract), Some(win), Some(loss)) =
            (c.get(&d.contract), c.get(&d.win), c.get(&d.loss))
        else {
            return Ok(None);
        };
        Ok(Some(div_floor(
            mul(mul(contract, win)?, FULL - loss)?,
            FULL * FULL,
        )))
    })?;
    // Native cash contribution per occupied month.
    let cash_pm: Need<i128> = need(|c| {
        let (Some(e), Some(v), Some(f), Some(occ)) = (
            c.get(&expected),
            c.get(&common.variable),
            c.get(&common.fixed),
            c.get(&occ),
        ) else {
            return Ok(None);
        };
        let net = e - div_ceil(mul(e, v)?, FULL) - mul(f, occ)?;
        Ok(Some(div_floor(net, occ)))
    })?;
    let hours_pm: Need<u32> = need(|c| {
        let (Some(h), Some(t), Some(occ)) =
            (c.get(&common.hours), c.get(&d.hours_total), c.get(&occ))
        else {
            return Ok(None);
        };
        u32::try_from(h as i128 + div_ceil(t as i128, occ))
            .map(Some)
            .map_err(|_| overflow("owner hours"))
    })?;
    // Profile currency: ongoing hours + the delivery hours spread over occ.
    let owner_cost: Need<i128> = need(|c| {
        let (Some(h), Some(t), Some(occ)) =
            (c.get(&common.hours), c.get(&d.hours_total), c.get(&occ))
        else {
            return Ok(None);
        };
        Ok(Some(add(
            mul(h as i128, k.rate)?,
            div_ceil(mul(t as i128, k.rate)?, occ),
        )?))
    })?;
    let revenue_pm: Need<i128> = need(|c| {
        let (Some(e), Some(occ), Some(fx)) = (c.get(&expected), c.get(&occ), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(div_floor(e, occ), Flow::Inflow).map(Some)
    })?;
    let cash: Need<i128> = need(|c| {
        let (Some(cash), Some(fx)) = (c.get(&cash_pm), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(cash, Flow::Inflow).map(Some)
    })?;
    let time_adjusted: Need<i128> = need(|c| {
        let (Some(cash), Some(cost)) = (c.get(&cash), c.get(&owner_cost)) else {
            return Ok(None);
        };
        Ok(Some(cash - cost))
    })?;
    let initial_capital: Need<i128> = need(|c| {
        let (Some(initial), Some(fx)) = (c.get(&common.initial), c.get(&k.fx)) else {
            return Ok(None);
        };
        fx.convert(initial, Flow::Outflow).map(Some)
    })?;
    let cash_payback = need(|c| {
        let (Some(ramp), Some(f), Some(occ), Some(cash), Some(initial)) = (
            c.get(&common.ramp),
            c.get(&common.fixed),
            c.get(&occ),
            c.get(&cash_pm),
            c.get(&common.initial),
        ) else {
            return Ok(None);
        };
        payback(engagement(ramp, occ, -f, cash), initial).map(Some)
    })?;
    let ta_payback = need(|c| {
        let (Some(ramp), Some(f), Some(h), Some(occ), Some(ta), Some(initial), Some(fx)) = (
            c.get(&common.ramp),
            c.get(&common.fixed),
            c.get(&common.hours),
            c.get(&occ),
            c.get(&time_adjusted),
            c.get(&initial_capital),
            c.get(&k.fx),
        ) else {
            return Ok(None);
        };
        // During the ramp: the fixed costs and the ongoing hours.
        let ramp_month = fx.convert(-f, Flow::Inflow)? - mul(h as i128, k.rate)?;
        payback(engagement(ramp, occ, ramp_month, ta), initial).map(Some)
    })?;

    Ok(ScenarioMetrics {
        scenario: s,
        monthly_revenue_collected: as_metric(&revenue_pm)?,
        monthly_cash_contribution: as_metric(&cash)?,
        owner_hours_per_month: hours_pm.into(),
        owner_time_cost: as_metric(&owner_cost)?,
        time_adjusted_contribution: as_metric(&time_adjusted)?,
        initial_capital: as_metric(&initial_capital)?,
        payback: cash_payback.into(),
        payback_time_adjusted: ta_payback.into(),
        occupied_months: Some(occ.map(|o| o as u32).into()),
    })
}

/// Downside: each figure's worse of the two delivery ends; upside: its
/// better (module table). Both ends share their unknown fields.
fn combine(short: ScenarioMetrics, long: ScenarioMetrics, s: Scenario) -> ScenarioMetrics {
    let worst = s == Scenario::Downside;
    fn both<T: Copy + Ord>(a: Metric<T>, b: Metric<T>, low: bool) -> Metric<T> {
        match (&a, &b) {
            (Metric::Known(x), Metric::Known(y)) => {
                Metric::Known(if low { *x.min(y) } else { *x.max(y) })
            }
            _ => a,
        }
    }
    // Income: the downside takes the lower; costs, hours and payback: the higher.
    let (income, cost) = (worst, !worst);
    ScenarioMetrics {
        scenario: s,
        monthly_revenue_collected: both(
            short.monthly_revenue_collected,
            long.monthly_revenue_collected,
            income,
        ),
        monthly_cash_contribution: both(
            short.monthly_cash_contribution,
            long.monthly_cash_contribution,
            income,
        ),
        owner_hours_per_month: both(
            short.owner_hours_per_month,
            long.owner_hours_per_month,
            cost,
        ),
        owner_time_cost: both(short.owner_time_cost, long.owner_time_cost, cost),
        time_adjusted_contribution: both(
            short.time_adjusted_contribution,
            long.time_adjusted_contribution,
            income,
        ),
        initial_capital: both(short.initial_capital, long.initial_capital, cost),
        payback: both(short.payback, long.payback, cost),
        payback_time_adjusted: both(
            short.payback_time_adjusted,
            long.payback_time_adjusted,
            cost,
        ),
        occupied_months: if worst {
            long.occupied_months
        } else {
            short.occupied_months
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::opportunity::tests::{recurring, RECURRING};
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::record::from_toml;
    use crate::domain::soe::risk::{Risk, RiskKind, RiskStatus};

    pub(crate) fn m(s: &str) -> Minor {
        s.parse().unwrap()
    }

    pub(crate) fn known<T: Copy + std::fmt::Debug>(x: &Metric<T>) -> T {
        *x.known().unwrap_or_else(|| panic!("unknown: {x:?}"))
    }

    /// A deal review with every block verified (example locators).
    pub(crate) const VERIFIED_DEAL: &str = r#"
[deal]
red_flags = []
identity = { status = "VERIFIED", evidence = ["url:https://example.org/registry"] }
ownership = { status = "VERIFIED", evidence = ["url:https://example.org/registry"] }
revenue_proof = { status = "VERIFIED", evidence = ["url:https://example.org/payouts"] }
costs = { status = "VERIFIED", evidence = ["url:https://example.org/invoices"] }
technical = { status = "VERIFIED", evidence = ["url:https://example.org/audit"] }
distribution = { status = "VERIFIED", evidence = ["url:https://example.org/traffic"] }
legal = { status = "VERIFIED", evidence = ["url:https://example.org/terms"] }
transition = { status = "VERIFIED", evidence = ["url:https://example.org/handover"] }
"#;

    /// `RECURRING` with `mechanism`, its `[economics]` block replaced by
    /// `economics` and `extra` tables appended; parsed and validated.
    pub(crate) fn with_economics(mechanism: &str, economics: &str, extra: &str) -> Opportunity {
        let head = RECURRING.split("[economics]").next().unwrap();
        let tail = RECURRING.split("[risk]").nth(1).unwrap();
        let text = format!("{head}{economics}\n[risk]{tail}{extra}")
            .replace(
                "mechanism = \"AUTOMATE\"",
                &format!("mechanism = \"{mechanism}\""),
            )
            .replace(
                "alternatives = [\"PRODUCTIZE\", \"HOLD\"]",
                "alternatives = [\"REJECT\"]",
            );
        from_toml(&text).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// A point estimate (`low = base = high`) with an example locator.
    fn pt(v: &str) -> String {
        format!(
            "{{ value = {{ low = {v}, base = {v}, high = {v} }}, evidence = [\"url:https://example.org/e\"], as_of = \"2026-10-01\" }}"
        )
    }

    fn rows(rows: &[(&str, &str)]) -> String {
        rows.iter()
            .map(|(k, v)| format!("{k} = {}\n", pt(v)))
            .collect()
    }

    /// An `[economics]` block of point estimates in EUR: the shared inputs,
    /// the revenue model (`kind` + rows) and the four initial amounts.
    pub(crate) fn economics(
        common: &[(&str, &str)],
        kind: &str,
        revenue: &[(&str, &str)],
        initial: [&str; 4],
    ) -> String {
        let initial: Vec<(&str, &str)> = ["acquisition", "setup", "validation", "working_capital"]
            .into_iter()
            .zip(initial)
            .collect();
        format!(
            "[economics]\ncurrency = \"EUR\"\nrevenue_quality = \"RECURRING\"\ntax_review = \"UNKNOWN\"\n{}\n[economics.revenue]\nkind = \"{kind}\"\n{}\n[economics.initial]\n{}",
            rows(common),
            rows(revenue),
            rows(&initial)
        )
    }

    fn base(opp: &Opportunity) -> ScenarioMetrics {
        scenarios(opp, &synthetic()).unwrap().base
    }

    /// The recurring example with a price range and setup 400 / 650 / 900.
    pub(crate) fn priced() -> Opportunity {
        let mut o = recurring();
        if let RevenueModel::Recurring {
            price_per_month, ..
        } = &mut o.economics.revenue
        {
            price_per_month.value = Est::range(m("400.00"), m("650.00"), m("900.00")).unwrap();
        }
        o.economics.initial.setup.value =
            Est::range(m("400.00"), m("650.00"), m("900.00")).unwrap();
        o
    }

    #[test]
    fn golden_recurring_automation() {
        // 10 leads × 20 % = 2 customers (2000 thousandths) won a month from
        // month 3 (ramp 2), churn 5 %: n = 2000, 3900, 5705, 7419, 9048,
        // 10595, 12065 in months 3..=9 (each n × 0.95 floored, + 2000).
        // Month 9 (H = max_payback_months = 9): 12.065 × 150.00 = 1809.75;
        // variable 9 % = ⌈162.8775⌉ = 162.88; − fixed 40.00 ⇒ 1606.87.
        // Owner 9 h × 70.00 = 630.00 ⇒ time-adjusted 976.87.
        // Initial 900.00 + 300.00 = 1200.00. Cash by month: −40.00, −40.00,
        // 233.00, 492.35, 738.73 ⇒ the running sum reaches 1384.08 in month 5.
        // Time-adjusted (− 630.00 a month): −670.00, −670.00, −397.00,
        // −137.65, 108.73, 342.69, 565.05, 776.21, 976.87, 1167.42 ⇒ the
        // running sum reaches 2062.32 in month 10 (894.90 in month 9).
        let opp = with_economics(
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
                    ("price_per_month", "\"150.00\""),
                    ("collection_loss", "0"),
                ],
                ["\"0.00\"", "\"900.00\"", "\"300.00\"", "\"0.00\""],
            ),
            "",
        );
        let sc = scenarios(&opp, &synthetic()).unwrap();
        assert_eq!(sc.run_rate_month, 9);
        let b = &sc.base;
        assert_eq!(known(&b.monthly_revenue_collected), m("1809.75"));
        assert_eq!(known(&b.monthly_cash_contribution), m("1606.87"));
        assert_eq!(known(&b.owner_hours_per_month), 9);
        assert_eq!(known(&b.owner_time_cost), m("630.00"));
        assert_eq!(known(&b.time_adjusted_contribution), m("976.87"));
        assert_eq!(known(&b.initial_capital), m("1200.00"));
        assert_eq!(known(&b.payback), Payback::Months(5));
        assert_eq!(known(&b.payback_time_adjusted), Payback::Months(10));
        assert_eq!(b.occupied_months, None);
        // Point inputs: the three scenarios agree.
        assert_eq!(
            sc.downside,
            ScenarioMetrics {
                scenario: Scenario::Downside,
                ..b.clone()
            }
        );
        assert_eq!(
            sc.upside,
            ScenarioMetrics {
                scenario: Scenario::Upside,
                ..b.clone()
            }
        );
        assert_eq!(sc.basis, ProfitBasis::PreTax);
        assert_eq!(sc.economics_version, ECONOMICS_VERSION);
    }

    #[test]
    fn golden_one_off_integration_per_operator_month() {
        // 7 weeks ⇒ occ = ⌈84 / 52⌉ = 2 months. E = 22000.00 × 45 % × 98 %
        // = 9702.00 (4851.00 a month); variable 5 % = 485.10; fixed 50.00 × 2
        // ⇒ cash per operator month (9702.00 − 485.10 − 2 × 50.00) / 2 = 4558.45.
        // Owner 90 h total over 2 months = 45 h × 70.00 = 3150.00 a month ⇒
        // time-adjusted 1408.45. Initial 600.00; ramp 1: −50.00, then
        // +4558.45 ⇒ payback month 2 (time-adjusted −50.00, +1408.45 ⇒ 2).
        let opp = with_economics(
            "HIGH_TICKET_DELIVERY",
            &economics(
                &[
                    ("variable_cost", "500"),
                    ("fixed_costs_per_month", "\"50.00\""),
                    ("owner_hours_per_month", "0"),
                    ("ramp_months", "1"),
                ],
                "ONE_OFF",
                &[
                    ("contract_value", "\"22000.00\""),
                    ("win_probability", "4500"),
                    ("delivery_weeks", "7"),
                    ("owner_hours_total", "90"),
                    ("collection_loss", "200"),
                ],
                ["\"0.00\"", "\"400.00\"", "\"200.00\"", "\"0.00\""],
            ),
            "",
        );
        let b = base(&opp);
        assert_eq!(b.occupied_months, Some(Metric::Known(2)));
        assert_eq!(known(&b.monthly_revenue_collected), m("4851.00"));
        assert_eq!(known(&b.monthly_cash_contribution), m("4558.45"));
        assert_eq!(known(&b.owner_hours_per_month), 45);
        assert_eq!(known(&b.owner_time_cost), m("3150.00"));
        assert_eq!(known(&b.time_adjusted_contribution), m("1408.45"));
        assert_eq!(known(&b.initial_capital), m("600.00"));
        assert_eq!(known(&b.payback), Payback::Months(2));
        assert_eq!(known(&b.payback_time_adjusted), Payback::Months(2));
        // An engagement that never covers its capital: −50.00 + 2 × 4558.45
        // = 9066.90 < 9400.00, and nothing comes after it.
        let mut big = opp.clone();
        big.economics.initial.setup.value = Est::point(m("9200.00"));
        assert_eq!(known(&base(&big).payback), Payback::NotReached);
    }

    #[test]
    fn golden_acquisition_payback() {
        // 2400.00 a month churned 2 % monthly from month 1 (floored), 1.5 %
        // collection loss, ramp 1 (no revenue in month 1). Month 9: 2041.81
        // × 98.5 % = 2011.18; variable 15 % = ⌈301.677⌉ = 301.68; fixed
        // 300.00 ⇒ 1409.50; owner 15 h × 70.00 = 1050.00 ⇒ 359.50.
        // Cash by month: −300.00, 1669.21, 1629.82, 1591.22, 1553.39,
        // 1516.32, 1479.99 ⇒ the running sum reaches 9139.95 ≥ initial
        // 8650.00 in month 7 (7659.96 in month 6).
        let opp = with_economics(
            "ACQUIRE_TRANSFORM",
            &economics(
                &[
                    ("variable_cost", "1500"),
                    ("fixed_costs_per_month", "\"300.00\""),
                    ("owner_hours_per_month", "15"),
                    ("ramp_months", "1"),
                ],
                "ACQUISITION",
                &[
                    ("asset_monthly_revenue", "\"2400.00\""),
                    ("churn_per_month", "200"),
                    ("collection_loss", "150"),
                ],
                ["\"7500.00\"", "\"500.00\"", "\"250.00\"", "\"400.00\""],
            ),
            VERIFIED_DEAL,
        );
        let b = base(&opp);
        assert_eq!(known(&b.monthly_revenue_collected), m("2011.18"));
        assert_eq!(known(&b.monthly_cash_contribution), m("1409.50"));
        assert_eq!(known(&b.time_adjusted_contribution), m("359.50"));
        assert_eq!(known(&b.initial_capital), m("8650.00"));
        assert_eq!(known(&b.payback), Payback::Months(7));
    }

    #[test]
    fn golden_revenue_share() {
        // 9000.00 × 20 % × 97 % = 1746.00 from month 3; fixed 25.00 ⇒ cash
        // 1721.00; owner 4 h × 70.00 = 280.00 ⇒ 1441.00. Initial 350.00:
        // −25.00, −25.00, +1721.00 ⇒ month 3 (time-adjusted −305.00,
        // −305.00, +1441.00 ⇒ month 3).
        let opp = with_economics(
            "PARTNER_REVSHARE",
            &economics(
                &[
                    ("variable_cost", "0"),
                    ("fixed_costs_per_month", "\"25.00\""),
                    ("owner_hours_per_month", "4"),
                    ("ramp_months", "2"),
                ],
                "REVENUE_SHARE",
                &[
                    ("partner_monthly_revenue", "\"9000.00\""),
                    ("share", "2000"),
                    ("collection_loss", "300"),
                ],
                ["\"0.00\"", "\"350.00\"", "\"0.00\"", "\"0.00\""],
            ),
            "",
        );
        let b = base(&opp);
        assert_eq!(known(&b.monthly_revenue_collected), m("1746.00"));
        assert_eq!(known(&b.monthly_cash_contribution), m("1721.00"));
        assert_eq!(known(&b.time_adjusted_contribution), m("1441.00"));
        assert_eq!(known(&b.payback), Payback::Months(3));
        assert_eq!(known(&b.payback_time_adjusted), Payback::Months(3));
        // A ramp as long as the horizon: nothing earned by month 9.
        let mut late = opp.clone();
        late.economics.ramp_months.value = Est::point(9);
        assert_eq!(known(&base(&late).monthly_revenue_collected), Minor::ZERO);
    }

    /// `downside ≤ base ≤ upside`, read in each figure's favorable direction.
    fn assert_ordered(sc: &Scenarios, what: &str) {
        let (d, b, u) = (&sc.downside, &sc.base, &sc.upside);
        type Get<T> = fn(&ScenarioMetrics) -> &Metric<T>;
        let income: [(Get<Minor>, &str); 3] = [
            (|s| &s.monthly_revenue_collected, "revenue"),
            (|s| &s.monthly_cash_contribution, "cash"),
            (|s| &s.time_adjusted_contribution, "time-adjusted"),
        ];
        for (f, name) in income {
            let (x, y, z) = (known(f(d)), known(f(b)), known(f(u)));
            assert!(x <= y && y <= z, "{what}: {name}: {x} {y} {z}");
        }
        let cost: [(Get<Minor>, &str); 2] = [
            (|s| &s.owner_time_cost, "owner cost"),
            (|s| &s.initial_capital, "initial"),
        ];
        for (f, name) in cost {
            let (x, y, z) = (known(f(d)), known(f(b)), known(f(u)));
            assert!(x >= y && y >= z, "{what}: {name}: {x} {y} {z}");
        }
        let paybacks: [Get<Payback>; 2] = [|s| &s.payback, |s| &s.payback_time_adjusted];
        for f in paybacks {
            let (x, y, z) = (known(f(d)), known(f(b)), known(f(u)));
            assert!(x >= y && y >= z, "{what}: payback {x:?} {y:?} {z:?}");
        }
        let hours = [d, b, u].map(|s| known(&s.owner_hours_per_month));
        assert!(
            hours[0] >= hours[1] && hours[1] >= hours[2],
            "{what}: hours"
        );
    }

    #[test]
    fn downside_le_base_le_upside_every_metric() {
        let p = synthetic();
        assert_ordered(&scenarios(&priced(), &p).unwrap(), "recurring");
        // One-off with ranges everywhere, the delivery length included; a
        // costly owner makes the time-adjusted figure negative — the
        // downside still takes the worse end for each figure.
        let one_off = with_economics(
            "INTEGRATE",
            r#"[economics]
currency = "EUR"
revenue_quality = "ONE_OFF"
tax_review = "UNKNOWN"
variable_cost = { value = { low = 200, base = 500, high = 900 }, evidence = [], as_of = "2026-10-01" }
fixed_costs_per_month = { value = { low = "20.00", base = "50.00", high = "90.00" }, evidence = [], as_of = "2026-10-01" }
owner_hours_per_month = { value = { low = 2, base = 4, high = 7 }, evidence = [], as_of = "2026-10-01" }
ramp_months = { value = { low = 0, base = 1, high = 3 }, evidence = [], as_of = "2026-10-01" }

[economics.revenue]
kind = "ONE_OFF"
contract_value = { value = { low = "9000.00", base = "11000.00", high = "14000.00" }, evidence = [], as_of = "2026-10-01" }
win_probability = { value = { low = 2000, base = 3500, high = 4500 }, evidence = [], as_of = "2026-10-01" }
delivery_weeks = { value = { low = 3, base = 7, high = 15 }, evidence = [], as_of = "2026-10-01" }
owner_hours_total = { value = { low = 40, base = 70, high = 130 }, evidence = [], as_of = "2026-10-01" }
collection_loss = { value = { low = 0, base = 200, high = 500 }, evidence = [], as_of = "2026-10-01" }

[economics.initial]
acquisition = { value = { low = "0.00", base = "0.00", high = "0.00" }, evidence = [], as_of = "2026-10-01" }
setup = { value = { low = "200.00", base = "400.00", high = "700.00" }, evidence = [], as_of = "2026-10-01" }
validation = { value = { low = "50.00", base = "150.00", high = "350.00" }, evidence = [], as_of = "2026-10-01" }
working_capital = { value = { low = "0.00", base = "0.00", high = "250.00" }, evidence = [], as_of = "2026-10-01" }
"#,
            "",
        );
        let sc = scenarios(&one_off, &p).unwrap();
        assert_ordered(&sc, "one-off");
        assert!(known(&sc.downside.time_adjusted_contribution) < Minor::ZERO);
        // 15 weeks ⇒ 4 months; 3 weeks ⇒ 1.
        assert_eq!(sc.downside.occupied_months, Some(Metric::Known(4)));
        assert_eq!(sc.upside.occupied_months, Some(Metric::Known(1)));
    }

    #[test]
    fn missing_price_makes_contribution_unknown_not_zero() {
        // The recurring example's price is UNKNOWN.
        let sc = scenarios(&recurring(), &synthetic()).unwrap();
        for s in Scenario::ALL {
            let x = sc.get(s);
            for f in [
                &x.monthly_revenue_collected,
                &x.monthly_cash_contribution,
                &x.time_adjusted_contribution,
            ] {
                assert_eq!(f.fields(), [fields::PRICE], "{s:?}");
            }
            assert_eq!(x.payback.fields(), [fields::PRICE]);
            assert_eq!(x.payback_time_adjusted.fields(), [fields::PRICE]);
            // Figures that do not need the price stay known.
            assert!(x.initial_capital.known().is_some());
            assert!(x.owner_time_cost.known().is_some());
        }
        let j = serde_json::to_value(&sc.base).unwrap();
        assert_eq!(
            j["monthly_cash_contribution"],
            "UNKNOWN: needs economics.revenue.price_per_month"
        );
        assert!(j["owner_time_cost"].as_str().unwrap().ends_with(".00"));
    }

    #[test]
    fn missing_hours_makes_time_adjusted_unknown() {
        let mut o = priced();
        o.economics.owner_hours_per_month.value = Est::unknown("no time log yet");
        let b = base(&o);
        assert!(b.monthly_cash_contribution.known().is_some());
        assert!(b.payback.known().is_some());
        for f in [&b.owner_time_cost, &b.time_adjusted_contribution] {
            assert_eq!(f.fields(), [fields::OWNER_HOURS]);
        }
        assert_eq!(b.owner_hours_per_month.fields(), [fields::OWNER_HOURS]);
        assert_eq!(b.payback_time_adjusted.fields(), [fields::OWNER_HOURS]);
        // Two unknowns: both named, sorted.
        o.economics.variable_cost.value = Est::unknown("");
        assert_eq!(
            base(&o).time_adjusted_contribution.fields(),
            [fields::OWNER_HOURS, fields::VARIABLE_COST]
        );
    }

    #[test]
    fn missing_probability_makes_expected_loss_unknown() {
        let mut risk = priced().risk;
        risk.risks.push(Risk {
            kind: RiskKind::Platform,
            probability: Est::unknown("no base rate"),
            impact: Est::range(m("0.00"), m("200.00"), m("700.00")).unwrap(),
            control: "a second channel".into(),
            status: RiskStatus::Unresolved,
        });
        assert_eq!(
            expected_loss(&risk).unwrap(),
            Metric::Unknown {
                fields: vec!["risk.risks[0].probability".into()]
            }
        );
        let mut o = priced();
        o.risk = risk.clone();
        assert_eq!(
            scenarios(&o, &synthetic()).unwrap().expected_loss.fields(),
            ["risk.risks[0].probability"]
        );
        // Known: a range, low floored, high ceiled. 0.01 % × 0.01 floors to
        // 0; 3.33 % × 700.01 = 23.310333 ceils to 23.32.
        risk.risks[0].probability = Est::range(
            Bps::new(1).unwrap(),
            Bps::new(50).unwrap(),
            Bps::new(333).unwrap(),
        )
        .unwrap();
        risk.risks[0].impact = Est::range(m("0.01"), m("200.00"), m("700.01")).unwrap();
        assert_eq!(
            expected_loss(&risk).unwrap(),
            Metric::Known(LossRange {
                low: Minor::ZERO,
                high: m("23.32")
            })
        );
        // Two risks add: + 1.5 % × 0.00 .. 15 % × 4000.00 = 600.00.
        risk.risks.push(Risk {
            probability: Est::range(
                Bps::new(150).unwrap(),
                Bps::new(500).unwrap(),
                Bps::new(1500).unwrap(),
            )
            .unwrap(),
            impact: Est::range(m("0.00"), m("800.00"), m("4000.00")).unwrap(),
            ..risk.risks[0].clone()
        });
        assert_eq!(
            known(&expected_loss(&risk).unwrap()),
            LossRange {
                low: Minor::ZERO,
                high: m("623.32")
            }
        );
    }

    #[test]
    fn non_eur_without_fx_is_unknown() {
        let mut o = priced();
        o.economics.currency = Currency::Usd;
        let b = base(&o);
        for f in [
            &b.monthly_revenue_collected,
            &b.monthly_cash_contribution,
            &b.time_adjusted_contribution,
            &b.initial_capital,
        ] {
            assert_eq!(f.fields(), [fields::FX]);
        }
        assert_eq!(b.payback_time_adjusted.fields(), [fields::FX]);
        // The cash payback is a native ratio: it needs no rate.
        assert!(b.payback.known().is_some());
        // Owner time is priced in the profile currency already: 11 h × 70.00.
        assert_eq!(known(&b.owner_time_cost), m("770.00"));
        // An unknown price and no rate: both named.
        let mut both = o.clone();
        if let RevenueModel::Recurring {
            price_per_month, ..
        } = &mut both.economics.revenue
        {
            price_per_month.value = Est::unknown("");
        }
        assert_eq!(
            base(&both).monthly_cash_contribution.fields(),
            [fields::FX, fields::PRICE]
        );
        // A rate into another currency does not help.
        let day = "2026-10-01".parse().unwrap();
        let src = || "url:https://example.org/fx".parse().unwrap();
        o.economics.fx = Some(FxRate::new("USD/GBP".parse().unwrap(), "0.8", src(), day).unwrap());
        assert_eq!(base(&o).initial_capital.fields(), [fields::FX]);
        // USD/EUR converts: initial 1050.00 USD × 0.8 = 840.00 EUR.
        o.economics.fx = Some(FxRate::new("USD/EUR".parse().unwrap(), "0.8", src(), day).unwrap());
        let b = base(&o);
        assert_eq!(known(&b.initial_capital), m("840.00"));
        assert_eq!(
            known(&b.time_adjusted_contribution),
            known(&b.monthly_cash_contribution)
                .checked_sub(m("770.00"))
                .unwrap()
        );
    }

    #[test]
    fn negative_contribution_payback_not_reached() {
        let mut o = priced();
        o.economics.fixed_costs_per_month.value = Est::point(m("9000.00"));
        let b = base(&o);
        assert!(known(&b.monthly_cash_contribution) < Minor::ZERO);
        // The base keeps growing after the run-rate month: it pays back
        // later, never within the horizon.
        assert!(matches!(known(&b.payback), Payback::Months(n) if n > 9));
        // No lead ever converts: every month loses the fixed costs.
        if let RevenueModel::Recurring {
            leads_per_month, ..
        } = &mut o.economics.revenue
        {
            leads_per_month.value = Est::point(0);
        }
        let b = base(&o);
        assert_eq!(known(&b.monthly_revenue_collected), Minor::ZERO);
        assert_eq!(known(&b.monthly_cash_contribution), m("-9000.00"));
        assert_eq!(known(&b.payback), Payback::NotReached);
        assert_eq!(known(&b.payback_time_adjusted), Payback::NotReached);
        assert_eq!(
            serde_json::to_value(Payback::NotReached).unwrap(),
            "NOT_REACHED"
        );
        assert_eq!(serde_json::to_value(Payback::Months(4)).unwrap(), 4);
        assert!(Payback::Months(u32::MAX) < Payback::NotReached);
        // No initial capital pays back at once.
        let mut free = priced();
        for a in [
            &mut free.economics.initial.setup,
            &mut free.economics.initial.validation,
            &mut free.economics.initial.working_capital,
        ] {
            a.value = Est::point(Minor::ZERO);
        }
        assert_eq!(known(&base(&free).payback), Payback::Months(0));
    }

    #[test]
    fn same_inputs_same_canonical_hash() {
        let p = synthetic();
        let a = scenarios(&priced(), &p).unwrap();
        assert_eq!(scenarios(&priced(), &p).unwrap(), a);
        assert_eq!(a.inputs_sha256.len(), 64);
        // Key order in the file does not matter: the hash is canonical.
        let reordered = RECURRING
            .replace(
                "revenue_quality = \"RECURRING\"\ntax_review = \"UNKNOWN\"\n",
                "",
            )
            .replace(
                "currency = \"EUR\"\n",
                "tax_review = \"UNKNOWN\"\nrevenue_quality = \"RECURRING\"\ncurrency = \"EUR\"\n",
            );
        assert_ne!(reordered, RECURRING);
        let o: Opportunity = from_toml(&reordered).unwrap();
        assert_eq!(
            inputs_sha256(&o, &p).unwrap(),
            inputs_sha256(&recurring(), &p).unwrap()
        );
        // An input or a knob it reads changes it.
        let mut cheaper = priced();
        cheaper.economics.fixed_costs_per_month.value =
            Est::range(m("40.00"), m("59.99"), m("90.00")).unwrap();
        assert_ne!(
            scenarios(&cheaper, &p).unwrap().inputs_sha256,
            a.inputs_sha256
        );
        let mut q = p.clone();
        q.shadow_hourly_rate = m("70.01");
        assert_ne!(
            scenarios(&priced(), &q).unwrap().inputs_sha256,
            a.inputs_sha256
        );
        // A knob it does not read leaves it alone.
        let mut r = p.clone();
        r.max_cash_exposure = m("20000.01");
        assert_eq!(
            scenarios(&priced(), &r).unwrap().inputs_sha256,
            a.inputs_sha256
        );
    }
}
