//! Skill eval runner — `tengu eval <skill>`.
//!
//! Spec: `docs/superpowers/specs/2026-04-19-eval-runner-design.md`.
//! Replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent,
//! scores each row pass/fail via an LLM judge, and writes a report.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct EvalArgs {
    pub skills: Vec<String>,
    pub sandbox: Option<String>,
    pub judge_model: Option<String>,
    pub concurrency: usize,
    pub format: OutputFormat,
    pub out_dir: Option<PathBuf>,
    pub filter: Option<String>,
    pub keep_workspace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

pub async fn run(args: EvalArgs) -> Result<i32> {
    // Scaffold — Task 9 replaces this body with the real driver.
    let _ = args;
    println!("tengu eval: scaffold");
    Ok(0)
}

#[derive(Debug, Clone)]
pub struct PromptRow {
    pub id: String,
    pub prompt: String,
    pub expected: String,
    pub timeout_secs: u64,
    pub stubs: Vec<StubSpec>,
}

#[derive(Debug, Clone)]
pub struct StubSpec {
    pub tool: String,
    pub responses: Vec<serde_json::Value>,
}

pub fn derive_row_id(prompt: &str) -> String {
    let mut s: String = prompt
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let truncated: String = s.trim_matches('-').chars().take(64).collect();
    truncated.trim_end_matches('-').to_string()
}

pub fn parse_markdown_prompts(body: &str) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut lines = body.lines().peekable();
    let mut in_table = false;
    while let Some(line) = lines.next() {
        if !in_table {
            let lower = line.to_ascii_lowercase();
            if lower.contains("| prompt") && lower.contains("expected") {
                lines.next(); // skip the `|---|---|` separator
                in_table = true;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if !trimmed.starts_with('|') {
            break;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        if cells.len() < 2 {
            continue;
        }
        let prompt_cell = cells[0].trim().trim_matches('"').trim();
        let expected_cell = cells[1].trim();
        if prompt_cell.is_empty() {
            continue;
        }
        let id = derive_row_id(prompt_cell);
        if !seen.insert(id.clone()) {
            bail!("duplicate row id '{}' in markdown prompts", id);
        }
        rows.push(PromptRow {
            id,
            prompt: prompt_cell.to_string(),
            expected: expected_cell.to_string(),
            timeout_secs: default_timeout_secs(),
            stubs: Vec::new(),
        });
    }

    if !in_table {
        bail!("no prompts table found — expected '| Prompt | Expected behaviour |' header");
    }
    if rows.is_empty() {
        bail!("prompts table has zero rows");
    }
    Ok(rows)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlRow {
    id: String,
    prompt: String,
    expected: String,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    stubs: Vec<YamlStub>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlStub {
    tool: String,
    responses: Vec<serde_json::Value>,
}

fn default_timeout_secs() -> u64 {
    120
}

pub fn parse_yaml_prompts(body: &str) -> Result<Vec<PromptRow>> {
    let raw: Vec<YamlRow> =
        serde_yaml::from_str(body).context("yaml prompts parse failed")?;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::with_capacity(raw.len());
    for r in raw {
        if !seen.insert(r.id.clone()) {
            bail!("duplicate row id '{}' in yaml prompts", r.id);
        }
        rows.push(PromptRow {
            id: r.id,
            prompt: r.prompt,
            expected: r.expected,
            timeout_secs: r.timeout_secs,
            stubs: r
                .stubs
                .into_iter()
                .map(|s| StubSpec {
                    tool: s.tool,
                    responses: s.responses,
                })
                .collect(),
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTier {
    Managed,
    Workspace,
    Project,
}

impl SkillTier {
    pub fn label(self) -> &'static str {
        match self {
            SkillTier::Managed => "managed",
            SkillTier::Workspace => "workspace",
            SkillTier::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillUnderTest {
    pub name: String,
    pub tier: SkillTier,
    pub evals_dir: PathBuf,
    pub prompts_path: PathBuf,
    pub prompts_format: String, // "markdown" or "yaml"
    pub config_path: PathBuf,
}

pub fn default_skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs_next::home_dir() {
        roots.push(home.join(".tengu").join("skills"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".tengu").join("skills"));
        roots.push(cwd.join("skills"));
    }
    roots
}

fn tier_for_root(root: &Path) -> SkillTier {
    if let Some(home) = dirs_next::home_dir() {
        if root.starts_with(home.join(".tengu").join("skills")) {
            return SkillTier::Managed;
        }
    }
    let s = root.to_string_lossy();
    if s.ends_with("/.tengu/skills") || s.contains("/.tengu/skills/") {
        return SkillTier::Workspace;
    }
    SkillTier::Project
}

pub fn discover_skills(
    filter: &[String],
    roots: &[PathBuf],
) -> anyhow::Result<Vec<SkillUnderTest>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new(); // dedup by name; first tier wins
    for root in roots {
        if !root.exists() {
            continue;
        }
        let tier = tier_for_root(root);
        for entry in std::fs::read_dir(root)
            .with_context(|| format!("read_dir {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !filter.is_empty() && !filter.iter().any(|s| s == &name) {
                continue;
            }
            let evals_dir = entry.path().join("evals");
            if !evals_dir.exists() {
                continue;
            }
            let yaml = evals_dir.join("prompts.yaml");
            let md = evals_dir.join("prompts.md");
            let (prompts_path, fmt) = if yaml.exists() {
                (yaml, "yaml")
            } else if md.exists() {
                (md, "markdown")
            } else {
                continue;
            };
            // Dedup happens last, once we know this directory is a real evaluable skill.
            if !seen.insert(name.clone()) {
                continue;
            }
            let config_path = evals_dir.join("config.toml");
            out.push(SkillUnderTest {
                name,
                tier,
                evals_dir: evals_dir.clone(),
                prompts_path,
                prompts_format: fmt.to_string(),
                config_path,
            });
        }
    }
    if !filter.is_empty() {
        for wanted in filter {
            if !out.iter().any(|s| &s.name == wanted) {
                anyhow::bail!("skill '{}' has no evals/prompts.{{md,yaml}}", wanted);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_parses_well_formed_table() {
        let body = r#"# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
"#;
        let rows = parse_markdown_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].prompt, "research paper X then mint it as an IP token");
        assert_eq!(
            rows[0].expected,
            "Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`."
        );
        assert_eq!(rows[0].id, "research-paper-x-then-mint-it-as-an-ip-token");
        assert_eq!(rows[1].id, "what-s-2-2");
        assert_eq!(rows[0].timeout_secs, 120);
        assert!(rows[0].stubs.is_empty());
    }

    #[test]
    fn markdown_rejects_duplicate_row_ids() {
        let body = r#"| Prompt | Expected behaviour |
|---|---|
| "hello" | do nothing |
| "hello" | do something else |
"#;
        let err = parse_markdown_prompts(body).unwrap_err();
        assert!(err.to_string().contains("duplicate row id"), "got: {}", err);
    }

    #[test]
    fn markdown_requires_header_row() {
        let body = "# just a heading, no table\n\nno rows here";
        let err = parse_markdown_prompts(body).unwrap_err();
        assert!(err.to_string().contains("no prompts table"), "got: {}", err);
    }

    #[test]
    fn row_id_kebabs_truncates_collapses_dashes() {
        assert_eq!(
            derive_row_id("Research paper X, then mint it!"),
            "research-paper-x-then-mint-it"
        );
        assert_eq!(derive_row_id(""), "");
        let long = "a".repeat(80);
        assert_eq!(derive_row_id(&long).len(), 64);
    }

    #[test]
    fn row_id_trims_trailing_dash_after_truncation() {
        // 63 alphanumerics + one separator → without the fix, this would truncate to
        // 64 chars ending in `-`. With the fix, the trailing dash is stripped.
        let prompt = format!("{}!suffix", "a".repeat(63));
        let id = derive_row_id(&prompt);
        assert!(!id.ends_with('-'), "id should not end with dash, got: {:?}", id);
        assert_eq!(id.len(), 63, "id length after trailing-dash trim should be 63, got {}", id.len());
    }

    #[test]
    fn yaml_parses_well_formed() {
        let body = r#"
- id: seq-research-mint
  prompt: "research paper X then mint it as an IP token"
  expected: "Sequential sessions_spawn(researcher) then sessions_spawn(minter)."
- id: fail-503-retry
  prompt: "my trade failed with HTTP 503"
  expected: "Retry the same call. No decomposition."
  timeout_secs: 60
  stubs:
    - tool: http_request
      responses:
        - { status: 503, body: "Service Unavailable" }
        - { status: 200, body: "{\"ok\": true}" }
"#;
        let rows = parse_yaml_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "seq-research-mint");
        assert_eq!(rows[0].timeout_secs, 120);
        assert_eq!(rows[1].timeout_secs, 60);
        assert_eq!(rows[1].stubs.len(), 1);
        assert_eq!(rows[1].stubs[0].tool, "http_request");
        assert_eq!(rows[1].stubs[0].responses.len(), 2);
    }

    #[test]
    fn yaml_rejects_unknown_keys() {
        let body = r#"
- id: x
  prompt: "hello"
  expected: "ok"
  oops_unknown_field: true
"#;
        let err = parse_yaml_prompts(body).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("unknown field"), "got: {}", msg);
    }

    #[test]
    fn yaml_rejects_duplicate_ids() {
        let body = r#"
- id: same
  prompt: "a"
  expected: "a"
- id: same
  prompt: "b"
  expected: "b"
"#;
        let err = parse_yaml_prompts(body).unwrap_err();
        assert!(err.to_string().contains("duplicate row id"), "got: {}", err);
    }

    #[test]
    fn discover_finds_skill_with_markdown_prompts() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let skill_dir = tmp.path().join("skills").join("demo").join("evals");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("prompts.md"), "| Prompt | Expected |\n|---|---|\n| \"hi\" | ok |\n").unwrap();
        std::fs::write(skill_dir.join("config.toml"), "runtime_profile = \"cloud\"\n").unwrap();

        let skills = discover_skills(&[], &[tmp.path().join("skills")]).expect("discover");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].tier, SkillTier::Project);
        assert!(skills[0].prompts_path.ends_with("prompts.md"));
        assert_eq!(skills[0].prompts_format, "markdown");
    }

    #[test]
    fn discover_prefers_yaml_when_both_exist() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let skill_dir = tmp.path().join("skills").join("demo").join("evals");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("prompts.md"), "").unwrap();
        std::fs::write(skill_dir.join("prompts.yaml"), "[]").unwrap();
        std::fs::write(skill_dir.join("config.toml"), "").unwrap();

        let skills = discover_skills(&[], &[tmp.path().join("skills")]).unwrap();
        assert_eq!(skills[0].prompts_format, "yaml");
    }

    #[test]
    fn discover_filters_by_name() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        for name in ["alpha", "beta"] {
            let evals = tmp.path().join("skills").join(name).join("evals");
            std::fs::create_dir_all(&evals).unwrap();
            std::fs::write(evals.join("prompts.md"), "| Prompt | Expected |\n|---|---|\n| \"x\" | y |\n").unwrap();
            std::fs::write(evals.join("config.toml"), "").unwrap();
        }

        let skills = discover_skills(&["beta".to_string()], &[tmp.path().join("skills")]).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "beta");
    }

    #[test]
    fn discover_does_not_shadow_lower_tier_when_higher_tier_lacks_evals() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let higher = tmp.path().join("higher");
        let lower = tmp.path().join("lower");

        // Higher tier: a skill directory named "demo" but WITHOUT an evals/ folder.
        std::fs::create_dir_all(higher.join("demo")).unwrap();

        // Lower tier: "demo" with a valid evals/ folder.
        let lower_evals = lower.join("demo").join("evals");
        std::fs::create_dir_all(&lower_evals).unwrap();
        std::fs::write(lower_evals.join("prompts.md"), "| Prompt | Expected |\n|---|---|\n| \"hi\" | ok |\n").unwrap();
        std::fs::write(lower_evals.join("config.toml"), "").unwrap();

        let skills = discover_skills(&[], &[higher.clone(), lower.clone()]).unwrap();
        assert_eq!(skills.len(), 1, "expected lower tier's demo to be discovered");
        assert!(skills[0].evals_dir.starts_with(&lower), "expected lower tier, got {:?}", skills[0].evals_dir);
    }
}
