//! Opportunity — a testable profit hypothesis (PRD § 3, § 6, § 7.1):
//! customer, pain, mechanism, the source records it rests on, economic inputs
//! as ranges with evidence, risk, an experiment, a deal review. The economics
//! (O1 W3) and gates (O1 W4) read it; nothing here decides.
//!
//! | Field | Value |
//! |---|---|
//! | header | `schema = "soe.opportunity/1"`, `id`, `version` ≥ 1, `as_of` (known) |
//! | `customer`, `pain` | text |
//! | `mechanism`, `alternatives[]` | [`Mechanism`] (12, closed); alternatives unique and not the mechanism |
//! | `requires_skills[]` | skills a capability must cover (`OperatorCapability.skill`) |
//! | `signals[]` | source record ids (`domain/source/`, `<source_id>:<native_id>:<content_hash>`, in full) — no SOE signal type |
//! | `[jurisdictions]` | `customer` `operator` `entity` `delivery` `tax` `payment` `data`: a code or `"UNKNOWN"` |
//! | `[economics]` | [`EconomicInputs`] |
//! | `[risk]` | `risk::RiskAssessment` |
//! | `[experiment]?` · `[deal]?` | `experiment::ExperimentSpec` · [`DealReview`] |
//! | `[ordinal]` | `defensibility`, `reversibility`: `Tier` |
//!
//! | `[economics]` | Value |
//! |---|---|
//! | `currency`, `fx?` | every amount below is in `currency`; `fx` converts it (`USD/EUR`: base = `currency`) |
//! | `[economics.revenue]` | `kind` + its inputs (below), each an `Assumption` |
//! | `variable_cost` (bps of revenue), `fixed_costs_per_month`, `owner_hours_per_month`, `ramp_months` | `Assumption`s |
//! | `[economics.initial]` | `acquisition`, `setup`, `validation`, `working_capital`: `Assumption` money |
//! | `revenue_quality` · `tax_review` | `RECURRING` `CONTRACTED` `TRANSACTIONAL` `ONE_OFF` `SPECULATIVE` · `UNKNOWN` `REVIEWED` |
//!
//! | `revenue.kind` | Inputs |
//! |---|---|
//! | `RECURRING` | `leads_per_month`, `conversion` (bps), `churn_per_month` (bps), `price_per_month`, `collection_loss` (bps) |
//! | `ONE_OFF` | `contract_value`, `win_probability` (bps), `delivery_weeks`, `owner_hours_total`, `collection_loss` |
//! | `ACQUISITION` | `asset_monthly_revenue`, `churn_per_month`, `collection_loss` |
//! | `REVENUE_SHARE` | `partner_monthly_revenue`, `share` (bps), `collection_loss` |
//!
//! Every input has a dotted field (`economics.revenue.price_per_month`,
//! `economics.initial.setup`, …): `EconomicInputs::inputs` / `input` /
//! `input_mut` name the closed set an O3 proposal basis, forecast or
//! challenge may refer to.
//!
//! | Rule (`validate`) | Code |
//! |---|---|
//! | `HIGH_TICKET_DELIVERY` with `RECURRING` revenue | `fake_recurring` (PRD § 12: no fake recurring revenue) |
//! | `PARTNER_REVSHARE` without `REVENUE_SHARE`; `ACQUIRE_TRANSFORM` without `ACQUISITION` | `revenue_model_mismatch` |
//! | `ACQUIRE_TRANSFORM` without `[deal]` | `deal_review_missing` |
//! | an assumption or the `fx` rate dated after `as_of` | `future_leakage` |
//! | a negative amount; `fx` not from `currency`; a `VERIFIED` diligence block without evidence | `invalid_field` |

// `Mechanism::ALL` is read by tests only.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::experiment::ExperimentSpec;
use super::record::{Problems, SoeRecord, Tier};
use super::risk::RiskAssessment;
use super::value::{codes, Assumption, Bps, Currency, Est, FxRate, Minor, SchemaTag};
use crate::domain::lineage::value::{Locator, Time, TimeOrder};

/// How value is captured (PRD § 3; `HOLD` / `REJECT` preserve capital and time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mechanism {
    AcquireTransform,
    Automate,
    Integrate,
    RescueMigrate,
    Productize,
    BuildAdjacent,
    WhiteLabel,
    License,
    PartnerRevshare,
    HighTicketDelivery,
    Hold,
    Reject,
}

impl Mechanism {
    pub const ALL: [Mechanism; 12] = [
        Mechanism::AcquireTransform,
        Mechanism::Automate,
        Mechanism::Integrate,
        Mechanism::RescueMigrate,
        Mechanism::Productize,
        Mechanism::BuildAdjacent,
        Mechanism::WhiteLabel,
        Mechanism::License,
        Mechanism::PartnerRevshare,
        Mechanism::HighTicketDelivery,
        Mechanism::Hold,
        Mechanism::Reject,
    ];
}

/// How revenue arrives (module table: `revenue.kind`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum RevenueModel {
    Recurring {
        leads_per_month: Assumption<u32>,
        conversion: Assumption<Bps>,
        churn_per_month: Assumption<Bps>,
        price_per_month: Assumption<Minor>,
        collection_loss: Assumption<Bps>,
    },
    OneOff {
        contract_value: Assumption<Minor>,
        win_probability: Assumption<Bps>,
        delivery_weeks: Assumption<u32>,
        owner_hours_total: Assumption<u32>,
        collection_loss: Assumption<Bps>,
    },
    Acquisition {
        asset_monthly_revenue: Assumption<Minor>,
        churn_per_month: Assumption<Bps>,
        collection_loss: Assumption<Bps>,
    },
    RevenueShare {
        partner_monthly_revenue: Assumption<Minor>,
        share: Assumption<Bps>,
        collection_loss: Assumption<Bps>,
    },
}

impl RevenueModel {
    pub fn kind(&self) -> &'static str {
        match self {
            RevenueModel::Recurring { .. } => "RECURRING",
            RevenueModel::OneOff { .. } => "ONE_OFF",
            RevenueModel::Acquisition { .. } => "ACQUISITION",
            RevenueModel::RevenueShare { .. } => "REVENUE_SHARE",
        }
    }

    fn inputs(&self) -> Vec<(&'static str, Input<'_>)> {
        use Input::{Amount as A, Count as C, Share as S};
        match self {
            RevenueModel::Recurring {
                leads_per_month,
                conversion,
                churn_per_month,
                price_per_month,
                collection_loss,
            } => vec![
                ("leads_per_month", C(leads_per_month)),
                ("conversion", S(conversion)),
                ("churn_per_month", S(churn_per_month)),
                ("price_per_month", A(price_per_month)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::OneOff {
                contract_value,
                win_probability,
                delivery_weeks,
                owner_hours_total,
                collection_loss,
            } => vec![
                ("contract_value", A(contract_value)),
                ("win_probability", S(win_probability)),
                ("delivery_weeks", C(delivery_weeks)),
                ("owner_hours_total", C(owner_hours_total)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::Acquisition {
                asset_monthly_revenue,
                churn_per_month,
                collection_loss,
            } => vec![
                ("asset_monthly_revenue", A(asset_monthly_revenue)),
                ("churn_per_month", S(churn_per_month)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::RevenueShare {
                partner_monthly_revenue,
                share,
                collection_loss,
            } => vec![
                ("partner_monthly_revenue", A(partner_monthly_revenue)),
                ("share", S(share)),
                ("collection_loss", S(collection_loss)),
            ],
        }
    }

    /// [`RevenueModel::inputs`], mutable (same order).
    fn inputs_mut(&mut self) -> Vec<(&'static str, InputMut<'_>)> {
        use InputMut::{Amount as A, Count as C, Share as S};
        match self {
            RevenueModel::Recurring {
                leads_per_month,
                conversion,
                churn_per_month,
                price_per_month,
                collection_loss,
            } => vec![
                ("leads_per_month", C(leads_per_month)),
                ("conversion", S(conversion)),
                ("churn_per_month", S(churn_per_month)),
                ("price_per_month", A(price_per_month)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::OneOff {
                contract_value,
                win_probability,
                delivery_weeks,
                owner_hours_total,
                collection_loss,
            } => vec![
                ("contract_value", A(contract_value)),
                ("win_probability", S(win_probability)),
                ("delivery_weeks", C(delivery_weeks)),
                ("owner_hours_total", C(owner_hours_total)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::Acquisition {
                asset_monthly_revenue,
                churn_per_month,
                collection_loss,
            } => vec![
                ("asset_monthly_revenue", A(asset_monthly_revenue)),
                ("churn_per_month", S(churn_per_month)),
                ("collection_loss", S(collection_loss)),
            ],
            RevenueModel::RevenueShare {
                partner_monthly_revenue,
                share,
                collection_loss,
            } => vec![
                ("partner_monthly_revenue", A(partner_monthly_revenue)),
                ("share", S(share)),
                ("collection_loss", S(collection_loss)),
            ],
        }
    }
}

/// One economic input of any value type.
#[derive(Debug, Clone, Copy)]
pub enum Input<'a> {
    Count(&'a Assumption<u32>),
    Share(&'a Assumption<Bps>),
    Amount(&'a Assumption<Minor>),
}

/// One economic input, mutable (O3: a Critic's conservative merge, a
/// reprice search).
#[derive(Debug)]
pub enum InputMut<'a> {
    Count(&'a mut Assumption<u32>),
    Share(&'a mut Assumption<Bps>),
    Amount(&'a mut Assumption<Minor>),
}

/// What the gates read of one economic input: no value, only its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputState {
    /// Dotted path under the opportunity (`economics.revenue.price_per_month`).
    pub field: String,
    pub known: bool,
    pub evidenced: bool,
    pub as_of: Time,
}

impl Input<'_> {
    fn state(&self, field: String) -> InputState {
        let (known, evidenced, as_of) = match self {
            Input::Count(a) => (a.value.is_known(), a.is_evidenced(), a.as_of),
            Input::Share(a) => (a.value.is_known(), a.is_evidenced(), a.as_of),
            Input::Amount(a) => (a.value.is_known(), a.is_evidenced(), a.as_of),
        };
        InputState {
            field,
            known,
            evidenced,
            as_of,
        }
    }

    /// Its evidence locators.
    pub fn evidence(&self) -> &[Locator] {
        match self {
            Input::Count(a) => &a.evidence,
            Input::Share(a) => &a.evidence,
            Input::Amount(a) => &a.evidence,
        }
    }

    pub fn is_known(&self) -> bool {
        match self {
            Input::Count(a) => a.value.is_known(),
            Input::Share(a) => a.value.is_known(),
            Input::Amount(a) => a.value.is_known(),
        }
    }

    /// Unknown: why (`None` when no reason was given); known: `None`.
    pub fn unknown_reason(&self) -> Option<&str> {
        fn reason<T>(e: &Est<T>) -> Option<&str> {
            match e {
                Est::Unknown { reason } => reason.as_deref(),
                Est::Range { .. } => None,
            }
        }
        match self {
            Input::Count(a) => reason(&a.value),
            Input::Share(a) => reason(&a.value),
            Input::Amount(a) => reason(&a.value),
        }
    }

    /// The estimate as text: `low..base..high` (a share in bps), or
    /// `UNKNOWN[: reason]`.
    pub fn value_text(&self) -> String {
        fn text<T: std::fmt::Display>(e: &Est<T>) -> String {
            match e {
                Est::Range { low, base, high } => format!("{low}..{base}..{high}"),
                Est::Unknown { reason: None } => "UNKNOWN".into(),
                Est::Unknown { reason: Some(r) } => format!("UNKNOWN: {r}"),
            }
        }
        match self {
            Input::Count(a) => text(&a.value),
            Input::Share(a) => text(&a.value),
            Input::Amount(a) => text(&a.value),
        }
    }

    /// A known amount's low end (`None` for counts and shares).
    fn amount_low(&self) -> Option<Minor> {
        match self {
            Input::Amount(a) => match &a.value {
                Est::Range { low, .. } => Some(*low),
                Est::Unknown { .. } => None,
            },
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialCapital {
    pub acquisition: Assumption<Minor>,
    pub setup: Assumption<Minor>,
    pub validation: Assumption<Minor>,
    pub working_capital: Assumption<Minor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevenueQuality {
    Recurring,
    Contracted,
    Transactional,
    OneOff,
    Speculative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaxReview {
    /// PRD § 7.1: tax stays unknown until reviewed for the actual entity.
    Unknown,
    Reviewed,
}

/// Module table: `[economics]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomicInputs {
    pub currency: Currency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx: Option<FxRate>,
    pub revenue: RevenueModel,
    /// Share of collected revenue.
    pub variable_cost: Assumption<Bps>,
    pub fixed_costs_per_month: Assumption<Minor>,
    pub owner_hours_per_month: Assumption<u32>,
    pub ramp_months: Assumption<u32>,
    pub initial: InitialCapital,
    pub revenue_quality: RevenueQuality,
    pub tax_review: TaxReview,
}

impl EconomicInputs {
    /// Every input with its dotted field, revenue inputs first, then in
    /// field order (the closed set an O3 proposal or challenge may name).
    pub fn inputs(&self) -> Vec<(String, Input<'_>)> {
        let mut out: Vec<(String, Input<'_>)> = self
            .revenue
            .inputs()
            .into_iter()
            .map(|(f, i)| (format!("economics.revenue.{f}"), i))
            .collect();
        out.extend([
            (
                "economics.variable_cost".to_string(),
                Input::Share(&self.variable_cost),
            ),
            (
                "economics.fixed_costs_per_month".to_string(),
                Input::Amount(&self.fixed_costs_per_month),
            ),
            (
                "economics.owner_hours_per_month".to_string(),
                Input::Count(&self.owner_hours_per_month),
            ),
            (
                "economics.ramp_months".to_string(),
                Input::Count(&self.ramp_months),
            ),
            (
                "economics.initial.acquisition".to_string(),
                Input::Amount(&self.initial.acquisition),
            ),
            (
                "economics.initial.setup".to_string(),
                Input::Amount(&self.initial.setup),
            ),
            (
                "economics.initial.validation".to_string(),
                Input::Amount(&self.initial.validation),
            ),
            (
                "economics.initial.working_capital".to_string(),
                Input::Amount(&self.initial.working_capital),
            ),
        ]);
        out
    }

    /// Every input's state, revenue inputs first, then in field order.
    pub fn input_states(&self) -> Vec<InputState> {
        self.inputs().into_iter().map(|(f, i)| i.state(f)).collect()
    }

    /// The input named `field` (`economics.revenue.price_per_month`).
    pub fn input(&self, field: &str) -> Option<Input<'_>> {
        self.inputs()
            .into_iter()
            .find(|(f, _)| f == field)
            .map(|(_, i)| i)
    }

    /// The input named `field`, mutable.
    pub fn input_mut(&mut self, field: &str) -> Option<InputMut<'_>> {
        let rest = field.strip_prefix("economics.")?;
        if let Some(name) = rest.strip_prefix("revenue.") {
            return self
                .revenue
                .inputs_mut()
                .into_iter()
                .find(|(f, _)| *f == name)
                .map(|(_, i)| i);
        }
        let i = &mut self.initial;
        Some(match rest {
            "variable_cost" => InputMut::Share(&mut self.variable_cost),
            "fixed_costs_per_month" => InputMut::Amount(&mut self.fixed_costs_per_month),
            "owner_hours_per_month" => InputMut::Count(&mut self.owner_hours_per_month),
            "ramp_months" => InputMut::Count(&mut self.ramp_months),
            "initial.acquisition" => InputMut::Amount(&mut i.acquisition),
            "initial.setup" => InputMut::Amount(&mut i.setup),
            "initial.validation" => InputMut::Amount(&mut i.validation),
            "initial.working_capital" => InputMut::Amount(&mut i.working_capital),
            _ => return None,
        })
    }
}

/// `[jurisdictions]`: a code (`DE`, `EU`, …) or `"UNKNOWN"` each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Jurisdictions {
    pub customer: String,
    pub operator: String,
    pub entity: String,
    pub delivery: String,
    pub tax: String,
    pub payment: String,
    pub data: String,
}

impl Jurisdictions {
    pub fn each(&self) -> [(&'static str, &str); 7] {
        [
            ("customer", &self.customer),
            ("operator", &self.operator),
            ("entity", &self.entity),
            ("delivery", &self.delivery),
            ("tax", &self.tax),
            ("payment", &self.payment),
            ("data", &self.data),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiligenceStatus {
    Verified,
    /// What the seller says: never a fact.
    SellerClaim,
    Unverified,
    RedFlag,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiligenceBlock {
    pub status: DiligenceStatus,
    pub evidence: Vec<Locator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Acquisition / partnership diligence (PRD § 6 `DealReview`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DealReview {
    pub identity: DiligenceBlock,
    pub ownership: DiligenceBlock,
    pub revenue_proof: DiligenceBlock,
    pub costs: DiligenceBlock,
    pub technical: DiligenceBlock,
    pub distribution: DiligenceBlock,
    pub legal: DiligenceBlock,
    pub transition: DiligenceBlock,
    pub red_flags: Vec<String>,
}

impl DealReview {
    /// `(field, block)` in field order.
    pub fn blocks(&self) -> [(&'static str, &DiligenceBlock); 8] {
        [
            ("identity", &self.identity),
            ("ownership", &self.ownership),
            ("revenue_proof", &self.revenue_proof),
            ("costs", &self.costs),
            ("technical", &self.technical),
            ("distribution", &self.distribution),
            ("legal", &self.legal),
            ("transition", &self.transition),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ordinal {
    pub defensibility: Tier,
    pub reversibility: Tier,
}

/// `soe.opportunity/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opportunity {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub as_of: Time,
    pub customer: String,
    pub pain: String,
    pub mechanism: Mechanism,
    pub alternatives: Vec<Mechanism>,
    pub requires_skills: Vec<String>,
    pub signals: Vec<String>,
    pub jurisdictions: Jurisdictions,
    pub economics: EconomicInputs,
    pub risk: RiskAssessment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<ExperimentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deal: Option<DealReview>,
    pub ordinal: Ordinal,
}

impl SoeRecord for Opportunity {
    const RECORD: &'static str = "opportunity";

    fn schema(&self) -> &SchemaTag {
        &self.schema
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn problems(&self, p: &mut Problems) {
        p.known("as_of", &self.as_of);
        p.text("customer", &self.customer);
        p.text("pain", &self.pain);
        p.unique("alternatives", self.alternatives.iter());
        p.check(
            !self.alternatives.contains(&self.mechanism),
            codes::DUPLICATE,
            "alternatives",
            format_args!("{:?} is the mechanism itself", self.mechanism),
        );
        p.unique_texts(
            "requires_skills",
            self.requires_skills.iter().map(String::as_str),
        );
        p.unique_texts("signals", self.signals.iter().map(String::as_str));
        for (f, v) in self.jurisdictions.each() {
            p.text(&format!("jurisdictions.{f}"), v);
        }

        // Mechanism ↔ revenue model (module table).
        let kind = self.economics.revenue.kind();
        match (self.mechanism, &self.economics.revenue) {
            (Mechanism::HighTicketDelivery, RevenueModel::Recurring { .. }) => p.push(
                codes::FAKE_RECURRING,
                "economics.revenue.kind",
                "HIGH_TICKET_DELIVERY is one-off delivery: RECURRING revenue is not its model",
            ),
            (Mechanism::PartnerRevshare, m) if !matches!(m, RevenueModel::RevenueShare { .. }) => p
                .push(
                    codes::REVENUE_MODEL_MISMATCH,
                    "economics.revenue.kind",
                    format_args!("PARTNER_REVSHARE takes REVENUE_SHARE, not {kind}"),
                ),
            (Mechanism::AcquireTransform, m) if !matches!(m, RevenueModel::Acquisition { .. }) => p
                .push(
                    codes::REVENUE_MODEL_MISMATCH,
                    "economics.revenue.kind",
                    format_args!("ACQUIRE_TRANSFORM takes ACQUISITION, not {kind}"),
                ),
            _ => {}
        }
        p.check(
            self.mechanism != Mechanism::AcquireTransform || self.deal.is_some(),
            codes::DEAL_REVIEW_MISSING,
            "deal",
            "ACQUIRE_TRANSFORM needs a [deal] review",
        );

        // Economic inputs: amounts ≥ 0, none dated after the record.
        let e = &self.economics;
        if let Some(fx) = &e.fx {
            p.check(
                fx.pair.base == e.currency,
                codes::INVALID_FIELD,
                "economics.fx.pair",
                format_args!("{} does not convert {}", fx.pair, e.currency),
            );
            p.not_after("economics.fx.as_of", &fx.as_of, "as_of", &self.as_of);
        }
        for (field, input) in e.inputs() {
            if let Some(low) = input.amount_low() {
                p.non_negative(&field, low);
            }
            let state = input.state(field.clone());
            p.not_after(
                &format!("{field}.as_of"),
                &state.as_of,
                "as_of",
                &self.as_of,
            );
        }

        self.risk.problems("risk", p);
        if let Some(x) = &self.experiment {
            x.problems("experiment", p);
            p.check(
                self.as_of.order(&x.deadline) != TimeOrder::After,
                codes::INVALID_TIME,
                "experiment.deadline",
                format_args!("{} is before as_of {}", x.deadline, self.as_of),
            );
        }
        if let Some(d) = &self.deal {
            for (f, b) in d.blocks() {
                p.check(
                    b.status != DiligenceStatus::Verified
                        || b.evidence.iter().any(|l| *l != Locator::Unknown),
                    codes::INVALID_FIELD,
                    &format!("deal.{f}"),
                    "VERIFIED names its evidence",
                );
            }
            p.unique_texts("deal.red_flags", d.red_flags.iter().map(String::as_str));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::record::{from_toml, validate};
    use crate::domain::soe::value::ValueError;

    /// A synthetic recurring-automation candidate (example values).
    pub(crate) const RECURRING: &str = r#"
schema = "soe.opportunity/1"
id = "example-automation"
version = 1
as_of = "2026-10-05"
customer = "small accounting firms (example)"
pain = "a new filing format, filled by hand"
mechanism = "AUTOMATE"
alternatives = ["PRODUCTIZE", "HOLD"]
requires_skills = ["integration"]
signals = ["sec_edgar:0000320193-26-000006:0000000000000000000000000000000000000000000000000000000000000001"]

[jurisdictions]
customer = "DE"
operator = "DE"
entity = "UNKNOWN"
delivery = "EU"
tax = "UNKNOWN"
payment = "EU"
data = "EU"

[economics]
currency = "EUR"
variable_cost = { value = { low = 500, base = 800, high = 1500 }, evidence = [], as_of = "2026-10-01" }
fixed_costs_per_month = { value = { low = "40.00", base = "60.00", high = "90.00" }, evidence = [], as_of = "2026-10-01" }
owner_hours_per_month = { value = { low = 7, base = 11, high = 16 }, evidence = [], as_of = "2026-10-01" }
ramp_months = { value = { low = 2, base = 3, high = 5 }, evidence = [], as_of = "2026-10-01" }
revenue_quality = "RECURRING"
tax_review = "UNKNOWN"

[economics.revenue]
kind = "RECURRING"
leads_per_month = { value = { low = 4, base = 7, high = 11 }, evidence = ["url:https://example.org/demand"], as_of = "2026-10-01" }
conversion = { value = { low = 1200, base = 1800, high = 2500 }, evidence = [], as_of = "2026-10-01" }
churn_per_month = { value = { low = 150, base = 300, high = 600 }, evidence = [], as_of = "2026-10-01" }
price_per_month = { value = "UNKNOWN: no public price list", evidence = [], as_of = "2026-10-01" }
collection_loss = { value = { low = 0, base = 150, high = 300 }, evidence = [], as_of = "2026-10-01" }

[economics.initial]
acquisition = { value = { low = "0.00", base = "0.00", high = "0.00" }, evidence = [], as_of = "2026-10-01" }
setup = { value = { low = "400.00", base = "600.00", high = "900.00" }, evidence = [], as_of = "2026-10-01" }
validation = { value = { low = "200.00", base = "250.00", high = "400.00" }, evidence = [], as_of = "2026-10-01" }
working_capital = { value = { low = "0.00", base = "150.00", high = "300.00" }, evidence = [], as_of = "2026-10-01" }

[risk]
risks = []
concentration = []

[risk.max_loss]
cash = { low = "600.00", base = "1100.00", high = "1600.00" }
owner_hours = { low = 20, base = 30, high = 50 }
reputation = "a missed filing for one pilot firm"
dependency = "the filing portal's format"

[risk.bounds]
owner_time = "BOUNDED"
support = "BOUNDED"
payment_access = "BOUNDED"
scope = "BOUNDED"

[ordinal]
defensibility = "MEDIUM"
reversibility = "HIGH"
"#;

    pub(crate) fn recurring() -> Opportunity {
        from_toml(RECURRING).unwrap()
    }

    fn codes_of(r: Result<(), Vec<ValueError>>) -> Vec<&'static str> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.code)
            .collect()
    }

    #[test]
    fn recurring_example_parses_and_round_trips() {
        let o = recurring();
        assert_eq!(o.economics.revenue.kind(), "RECURRING");
        let states = o.economics.input_states();
        assert_eq!(states.len(), 13);
        let price = states
            .iter()
            .find(|s| s.field == "economics.revenue.price_per_month")
            .unwrap();
        assert!(!price.known && !price.evidenced);
        assert!(states[0].known && states[0].evidenced);
        let back: Opportunity = from_toml(&toml::to_string(&o).unwrap()).unwrap();
        assert_eq!(back, o);
        let j = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<Opportunity>(&j).unwrap(), o);
        // An unknown key inside a revenue variant is refused.
        let extra = RECURRING.replace("kind = \"RECURRING\"", "kind = \"RECURRING\"\nupsell = 1");
        let e = from_toml::<Opportunity>(&extra).unwrap_err();
        assert!(e[0].message.contains("unknown field `upsell`"), "{e:?}");
        // A float amount is refused (never money).
        let float = RECURRING.replace("low = \"40.00\"", "low = 40.5");
        let e = from_toml::<Opportunity>(&float).unwrap_err();
        assert!(e[0].message.contains("is a float"), "{e:?}");
        let wrong = RECURRING.replace("kind = \"RECURRING\"", "kind = \"SUBSCRIPTION\"");
        assert!(from_toml::<Opportunity>(&wrong).is_err());
    }

    #[test]
    fn every_input_is_reachable_by_its_field() {
        let mut o = recurring();
        let fields: Vec<String> = o.economics.inputs().into_iter().map(|(f, _)| f).collect();
        assert_eq!(fields.len(), 13);
        for f in &fields {
            let text = o.economics.input(f).unwrap().value_text();
            assert!(o.economics.input_mut(f).is_some(), "{f}");
            assert!(!text.is_empty(), "{f}");
        }
        let price = o
            .economics
            .input("economics.revenue.price_per_month")
            .unwrap();
        assert!(!price.is_known());
        assert_eq!(price.unknown_reason(), Some("no public price list"));
        assert_eq!(price.value_text(), "UNKNOWN: no public price list");
        let leads = o
            .economics
            .input("economics.revenue.leads_per_month")
            .unwrap();
        assert_eq!(leads.value_text(), "4..7..11");
        assert_eq!(leads.evidence().len(), 1);
        // Another model's input, a typo and a bare name are not inputs.
        for bad in [
            "economics.revenue.contract_value",
            "economics.revenue.price",
            "price_per_month",
            "economics.initial",
        ] {
            assert!(o.economics.input(bad).is_none(), "{bad}");
            assert!(o.economics.input_mut(bad).is_none(), "{bad}");
        }
        if let Some(InputMut::Count(h)) = o.economics.input_mut("economics.owner_hours_per_month") {
            h.value = Est::point(40);
        }
        assert_eq!(
            o.economics
                .input("economics.owner_hours_per_month")
                .unwrap()
                .value_text(),
            "40..40..40"
        );
    }

    #[test]
    fn high_ticket_with_recurring_revenue_is_fake_recurring() {
        let mut o = recurring();
        o.mechanism = Mechanism::HighTicketDelivery;
        let e = validate(&o).unwrap_err();
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].code, codes::FAKE_RECURRING);
        assert!(
            e[0].message.starts_with("economics.revenue.kind:"),
            "{}",
            e[0]
        );
        // The same mechanism with one-off revenue is fine.
        let one_off: RevenueModel = toml::from_str(
            r#"
kind = "ONE_OFF"
contract_value = { value = { low = "9000.00", base = "11000.00", high = "14000.00" }, evidence = [], as_of = "2026-10-01" }
win_probability = { value = { low = 2000, base = 3500, high = 4500 }, evidence = [], as_of = "2026-10-01" }
delivery_weeks = { value = { low = 3, base = 4, high = 7 }, evidence = [], as_of = "2026-10-01" }
owner_hours_total = { value = "UNKNOWN", evidence = [], as_of = "2026-10-01" }
collection_loss = { value = { low = 0, base = 200, high = 500 }, evidence = [], as_of = "2026-10-01" }
"#,
        )
        .unwrap();
        o.economics.revenue = one_off;
        assert!(validate(&o).is_ok());
    }

    #[test]
    fn mechanism_needs_its_revenue_model_and_deal() {
        let mut o = recurring();
        o.mechanism = Mechanism::PartnerRevshare;
        assert_eq!(codes_of(validate(&o)), vec![codes::REVENUE_MODEL_MISMATCH]);
        o.mechanism = Mechanism::AcquireTransform;
        assert_eq!(
            codes_of(validate(&o)),
            vec![codes::REVENUE_MODEL_MISMATCH, codes::DEAL_REVIEW_MISSING]
        );
        let block = DiligenceBlock {
            status: DiligenceStatus::SellerClaim,
            evidence: vec![],
            note: None,
        };
        let verified = DiligenceBlock {
            status: DiligenceStatus::Verified,
            evidence: vec![Locator::Unknown],
            note: None,
        };
        o.deal = Some(DealReview {
            identity: verified,
            ownership: block.clone(),
            revenue_proof: block.clone(),
            costs: block.clone(),
            technical: block.clone(),
            distribution: block.clone(),
            legal: block.clone(),
            transition: block,
            red_flags: vec![],
        });
        assert_eq!(
            codes_of(validate(&o)),
            vec![codes::REVENUE_MODEL_MISMATCH, codes::INVALID_FIELD]
        );
    }

    #[test]
    fn every_mechanism_round_trips() {
        let names: Vec<String> = Mechanism::ALL
            .iter()
            .map(|m| {
                serde_json::to_value(m)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "ACQUIRE_TRANSFORM",
                "AUTOMATE",
                "INTEGRATE",
                "RESCUE_MIGRATE",
                "PRODUCTIZE",
                "BUILD_ADJACENT",
                "WHITE_LABEL",
                "LICENSE",
                "PARTNER_REVSHARE",
                "HIGH_TICKET_DELIVERY",
                "HOLD",
                "REJECT"
            ]
        );
        for m in Mechanism::ALL {
            let j = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<Mechanism>(&j).unwrap(), m);
            #[derive(Serialize, Deserialize)]
            struct One {
                m: Mechanism,
            }
            let t = toml::to_string(&One { m }).unwrap();
            assert_eq!(toml::from_str::<One>(&t).unwrap().m, m);
        }
        assert!(serde_json::from_str::<Mechanism>("\"AGENCY\"").is_err());
        assert!(serde_json::from_str::<Mechanism>("\"automate\"").is_err());
    }

    #[test]
    fn later_assumption_negative_amount_and_foreign_fx_refused() {
        let mut o = recurring();
        o.economics.ramp_months.as_of = "2026-10-06".parse().unwrap();
        o.economics.initial.setup.value = Est::range(
            "-1.00".parse().unwrap(),
            "0.00".parse().unwrap(),
            "1.00".parse().unwrap(),
        )
        .unwrap();
        o.alternatives.push(Mechanism::Automate);
        o.economics.fx = Some(
            FxRate::new(
                "USD/EUR".parse().unwrap(),
                "0.9",
                Locator::Unknown,
                Time::Unknown,
            )
            .unwrap(),
        );
        assert_eq!(
            codes_of(validate(&o)),
            vec![
                codes::DUPLICATE,
                codes::INVALID_FIELD,
                codes::FUTURE_LEAKAGE,
                codes::INVALID_FIELD
            ]
        );
        // A rate read after the record is look-ahead too.
        let mut late_fx = recurring();
        late_fx.economics.currency = Currency::Usd;
        late_fx.economics.fx = Some(
            FxRate::new(
                "USD/EUR".parse().unwrap(),
                "0.9",
                Locator::Unknown,
                "2026-10-06".parse().unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(codes_of(validate(&late_fx)), vec![codes::FUTURE_LEAKAGE]);
    }
}
