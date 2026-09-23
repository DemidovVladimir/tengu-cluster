//! `manage_skill` LLM-callable tool — unified write-side counterpart to
//! `view_skill`.
//!
//! Replaces `apply_improver_proposal` entirely; partially replaces
//! `skill_distill` (kept as a thin alias for back-compat).
//!
//! See `docs/skill-redesign-2026-04-29.md` § "New tools > manage_skill actions"
//! for the action shapes and migration plan.
//!
//! Doctrine notes:
//! - All writes are atomic (temp + `std::fs::rename`).
//! - All writes refuse on `editable_by_learner: false` (except `create`).
//! - All writes audit-logged via `audit::append`.
//! - Cache discipline: every result includes
//!   `loaded_in_current_conversation: false` so the agent tells the user to
//!   restart.

#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::application::skills::lifecycle::audit;
use crate::application::skills::lifecycle::evolve::{
    apply_proposal_to_skill_md, is_editable_by_learner, nanos, validate_resource_path, ProposalBody,
};
use crate::application::skills::lifecycle::metrics::MetricSpec;
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};

pub(crate) const MANAGE_SKILL_TOOL_NAME: &str = "manage_skill";

pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![ManageSkillTool::new().definition().clone()]
}

pub(crate) struct ManageSkillPlugin;

#[async_trait]
impl ToolPlugin for ManageSkillPlugin {
    fn name(&self) -> &'static str {
        "manage_skill"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(ManageSkillTool::new())])
    }
}

pub(crate) struct ManageSkillTool {
    def: ToolDef,
}

impl ManageSkillTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                MANAGE_SKILL_TOOL_NAME,
                "Create, edit, patch, or delete tengu skills + their resources. \
                 ATOMIC writes. Refuses on `editable_by_learner: false`. \
                 Audit-logged. Cache-disciplined (changes activate next session). \
                 Use this DIRECTLY — do not emit JSON proposals as text. The \
                 harness has no parser for that. Tools is your only path to disk.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": [
                                "create",
                                "edit_body",
                                "patch",
                                "add_resource",
                                "remove_resource",
                                "delete"
                            ],
                            "description": "Which write operation to perform."
                        },
                        "name": {
                            "type": "string",
                            "description": "Skill name (kebab-case)."
                        },
                        "description": {
                            "type": "string",
                            "description": "Frontmatter description (\"Use when ...\"). Required for action=create."
                        },
                        "body": {
                            "type": "string",
                            "description": "SKILL.md body excluding frontmatter. Required for create + edit_body."
                        },
                        "tier": {
                            "type": "string",
                            "enum": ["project", "workspace", "managed"],
                            "description": "Tier root for create / required when delete targets a non-project skill. Default 'project'."
                        },
                        "learner_facing": {
                            "type": "boolean",
                            "description": "Frontmatter flag. Default true on create."
                        },
                        "editable_by_learner": {
                            "type": "boolean",
                            "description": "Frontmatter flag controlling whether future manage_skill writes are allowed. Default true on create."
                        },
                        "metrics": {
                            "type": "array",
                            "description": "Optional MetricSpec[] for create."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "patch: substring to find."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "patch: replacement."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "patch: target file relative to skill_dir. Defaults to 'SKILL.md'. Use 'resources/<file>' to patch a resource."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "patch: replace every occurrence (default false)."
                        },
                        "path": {
                            "type": "string",
                            "description": "add_resource / remove_resource: relative to resources/."
                        },
                        "content": {
                            "type": "string",
                            "description": "add_resource: utf-8 file content."
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "add_resource: overwrite existing file (default false)."
                        }
                    },
                    "required": ["action", "name"]
                }),
            ),
        }
    }
}

#[derive(Deserialize)]
struct Args {
    action: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    learner_facing: Option<bool>,
    #[serde(default)]
    editable_by_learner: Option<bool>,
    #[serde(default)]
    metrics: Option<Vec<MetricSpec>>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    replace_all: Option<bool>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    overwrite: Option<bool>,
}

#[async_trait]
impl Tool for ManageSkillTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // Coarse fs_write gate at the workspace root. Every action below
        // (create / edit_body / patch / add_resource / remove_resource /
        // delete) writes under <workspace>/skills/<name>/. Atomic-rename
        // discipline is inside each `do_*` helper; the scope check is the
        // sandbox boundary.
        ctx.scope.check_fs_write(ctx.workspace)?;

        let args: Args = serde_json::from_value(args.clone())
            .map_err(|e| anyhow!("manage_skill: bad args — {e}"))?;
        validate_skill_name(&args.name)?;

        let workspace = ctx.workspace.to_path_buf();

        match args.action.as_str() {
            "create" => do_create(&args, &workspace),
            "edit_body" => do_edit_body(&args, &workspace),
            "patch" => do_patch(&args, &workspace),
            "add_resource" => do_add_resource(&args, &workspace),
            "remove_resource" => do_remove_resource(&args, &workspace),
            "delete" => do_delete(&args, &workspace),
            other => bail!(
                "manage_skill: unknown action '{}'. Expected one of: \
                 create, edit_body, patch, add_resource, remove_resource, delete.",
                other
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn do_create(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    let description = args
        .description
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.create: 'description' is required"))?;
    let body = args
        .body
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.create: 'body' is required"))?;
    if description.trim().is_empty() {
        bail!("manage_skill.create: 'description' must not be empty");
    }
    if body.trim().is_empty() {
        bail!("manage_skill.create: 'body' must not be empty");
    }

    let tier = args.tier.as_deref().unwrap_or("project");
    let tier_root = tier_root(workspace, tier)?;

    // Collision check across all three tiers.
    for candidate in collision_candidates(workspace, &args.name) {
        if candidate.exists() {
            bail!(
                "manage_skill.create: skill '{}' already exists at {} — refused",
                args.name,
                candidate.display()
            );
        }
    }

    let skill_dir = tier_root.join(&args.name);
    std::fs::create_dir_all(&tier_root).map_err(|e| {
        anyhow!(
            "manage_skill.create: create_dir_all {}: {e}",
            tier_root.display()
        )
    })?;

    let learner_facing = args.learner_facing.unwrap_or(true);
    let editable = args.editable_by_learner.unwrap_or(true);

    // Atomic write via tempdir + rename.
    let tmp = tier_root.join(format!(".{}.tmp-{}", args.name, nanos()));
    std::fs::create_dir_all(&tmp)?;
    let mut guard = TmpDirGuard {
        path: Some(tmp.clone()),
    };

    // Compose SKILL.md frontmatter.
    let metrics_block = match &args.metrics {
        Some(ms) if !ms.is_empty() => {
            let yaml = serde_yaml::to_string(ms)?;
            format!("metrics:\n{}", indent(&yaml, 2))
        }
        _ => String::new(),
    };
    let skill_md = format!(
        "---\nname: {name}\ndescription: {desc}\nlearner_facing: {lf}\neditable_by_learner: {ed}\n{metrics}---\n\n{body}\n",
        name = args.name,
        desc = description,
        lf = learner_facing,
        ed = editable,
        metrics = metrics_block,
        body = body.trim_end(),
    );
    std::fs::write(tmp.join("SKILL.md"), skill_md)?;

    // resources/ + README.md stub
    let resources_dir = tmp.join("resources");
    std::fs::create_dir_all(&resources_dir)?;
    std::fs::write(
        resources_dir.join("README.md"),
        "# Resources\n\n\
         This folder holds the skill's reference material — markdown notes, \
         web links, PDFs, etc. Agents read these via the `view_skill` tool \
         (action=read_resource), not `read_file` — the agent's workspace is a \
         tmp dir and doesn't see this path.\n\n\
         Populate via `manage_skill(action=\"add_resource\", ...)` or by \
         dropping files here directly.\n",
    )?;

    // evals/prompts.yaml stub
    std::fs::create_dir_all(tmp.join("evals"))?;
    std::fs::write(
        tmp.join("evals").join("prompts.yaml"),
        "schema_version: 1\nfixtures:\n  - id: f1\n    prompt: \"<TODO: a typical question a learner would ask>\"\n    expected_tool_calls: []\n    expected_outcome: \"\"\n    metrics: []\n",
    )?;

    // Atomic rename — last step.
    std::fs::rename(&tmp, &skill_dir).map_err(|e| {
        anyhow!(
            "manage_skill.create: rename {} -> {}: {e}",
            tmp.display(),
            skill_dir.display()
        )
    })?;
    guard.path = None;

    audit_log(
        workspace,
        "manage_skill.create",
        &args.name,
        Some(format!("tier={}", tier)),
    );

    let rel_path = skill_dir
        .strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| skill_dir.display().to_string());

    Ok(ToolOutput::from(
        json!({
            "action": "create",
            "skill": args.name,
            "tier": tier,
            "path": rel_path,
            "loaded_in_current_conversation": false,
        })
        .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// edit_body
// ---------------------------------------------------------------------------

fn do_edit_body(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    let body = args
        .body
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.edit_body: 'body' is required"))?;

    let skill_dir = locate_skill_dir(&args.name, workspace).ok_or_else(|| {
        anyhow!(
            "manage_skill.edit_body: no skill '{}' found in any tier",
            args.name
        )
    })?;
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        bail!(
            "manage_skill.edit_body: SKILL.md missing at {}",
            skill_md.display()
        );
    }
    if !is_editable_by_learner(&skill_md)? {
        bail!(
            "manage_skill.edit_body: skill '{}' has `editable_by_learner: false` in frontmatter — refused.",
            args.name
        );
    }

    // Use evolve::apply_proposal_to_skill_md — it preserves frontmatter and
    // writes atomically. `metrics: None` keeps the existing metrics block.
    let proposal = ProposalBody {
        body_markdown: body.to_string(),
        metrics: None,
        rationale: String::new(),
        resource_additions: None,
    };
    apply_proposal_to_skill_md(&skill_md, &proposal)?;

    audit_log(workspace, "manage_skill.edit_body", &args.name, None);

    let rel_path = skill_md
        .strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| skill_md.display().to_string());

    Ok(ToolOutput::from(
        json!({
            "action": "edit_body",
            "skill": args.name,
            "path": rel_path,
            "loaded_in_current_conversation": false,
        })
        .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// patch
// ---------------------------------------------------------------------------

fn do_patch(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    let old_string = args
        .old_string
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.patch: 'old_string' is required"))?;
    let new_string = args
        .new_string
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.patch: 'new_string' is required"))?;
    if old_string.is_empty() {
        bail!("manage_skill.patch: 'old_string' must not be empty");
    }
    let replace_all = args.replace_all.unwrap_or(false);
    let file_path = args.file_path.as_deref().unwrap_or("SKILL.md");

    let skill_dir = locate_skill_dir(&args.name, workspace).ok_or_else(|| {
        anyhow!(
            "manage_skill.patch: no skill '{}' found in any tier",
            args.name
        )
    })?;
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        bail!(
            "manage_skill.patch: SKILL.md missing at {}",
            skill_md.display()
        );
    }
    if !is_editable_by_learner(&skill_md)? {
        bail!(
            "manage_skill.patch: skill '{}' has `editable_by_learner: false` in frontmatter — refused.",
            args.name
        );
    }

    // Resolve target file. Validate it stays under skill_dir.
    let target = resolve_patch_target(&skill_dir, file_path)?;
    if !target.is_file() {
        bail!(
            "manage_skill.patch: target file {} does not exist",
            target.display()
        );
    }

    let original = std::fs::read_to_string(&target)
        .map_err(|e| anyhow!("manage_skill.patch: read {}: {e}", target.display()))?;

    let (patched, match_count) =
        fuzzy_find_and_replace(&original, old_string, new_string, replace_all)?;

    // If editing SKILL.md, post-patch frontmatter must still parse.
    let target_is_skill_md = target == skill_md;
    if target_is_skill_md {
        validate_post_patch_frontmatter(&patched).map_err(|e| {
            anyhow!("manage_skill.patch: post-patch frontmatter invalid — refused: {e}")
        })?;
    }

    // Atomic write via temp + rename.
    atomic_write(&target, &patched)?;

    audit_log(
        workspace,
        "manage_skill.patch",
        &args.name,
        Some(format!("file={};matches={}", file_path, match_count)),
    );

    Ok(ToolOutput::from(
        json!({
            "action": "patch",
            "skill": args.name,
            "file_path": file_path,
            "match_count": match_count,
            "loaded_in_current_conversation": false,
        })
        .to_string(),
    ))
}

/// Resolve `file_path` relative to `skill_dir`, refusing anything that
/// escapes the directory (via `..`, absolute components, or symlinks).
fn resolve_patch_target(skill_dir: &Path, file_path: &str) -> Result<PathBuf> {
    if file_path.is_empty() {
        bail!("manage_skill.patch: file_path is empty");
    }
    let p = Path::new(file_path);
    for c in p.components() {
        match c {
            Component::ParentDir => {
                bail!(
                    "manage_skill.patch: file_path '{}' contains '..' — refused",
                    file_path
                )
            }
            Component::RootDir | Component::Prefix(_) => bail!(
                "manage_skill.patch: file_path '{}' is absolute — refused",
                file_path
            ),
            _ => {}
        }
    }
    let joined = skill_dir.join(p);

    // Symlink-safe prefix check via canonicalize. The skill_dir always
    // exists here; the joined target may not (file-not-found is reported by
    // caller). If canonicalize fails on joined, fall back to a lexical
    // prefix check — components have already been validated.
    let canon_skill = std::fs::canonicalize(skill_dir)
        .map_err(|e| anyhow!("manage_skill.patch: canonicalize skill_dir: {e}"))?;
    if let Ok(canon_target) = std::fs::canonicalize(&joined) {
        if !canon_target.starts_with(&canon_skill) {
            bail!(
                "manage_skill.patch: file_path '{}' resolves outside skill_dir (symlink escape?)",
                file_path
            );
        }
    }
    Ok(joined)
}

/// v1 fuzzy finder: tries (a) exact match, then (b) whitespace-normalized
/// match (collapse runs of whitespace to single space, both haystack and
/// needle). Returns the patched content and the count of replacements made.
///
/// TODO: hermes ships 8 strategies (line-trimmed, indent-flex,
/// escape-normalized, trimmed-boundary, unicode-normalized, block-anchor,
/// context-aware, …). Land them here if drift remains a problem.
fn fuzzy_find_and_replace(
    haystack: &str,
    needle: &str,
    replacement: &str,
    replace_all: bool,
) -> Result<(String, usize)> {
    // Strategy (a): exact match.
    let exact_count = count_occurrences(haystack, needle);
    if exact_count > 0 {
        if exact_count > 1 && !replace_all {
            bail!(
                "manage_skill.patch: old_string matched {} times — pass replace_all=true to apply all, or include more context to disambiguate",
                exact_count
            );
        }
        let patched = if replace_all {
            haystack.replace(needle, replacement)
        } else {
            // Single replacement (we know exact_count == 1 here).
            haystack.replacen(needle, replacement, 1)
        };
        return Ok((patched, exact_count));
    }

    // Strategy (b): whitespace-normalized match.
    // Build a normalized haystack alongside an index map back to original
    // byte offsets, then locate the normalized needle and translate matches
    // back to (start, end) ranges in the original.
    let (norm_hay, idx_map) = normalize_ws_with_index(haystack);
    let (norm_needle, _) = normalize_ws_with_index(needle);
    if norm_needle.is_empty() {
        let preview: String = haystack.chars().take(500).collect();
        bail!(
            "manage_skill.patch: old_string not found in target (and normalized form is empty). First 500 chars: {}",
            preview
        );
    }

    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = norm_hay[search_from..].find(&norm_needle) {
        let n_start = search_from + rel;
        let n_end = n_start + norm_needle.len();
        // Map back: norm_hay byte index `i` came from idx_map[i] in original.
        // The end offset is one past the last contributing original byte.
        let orig_start = idx_map.get(n_start).copied().unwrap_or(haystack.len());
        let orig_end_inclusive = if n_end == 0 {
            0
        } else {
            idx_map.get(n_end - 1).copied().unwrap_or(haystack.len())
        };
        // `orig_end_exclusive`: walk forward to the next char boundary.
        let orig_end_exclusive = next_char_boundary(haystack, orig_end_inclusive);
        matches.push((orig_start, orig_end_exclusive));
        // Advance past this match in the normalized space.
        search_from = n_end;
    }

    if matches.is_empty() {
        let preview: String = haystack.chars().take(500).collect();
        bail!(
            "manage_skill.patch: old_string not found in target. First 500 chars: {}",
            preview
        );
    }
    if matches.len() > 1 && !replace_all {
        bail!(
            "manage_skill.patch: old_string matched {} times (whitespace-normalized) — pass replace_all=true to apply all, or include more context to disambiguate",
            matches.len()
        );
    }

    // Apply replacements right-to-left to avoid offset shift.
    let mut out = haystack.to_string();
    for (start, end) in matches.iter().rev() {
        out.replace_range(*start..*end, replacement);
    }
    Ok((out, matches.len()))
}

/// Count exact substring occurrences (non-overlapping).
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        n += 1;
        start += rel + needle.len();
    }
    n
}

/// Build a whitespace-normalized version of `s` and a map from each byte
/// offset in the normalized output back to a byte offset in the original.
/// Runs of whitespace (Unicode `is_whitespace`) collapse to a single ASCII
/// space; the map records the byte offset of the first whitespace char in
/// that run for the collapsed-space output position.
///
/// Leading whitespace is dropped (no leading space in the output);
/// trailing whitespace is also dropped. The returned `idx` has one entry
/// per BYTE in the normalized output — for multi-byte chars, all bytes
/// share the same source offset (start of the char). This is sufficient
/// for the back-mapping the caller does (it only consults the start and
/// end positions of a substring match).
fn normalize_ws_with_index(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut idx: Vec<usize> = Vec::with_capacity(s.len());
    let mut prev_ws = false;
    let mut byte_offset = 0usize;

    for ch in s.chars() {
        let ch_start = byte_offset;
        let ch_len = ch.len_utf8();

        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                // Tentatively emit a single space — may be popped at end if
                // it turns out to be trailing.
                out.push(' ');
                idx.push(ch_start);
            }
            prev_ws = true;
        } else {
            // Encode the char into utf-8; push each byte to `out` and map
            // each byte index back to `ch_start`.
            let mut buf = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buf);
            // Safety: we're appending valid utf-8 bytes from `encoded` to a
            // String that already holds valid utf-8.
            // SAFETY: `encoded` is a valid utf-8 substring per `encode_utf8`.
            unsafe {
                out.as_mut_vec().extend_from_slice(encoded.as_bytes());
            }
            for _ in 0..ch_len {
                idx.push(ch_start);
            }
            prev_ws = false;
        }
        byte_offset += ch_len;
    }

    // Trim a trailing space, if any.
    if out.ends_with(' ') {
        out.pop();
        idx.pop();
    }
    debug_assert_eq!(
        out.len(),
        idx.len(),
        "idx must have one entry per output byte"
    );
    (out, idx)
}

/// Walk forward from `pos` to the next utf-8 char boundary in `s`. If `pos`
/// is already a boundary, returns `pos + len_of_next_char` (i.e. the end of
/// the char that starts at `pos`). Used to convert an inclusive end offset
/// to an exclusive one.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    // s[pos..] starts with a char; advance one char.
    match s[pos..].chars().next() {
        Some(c) => pos + c.len_utf8(),
        None => s.len(),
    }
}

fn validate_post_patch_frontmatter(skill_md: &str) -> Result<()> {
    let rest = skill_md
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow!("missing leading frontmatter marker '---\\n'"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("frontmatter not closed (missing '\\n---')"))?;
    let fm_block = &rest[..end + 1];
    let parsed: serde_yaml::Value = serde_yaml::from_str(fm_block)
        .map_err(|e| anyhow!("frontmatter YAML parse failure: {e}"))?;
    // Minimal structural check: name + description must remain present.
    if parsed.get("name").is_none() {
        bail!("frontmatter missing required 'name' key");
    }
    if parsed.get("description").is_none() {
        bail!("frontmatter missing required 'description' key");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// add_resource / remove_resource
// ---------------------------------------------------------------------------

fn do_add_resource(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    let path = args
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.add_resource: 'path' is required"))?;
    let content = args
        .content
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.add_resource: 'content' is required"))?;
    if path.len() > 256 {
        bail!("manage_skill.add_resource: 'path' too long (>256 chars)");
    }
    validate_resource_path(path)?;

    let skill_dir = locate_skill_dir(&args.name, workspace).ok_or_else(|| {
        anyhow!(
            "manage_skill.add_resource: no skill '{}' found in any tier",
            args.name
        )
    })?;
    let skill_md = skill_dir.join("SKILL.md");
    if !is_editable_by_learner(&skill_md)? {
        bail!(
            "manage_skill.add_resource: skill '{}' has `editable_by_learner: false` — refused.",
            args.name
        );
    }

    let dest = skill_dir.join("resources").join(path);
    let overwrite = args.overwrite.unwrap_or(false);
    let overwrote = dest.exists();
    if overwrote && !overwrite {
        bail!(
            "manage_skill.add_resource: file {} already exists — pass overwrite=true to replace",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&dest, content)?;

    audit_log(
        workspace,
        "manage_skill.add_resource",
        &args.name,
        Some(format!("path={};overwrote={}", path, overwrote)),
    );

    Ok(ToolOutput::from(
        json!({
            "action": "add_resource",
            "skill": args.name,
            "path": format!("resources/{}", path),
            "overwrote": overwrote,
            "loaded_in_current_conversation": false,
        })
        .to_string(),
    ))
}

fn do_remove_resource(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    let path = args
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("manage_skill.remove_resource: 'path' is required"))?;
    validate_resource_path(path)?;

    let skill_dir = locate_skill_dir(&args.name, workspace).ok_or_else(|| {
        anyhow!(
            "manage_skill.remove_resource: no skill '{}' found in any tier",
            args.name
        )
    })?;
    let skill_md = skill_dir.join("SKILL.md");
    if !is_editable_by_learner(&skill_md)? {
        bail!(
            "manage_skill.remove_resource: skill '{}' has `editable_by_learner: false` — refused.",
            args.name
        );
    }

    let dest = skill_dir.join("resources").join(path);
    if !dest.exists() {
        bail!(
            "manage_skill.remove_resource: file {} does not exist",
            dest.display()
        );
    }
    std::fs::remove_file(&dest).map_err(|e| {
        anyhow!(
            "manage_skill.remove_resource: remove {}: {e}",
            dest.display()
        )
    })?;

    audit_log(
        workspace,
        "manage_skill.remove_resource",
        &args.name,
        Some(format!("path={}", path)),
    );

    Ok(ToolOutput::from(
        json!({
            "action": "remove_resource",
            "skill": args.name,
            "path": format!("resources/{}", path),
            "loaded_in_current_conversation": false,
        })
        .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

fn do_delete(args: &Args, workspace: &Path) -> Result<ToolOutput> {
    // Locked-tiers rule (mirror `tengu skill remove`): default to project,
    // require explicit `tier` for non-project locations.
    let skill_dir = if let Some(tier) = args.tier.as_deref() {
        let dir = tier_root(workspace, tier)?.join(&args.name);
        if !dir.is_dir() {
            bail!(
                "manage_skill.delete: no skill '{}' at tier='{}' ({})",
                args.name,
                tier,
                dir.display()
            );
        }
        dir
    } else {
        let project = workspace.join("skills").join(&args.name);
        if !project.is_dir() {
            // If a non-project tier holds it, demand an explicit param.
            for cand in collision_candidates(workspace, &args.name) {
                if cand.is_dir() && cand != project {
                    bail!(
                        "manage_skill.delete: skill '{}' lives at {} — pass tier='workspace' or tier='managed' explicitly",
                        args.name,
                        cand.display()
                    );
                }
            }
            bail!(
                "manage_skill.delete: no skill '{}' found in any tier",
                args.name
            );
        }
        project
    };

    // Refuse if an active evolve worktree exists.
    let worktree_root = workspace.join(".tengu").join("worktrees");
    if worktree_root.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&worktree_root) {
            for e in rd.flatten() {
                let fname = e.file_name();
                let s = fname.to_string_lossy();
                if s.starts_with(&format!("evolve-{}-", args.name)) {
                    bail!(
                        "manage_skill.delete: active evolve worktree {} blocks delete; finish or sweep it first",
                        e.path().display()
                    );
                }
            }
        }
    }

    let removed_path = skill_dir.display().to_string();
    std::fs::remove_dir_all(&skill_dir).map_err(|e| {
        anyhow!(
            "manage_skill.delete: remove_dir_all {}: {e}",
            skill_dir.display()
        )
    })?;

    audit_log(workspace, "manage_skill.delete", &args.name, None);

    Ok(ToolOutput::from(
        json!({
            "action": "delete",
            "skill": args.name,
            "removed_path": removed_path,
        })
        .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Helpers
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

fn tier_root(workspace: &Path, tier: &str) -> Result<PathBuf> {
    match tier {
        "project" => Ok(workspace.join("skills")),
        "workspace" => Ok(workspace.join(".tengu").join("skills")),
        "managed" => dirs_next::home_dir()
            .map(|h| h.join(".tengu").join("skills"))
            .ok_or_else(|| anyhow!("manage_skill: cannot resolve managed-tier root (no $HOME)")),
        other => bail!(
            "manage_skill: unknown tier '{}' (expected project | workspace | managed)",
            other
        ),
    }
}

fn collision_candidates(workspace: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = vec![
        workspace.join("skills").join(name),
        workspace.join(".tengu").join("skills").join(name),
    ];
    if let Some(home) = dirs_next::home_dir() {
        out.push(home.join(".tengu").join("skills").join(name));
    }
    out
}

/// Three-tier walk: managed → workspace → project, first match wins. Mirrors
/// `apply_improver_proposal::locate_skill_dir` and `application/skills/registry.rs::skill_directories`.
fn locate_skill_dir(name: &str, workspace: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(home.join(".tengu").join("skills").join(name));
    }
    candidates.push(workspace.join(".tengu").join("skills").join(name));
    candidates.push(workspace.join("skills").join(name));
    candidates.into_iter().find(|p| p.is_dir())
}

fn atomic_write(target: &Path, content: &str) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("atomic_write: {} has no parent", target.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp-{}", filename_of(target), nanos()));
    std::fs::write(&tmp, content)
        .map_err(|e| anyhow!("atomic_write: write tmp {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, target).map_err(|e| {
        anyhow!(
            "atomic_write: rename {} -> {}: {e}",
            tmp.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn filename_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string())
}

fn audit_log(workspace: &Path, op: &str, name: &str, source: Option<String>) {
    let entry = audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        op: op.to_string(),
        name: name.to_string(),
        verdict: None,
        source,
        sha256: None,
    };
    if let Err(e) = audit::append(workspace, entry) {
        tracing::warn!(error = %e, "manage_skill: audit append failed (non-fatal)");
    }
}

fn indent(s: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    s.lines().map(|l| format!("{pad}{l}\n")).collect()
}

/// RAII guard: best-effort `remove_dir_all` of a tmp dir on drop. Disarmed
/// by setting `path = None` after a successful atomic rename.
struct TmpDirGuard {
    path: Option<PathBuf>,
}

impl Drop for TmpDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(workspace: &Path, name: &str, editable: Option<bool>, body: &str) -> PathBuf {
        let skill_dir = workspace.join("skills").join(name);
        std::fs::create_dir_all(skill_dir.join("resources")).unwrap();
        let flag = match editable {
            Some(true) => "editable_by_learner: true\n",
            Some(false) => "editable_by_learner: false\n",
            None => "",
        };
        let md = format!(
            "---\nname: {}\ndescription: test skill\n{}---\n\n{}\n",
            name, flag, body
        );
        std::fs::write(skill_dir.join("SKILL.md"), md).unwrap();
        skill_dir
    }

    fn make_args(action: &str, name: &str) -> Args {
        Args {
            action: action.to_string(),
            name: name.to_string(),
            description: None,
            body: None,
            tier: None,
            learner_facing: None,
            editable_by_learner: None,
            metrics: None,
            old_string: None,
            new_string: None,
            file_path: None,
            replace_all: None,
            path: None,
            content: None,
            overwrite: None,
        }
    }

    // --------- create -----------------------------------------------------

    #[test]
    fn create_writes_skill_md_resources_evals() {
        let ws = TempDir::new().unwrap();
        let mut a = make_args("create", "demo-skill");
        a.description = Some("Use when demoing.".into());
        a.body = Some("# Demo\n\nProcedure.\n".into());

        let out = do_create(&a, ws.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["action"], "create");
        assert_eq!(v["loaded_in_current_conversation"], false);

        let dir = ws.path().join("skills/demo-skill");
        assert!(dir.join("SKILL.md").is_file());
        assert!(dir.join("resources").is_dir());
        assert!(dir.join("resources/README.md").is_file());
        assert!(dir.join("evals/prompts.yaml").is_file());

        let md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(md.contains("name: demo-skill"));
        assert!(md.contains("editable_by_learner: true"));
        assert!(md.contains("# Demo"));
    }

    #[test]
    fn create_refuses_collision() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "dupe", Some(true), "# x");
        let mut a = make_args("create", "dupe");
        a.description = Some("d".into());
        a.body = Some("b".into());
        let err = do_create(&a, ws.path()).unwrap_err();
        assert!(format!("{err}").contains("already exists"), "{err}");
    }

    // --------- edit_body --------------------------------------------------

    #[test]
    fn edit_body_replaces_body_preserves_frontmatter() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# OLD BODY");

        let mut a = make_args("edit_body", "demo");
        a.body = Some("# NEW BODY".into());
        do_edit_body(&a, ws.path()).unwrap();

        let md = std::fs::read_to_string(ws.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(
            md.contains("name: demo"),
            "frontmatter name preserved: {md}"
        );
        assert!(md.contains("# NEW BODY"));
        assert!(!md.contains("# OLD BODY"));
    }

    #[test]
    fn edit_body_refuses_on_editable_by_learner_false() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "locked", Some(false), "# body");

        let mut a = make_args("edit_body", "locked");
        a.body = Some("# new".into());
        let err = do_edit_body(&a, ws.path()).unwrap_err();
        assert!(
            format!("{err}").contains("editable_by_learner"),
            "expected editable_by_learner error: {err}"
        );
    }

    // --------- patch ------------------------------------------------------

    #[test]
    fn patch_exact_match_replaces() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "Hello world. Greetings.");

        let mut a = make_args("patch", "demo");
        a.old_string = Some("Hello world".into());
        a.new_string = Some("Hi there".into());
        let out = do_patch(&a, ws.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["match_count"], 1);

        let md = std::fs::read_to_string(ws.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(md.contains("Hi there"));
        assert!(!md.contains("Hello world"));
    }

    #[test]
    fn patch_zero_match_returns_helpful_error() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "totally different content");

        let mut a = make_args("patch", "demo");
        a.old_string = Some("not present anywhere".into());
        a.new_string = Some("x".into());
        let err = do_patch(&a, ws.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("First 500 chars"), "{msg}");
    }

    #[test]
    fn patch_multiple_match_refuses_without_replace_all() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "foo foo foo");

        let mut a = make_args("patch", "demo");
        a.old_string = Some("foo".into());
        a.new_string = Some("bar".into());
        let err = do_patch(&a, ws.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("matched"), "{msg}");
        assert!(msg.contains("replace_all=true"), "{msg}");
    }

    #[test]
    fn patch_replace_all_replaces_every_occurrence() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "foo foo foo");

        let mut a = make_args("patch", "demo");
        a.old_string = Some("foo".into());
        a.new_string = Some("bar".into());
        a.replace_all = Some(true);
        let out = do_patch(&a, ws.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["match_count"], 3);
        let md = std::fs::read_to_string(ws.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(md.contains("bar bar bar"), "got: {md}");
        assert!(!md.contains("foo"));
    }

    #[test]
    fn patch_whitespace_normalised_match() {
        let ws = TempDir::new().unwrap();
        // Original has a single space; the agent's old_string has multiple
        // spaces / tabs — strategy (b) should still find it.
        write_skill(ws.path(), "demo", Some(true), "alpha beta gamma");

        let mut a = make_args("patch", "demo");
        a.old_string = Some("alpha   \t beta".into());
        a.new_string = Some("ALPHA BETA".into());
        let out = do_patch(&a, ws.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["match_count"], 1);
        let md = std::fs::read_to_string(ws.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(md.contains("ALPHA BETA"), "got: {md}");
        assert!(md.contains("gamma"));
    }

    #[test]
    fn patch_post_patch_frontmatter_validation_refuses_breaking_change() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");

        // Try to patch out the closing frontmatter marker — must refuse.
        let mut a = make_args("patch", "demo");
        a.old_string = Some("---\n\n# body".into());
        a.new_string = Some("(boom)\n\n# body".into());
        let err = do_patch(&a, ws.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("frontmatter"),
            "expected frontmatter validation error: {msg}"
        );
        // Original should still be readable / unchanged.
        let md = std::fs::read_to_string(ws.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(md.contains("name: demo"));
    }

    // --------- add_resource / remove_resource -----------------------------

    #[test]
    fn add_resource_writes_atomically() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");

        let mut a = make_args("add_resource", "demo");
        a.path = Some("notes/intro.md".into());
        a.content = Some("# Notes\n".into());
        let out = do_add_resource(&a, ws.path()).unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["overwrote"], false);

        let target = ws.path().join("skills/demo/resources/notes/intro.md");
        assert!(target.is_file(), "expected {} to exist", target.display());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# Notes\n");
        // No leftover .tmp- siblings.
        let parent = target.parent().unwrap();
        for entry in std::fs::read_dir(parent).unwrap() {
            let n = entry.unwrap().file_name();
            let s = n.to_string_lossy();
            assert!(!s.contains(".tmp-"), "leftover temp file: {s}");
        }
    }

    #[test]
    fn add_resource_refuses_overwrite_without_flag() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");
        std::fs::write(ws.path().join("skills/demo/resources/x.md"), "ORIG").unwrap();

        let mut a = make_args("add_resource", "demo");
        a.path = Some("x.md".into());
        a.content = Some("NEW".into());
        let err = do_add_resource(&a, ws.path()).unwrap_err();
        assert!(format!("{err}").contains("already exists"));

        // With overwrite=true → succeeds.
        a.overwrite = Some(true);
        do_add_resource(&a, ws.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("skills/demo/resources/x.md")).unwrap(),
            "NEW"
        );
    }

    #[test]
    fn add_resource_rejects_traversal_path() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");

        let mut a = make_args("add_resource", "demo");
        a.path = Some("../escape.md".into());
        a.content = Some("oops".into());
        let err = do_add_resource(&a, ws.path()).unwrap_err();
        assert!(
            format!("{err}").contains(".."),
            "expected '..' rejection: {err}"
        );
    }

    #[test]
    fn remove_resource_deletes_file() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");
        std::fs::write(ws.path().join("skills/demo/resources/gone.md"), "data").unwrap();

        let mut a = make_args("remove_resource", "demo");
        a.path = Some("gone.md".into());
        do_remove_resource(&a, ws.path()).unwrap();
        assert!(!ws.path().join("skills/demo/resources/gone.md").exists());
    }

    // --------- delete -----------------------------------------------------

    #[test]
    fn delete_refuses_with_active_evolve_worktree() {
        let ws = TempDir::new().unwrap();
        write_skill(ws.path(), "demo", Some(true), "# body");
        let wt = ws.path().join(".tengu/worktrees/evolve-demo-123");
        std::fs::create_dir_all(&wt).unwrap();

        let a = make_args("delete", "demo");
        let err = do_delete(&a, ws.path()).unwrap_err();
        assert!(format!("{err}").contains("active evolve worktree"), "{err}");
        // Skill must still exist.
        assert!(ws.path().join("skills/demo").is_dir());
    }

    // --------- fuzzy_find_and_replace unit tests --------------------------

    #[test]
    fn fuzzy_exact_first_strategy_wins() {
        let (out, n) = fuzzy_find_and_replace("hello world", "hello", "HI", false).unwrap();
        assert_eq!(out, "HI world");
        assert_eq!(n, 1);
    }

    #[test]
    fn fuzzy_whitespace_normalized_collapses_runs() {
        // Original has single spaces; needle has tabs and double spaces.
        let (out, n) =
            fuzzy_find_and_replace("alpha beta gamma", "alpha\t\tbeta", "X", false).unwrap();
        assert_eq!(n, 1);
        assert!(out.contains("X gamma"), "got: {out}");
    }
}
