//! Experiment — `lineage/experiments/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, roadmap P2): hypothesis → configuration → evidence → result →
//! verdict, referencing the original artifacts, never copying them.
//!
//! | Field | Value |
//! |---|---|
//! | `family`, `variant`, `generation` | ids (`UNKNOWN` / `NONE` allowed) |
//! | `environment` · `kind` | `XLAB` `XMARKET` `EXTERNAL` · `BACKTEST` `HOLDOUT_TEST` `FORWARD_PAPER` `ANALYSIS` |
//! | `registered_at`, `ran_at`, `preregistered` | times; a preregistration is sealed (`locks.toml`) or carries a `prereg` evidence ref |
//! | `policies[]` | `DETERMINISTIC` `JEV_GATE` `ARCHITECT` `HOLD` |
//! | `cost_model`, `cost_pin?` | text; a pin target |
//! | `capabilities[]`, `incidents[]` | ids |
//! | `uncertainty`, `split_by?` | text; `TIME` `INSTRUMENTS` `NONE` |
//! | `[[windows]] role, from, to, scope?, integrity, note?` | `DATA` `DEVELOPMENT` `HOLDOUT` `FORWARD`; `[from, to)` |
//! | `[[results]] label, arm?, class, n, mean_net_bps?, ci95_bps?, net_usd?, t_stat?, evidence, extract?, note?` | `extract` = how `verify --evidence` recomputes it from `evidence` (`arm:<name>`, `arm:<name>/in_sample`, `arm:<name>/holdout`, `gate`, `ledger:<account>` …) |
//! | `[verdict] value, reason, decided_at, decided_by` | `PASS` `FAIL` `INCONCLUSIVE` `NO_GO` `PENDING` `INVALID` |
//! | `validity?`, `limitations[]` | `VALID` `VALID_WITH_LIMITATIONS` `INVALID_FOR_STRATEGY_INFERENCE` `UNRESOLVED` — required for `FORWARD_PAPER` |

use serde::{Deserialize, Serialize};

use super::value::{Count, EvidenceRef, Integrity, Locator, PinTarget, Time};
use crate::domain::evidence::EvidenceClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Environment {
    Xlab,
    Xmarket,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExperimentKind {
    Backtest,
    HoldoutTest,
    ForwardPaper,
    Analysis,
}

/// A decision policy (experiment `policies`, episode `[decision] policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Policy {
    Deterministic,
    JevGate,
    Architect,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SplitBy {
    Time,
    Instruments,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WindowRole {
    Data,
    Development,
    Holdout,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerdictValue {
    Pass,
    Fail,
    Inconclusive,
    NoGo,
    Pending,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Validity {
    Valid,
    ValidWithLimitations,
    InvalidForStrategyInference,
    Unresolved,
}

/// One data / development / holdout / forward window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub role: WindowRole,
    pub from: Time,
    pub to: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub integrity: Integrity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One result figure and where it comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<String>,
    pub class: EvidenceClass,
    pub n: Count,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci95_bps: Option<[f64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_stat: Option<f64>,
    pub evidence: Locator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub value: VerdictValue,
    pub reason: String,
    pub decided_at: Time,
    pub decided_by: String,
}

/// `lineage/experiments/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Experiment {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub family: String,
    pub variant: String,
    pub generation: String,
    pub environment: Environment,
    pub kind: ExperimentKind,
    pub registered_at: Time,
    pub ran_at: Time,
    pub preregistered: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<Policy>,
    pub cost_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_pin: Option<PinTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    pub uncertainty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_by: Option<SplitBy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incidents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<Window>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ResultRow>,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<Validity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}

impl Experiment {
    pub fn windows_of(&self, role: WindowRole) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(move |w| w.role == role)
    }

    /// When its outcome starts to be known (seal rule, `locks.rs`): a
    /// forward experiment's FORWARD window START (outcomes accrue from the
    /// first entry; the earliest when several, UNKNOWN when one start is or
    /// there is none), else `ran_at`.
    pub fn outcome_at(&self) -> Time {
        if self.kind != ExperimentKind::ForwardPaper {
            return self.ran_at;
        }
        let starts: Vec<Time> = self
            .windows_of(WindowRole::Forward)
            .map(|w| w.from)
            .collect();
        if starts.is_empty() || starts.iter().any(|t| !t.is_known()) {
            return Time::Unknown;
        }
        starts
            .into_iter()
            .min_by_key(Time::sort_key)
            .unwrap_or(Time::Unknown)
    }
}
