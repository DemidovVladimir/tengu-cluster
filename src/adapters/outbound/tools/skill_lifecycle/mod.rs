//! Skill-lifecycle plugin — registers the `skill_distill` LLM-callable tool.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod apply_improver_proposal;
pub mod compress_and_store;
pub(crate) mod distill;

#[allow(unused_imports)]
pub(crate) use apply_improver_proposal::APPLY_IMPROVER_PROPOSAL_TOOL_NAME;
#[allow(unused_imports)]
pub(crate) use distill::{SkillDistillTool, SKILL_DISTILL_TOOL_NAME};

pub(crate) struct SkillLifecyclePlugin;

#[async_trait]
impl ToolPlugin for SkillLifecyclePlugin {
    fn name(&self) -> &'static str {
        "skill_lifecycle"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        // Both tools register here; the workspace_tools allowlist gating in
        // `register_catalog` / the `tools` list decides which the agent sees.
        Ok(vec![
            Arc::new(SkillDistillTool::new()),
            apply_improver_proposal::make_tool(),
        ])
    }
}

/// Tool defs for `skill_distill` only — gated on `workspace_tools = ["skill_distill"]`.
pub(crate) fn distill_tool_defs() -> Vec<ToolDef> {
    vec![SkillDistillTool::new().definition().clone()]
}

/// Tool defs for `apply_improver_proposal` only — gated on
/// `workspace_tools = ["apply_improver_proposal"]`.
pub(crate) fn apply_improver_tool_defs() -> Vec<ToolDef> {
    vec![apply_improver_proposal::tool_def()]
}
