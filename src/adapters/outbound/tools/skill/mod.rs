// src/adapters/outbound/tools/skill/mod.rs
//! Skill plugin — dispatch for shell skills.
//!
//! Documentation / API skills render into the system prompt via
//! `skills::registry::active_context_fragments` — the skill plugin does NOT
//! touch that path. Only shell skills produce LLM-callable tools, and those
//! all share the single [`SkillShellTool`] implementation: at construction
//! time we read the active shell skills out of the [`SkillRegistry`] and
//! materialise one `Arc<SkillShellTool>` per skill, each carrying its own
//! `ToolDef` + command template.
//!
//! Because `SkillRegistry` lives outside `PluginCtx`, the plugin captures
//! the required definitions when it is built (mirroring how
//! `CryptoPlugin::new(cancel)` and `MemoryPlugin::new(...)` capture their
//! construction inputs). `channel_runtime::build_tool_executor` is the sole
//! caller today.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::application::skills::registry::{SkillExecution, SkillRegistry};
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod shell_tool;

pub(crate) use shell_tool::SkillShellTool;

/// One entry per active shell skill: the ToolDef we advertise to the LLM
/// plus the command template we render at execute time.
struct ShellSkillEntry {
    def: ToolDef,
    template: String,
}

/// Plugin that owns shell-skill dispatch.
pub(crate) struct SkillPlugin {
    shell_skills: Vec<ShellSkillEntry>,
}

impl SkillPlugin {
    /// Build a plugin instance from the registry's current active shell skills.
    ///
    /// Only skills whose execution is `SkillExecution::Shell` contribute tools.
    /// API and documentation skills continue to flow through the system-prompt
    /// path and are intentionally ignored here.
    ///
    /// We pair the registry's `active_tools()` (ToolDefs) with the execution
    /// templates from `active_skill_definitions()` by name, so the LLM-visible
    /// parameter schema stays identical to the pre-A8 behaviour — we never
    /// rebuild the ToolDef ourselves.
    pub(crate) fn from_registry(registry: &SkillRegistry) -> Self {
        let templates: HashMap<String, String> = registry
            .active_skill_definitions()
            .into_iter()
            .filter_map(|skill| match skill.execution {
                SkillExecution::Shell { template } => Some((skill.name, template)),
                SkillExecution::Api(_) | SkillExecution::Documentation => None,
            })
            .collect();

        let shell_skills = registry
            .active_tools()
            .into_iter()
            .filter_map(|def| {
                templates
                    .get(&def.name)
                    .cloned()
                    .map(|template| ShellSkillEntry { def, template })
            })
            .collect();

        Self { shell_skills }
    }
}

#[async_trait]
impl ToolPlugin for SkillPlugin {
    fn name(&self) -> &'static str {
        "skill"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(self.shell_skills.len());
        for entry in &self.shell_skills {
            tools.push(Arc::new(SkillShellTool::new(
                entry.def.clone(),
                entry.template.clone(),
            )));
        }
        Ok(tools)
    }
}
