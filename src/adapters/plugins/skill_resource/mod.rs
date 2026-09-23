//! Skill-resource plugin — `skill_resource` tool.
//!
//! Lets agents discover and read files under `skills/<name>/resources/` even
//! when the agent's workspace is a tmpdir. Walks the three-tier shadowing
//! order (managed → workspace → project) — same precedence as
//! `skill_builder.rs::skill_directories`.
//!
//! ### When to use
//! Skills with materials (PDFs, markdown notes, links files, etc.) under
//! `resources/` rely on this tool because the SKILL.md body is loaded into
//! the system prompt but `resources/<file>` files are not. Without this
//! tool, an agent that tries to "cite the resources" can only refuse —
//! it has no path to read them.
//!
//! ### Schema
//! - `action: "list" | "read"` (required)
//! - `skill: string` (required) — kebab-case skill name
//! - `path: string` (required when `action == "read"`) — relative to
//!   `resources/`. No `..`, no leading `/`.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_utils::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};

pub(crate) const SKILL_RESOURCE_TOOL_NAME: &str = "skill_resource";

/// Maximum file size returned by `read` — guard against `cat`-ing a 100MB PDF
/// into the LLM context.
const MAX_READ_BYTES: u64 = 1_000_000;

pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![SkillResourceTool::new().definition().clone()]
}

pub(crate) struct SkillResourcePlugin;

#[async_trait]
impl ToolPlugin for SkillResourcePlugin {
    fn name(&self) -> &'static str {
        "skill_resource"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(SkillResourceTool::new())])
    }
}

pub(crate) struct SkillResourceTool {
    def: ToolDef,
}

impl SkillResourceTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                SKILL_RESOURCE_TOOL_NAME,
                "List or read files under a skill's resources/ folder. \
                 Use action=list to discover what's available; action=read \
                 to fetch a specific file. Walks managed → workspace → \
                 project tiers (first match wins). Path must be relative \
                 to resources/ — no `..`, no leading `/`.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "read"],
                            "description": "Whether to enumerate files or read one."
                        },
                        "skill": {
                            "type": "string",
                            "description": "Skill name (kebab-case)."
                        },
                        "path": {
                            "type": "string",
                            "description": "Required when action=read. Relative to skills/<skill>/resources/."
                        }
                    },
                    "required": ["action", "skill"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for SkillResourceTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // Coarse fs_read gate at the workspace root. Read-only browse over
        // skills/<name>/resources/ below this point.
        ctx.scope.check_fs_read(ctx.workspace)?;

        let action = require_str(args, SKILL_RESOURCE_TOOL_NAME, "action")?;
        let skill = require_str(args, SKILL_RESOURCE_TOOL_NAME, "skill")?;

        validate_skill_name(skill)?;
        let resources_dir = locate_resources_dir(skill)?
            .ok_or_else(|| anyhow!("no skill '{}' found in any tier", skill))?;

        match action {
            "list" => list_resources(skill, &resources_dir),
            "read" => {
                let path = require_str(args, SKILL_RESOURCE_TOOL_NAME, "path")?;
                validate_resource_path(path)?;
                read_resource(skill, &resources_dir, path)
            }
            other => bail!(
                "skill_resource: unknown action '{}', expected 'list' or 'read'",
                other
            ),
        }
    }
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("skill name '{}' length out of range (1..=64)", name);
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        bail!(
            "skill name '{}' must start with [a-z0-9]; got '{}'",
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

fn validate_resource_path(rel: &str) -> Result<()> {
    if rel.is_empty() {
        bail!("path is empty");
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

/// Walk managed → workspace → project, return the first `resources/` that
/// exists. Mirrors `skill_builder.rs::skill_directories`'s shadowing order.
fn locate_resources_dir(skill: &str) -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidates = candidate_resource_dirs(skill, &cwd);
    for c in candidates {
        if c.is_dir() {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

fn candidate_resource_dirs(skill: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    if let Some(home) = dirs_next::home_dir() {
        out.push(home.join(".tengu/skills").join(skill).join("resources"));
    }
    out.push(cwd.join(".tengu/skills").join(skill).join("resources"));
    out.push(cwd.join("skills").join(skill).join("resources"));
    out
}

fn list_resources(skill: &str, resources_dir: &Path) -> Result<ToolOutput> {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    walk_files(resources_dir, resources_dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let entries: Vec<Value> = files
        .iter()
        .map(|(p, size)| {
            json!({
                "path": p.display().to_string().replace('\\', "/"),
                "size_bytes": size,
            })
        })
        .collect();

    Ok(ToolOutput::from(
        json!({
            "skill": skill,
            "resources_dir": resources_dir.display().to_string(),
            "files": entries,
            "count": entries.len(),
        })
        .to_string(),
    ))
}

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

fn read_resource(skill: &str, resources_dir: &Path, rel: &str) -> Result<ToolOutput> {
    let abs = resources_dir.join(rel);
    // Re-validate post-join: a malicious-or-mistaken combination can still
    // escape if the host filesystem has symlinks. Canonicalise + prefix-check.
    let canon_resources = std::fs::canonicalize(resources_dir)
        .map_err(|e| anyhow!("canonicalize resources_dir: {e}"))?;
    let canon_target = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(_) => bail!(
            "skill_resource: '{}' not found under skills/{}/resources/",
            rel,
            skill
        ),
    };
    if !canon_target.starts_with(&canon_resources) {
        bail!(
            "skill_resource: path '{}' resolves outside resources/ (symlink escape?)",
            rel
        );
    }
    let meta = std::fs::metadata(&canon_target)?;
    if !meta.is_file() {
        bail!("skill_resource: '{}' is not a regular file", rel);
    }
    if meta.len() > MAX_READ_BYTES {
        bail!(
            "skill_resource: '{}' is {} bytes — over {} limit. List first, then read targeted files.",
            rel,
            meta.len(),
            MAX_READ_BYTES
        );
    }
    let content = std::fs::read_to_string(&canon_target).map_err(|e| {
        anyhow!(
            "skill_resource: read '{}' failed: {e} (binary file? not utf8?)",
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

    #[test]
    fn validate_skill_name_accepts_kebab() {
        validate_skill_name("spanish-teacher").unwrap();
        validate_skill_name("skill-creator").unwrap();
        validate_skill_name("a1").unwrap();
    }

    #[test]
    fn validate_skill_name_rejects_garbage() {
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("UPPER").is_err());
        assert!(validate_skill_name("with spaces").is_err());
        assert!(validate_skill_name("../etc").is_err());
        assert!(validate_skill_name("under_score").is_err());
    }

    #[test]
    fn validate_resource_path_rejects_traversal() {
        assert!(validate_resource_path("../etc/passwd").is_err());
        assert!(validate_resource_path("/etc/passwd").is_err());
        assert!(validate_resource_path("").is_err());
    }

    #[test]
    fn validate_resource_path_accepts_relative() {
        validate_resource_path("subjunctive.md").unwrap();
        validate_resource_path("topic/dative.md").unwrap();
    }

    #[test]
    fn list_resources_walks_recursively_skipping_dotfiles() {
        let tmp = TempDir::new().unwrap();
        let res = tmp.path();
        write(res, "a.md", "alpha");
        write(res, "topic/b.md", "beta");
        write(res, ".hidden", "secret");
        let out = list_resources("demo", res).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["count"], 2);
        let paths: Vec<&str> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"a.md"));
        assert!(paths.contains(&"topic/b.md"));
        assert!(!paths.iter().any(|p| p.contains(".hidden")));
    }

    #[test]
    fn read_resource_returns_content() {
        let tmp = TempDir::new().unwrap();
        let res = tmp.path();
        write(res, "x.md", "hello world");
        let out = read_resource("demo", res, "x.md").unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["content"], "hello world");
        assert_eq!(v["bytes"], 11);
    }

    #[test]
    fn read_resource_refuses_missing_file() {
        let tmp = TempDir::new().unwrap();
        let res = tmp.path();
        std::fs::create_dir_all(res).unwrap();
        let err = read_resource("demo", res, "nope.md").unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[test]
    fn read_resource_refuses_oversized() {
        let tmp = TempDir::new().unwrap();
        let res = tmp.path();
        let big = "x".repeat((MAX_READ_BYTES + 1) as usize);
        write(res, "huge.md", &big);
        let err = read_resource("demo", res, "huge.md").unwrap_err();
        assert!(format!("{err}").contains("over"));
    }
}
