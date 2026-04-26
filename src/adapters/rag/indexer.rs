//! Embed + upsert into `tengu_registry`.
//!
//! Phase 1 indexed only `ToolDef`s (and cleared the registry first). Phase 2
//! adds two more indexers — [`index_agents`] and [`index_skills`] — and
//! splits `clear` out as a separate step so the caller can wipe once then
//! write all three categories.
//!
//! None of these functions implement content-hash dedup yet; that lands in
//! a later phase (see `docs/IMPLEMENTATION_PLAN.md`). Tools/agents/skills are
//! fully re-written each call. The small-N case means this is acceptable.

#![cfg(feature = "qdrant")]

use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::adapters::agents::AgentSpec;
use crate::adapters::rag::{registry_metadata, RagKind, RagStore, SkillEntry};
use crate::adapters::types::ToolDef;

/// Counts + loaded inputs returned from a full registry reindex. The CLI
/// uses the spec/entry lists for its per-item listing; the auto-reindex
/// path on chat startup only looks at the counts.
pub struct RegistryReindexed {
    pub tools_indexed: usize,
    pub agents_indexed: usize,
    pub skills_indexed: usize,
    pub agent_specs: Vec<AgentSpec>,
    pub skill_entries: Vec<SkillEntry>,
}

/// Phase 2 placeholder tool descriptions. Phase 3 (item 6.6 in the handoff)
/// replaces this with a real enumeration of compiled-in + MCP tools.
/// Kept here rather than `main.rs` so the auto-reindex on chat startup
/// (`channel_runtime::build_orchestrator`) can reuse it without bouncing
/// through the binary crate.
pub fn placeholder_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "http_request".to_string(),
            description:
                "Perform an HTTP request (GET/POST/PUT/DELETE). Use for web scraping, \
                 REST API calls, fetching documents. Not for file I/O."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "read_file".to_string(),
            description:
                "Read a file from the local workspace. Returns text content. \
                 Scoped to agent workspace by default."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "list_directory".to_string(),
            description:
                "List the contents of a directory on the local workspace. Returns \
                 filenames and types. Scoped to agent workspace."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "run_command".to_string(),
            description:
                "Run a shell command inside the agent workspace. For builds, tests, \
                 git operations, and other filesystem-local work."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "remember".to_string(),
            description:
                "Store a short fact for cross-session recall via the memory provider. \
                 Use for user preferences and facts that should persist."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "persistent_store".to_string(),
            description:
                "Store, search, list, or delete files against a semantic index. \
                 Use for longer-lived document storage that should be queryable by \
                 natural language."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
    ]
}

/// Full registry reindex: clear, then index placeholder tools + every
/// `agents/<name>.toml` under `<root>/agents/` + every discovered skill in
/// the three-tier scan rooted at `<root>`. Called by both the manual
/// `tengu registry reindex-all` CLI subcommand and the auto-reindex hook
/// on chat startup. Caller decides whether to fail loudly or fail-soft.
pub async fn reindex_all_workspace(rag: &RagStore, root: &Path) -> Result<RegistryReindexed> {
    let agents_dir = root.join("agents");
    let agent_specs = crate::adapters::agents::load_agents_dir(&agents_dir)
        .map_err(|e| anyhow::anyhow!("load agents from {}: {}", agents_dir.display(), e))?;
    let skill_entries = scan_skills(root);

    rag.clear_registry().await?;
    let tools_indexed = rag.index_tools(placeholder_tools()).await?;
    let agents_indexed = rag.index_agents(agent_specs.clone()).await?;
    let skills_indexed = rag.index_skills(skill_entries.clone()).await?;

    Ok(RegistryReindexed {
        tools_indexed,
        agents_indexed,
        skills_indexed,
        agent_specs,
        skill_entries,
    })
}

/// Upsert `ToolDef`s into `tengu_registry` as `kind = tool`. Does not clear.
pub async fn index_tools(rag: &RagStore, tools: Vec<ToolDef>) -> Result<usize> {
    let mut count = 0usize;
    for tool in &tools {
        let text = format!("{}\n\n{}", tool.name, tool.description);
        let vec = match rag.embedder().embed(&text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(tool = %tool.name, error = %e, "embed failed; skipping");
                continue;
            }
        };
        let meta = registry_metadata(RagKind::Tool, &tool.name, None);
        if let Err(e) = rag.registry().write(vec, &text, meta).await {
            tracing::warn!(tool = %tool.name, error = %e, "registry write failed");
            continue;
        }
        count += 1;
    }
    tracing::info!(total = tools.len(), indexed = count, "rag index_tools complete");
    Ok(count)
}

/// Upsert agent specs into `tengu_registry` as `kind = agent`. Does not clear.
///
/// Each agent gets ONE vector for its description; if `example_queries` is
/// non-empty it ALSO gets a second vector built from those examples. Both
/// vectors carry the same `(kind=agent, name)` so search-side dedup picks
/// the higher-scoring one. The example-queries vector dramatically improves
/// recall for short user questions, since the embedded text now mirrors the
/// kind of phrasing the user actually types.
pub async fn index_agents(rag: &RagStore, agents: Vec<AgentSpec>) -> Result<usize> {
    let mut count = 0usize;
    for agent in &agents {
        let source = agent.source_path.as_deref().map(|p| p.to_string_lossy().to_string());

        // Vector 1 — description (always written).
        let desc_text = format!("{}\n\n{}", agent.name, agent.description);
        match rag.embedder().embed(&desc_text).await {
            Ok(v) => {
                let meta = registry_metadata(RagKind::Agent, &agent.name, source.as_deref());
                if let Err(e) = rag.registry().write(v, &desc_text, meta).await {
                    tracing::warn!(agent = %agent.name, error = %e, "registry write (desc) failed");
                    continue;
                }
                count += 1;
            }
            Err(e) => {
                tracing::warn!(agent = %agent.name, error = %e, "embed (desc) failed; skipping");
                continue;
            }
        }

        // Vector 2 — example queries (only if the agent declared any).
        if !agent.example_queries.is_empty() {
            let ex_body = agent
                .example_queries
                .iter()
                .map(|q| format!("- {}", q))
                .collect::<Vec<_>>()
                .join("\n");
            let ex_text = format!(
                "{}\n\nExample questions this agent answers:\n{}",
                agent.name, ex_body
            );
            match rag.embedder().embed(&ex_text).await {
                Ok(v) => {
                    let meta = registry_metadata(RagKind::Agent, &agent.name, source.as_deref());
                    if let Err(e) = rag.registry().write(v, &ex_text, meta).await {
                        tracing::warn!(agent = %agent.name, error = %e, "registry write (examples) failed");
                    } else {
                        count += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(agent = %agent.name, error = %e, "embed (examples) failed; skipping examples vector");
                }
            }
        }
    }
    tracing::info!(total = agents.len(), indexed = count, "rag index_agents complete");
    Ok(count)
}

/// Upsert skill descriptions into `tengu_registry` as `kind = skill`. Does not clear.
pub async fn index_skills(rag: &RagStore, skills: Vec<SkillEntry>) -> Result<usize> {
    let mut count = 0usize;
    for skill in &skills {
        let text = format!("{}\n\n{}", skill.name, skill.description);
        let vec = match rag.embedder().embed(&text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(skill = %skill.name, error = %e, "embed failed; skipping");
                continue;
            }
        };
        let source = skill.source_path.to_string_lossy().to_string();
        let meta = registry_metadata(RagKind::Skill, &skill.name, Some(&source));
        if let Err(e) = rag.registry().write(vec, &text, meta).await {
            tracing::warn!(skill = %skill.name, error = %e, "registry write failed");
            continue;
        }
        count += 1;
    }
    tracing::info!(total = skills.len(), indexed = count, "rag index_skills complete");
    Ok(count)
}

// -------------------------------------------------------------------------
// Skill discovery — three-tier scanner with shadowing.
// -------------------------------------------------------------------------

/// Fields we extract from a SKILL.md's YAML frontmatter.
#[derive(Debug, Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Scan the three-tier skill hierarchy and return one `SkillEntry` per unique
/// `name`. Later tiers shadow earlier ones. The tiers are:
///
/// 1. Managed — `~/.tengu/skills/` (lowest priority)
/// 2. Workspace dotdir — `<workspace>/.tengu/skills/`
/// 3. Workspace root — `<workspace>/skills/` (highest priority)
///
/// Missing tiers are skipped silently. A SKILL.md with no usable frontmatter
/// `description` is logged and skipped.
pub fn scan_skills(workspace: &Path) -> Vec<SkillEntry> {
    let mut by_name: std::collections::HashMap<String, SkillEntry> =
        std::collections::HashMap::new();

    let tier1 = dirs_next::home_dir().map(|h| h.join(".tengu").join("skills"));
    let tier2 = Some(workspace.join(".tengu").join("skills"));
    let tier3 = Some(workspace.join("skills"));

    // Scan in precedence order — later writes overwrite earlier ones.
    for root in [tier1, tier2, tier3].into_iter().flatten() {
        if !root.is_dir() {
            continue;
        }
        for entry in scan_one_skill_root(&root) {
            by_name.insert(entry.name.clone(), entry);
        }
    }

    let mut out: Vec<SkillEntry> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn scan_one_skill_root(root: &Path) -> Vec<SkillEntry> {
    let mut entries = Vec::new();
    let read_dir = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(root = %root.display(), error = %e, "skill root unreadable");
            return entries;
        }
    };
    for dir_entry in read_dir.flatten() {
        let skill_md = dir_entry.path().join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match parse_skill_md(&skill_md) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {
                tracing::debug!(path = %skill_md.display(), "SKILL.md has no frontmatter; skipping");
            }
            Err(e) => {
                tracing::warn!(path = %skill_md.display(), error = %e, "SKILL.md parse failed");
            }
        }
    }
    entries
}

fn parse_skill_md(path: &Path) -> Result<Option<SkillEntry>> {
    let content = std::fs::read_to_string(path)?;

    // Expect `---\n<yaml>\n---\n<body>`. Anything else → no frontmatter.
    if !content.starts_with("---") {
        return Ok(None);
    }
    let rest = &content[3..];
    let end = match rest.find("\n---") {
        Some(i) => i,
        None => return Ok(None),
    };
    // Trim leading newline after opening `---`.
    let yaml_text = rest[..end].trim_start_matches('\n');
    let fm: SkillFrontmatter = serde_yaml::from_str(yaml_text).unwrap_or_default();

    let name = fm.name.unwrap_or_else(|| {
        // Fallback: parent directory name.
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    let description = match fm.description {
        Some(d) if !d.trim().is_empty() => d,
        _ => return Ok(None),
    };
    Ok(Some(SkillEntry {
        name,
        description,
        source_path: PathBuf::from(path),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_md_reads_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: my-skill\ndescription: \"Does things.\"\n---\n\n# Body",
        )
        .unwrap();
        let entry = parse_skill_md(&skill_md).unwrap().unwrap();
        assert_eq!(entry.name, "my-skill");
        assert_eq!(entry.description, "Does things.");
    }

    #[test]
    fn parse_skill_md_returns_none_without_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        std::fs::write(&skill_md, "# Body only, no frontmatter.").unwrap();
        let entry = parse_skill_md(&skill_md).unwrap();
        assert!(entry.is_none());
    }
}
