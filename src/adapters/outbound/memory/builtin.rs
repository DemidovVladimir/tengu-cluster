//! `BuiltinMemoryProvider` — MEMORY.md + identity + daily logs + vector.
//!
//! Always registered first in `MemoryManager`. Cannot be removed.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::adapters::outbound::memory::embedder::Embedder;
use crate::application::memory::fencing::sanitize_context;
use crate::domain::memory::ChunkMetadata;
use crate::ports::memory::MemoryProvider;
use crate::ports::memory::VectorStore;

pub struct BuiltinMemoryProvider {
    // `workspace` and `system_block` are unused on the v2 path (the static
    // pre-load was replaced by RAG queries on demand). They remain wired
    // through the constructor so static-mode sandboxes that still call
    // `load_system_prompt_files()` keep working until Phase 7.1 deletes
    // the static path entirely.
    #[allow(dead_code)]
    workspace: PathBuf,
    store: Arc<dyn VectorStore>,
    embedder: Arc<Embedder>,
    // cached system prompt block, computed during initialize()
    #[allow(dead_code)]
    system_block: RwLock<String>,
}

impl BuiltinMemoryProvider {
    pub fn new(workspace: PathBuf, store: Arc<dyn VectorStore>, embedder: Arc<Embedder>) -> Self {
        Self {
            workspace,
            store,
            embedder,
            system_block: RwLock::new(String::new()),
        }
    }

    #[cfg(test)]
    pub fn new_for_test(workspace: PathBuf) -> Self {
        use crate::adapters::outbound::memory::disk_vector::DiskVectorStore;
        let store: Arc<dyn VectorStore> = Arc::new(DiskVectorStore::in_memory());
        let embedder = Arc::new(Embedder::null());
        Self::new(workspace, store, embedder)
    }

    #[allow(dead_code)]
    async fn load_system_prompt_files(&self) -> String {
        // AGENTS.md, MEMORY.md, identity files, daily logs for today + yesterday.
        // Each file, if present and non-empty, gets a `## <name>` heading and is concatenated.
        let mut out = String::new();
        for name in [
            "AGENTS.md",
            "MEMORY.md",
            "USER.md",
            "IDENTITY.md",
            "PROFILE.md",
            "CONTEXT.md",
        ] {
            if let Ok(body) = tokio::fs::read_to_string(self.workspace.join(name)).await {
                if !body.trim().is_empty() {
                    out.push_str(&format!("## {}\n\n{}\n\n", name, body.trim()));
                }
            }
        }
        // Daily logs: today + yesterday
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        for day in [yesterday, today] {
            let p = self.workspace.join(format!(".tengu/memory/{}.md", day));
            if let Ok(body) = tokio::fs::read_to_string(&p).await {
                if !body.trim().is_empty() {
                    out.push_str(&format!("## Daily log {}\n\n{}\n\n", day, body.trim()));
                }
            }
        }
        out
    }
}

#[async_trait]
impl MemoryProvider for BuiltinMemoryProvider {
    fn name(&self) -> &str {
        "builtin"
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn initialize(&self, _session_id: &str, _workspace: &Path) -> anyhow::Result<()> {
        let block = self.load_system_prompt_files().await;
        *self.system_block.write().await = block;
        Ok(())
    }

    fn system_prompt_block(&self) -> String {
        self.system_block
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    async fn prefetch(&self, agent: &str, query: &str) -> String {
        let embedding = match self.embedder.embed(query).await {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        let filter = ChunkMetadata {
            agent: Some(agent.to_string()),
            ..Default::default()
        };
        let hits = match self.store.search(&embedding, 5, Some(&filter)).await {
            Ok(h) => h,
            Err(_) => return String::new(),
        };
        if hits.is_empty() {
            return String::new();
        }
        hits.iter()
            .map(|h| format!("- (score {:.2}) {}", h.score, sanitize_context(&h.text)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn sync_turn(&self, agent: &str, user: &str, assistant: &str) {
        let summary = format!("Q: {}\nA: {}", user.trim(), assistant.trim());
        let Ok(embedding) = self.embedder.embed(&summary).await else {
            return;
        };
        let metadata = ChunkMetadata {
            agent: Some(agent.to_string()),
            kind: Some("turn".to_string()),
            ..Default::default()
        };
        let _ = self.store.write(embedding, &summary, metadata).await;
    }

    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    async fn setup_workspace() -> (TempDir, BuiltinMemoryProvider) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join(".tengu/memory"))
            .await
            .unwrap();
        fs::write(root.join("MEMORY.md"), "# Project memory\n\nfacts")
            .await
            .unwrap();
        fs::write(root.join("AGENTS.md"), "# Agents\n\nhello")
            .await
            .unwrap();
        let provider = BuiltinMemoryProvider::new_for_test(root.clone());
        provider.initialize("sess-1", &root).await.unwrap();
        (tmp, provider)
    }

    #[tokio::test]
    async fn system_prompt_block_contains_memory_md() {
        let (_tmp, provider) = setup_workspace().await;
        let block = provider.system_prompt_block();
        assert!(block.contains("facts"));
        assert!(block.contains("AGENTS"));
    }

    #[tokio::test]
    async fn name_is_builtin() {
        let tmp = TempDir::new().unwrap();
        let provider = BuiltinMemoryProvider::new_for_test(tmp.path().into());
        assert_eq!(provider.name(), "builtin");
    }

    #[tokio::test]
    async fn prefetch_empty_store_returns_empty() {
        let (_tmp, provider) = setup_workspace().await;
        let out = provider.prefetch("researcher", "anything").await;
        assert_eq!(out, "");
    }
}
