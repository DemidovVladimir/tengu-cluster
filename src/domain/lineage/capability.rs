//! Capability — `lineage/capabilities/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, handoff § 31–32; roadmap P5): a decision-relevant capability, never
//! a helper and never a parameter variant (that is a variant).
//!
//! | Field | Value |
//! |---|---|
//! | `class` | `INTELLIGENCE` `CAPITAL` `RISK` `INFRASTRUCTURE` |
//! | `version` | ≥ 1; a generation names `{id, version}` and must find exactly that version |
//! | `permission` | `READ_ONLY` `RESEARCH` `ADVISORY` `PAPER` `LIVE` `RISK_AUTHORITY` |
//! | `lifecycle` | `DISCOVERED` → `CANDIDATE` → `HISTORICALLY_TESTED` → `FORWARD_PAPER` → `APPROVED` → `ACTIVE` → `DEMOTED` → `RETIRED` |
//! | `contract` | a pin target: its typed contract (`tool_schema:<tool>`, `repo:<file>` …) |
//! | `bindings[]` | `tool:<name>` · `strategy_kind:<kind>` — each owned by one capability (`binding_conflict`); what a bound sandbox may use (`GenerationScope`, `generation.rs`) |

use serde::{Deserialize, Serialize};

use super::value::{Binding, EvidenceRef, PinTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityClass {
    Intelligence,
    Capital,
    Risk,
    Infrastructure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Permission {
    ReadOnly,
    Research,
    Advisory,
    Paper,
    Live,
    RiskAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Lifecycle {
    Discovered,
    Candidate,
    HistoricallyTested,
    ForwardPaper,
    Approved,
    Active,
    Demoted,
    Retired,
}

/// `lineage/capabilities/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub class: CapabilityClass,
    pub version: u32,
    pub permission: Permission,
    pub lifecycle: Lifecycle,
    pub contract: PinTarget,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}
