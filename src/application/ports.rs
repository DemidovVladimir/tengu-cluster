use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tengu_core::types::{Message, ToolCall};

use crate::domain::evm::{EvmTransactionReceipt, EvmTransactionRequest};
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
///
/// Accepts one or more text strings and returns a vector of f32 embeddings,
/// one per input text. The embedding dimensionality depends on the model
/// (e.g. 1536 for `text-embedding-3-small`). Both `remember` and `recall`
/// operations use the same port instance so query and stored vectors always
/// share the same embedding space.
pub(crate) trait EmbeddingPort: Send + Sync {
    fn embed(
        &self,
        texts: &[&str],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>>;
}

/// Port for persistent vector memory storage and retrieval.
///
/// Two adapters implement this trait:
/// - `DiskVectorMemoryStore` — in-process brute-force cosine similarity with
///   bincode persistence (zero-config default).
/// - `QdrantMemoryStore` — delegates to a Qdrant instance via gRPC for ANN
///   (approximate nearest-neighbor) search (opt-in via `--features qdrant`).
///
/// The `store` method persists a pre-embedded `MemoryEntry` (embedding vector
/// already attached). The `search_by_vector` method accepts a query embedding
/// and returns the top-k closest entries scored by cosine similarity.
#[allow(dead_code)]
pub(crate) trait MemoryStorePort: Send + Sync {
    /// Persist a memory entry (content + pre-computed embedding vector).
    fn store(&self, entry: &MemoryEntry) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Find the `top_k` entries closest to `embedding` by cosine similarity.
    fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<MemorySearchResult>>> + Send + '_>>;

    /// Delete a memory entry by its UUID. Returns `true` if it existed.
    fn delete(&self, id: &str) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>>;

    /// Delete all stored entries, resetting the store to empty.
    fn clear_all(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Total number of stored entries.
    fn entry_count(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>>;

    /// Approximate storage size in bytes (meaningful for disk, returns 0 for remote stores).
    fn storage_bytes(&self) -> Pin<Box<dyn Future<Output = u64> + Send + '_>>;
}

/// Port for EVM wallet signing and transaction submission.
///
/// Implemented by `AlloySigner` (adapter layer, `--features evm`). The port
/// uses only domain types from `crate::domain::evm`, keeping the application
/// layer free from alloy/provider imports.
#[allow(dead_code)]
pub(crate) trait EvmPort: Send + Sync {
    /// Return the wallet's checksummed hex address (sync — pure key derivation).
    fn get_address(&self) -> Result<String>;

    /// Sign an arbitrary message and return the hex-encoded signature.
    fn sign_message(&self, message: &str)
        -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;

    /// Build, sign, send a transaction and wait for the receipt.
    fn send_transaction(
        &self,
        tx: &EvmTransactionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EvmTransactionReceipt>> + Send + '_>>;
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
