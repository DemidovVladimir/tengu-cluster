//! Port traits — interfaces for external dependencies.
//!
//! Concrete implementations live alongside these traits in `src/adapters/`.
//!
//! Sync ports: `ToolActivityPort`, `SkillSourcePort`, `ShellExecutionPort`.
//!
//! The legacy `EmbeddingPort` / `MemoryStorePort` async traits were removed
//! in the memory-service-port migration — the harness memory stack now
//! lives in `crate::adapters::memory::vector::{Embedder, VectorStore}`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::adapters::types::ToolCall;

/// Output port for publishing tool activity events to the UI/log layer.
pub(crate) trait ToolActivityPort: Send + Sync {
    fn publish_tool_activity(&self, call: &ToolCall);
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

// ---------------------------------------------------------------------------
// ToolScope — default-deny, per-tool access control
// ---------------------------------------------------------------------------

/// Fine-grained scope for tool execution. Default-deny: every field empty
/// means the tool can do nothing. Config must grant access explicitly.
///
/// This type is defined by Phase 0 and consumed by Phase A's `ToolCtx`.
/// Subject to refinement during Phase A if additional fields are needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ToolScope {
    /// Allowed filesystem roots. Every fs-touching tool must reject
    /// paths that, after canonicalization, do not start with one of these.
    /// Empty = no fs access.
    #[serde(default)]
    pub fs_roots: Vec<PathBuf>,

    /// Allowed outbound host patterns (exact or glob: "api.linear.app",
    /// "*.anthropic.com"). Empty = no network access.
    #[serde(default)]
    pub net_hosts: Vec<String>,

    /// Env var names the tool is allowed to read (via SecretVault $VAR refs).
    /// Empty = no env reads.
    #[serde(default)]
    pub env_reads: Vec<String>,

    /// For run_command: allowed binary names (basename match).
    /// Empty = no shell execution.
    #[serde(default)]
    pub shell_bins: Vec<String>,

    /// For crypto tools: allowed wallet labels.
    /// Empty = no crypto access.
    #[serde(default)]
    pub wallets: Vec<String>,
}

#[allow(dead_code)] // Phase A wires these into ToolCtx
impl ToolScope {
    pub(crate) fn check_fs_read(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "read")
    }

    pub(crate) fn check_fs_write(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "write")
    }

    pub(crate) fn check_net_host(&self, host: &str) -> anyhow::Result<()> {
        for pattern in &self.net_hosts {
            // `"*"` is an allow-any wildcard — used by the A1 migration-window
            // `permissive_scope` to preserve pre-Phase-A behaviour where http
            // access was ungated. Mirrors the `check_shell_bin` wildcard.
            // TODO(Phase B): drop once per-agent net_hosts land.
            if pattern == "*" {
                return Ok(());
            }
            if pattern == host {
                return Ok(());
            }
            if let Some(suffix) = pattern.strip_prefix("*.") {
                if let Some(prefix) = host.strip_suffix(suffix) {
                    if prefix.ends_with('.') && prefix.len() > 1 {
                        return Ok(());
                    }
                }
            }
        }
        anyhow::bail!(
            "host '{}' not in allowed net_hosts {:?}",
            host,
            self.net_hosts
        )
    }

    pub(crate) fn check_env_read(&self, var: &str) -> anyhow::Result<()> {
        if self.env_reads.iter().any(|v| v == var) {
            return Ok(());
        }
        anyhow::bail!(
            "env var '{}' not in allowed env_reads {:?}",
            var,
            self.env_reads
        )
    }

    /// Check whether `bin` is allowed by this scope's `shell_bins` list.
    ///
    /// `"*"` is an allow-any wildcard — used by the A1 migration-window
    /// `permissive_scope` to preserve pre-Phase-A behaviour where shell access
    /// was ungated. Real per-agent shell allow-lists arrive with Phase B.
    ///
    /// Callers must guard against empty-string `bin` values themselves
    /// (`run_command` rejects empty commands before reaching this check).
    pub(crate) fn check_shell_bin(&self, bin: &str) -> anyhow::Result<()> {
        let basename = Path::new(bin)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(bin);
        if self.shell_bins.iter().any(|b| b == "*" || b == basename) {
            return Ok(());
        }
        anyhow::bail!(
            "binary '{}' not in allowed shell_bins {:?}",
            bin,
            self.shell_bins
        )
    }

    pub(crate) fn check_wallet(&self, label: &str) -> anyhow::Result<()> {
        if self.wallets.iter().any(|w| w == label) {
            return Ok(());
        }
        anyhow::bail!(
            "wallet '{}' not in allowed wallets {:?}",
            label,
            self.wallets
        )
    }

    fn check_fs(&self, path: &Path, op: &str) -> anyhow::Result<PathBuf> {
        if self.fs_roots.is_empty() {
            anyhow::bail!(
                "fs {} denied for '{}': no fs_roots configured (default-deny)",
                op,
                path.display()
            );
        }

        let canonical = self.canonicalize_with_walkup(path)?;

        for root in &self.fs_roots {
            let canonical_root = if root.exists() {
                root.canonicalize().unwrap_or_else(|_| root.clone())
            } else {
                root.clone()
            };
            if canonical.starts_with(&canonical_root) {
                return Ok(canonical);
            }
        }
        anyhow::bail!(
            "fs {} denied for '{}' (resolved: '{}'): not under any allowed fs_roots {:?}",
            op,
            path.display(),
            canonical.display(),
            self.fs_roots
        )
    }

    fn canonicalize_with_walkup(&self, path: &Path) -> anyhow::Result<PathBuf> {
        if path.exists() {
            return path.canonicalize().map_err(Into::into);
        }

        let mut existing = path.to_path_buf();
        let mut tail_parts: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if existing.exists() {
                let mut result = existing.canonicalize()?;
                for part in tail_parts.into_iter().rev() {
                    result.push(part);
                }
                return Ok(result);
            }
            match existing.file_name() {
                Some(name) => {
                    tail_parts.push(name.to_os_string());
                    existing.pop();
                }
                None => {
                    return Ok(path.to_path_buf());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn fs_allowed_root_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_disallowed_rejects() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().join("allowed")],
            ..Default::default()
        };
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "hello").unwrap();
        assert!(scope.check_fs_read(&outside).is_err());
    }

    #[test]
    fn fs_subdir_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let sub = root.join("deep").join("nested");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("file.txt");
        fs::write(&file, "data").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_traversal_rejects() {
        let tmp = TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let scope = ToolScope {
            fs_roots: vec![allowed],
            ..Default::default()
        };
        let escaped = tmp.path().join("allowed").join("..").join("outside.txt");
        fs::write(tmp.path().join("outside.txt"), "data").unwrap();
        assert!(scope.check_fs_read(&escaped).is_err());
    }

    #[test]
    fn fs_nonexistent_leaf_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        let new_file = root.join("new-doc.md");
        assert!(scope.check_fs_write(&new_file).is_ok());
    }

    #[test]
    fn fs_write_same_logic_as_read() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_write(&file).is_ok());
    }

    #[test]
    fn net_exact_match() {
        let scope = ToolScope {
            net_hosts: vec!["api.linear.app".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.linear.app").is_ok());
    }

    #[test]
    fn net_wildcard_match() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.anthropic.com").is_ok());
    }

    #[test]
    fn net_wildcard_no_bare() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("anthropic.com").is_err());
    }

    #[test]
    fn net_empty_rejects_all() {
        let scope = ToolScope::default();
        assert!(scope.check_net_host("anything.com").is_err());
    }

    #[test]
    fn net_wildcard_allows_any_host() {
        let scope = ToolScope {
            net_hosts: vec!["*".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.example.com").is_ok());
        assert!(scope.check_net_host("raw-host").is_ok());
    }

    #[test]
    fn env_allowed_passes() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("API_KEY").is_ok());
    }

    #[test]
    fn env_unlisted_rejects() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("SECRET_TOKEN").is_err());
    }

    #[test]
    fn shell_basename_match() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("/usr/bin/git").is_ok());
        assert!(scope.check_shell_bin("git").is_ok());
    }

    #[test]
    fn shell_unlisted_rejects() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("rm").is_err());
    }

    #[test]
    fn shell_wildcard_allows_any_binary() {
        let scope = ToolScope {
            shell_bins: vec!["*".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("git").is_ok());
        assert!(scope.check_shell_bin("rm").is_ok());
        // Empty-string guarding lives in `run_command` (step 2), not in the
        // scope check — the wildcard accepts any input here.
        assert!(scope.check_shell_bin("").is_ok());
    }

    #[test]
    fn wallet_exact_match() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("research-wallet").is_ok());
    }

    #[test]
    fn wallet_unlisted_rejects() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("prod-wallet").is_err());
    }

    #[test]
    fn default_denies_all() {
        let scope = ToolScope::default();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("any.txt");
        fs::write(&file, "data").unwrap();

        assert!(scope.check_fs_read(&file).is_err());
        assert!(scope.check_fs_write(&file).is_err());
        assert!(scope.check_net_host("any.com").is_err());
        assert!(scope.check_env_read("ANY").is_err());
        assert!(scope.check_shell_bin("any").is_err());
        assert!(scope.check_wallet("any").is_err());
    }
}
