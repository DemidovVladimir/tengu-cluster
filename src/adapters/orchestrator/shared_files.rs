//! Planner context shared by planner and subagents.
//!
//! | Artifact | Writer | Reader | Role |
//! |---|---|---|---|
//! | `TENGU_PLANNER_REGISTRY.md` | `ensure_planner_registry` (every planner turn) | `RagPlanner` (in-memory snapshot) | Roster of agents / skills / tools (core + MCP). The file is a debug mirror — the snapshot is built from the in-memory render even when the write fails. |
//! | `AgentIpcInput.plan_state` | `SubprocessRunner::run_step` via `active_plan(session_id)` | `tengu run-agent` | **Source of truth** for the plan a subagent sees. Per-session, set by `replan::drive` on every accepted plan. |
//! | `TENGU_PLAN.md` | `write_plan_state` (every accepted plan, any session) | `run-agent` fallback only (old parents that send no `plan_state`) | Human-readable debug artifact. One global file under cwd — concurrent sessions overwrite each other, which is why it is no longer the IPC path. |
//!
//! Both files are runtime artifacts — gitignored, never hand-edited.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::adapters::config::{AgentConfig, McpServerConfig};
use crate::domain::message::ToolDef;
use crate::domain::plan::Plan;

pub(crate) const PLANNER_REGISTRY_FILE: &str = "TENGU_PLANNER_REGISTRY.md";
pub(crate) const PLAN_STATE_FILE: &str = "TENGU_PLAN.md";

/// In-process plan state keyed by orchestrator `session_id`. Populated by
/// `replan::drive` (via `set_active_plan`) and read by
/// `SubprocessRunner::run_step` so each child gets ITS session's plan over
/// IPC instead of whatever session last wrote `TENGU_PLAN.md`.
static ACTIVE_PLANS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn with_active_plans<R>(f: impl FnOnce(&mut HashMap<String, String>) -> R) -> R {
    let mut guard = ACTIVE_PLANS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Render `plan` and register it as the active plan for `session_id`.
/// Returns the rendered markdown (the same text `write_plan_state` writes).
pub(crate) fn set_active_plan(session_id: &str, phase: &str, plan: &Plan) -> Result<String> {
    let rendered = render_plan_state(phase, plan)?;
    with_active_plans(|m| {
        m.insert(session_id.to_string(), rendered.clone());
    });
    Ok(rendered)
}

/// Rendered plan markdown for `session_id`, if one is active.
pub(crate) fn active_plan(session_id: &str) -> Option<String> {
    with_active_plans(|m| m.get(session_id).cloned())
}

/// Drop the active plan for `session_id`, but only if it is still the
/// `rendered` text the caller registered — a concurrent turn on the same
/// session that has since replaced it is left alone.
pub(crate) fn clear_active_plan(session_id: &str, rendered: &str) {
    with_active_plans(|m| {
        if m.get(session_id).map(String::as_str) == Some(rendered) {
            m.remove(session_id);
        }
    });
}

#[derive(Debug, Clone)]
pub(crate) struct PlannerRegistryEntry {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannerRegistrySnapshot {
    pub prompt_block: String,
    pub entries: Vec<PlannerRegistryEntry>,
}

/// Render the roster (agents + skills + core tools + `mcp_tools`) and return
/// it as a prompt block. Fail-soft on the file write: the planner must never
/// lose its roster because the workspace is read-only — the snapshot is
/// built from the in-memory render either way and the write failure is
/// only logged.
pub(crate) fn ensure_planner_registry(
    workspace: &Path,
    agents: &[(String, AgentConfig)],
    mcp_tools: &[ToolDef],
) -> Result<PlannerRegistrySnapshot> {
    let content = render_registry(workspace, agents, mcp_tools)?;
    let path = workspace.join(PLANNER_REGISTRY_FILE);
    if let Err(e) = std::fs::write(&path, &content) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to write planner registry file; planner continues with in-memory roster"
        );
    }
    Ok(PlannerRegistrySnapshot {
        prompt_block: format!(
            "\n## Planner registry (loaded from `{}`)\n\n{}",
            PLANNER_REGISTRY_FILE, content
        ),
        entries: registry_entries_from_content(&content),
    })
}

/// Wrap rendered plan markdown in the system-prompt block header the
/// subagent sees. `source` names where the text came from (IPC vs file).
pub(crate) fn plan_state_block(rendered: &str, source: &str) -> String {
    if rendered.trim().is_empty() {
        return String::new();
    }
    format!(
        "\n## Current execution plan (loaded from {})\n\n{}",
        source, rendered
    )
}

/// Fallback for `run-agent` children whose parent sent no
/// `AgentIpcInput.plan_state` (old binaries): read the global debug file.
pub(crate) fn read_plan_state_block(workspace: &Path) -> String {
    let path = workspace.join(PLAN_STATE_FILE);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    plan_state_block(&content, &format!("`{}`", PLAN_STATE_FILE))
}

/// Write the human-readable `TENGU_PLAN.md` debug artifact. Not the IPC
/// path — see the module doc; `rendered` is the text `set_active_plan`
/// produced so the file and the IPC field never diverge.
pub(crate) fn write_plan_state(workspace: &Path, rendered: &str) -> Result<()> {
    let path = workspace.join(PLAN_STATE_FILE);
    std::fs::write(&path, rendered).with_context(|| format!("write plan state {}", path.display()))
}

/// Markdown rendering of a plan — the single template behind both the IPC
/// field and the debug file.
pub(crate) fn render_plan_state(phase: &str, plan: &Plan) -> Result<String> {
    let json = serde_json::to_string_pretty(plan)?;
    let mut out = format!("# Tengu Current Plan\n\n- phase: `{}`\n\n", phase);
    out.push_str("## Steps\n\n");
    for step in &plan.steps {
        let deps = if step.depends_on.is_empty() {
            "-".to_string()
        } else {
            step.depends_on
                .iter()
                .map(|d| d.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "- `{}` agent=`{}` depends_on=`{}` goal: {}\n",
            step.id.0, step.agent, deps, step.goal
        ));
        if let Some(compose) = &step.compose {
            out.push_str(&format!(
                "  - compose: base_agent=`{}` skills={:?} tools={:?}\n",
                compose.base_agent, compose.skills, compose.tools
            ));
        }
    }
    out.push_str("\n## JSON\n\n```json\n");
    out.push_str(&json);
    out.push_str("\n```\n");
    Ok(out)
}

/// Subagents = `[agents.<name>]` blocks with a `description`. Sorted by
/// name so the registry (and the planner prompt) is stable across turns.
pub(crate) fn routable_agents(
    agents: &std::collections::HashMap<String, AgentConfig>,
) -> Vec<(String, AgentConfig)> {
    let mut out: Vec<(String, AgentConfig)> = agents
        .iter()
        .filter(|(_, a)| a.description.is_some())
        .map(|(n, a)| (n.clone(), a.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn render_registry(
    workspace: &Path,
    agents: &[(String, AgentConfig)],
    mcp_tools: &[ToolDef],
) -> Result<String> {
    let skills = scan_skill_summaries(workspace);
    let tools = registry_tools(mcp_tools);

    let mut out = String::from("# Tengu Planner Registry\n\n");
    out.push_str("This file is generated from the `[agents.*]` blocks of the active config that carry a `description`, skills, core tool definitions, and the tools of every configured MCP server (`<server>.<tool>`). The planner loads it into every planner turn.\n\n");

    out.push_str("## Agents\n\n");
    if agents.is_empty() {
        out.push_str("_No agents found._\n\n");
    }
    for (name, agent) in agents {
        out.push_str(&format!("### {}\n\n", name));
        out.push_str(&format!("- engine: `{}`\n", agent.engine));
        out.push_str(&format!("- model: `{}`\n", agent.model));
        out.push_str(&format!("- tools: `{}`\n", join_or_dash(&agent.tools)));
        out.push_str(&format!(
            "- skills: `{}`\n",
            join_or_dash(&agent.skill_packages)
        ));
        out.push_str(&format!(
            "\n{}\n\n",
            agent.description.as_deref().unwrap_or("").trim()
        ));
        if !agent.example_queries.is_empty() {
            out.push_str("Example queries:\n");
            for query in &agent.example_queries {
                out.push_str(&format!("- {}\n", query));
            }
            out.push('\n');
        }
    }

    out.push_str("## Skills\n\n");
    if skills.is_empty() {
        out.push_str("_No skills found._\n\n");
    }
    for skill in skills {
        out.push_str(&format!(
            "- `{}`: {} _(source: `{}`)_\n",
            skill.name, skill.description, skill.source
        ));
    }
    out.push('\n');

    out.push_str("## Tools\n\n");
    if tools.is_empty() {
        out.push_str("_No tools found._\n");
    }
    for tool in tools {
        out.push_str(&format!(
            "- `{}`: {}\n",
            tool.name,
            first_line(&tool.description)
        ));
    }
    Ok(out)
}

/// Core tool defs (every workspace-tool opt-in included) followed by the
/// MCP tool defs the planner enumerated for this session.
fn registry_tools(mcp_tools: &[ToolDef]) -> Vec<ToolDef> {
    let workspace_tools = crate::adapters::channel_runtime::WORKSPACE_TOOLS_ALLOWLIST
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut tools =
        crate::adapters::channel_runtime::compute_base_tools(true, true, &workspace_tools);
    tools.extend(mcp_tools.iter().cloned());
    tools
}

/// Enumerate tools from every configured external MCP server (ported from
/// the Phase 6.6 `rag::indexer::enumerate_mcp_tools`).
///
/// For each `McpServerConfig`: dial the server (stdio or HTTP), call
/// `tools/list`, and flatten each remote tool into a `ToolDef` named
/// `{server}.{tool}` — the same qualifier the runtime `McpProxyTool` uses,
/// so the registry name and the tool-call name stay in lockstep.
///
/// Requires a live connection (`McpServerConfig` carries no static tool
/// list). Fail-soft per server: an unreachable server logs a warning and is
/// skipped. Returns an empty `Vec` when `servers` is empty.
pub(crate) async fn enumerate_mcp_tools(servers: &[McpServerConfig]) -> Vec<ToolDef> {
    use crate::adapters::plugins::mcp::client::{McpCaller, McpClient};

    let mut out: Vec<ToolDef> = Vec::new();
    for cfg in servers {
        let client = match McpClient::connect(cfg).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    error = %e,
                    "planner registry: MCP server connect failed; skipping (other servers continue)"
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
                    "planner registry: MCP tools/list failed; skipping (other servers continue)"
                );
                continue;
            }
        };
        for remote in manifest {
            out.push(ToolDef {
                name: format!("{}.{}", cfg.name, remote.name),
                description: remote.description,
                parameters: remote.input_schema,
            });
        }
    }
    out
}

fn registry_entries_from_content(content: &str) -> Vec<PlannerRegistryEntry> {
    let mut entries = Vec::new();
    let mut section = "";
    for line in content.lines() {
        if line == "## Agents" {
            section = "agent";
            continue;
        }
        if line == "## Skills" {
            section = "skill";
            continue;
        }
        if line == "## Tools" {
            section = "tool";
            continue;
        }
        if line.starts_with("## ") {
            section = "";
            continue;
        }
        if section == "agent" {
            if let Some(name) = line.strip_prefix("### ") {
                entries.push(PlannerRegistryEntry {
                    kind: "agent".to_string(),
                    name: name.trim().to_string(),
                });
            }
        } else if section == "skill" || section == "tool" {
            if let Some(rest) = line.strip_prefix("- `") {
                if let Some((name, _)) = rest.split_once('`') {
                    entries.push(PlannerRegistryEntry {
                        kind: section.to_string(),
                        name: name.to_string(),
                    });
                }
            }
        }
    }
    entries
}

#[derive(Debug)]
struct SkillSummary {
    name: String,
    description: String,
    source: String,
}

#[derive(Debug, Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

fn scan_skill_summaries(workspace: &Path) -> Vec<SkillSummary> {
    let mut roots = Vec::new();
    roots.push(workspace.join("skills"));
    roots.push(workspace.join(".tengu/skills"));
    if let Some(home) = dirs_next::home_dir() {
        roots.push(home.join(".tengu/skills"));
    }

    let mut by_name = std::collections::BTreeMap::new();
    for root in roots {
        for skill in scan_one_skill_root(&root) {
            by_name.entry(skill.name.clone()).or_insert(skill);
        }
    }
    by_name.into_values().collect()
}

fn scan_one_skill_root(root: &Path) -> Vec<SkillSummary> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|path| path.is_file())
        .filter_map(|path| parse_skill_summary(&path).ok().flatten())
        .collect()
}

fn parse_skill_summary(path: &Path) -> Result<Option<SkillSummary>> {
    let content = std::fs::read_to_string(path)?;
    let Some(frontmatter) = parse_frontmatter(&content) else {
        return Ok(None);
    };
    let Some(name) = frontmatter.name.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(description) = frontmatter.description.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(SkillSummary {
        name,
        description,
        source: path.to_string_lossy().to_string(),
    }))
}

fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    if !content.starts_with("---\n") {
        return None;
    }
    let rest = &content[4..];
    let end = rest.find("\n---")?;
    serde_yaml::from_str(&rest[..end]).ok()
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plan::{Step, StepId};

    fn plan(step: &str) -> Plan {
        Plan {
            steps: vec![Step {
                id: StepId::new(step),
                agent: "researcher".into(),
                goal: "g".into(),
                depends_on: vec![],
                compose: None,
            }],
        }
    }

    #[test]
    fn active_plan_is_per_session() {
        let a = set_active_plan("sf-sess-a", "active", &plan("a1")).unwrap();
        let b = set_active_plan("sf-sess-b", "active", &plan("b1")).unwrap();
        assert_ne!(a, b);
        assert_eq!(active_plan("sf-sess-a").as_deref(), Some(a.as_str()));
        assert_eq!(active_plan("sf-sess-b").as_deref(), Some(b.as_str()));
        assert!(active_plan("sf-sess-none").is_none());

        // Clearing with a stale rendering leaves a newer registration alone.
        let a2 = set_active_plan("sf-sess-a", "active", &plan("a2")).unwrap();
        clear_active_plan("sf-sess-a", &a);
        assert_eq!(active_plan("sf-sess-a").as_deref(), Some(a2.as_str()));
        clear_active_plan("sf-sess-a", &a2);
        assert!(active_plan("sf-sess-a").is_none());
        clear_active_plan("sf-sess-b", &b);
    }

    #[test]
    fn plan_state_block_wraps_rendered_text() {
        let rendered = render_plan_state("active", &plan("s1")).unwrap();
        assert!(rendered.contains("- `s1` agent=`researcher`"));
        let block = plan_state_block(&rendered, "IPC `plan_state`");
        assert!(block.starts_with("\n## Current execution plan (loaded from IPC `plan_state`)"));
        assert!(block.contains(&rendered));
        assert!(plan_state_block("   ", "x").is_empty());
    }

    #[test]
    fn write_plan_state_mirrors_rendered_text() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render_plan_state("active", &plan("s1")).unwrap();
        write_plan_state(dir.path(), &rendered).unwrap();
        let block = read_plan_state_block(dir.path());
        assert!(block.contains(&rendered));
        assert!(read_plan_state_block(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn registry_lists_core_and_mcp_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = vec![ToolDef {
            name: "beach.search_posts".into(),
            description: "Search Beach.science posts.\nSecond line ignored.".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let snapshot = ensure_planner_registry(dir.path(), &[], &mcp).unwrap();
        assert!(snapshot
            .prompt_block
            .contains("- `beach.search_posts`: Search Beach.science posts."));
        assert!(snapshot
            .entries
            .iter()
            .any(|e| e.kind == "tool" && e.name == "beach.search_posts"));
        assert!(snapshot
            .entries
            .iter()
            .any(|e| e.kind == "tool" && e.name == "http_request"));
        assert!(dir.path().join(PLANNER_REGISTRY_FILE).is_file());
    }

    #[test]
    fn registry_lists_routable_agents_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::adapters::config::Config::default()
            .agents
            .remove("main")
            .unwrap();
        let mut routable = base.clone();
        routable.description = Some("Fetches prices from the web.".into());
        routable.example_queries = vec!["what is the BTC price?".into()];
        routable.tools = vec!["http_request".into()];
        routable.skill_packages = vec!["web-research".into()];
        let mut agents = std::collections::HashMap::new();
        agents.insert("researcher".to_string(), routable);
        agents.insert("planner".to_string(), base); // no description → not listed
        let list = routable_agents(&agents);
        assert_eq!(
            list.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["researcher"]
        );
        let snapshot = ensure_planner_registry(dir.path(), &list, &[]).unwrap();
        let block = &snapshot.prompt_block;
        assert!(block.contains("### researcher"), "{block}");
        assert!(block.contains("- tools: `http_request`"), "{block}");
        assert!(block.contains("- skills: `web-research`"), "{block}");
        assert!(block.contains("Fetches prices from the web."), "{block}");
        assert!(block.contains("- what is the BTC price?"), "{block}");
        assert!(!block.contains("### planner"), "{block}");
        assert!(snapshot
            .entries
            .iter()
            .any(|e| e.kind == "agent" && e.name == "researcher"));
    }

    #[test]
    fn registry_survives_unwritable_workspace() {
        // A regular file as "workspace": `<file>/TENGU_PLANNER_REGISTRY.md`
        // cannot be written, but the snapshot must still carry the roster.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let snapshot = ensure_planner_registry(&not_a_dir, &[], &[]).unwrap();
        assert!(snapshot.prompt_block.contains("## Tools"));
        assert!(!snapshot.entries.is_empty());
    }
}
