# Eval Runner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `tengu eval <skill>`: a generic LLM-judge runner that replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent, scores each row pass/fail, and writes `report.json` + per-row transcripts.

**Architecture:** One new module `src/adapters/eval_builder.rs` wires the pieces. Reuses existing `collect_engine_response` (the tool loop in `engine_builder.rs`) by passing a `StubbedExecutor` wrapper and a `ToolResultObserver` closure — both hooks already exist in that function's signature. The judge is a second `Engine` instance (different model) with no tools. CLI adds a `Commands::Eval` arm in `main.rs`.

**Tech Stack:** Rust 2021, existing Tengu infrastructure (`Engine` trait, `ToolExecutor` trait, `collect_engine_response`), `serde_yaml` for YAML prompts, `pulldown-cmark` or hand parser for markdown prompts, `tempfile` for per-row workspaces. No new external services beyond what OpenRouter already gives us.

**Spec:** `docs/superpowers/specs/2026-04-19-eval-runner-design.md` — reference for any ambiguity.

---

## File Map

| File | Action | Purpose |
|---|---|---|
| `src/main.rs` | Modify | Add `Commands::Eval { … }` arm and dispatch to `eval_builder::run`. |
| `src/adapters/mod.rs` | Modify | Add `pub mod eval_builder;`. |
| `src/adapters/eval_builder.rs` | Create | Entire runner. Types, parsers, driver, judge, report, CLI entry. |
| `src/adapters/engine_builder.rs` | Modify | Promote `ToolExecutor` and `ToolResultObserver` from `pub(crate)` to `pub` — same crate but eval_builder needs them. Also expose `collect_engine_response` similarly. |
| `tests/eval_runner_test.rs` | Create | Unit tests for parsers, row id derivation, stubbed executor, judge output parser. |
| `skills/orchestration/evals/config.toml` | Create | Canonical eval config for the orchestration skill — also the first consumer of the runner. |
| `.gitignore` | Modify | Add `evals/runs/`. |
| `Cargo.toml` | Modify | Add `serde_yaml`, `tempfile` (may already be present), optional feature `eval-integration`. |

---

## Dependencies & Task Ordering

Tasks 1–3 are foundation (scaffold + types). Task 4 can run in parallel with 5. Task 6 (stubbed executor) depends on row-spec types from Task 2. Task 7 (config loader) is standalone. Tasks 8–10 (judge, driver, report) depend on everything before. Task 11 is the canonical config (can run any time once Task 7 lands). Task 12 is the end-to-end smoke check — it runs last.

---

### Task 1: Scaffold the CLI subcommand and module

**Files:**
- Create: `src/adapters/eval_builder.rs`
- Modify: `src/main.rs` — add `Commands::Eval` arm
- Modify: `src/adapters/mod.rs` — `pub mod eval_builder;`

- [ ] **Step 1: Add empty module**

Write `src/adapters/eval_builder.rs`:

```rust
//! Skill eval runner — `tengu eval <skill>`.
//!
//! Spec: `docs/superpowers/specs/2026-04-19-eval-runner-design.md`.
//! Replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent,
//! scores each row pass/fail via an LLM judge, and writes a report.

use anyhow::Result;
use std::path::PathBuf;

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
```

- [ ] **Step 2: Register module**

In `src/adapters/mod.rs`, add the line (keep alphabetical order with the other adapters):

```rust
pub mod eval_builder;
```

- [ ] **Step 3: Add the `Commands::Eval` arm**

In `src/main.rs`, add to the `Commands` enum (after `Telegram { … }`, before `Secret`):

```rust
/// Run skill evals against prompts.md/yaml and score pass/fail with an LLM judge.
Eval {
    /// One or more skill names. Empty = discover all skills with evals.
    skills: Vec<String>,
    /// Override skill-local evals/config.toml with sandboxes/<name>/config.toml.
    #[arg(long)]
    sandbox: Option<String>,
    /// Judge model override. Default: anthropic/claude-opus-4-7.
    #[arg(long)]
    judge_model: Option<String>,
    /// Max rows run in parallel within a skill. Default: 1 (sequential).
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    /// Output format. Table prints a human summary; json prints the report JSON and suppresses the table.
    #[arg(long, default_value = "table")]
    format: String,
    /// Output directory for transcripts + report.json. Default: evals/runs/<ISO8601-ts>/.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Glob filter over row ids within a skill.
    #[arg(long)]
    filter: Option<String>,
    /// Keep per-row tmp workspaces after run (for debugging).
    #[arg(long)]
    keep_workspace: bool,
},
```

- [ ] **Step 4: Dispatch to `eval_builder::run`**

In `src/main.rs`'s `match cli.command` block (wherever the other subcommands are dispatched), add:

```rust
Some(Commands::Eval {
    skills,
    sandbox,
    judge_model,
    concurrency,
    format,
    out,
    filter,
    keep_workspace,
}) => {
    let format = match format.as_str() {
        "table" => adapters::eval_builder::OutputFormat::Table,
        "json" => adapters::eval_builder::OutputFormat::Json,
        other => anyhow::bail!("invalid --format: {} (expected 'table' or 'json')", other),
    };
    let args = adapters::eval_builder::EvalArgs {
        skills,
        sandbox,
        judge_model,
        concurrency,
        format,
        out_dir: out,
        filter,
        keep_workspace,
    };
    let exit_code = adapters::eval_builder::run(args).await?;
    std::process::exit(exit_code);
}
```

- [ ] **Step 5: Compile and smoke**

Run: `cargo check --bin tengu`
Expected: clean compile.

Run: `cargo run --bin tengu -- eval orchestration`
Expected: prints `tengu eval: scaffold` and exits 0.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/adapters/mod.rs src/adapters/eval_builder.rs
git commit -m "$(cat <<'EOF'
feat(eval): scaffold tengu eval CLI subcommand

Empty module eval_builder.rs + Commands::Eval arm wired through to it.
No logic yet — subsequent tasks fill in parsers, driver, judge, report.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Row spec types + markdown prompts parser

**Files:**
- Modify: `src/adapters/eval_builder.rs` — add types and markdown parser
- Create: `tests/eval_runner_test.rs` — unit tests

- [ ] **Step 1: Write the failing tests**

Create `tests/eval_runner_test.rs`:

```rust
use tengu_cluster::adapters::eval_builder::{
    derive_row_id, parse_markdown_prompts, PromptRow,
};

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
    assert_eq!(rows[0].expected, "Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`.");
    assert_eq!(rows[0].id, "research-paper-x-then-mint-it-as-an-ip-token");
    assert_eq!(rows[1].id, "what-s-2-2");
    // Defaults
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
```

Also expose the crate as a library for tests. Add to `Cargo.toml`:

```toml
[lib]
name = "tengu_cluster"
path = "src/lib.rs"
```

Create `src/lib.rs`:

```rust
pub mod adapters;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test`
Expected: fails — `parse_markdown_prompts`, `derive_row_id`, `PromptRow` not defined.

- [ ] **Step 3: Implement types + parser**

Append to `src/adapters/eval_builder.rs`:

```rust
use anyhow::{anyhow, bail, Context};

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
    let trimmed = s.trim_matches('-');
    trimmed.chars().take(64).collect()
}

pub fn parse_markdown_prompts(body: &str) -> anyhow::Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Locate the first table header row (case-insensitive on "Prompt" / "Expected").
    let mut lines = body.lines().peekable();
    let mut in_table = false;
    while let Some(line) = lines.next() {
        if !in_table {
            let lower = line.to_ascii_lowercase();
            if lower.contains("| prompt") && lower.contains("expected") {
                // Skip the `|---|---|` separator.
                lines.next();
                in_table = true;
            }
            continue;
        }
        // Table body rows start with `|`. Blank line or non-pipe line ends the table.
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
            timeout_secs: 120,
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test eval_runner_test`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/adapters/eval_builder.rs tests/eval_runner_test.rs Cargo.toml
git commit -m "$(cat <<'EOF'
feat(eval): PromptRow + markdown prompts parser

Parses skills/<skill>/evals/prompts.md tables into PromptRow structs
with kebab-cased row ids. Rejects duplicate ids; requires the
canonical `| Prompt | Expected behaviour |` header.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: YAML prompts parser

**Files:**
- Modify: `src/adapters/eval_builder.rs`
- Modify: `tests/eval_runner_test.rs`
- Modify: `Cargo.toml` — add `serde_yaml = "0.9"`

- [ ] **Step 1: Write the failing tests**

Append to `tests/eval_runner_test.rs`:

```rust
use tengu_cluster::adapters::eval_builder::parse_yaml_prompts;

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
    assert!(err.to_string().contains("unknown field"), "got: {}", err);
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test yaml_`
Expected: compile error — `parse_yaml_prompts` undefined.

- [ ] **Step 3: Add `serde_yaml` dep**

In `Cargo.toml` `[dependencies]`:

```toml
serde_yaml = "0.9"
```

- [ ] **Step 4: Implement YAML parser**

Append to `src/adapters/eval_builder.rs`:

```rust
use serde::Deserialize;

// Intermediate serde struct — translates into PromptRow. `deny_unknown_fields`
// catches typos at the row level.
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

pub fn parse_yaml_prompts(body: &str) -> anyhow::Result<Vec<PromptRow>> {
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test eval_runner_test yaml_`
Expected: 3 YAML tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/eval_builder.rs tests/eval_runner_test.rs Cargo.toml
git commit -m "$(cat <<'EOF'
feat(eval): YAML prompts parser with stub support

evals/prompts.yaml with id/prompt/expected/timeout_secs/stubs fields.
deny_unknown_fields catches typos; duplicate ids rejected.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Skill discovery

**Files:**
- Modify: `src/adapters/eval_builder.rs`
- Modify: `tests/eval_runner_test.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/eval_runner_test.rs`:

```rust
use std::fs;
use tengu_cluster::adapters::eval_builder::{discover_skills, SkillTier, SkillUnderTest};

#[test]
fn discover_finds_skill_with_markdown_prompts() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let skill_dir = tmp.path().join("skills").join("demo").join("evals");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("prompts.md"), "| Prompt | Expected |\n|---|---|\n| \"hi\" | ok |\n").unwrap();
    fs::write(skill_dir.join("config.toml"), "runtime_profile = \"cloud\"\n").unwrap();

    let skills = discover_skills(
        &[],                       // no skill name filter
        &[tmp.path().join("skills")], // only project tier for this test
    )
    .expect("discover");
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
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("prompts.md"), "").unwrap();
    fs::write(skill_dir.join("prompts.yaml"), "[]").unwrap();
    fs::write(skill_dir.join("config.toml"), "").unwrap();

    let skills = discover_skills(&[], &[tmp.path().join("skills")]).unwrap();
    assert_eq!(skills[0].prompts_format, "yaml");
}

#[test]
fn discover_filters_by_name() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    for name in ["alpha", "beta"] {
        let evals = tmp.path().join("skills").join(name).join("evals");
        fs::create_dir_all(&evals).unwrap();
        fs::write(evals.join("prompts.md"), "| Prompt | Expected |\n|---|---|\n| \"x\" | y |\n").unwrap();
        fs::write(evals.join("config.toml"), "").unwrap();
    }

    let skills = discover_skills(&["beta".to_string()], &[tmp.path().join("skills")]).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "beta");
}
```

Add `tempfile = "3"` under `[dev-dependencies]` in `Cargo.toml` if not already present.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test discover_`
Expected: compile error — undefined symbols.

- [ ] **Step 3: Implement discovery**

Append to `src/adapters/eval_builder.rs`:

```rust
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTier {
    Managed,
    Workspace,
    Project,
}

impl SkillTier {
    fn label(self) -> &'static str {
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
    let s = root.to_string_lossy();
    if s.contains("/.tengu/skills") {
        // Could be $HOME/.tengu/skills (managed) or cwd/.tengu/skills (workspace).
        if let Some(home) = dirs_next::home_dir() {
            if root.starts_with(home.join(".tengu").join("skills")) {
                return SkillTier::Managed;
            }
        }
        SkillTier::Workspace
    } else {
        SkillTier::Project
    }
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
        for entry in std::fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !filter.is_empty() && !filter.iter().any(|s| s == &name) {
                continue;
            }
            if !seen.insert(name.clone()) {
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
                bail!("skill '{}' has no evals/prompts.{{md,yaml}}", wanted);
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test eval_runner_test discover_`
Expected: 3 discover tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/eval_builder.rs tests/eval_runner_test.rs Cargo.toml
git commit -m "$(cat <<'EOF'
feat(eval): skill discovery across three tiers

Walks ~/.tengu/skills, .tengu/skills, skills/ and finds every directory
containing evals/. YAML prompts override markdown when both exist.
Name filter honoured; missing skills error loudly.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Eval config loader with `{TMP_WORKSPACE}` expansion

**Files:**
- Modify: `src/adapters/eval_builder.rs`
- Modify: `tests/eval_runner_test.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/eval_runner_test.rs`:

```rust
use tengu_cluster::adapters::eval_builder::load_eval_config;

#[test]
fn eval_config_expands_tmp_workspace_placeholder() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
runtime_profile = "cloud"

[agents.main]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
default = true
workspace = "{TMP_WORKSPACE}"
skill_packages = ["orchestration"]
"#,
    )
    .unwrap();

    let ws = tmp.path().join("row-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = load_eval_config(&config_path, &ws).expect("load");

    let agent = cfg.agents.get("main").expect("main agent");
    assert_eq!(agent.workspace.as_deref(), Some(ws.to_string_lossy().as_ref()));
}

#[test]
fn eval_config_rejects_claude_code_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[agents.main]
engine = "claude_code"
model = "sonnet"
default = true
workspace = "{TMP_WORKSPACE}"
"#,
    )
    .unwrap();

    let err = load_eval_config(&config_path, tmp.path()).unwrap_err();
    assert!(
        err.to_string().contains("claude_code engine not supported"),
        "got: {}",
        err
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test eval_config_`
Expected: compile error.

- [ ] **Step 3: Implement config loader**

Append to `src/adapters/eval_builder.rs`:

```rust
use crate::adapters::config::Config;

pub fn load_eval_config(path: &Path, tmp_workspace: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let expanded = raw.replace("{TMP_WORKSPACE}", &tmp_workspace.to_string_lossy());
    let cfg: Config = toml::from_str(&expanded)
        .with_context(|| format!("parse {}", path.display()))?;

    for (name, agent) in &cfg.agents {
        if agent.engine == "claude_code" {
            bail!(
                "agent '{}': claude_code engine not supported by eval runner in v1 \
                 (spec §3 non-goal). Switch to engine = \"openrouter\" or pass --sandbox \
                 with an openrouter config.",
                name
            );
        }
    }
    Ok(cfg)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test eval_runner_test eval_config_`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/eval_builder.rs tests/eval_runner_test.rs
git commit -m "$(cat <<'EOF'
feat(eval): config loader with {TMP_WORKSPACE} expansion

Reads skill-local evals/config.toml, substitutes the only templated
field, and errors out on engine = "claude_code" (v1 non-goal).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Stubbed tool executor

**Files:**
- Modify: `src/adapters/engine_builder.rs` — promote `ToolExecutor` + `ToolResultObserver` + `collect_engine_response` to `pub`
- Modify: `src/adapters/eval_builder.rs`
- Modify: `tests/eval_runner_test.rs`

- [ ] **Step 1: Promote visibility in engine_builder.rs**

In `src/adapters/engine_builder.rs`, change:

```rust
pub(crate) trait ToolExecutor: Send + Sync {
```
to:
```rust
pub trait ToolExecutor: Send + Sync {
```

Same for `ToolResultObserver` (line 524) and `collect_engine_response` (line 527). If the compiler flags `EngineResponse` as crate-private in the signature, also make it `pub` (line ~483).

Run: `cargo check --bin tengu`
Expected: clean compile.

- [ ] **Step 2: Write the failing test**

Append to `tests/eval_runner_test.rs`:

```rust
use async_trait::async_trait;
use tengu_cluster::adapters::engine_builder::ToolExecutor;
use tengu_cluster::adapters::eval_builder::{StubbedExecutor, StubSpec};
use tengu_cluster::adapters::types::ToolCall;

struct CountingExecutor {
    counter: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl ToolExecutor for CountingExecutor {
    async fn execute(&self, call: &ToolCall) -> anyhow::Result<String> {
        self.counter.lock().unwrap().push(call.name.clone());
        Ok(format!("live-result-for-{}", call.name))
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("id-{}", name),
        name: name.to_string(),
        arguments: serde_json::json!({}),
    }
}

#[tokio::test]
async fn stubbed_executor_consumes_queue_then_repeats_last() {
    let inner = CountingExecutor {
        counter: std::sync::Mutex::new(Vec::new()),
    };
    let stubs = vec![StubSpec {
        tool: "http_request".into(),
        responses: vec![
            serde_json::json!({"status": 503}),
            serde_json::json!({"status": 200}),
        ],
    }];
    let stubbed = StubbedExecutor::new(&inner, &stubs);

    let r1 = stubbed.execute(&call("http_request")).await.unwrap();
    let r2 = stubbed.execute(&call("http_request")).await.unwrap();
    let r3 = stubbed.execute(&call("http_request")).await.unwrap();

    assert!(r1.contains("503"));
    assert!(r2.contains("200"));
    assert!(r3.contains("200")); // last entry repeats
    assert!(
        inner.counter.lock().unwrap().is_empty(),
        "stubbed calls should not reach inner executor"
    );
}

#[tokio::test]
async fn stubbed_executor_delegates_unstubbed_tools() {
    let inner = CountingExecutor {
        counter: std::sync::Mutex::new(Vec::new()),
    };
    let stubs: Vec<StubSpec> = vec![];
    let stubbed = StubbedExecutor::new(&inner, &stubs);

    let r = stubbed.execute(&call("sessions_spawn")).await.unwrap();
    assert_eq!(r, "live-result-for-sessions_spawn");
    assert_eq!(inner.counter.lock().unwrap().as_slice(), &["sessions_spawn"]);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test stubbed_`
Expected: compile error — `StubbedExecutor` not defined.

- [ ] **Step 4: Implement StubbedExecutor**

Append to `src/adapters/eval_builder.rs`:

```rust
use async_trait::async_trait;
use crate::adapters::engine_builder::ToolExecutor;
use crate::adapters::types::ToolCall;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct StubbedExecutor<'a> {
    inner: &'a dyn ToolExecutor,
    queues: Mutex<HashMap<String, VecDeque<serde_json::Value>>>,
}

impl<'a> StubbedExecutor<'a> {
    pub fn new(inner: &'a dyn ToolExecutor, stubs: &[StubSpec]) -> Self {
        let mut queues: HashMap<String, VecDeque<serde_json::Value>> = HashMap::new();
        for spec in stubs {
            queues
                .entry(spec.tool.clone())
                .or_default()
                .extend(spec.responses.iter().cloned());
        }
        Self {
            inner,
            queues: Mutex::new(queues),
        }
    }
}

#[async_trait]
impl<'a> ToolExecutor for StubbedExecutor<'a> {
    async fn execute(&self, call: &ToolCall) -> anyhow::Result<String> {
        {
            let mut guard = self.queues.lock().unwrap();
            if let Some(q) = guard.get_mut(&call.name) {
                let response = if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    // Last entry repeats forever once we stop popping.
                    q.front().cloned().unwrap_or_else(|| serde_json::json!(null))
                };
                return Ok(serde_json::to_string(&response)?);
            }
        }
        self.inner.execute(call).await
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --test eval_runner_test stubbed_`
Expected: 2 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/engine_builder.rs src/adapters/eval_builder.rs tests/eval_runner_test.rs
git commit -m "$(cat <<'EOF'
feat(eval): StubbedExecutor + pub visibility for engine hooks

Wraps any ToolExecutor. For tools with stubs, pops from the queue
(last entry repeats). Unstubbed tools delegate to the inner executor.
Engine hooks (ToolExecutor trait, ToolResultObserver, collect_engine_response)
promoted to pub so the eval runner can reuse them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Judge client + verdict parser

**Files:**
- Modify: `src/adapters/eval_builder.rs`
- Modify: `tests/eval_runner_test.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/eval_runner_test.rs`:

```rust
use tengu_cluster::adapters::eval_builder::{parse_verdict, Verdict};

#[test]
fn verdict_parses_pass() {
    let v = parse_verdict(r#"{"verdict": "pass", "rationale": "all good"}"#).unwrap();
    assert_eq!(v.verdict, "pass");
    assert_eq!(v.rationale, "all good");
}

#[test]
fn verdict_parses_fail() {
    let v = parse_verdict(r#"{"verdict":"fail","rationale":"missed sessions_spawn"}"#).unwrap();
    assert_eq!(v.verdict, "fail");
}

#[test]
fn verdict_malformed_produces_fail_with_diagnostic() {
    let v = parse_verdict("not json at all").unwrap();
    assert_eq!(v.verdict, "fail");
    assert!(v.rationale.contains("judge emitted malformed output"));
}

#[test]
fn verdict_rejects_unknown_verdict_value() {
    let v = parse_verdict(r#"{"verdict":"maybe","rationale":"unsure"}"#).unwrap();
    assert_eq!(v.verdict, "fail");
    assert!(v.rationale.contains("judge emitted malformed output"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test eval_runner_test verdict_`
Expected: compile error.

- [ ] **Step 3: Implement verdict parser (sync, no network yet)**

Append to `src/adapters/eval_builder.rs`:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct Verdict {
    pub verdict: String, // "pass" or "fail"
    pub rationale: String,
}

pub fn parse_verdict(raw: &str) -> anyhow::Result<Verdict> {
    #[derive(Deserialize)]
    struct Raw {
        verdict: String,
        rationale: String,
    }
    match serde_json::from_str::<Raw>(raw) {
        Ok(r) if r.verdict == "pass" || r.verdict == "fail" => Ok(Verdict {
            verdict: r.verdict,
            rationale: r.rationale,
        }),
        _ => {
            let preview: String = raw.chars().take(200).collect();
            Ok(Verdict {
                verdict: "fail".into(),
                rationale: format!("judge emitted malformed output: {}", preview),
            })
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test eval_runner_test verdict_`
Expected: 4 tests pass.

- [ ] **Step 5: Implement judge call (async, uses OpenRouter engine)**

Append to `src/adapters/eval_builder.rs`:

```rust
use crate::adapters::types::{Engine, EngineContext, Message, Role, StreamEvent};
use futures::StreamExt;

const JUDGE_SYSTEM_PROMPT: &str = r#"You are evaluating whether an AI agent's tool-call sequence matches an expected behaviour.

Input shape:
- An "Expected behaviour" description in natural language.
- An ordered list of the agent's observed tool calls (name + truncated args).
- The agent's final assistant text.

Decide: did the agent's behaviour match the expected behaviour?

Reply with strict JSON only, on a single line:
{"verdict": "pass" | "fail", "rationale": "<one sentence>"}

No prose, no markdown, no code fences. Just the JSON object."#;

#[derive(Debug, Clone)]
pub struct Observation {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}

pub fn format_judge_user_turn(
    expected: &str,
    observations: &[Observation],
    final_text: &str,
) -> String {
    let mut s = String::new();
    s.push_str("Expected behaviour: ");
    s.push_str(expected);
    s.push_str("\n\nObserved tool calls (in order):\n");
    if observations.is_empty() {
        s.push_str("(none)\n");
    } else {
        for obs in observations {
            s.push_str(&format!("{}. {}({})\n", obs.seq, obs.name, obs.args_preview));
        }
    }
    s.push_str("\nFinal assistant text:\n");
    let text_preview: String = final_text.chars().take(1024).collect();
    s.push_str(&text_preview);
    s.push_str("\n\nDid the agent's behaviour match the expected? Reply with JSON only.");
    s
}

pub async fn judge_row(
    judge: &dyn Engine,
    expected: &str,
    observations: &[Observation],
    final_text: &str,
) -> anyhow::Result<Verdict> {
    let messages = vec![
        Message {
            role: Role::System,
            content: JUDGE_SYSTEM_PROMPT.to_string(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: format_judge_user_turn(expected, observations, final_text),
            tool_call_id: None,
            tool_calls: None,
        },
        // Prefill the assistant turn with an opening brace to force JSON start.
        Message {
            role: Role::Assistant,
            content: r#"{"verdict":"#.to_string(),
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    let ctx = EngineContext {
        workspace: None,
        system_prompt: None,
        bridge_tools: None,
        max_tool_rounds: Some(1),
        max_mcp_result_chars: None,
    };
    let mut stream = judge.run(&messages, &[], &ctx).await?;
    let mut output = String::from(r#"{"verdict":"#);
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta { text } => output.push_str(&text),
            StreamEvent::Done => break,
            StreamEvent::Error { message } => {
                bail!("judge engine error: {}", message);
            }
            _ => {}
        }
    }
    parse_verdict(&output)
}
```

- [ ] **Step 6: Compile check**

Run: `cargo check --bin tengu`
Expected: clean compile.

- [ ] **Step 7: Commit**

```bash
git add src/adapters/eval_builder.rs tests/eval_runner_test.rs
git commit -m "$(cat <<'EOF'
feat(eval): judge client + verdict parser

Sends expected-behaviour + observed tool calls + final text to a second
Engine instance (defaults to opus-4-7 via OpenRouter), prefills
`{"verdict":` to force JSON start, and parses strictly. Malformed
output becomes a diagnostic fail rather than a runner error.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Per-row driver — the heart of the runner

**Files:**
- Modify: `src/adapters/eval_builder.rs`

No new test in this task — the end-to-end smoke test in Task 12 exercises it. Unit-testing a driver that wires together engine, executor, and FS is mostly testing mocks of mocks.

- [ ] **Step 1: Add row-result types**

Append to `src/adapters/eval_builder.rs`:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenCount {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RowResult {
    pub id: String,
    pub prompt: String,
    pub expected: String,
    pub verdict: String,
    pub rationale: String,
    pub observed_tools: Vec<ObservationJson>,
    pub wall_ms: u64,
    pub agent_tokens: TokenCount,
    pub judge_tokens: TokenCount,
    pub transcript_path: PathBuf,
    pub timed_out: bool,
    pub stubs_used: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservationJson {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}
```

- [ ] **Step 2: Add helpers for tool-args truncation and transcript writing**

Append:

```rust
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("…");
        out
    }
}

fn write_transcript(
    out_path: &Path,
    skill: &str,
    row: &PromptRow,
    agent_model: &str,
    engine_id: &str,
    workspace: &Path,
    messages: &[Message],
    tool_outcomes: &[(String, String)],
    observations: &[Observation],
    final_text: &str,
    verdict: &Verdict,
    judge_user_turn: &str,
) -> anyhow::Result<()> {
    let mut body = String::new();
    body.push_str(&format!("# {} / {}\n\n", skill, row.id));
    body.push_str("## Config\n");
    body.push_str(&format!(
        "engine={}  model={}  workspace={}\n\n",
        engine_id, agent_model, workspace.display()
    ));
    body.push_str("## User prompt\n");
    body.push_str(&row.prompt);
    body.push_str("\n\n");
    body.push_str("## Message log\n");
    for (i, m) in messages.iter().enumerate() {
        body.push_str(&format!("[turn {} — {:?}]\n", i + 1, m.role));
        body.push_str(&m.content);
        body.push('\n');
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                body.push_str(&format!(
                    "→ tool call: {}({})\n",
                    c.name,
                    truncate(&c.arguments.to_string(), 2048)
                ));
            }
        }
    }
    for (name, out) in tool_outcomes {
        body.push_str(&format!("← tool result ({}): {}\n", name, truncate(out, 2048)));
    }
    body.push_str("\n## Observations (judge input)\n");
    for o in observations {
        body.push_str(&format!("{}. {}({})\n", o.seq, o.name, o.args_preview));
    }
    body.push_str("\n## Final assistant text\n");
    body.push_str(final_text);
    body.push_str("\n\n## Judge\n");
    body.push_str("### Prompt\n");
    body.push_str(judge_user_turn);
    body.push_str(&format!(
        "\n### Verdict\n{}: {}\n",
        verdict.verdict, verdict.rationale
    ));
    std::fs::write(out_path, body).with_context(|| format!("write transcript {}", out_path.display()))?;
    Ok(())
}
```

- [ ] **Step 3: Implement `run_row`**

Append:

```rust
use crate::adapters::engine_builder::{collect_engine_response, ToolResultObserver};
use std::sync::Arc;
use std::time::Instant;

pub struct RowCtx<'a> {
    pub skill: &'a SkillUnderTest,
    pub row: &'a PromptRow,
    pub eval_config: &'a Config,
    pub judge: &'a dyn Engine,
    pub out_dir: &'a Path,
    pub keep_workspace: bool,
}

pub async fn run_row(ctx: RowCtx<'_>) -> anyhow::Result<RowResult> {
    let started = Instant::now();

    // 1. Allocate a fresh tmp workspace.
    let ws = tempfile::tempdir_in(std::env::temp_dir())
        .context("create per-row tmp workspace")?;
    let ws_path = ws.path().to_path_buf();

    // 2. Re-load config with this workspace substituted.
    let cfg = load_eval_config(&ctx.skill.config_path, &ws_path)?;
    let agent = cfg
        .agents
        .values()
        .find(|a| a.default)
        .or_else(|| cfg.agents.values().next())
        .ok_or_else(|| anyhow!("eval config has no agent defined"))?;

    // 3. Build the agent engine and the real tool executor using existing builders.
    //    The exact builder calls mirror what chat_builder does during startup.
    let engine = crate::adapters::engine_builder::build_engine(&cfg, agent).await?;
    let tool_executor = crate::adapters::channel_runtime::build_tool_executor(
        &cfg, agent, &ws_path,
    )
    .await?;
    let stubbed = StubbedExecutor::new(&*tool_executor, &ctx.row.stubs);

    // 4. Install observation tap.
    let observations: Arc<Mutex<Vec<Observation>>> = Arc::new(Mutex::new(Vec::new()));
    let observations_cloned = observations.clone();
    let seq = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let seq_cloned = seq.clone();
    let observer_closure = move |tc: &ToolCall, _result: &str| {
        let n = seq_cloned.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        observations_cloned.lock().unwrap().push(Observation {
            seq: n,
            name: tc.name.clone(),
            args_preview: truncate(&tc.arguments.to_string(), 2048),
        });
    };
    let observer: ToolResultObserver = &observer_closure;

    // 5. Build system prompt for the skill under test.
    //    For v1, mirror what chat_builder does (skill body injection, etc.).
    let system_prompt = crate::adapters::chat_builder::build_system_prompt(&cfg, agent).await?;

    let messages = vec![Message {
        role: Role::System,
        content: system_prompt.clone(),
        tool_call_id: None,
        tool_calls: None,
    }, Message {
        role: Role::User,
        content: ctx.row.prompt.clone(),
        tool_call_id: None,
        tool_calls: None,
    }];

    let tool_defs: Vec<_> = tool_executor.tool_defs();
    let engine_context = EngineContext {
        workspace: Some(ws_path.clone()),
        system_prompt: Some(system_prompt),
        bridge_tools: None,
        max_tool_rounds: agent.limits.as_ref().and_then(|l| l.max_tool_rounds),
        max_mcp_result_chars: agent.limits.as_ref().and_then(|l| l.max_mcp_result_chars),
    };

    // 6. Drive under a timeout.
    let driver_fut = collect_engine_response(
        &*engine,
        &messages,
        &tool_defs,
        &engine_context,
        Some(&stubbed),
        Some(&observer),
        None,
        None,
        agent.limits.as_ref().and_then(|l| l.max_tool_rounds).unwrap_or(10) as u32,
        10_000,
        agent.limits.as_ref().and_then(|l| l.stream_event_timeout_secs).unwrap_or(60) as u64,
        5_000,
    );

    let timed_out;
    let engine_response;
    match tokio::time::timeout(
        std::time::Duration::from_secs(ctx.row.timeout_secs),
        driver_fut,
    )
    .await
    {
        Ok(Ok(resp)) => {
            timed_out = false;
            engine_response = resp;
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            timed_out = true;
            engine_response = crate::adapters::engine_builder::EngineResponse {
                text: String::new(),
                input_tokens_delta: 0,
                output_tokens_delta: 0,
                tool_outcomes: vec![],
            };
        }
    }

    // 7. Judge.
    let obs_snapshot = observations.lock().unwrap().clone();
    let judge_user_turn = format_judge_user_turn(
        &ctx.row.expected,
        &obs_snapshot,
        &engine_response.text,
    );
    let verdict = if timed_out {
        Verdict {
            verdict: "fail".into(),
            rationale: format!("row timed out after {}s", ctx.row.timeout_secs),
        }
    } else {
        judge_row(ctx.judge, &ctx.row.expected, &obs_snapshot, &engine_response.text).await?
    };

    // 8. Write transcript.
    let transcript_path = ctx
        .out_dir
        .join(format!("{}-{}.md", ctx.skill.name, ctx.row.id));
    write_transcript(
        &transcript_path,
        &ctx.skill.name,
        ctx.row,
        &agent.model,
        engine.id(),
        &ws_path,
        &messages,
        &engine_response.tool_outcomes,
        &obs_snapshot,
        &engine_response.text,
        &verdict,
        &judge_user_turn,
    )?;

    if ctx.keep_workspace {
        std::mem::forget(ws); // leak the TempDir guard so teardown doesn't fire
    }

    Ok(RowResult {
        id: ctx.row.id.clone(),
        prompt: ctx.row.prompt.clone(),
        expected: ctx.row.expected.clone(),
        verdict: verdict.verdict,
        rationale: verdict.rationale,
        observed_tools: obs_snapshot
            .into_iter()
            .map(|o| ObservationJson {
                seq: o.seq,
                name: o.name,
                args_preview: o.args_preview,
            })
            .collect(),
        wall_ms: started.elapsed().as_millis() as u64,
        agent_tokens: TokenCount {
            input: engine_response.input_tokens_delta,
            output: engine_response.output_tokens_delta,
        },
        judge_tokens: TokenCount { input: 0, output: 0 }, // judge token accounting added in Task 9
        transcript_path,
        timed_out,
        stubs_used: !ctx.row.stubs.is_empty(),
    })
}
```

NOTE: `build_tool_executor` and `build_system_prompt` signatures may need small tweaks to be callable from here. If they currently take channel-specific parameters, extract the skill-agnostic subset into a new `pub fn` in their respective builders. Keep changes minimal — the eval runner's needs are: `(config, agent, workspace) → (impl ToolExecutor, Vec<ToolDef>, String system_prompt)`.

- [ ] **Step 4: Compile check**

Run: `cargo check --bin tengu`
Expected: clean compile. If `build_tool_executor` or `build_system_prompt` aren't the exact current names or need a thin extraction, do the minimum rename/refactor to make this callable. Keep the refactor to < 20 LOC; it's not a subsystem change.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/eval_builder.rs src/adapters/channel_runtime.rs src/adapters/chat_builder.rs
git commit -m "$(cat <<'EOF'
feat(eval): per-row driver with observation tap

Builds a fresh workspace + engine + tool executor per row, wraps
the executor in StubbedExecutor, taps tool calls via ToolResultObserver,
drives collect_engine_response under a row-level timeout, then calls
the judge. Transcript is written regardless of verdict.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: Report builder + skill-level driver

**Files:**
- Modify: `src/adapters/eval_builder.rs`

- [ ] **Step 1: Add report types**

Append:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillReport {
    pub skill: String,
    pub tier: String,
    pub config_source: String, // "skill-local" | "sandbox" | "override"
    pub engine: String,
    pub agent_model: String,
    pub prompts_format: String,
    pub wall_ms: u64,
    pub rows: Vec<RowResult>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
    pub total_rows: u32,
    pub passed: u32,
    pub failed: u32,
    pub timed_out: u32,
    pub total_agent_tokens: u64,
    pub total_judge_tokens: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub started_at: String,
    pub finished_at: String,
    pub runner_version: String,
    pub judge_model: String,
    pub concurrency: usize,
    pub skills: Vec<SkillReport>,
    pub summary: Summary,
}
```

- [ ] **Step 2: Implement `run_skill`**

Append:

```rust
pub async fn run_skill(
    skill: &SkillUnderTest,
    judge: &dyn Engine,
    out_dir: &Path,
    filter: Option<&str>,
    concurrency: usize,
    keep_workspace: bool,
) -> anyhow::Result<SkillReport> {
    let skill_started = Instant::now();
    let prompts_body = std::fs::read_to_string(&skill.prompts_path)
        .with_context(|| format!("read {}", skill.prompts_path.display()))?;
    let mut rows = if skill.prompts_format == "yaml" {
        parse_yaml_prompts(&prompts_body)?
    } else {
        parse_markdown_prompts(&prompts_body)?
    };
    if let Some(pattern) = filter {
        let glob = glob::Pattern::new(pattern).context("invalid --filter glob")?;
        rows.retain(|r| glob.matches(&r.id));
    }

    // Load config once to grab agent metadata (engine, model) for report header.
    let dummy_ws = std::env::temp_dir();
    let cfg_probe = load_eval_config(&skill.config_path, &dummy_ws)?;
    let agent = cfg_probe
        .agents
        .values()
        .find(|a| a.default)
        .or_else(|| cfg_probe.agents.values().next())
        .ok_or_else(|| anyhow!("eval config has no agent"))?;
    let engine_id = agent.engine.clone();
    let agent_model = agent.model.clone();

    let mut row_results = Vec::new();
    if concurrency <= 1 {
        for row in &rows {
            eprintln!("[{} row {}] {}: running…", skill.name, row.id, &row.prompt);
            let rr = run_row(RowCtx {
                skill,
                row,
                eval_config: &cfg_probe,
                judge,
                out_dir,
                keep_workspace,
            })
            .await?;
            row_results.push(rr);
        }
    } else {
        // Bounded-concurrency: spawn tasks with a semaphore.
        let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let mut handles = Vec::new();
        for row in rows.iter().cloned().collect::<Vec<_>>() {
            let permit = sem.clone().acquire_owned().await?;
            let skill = skill.clone();
            let out_dir = out_dir.to_path_buf();
            let cfg = cfg_probe.clone();
            // judge can't be cloned (it's a trait object). For v1, concurrency>1 with shared judge
            // requires Arc<dyn Engine>. Implement after smoke test confirms sequential works.
            bail!("concurrency > 1 not yet implemented in v1 — use --concurrency 1");
        }
        let _ = (sem, handles);
    }

    Ok(SkillReport {
        skill: skill.name.clone(),
        tier: skill.tier.label().to_string(),
        config_source: "skill-local".into(),
        engine: engine_id,
        agent_model,
        prompts_format: skill.prompts_format.clone(),
        wall_ms: skill_started.elapsed().as_millis() as u64,
        rows: row_results,
    })
}
```

NOTE: concurrency >1 intentionally punts to a `bail!`. Sequential is the v1 contract (spec §4.2, "default: 1 (sequential)"). Wiring parallel execution through an `Arc<dyn Engine>` judge is mechanical but not blocking v1.

Add `glob = "0.3"` to `Cargo.toml` dependencies.

- [ ] **Step 3: Implement top-level `run`**

Replace the scaffold `pub async fn run` body with the real one:

```rust
pub async fn run(args: EvalArgs) -> anyhow::Result<i32> {
    let started_at = chrono::Utc::now();
    let out_dir = args.out_dir.unwrap_or_else(|| {
        PathBuf::from("evals/runs").join(started_at.format("%Y-%m-%dT%H-%M-%SZ").to_string())
    });
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create out dir {}", out_dir.display()))?;

    let roots = default_skill_roots();
    let skills = discover_skills(&args.skills, &roots)?;
    if skills.is_empty() {
        eprintln!("no skills with evals/ found in roots: {:?}", roots);
        return Ok(2);
    }

    // Build the judge engine. Use a throwaway Config purely to construct an OpenRouter engine
    // with the judge model.
    let judge_model = args
        .judge_model
        .unwrap_or_else(|| "anthropic/claude-opus-4-7".to_string());
    let judge = crate::adapters::engine_builder::build_openrouter_engine_with_model(&judge_model)
        .await
        .context("build judge engine")?;

    let mut skill_reports = Vec::new();
    let mut runner_exit = 0i32;
    for skill in &skills {
        let report = run_skill(
            skill,
            &*judge,
            &out_dir,
            args.filter.as_deref(),
            args.concurrency,
            args.keep_workspace,
        )
        .await?;
        if report.rows.iter().any(|r| r.verdict != "pass") {
            runner_exit = 1;
        }
        skill_reports.push(report);
    }

    let finished_at = chrono::Utc::now();

    let summary = Summary {
        total_rows: skill_reports.iter().map(|s| s.rows.len() as u32).sum(),
        passed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict == "pass")
            .count() as u32,
        failed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict != "pass")
            .count() as u32,
        timed_out: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.timed_out)
            .count() as u32,
        total_agent_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.agent_tokens.input + r.agent_tokens.output) as u64)
            .sum(),
        total_judge_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.judge_tokens.input + r.judge_tokens.output) as u64)
            .sum(),
        wall_ms: (finished_at - started_at).num_milliseconds() as u64,
    };

    let report = Report {
        schema_version: 1,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        runner_version: format!("tengu {}", env!("CARGO_PKG_VERSION")),
        judge_model,
        concurrency: args.concurrency,
        skills: skill_reports,
        summary,
    };

    let report_path = out_dir.join("report.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("write {}", report_path.display()))?;

    // Terminal table (Task 10 expands this; scaffold now):
    if args.format == OutputFormat::Table {
        print_table(&report);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    Ok(runner_exit)
}

fn print_table(report: &Report) {
    for skill in &report.skills {
        println!(
            "{}  ({} rows, {:.1}s)",
            skill.skill,
            skill.rows.len(),
            skill.wall_ms as f64 / 1000.0
        );
        for r in &skill.rows {
            let mark = if r.verdict == "pass" { "✓" } else { "✗" };
            println!("  {} {:32} {:4}  {}", mark, r.id, r.verdict, r.rationale);
            if r.verdict != "pass" {
                println!("      → see {}", r.transcript_path.display());
            }
        }
        println!();
    }
    println!(
        "{}/{} passed ({} failed). Total wall: {:.1}s. Total agent tokens: {}. Total judge tokens: {}.",
        report.summary.passed,
        report.summary.total_rows,
        report.summary.failed,
        report.summary.wall_ms as f64 / 1000.0,
        report.summary.total_agent_tokens,
        report.summary.total_judge_tokens,
    );
}
```

NOTE: `build_openrouter_engine_with_model` may not exist as named. If not, add a tiny helper in `engine_builder.rs` that constructs an OpenRouter engine with a fixed model and no tools — ~10 LOC. Keep it narrow (judge-only).

- [ ] **Step 4: Compile check**

Run: `cargo check --bin tengu`
Expected: clean compile. Address any missing helpers as described above.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/eval_builder.rs src/adapters/engine_builder.rs Cargo.toml
git commit -m "$(cat <<'EOF'
feat(eval): report builder + top-level run() driver

Assembles SkillReport and Report structs, writes report.json at
evals/runs/<ts>/report.json (schema_version 1), prints terminal
table, and returns exit code 0/1 based on row verdicts.
Concurrency > 1 bail!s for v1 — sequential is the contract.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Polish terminal table (ANSI colour + NO_COLOR support)

**Files:**
- Modify: `src/adapters/eval_builder.rs`

- [ ] **Step 1: Add colour helpers**

Replace `print_table` with:

```rust
fn should_colour() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn colour(s: &str, code: &str) -> String {
    if should_colour() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

fn print_table(report: &Report) {
    for skill in &report.skills {
        println!(
            "{}  ({} rows, {:.1}s)",
            colour(&skill.skill, "1"), // bold
            skill.rows.len(),
            skill.wall_ms as f64 / 1000.0
        );
        for r in &skill.rows {
            let (mark, code) = if r.verdict == "pass" {
                ("✓", "32")
            } else {
                ("✗", "31")
            };
            println!(
                "  {} {:32} {:4}  {}",
                colour(mark, code),
                r.id,
                r.verdict,
                r.rationale
            );
            if r.verdict != "pass" {
                println!("      → see {}", r.transcript_path.display());
            }
        }
        println!();
    }
    let summary_line = format!(
        "{}/{} passed ({} failed). Total wall: {:.1}s. Agent tokens: {}. Judge tokens: {}.",
        report.summary.passed,
        report.summary.total_rows,
        report.summary.failed,
        report.summary.wall_ms as f64 / 1000.0,
        report.summary.total_agent_tokens,
        report.summary.total_judge_tokens,
    );
    if report.summary.failed == 0 {
        println!("{}", colour(&summary_line, "32"));
    } else {
        println!("{}", colour(&summary_line, "31"));
    }
}
```

- [ ] **Step 2: Compile**

Run: `cargo check --bin tengu`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/eval_builder.rs
git commit -m "$(cat <<'EOF'
feat(eval): ANSI colour in terminal table with NO_COLOR support

Green checkmarks for pass, red crosses for fail, bold skill names.
Colour suppressed when NO_COLOR is set or stdout is not a TTY.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Canonical `skills/orchestration/evals/config.toml` + `.gitignore`

**Files:**
- Create: `skills/orchestration/evals/config.toml`
- Modify: `.gitignore`

No tests — this is content + repo housekeeping.

- [ ] **Step 1: Write the eval config**

Create `skills/orchestration/evals/config.toml`:

```toml
# Canonical eval config for the orchestration skill.
# Consumed by `tengu eval orchestration` (docs/superpowers/specs/2026-04-19-eval-runner-design.md).

runtime_profile = "cloud"

[memory]
enabled = true

[orchestrator]
enabled = true          # registers sessions_spawn / sessions_fan_out
max_concurrent = 3

[agents.main]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
default = true
workspace = "{TMP_WORKSPACE}"
skill_packages = ["orchestration"]

[agents.main.limits]
max_tool_rounds = 10
stream_event_timeout_secs = 60
```

- [ ] **Step 2: Add gitignore entry**

Append to `.gitignore`:

```
# Eval runner outputs — runs accumulate and are not source-controlled.
evals/runs/
```

- [ ] **Step 3: Commit**

```bash
git add skills/orchestration/evals/config.toml .gitignore
git commit -m "$(cat <<'EOF'
feat(eval): canonical orchestration eval config + gitignore evals/runs/

orchestration is the first consumer of the eval runner. The 12-line
config pins OpenRouter + sonnet-4-6, enables orchestrator so
sessions_spawn/fan_out register, and caps max_tool_rounds at 10
for subagent taming.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: End-to-end smoke test against orchestration

**Files:**
- Create: `tests/eval_integration_test.rs`
- Modify: `Cargo.toml` — add feature `eval-integration`

- [ ] **Step 1: Add the feature flag**

In `Cargo.toml`:

```toml
[features]
eval-integration = []
```

- [ ] **Step 2: Write the integration test**

Create `tests/eval_integration_test.rs`:

```rust
//! End-to-end smoke test for the eval runner.
//!
//! Requires OPENROUTER_API_KEY in env. Gated behind `eval-integration`
//! feature so CI without network access stays green.
//!
//! Run: `cargo test --features eval-integration --test eval_integration_test -- --nocapture`

#![cfg(feature = "eval-integration")]

use tengu_cluster::adapters::eval_builder::{run, EvalArgs, OutputFormat};

#[tokio::test]
async fn orchestration_evals_smoke() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("skipped: OPENROUTER_API_KEY not set");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let args = EvalArgs {
        skills: vec!["orchestration".to_string()],
        sandbox: None,
        judge_model: None,
        concurrency: 1,
        format: OutputFormat::Table,
        out_dir: Some(tmp.path().to_path_buf()),
        filter: None,
        keep_workspace: false,
    };
    let exit = run(args).await.expect("runner succeeded");
    assert!(exit == 0 || exit == 1, "exit code was {} (expected 0 or 1)", exit);

    let report_path = tmp.path().join("report.json");
    assert!(report_path.exists(), "report.json not written");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["skills"][0]["skill"], "orchestration");
    let rows = report["skills"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "orchestration has 5 prompt rows");
}
```

- [ ] **Step 3: Run the smoke test**

Run:

```bash
cargo test --features eval-integration --test eval_integration_test -- --nocapture
```

Expected: if `OPENROUTER_API_KEY` is set, all 5 orchestration rows execute, `report.json` lands in the tmp dir, the assertion on `schema_version` and row count passes. If the key is missing, the test prints "skipped" and exits 0.

- [ ] **Step 4: Manual smoke from the CLI**

```bash
cargo run --bin tengu -- eval orchestration
```

Expected: prints the per-skill table, writes `evals/runs/<ts>/report.json`, exits 0 if all 5 rows pass. If any row fails, exit code is 1 and the transcript path is printed beneath the row.

- [ ] **Step 5: Commit**

```bash
git add tests/eval_integration_test.rs Cargo.toml
git commit -m "$(cat <<'EOF'
test(eval): end-to-end smoke against orchestration skill

Gated behind eval-integration feature. Runs the full 5-prompt eval
suite and asserts the report schema + row count. Skips gracefully
when OPENROUTER_API_KEY is unset.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review Checklist (run after all 12 tasks)

- [ ] Every spec section (§4.1–§4.11) has at least one implementing task.
- [ ] Search the plan for `TODO`, `TBD`, `fill in`, `similar to` — none should appear.
- [ ] `PromptRow`, `Observation`, `RowResult`, `Report` field names are consistent across tasks (e.g. Task 2's `stubs: Vec<StubSpec>` matches Task 6's `&row.stubs` use).
- [ ] `--format` in CLI (Task 1, string) maps to `OutputFormat` enum (Task 1). Run checks this.
- [ ] `schema_version: 1` constant appears only in Task 9 (`Report` construction) — no drift.
- [ ] Exit code policy: 0 all-pass, 1 any-fail, 2 runner-error. Tasks 9 + 12 cover this.
- [ ] `evals/runs/` gitignored (Task 11).
- [ ] Judge model `anthropic/claude-opus-4-7` default appears in one place only (Task 9 `run`).
- [ ] Orchestration's canonical config has `engine = "openrouter"` (Task 11) — matches spec §3 "v1 OpenRouter-only".

## Acceptance

All of:

1. `cargo build --bin tengu` clean.
2. `cargo test` (without `eval-integration`) passes all unit tests from Tasks 2–7.
3. `tengu eval orchestration` against live OpenRouter produces a table + `evals/runs/<ts>/report.json` + 5 transcripts. Exit code reflects row pass/fail.
4. `tengu eval nonexistent-skill` exits 2 with a clear error.
5. Memory `project_eval_runner_spec.md` updated to say "implemented, not just designed" with the commit SHA range.

---

## Follow-up (not this plan)

- Concurrency > 1 (Task 9 bails today).
- Claude Code engine support (spec §3 non-goal).
- `--format gh` for GitHub Actions annotations (spec §7 future work).
- `tengu eval --prune-runs` (spec §4.11).
