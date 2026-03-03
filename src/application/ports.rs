use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tengu_core::types::{Message, ToolCall};

use crate::domain::memory::{MemoryEntry, MemorySearchResult};

/// Port for persistence of flow transcript messages.
pub(crate) trait FlowStorePort: Send + Sync {
    fn load_messages(&self, flow_key: &str, max_messages: usize) -> Result<Vec<Message>>;
    fn append_message(&self, flow_key: &str, agent_id: &str, message: &Message) -> Result<()>;
}

/// Output port for publishing tool activity events to the UI/log layer.
pub(crate) trait ToolActivityPort: Send + Sync {
    fn publish_tool_activity(&self, call: &ToolCall);
}

/// Input port for obtaining user approval before running sensitive tools.
pub(crate) trait ToolApprovalPort: Send + Sync {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool>;
}

/// Output port for executing tool calls against a concrete infrastructure.
pub(crate) trait ToolExecutionPort: Send + Sync {
    fn execute_tool(&self, call: &ToolCall) -> Result<String>;
}

/// Port for discovering skill.md files from the workspace.
pub(crate) trait SkillSourcePort: Send + Sync {
    /// Returns a list of (filename, file_content) pairs for all discovered skill files.
    fn discover_skill_files(&self) -> Vec<(String, String)>;
}

/// Port for executing shell commands in a workspace directory.
pub(crate) trait ShellExecutionPort: Send + Sync {
    fn execute_shell(&self, command: &str, workspace: &std::path::Path) -> Result<String>;
}

/// Port for generating text embeddings via an external model.
pub(crate) trait EmbeddingPort: Send + Sync {
    fn embed(
        &self,
        texts: &[&str],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>>;
}

/// Port for persistent vector memory storage and retrieval.
#[allow(dead_code)]
pub(crate) trait MemoryStorePort: Send + Sync {
    fn store(&self, entry: &MemoryEntry) -> Result<()>;
    fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<MemorySearchResult>>;
    fn delete(&self, id: &str) -> Result<bool>;
    fn entry_count(&self) -> usize;
    fn storage_bytes(&self) -> u64;
}

/// Port for task persistence in orchestration.
#[allow(dead_code)]
pub(crate) trait TaskStorePort: Send + Sync {
    fn save_task(&self, task: &crate::domain::task::Task) -> Result<()>;
    fn load_task(&self, task_id: &str) -> Result<Option<crate::domain::task::Task>>;
    fn load_tasks_by_status(
        &self,
        status: crate::domain::task::TaskStatus,
    ) -> Result<Vec<crate::domain::task::Task>>;
    fn load_all_tasks(&self) -> Result<Vec<crate::domain::task::Task>>;
}
