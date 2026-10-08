//! Generation — `lineage/generations/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, § 4, handoff § 33–34; roadmap P1 / P5): a frozen, measurable
//! configuration of Tengu (W1, W2-CANDIDATE …), and the scope a sandbox bound
//! to it may use ([`GenerationScope`]).
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
//!
//! | [`GenerationScope`] rule | Refusal |
//! |---|---|
//! | a tool some capability binds (`tool:<name>`) | refused unless one of the generation's capabilities binds it |
//! | an opt-in tool (`domain::tools::WORKSPACE_TOOLS`) no capability binds | refused (closed world) |
//! | any other tool (base tools no capability binds) | allowed |
//! | a strategy kind | allowed only when one of the generation's capabilities binds `strategy_kind:<kind>` |
//!
//! [`GenerationScope::cited_runs`]: every backtest run dir the registry cites
//! (`run:<state>/<run id>` in any record), by state — run-dir retention never
//! prunes one (`application/backtest/run_dir.rs::prune_runs`, lineage D3).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::registry::Registry;
use super::value::{Binding, EvidenceRef, Locator, PinTarget, Time};
use crate::domain::tools::WORKSPACE_TOOLS;

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

/// What a sandbox bound to a generation may use (module table), resolved
/// once at `Config::load` (`config/lineage.rs`) and carried to tools in
/// `SandboxSections::generation`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GenerationScope {
    pub id: String,
    /// The capability ids the generation lists.
    pub capabilities: BTreeSet<String>,
    /// Every `tool:` binding of the registry → the capability owning it.
    pub bound_tools: BTreeMap<String, String>,
    /// Every `strategy_kind:` binding of the registry → its capability.
    pub bound_kinds: BTreeMap<String, String>,
    /// The tools the generation's capabilities bind.
    pub available_tools: BTreeSet<String>,
    /// The strategy kinds the generation's capabilities bind.
    pub available_kinds: BTreeSet<String>,
    /// Every run the registry cites (`run:<state>/<run id>`), by state name:
    /// never pruned by run-dir retention (module doc).
    pub cited_runs: BTreeMap<String, BTreeSet<String>>,
}

impl GenerationScope {
    /// The scope of generation `id` in `registry`; `Err` when it is absent.
    pub fn of(registry: &Registry, id: &str) -> Result<Self, String> {
        let g = registry
            .generations
            .get(id)
            .ok_or_else(|| format!("generation `{id}` is not in the registry"))?;
        let capabilities: BTreeSet<String> = g.capabilities.iter().map(|c| c.id.clone()).collect();
        let mut scope = GenerationScope {
            id: id.to_string(),
            capabilities,
            ..Default::default()
        };
        for cap in registry.capabilities.values() {
            let mine = scope.capabilities.contains(&cap.id);
            for b in &cap.bindings {
                match b {
                    Binding::Tool(t) => {
                        scope
                            .bound_tools
                            .entry(t.clone())
                            .or_insert_with(|| cap.id.clone());
                        if mine {
                            scope.available_tools.insert(t.clone());
                        }
                    }
                    Binding::StrategyKind(k) => {
                        scope
                            .bound_kinds
                            .entry(k.clone())
                            .or_insert_with(|| cap.id.clone());
                        if mine {
                            scope.available_kinds.insert(k.clone());
                        }
                    }
                }
            }
        }
        for u in registry.locator_uses() {
            if let Locator::Run { state, run_id, .. } = u.locator {
                scope
                    .cited_runs
                    .entry(state.clone())
                    .or_default()
                    .insert(run_id.clone());
            }
        }
        Ok(scope)
    }

    /// Why `tool` may not be used under this generation (module table), or
    /// `None`.
    pub fn tool_refusal(&self, tool: &str) -> Option<String> {
        let id = &self.id;
        match self.bound_tools.get(tool) {
            Some(_) if self.available_tools.contains(tool) => None,
            Some(cap) => Some(format!(
                "tool `{tool}` is bound by capability `{cap}`, which generation `{id}` does not include"
            )),
            None if WORKSPACE_TOOLS.contains(&tool) => Some(format!(
                "opt-in tool `{tool}` is bound by no capability of the registry — generation `{id}` \
                 admits no unregistered opt-in tool"
            )),
            None => None,
        }
    }

    /// Why strategy kind `kind` may not run under this generation, or `None`.
    pub fn kind_refusal(&self, kind: &str) -> Option<String> {
        let id = &self.id;
        match self.bound_kinds.get(kind) {
            Some(_) if self.available_kinds.contains(kind) => None,
            Some(cap) => Some(format!(
                "strategy kind `{kind}` is bound by capability `{cap}`, which generation `{id}` \
                 does not include"
            )),
            None => Some(format!(
                "strategy kind `{kind}` is bound by no capability of the registry — generation \
                 `{id}` runs only the kinds its capabilities bind"
            )),
        }
    }
}
