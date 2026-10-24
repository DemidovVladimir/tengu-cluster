// src/adapters/outbound/tools/workspace/test_support.rs
//! Shared test harness for workspace tool unit tests.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::outbound::shell::LocalShellExecutor;
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
use crate::ports::shell::ShellExecutionPort;
use crate::ports::tool::ToolCtx;

/// Self-contained test fixtures that own every long-lived port used by a
/// `ToolCtx`. Borrow the cx via `ctx()` inside a single test function.
pub(crate) struct TestHarness {
    pub workspace: PathBuf,
    pub scope: ToolScope,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub http: reqwest::Client,
    pub secrets: SecretRegistry,
}

impl TestHarness {
    /// Build a permissive harness: fs_roots = [workspace], shell_bins = ["*"].
    pub(crate) fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            scope: ToolScope {
                fs_roots: vec![workspace.to_path_buf()],
                net_hosts: Vec::new(),
                env_reads: Vec::new(),
                shell_bins: vec!["*".to_string()],
                wallets: Vec::new(),
            },
            shell: Arc::new(LocalShellExecutor::new()),
            http: reqwest::Client::new(),
            secrets: SecretRegistry::new(),
        }
    }

    /// Build a harness with a caller-provided scope (for scope-denied cases).
    pub(crate) fn with_scope(workspace: &Path, scope: ToolScope) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            scope,
            shell: Arc::new(LocalShellExecutor::new()),
            http: reqwest::Client::new(),
            secrets: SecretRegistry::new(),
        }
    }

    pub(crate) fn ctx(&self) -> ToolCtx<'_> {
        ToolCtx {
            workspace: &self.workspace,
            scope: &self.scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            secret_registry: &self.secrets,
            conversation: crate::ports::tool::ConversationView::empty(),
            agent_config: None,
            call_id: None,
        }
    }
}
