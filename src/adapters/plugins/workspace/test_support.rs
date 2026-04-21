// src/adapters/plugins/workspace/test_support.rs
//! Shared test harness for workspace tool unit tests.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolScope};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::tool_plugin::ToolCtx;
use crate::adapters::types::ToolCall;

/// No-op activity port for tests.
pub(crate) struct NoopActivity;

impl ToolActivityPort for NoopActivity {
    fn publish_tool_activity(&self, _call: &ToolCall) {}
}

/// Self-contained test fixtures that own every long-lived port used by a
/// `ToolCtx`. Borrow the cx via `ctx()` inside a single test function.
pub(crate) struct TestHarness {
    pub workspace: PathBuf,
    pub scope: ToolScope,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub http: reqwest::Client,
    pub secrets: SecretRegistry,
    pub activity: Arc<dyn ToolActivityPort>,
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
            activity: Arc::new(NoopActivity),
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
            activity: Arc::new(NoopActivity),
        }
    }

    pub(crate) fn ctx(&self) -> ToolCtx<'_> {
        ToolCtx {
            workspace: &self.workspace,
            scope: &self.scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            memory: None,
            secret_registry: &self.secrets,
            activity: self.activity.as_ref(),
            conversation: crate::adapters::tool_plugin::ConversationView::empty(),
        }
    }
}
