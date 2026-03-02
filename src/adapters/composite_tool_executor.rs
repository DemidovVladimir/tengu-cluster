//! Composite adapter that routes tool calls to the correct executor.

use crate::application::ports::ToolExecutionPort;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;
use tengu_core::types::ToolCall;

pub(crate) struct CompositeToolExecutionAdapter {
    workspace_executor: Arc<dyn ToolExecutionPort>,
    skill_executor: Option<Arc<dyn ToolExecutionPort>>,
    skill_names: HashSet<String>,
    memory_executor: Option<Arc<dyn ToolExecutionPort>>,
    memory_names: HashSet<String>,
}

impl CompositeToolExecutionAdapter {
    pub(crate) fn new(
        workspace_executor: Arc<dyn ToolExecutionPort>,
        skill_executor: Option<Arc<dyn ToolExecutionPort>>,
        skill_names: HashSet<String>,
    ) -> Self {
        Self {
            workspace_executor,
            skill_executor,
            skill_names,
            memory_executor: None,
            memory_names: HashSet::new(),
        }
    }

    pub(crate) fn with_memory_executor(
        mut self,
        executor: Arc<dyn ToolExecutionPort>,
        names: HashSet<String>,
    ) -> Self {
        self.memory_executor = Some(executor);
        self.memory_names = names;
        self
    }
}

impl ToolExecutionPort for CompositeToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        if self.memory_names.contains(&call.name) {
            if let Some(ref executor) = self.memory_executor {
                return executor.execute_tool(call);
            }
        }
        if self.skill_names.contains(&call.name) {
            if let Some(ref executor) = self.skill_executor {
                return executor.execute_tool(call);
            }
        }
        self.workspace_executor.execute_tool(call)
    }
}
