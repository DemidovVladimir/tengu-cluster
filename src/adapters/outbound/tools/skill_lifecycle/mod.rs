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

/// Element schema of every `metrics` array (`skill_distill`,
/// `apply_improver_proposal`, `manage_skill`): one `MetricSpec`
/// (`application/skills/lifecycle/metrics.rs`). Fields depend on `kind` and
/// the engine schema subset has no `oneOf` (`tools/schema_lint.rs`), so the
/// description names them; serde validates at call time.
pub(crate) fn metric_spec_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "MetricSpec {kind, name, min_pass_rate?} plus the kind's fields: \
                        shell_check {cmd, expect_exit_code?, expect_stdout_matches?} | \
                        llm_judge {rubric_file, judge_model?} | \
                        tool_assertion {tool, action, key?, assert} | script {path} | \
                        dialog_replay {from_message_index, delegate_metric, expected_outcome?} | \
                        description_trigger {queries_file, judge_model?, runs_per_query?, holdout?}"
    })
}
