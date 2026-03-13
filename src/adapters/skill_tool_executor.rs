//! Adapter that executes skill-based tools via shell commands.

use crate::application::ports::{ShellExecutionPort, ToolExecutionPort};
use crate::domain::skill::{render_command, SkillDefinition, SkillExecution};
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tengu_core::types::ToolCall;

pub(crate) struct SkillToolExecutionAdapter {
    skills: HashMap<String, SkillDefinition>,
    shell: Arc<dyn ShellExecutionPort>,
    workspace: PathBuf,
}

impl SkillToolExecutionAdapter {
    pub(crate) fn new(
        skill_defs: Vec<SkillDefinition>,
        shell: Arc<dyn ShellExecutionPort>,
        workspace: PathBuf,
    ) -> Self {
        let skills = skill_defs
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        Self {
            skills,
            shell,
            workspace,
        }
    }
}

impl ToolExecutionPort for SkillToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        let skill = self
            .skills
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown skill: {}", call.name))?;
        let template = match &skill.execution {
            SkillExecution::Shell { template } => template,
            SkillExecution::Api(_) => {
                return Err(anyhow::anyhow!(
                    "API skill '{}' cannot execute through the shell adapter",
                    call.name
                ))
            }
        };
        let command = render_command(template, &call.arguments)?;
        tracing::info!(skill = %call.name, command = %command, "Executing skill tool");
        let result = self.shell.execute_shell(&command, &self.workspace);
        match &result {
            Ok(output) => tracing::info!(
                skill = %call.name,
                output_len = output.len(),
                "Skill tool executed"
            ),
            Err(e) => tracing::warn!(skill = %call.name, error = %e, "Skill tool failed"),
        }
        result
    }
}
