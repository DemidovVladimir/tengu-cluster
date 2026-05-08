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
use crate::adapters::config::McpServerConfig;
use crate::adapters::rag::{registry_metadata, RagKind, RagStore, SkillEntry};
use crate::adapters::types::ToolDef;

/// Counts + loaded inputs returned from a full registry reindex. The CLI
/// uses the spec/entry lists for its per-item listing; the auto-reindex
/// path on chat startup only looks at the counts.
///
/// Phase 6.2 — `unchanged: true` means the workspace fingerprint
/// (sha256 over all agent TOMLs + skill MDs + tool defs) matched the
/// cached value at `<root>/.tengu/registry-fingerprint`, and the heavy
/// reindex (clear + re-embed every entry) was skipped. In that case
/// `*_indexed` counts are 0 — they describe what THIS call wrote, not
/// what's already in the registry. `agent_specs` and `skill_entries`
/// are still populated so the CLI listing still works.
pub struct RegistryReindexed {
    pub tools_indexed: usize,
    pub agents_indexed: usize,
    pub skills_indexed: usize,
    pub agent_specs: Vec<AgentSpec>,
    pub skill_entries: Vec<SkillEntry>,
    pub unchanged: bool,
}

/// Phase 6.2 — relative path inside the workspace where the fingerprint
/// is persisted. Same dotdir other tengu state goes into.
const FINGERPRINT_PATH: &str = ".tengu/registry-fingerprint";

/// Phase 6.2 — env var to bypass the fingerprint check and force a full
/// reindex even when nothing changed. Useful for recovery (e.g. after a
/// botched manual write to Qdrant) or for testing.
const FORCE_REINDEX_ENV: &str = "TENGU_REGISTRY_FORCE_REINDEX";

/// Phase 6.2 — compute a stable workspace fingerprint over every input
/// the indexer would consume, returning a hex sha256.
///
/// What goes in (sorted-by-name for order stability):
/// - Each agent spec serialized as `(name, description, model, joined skills/tools, joined example_queries)`.
/// - Each skill entry serialized as `(name, description)`.
/// - Each tool def (built-in + MCP) serialized as `(name, description)`.
///
/// Order matters for stability: any change to ordering would invalidate
/// every previous fingerprint. The serialization is deliberately tab/
/// newline-separated to make hash collisions across legitimately-different
/// inputs vanishingly unlikely while staying easy to reason about.
fn compute_workspace_fingerprint(
    agent_specs: &[AgentSpec],
    skill_entries: &[SkillEntry],
    tool_defs: &[ToolDef],
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();

    let mut agents: Vec<&AgentSpec> = agent_specs.iter().collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    for a in agents {
        hasher.update(b"AGENT\t");
        hasher.update(a.name.as_bytes());
        hasher.update(b"\t");
        hasher.update(a.description.as_bytes());
        hasher.update(b"\t");
        hasher.update(a.model.as_bytes());
        hasher.update(b"\t");
        hasher.update(a.skills.join(",").as_bytes());
        hasher.update(b"\t");
        hasher.update(a.tools.join(",").as_bytes());
        hasher.update(b"\t");
        hasher.update(a.example_queries.join("|").as_bytes());
        hasher.update(b"\n");
    }

    let mut skills: Vec<&SkillEntry> = skill_entries.iter().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    for s in skills {
        hasher.update(b"SKILL\t");
        hasher.update(s.name.as_bytes());
        hasher.update(b"\t");
        hasher.update(s.description.as_bytes());
        hasher.update(b"\n");
    }

    let mut tools: Vec<&ToolDef> = tool_defs.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    for t in tools {
        hasher.update(b"TOOL\t");
        hasher.update(t.name.as_bytes());
        hasher.update(b"\t");
        hasher.update(t.description.as_bytes());
        hasher.update(b"\n");
    }

    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Phase 6.2 — read the cached fingerprint at `<root>/.tengu/registry-fingerprint`.
/// Returns `None` if the file is missing or unreadable (treated as
/// fingerprint-mismatch downstream).
fn read_cached_fingerprint(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(FINGERPRINT_PATH))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Phase 6.2 — write the new fingerprint, creating parent dirs as needed.
/// Failure is logged and swallowed because the worst case is a redundant
/// reindex on next startup, never a data-correctness issue.
fn write_cached_fingerprint(root: &Path, fingerprint: &str) {
    let path = root.join(FINGERPRINT_PATH);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, path = %path.display(), "fingerprint write skipped: mkdir failed");
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, fingerprint) {
        tracing::warn!(error = %e, path = %path.display(), "fingerprint write skipped: write failed");
    }
}

/// Phase 6.6 — real built-in tool enumeration. Returns the FULL roster of
/// tools compiled into the binary (workspace + memory + cache +
/// persistent_store + skill_distill + http + crypto). We use
/// `compute_bridge_tools` rather than `compute_base_tools` so the
/// registry sees every built-in regardless of whether the active agent
/// has memory or specific workspace_tools opt-ins — the registry's job is
/// to advertise capability, not gate it.
///
/// Replaces the prior `placeholder_tools()` set of 6 hardcoded `ToolDef`s.
/// The `has_memory` flag is `true` so the memory plugin's tools (e.g.
/// `remember`) are included; `workspace_tools` lists every opt-in name so
/// `compute_bridge_tools` includes their tool defs too.
pub fn enumerate_builtin_tools() -> Vec<ToolDef> {
    let workspace_tools = [
        "shared_cache".to_string(),
        "persistent_store".to_string(),
        crate::adapters::plugins::skill_lifecycle::SKILL_DISTILL_TOOL_NAME.to_string(),
    ];
    crate::adapters::channel_runtime::compute_bridge_tools(true, &workspace_tools)
}

/// Phase 6.6 — enumerate tools from every configured external MCP server.
///
/// For each `McpServerConfig`:
/// - dial the server (stdio or HTTP transport, per `McpClient::connect`),
/// - call `tools/list` over JSON-RPC,
/// - flatten each remote tool into a `ToolDef` named `{server}.{tool}`
///   (matching the qualifier the runtime `McpProxyTool` uses, so the
///   registry name and the tool-call name stay in lockstep).
///
/// Fail-soft per server — a misconfigured / unreachable server logs a
/// warning and is skipped, never aborting the whole registry reindex.
/// Returns an empty `Vec` when `servers` is empty.
pub async fn enumerate_mcp_tools(servers: &[McpServerConfig]) -> Vec<ToolDef> {
    use crate::adapters::plugins::mcp::client::{McpCaller, McpClient};

    let mut out: Vec<ToolDef> = Vec::new();
    for cfg in servers {
        let client = match McpClient::connect(cfg).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    error = %e,
                    "rag indexer: MCP server connect failed; skipping (other servers continue)"
                );
                continue;
            }
        };
        let manifest = match client.list_tools().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    error = %e,
                    "rag indexer: MCP tools/list failed; skipping (other servers continue)"
                );
                continue;
            }
        };
        for remote in manifest {
            let qualified = format!("{}.{}", cfg.name, remote.name);
            out.push(ToolDef {
                name: qualified,
                description: remote.description,
                parameters: remote.input_schema,
            });
        }
    }
    out
}

/// Full registry reindex: clear, then index built-in tools + MCP tools (if
/// any servers are configured) + every `agents/<name>.toml` under
/// `<root>/agents/` + every discovered skill in the three-tier scan rooted
/// at `<root>`. Called by both the manual `tengu registry reindex-all` CLI
/// subcommand and the auto-reindex hook on chat startup. Caller decides
/// whether to fail loudly or fail-soft.
///
/// `mcp_servers` is the `Config.mcp_servers` slice. Pass an empty slice
/// (`&[]`) to skip MCP enumeration — Phase 6.6 made this a first-class
/// arg so editing `mcp_servers` in the sandbox config and restarting
/// `tengu chat` automatically refreshes registry MCP entries (same
/// auto-reindex hook that already covers agents/skills).
pub async fn reindex_all_workspace(
    rag: &RagStore,
    root: &Path,
    mcp_servers: &[McpServerConfig],
) -> Result<RegistryReindexed> {
    let agents_dir = root.join("agents");
    let agent_specs = crate::adapters::agents::load_agents_dir(&agents_dir)
        .map_err(|e| anyhow::anyhow!("load agents from {}: {}", agents_dir.display(), e))?;
    let scan = scan_skills_with_counts(root, None);
    let skill_entries = scan.entries;

    // Built-in + MCP tools combined. Built-in is sync; MCP enumeration is
    // async + fail-soft (a dead server doesn't abort reindex).
    let mut tools = enumerate_builtin_tools();
    let mcp_tools = enumerate_mcp_tools(mcp_servers).await;
    tools.extend(mcp_tools);

    // Phase 6.2 — fingerprint-based dedup. Compute a stable hash of every
    // input the indexer would feed Qdrant, compare with the cached value.
    // On match, skip the heavy clear+embed+upsert sequence entirely (the
    // entries are already in tengu_registry from a prior run). On miss,
    // do the full reindex and update the cache. `TENGU_REGISTRY_FORCE_REINDEX=1`
    // bypasses the cache for recovery.
    let fingerprint = compute_workspace_fingerprint(&agent_specs, &skill_entries, &tools);
    let force = matches!(
        std::env::var(FORCE_REINDEX_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("on")
    );
    if !force {
        if let Some(cached) = read_cached_fingerprint(root) {
            if cached == fingerprint {
                tracing::info!(
                    fingerprint = %&fingerprint[..16],
                    root = %root.display(),
                    "rag reindex skipped: workspace fingerprint matches cache (set TENGU_REGISTRY_FORCE_REINDEX=1 to override)"
                );
                return Ok(RegistryReindexed {
                    tools_indexed: 0,
                    agents_indexed: 0,
                    skills_indexed: 0,
                    agent_specs,
                    skill_entries,
                    unchanged: true,
                });
            }
        }
    }

    rag.clear_registry().await?;
    let tools_indexed = rag.index_tools(tools).await?;
    let agents_indexed = rag.index_agents(agent_specs.clone()).await?;
    let skills_indexed = rag.index_skills(skill_entries.clone()).await?;

    tracing::info!(
        total = skills_indexed,
        managed = scan.managed_count,
        workspace = scan.workspace_count,
        project = scan.project_count,
        "reindexed {} skills ({} managed, {} workspace, {} project)",
        skills_indexed,
        scan.managed_count,
        scan.workspace_count,
        scan.project_count
    );

    // Cache write happens AFTER successful reindex so a partial-failure
    // doesn't leave a fingerprint that masks an incomplete registry.
    write_cached_fingerprint(root, &fingerprint);

    Ok(RegistryReindexed {
        tools_indexed,
        agents_indexed,
        skills_indexed,
        agent_specs,
        skill_entries,
        unchanged: false,
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
/// Each agent gets ONE vector for its description; in addition, every entry
/// in `example_queries` is embedded as its OWN vector. All vectors carry the
/// same `(kind=agent, name)` metadata so the search-side dedup in
/// `query::search_registry` collapses them to one row at the requested agent's
/// max score. Decoupling embedded text from stored snippet text lets us:
/// - embed the BARE example string (tight cosine similarity to user queries),
/// - store the FULL description as the snippet so the planner LLM sees the
///   same context regardless of which vector won.
///
/// The previous design joined all examples into a single vector, which meant
/// a query matching one example out of N only matched ~1/N of that vector and
/// scores capped near 0.35. Per-example vectors push real matches into the
/// 0.5–0.7 range — wider gap to non-matches, more decisive routing. See
/// SESSION_HANDOFF.md "Per-example vectors (registry recall, v2)".
pub async fn index_agents(rag: &RagStore, agents: Vec<AgentSpec>) -> Result<usize> {
    let mut count = 0usize;
    for agent in &agents {
        let source = agent.source_path.as_deref().map(|p| p.to_string_lossy().to_string());

        // The "snippet" text — what the planner LLM reads when this agent is
        // surfaced. Stays identical across all of this agent's vectors so
        // dedup-by-(kind,name) doesn't make snippet content score-dependent.
        let snippet = format!("{}\n\n{}", agent.name, agent.description);

        // Vector 1 — description (always written). Embedded text == snippet.
        match rag.embedder().embed(&snippet).await {
            Ok(v) => {
                let meta = registry_metadata(RagKind::Agent, &agent.name, source.as_deref());
                if let Err(e) = rag.registry().write(v, &snippet, meta).await {
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

        // Vectors 2..N — one per example query. Embed the BARE query string
        // (no agent name, no preamble — preserves cosine similarity); store
        // the same snippet so the planner reads the full description on hit.
        for query in &agent.example_queries {
            let trimmed = query.trim();
            if trimmed.is_empty() {
                continue;
            }
            match rag.embedder().embed(trimmed).await {
                Ok(v) => {
                    let meta = registry_metadata(RagKind::Agent, &agent.name, source.as_deref());
                    if let Err(e) = rag.registry().write(v, &snippet, meta).await {
                        tracing::warn!(
                            agent = %agent.name,
                            example = %trimmed,
                            error = %e,
                            "registry write (example) failed"
                        );
                    } else {
                        count += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        agent = %agent.name,
                        example = %trimmed,
                        error = %e,
                        "embed (example) failed; skipping this example"
                    );
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

/// Per-tier counts + entries from a three-tier skill scan. Used internally by
/// `reindex_all_workspace` so the per-tier breakdown can be logged.
pub(crate) struct SkillScan {
    pub entries: Vec<SkillEntry>,
    pub managed_count: usize,
    pub workspace_count: usize,
    pub project_count: usize,
}

/// Scan the three-tier skill hierarchy and return one `SkillEntry` per unique
/// `name`. Higher tiers shadow lower ones using **first-occurrence wins**
/// semantics — same as `skill_builder::FileSystemSkillSource::discover_skill_files`.
/// The tiers, scanned in this order:
///
/// 1. Managed — `~/.tengu/skills/` (highest priority — installed via `tengu skill install --tier managed`)
/// 2. Workspace dotdir — `<workspace>/.tengu/skills/`
/// 3. Project — `<workspace>/skills/` (lowest priority)
///
/// Missing tiers are skipped silently. A SKILL.md with no usable frontmatter
/// `description` is logged and skipped. When the same skill name appears in
/// multiple tiers, the first-seen wins and a `tracing::debug!` line is logged
/// for each shadowed copy.
pub fn scan_skills(workspace: &Path) -> Vec<SkillEntry> {
    scan_skills_with_counts(workspace, None).entries
}

/// Three-tier scan with per-tier counts and an optional injected `managed_root`
/// for tests. When `managed_root` is `None`, the managed tier is resolved via
/// `dirs_next::home_dir().map(|h| h.join(".tengu/skills"))`; if `home_dir()`
/// returns `None`, the managed tier is skipped silently. When `managed_root`
/// is `Some(path)`, that path is used verbatim — no `home_dir()` call. Tests
/// pass a `TempDir` here to avoid touching the real `~/.tengu/skills/`.
pub(crate) fn scan_skills_with_counts(
    workspace: &Path,
    managed_root: Option<&Path>,
) -> SkillScan {
    let mut by_name: std::collections::HashMap<String, SkillEntry> =
        std::collections::HashMap::new();

    let managed: Option<PathBuf> = match managed_root {
        Some(p) => Some(p.to_path_buf()),
        None => dirs_next::home_dir().map(|h| h.join(".tengu").join("skills")),
    };
    let workspace_dir = workspace.join(".tengu").join("skills");
    let project_dir = workspace.join("skills");

    let tiers: [(&str, Option<PathBuf>); 3] = [
        ("managed", managed),
        ("workspace", Some(workspace_dir)),
        ("project", Some(project_dir)),
    ];

    let mut managed_count = 0usize;
    let mut workspace_count = 0usize;
    let mut project_count = 0usize;

    // First-occurrence wins. managed > workspace > project.
    for (label, root_opt) in tiers.into_iter() {
        let root = match root_opt {
            Some(r) => r,
            None => continue,
        };
        if !root.is_dir() {
            continue;
        }
        for entry in scan_one_skill_root(&root) {
            if let Some(existing) = by_name.get(&entry.name) {
                tracing::debug!(
                    skill = %entry.name,
                    shadowed_by = %existing.source_path.display(),
                    shadowed = %entry.source_path.display(),
                    "skill '{}' shadowed by higher-tier copy at {}",
                    entry.name,
                    existing.source_path.display()
                );
                continue;
            }
            match label {
                "managed" => managed_count += 1,
                "workspace" => workspace_count += 1,
                "project" => project_count += 1,
                _ => {}
            }
            by_name.insert(entry.name.clone(), entry);
        }
    }

    let mut out: Vec<SkillEntry> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    SkillScan {
        entries: out,
        managed_count,
        workspace_count,
        project_count,
    }
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

    /// Helper: write `<root>/<skill_name>/SKILL.md` with a minimal valid
    /// frontmatter (name + description). Description is templated so each
    /// fixture is uniquely identifiable in shadowing assertions.
    fn write_skill(root: &Path, skill_name: &str, description: &str) -> PathBuf {
        let dir = root.join(skill_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {}\ndescription: \"{}\"\n---\n\n# Body\n", skill_name, description),
        )
        .unwrap();
        path
    }

    #[test]
    fn scan_skills_walks_all_three_tiers() {
        let ws = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        // Project tier: <ws>/skills/foo
        write_skill(&ws.path().join("skills"), "foo", "project foo");
        // Workspace tier: <ws>/.tengu/skills/bar
        write_skill(&ws.path().join(".tengu/skills"), "bar", "workspace bar");
        // Managed tier: <managed>/baz
        write_skill(managed.path(), "baz", "managed baz");

        let scan = scan_skills_with_counts(ws.path(), Some(managed.path()));
        let names: Vec<&str> = scan.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["bar", "baz", "foo"]); // sorted alphabetically
        assert_eq!(scan.managed_count, 1);
        assert_eq!(scan.workspace_count, 1);
        assert_eq!(scan.project_count, 1);
    }

    #[test]
    fn scan_skills_managed_shadows_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        // Same skill name in BOTH managed and workspace.
        write_skill(managed.path(), "shared", "from managed");
        write_skill(&ws.path().join(".tengu/skills"), "shared", "from workspace");

        let scan = scan_skills_with_counts(ws.path(), Some(managed.path()));
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].name, "shared");
        assert_eq!(scan.entries[0].description, "from managed");
        assert_eq!(scan.managed_count, 1);
        assert_eq!(scan.workspace_count, 0);
        assert_eq!(scan.project_count, 0);
    }

    #[test]
    fn scan_skills_workspace_shadows_project() {
        let ws = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        // No managed; workspace and project share a name.
        write_skill(&ws.path().join(".tengu/skills"), "shared", "from workspace");
        write_skill(&ws.path().join("skills"), "shared", "from project");

        let scan = scan_skills_with_counts(ws.path(), Some(managed.path()));
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].name, "shared");
        assert_eq!(scan.entries[0].description, "from workspace");
        assert_eq!(scan.managed_count, 0);
        assert_eq!(scan.workspace_count, 1);
        assert_eq!(scan.project_count, 0);
    }

    #[test]
    fn scan_skills_handles_missing_managed_dir() {
        let ws = tempfile::tempdir().unwrap();
        let bogus_managed = ws.path().join("does-not-exist");

        write_skill(&ws.path().join("skills"), "foo", "project foo");

        // Managed root points at a path that doesn't exist — must not error.
        let scan = scan_skills_with_counts(ws.path(), Some(&bogus_managed));
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].name, "foo");
        assert_eq!(scan.managed_count, 0);
        assert_eq!(scan.project_count, 1);
    }

    #[test]
    fn scan_skills_handles_missing_home() {
        // Simulate `dirs_next::home_dir()` returning None by passing
        // `Some(<nonexistent path>)` — same code path for "managed tier
        // skipped silently" because `is_dir()` returns false. The "real"
        // None branch (callers pass `managed_root: None` AND `home_dir()`
        // returns None) converges on the same skip — but exercising it
        // directly would touch the real `~/.tengu/skills/`, so we don't.
        let ws = tempfile::tempdir().unwrap();
        write_skill(&ws.path().join("skills"), "foo", "project foo");

        // Path that does not exist — equivalent to home_dir() returning None.
        let nowhere = PathBuf::from("/this/path/should/never/exist/tengu-test");
        let scan = scan_skills_with_counts(ws.path(), Some(&nowhere));
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.managed_count, 0);
    }
}
