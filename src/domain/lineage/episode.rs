//! Experience episode — `lineage/episodes/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, handoff § 22–24, § 37, § 71; roadmap P4): context + hypothesis +
//! information + decision + action + outcome + quality. Decision quality and
//! outcome are stored apart, so a lucky bad decision and a good unlucky one
//! are both representable ([`Quadrant`], derived, never stored).
//!
//! | Section | Fields |
//! |---|---|
//! | top | `kind` (`STRATEGY` `REJECTED_STRATEGY` `OPERATIONAL_INCIDENT` `NO_ACTION`), `generation`, `experiment?`, `family?`, `variant?`, `incidents[]` |
//! | `[context]` | `summary`, `as_of`, `[[context.facts]] key, value, ref?` |
//! | `[hypothesis]?` | `statement`, `family?` |
//! | `[[information]]` | `item`, `available_at`, `evidence`, `provenance` — each known and ≤ `decided_at` (`future_leakage`) |
//! | `[[alternatives]]` | `action`, `chosen`, `counterfactual?`, `counterfactual_net_usd?`, `counterfactual_ref?`, `counterfactual_valid` — exactly one chosen (at most one for `NO_ACTION` / `OPERATIONAL_INCIDENT`) |
//! | `[decision]` | `action`, `decided_at` (known; `context.as_of` ≤ it ≤ `action.executed_at` — `future_leakage`), `policy` (`DETERMINISTIC` `JEV_GATE` `ARCHITECT` `HOLD`), `risk_verdict?` |
//! | `[action]?` · `[outcome]?` | `executed`, `executed_at?`, `evidence?` · `economic?`, `net_usd?`, `net_bps?`, `operational?` |
//! | `[quality]` | `decision` (`SUPPORTED` `UNSUPPORTED` `UNKNOWN`), `decision_note`, `execution` (`CLEAN` `DEGRADED` `FAILED` `NOT_APPLICABLE`), `execution_note?`, `outcome` (`FAVORABLE` `UNFAVORABLE` `NEUTRAL` `UNKNOWN`), `attribution[]` (handoff § 71) |
//! | `[lesson]?` | `text`, `status` (`PROPOSED` `ACCEPTED` `REJECTED`), `affects[]` (`record:<kind>/<id>`) |
//!
//! | Quadrant (decision × outcome) | `SUPPORTED` | `UNSUPPORTED` |
//! |---|---|---|
//! | `FAVORABLE` | `GOOD_DECISION_GOOD_OUTCOME` | `BAD_DECISION_GOOD_OUTCOME` (lucky bad) |
//! | `UNFAVORABLE` | `GOOD_DECISION_BAD_OUTCOME` (good unlucky) | `BAD_DECISION_BAD_OUTCOME` |
//! | anything `UNKNOWN` / `NEUTRAL` | `UNKNOWN` | `UNKNOWN` |

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::experiment::Policy;
use super::value::{EvidenceRef, Locator, Time};
use crate::domain::evidence::Provenance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EpisodeKind {
    Strategy,
    RejectedStrategy,
    OperationalIncident,
    NoAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionQuality {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionQuality {
    Clean,
    Degraded,
    Failed,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutcomeQuality {
    Favorable,
    Unfavorable,
    Neutral,
    Unknown,
}

/// Why performance came out as it did (handoff § 71).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Attribution {
    Information,
    Analysis,
    Memory,
    Composition,
    Decision,
    Risk,
    Execution,
    Infrastructure,
    RandomOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LessonStatus {
    Proposed,
    Accepted,
    Rejected,
}

/// Decision quality × outcome (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Quadrant {
    GoodDecisionGoodOutcome,
    GoodDecisionBadOutcome,
    BadDecisionGoodOutcome,
    BadDecisionBadOutcome,
    Unknown,
}

impl Quadrant {
    pub fn name(&self) -> &'static str {
        match self {
            Quadrant::GoodDecisionGoodOutcome => "GOOD_DECISION_GOOD_OUTCOME",
            Quadrant::GoodDecisionBadOutcome => "GOOD_DECISION_BAD_OUTCOME",
            Quadrant::BadDecisionGoodOutcome => "BAD_DECISION_GOOD_OUTCOME",
            Quadrant::BadDecisionBadOutcome => "BAD_DECISION_BAD_OUTCOME",
            Quadrant::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub key: String,
    pub value: Value,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<Locator>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub summary: String,
    pub as_of: Time,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeHypothesis {
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
}

/// What was known, and when it became available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Information {
    pub item: String,
    pub available_at: Time,
    pub evidence: Locator,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alternative {
    pub action: String,
    pub chosen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterfactual: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterfactual_net_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterfactual_ref: Option<Locator>,
    pub counterfactual_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub action: String,
    pub decided_at: Time,
    pub policy: Policy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_verdict: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionTaken {
    pub executed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Locator>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quality {
    pub decision: DecisionQuality,
    pub decision_note: String,
    pub execution: ExecutionQuality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_note: Option<String>,
    pub outcome: OutcomeQuality,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lesson {
    pub text: String,
    pub status: LessonStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affects: Vec<Locator>,
}

/// `lineage/episodes/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Episode {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub kind: EpisodeKind,
    pub generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incidents: Vec<String>,
    pub context: Context,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hypothesis: Option<EpisodeHypothesis>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub information: Vec<Information>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<Alternative>,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionTaken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub quality: Quality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<Lesson>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}

/// Module table: decision quality × outcome — the one rule every episode
/// shape reads (this record, the SOE `OpportunityEpisode`).
pub fn quadrant_of(decision: DecisionQuality, outcome: OutcomeQuality) -> Quadrant {
    match (decision, outcome) {
        (DecisionQuality::Supported, OutcomeQuality::Favorable) => {
            Quadrant::GoodDecisionGoodOutcome
        }
        (DecisionQuality::Supported, OutcomeQuality::Unfavorable) => {
            Quadrant::GoodDecisionBadOutcome
        }
        (DecisionQuality::Unsupported, OutcomeQuality::Favorable) => {
            Quadrant::BadDecisionGoodOutcome
        }
        (DecisionQuality::Unsupported, OutcomeQuality::Unfavorable) => {
            Quadrant::BadDecisionBadOutcome
        }
        _ => Quadrant::Unknown,
    }
}

impl Episode {
    /// Module table: decision quality × outcome ([`quadrant_of`]).
    pub fn quadrant(&self) -> Quadrant {
        quadrant_of(self.quality.decision, self.quality.outcome)
    }
}
