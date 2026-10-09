//! Operator profile — `<TENGU_HOME>/state/soe/operator.toml`, private and
//! signed (`docs/soe-2026-10-08.md` § 2; PRD § 4, § 7.2, § 14). The
//! operator's parameters live only here: no field has a serde default and the
//! code has no built-in value. The loader (`config/soe.rs`) refuses a missing,
//! unsigned, group/world-readable or in-repo profile.
//!
//! | Field | Value |
//! |---|---|
//! | header | `schema = "soe.operator_profile/1"`, `id`, `version` ≥ 1, `valid_from` (known) |
//! | signature | `signed_by` ([`UNSIGNED`] = a template), `signed_at` (known once signed) |
//! | `synthetic` | `true` = a public test fixture (round values that are not the operator's) |
//! | `currency` | the reporting currency; every money knob is in it |
//! | `profit_basis` · `contribution_basis` | `PRE_TAX` `POST_TAX` · `TIME_ADJUSTED` `CASH` |
//! | money knobs | `max_cash_exposure` > 0 · `max_validation_tranche` > 0 and ≤ `max_cash_exposure` · `min_monthly_contribution` ≥ 0 · `shadow_hourly_rate` ≥ 0 |
//! | count knobs | `max_payback_months` 1..=600 (the payback scan) · `weekly_owner_hours` 1..=168 · `max_one_off_delivery_weeks` ≥ 1 |
//! | lists | `jurisdictions_allow`, `channels_allow`, `languages`, `exclusions`: non-empty entries, no repeats (an empty list allows nothing) |
//! | `public_cadence` | `NONE` `WEEKLY` `MONTHLY` `ON_EVIDENCE` |
//! | `rank_order` | [`RankKey`]s, ≥ 1, no repeats — the only ranking order (ties: id ascending) |
//! | `[[capabilities]]` | [`OperatorCapability`]: `id` (unique), `skill`, `level` (`PROVEN` needs a proof locator), `proof[]`, `capacity_hours_per_week`, `delivery_cost_per_hour` (in `currency`), `dependencies[]`, `as_of`, `valid_until` (not before `as_of`) |
//!
//! No salary field: existing income enters only through `shadow_hourly_rate`
//! (PRD § 4).

use serde::{Deserialize, Serialize};

use super::economics::PAYBACK_SCAN_MONTHS;
use super::record::{Problems, SoeRecord};
use super::value::{codes, Better, Currency, Est, Minor, SchemaTag};
use crate::domain::lineage::value::{Locator, Time, TimeOrder};

/// `signed_by` of a template nobody signed.
pub const UNSIGNED: &str = "UNSIGNED";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProfitBasis {
    PreTax,
    PostTax,
}

/// What the contribution rule compares (`min_monthly_contribution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContributionBasis {
    /// Cash contribution − owner hours × `shadow_hourly_rate`.
    TimeAdjusted,
    Cash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicCadence {
    None,
    Weekly,
    Monthly,
    /// Only when there is useful evidence to show.
    OnEvidence,
}

/// One ranking criterion (PRD § 7.2); the order is the profile's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RankKey {
    EvidenceConfidence,
    TimeAdjustedBase,
    PaybackBase,
    DaysToDecisiveEvidence,
    Reversibility,
    CapabilityFit,
    ConcentrationMax,
    Defensibility,
}

impl RankKey {
    pub const ALL: [RankKey; 8] = [
        RankKey::EvidenceConfidence,
        RankKey::TimeAdjustedBase,
        RankKey::PaybackBase,
        RankKey::DaysToDecisiveEvidence,
        RankKey::Reversibility,
        RankKey::CapabilityFit,
        RankKey::ConcentrationMax,
        RankKey::Defensibility,
    ];

    /// Which way ranks first: payback, days and concentration lower, the
    /// rest higher.
    pub fn better(self) -> Better {
        match self {
            RankKey::PaybackBase | RankKey::DaysToDecisiveEvidence | RankKey::ConcentrationMax => {
                Better::Lower
            }
            _ => Better::Higher,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityLevel {
    /// Delivered before; `proof` points at it.
    Proven,
    Claimed,
    Unknown,
}

/// What the operator can actually deliver (PRD § 6 `Capability`; kept in the
/// private profile, not `lineage/capabilities/` — `docs/soe-2026-10-08.md` § 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCapability {
    pub id: String,
    pub skill: String,
    pub level: CapabilityLevel,
    pub proof: Vec<Locator>,
    pub capacity_hours_per_week: Est<u32>,
    /// In the profile's `currency`.
    pub delivery_cost_per_hour: Est<Minor>,
    pub dependencies: Vec<String>,
    pub as_of: Time,
    pub valid_until: Time,
}

/// `soe.operator_profile/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorProfile {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub valid_from: Time,
    pub signed_by: String,
    pub signed_at: Time,
    pub synthetic: bool,
    pub currency: Currency,
    pub profit_basis: ProfitBasis,
    pub contribution_basis: ContributionBasis,
    pub max_cash_exposure: Minor,
    pub min_monthly_contribution: Minor,
    pub shadow_hourly_rate: Minor,
    pub max_payback_months: u32,
    pub weekly_owner_hours: u32,
    pub max_validation_tranche: Minor,
    pub max_one_off_delivery_weeks: u32,
    pub jurisdictions_allow: Vec<String>,
    pub channels_allow: Vec<String>,
    pub languages: Vec<String>,
    pub exclusions: Vec<String>,
    pub public_cadence: PublicCadence,
    pub rank_order: Vec<RankKey>,
    pub capabilities: Vec<OperatorCapability>,
}

impl OperatorProfile {
    /// Signed: `signed_by` names someone (not [`UNSIGNED`]) and `signed_at`
    /// is known.
    pub fn is_signed(&self) -> bool {
        let by = self.signed_by.trim();
        !by.is_empty() && by != UNSIGNED && self.signed_at.is_known()
    }
}

impl SoeRecord for OperatorProfile {
    const RECORD: &'static str = "operator_profile";

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
        p.known("valid_from", &self.valid_from);
        p.text("signed_by", &self.signed_by);
        if self.signed_by.trim() != UNSIGNED {
            p.known("signed_at", &self.signed_at);
        }
        let positive = |m: Minor| m.0 > 0;
        p.check(
            positive(self.max_cash_exposure),
            codes::INVALID_FIELD,
            "max_cash_exposure",
            "must be > 0",
        );
        p.check(
            positive(self.max_validation_tranche),
            codes::INVALID_FIELD,
            "max_validation_tranche",
            "must be > 0",
        );
        p.check(
            self.max_validation_tranche <= self.max_cash_exposure,
            codes::INVALID_FIELD,
            "max_validation_tranche",
            format_args!(
                "{} is above max_cash_exposure {}",
                self.max_validation_tranche, self.max_cash_exposure
            ),
        );
        p.non_negative("min_monthly_contribution", self.min_monthly_contribution);
        p.non_negative("shadow_hourly_rate", self.shadow_hourly_rate);
        for (field, v) in [
            ("max_payback_months", self.max_payback_months),
            ("weekly_owner_hours", self.weekly_owner_hours),
            (
                "max_one_off_delivery_weeks",
                self.max_one_off_delivery_weeks,
            ),
        ] {
            p.check(v >= 1, codes::INVALID_FIELD, field, "must be ≥ 1");
        }
        // The run-rate month and the payback scan: beyond the scan a payback
        // is NOT_REACHED anyway (and the figures would need that many months).
        p.check(
            self.max_payback_months <= PAYBACK_SCAN_MONTHS,
            codes::INVALID_FIELD,
            "max_payback_months",
            format_args!("at most {PAYBACK_SCAN_MONTHS} (the payback scan)"),
        );
        p.check(
            self.weekly_owner_hours <= 168,
            codes::INVALID_FIELD,
            "weekly_owner_hours",
            "a week has 168 hours",
        );
        for (field, list) in [
            ("jurisdictions_allow", &self.jurisdictions_allow),
            ("channels_allow", &self.channels_allow),
            ("languages", &self.languages),
            ("exclusions", &self.exclusions),
        ] {
            p.unique_texts(field, list.iter().map(String::as_str));
        }
        p.check(
            !self.rank_order.is_empty(),
            codes::INVALID_FIELD,
            "rank_order",
            "lists at least one criterion — there is no built-in order",
        );
        p.unique("rank_order", self.rank_order.iter());
        p.unique("capabilities.id", self.capabilities.iter().map(|c| &c.id));
        for (i, c) in self.capabilities.iter().enumerate() {
            let at = |f: &str| format!("capabilities[{i}].{f}");
            p.id(&at("id"), &c.id);
            p.text(&at("skill"), &c.skill);
            p.check(
                c.level != CapabilityLevel::Proven
                    || c.proof.iter().any(|l| *l != Locator::Unknown),
                codes::INVALID_FIELD,
                &at("proof"),
                "a PROVEN capability names its proof",
            );
            p.unique_texts(
                &at("dependencies"),
                c.dependencies.iter().map(String::as_str),
            );
            if let Est::Range { low, .. } = &c.delivery_cost_per_hour {
                p.non_negative(&at("delivery_cost_per_hour"), *low);
            }
            p.check(
                c.as_of.order(&c.valid_until) != TimeOrder::After,
                codes::INVALID_TIME,
                &at("valid_until"),
                format_args!("{} is before as_of {}", c.valid_until, c.as_of),
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::record::{from_toml, validate};
    use crate::domain::soe::value::ValueError;

    /// The synthetic profile fixture: round values that are not the operator's
    /// (three capabilities: `integration` PROVEN, `automation` CLAIMED,
    /// `migration` PROVEN but expired 2026-06-30).
    pub(crate) const SYNTHETIC: &str =
        include_str!("../../../tests/fixtures/soe/profile.synthetic.toml");

    /// The top-level knobs, each required (no serde default).
    const KNOBS: [&str; 21] = [
        "valid_from",
        "signed_by",
        "signed_at",
        "synthetic",
        "currency",
        "profit_basis",
        "contribution_basis",
        "max_cash_exposure",
        "min_monthly_contribution",
        "shadow_hourly_rate",
        "max_payback_months",
        "weekly_owner_hours",
        "max_validation_tranche",
        "max_one_off_delivery_weeks",
        "jurisdictions_allow",
        "channels_allow",
        "languages",
        "exclusions",
        "public_cadence",
        "rank_order",
        "capabilities",
    ];

    pub(crate) fn synthetic() -> OperatorProfile {
        from_toml(SYNTHETIC).unwrap()
    }

    fn codes_of(r: Result<(), Vec<ValueError>>) -> Vec<&'static str> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.code)
            .collect()
    }

    /// `SYNTHETIC` without the `key = …` entry (multi-line arrays and the
    /// capabilities tables included).
    fn without(key: &str) -> String {
        if key == "capabilities" {
            return SYNTHETIC
                .split("[[capabilities]]")
                .next()
                .unwrap()
                .to_string();
        }
        let mut out = Vec::new();
        let mut skipping = false;
        for line in SYNTHETIC.lines() {
            if line.starts_with(&format!("{key} = ")) {
                skipping = line.ends_with('[');
                continue;
            }
            if skipping {
                skipping = line != "]";
                continue;
            }
            out.push(line);
        }
        out.join("\n")
    }

    #[test]
    fn synthetic_profile_parses_signed_and_round_trips() {
        let p = synthetic();
        assert!(p.synthetic && p.is_signed());
        assert_eq!(p.rank_order, RankKey::ALL.to_vec());
        assert_eq!(p.max_cash_exposure, "20000.00".parse().unwrap());
        let back: OperatorProfile = from_toml(&toml::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<OperatorProfile>(&j).unwrap(), p);
        assert_eq!(RankKey::PaybackBase.better(), Better::Lower);
        assert_eq!(RankKey::TimeAdjustedBase.better(), Better::Higher);
    }

    #[test]
    fn missing_required_knob_fails_parse() {
        for knob in KNOBS {
            let text = without(knob);
            assert_ne!(text, SYNTHETIC, "{knob}: nothing removed");
            let e = from_toml::<OperatorProfile>(&text).unwrap_err();
            assert_eq!(e.len(), 1, "{knob}: {e:?}");
            assert_eq!(e[0].code, "invalid_record", "{knob}");
            assert!(
                e[0].message.contains(&format!("missing field `{knob}`")),
                "{knob}: {}",
                e[0].message
            );
        }
    }

    #[test]
    fn unknown_field_refused() {
        let salary = format!("salary = \"1.00\"\n{SYNTHETIC}");
        let e = from_toml::<OperatorProfile>(&salary).unwrap_err();
        assert!(e[0].message.contains("unknown field `salary`"), "{e:?}");
        let nested = SYNTHETIC.replace("dependencies = []", "dependencies = []\nrate = 1");
        let e = from_toml::<OperatorProfile>(&nested).unwrap_err();
        assert!(e[0].message.contains("unknown field `rate`"), "{e:?}");
        let typo = SYNTHETIC.replace(
            "public_cadence = \"ON_EVIDENCE\"",
            "public_cadence = \"DAILY\"",
        );
        assert!(from_toml::<OperatorProfile>(&typo).is_err());
        let float = SYNTHETIC.replace("\"70.00\"", "70.5");
        assert!(from_toml::<OperatorProfile>(&float).is_err());
    }

    #[test]
    fn knob_rules_and_signature() {
        let mut p = synthetic();
        p.max_validation_tranche = "20000.01".parse().unwrap();
        p.weekly_owner_hours = 169;
        p.max_payback_months = 0;
        p.rank_order = vec![RankKey::PaybackBase, RankKey::PaybackBase];
        p.languages.push(String::new());
        p.capabilities[0].proof = vec![Locator::Unknown];
        p.capabilities[0].valid_until = "2026-08-01".parse().unwrap();
        assert_eq!(
            codes_of(validate(&p)),
            vec![
                "invalid_field",
                "invalid_field",
                "invalid_field",
                "invalid_field",
                "duplicate",
                "invalid_field",
                "invalid_time"
            ]
        );
        let mut empty = synthetic();
        empty.rank_order.clear();
        assert_eq!(codes_of(validate(&empty)), vec!["invalid_field"]);
        // The payback knob stays within the payback scan (the economics size
        // their month series by it): a typo is refused, never allocated.
        let mut long = synthetic();
        long.max_payback_months = PAYBACK_SCAN_MONTHS;
        assert!(validate(&long).is_ok());
        for months in [PAYBACK_SCAN_MONTHS + 1, u32::MAX] {
            long.max_payback_months = months;
            let e = validate(&long).unwrap_err();
            assert_eq!(e.len(), 1, "{e:?}");
            assert_eq!(
                e[0].to_string(),
                "invalid_field: max_payback_months: at most 600 (the payback scan)"
            );
        }

        // A template: UNSIGNED parses (the loader refuses it), unknown time ok.
        let mut t = synthetic();
        t.signed_by = UNSIGNED.into();
        t.signed_at = Time::Unknown;
        assert!(validate(&t).is_ok() && !t.is_signed());
        // A name without a time is neither signed nor valid.
        t.signed_by = "someone".into();
        assert_eq!(codes_of(validate(&t)), vec!["invalid_time"]);
        assert!(!t.is_signed());
    }
}
