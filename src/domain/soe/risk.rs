//! Risk assessment of one opportunity (PRD § 6 `RiskAssessment`, § 7.1,
//! § 7.2): the maximum loss, each risk with a probability range and an impact
//! range, concentration and what bounds owner time, support, payment access
//! and scope. An unknown is written `"UNKNOWN"` and stays one — the gates
//! (O1 W4) hold on it, never read it as safe.
//!
//! | Field | Value |
//! |---|---|
//! | `[max_loss]` | `cash` (`Est` money, in the opportunity's `economics.currency`), `owner_hours` (`Est` hours), `reputation`, `dependency` (text; `"UNKNOWN"` = unstated) |
//! | `[[risks]]` | `kind` (`CASH` `TIME` `LEGAL` `SANCTIONS` `LICENSING` `DATA` `PAYMENT` `TRANSFERABILITY` `DEPENDENCY` `KEY_PERSON` `PLATFORM` `CONCENTRATION`), `probability` (`Est` bps), `impact` (`Est` money), `control`, `status` (`RESOLVED` `UNRESOLVED` `UNKNOWN`) |
//! | `[[concentration]]` | `dimension` (`CLIENT` `PLATFORM` `CHANNEL` `SUPPLIER` `JURISDICTION`), `name`, `share` (bps); one row per (dimension, name) |
//! | `[bounds]` | `owner_time`, `support`, `payment_access`, `scope`: `BOUNDED` `UNBOUNDED` `UNKNOWN` |

// Consumers land with the economics and gates (O1 W3–W4).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::record::{stated, Problems};
use super::value::{Bps, Est, Minor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskKind {
    Cash,
    Time,
    Legal,
    Sanctions,
    Licensing,
    Data,
    Payment,
    Transferability,
    Dependency,
    KeyPerson,
    Platform,
    Concentration,
}

impl RiskKind {
    /// Legality, sanctions, licensing, data rights or transferability: an
    /// unresolved one holds the candidate (PRD § 7.2).
    pub fn is_legal(self) -> bool {
        matches!(
            self,
            RiskKind::Legal
                | RiskKind::Sanctions
                | RiskKind::Licensing
                | RiskKind::Data
                | RiskKind::Transferability
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskStatus {
    Resolved,
    Unresolved,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConcentrationDimension {
    Client,
    Platform,
    Channel,
    Supplier,
    Jurisdiction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Boundedness {
    Bounded,
    Unbounded,
    Unknown,
}

/// The downside in cash, time, reputation and dependency terms (PRD § 7.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxLoss {
    pub cash: Est<Minor>,
    pub owner_hours: Est<u32>,
    pub reputation: String,
    pub dependency: String,
}

impl MaxLoss {
    /// The terms not stated (unknown), in field order.
    pub fn unstated(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.cash.is_known() {
            out.push("cash");
        }
        if !self.owner_hours.is_known() {
            out.push("owner_hours");
        }
        if !stated(&self.reputation) {
            out.push("reputation");
        }
        if !stated(&self.dependency) {
            out.push("dependency");
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Risk {
    pub kind: RiskKind,
    pub probability: Est<Bps>,
    pub impact: Est<Minor>,
    pub control: String,
    pub status: RiskStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Concentration {
    pub dimension: ConcentrationDimension,
    pub name: String,
    pub share: Bps,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub owner_time: Boundedness,
    pub support: Boundedness,
    pub payment_access: Boundedness,
    pub scope: Boundedness,
}

impl Bounds {
    /// `(field, value)` in field order.
    pub fn each(&self) -> [(&'static str, Boundedness); 4] {
        [
            ("owner_time", self.owner_time),
            ("support", self.support),
            ("payment_access", self.payment_access),
            ("scope", self.scope),
        ]
    }
}

/// Module table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskAssessment {
    pub max_loss: MaxLoss,
    pub risks: Vec<Risk>,
    pub concentration: Vec<Concentration>,
    pub bounds: Bounds,
}

impl RiskAssessment {
    /// The largest concentration share; `None` when none is listed.
    pub fn concentration_max(&self) -> Option<Bps> {
        self.concentration.iter().map(|c| c.share).max()
    }

    /// Rules under `at` (`risk`).
    pub fn problems(&self, at: &str, p: &mut Problems) {
        if let Est::Range { low, .. } = &self.max_loss.cash {
            p.non_negative(&format!("{at}.max_loss.cash"), *low);
        }
        for (i, r) in self.risks.iter().enumerate() {
            let f = |x: &str| format!("{at}.risks[{i}].{x}");
            p.text(&f("control"), &r.control);
            if let Est::Range { low, .. } = &r.impact {
                p.non_negative(&f("impact"), *low);
            }
        }
        for (i, c) in self.concentration.iter().enumerate() {
            p.text(&format!("{at}.concentration[{i}].name"), &c.name);
        }
        p.unique(
            &format!("{at}.concentration"),
            self.concentration
                .iter()
                .map(|c| (c.dimension, c.name.as_str())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RISK: &str = r#"
[max_loss]
cash = { low = "0.00", base = "1500.00", high = "2500.00" }
owner_hours = "UNKNOWN: no delivery estimate yet"
reputation = "one client relationship"
dependency = "UNKNOWN"

[[risks]]
kind = "LICENSING"
probability = { low = 150, base = 500, high = 1500 }
impact = { low = "0.00", base = "800.00", high = "4000.00" }
control = "terms review before the first sale"
status = "UNRESOLVED"

[[concentration]]
dimension = "PLATFORM"
name = "example-marketplace"
share = 7500

[bounds]
owner_time = "BOUNDED"
support = "UNKNOWN"
payment_access = "BOUNDED"
scope = "UNBOUNDED"
"#;

    #[test]
    fn unknown_terms_stay_unknown_and_round_trip() {
        let r: RiskAssessment = toml::from_str(RISK).unwrap();
        assert_eq!(r.max_loss.unstated(), vec!["owner_hours", "dependency"]);
        assert!(r.risks[0].kind.is_legal() && !RiskKind::Platform.is_legal());
        assert_eq!(r.concentration_max(), Some(Bps::new(7500).unwrap()));
        assert_eq!(r.bounds.each()[3], ("scope", Boundedness::Unbounded));
        let back: RiskAssessment = toml::from_str(&toml::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let mut p = Problems::default();
        r.problems("risk", &mut p);
        assert!(p.is_empty());
        // Unknown keys are refused; a share above 100 % too.
        assert!(toml::from_str::<RiskAssessment>(&format!("{RISK}\nnote = \"x\"")).is_err());
        assert!(toml::from_str::<RiskAssessment>(&RISK.replace("7500", "10001")).is_err());
        let mut dup = r.clone();
        dup.concentration.push(dup.concentration[0].clone());
        dup.risks[0].control = " ".into();
        let mut p = Problems::default();
        dup.problems("risk", &mut p);
        let codes: Vec<_> = p
            .into_result()
            .unwrap_err()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(codes, vec!["invalid_field", "duplicate"]);
    }
}
