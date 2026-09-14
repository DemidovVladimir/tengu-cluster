//! Agent spec loader — Phase 2 of the redesign.
//!
//! Each `agents/<name>.toml` file is parsed into an [`AgentSpec`]. The spec
//! carries the metadata rendered into `TENGU_PLANNER_REGISTRY.md` for the
//! planner LLM (`name`, `description`, `example_queries`) plus the runtime
//! configuration `tengu run-agent` consumes (`engine`, `model`, `tools`,
//! `skills`, `max_turns`, `timeout_secs`, `sandbox`).
//!
//! Unknown keys are rejected (`deny_unknown_fields`) so a typo such as
//! `workspace_tools = [...]` — which is NOT an `AgentSpec` field; workspace
//! tool opt-ins go in `tools` — fails loudly instead of being silently
//! ignored.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Engines a subagent spec may declare. Mirrors the per-sandbox
/// `[agents.*].engine` allow-list in `config.rs::validate_agent`.
pub const VALID_AGENT_ENGINES: &[&str] = &["openrouter", "claude_code"];

/// v2-style agent specification. File-based replacement for the per-sandbox
/// `[agents.*]` blocks that lived in `sandboxes/*/config.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// Agent name. Must match the filename stem (e.g. `researcher.toml`
    /// => `name = "researcher"`). Orchestrator plans reference this.
    pub name: String,

    /// Human-readable description. Rendered verbatim under the agent's
    /// heading in `TENGU_PLANNER_REGISTRY.md`; the planner LLM reads it to
    /// decide routing, so write it for a reader: what the agent handles,
    /// what it is NOT good for.
    pub description: String,

    /// Optional list of example user questions this agent handles well.
    /// Rendered as an "Example queries" bullet list under the agent's
    /// registry entry so the planner can match a short casual message
    /// ("what is the BTC price?") against a near-identical example line
    /// instead of only the longer formal description. Empty by default.
    #[serde(default)]
    pub example_queries: Vec<String>,

    /// Engine the subagent runs on. Default `"openrouter"` for backward
    /// compatibility — pre-existing agents/*.toml without an `engine` field
    /// continue to use OpenRouter unchanged. Set to `"claude_code"` to run
    /// the agent through the Claude Code CLI engine instead. Phase 7.3.
    #[serde(default = "default_agent_engine")]
    pub engine: String,

    /// Model slug. Format depends on `engine`:
    ///   - `engine = "openrouter"` → OpenRouter slug, e.g. `"openai/gpt-4o"` or
    ///     `"anthropic/claude-sonnet-4-6"`.
    ///   - `engine = "claude_code"` → bare Claude model name, e.g.
    ///     `"claude-sonnet-4-6"` (no `anthropic/` prefix). Empty string → use
    ///     the Claude CLI's default.
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

fn default_agent_engine() -> String {
    "openrouter".to_string()
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
        if !VALID_AGENT_ENGINES.contains(&self.engine.as_str()) {
            bail!(
                "agent spec `{}` has unsupported engine `{}` (expected one of: {})",
                self.name,
                self.engine,
                VALID_AGENT_ENGINES.join(", ")
            );
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
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
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
    fn validate_rejects_unknown_engine() {
        let toml_src = r#"
            name = "x"
            description = "d"
            model = "m"
            engine = "ollama"
        "#;
        let spec: AgentSpec = toml::from_str(toml_src).unwrap();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("unsupported engine `ollama`"));
    }

    #[test]
    fn validate_accepts_both_engines() {
        for engine in VALID_AGENT_ENGINES {
            let toml_src = format!(
                "name = \"x\"\ndescription = \"d\"\nmodel = \"m\"\nengine = \"{}\"\n",
                engine
            );
            let spec: AgentSpec = toml::from_str(&toml_src).unwrap();
            assert!(spec.validate().is_ok(), "engine {engine} should validate");
        }
    }

    /// `workspace_tools` is an `AgentConfig` (sandbox config) field, not an
    /// `AgentSpec` field — pre-`deny_unknown_fields` it was silently dropped.
    #[test]
    fn parse_rejects_unknown_key() {
        let toml_src = r#"
            name = "x"
            description = "d"
            model = "m"
            workspace_tools = []
        "#;
        let err = toml::from_str::<AgentSpec>(toml_src).unwrap_err();
        assert!(
            err.to_string().contains("workspace_tools"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_dir_ignores_missing() {
        let specs = load_agents_dir(Path::new("/nonexistent/path/agents")).unwrap();
        assert!(specs.is_empty());
    }
}
