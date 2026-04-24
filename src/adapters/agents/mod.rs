//! Agent spec loader — Phase 2 of the redesign.
//!
//! Each `agents/<name>.toml` file is parsed into an [`AgentSpec`]. The spec
//! carries the metadata the RAG registry indexes (`name` + `description`)
//! plus the runtime configuration the (future) subprocess runner consumes
//! (`model`, `tools`, `skills`, `max_turns`, `timeout_secs`, `sandbox`).
//!
//! Phase 2 only uses the loader. Phase 3 wires `SubprocessRunner` to look
//! up a spec by name.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// v2-style agent specification. File-based replacement for the per-sandbox
/// `[agents.*]` blocks that lived in `sandboxes/*/config.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentSpec {
    /// Agent name. Must match the filename stem (e.g. `researcher.toml`
    /// => `name = "researcher"`). Orchestrator plans reference this.
    pub name: String,

    /// Human-readable description. This field IS what the RAG registry
    /// embeds — write it for semantic search: what the agent handles,
    /// what it is NOT good for.
    pub description: String,

    /// OpenRouter-compatible model slug (e.g. `"openai/gpt-4o"`).
    pub model: String,

    /// Tool allow-list. The runner registers only tools whose name is in
    /// this list (plus `compress_and_store`, which is appended implicitly).
    #[serde(default)]
    pub tools: Vec<String>,

    /// Skills to load into the agent's system prompt at runtime.
    #[serde(default)]
    pub skills: Vec<String>,

    /// Cap on LLM mini-loop turns before `compress_and_store` must be called.
    #[serde(default = "default_agent_max_turns")]
    pub max_turns: u32,

    /// Hard timeout for the subprocess, in seconds.
    #[serde(default = "default_agent_timeout_secs")]
    pub timeout_secs: u64,

    /// Optional persistent workspace. If absent, the runner creates a temp
    /// dir and deletes it after the step.
    #[serde(default)]
    pub sandbox: Option<PathBuf>,

    /// Absolute path of the source file on disk. Set by the loader, never
    /// present in the TOML file itself.
    #[serde(skip_deserializing, default)]
    pub source_path: Option<PathBuf>,
}

fn default_agent_max_turns() -> u32 {
    20
}

fn default_agent_timeout_secs() -> u64 {
    180
}

impl AgentSpec {
    /// Validate the spec. Called by the loader after deserialisation.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("agent spec missing `name`");
        }
        if self.description.trim().is_empty() {
            bail!("agent spec `{}` missing `description`", self.name);
        }
        if self.model.trim().is_empty() {
            bail!("agent spec `{}` missing `model`", self.name);
        }
        Ok(())
    }
}

/// Load a single agent spec from a `.toml` file. Validates and attaches
/// `source_path`.
pub fn load_agent_file(path: &Path) -> Result<AgentSpec> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read agent spec {}", path.display()))?;
    let mut spec: AgentSpec = toml::from_str(&contents)
        .with_context(|| format!("parse agent spec {}", path.display()))?;
    spec.source_path = Some(path.to_path_buf());
    spec.validate()
        .with_context(|| format!("validate agent spec {}", path.display()))?;

    // Warn (don't fail) if filename stem disagrees with `name` — the indexer
    // uses `name`, but a mismatch is usually a copy-paste bug worth surfacing.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if stem != spec.name {
            tracing::warn!(
                path = %path.display(),
                file_stem = stem,
                spec_name = %spec.name,
                "agent spec `name` does not match filename stem"
            );
        }
    }
    Ok(spec)
}

/// Load every `*.toml` file under `dir` as an `AgentSpec`. Missing dir
/// returns an empty vector (not an error) — this is the normal Phase 0/1
/// state before agents exist.
pub fn load_agents_dir(dir: &Path) -> Result<Vec<AgentSpec>> {
    if !dir.is_dir() {
        tracing::debug!(path = %dir.display(), "agents dir absent; returning empty set");
        return Ok(Vec::new());
    }
    let mut specs = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match load_agent_file(&path) {
            Ok(spec) => specs.push(spec),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping invalid agent spec");
            }
        }
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_spec() {
        let toml_src = r#"
            name = "researcher"
            description = "Research things."
            model = "openai/gpt-4o"
        "#;
        let spec: AgentSpec = toml::from_str(toml_src).unwrap();
        assert_eq!(spec.name, "researcher");
        assert_eq!(spec.max_turns, 20);
        assert_eq!(spec.timeout_secs, 180);
        assert!(spec.tools.is_empty());
    }

    #[test]
    fn validate_rejects_empty_description() {
        let toml_src = r#"
            name = "x"
            description = ""
            model = "m"
        "#;
        let spec: AgentSpec = toml::from_str(toml_src).unwrap();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn load_dir_ignores_missing() {
        let specs = load_agents_dir(Path::new("/nonexistent/path/agents")).unwrap();
        assert!(specs.is_empty());
    }
}
