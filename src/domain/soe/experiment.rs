//! Experiment spec — the cheapest decisive test of one opportunity (PRD § 6
//! `ExperimentSpec`, § 9): a metric with a threshold, a deadline, an owner-time
//! cap, a stop rule, the approvals each step needs and staged cash. It lives
//! inside the opportunity in O1; O5 registers a live one as a lineage
//! experiment (environment `EXTERNAL`) — not here.
//!
//! | Field | Value |
//! |---|---|
//! | `hypothesis`, `metric`, `stop_rule` | text, non-empty |
//! | `[threshold]` | `op` (`GE` `LE`), `value` (integer), `unit` |
//! | `deadline` · `expires_at` | known times; the deadline not after the expiry |
//! | `max_owner_hours` | hours over the whole test |
//! | `approvals[]` | [`ApprovalKind`]: `READ_SOURCE` `ADD_SOURCE` `CONTACT` `PUBLISH` `SPEND` `SIGN` `DEPLOY` `PERSONAL_DATA` (PRD § 9), no repeats |
//! | `[[stages]]` | ≥ 1; `name` (unique), `cash` ≥ 0, `recoverable` 0..=`cash`, `owner_hours`, `stop_rule?` (in the opportunity's `economics.currency`) |
//!
//! The first stage with cash lacking a stop rule is a gate (`NO_STAGED_STOP_RULE`,
//! O1 W4), not a parse error: [`ExperimentSpec::first_spend`] finds it.

// Consumers land with the gates and ranking (O1 W4–W5).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::record::Problems;
use super::value::{codes, Minor, ValueError};
use crate::domain::lineage::value::{Time, TimeOrder};

/// A human approval kind (PRD § 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalKind {
    ReadSource,
    AddSource,
    Contact,
    Publish,
    Spend,
    Sign,
    Deploy,
    PersonalData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ThresholdOp {
    /// The metric must reach at least `value`.
    Ge,
    /// The metric must stay at most `value`.
    Le,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threshold {
    pub op: ThresholdOp,
    pub value: i64,
    pub unit: String,
}

impl Threshold {
    /// Whether `observed` meets the threshold.
    pub fn met(&self, observed: i64) -> bool {
        match self.op {
            ThresholdOp::Ge => observed >= self.value,
            ThresholdOp::Le => observed <= self.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub name: String,
    pub cash: Minor,
    pub recoverable: Minor,
    pub owner_hours: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_rule: Option<String>,
}

/// Module table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    pub hypothesis: String,
    pub metric: String,
    pub threshold: Threshold,
    pub deadline: Time,
    pub expires_at: Time,
    pub max_owner_hours: u32,
    pub stop_rule: String,
    pub approvals: Vec<ApprovalKind>,
    pub stages: Vec<Stage>,
}

impl ExperimentSpec {
    /// Σ stage cash (checked).
    pub fn total_cash(&self) -> Result<Minor, ValueError> {
        self.stages
            .iter()
            .try_fold(Minor::ZERO, |acc, s| acc.checked_add(s.cash))
    }

    /// The first stage that spends cash, with its index.
    pub fn first_spend(&self) -> Option<(usize, &Stage)> {
        self.stages.iter().enumerate().find(|(_, s)| s.cash.0 > 0)
    }

    /// Rules under `at` (`experiment`).
    pub fn problems(&self, at: &str, p: &mut Problems) {
        let f = |x: &str| format!("{at}.{x}");
        p.text(&f("hypothesis"), &self.hypothesis);
        p.text(&f("metric"), &self.metric);
        p.text(&f("stop_rule"), &self.stop_rule);
        p.text(&f("threshold.unit"), &self.threshold.unit);
        p.known(&f("deadline"), &self.deadline);
        p.known(&f("expires_at"), &self.expires_at);
        p.check(
            self.deadline.order(&self.expires_at) != TimeOrder::After,
            codes::INVALID_TIME,
            &f("deadline"),
            format_args!("{} is after expires_at {}", self.deadline, self.expires_at),
        );
        p.unique(&f("approvals"), self.approvals.iter());
        p.check(
            !self.stages.is_empty(),
            codes::INVALID_FIELD,
            &f("stages"),
            "at least one stage",
        );
        p.unique_texts(
            &f("stages.name"),
            self.stages.iter().map(|s| s.name.as_str()),
        );
        for (i, s) in self.stages.iter().enumerate() {
            let g = |x: &str| format!("{at}.stages[{i}].{x}");
            p.non_negative(&g("cash"), s.cash);
            p.non_negative(&g("recoverable"), s.recoverable);
            p.check(
                s.recoverable <= s.cash,
                codes::INVALID_FIELD,
                &g("recoverable"),
                format_args!("{} is above the stage cash {}", s.recoverable, s.cash),
            );
            if let Some(rule) = &s.stop_rule {
                p.text(&g("stop_rule"), rule);
            }
        }
        if self.total_cash().is_err() {
            p.push(codes::OVERFLOW, &f("stages"), "Σ cash overflows");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
hypothesis = "teams on the retired API pay for a migration"
metric = "paid pilots"
threshold = { op = "GE", value = 2, unit = "pilots" }
deadline = "2026-11-15"
expires_at = "2026-11-30"
max_owner_hours = 20
stop_rule = "no pilot by the deadline"
approvals = ["CONTACT", "SPEND"]

[[stages]]
name = "interviews"
cash = 0
recoverable = 0
owner_hours = 5

[[stages]]
name = "prototype"
cash = "750.00"
recoverable = "0.00"
owner_hours = 14
stop_rule = "fewer than 2 pilots signed"
"#;

    #[test]
    fn stages_and_threshold() {
        let s: ExperimentSpec = toml::from_str(SPEC).unwrap();
        assert_eq!(s.total_cash().unwrap(), "750.00".parse().unwrap());
        assert_eq!(
            s.first_spend().map(|(i, st)| (i, st.name.as_str())),
            Some((1, "prototype"))
        );
        assert!(s.threshold.met(2) && !s.threshold.met(1));
        let le = Threshold {
            op: ThresholdOp::Le,
            value: 5,
            unit: "days".into(),
        };
        assert!(le.met(5) && !le.met(6));
        let mut p = Problems::default();
        s.problems("experiment", &mut p);
        assert!(p.is_empty(), "{p:?}");
        let back: ExperimentSpec = toml::from_str(&toml::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);

        let mut bad = s.clone();
        bad.deadline = "2026-12-01".parse().unwrap();
        bad.approvals.push(ApprovalKind::Spend);
        bad.stages[1].recoverable = "800.00".parse().unwrap();
        bad.stages[0].name = "prototype".into();
        let mut p = Problems::default();
        bad.problems("experiment", &mut p);
        let codes: Vec<_> = p
            .into_result()
            .unwrap_err()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(
            codes,
            vec!["invalid_time", "duplicate", "duplicate", "invalid_field"]
        );
        // Unknown keys refused at the stage level too.
        assert!(toml::from_str::<ExperimentSpec>(
            &SPEC.replace("owner_hours = 5", "owner_hours = 5\nrisk = 1")
        )
        .is_err());
        assert!(toml::from_str::<ExperimentSpec>(&SPEC.replace("\"SPEND\"", "\"PAY\"")).is_err());
    }
}
