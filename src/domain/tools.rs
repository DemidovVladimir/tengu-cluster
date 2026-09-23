//! Names of the opt-in workspace tools — the values `[agents.<name>]`
//! `workspace_tools` (or `tools`) may use to switch one on. Config validation
//! checks against `WORKSPACE_TOOLS`; the tool catalog
//! (`adapters/outbound/tools/mod.rs`) gates registration on the same names,
//! and its tests fail if the two drift.

pub(crate) const AGENTIC_MEMORY: &str = "agentic_memory";
pub(crate) const SHARED_CACHE: &str = "shared_cache";
pub(crate) const PERSISTENT_STORE: &str = "persistent_store";
pub(crate) const SKILL_DISTILL: &str = "skill_distill";
pub(crate) const APPLY_IMPROVER_PROPOSAL: &str = "apply_improver_proposal";
pub(crate) const MANAGE_SKILL: &str = "manage_skill";

/// Every opt-in workspace tool name.
pub(crate) const WORKSPACE_TOOLS: &[&str] = &[
    AGENTIC_MEMORY,
    SHARED_CACHE,
    PERSISTENT_STORE,
    SKILL_DISTILL,
    APPLY_IMPROVER_PROPOSAL,
    MANAGE_SKILL,
];
