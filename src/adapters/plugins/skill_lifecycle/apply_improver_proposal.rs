//! `apply_improver_proposal` — LLM-callable tool used by `skill-improver-inline`
//! to apply an in-chat improvement directly to a skill on disk.
//!
//! This is the in-chat sibling of the CLI `tengu skill evolve` driver. The CLI
//! path parses the improver's JSON output and calls `apply_proposal_to_skill_md`
//! + `apply_proposal_resources` itself; the in-chat path has no such code, so
//! we expose the same helpers as a tool the agent calls directly.
//!
//! Doctrine notes:
//! - Validates `editable_by_learner` flag — refuses on locked skills.
//! - Atomic writes (temp+rename) for SKILL.md AND each resource file.
//! - Audit log line on success.
//! - Cache discipline: changes don't activate in the current conversation
//!   (returned in the success payload so the agent tells the user).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::skill_lifecycle::audit;
use crate::adapters::skill_lifecycle::evolve::{
    apply_proposal_resources, apply_proposal_to_skill_md, is_editable_by_learner, ProposalBody,
    ResourceFile,
};
use crate::adapters::skill_lifecycle::metrics::MetricSpec;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) const APPLY_IMPROVER_PROPOSAL_TOOL_NAME: &str = "apply_improver_proposal";

pub(crate) struct ApplyImproverProposalTool {
    def: ToolDef,
}

#[derive(Deserialize)]
struct Args {
    skill: String,
    body_markdown: String,
    rationale: String,
    #[serde(default)]
    metrics: Option<Vec<MetricSpec>>,
    #[serde(default)]
    resource_additions: Option<Vec<ResourceFile>>,
}

impl ApplyImproverProposalTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                APPLY_IMPROVER_PROPOSAL_TOOL_NAME,
                "Apply an in-chat improvement to a skill: rewrite SKILL.md body, \
                 optionally update metrics, and write any resource_additions \
                 atomically. The harness handles all file I/O; the agent ONLY \
                 calls this tool with the proposal payload. Refuses if the skill \
                 has `editable_by_learner: false` in its frontmatter. Cache \
                 discipline holds: changes activate next session, not in the \
                 current conversation.",
                json!({
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (kebab-case)."
                        },
                        "body_markdown": {
                            "type": "string",
                            "description": "Full new SKILL.md body, EXCLUDING frontmatter."
                        },
                        "rationale": {
                            "type": "string",
                            "description": "1-3 sentence explanation of what changed and why."
                        },
                        "metrics": {
                            "type": "array",
                            "description": "Optional updated metric specs. Omit to keep existing metrics."
                        },
                        "resource_additions": {
                            "type": "array",
                            "description": "Optional new files to drop under skills/<name>/resources/. Each: {path (relative, no `..`), content (utf-8), overwrite? (default false)}."
                        }
                    },
                    "required": ["skill", "body_markdown", "rationale"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for ApplyImproverProposalTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args.clone())
            .map_err(|e| anyhow!("apply_improver_proposal: bad args — {e}"))?;

        validate_skill_name(&args.skill)?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let skill_dir = locate_skill_dir(&args.skill, &cwd)
            .ok_or_else(|| anyhow!("no skill '{}' found in any tier", args.skill))?;
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.is_file() {
            anyhow::bail!(
                "apply_improver_proposal: SKILL.md missing at {}",
                skill_md.display()
            );
        }

        // Opt-in gate. Refuse on `editable_by_learner: false`.
        if !is_editable_by_learner(&skill_md)? {
            anyhow::bail!(
                "apply_improver_proposal: skill '{}' has `editable_by_learner: false` in frontmatter — refused. Set the flag to true if you intended this skill to be learner-editable.",
                args.skill
            );
        }

        let body = ProposalBody {
            body_markdown: args.body_markdown,
            metrics: args.metrics,
            rationale: args.rationale,
            resource_additions: args.resource_additions.clone(),
        };

        // Apply SKILL.md body (atomic temp+rename).
        apply_proposal_to_skill_md(&skill_md, &body)?;

        // Apply resource files (each atomic temp+rename).
        let written: Vec<PathBuf> = match &args.resource_additions {
            Some(adds) if !adds.is_empty() => apply_proposal_resources(&skill_dir, adds)?,
            _ => Vec::new(),
        };

        // Audit log.
        let entry = audit::AuditEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            op: "apply_improver_proposal".to_string(),
            name: args.skill.clone(),
            verdict: None,
            source: Some(format!("in-chat;resources={}", written.len())),
            sha256: None,
        };
        if let Err(e) = audit::append(&cwd, entry) {
            tracing::warn!(error = %e, "audit append failed (non-fatal)");
        }

        Ok(ToolOutput::from(
            json!({
                "skill": args.skill,
                "skill_md_updated": true,
                "resources_written": written
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>(),
                "loaded_in_current_conversation": false,
                "next_step": "Tell the user the changes will take effect on next session start (cache discipline). Suggest they restart the chat to see the updated skill in action.",
            })
            .to_string(),
        ))
    }
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("skill name '{}' length out of range (1..=64)", name);
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        anyhow::bail!(
            "skill name '{}' must start with [a-z0-9]; got '{}'",
            name,
            first
        );
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            anyhow::bail!(
                "skill name '{}' has invalid char '{}' (allowed: a-z, 0-9, -)",
                name,
                c
            );
        }
    }
    Ok(())
}

fn locate_skill_dir(name: &str, cwd: &std::path::Path) -> Option<PathBuf> {
    // Same shadowing order as `rag/indexer.rs::scan_skills`:
    // managed → workspace → project, first match wins.
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(home.join(".tengu/skills").join(name));
    }
    candidates.push(cwd.join(".tengu/skills").join(name));
    candidates.push(cwd.join("skills").join(name));
    candidates.into_iter().find(|p| p.is_dir())
}

pub(crate) fn tool_def() -> ToolDef {
    ApplyImproverProposalTool::new().definition().clone()
}

pub(crate) fn make_tool() -> Arc<dyn Tool> {
    Arc::new(ApplyImproverProposalTool::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &std::path::Path, name: &str, editable: Option<bool>) {
        let skill_dir = dir.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let flag = match editable {
            Some(true) => "editable_by_learner: true\n",
            Some(false) => "editable_by_learner: false\n",
            None => "",
        };
        let body = format!(
            "---\nname: {}\ndescription: test skill\n{}---\n\n# Test\n\nOriginal body.\n",
            name, flag
        );
        std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn validate_skill_name_rejects_garbage() {
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("UPPER").is_err());
        assert!(validate_skill_name("with spaces").is_err());
        assert!(validate_skill_name("../etc").is_err());
    }

    #[test]
    fn validate_skill_name_accepts_kebab() {
        validate_skill_name("spanish-teacher").unwrap();
        validate_skill_name("a1").unwrap();
    }

    #[test]
    fn locate_skill_dir_finds_project_tier() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "demo", Some(true));
        let found = locate_skill_dir("demo", tmp.path());
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("skills/demo"));
    }
}
