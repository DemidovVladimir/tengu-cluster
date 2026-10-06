//! Generation — `lineage/generations/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, § 4, handoff § 33–34; roadmap P1 / P5): a frozen, measurable
//! configuration of Tengu (W1, W2-CANDIDATE …).
//!
//! | Field | Value |
//! |---|---|
//! | `status` | `CANDIDATE` `FROZEN` `ACTIVE` `REJECTED` `RETIRED`; `FROZEN` needs a known `frozen_at` and a matching `[[frozen]]` lock |
//! | `parent`, `sandboxes[]` | `NONE` or a generation id; the sandbox names that may bind it (`[generation] id`) |
//! | `[code] commit, forward_binary?, forward_binary_sha256?, forward_commit?` | 40 hex (or `UNKNOWN`) |
//! | `[[capabilities]] id, version` | exactly that version of each capability |
//! | `[decision_policy] primary, summary, [[arms]] name, status, summary?` | arm status `BASELINE` `UNPROVEN` `PROVEN` `REJECTED` |
//! | `[research_policy] summary` · `[[models]] role, engine, model, where` | `where` = a pin target |
//! | `[[pins]] target, role, sha256` | role `STRATEGY` `RISK_POLICY` `DECISION_POLICY` `COST_MODEL` `MODEL` `SCHEMA` `CONTRACT` `CONFIG` `RESEARCH_POLICY` |

use serde::{Deserialize, Serialize};

use super::value::{EvidenceRef, PinTarget, Time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GenerationStatus {
    Candidate,
    Frozen,
    Active,
    Rejected,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArmStatus {
    Baseline,
    Unproven,
    Proven,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PinRole {
    Strategy,
    RiskPolicy,
    DecisionPolicy,
    CostModel,
    Model,
    Schema,
    Contract,
    Config,
    ResearchPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Code {
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_binary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRef {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyArm {
    pub name: String,
    pub status: ArmStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionPolicy {
    pub primary: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arms: Vec<PolicyArm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchPolicy {
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    pub role: String,
    pub engine: String,
    pub model: String,
    #[serde(rename = "where")]
    pub location: PinTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub target: PinTarget,
    pub role: PinRole,
    pub sha256: String,
}

/// `lineage/generations/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub status: GenerationStatus,
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandboxes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_policy: Option<DecisionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_policy: Option<ResearchPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}
