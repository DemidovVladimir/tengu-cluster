//! Composite adapter that routes tool calls to the correct executor.

use crate::adapters::ports::ToolExecutionPort;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;
use crate::adapters::types::ToolCall;

pub(crate) struct CompositeToolExecutionAdapter {
    executors: Vec<(Arc<dyn ToolExecutionPort>, HashSet<String>)>,
    default_executor: Arc<dyn ToolExecutionPort>,
}

impl CompositeToolExecutionAdapter {
    pub(crate) fn new(default_executor: Arc<dyn ToolExecutionPort>) -> Self {
        Self {
            executors: Vec::new(),
            default_executor,
        }
    }

    pub(crate) fn with_executor(
        mut self,
        executor: Arc<dyn ToolExecutionPort>,
        names: HashSet<String>,
    ) -> Self {
        self.executors.push((executor, names));
        self
    }
}

impl ToolExecutionPort for CompositeToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        for (executor, names) in &self.executors {
            if names.contains(&call.name) {
                return executor.execute_tool(call);
            }
        }
        self.default_executor.execute_tool(call)
    }
}
