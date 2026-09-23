//! View-skill plugin — `view_skill` tool.
//!
//! Read-only inspection of tengu skills. Three actions:
//!
//! - `list` — enumerate every available skill across managed → workspace
//!   → project tiers, with shadowing (first match wins). Returns frontmatter
//!   description, tier label, and counts for fixtures / resources / metrics.
//! - `read` — return SKILL.md frontmatter + body + resource listing +
//!   metrics summary for one skill.
//! - `read_resource` — return the raw UTF-8 content of a single file under
//!   `skills/<name>/resources/<path>`. Symlink-escape guarded; max 1 MB.
//!
//! Replaces `skill_resource` with a cleaner three-action API mirroring
//! Hermes-agent's `skill_view`. Read-only — does NOT modify files. Use
//! `manage_skill` for writes.
//!
//! ### Three-tier walk
//!
//! Mirrors `application/skills/registry.rs::skill_directories` precedence: managed (`~/.tengu/skills`)
//! → workspace (`<cwd>/.tengu/skills`) → project (`<cwd>/skills`). First
//! match wins; the winning tier is reported in `list` and `read` outputs.
//!
//! ### Schema
//! - `action: "list" | "read" | "read_resource"` (required)
//! - `skill: string` (required for `read` + `read_resource`; ignored for `list`)
//! - `path: string` (required for `read_resource` — relative to `resources/`)

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};

pub(crate) const VIEW_SKILL_TOOL_NAME: &str = "view_skill";

/// Maximum file size returned by `read_resource` — guard against `cat`-ing a
/// 100 MB PDF into the LLM context.
const MAX_READ_BYTES: u64 = 1_000_000;

/// Maximum length of a `path` argument to `read_resource`.
const MAX_PATH_LEN: usize = 256;

pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![ViewSkillTool::new().definition().clone()]
}

pub(crate) struct ViewSkillPlugin;

#[async_trait]
impl ToolPlugin for ViewSkillPlugin {
    fn name(&self) -> &'static str {
        "view_skill"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(ViewSkillTool::new())])
    }
}

pub(crate) struct ViewSkillTool {
    def: ToolDef,
}

impl ViewSkillTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                VIEW_SKILL_TOOL_NAME,
                "Inspect tengu skills: list every available skill, read a \
                 skill's frontmatter + body, or read a specific resource \
                 file from a skill. Walks managed (~/.tengu/skills) → \
                 workspace (.tengu/skills) → project (skills/) tiers; \
                 first match wins. Read-only — does NOT modify files. Use \
                 manage_skill for writes.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "read", "read_resource"],
                            "description": "Which view: enumerate all skills, read one skill's SKILL.md, or read a single resource file."
                        },
                        "skill": {
                            "type": "string",
                            "description": "Skill name (kebab-case). Required for read + read_resource; ignored for list."
                        },
                        "path": {
                            "type": "string",
                            "description": "Required when action=read_resource. Relative to skills/<skill>/resources/. No '..', no leading '/'."
                        }
                    },
                    "required": ["action"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for ViewSkillTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // Coarse fs_read gate at the workspace root. All three-tier scanner
        // outputs (`<workspace>/skills`, `<workspace>/.tengu/skills`,
        // `~/.tengu/skills`) are read-only browses below this point.
        ctx.scope.check_fs_read(ctx.workspace)?;

        let action = require_str(args, VIEW_SKILL_TOOL_NAME, "action")?;

        match action {
            "list" => list_all_skills(&cwd()),
            "read" => {
                let skill = require_str(args, VIEW_SKILL_TOOL_NAME, "skill")?;
                validate_skill_name(skill)?;
                let (skill_dir, tier) = locate_skill_dir(skill, &cwd())
                    .ok_or_else(|| anyhow!("no skill '{}' found in any tier", skill))?;
                read_skill(skill, &skill_dir, tier)
            }
            "read_resource" => {
                let skill = require_str(args, VIEW_SKILL_TOOL_NAME, "skill")?;
                let path = require_str(args, VIEW_SKILL_TOOL_NAME, "path")?;
                validate_skill_name(skill)?;
                validate_path_under_resources(path)?;
                let (skill_dir, _tier) = locate_skill_dir(skill, &cwd())
                    .ok_or_else(|| anyhow!("no skill '{}' found in any tier", skill))?;
                let resources_dir = skill_dir.join("resources");
                read_resource(skill, &resources_dir, path)
            }
            other => bail!(
                "view_skill: unknown action '{}', expected 'list' | 'read' | 'read_resource'",
                other
            ),
        }
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Validation helpers (lifted from skill_resource)
// ---------------------------------------------------------------------------

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("skill name '{}' length out of range (1..=64)", name);
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        bail!(
            "skill name '{}' must start with [a-z]; got '{}'",
            name,
            first
        );
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            bail!(
                "skill name '{}' has invalid char '{}' (allowed: a-z, 0-9, -)",
                name,
                c
            );
        }
    }
    Ok(())
}

fn validate_path_under_resources(rel: &str) -> Result<()> {
    if rel.is_empty() {
        bail!("path is empty");
    }
    if rel.len() > MAX_PATH_LEN {
        bail!("path is {} bytes — over {} limit", rel.len(), MAX_PATH_LEN);
    }
    if rel.starts_with('/') {
        bail!("path '{}' is absolute; must be relative to resources/", rel);
    }
    let p = Path::new(rel);
    for c in p.components() {
        match c {
            Component::ParentDir => bail!("path '{}' contains '..' — refused", rel),
            Component::RootDir | Component::Prefix(_) => {
                bail!("path '{}' has a root/prefix component — refused", rel)
            }
            Component::Normal(os) => {
                if os.to_str().is_none() {
                    bail!("path '{}' has a non-utf8 component", rel);
                }
            }
            Component::CurDir => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Three-tier locator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Managed,
    Workspace,
    Project,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::Managed => "managed",
            Tier::Workspace => "workspace",
            Tier::Project => "project",
        }
    }
}

/// Walk managed → workspace → project; return the first directory that
/// contains a `SKILL.md` file (matching `scan_skills`'s shadowing order).
fn locate_skill_dir(skill: &str, cwd: &Path) -> Option<(PathBuf, Tier)> {
    for (tier, root) in tier_roots(cwd) {
        let candidate = root.join(skill);
        if candidate.join("SKILL.md").is_file() {
            return Some((candidate, tier));
        }
    }
    None
}

fn tier_roots(cwd: &Path) -> Vec<(Tier, PathBuf)> {
    let mut out = Vec::with_capacity(3);
    if let Some(home) = dirs_next::home_dir() {
        out.push((Tier::Managed, home.join(".tengu").join("skills")));
    }
    out.push((Tier::Workspace, cwd.join(".tengu").join("skills")));
    out.push((Tier::Project, cwd.join("skills")));
    out
}

// ---------------------------------------------------------------------------
// Action: list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SkillSummary {
    name: String,
    description: String,
    tier: Tier,
    fixtures: usize,
    resources_count: usize,
    metrics_count: usize,
    editable_by_learner: bool,
}

fn list_all_skills(cwd: &Path) -> Result<ToolOutput> {
    let mut by_name: std::collections::HashMap<String, SkillSummary> =
        std::collections::HashMap::new();

    for (tier, root) in tier_roots(cwd) {
        if !root.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    root = %root.display(),
                    error = %e,
                    "view_skill: skill root unreadable"
                );
                continue;
            }
        };
        for dir_entry in entries.flatten() {
            let skill_dir = dir_entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let summary = match summarise_skill(&skill_dir, tier) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    tracing::debug!(
                        path = %skill_md.display(),
                        "view_skill: SKILL.md has no parseable frontmatter; skipping"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::debug!(
                        path = %skill_md.display(),
                        error = %e,
                        "view_skill: skill summary failed; skipping"
                    );
                    continue;
                }
            };
            // First-tier-wins shadowing.
            by_name.entry(summary.name.clone()).or_insert(summary);
        }
    }

    let mut out: Vec<SkillSummary> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));

    let entries: Vec<Value> = out
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "tier": s.tier.as_str(),
                "fixtures": s.fixtures,
                "resources_count": s.resources_count,
                "metrics_count": s.metrics_count,
                "editable_by_learner": s.editable_by_learner,
            })
        })
        .collect();

    Ok(ToolOutput::from(
        json!({
            "skills": entries,
            "count": entries.len(),
        })
        .to_string(),
    ))
}

fn summarise_skill(skill_dir: &Path, tier: Tier) -> Result<Option<SkillSummary>> {
    let skill_md = skill_dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_md)?;
    let (fm_value, _body) = match split_frontmatter(&content) {
        Some(parts) => parts,
        None => return Ok(None),
    };
    let fm = match fm_value {
        Some(v) => v,
        None => return Ok(None),
    };

    let name = fm
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            skill_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    if name.is_empty() {
        return Ok(None);
    }
    let description = fm
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let editable_by_learner = fm
        .get("editable_by_learner")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let metrics_count = fm
        .get("metrics")
        .and_then(|m| m.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);

    let resources_count = count_resource_files(&skill_dir.join("resources"));
    let fixtures = count_fixtures(&skill_dir.join("evals").join("prompts.yaml"));

    Ok(Some(SkillSummary {
        name,
        description,
        tier,
        fixtures,
        resources_count,
        metrics_count,
        editable_by_learner,
    }))
}

fn count_resource_files(resources_dir: &Path) -> usize {
    if !resources_dir.is_dir() {
        return 0;
    }
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    let _ = walk_files(resources_dir, resources_dir, &mut files);
    files.len()
}

fn count_fixtures(prompts_yaml: &Path) -> usize {
    if !prompts_yaml.is_file() {
        return 0;
    }
    let body = match std::fs::read_to_string(prompts_yaml) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let v: serde_yaml::Value = match serde_yaml::from_str(&body) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    v.get("fixtures")
        .and_then(|f| f.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Action: read
// ---------------------------------------------------------------------------

fn read_skill(skill: &str, skill_dir: &Path, tier: Tier) -> Result<ToolOutput> {
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.is_file() {
        return Ok(ToolOutput::from(
            json!({
                "error": "SKILL.md missing",
                "skill": skill,
            })
            .to_string(),
        ));
    }
    let content =
        std::fs::read_to_string(&skill_md_path).map_err(|e| anyhow!("read SKILL.md: {e}"))?;

    let (fm_value, body) = match split_frontmatter(&content) {
        Some(parts) => parts,
        None => {
            return Ok(ToolOutput::from(
                json!({
                    "error": "SKILL.md has no frontmatter delimiters",
                    "skill": skill,
                })
                .to_string(),
            ));
        }
    };
    let frontmatter = fm_value.unwrap_or(serde_yaml::Value::Null);

    // Resources list (recursive walk, dotfiles skipped).
    let resources_dir = skill_dir.join("resources");
    let mut resource_files: Vec<(PathBuf, u64)> = Vec::new();
    if resources_dir.is_dir() {
        walk_files(&resources_dir, &resources_dir, &mut resource_files)?;
    }
    resource_files.sort_by(|a, b| a.0.cmp(&b.0));
    let resources_json: Vec<Value> = resource_files
        .iter()
        .map(|(p, size)| {
            json!({
                "path": p.display().to_string().replace('\\', "/"),
                "size_bytes": size,
            })
        })
        .collect();

    let (metrics_summary, metrics_warning) = build_metrics_summary(&frontmatter);

    // Convert serde_yaml::Value -> serde_json::Value for output.
    let frontmatter_json: Value = serde_json::to_value(&frontmatter).unwrap_or(Value::Null);

    let mut out = json!({
        "name": skill,
        "tier": tier.as_str(),
        "frontmatter": frontmatter_json,
        "body": body,
        "resources": resources_json,
        "metrics_summary": metrics_summary,
    });
    if let Some(w) = metrics_warning {
        if let Some(map) = out.as_object_mut() {
            map.insert("metrics_warning".to_string(), Value::String(w));
        }
    }
    Ok(ToolOutput::from(out.to_string()))
}

/// Returns `Some((frontmatter, body))` when SKILL.md has the standard
/// `---\n<yaml>\n---\n<body>` envelope. The frontmatter `Option<Value>` is
/// `None` when the YAML is unparseable (caller can decide to error or warn).
/// Returns `None` when the envelope itself is missing.
fn split_frontmatter(content: &str) -> Option<(Option<serde_yaml::Value>, String)> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let yaml_text = rest[..end].trim_start_matches('\n');
    let body_start = end + "\n---".len();
    // Body: skip up to and including the next newline (if present).
    let body = match rest[body_start..].strip_prefix('\n') {
        Some(after_nl) => after_nl.to_string(),
        None => rest[body_start..].to_string(),
    };
    let fm = serde_yaml::from_str::<serde_yaml::Value>(yaml_text).ok();
    Some((fm, body))
}

fn build_metrics_summary(fm: &serde_yaml::Value) -> (Vec<Value>, Option<String>) {
    let metrics_seq = match fm.get("metrics").and_then(|m| m.as_sequence()) {
        Some(s) => s,
        None => return (Vec::new(), None),
    };
    let mut out: Vec<Value> = Vec::new();
    let mut warned = false;
    for m in metrics_seq {
        let mapping = match m.as_mapping() {
            Some(map) => map,
            None => {
                warned = true;
                continue;
            }
        };
        let mut entry = serde_json::Map::new();
        for (k, v) in mapping.iter() {
            let key = match k.as_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            let json_val = serde_json::to_value(v).unwrap_or(Value::Null);
            entry.insert(key, json_val);
        }
        out.push(Value::Object(entry));
    }
    let warning = if warned {
        Some("one or more metric entries were not mappings; skipped".to_string())
    } else {
        None
    };
    (out, warning)
}

// ---------------------------------------------------------------------------
// Action: read_resource
// ---------------------------------------------------------------------------

fn read_resource(skill: &str, resources_dir: &Path, rel: &str) -> Result<ToolOutput> {
    if !resources_dir.is_dir() {
        bail!("view_skill: skill '{}' has no resources/ directory", skill);
    }
    let abs = resources_dir.join(rel);
    let canon_resources = std::fs::canonicalize(resources_dir)
        .map_err(|e| anyhow!("canonicalize resources_dir: {e}"))?;
    let canon_target = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(_) => bail!(
            "view_skill: '{}' not found under skills/{}/resources/",
            rel,
            skill
        ),
    };
    if !canon_target.starts_with(&canon_resources) {
        bail!(
            "view_skill: path '{}' resolves outside resources/ (symlink escape?)",
            rel
        );
    }
    let meta = std::fs::metadata(&canon_target)?;
    if !meta.is_file() {
        bail!("view_skill: '{}' is not a regular file", rel);
    }
    if meta.len() > MAX_READ_BYTES {
        bail!(
            "view_skill: '{}' is {} bytes — over {} limit. Use list to browse, then read targeted files.",
            rel,
            meta.len(),
            MAX_READ_BYTES
        );
    }
    let content = std::fs::read_to_string(&canon_target).map_err(|e| {
        anyhow!(
            "view_skill: read '{}' failed: {e} (binary file? not utf8?)",
            rel
        )
    })?;
    Ok(ToolOutput::from(
        json!({
            "skill": skill,
            "path": rel,
            "content": content,
            "bytes": meta.len(),
        })
        .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Recursive directory walker (lifted from skill_resource, dotfile-skipping)
// ---------------------------------------------------------------------------

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64)>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()), // missing dir → empty list
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip dotfiles
        }
        if path.is_dir() {
            walk_files(root, &path, out)?;
        } else if path.is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.clone());
            out.push((rel, size));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }

    /// Seed `<root>/skills/<name>/SKILL.md` with the given frontmatter +
    /// body. Returns the skill directory.
    fn seed_skill(root: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let skill_dir = root.join("skills").join(name);
        let skill_md = skill_dir.join("SKILL.md");
        let content = format!("---\n{}---\n{}", frontmatter, body);
        write(skill_md.parent().unwrap(), "SKILL.md", &content);
        skill_dir
    }

    // ----- validation -----

    #[test]
    fn validate_skill_name_rejects_uppercase_underscore() {
        assert!(validate_skill_name("UPPER").is_err());
        assert!(validate_skill_name("under_score").is_err());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("with spaces").is_err());
        assert!(validate_skill_name("../etc").is_err());
        // accepts well-formed.
        validate_skill_name("spanish-teacher").unwrap();
        validate_skill_name("skill-creator").unwrap();
        validate_skill_name("a1").unwrap();
    }

    #[test]
    fn validate_path_rejects_traversal_and_absolute() {
        assert!(validate_path_under_resources("../etc/passwd").is_err());
        assert!(validate_path_under_resources("/etc/passwd").is_err());
        assert!(validate_path_under_resources("").is_err());
        let too_long = "a".repeat(MAX_PATH_LEN + 1);
        assert!(validate_path_under_resources(&too_long).is_err());
        // accepts well-formed.
        validate_path_under_resources("subjunctive.md").unwrap();
        validate_path_under_resources("topic/dative.md").unwrap();
    }

    // ----- list -----

    #[test]
    fn list_returns_all_seeded_skills() {
        let tmp = TempDir::new().unwrap();
        seed_skill(
            tmp.path(),
            "alpha",
            "name: alpha\ndescription: first\nmetrics:\n  - name: q\n    kind: llm_judge\n",
            "# Alpha\n",
        );
        seed_skill(
            tmp.path(),
            "bravo",
            "name: bravo\ndescription: second\neditable_by_learner: true\n",
            "# Bravo\n",
        );
        // Add a resource file under alpha for resources_count.
        write(
            &tmp.path().join("skills").join("alpha").join("resources"),
            "topic.md",
            "x",
        );

        let out = list_all_skills(tmp.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["count"], 2);
        let arr = v["skills"].as_array().unwrap();
        // sorted by name.
        assert_eq!(arr[0]["name"], "alpha");
        assert_eq!(arr[0]["tier"], "project");
        assert_eq!(arr[0]["resources_count"], 1);
        assert_eq!(arr[0]["metrics_count"], 1);
        assert_eq!(arr[0]["editable_by_learner"], false);
        assert_eq!(arr[1]["name"], "bravo");
        assert_eq!(arr[1]["editable_by_learner"], true);
    }

    #[test]
    fn list_handles_no_skills_dir() {
        let tmp = TempDir::new().unwrap();
        // No skills/ directory at all.
        let out = list_all_skills(tmp.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["count"], 0);
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_skips_skills_with_malformed_frontmatter() {
        let tmp = TempDir::new().unwrap();
        // Good skill.
        seed_skill(
            tmp.path(),
            "good",
            "name: good\ndescription: ok\n",
            "# OK\n",
        );
        // Bad skill: SKILL.md without frontmatter delimiters at all.
        let bad_dir = tmp.path().join("skills").join("bad");
        write(&bad_dir, "SKILL.md", "no frontmatter here\nnothing\n");
        // Worse skill: malformed YAML inside delimiters.
        let ugly_dir = tmp.path().join("skills").join("ugly");
        write(
            &ugly_dir,
            "SKILL.md",
            "---\nname: : : :\n  bad: [unclosed\n---\nbody\n",
        );

        let out = list_all_skills(tmp.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        let names: Vec<&str> = v["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"good"));
        // bad/ugly may be skipped or, if they parse to something non-empty,
        // surface — but must NOT crash the listing.
        assert!(v["count"].as_u64().unwrap() >= 1);
    }

    // ----- read -----

    #[test]
    fn read_returns_frontmatter_body_resources() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = seed_skill(
            tmp.path(),
            "demo",
            "name: demo\ndescription: a demo\nmetrics:\n  - name: quality\n    kind: llm_judge\n    min_pass_rate: 0.7\n",
            "# Demo\n\nBody text here.\n",
        );
        write(&skill_dir.join("resources"), "topic.md", "topic content");
        write(
            &skill_dir.join("resources").join("nested"),
            "deep.md",
            "deep",
        );

        let out = read_skill("demo", &skill_dir, Tier::Project).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["name"], "demo");
        assert_eq!(v["tier"], "project");
        assert_eq!(v["frontmatter"]["name"], "demo");
        assert_eq!(v["frontmatter"]["description"], "a demo");
        assert!(v["body"].as_str().unwrap().contains("Body text here"));
        let resources = v["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
        let metrics = v["metrics_summary"].as_array().unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["kind"], "llm_judge");
        assert_eq!(metrics[0]["name"], "quality");
    }

    #[test]
    fn read_handles_skill_without_resources() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = seed_skill(
            tmp.path(),
            "lean",
            "name: lean\ndescription: no extras\n",
            "# Lean\n",
        );
        let out = read_skill("lean", &skill_dir, Tier::Project).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["resources"].as_array().unwrap().len(), 0);
        assert_eq!(v["metrics_summary"].as_array().unwrap().len(), 0);
        assert!(v["body"].as_str().unwrap().contains("# Lean"));
    }

    // ----- read_resource -----

    #[test]
    fn read_resource_returns_content() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = seed_skill(
            tmp.path(),
            "demo",
            "name: demo\ndescription: x\n",
            "# Demo\n",
        );
        write(&skill_dir.join("resources"), "x.md", "hello world");

        let resources_dir = skill_dir.join("resources");
        let out = read_resource("demo", &resources_dir, "x.md").unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["content"], "hello world");
        assert_eq!(v["bytes"], 11);
        assert_eq!(v["path"], "x.md");
        assert_eq!(v["skill"], "demo");
    }

    #[test]
    fn read_resource_rejects_traversal() {
        // Validation happens at the API boundary.
        assert!(validate_path_under_resources("../../../etc/passwd").is_err());
        assert!(validate_path_under_resources("../sibling/file.md").is_err());
        assert!(validate_path_under_resources("/etc/passwd").is_err());
    }

    #[test]
    fn read_resource_rejects_oversized() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = seed_skill(
            tmp.path(),
            "demo",
            "name: demo\ndescription: x\n",
            "# Demo\n",
        );
        // 1.5 MB content.
        let big = "x".repeat(1_500_000);
        write(&skill_dir.join("resources"), "huge.md", &big);
        let resources_dir = skill_dir.join("resources");

        let err = read_resource("demo", &resources_dir, "huge.md").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("over"), "expected oversize error, got: {msg}");
    }

    // ----- locator -----

    #[test]
    fn locate_skill_dir_finds_project_tier() {
        let tmp = TempDir::new().unwrap();
        seed_skill(
            tmp.path(),
            "found",
            "name: found\ndescription: x\n",
            "# Found\n",
        );
        let (dir, tier) = locate_skill_dir("found", tmp.path()).unwrap();
        assert_eq!(tier, Tier::Project);
        assert!(dir.ends_with("skills/found"));
    }

    #[test]
    fn locate_skill_dir_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(locate_skill_dir("ghost", tmp.path()).is_none());
    }
}
