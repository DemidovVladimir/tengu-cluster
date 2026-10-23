//! Planner context shared by planner and subagents.
//!
//! | Artifact | Writer | Reader | Role |
//! |---|---|---|---|
//! | `TENGU_PLANNER_REGISTRY.md` | `ensure_planner_registry` (every planner turn) | `RegistryPlanner` (in-memory render) | Roster of agents / skills / tools (core + MCP). The file is a debug mirror — the prompt block is built from the in-memory render even when the write fails. |
//! | `AgentIpcInput.plan_state` | `SubprocessRunner::run_step` via `active_plan(session_id)` | `tengu run-agent` | **Source of truth** for the plan a subagent sees. Per-session, set by `replan::drive` on every accepted plan. |
//! | `TENGU_PLAN.md` | `write_plan_state` (every accepted plan, any session) | `run-agent` fallback only (old parents that send no `plan_state`) | Human-readable debug artifact. One global file under cwd — concurrent sessions overwrite each other, which is why it is no longer the IPC path. |
//!
//! Both files are runtime artifacts — gitignored, never hand-edited.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::config::AgentConfig;
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
pub(crate) fn set_active_plan(session_id: &str, phase: &str, plan: &Plan) -> String {
    let rendered = render_plan_state(phase, plan);
    with_active_plans(|m| {
        m.insert(session_id.to_string(), rendered.clone());
    });
    rendered
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

/// Render the roster (agents + skills + core tools + `mcp_tools`) and return
/// it as a prompt block. Fail-soft on the file write: the planner must never
/// lose its roster because the workspace is read-only — the block is
/// built from the in-memory render either way and the write failure is
/// only logged.
pub(crate) fn ensure_planner_registry(
    workspace: &Path,
    agents: &[(String, AgentConfig)],
    tools: &[ToolDef],
) -> Result<String> {
    let content = render_registry(workspace, agents, tools)?;
    let path = workspace.join(PLANNER_REGISTRY_FILE);
    if let Err(e) = std::fs::write(&path, &content) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to write planner registry file; planner continues with in-memory roster"
        );
    }
    Ok(format!(
        "\n## Planner registry (loaded from `{}`)\n\n{}",
        PLANNER_REGISTRY_FILE, content
    ))
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
/// field and the debug file. One bullet per step carries every `Step` field
/// (id, agent, dependencies, goal, compose), so no JSON copy rides along.
pub(crate) fn render_plan_state(phase: &str, plan: &Plan) -> String {
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
    out
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
    tools: &[ToolDef],
) -> Result<String> {
    let skills = scan_skill_names(workspace);

    let mut out = String::from("# Tengu Planner Registry\n\n");
    out.push_str("This file is generated from the `[agents.*]` blocks of the active config that carry a `description`; `## Skills` and `## Tools` list names only (skills on disk, catalog tools and every configured MCP server's `<server>__<tool>`) for a `compose`. The planner loads it into every planner turn.\n\n");

    out.push_str("## Agents\n\n");
    if agents.is_empty() {
        out.push_str("_No agents found._\n\n");
    }
    for (name, agent) in agents {
        out.push_str(&format!("### {}\n\n", name));
        out.push_str(&format!("- engine: `{}`\n", agent.engine));
        out.push_str(&format!("- model: `{}`\n", agent.model));
        out.push_str(&format!("- tools: {}\n", agent_tools_line(agent)));
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
    out.push_str(&names_line(skills.iter().map(String::as_str)));
    out.push_str("## Tools\n\n");
    out.push_str(&names_line(tools.iter().map(|t| t.name.as_str())));
    Ok(out)
}

/// One line of backticked names, or `_None._`.
fn names_line<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<String> = names.map(|n| format!("`{n}`")).collect();
    if names.is_empty() {
        "_None._\n\n".to_string()
    } else {
        format!("{}\n\n", names.join(", "))
    }
}

/// An agent's tool list as `agent_base_tools` reads it: an empty `tools`
/// means every default tool (plus its `workspace_tools`).
fn agent_tools_line(agent: &AgentConfig) -> String {
    let ticked = |items: &[String]| {
        items
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (agent.tools.is_empty(), agent.workspace_tools.is_empty()) {
        (false, _) => ticked(&agent.tools),
        (true, true) => "every default tool".to_string(),
        (true, false) => format!("every default tool + {}", ticked(&agent.workspace_tools)),
    }
}

#[derive(Debug, Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Names of the skills the loader would find, sorted, first root wins. A
/// skill without a `name` and a `description` in its front matter is skipped.
fn scan_skill_names(workspace: &Path) -> Vec<String> {
    // The skill loader's directories and order (managed first; was project
    // first, so the registry could describe another skill than agents load).
    let roots = crate::application::skills::registry::skill_directories(workspace);

    let mut names = std::collections::BTreeSet::new();
    for root in roots {
        names.extend(scan_one_skill_root(&root));
    }
    names.into_iter().collect()
}

fn scan_one_skill_root(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|path| path.is_file())
        .filter_map(|path| parse_skill_name(&path).ok().flatten())
        .collect()
}

fn parse_skill_name(path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    let Some(frontmatter) = parse_frontmatter(&content) else {
        return Ok(None);
    };
    let Some(name) = frontmatter.name.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    if frontmatter
        .description
        .filter(|s| !s.trim().is_empty())
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(name))
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
        let a = set_active_plan("sf-sess-a", "active", &plan("a1"));
        let b = set_active_plan("sf-sess-b", "active", &plan("b1"));
        assert_ne!(a, b);
        assert_eq!(active_plan("sf-sess-a").as_deref(), Some(a.as_str()));
        assert_eq!(active_plan("sf-sess-b").as_deref(), Some(b.as_str()));
        assert!(active_plan("sf-sess-none").is_none());

        // Clearing with a stale rendering leaves a newer registration alone.
        let a2 = set_active_plan("sf-sess-a", "active", &plan("a2"));
        clear_active_plan("sf-sess-a", &a);
        assert_eq!(active_plan("sf-sess-a").as_deref(), Some(a2.as_str()));
        clear_active_plan("sf-sess-a", &a2);
        assert!(active_plan("sf-sess-a").is_none());
        clear_active_plan("sf-sess-b", &b);
    }

    #[test]
    fn plan_state_block_wraps_rendered_text() {
        let rendered = render_plan_state("active", &plan("s1"));
        assert!(rendered.contains("- `s1` agent=`researcher`"));
        assert!(!rendered.contains("```json"), "bullets only");
        let block = plan_state_block(&rendered, "IPC `plan_state`");
        assert!(block.starts_with("\n## Current execution plan (loaded from IPC `plan_state`)"));
        assert!(block.contains(&rendered));
        assert!(plan_state_block("   ", "x").is_empty());
    }

    #[test]
    fn write_plan_state_mirrors_rendered_text() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render_plan_state("active", &plan("s1"));
        write_plan_state(dir.path(), &rendered).unwrap();
        let block = read_plan_state_block(dir.path());
        assert!(block.contains(&rendered));
        assert!(read_plan_state_block(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn registry_lists_core_and_mcp_tools() {
        let dir = tempfile::tempdir().unwrap();
        // Full list as `ToolDirectory` returns it: catalog + MCP tools.
        let tools = vec![
            ToolDef::new(
                "http_request",
                "Make an HTTP request.",
                serde_json::json!({}),
            ),
            ToolDef {
                name: "notes__search_posts".into(),
                description: "Search notes posts.\nSecond line ignored.".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];
        let block = ensure_planner_registry(dir.path(), &[], &tools).unwrap();
        assert!(
            block.contains("## Tools\n\n`http_request`, `notes__search_posts`\n"),
            "{block}"
        );
        assert!(!block.contains("Search notes posts"), "names only: {block}");
        assert!(dir.path().join(PLANNER_REGISTRY_FILE).is_file());
    }

    #[test]
    fn registry_lists_routable_agents_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::config::Config::default()
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
        let block = ensure_planner_registry(dir.path(), &list, &[]).unwrap();
        assert!(block.contains("### researcher"), "{block}");
        assert!(block.contains("- tools: `http_request`\n"), "{block}");
        assert!(block.contains("- skills: `web-research`"), "{block}");
        assert!(block.contains("Fetches prices from the web."), "{block}");
        assert!(block.contains("- what is the BTC price?"), "{block}");
        assert!(!block.contains("### planner"), "{block}");
    }

    /// Skills are listed by name: no description, no absolute path (the
    /// registry used to send `_(source: /Users/…/SKILL.md)_` per skill). An
    /// agent with no `tools` line holds every default tool.
    #[test]
    fn registry_lists_skill_names_without_paths() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("skills").join("demo-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: A long description.\n---\nbody\n",
        )
        .unwrap();
        let mut agent = crate::config::Config::default()
            .agents
            .remove("main")
            .unwrap();
        agent.tools.clear();
        agent.description = Some("Does things.".into());
        let block = ensure_planner_registry(dir.path(), &[("main".into(), agent)], &[]).unwrap();
        assert!(block.contains("`demo-skill`"), "{block}");
        assert!(!block.contains("A long description."), "{block}");
        assert!(!block.contains("SKILL.md"), "{block}");
        assert!(
            !block.contains(&dir.path().display().to_string()),
            "{block}"
        );
        assert!(block.contains("- tools: every default tool"), "{block}");
    }

    #[test]
    fn registry_survives_unwritable_workspace() {
        // A regular file as "workspace": `<file>/TENGU_PLANNER_REGISTRY.md`
        // cannot be written, but the block must still carry the roster.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let tools = [ToolDef::new("http_request", "d", serde_json::json!({}))];
        let block = ensure_planner_registry(&not_a_dir, &[], &tools).unwrap();
        assert!(block.contains("## Tools\n\n`http_request`\n"), "{block}");
    }
}
