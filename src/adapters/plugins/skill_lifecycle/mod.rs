//! Skill-lifecycle plugin — registers the `skill_distill` LLM-callable tool.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::tool_plugin::{PluginCtx, Tool, ToolPlugin};
use crate::adapters::types::ToolDef;

pub(crate) mod distill;

#[allow(unused_imports)]
pub(crate) use distill::{SkillDistillTool, SKILL_DISTILL_TOOL_NAME};

pub(crate) struct SkillLifecyclePlugin;

#[async_trait]
impl ToolPlugin for SkillLifecyclePlugin {
    fn name(&self) -> &'static str {
        "skill_lifecycle"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(SkillDistillTool::new())])
    }
}

pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![SkillDistillTool::new().definition().clone()]
}
