# Phase E — Self-Alignment Loop — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the skill evolution loop — fixture-driven evaluation metrics, `tengu align` CLI, `skill-improver` meta-skill, and `/feedback` capture — so skills improve over time against objective measurements.

**Architecture:** New `alignment_runner.rs` module spawns sandboxed sessions via the existing `channel_runtime::open_session` entry point, runs each skill's `evals/*.jsonl` fixtures, scores them against per-skill thresholds in frontmatter, writes dated reports + rolling JSONL metrics, and optionally invokes a `skill-improver` meta-skill that proposes `SKILL.md` patches. No new subsystems; the whole loop sits on top of Phase A's subagent spawning and Phase B's orchestration skill.

**Tech Stack:** Rust, Tokio, clap, serde, serde_yaml (new dep), serde_json, tempfile. Skills are markdown + JSONL.

**Spec:** `docs/superpowers/specs/2026-04-16-phase-e-self-alignment-design.md`

---

## Prerequisites

Phase A and Phase B must be merged before execution begins:

- **Phase A** provides `sessions_spawn`/`sessions_fan_out`, the tool plugin registry, and the post-A8 `SkillPlugin` + `SkillCatalog`. The `eval:` parsing in Task 2 extends whatever the post-A8 skill frontmatter parser is (likely in `src/adapters/plugins/skill/` or `skill_builder.rs` if A8 hasn't fully shipped). Verify at execution time.
- **Phase B** provides the `orchestration` skill (so `skill-improver` has a reference to edit) and `telegram_builder.rs` already routing through `channel_runtime::open_session` (so `/feedback` wires into one path, not two).

If executing before Phase B ships, Task 22 (Telegram wiring) still works but tests against `TelegramTaskExecutor` instead of the unified session path — flag it and rework after B3 lands.

---

## File Structure

**New files:**
- `src/adapters/alignment_runner.rs` — runner module (fixture loading, sandbox session spawning, scoring, report writing)
- `src/adapters/feedback.rs` — `/feedback` command handler (appends to `memory/feedback.md`)
- `skills/skill-improver/SKILL.md` — new meta-skill
- `skills/skill-improver/evals/improver-basic.jsonl` — self-fixtures
- `skills/<each>/evals/*.jsonl` — one fixtures directory per existing skill

**Modified files:**
- `Cargo.toml` — add `serde_yaml` dependency
- `src/main.rs` — add `Align` + `Feedback` subcommands (the latter is internal, for tests)
- `src/adapters/mod.rs` — register `alignment_runner` and `feedback` modules
- `src/adapters/skill_builder.rs` (or post-A8 equivalent) — parse `eval:` block into `EvalConfig`
- `src/adapters/channel_runtime.rs` — add `open_sandboxed_session` helper + `/feedback` dispatch
- `src/adapters/chat_builder.rs` — `/feedback` command parsing (CLI)
- `src/adapters/telegram_builder.rs` — `/feedback` command parsing (Telegram)
- `skills/skill-creator/SKILL.md` — teach `eval:` block and `evals/*.jsonl` convention
- `skills/skill-eval/SKILL.md` — document fixture-replay mode
- `skills/<each>/SKILL.md` — add `eval:` block (7 skills)
- `docs/architecture.md` — doctrine wording tweak
- `docs/superpowers/specs/2026-04-15-phase-b-*.md` — promote `skill-cleaner` to critical path
- `docs/superpowers/specs/2026-04-15-phase-c-*.md` — drop `ChannelPlugin` from scope
- `docs/superpowers/specs/2026-04-15-phase-d-*.md` — expand orchestration-skill paragraph; unpin model dates

---

## Tasks

### Task 1: Add serde_yaml dependency

**Files:**
- Modify: `Cargo.toml:16`

- [ ] **Step 1: Edit Cargo.toml to add serde_yaml**

Add to `[dependencies]`, alphabetized next to `serde_json`:

```toml
serde_yaml = "0.9"
```

- [ ] **Step 2: Verify build**

Run: `cargo build`
Expected: compiles clean, one new dependency fetched.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add serde_yaml for structured skill frontmatter parsing"
```

---

### Task 2: Parse `eval:` block in skill frontmatter

**Files:**
- Modify: `src/adapters/skill_builder.rs` (or post-A8 plugin equivalent)

- [ ] **Step 1: Write the failing unit test**

Append to the `#[cfg(test)] mod tests` block in `skill_builder.rs`:

```rust
#[test]
fn parses_eval_block_from_frontmatter() {
    let content = r#"---
name: demo
description: demo skill
eval:
  triggers:
    - "do X"
  non_triggers:
    - "ignore Y"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.8
    outcome_contains: 0.8
---
body
"#;
    let parsed = parse_eval_from_frontmatter(content).expect("eval parses");
    assert_eq!(parsed.triggers, vec!["do X".to_string()]);
    assert_eq!(parsed.non_triggers, vec!["ignore Y".to_string()]);
    assert_eq!(parsed.fixtures.as_deref(), Some("./evals/*.jsonl"));
    assert_eq!(parsed.thresholds.trigger_precision, Some(0.9));
    assert_eq!(parsed.thresholds.outcome_contains, Some(0.8));
}

#[test]
fn eval_block_is_optional() {
    let content = "---\nname: bare\ndescription: no eval\n---\nbody\n";
    let parsed = parse_eval_from_frontmatter(content);
    assert!(parsed.is_none(), "skills without eval: return None");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tengu-cluster parses_eval_block_from_frontmatter`
Expected: FAIL with "cannot find function `parse_eval_from_frontmatter`".

- [ ] **Step 3: Add EvalConfig types**

Insert near the existing `SkillFrontmatter` struct (around line 123):

```rust
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct EvalConfig {
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub non_triggers: Vec<String>,
    #[serde(default)]
    pub fixtures: Option<String>,
    #[serde(default)]
    pub thresholds: EvalThresholds,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct EvalThresholds {
    #[serde(default)]
    pub trigger_precision: Option<f32>,
    #[serde(default)]
    pub trigger_recall: Option<f32>,
    #[serde(default)]
    pub tool_call_match: Option<f32>,
    #[serde(default)]
    pub outcome_contains: Option<f32>,
}
```

- [ ] **Step 4: Implement `parse_eval_from_frontmatter`**

The existing `try_parse_frontmatter` is hand-rolled and skips unknown keys. Add a separate small helper that re-parses the YAML block with `serde_yaml` to extract just the `eval:` subtree. Insert after `try_parse_frontmatter` (around line 400):

```rust
pub(crate) fn parse_eval_from_frontmatter(content: &str) -> Option<EvalConfig> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = trimmed[3..].trim_start_matches('-').strip_prefix('\n')?;
    let closing = after_first.find("\n---")?;
    let yaml_block = &after_first[..closing];

    #[derive(serde::Deserialize)]
    struct EvalWrapper {
        eval: Option<EvalConfig>,
    }
    let wrapper: EvalWrapper = serde_yaml::from_str(yaml_block).ok()?;
    wrapper.eval
}
```

- [ ] **Step 5: Run test to verify pass**

Run: `cargo test -p tengu-cluster parses_eval_block_from_frontmatter eval_block_is_optional`
Expected: both PASS.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/skill_builder.rs
git commit -m "skills: parse optional eval: block from frontmatter"
```

---

### Task 3: Document `eval:` convention in skill-creator

**Files:**
- Modify: `skills/skill-creator/SKILL.md`

- [ ] **Step 1: Append a new section to `skills/skill-creator/SKILL.md`**

Add after the existing "Frontmatter" section:

```markdown
## Optional: Evaluation metrics (`eval:` block)

Skills that can be evaluated against fixtures declare an `eval:` block in their frontmatter. `tengu align` uses this to run fixtures through a sandboxed subagent and score behaviour.

```yaml
---
name: my-skill
description: ...
eval:
  triggers:
    - "sample prompt that SHOULD fire this skill"
  non_triggers:
    - "sample prompt that must NOT fire this skill"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.8
    outcome_contains: 0.8
---
```

### Minimum viable fixture set

Create `evals/basic.jsonl` next to your `SKILL.md`:

```jsonl
{"id":"happy-path","prompt":"...","expected_skill":"my-skill","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*/endpoint*"}}],"expected_contains":["success"]}
{"id":"non-trigger","prompt":"unrelated question","expected_skill":null}
```

At least one positive (`expected_skill = self`) and one negative (`expected_skill = null`) are required before the skill participates in alignment.

### Calibrating thresholds

Run `tengu align --skill my-skill` once against the freshly-authored skill. Whatever numbers it produces become your baseline — set thresholds 5–10 points below observed so the first run passes and drift is detectable.
```

- [ ] **Step 2: Commit**

```bash
git add skills/skill-creator/SKILL.md
git commit -m "skill-creator: document eval: frontmatter block and evals/*.jsonl convention"
```

---

### Task 4: Fixture JSONL types and parser

**Files:**
- Create: `src/adapters/alignment_runner.rs`
- Modify: `src/adapters/mod.rs`

- [ ] **Step 1: Register the new module**

Append to `src/adapters/mod.rs`:

```rust
pub(crate) mod alignment_runner;
```

- [ ] **Step 2: Create the file with types and a failing test**

Write `src/adapters/alignment_runner.rs`:

```rust
//! Phase E — Fixture-driven skill alignment runner.
//!
//! Loads skills with `eval:` frontmatter blocks, runs each fixture against a
//! sandboxed subagent via `channel_runtime::open_sandboxed_session`, scores
//! the trace, and writes reports/metrics.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Fixture {
    pub id: String,
    pub prompt: String,
    pub expected_skill: Option<String>,
    #[serde(default)]
    pub expected_tool_calls: Vec<ExpectedToolCall>,
    #[serde(default)]
    pub expected_contains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExpectedToolCall {
    pub name: String,
    #[serde(default)]
    pub args_pattern: serde_json::Value,
}

pub(crate) fn load_fixtures_from_file(path: &Path) -> Result<Vec<Fixture>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read fixture file {}", path.display()))?;
    let mut out = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fixture: Fixture = serde_json::from_str(trimmed).with_context(|| {
            format!("{}: malformed fixture on line {}", path.display(), line_no + 1)
        })?;
        out.push(fixture);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_valid_jsonl() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(
            f,
            r#"{{"id":"a","prompt":"foo","expected_skill":"x","expected_contains":["ok"]}}"#
        )
        .unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "").unwrap();
        writeln!(
            f,
            r#"{{"id":"b","prompt":"bar","expected_skill":null}}"#
        )
        .unwrap();

        let fixtures = load_fixtures_from_file(tmp.path()).unwrap();
        assert_eq!(fixtures.len(), 2);
        assert_eq!(fixtures[0].id, "a");
        assert_eq!(fixtures[0].expected_skill.as_deref(), Some("x"));
        assert_eq!(fixtures[1].expected_skill, None);
    }

    #[test]
    fn rejects_malformed_jsonl() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(f, "{{ this is not json }}").unwrap();
        let err = load_fixtures_from_file(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("line 1"), "error names the bad line");
    }
}
```

- [ ] **Step 3: Run tests to verify pass**

Run: `cargo test -p tengu-cluster alignment_runner::tests`
Expected: both tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/alignment_runner.rs src/adapters/mod.rs
git commit -m "align: add Fixture types and JSONL loader"
```

---

### Task 5: Tool-call pattern matcher

**Files:**
- Modify: `src/adapters/alignment_runner.rs`

- [ ] **Step 1: Append failing tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn matches_ordered_sublist_with_extra_calls_between() {
    let expected = vec![
        ExpectedToolCall { name: "a".into(), args_pattern: serde_json::Value::Null },
        ExpectedToolCall { name: "c".into(), args_pattern: serde_json::Value::Null },
    ];
    let observed = vec![
        ("a".to_string(), serde_json::json!({})),
        ("b".to_string(), serde_json::json!({})),
        ("c".to_string(), serde_json::json!({})),
    ];
    assert!(match_tool_calls(&expected, &observed));
}

#[test]
fn rejects_wrong_order() {
    let expected = vec![
        ExpectedToolCall { name: "a".into(), args_pattern: serde_json::Value::Null },
        ExpectedToolCall { name: "b".into(), args_pattern: serde_json::Value::Null },
    ];
    let observed = vec![
        ("b".to_string(), serde_json::json!({})),
        ("a".to_string(), serde_json::json!({})),
    ];
    assert!(!match_tool_calls(&expected, &observed));
}

#[test]
fn matches_arg_glob_on_strings() {
    let expected = vec![ExpectedToolCall {
        name: "http_request".into(),
        args_pattern: serde_json::json!({ "url": "*/poi/register*", "method": "POST" }),
    }];
    let observed = vec![(
        "http_request".to_string(),
        serde_json::json!({ "url": "https://example.com/poi/register/foo", "method": "POST" }),
    )];
    assert!(match_tool_calls(&expected, &observed));
}

#[test]
fn rejects_arg_mismatch() {
    let expected = vec![ExpectedToolCall {
        name: "http_request".into(),
        args_pattern: serde_json::json!({ "method": "POST" }),
    }];
    let observed = vec![(
        "http_request".to_string(),
        serde_json::json!({ "method": "GET" }),
    )];
    assert!(!match_tool_calls(&expected, &observed));
}
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p tengu-cluster match_tool_calls`
Expected: FAIL — function not defined.

- [ ] **Step 3: Implement the matcher**

Add to `alignment_runner.rs` above the test module:

```rust
pub(crate) fn match_tool_calls(
    expected: &[ExpectedToolCall],
    observed: &[(String, serde_json::Value)],
) -> bool {
    let mut expected_iter = expected.iter();
    let mut pending = expected_iter.next();
    for (name, args) in observed {
        let Some(exp) = pending else {
            return true; // all expected matched, extras allowed
        };
        if &exp.name == name && args_match(&exp.args_pattern, args) {
            pending = expected_iter.next();
        }
    }
    pending.is_none()
}

fn args_match(pattern: &serde_json::Value, observed: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    match (pattern, observed) {
        (Null, _) => true,
        (String(p), String(o)) => glob_match(p, o),
        (Object(p_map), Object(o_map)) => p_map.iter().all(|(k, v)| {
            o_map.get(k).map(|ov| args_match(v, ov)).unwrap_or(false)
        }),
        (Array(p), Array(o)) => {
            p.len() == o.len() && p.iter().zip(o).all(|(a, b)| args_match(a, b))
        }
        (a, b) => a == b,
    }
}

fn glob_match(pattern: &str, input: &str) -> bool {
    // Minimal glob: `*` matches any substring. No escapes, no char classes.
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == input;
    }
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !input.starts_with(part) {
                return false;
            }
            cursor = part.len();
        } else if i == parts.len() - 1 {
            if !input[cursor..].ends_with(part) {
                return false;
            }
        } else {
            match input[cursor..].find(part) {
                Some(pos) => cursor += pos + part.len(),
                None => return false,
            }
        }
    }
    true
}
```

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p tengu-cluster alignment_runner::tests`
Expected: all tests PASS (previous 2 + new 4).

- [ ] **Step 5: Commit**

```bash
git add src/adapters/alignment_runner.rs
git commit -m "align: add ordered-sublist tool-call matcher with arg glob patterns"
```

---

### Task 6: Outcome-contains matcher

**Files:**
- Modify: `src/adapters/alignment_runner.rs`

- [ ] **Step 1: Append failing tests**

Add to the test module:

```rust
#[test]
fn outcome_contains_all_substrings_case_insensitive() {
    let output = "Transaction sent. TX hash: 0xabc. SUCCESS.";
    assert_eq!(
        outcome_contains_score(&["tx hash".into(), "success".into()], output),
        1.0
    );
}

#[test]
fn outcome_contains_partial_score() {
    let output = "transaction sent, no details";
    assert!((outcome_contains_score(&["tx hash".into(), "success".into()], output) - 0.0).abs() < 1e-6);
    assert!((outcome_contains_score(&["transaction".into(), "success".into()], output) - 0.5).abs() < 1e-6);
}

#[test]
fn empty_expected_returns_one() {
    assert_eq!(outcome_contains_score(&[], "anything"), 1.0);
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test -p tengu-cluster outcome_contains`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add to `alignment_runner.rs`:

```rust
pub(crate) fn outcome_contains_score(expected: &[String], output: &str) -> f32 {
    if expected.is_empty() {
        return 1.0;
    }
    let haystack = output.to_lowercase();
    let hits = expected
        .iter()
        .filter(|needle| haystack.contains(&needle.to_lowercase()))
        .count();
    hits as f32 / expected.len() as f32
}
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p tengu-cluster outcome_contains`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/adapters/alignment_runner.rs
git commit -m "align: add case-insensitive outcome_contains_score"
```

---

### Task 7: Per-fixture scoring and report/metrics writers

**Files:**
- Modify: `src/adapters/alignment_runner.rs`

- [ ] **Step 1: Define the FixtureResult and SkillReport types**

Add to `alignment_runner.rs`:

```rust
#[derive(Debug, Clone)]
pub(crate) struct FixtureResult {
    pub fixture_id: String,
    pub skill_fired: Option<String>,
    pub expected_skill: Option<String>,
    pub trigger_correct: bool,
    pub tool_call_match: bool,
    pub outcome_contains: f32,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SkillReport {
    pub skill_name: String,
    pub fixtures: Vec<FixtureResult>,
    pub trigger_precision: f32,
    pub trigger_recall: f32,
    pub tool_call_match: f32,
    pub outcome_contains: f32,
    pub thresholds: crate::adapters::skill_builder::EvalThresholds,
}

pub(crate) fn aggregate(
    skill_name: &str,
    results: Vec<FixtureResult>,
    thresholds: crate::adapters::skill_builder::EvalThresholds,
) -> SkillReport {
    let triggerable: Vec<_> = results.iter().filter(|r| r.expected_skill.is_some()).collect();
    let non_triggerable: Vec<_> =
        results.iter().filter(|r| r.expected_skill.is_none()).collect();

    let triggered = triggerable.iter().filter(|r| r.trigger_correct).count();
    let trigger_recall = if triggerable.is_empty() {
        1.0
    } else {
        triggered as f32 / triggerable.len() as f32
    };

    let false_positives = non_triggerable
        .iter()
        .filter(|r| r.skill_fired.as_deref() == Some(skill_name))
        .count();
    let trigger_precision = if triggered + false_positives == 0 {
        1.0
    } else {
        triggered as f32 / (triggered + false_positives) as f32
    };

    let tool_call_match = avg(&triggerable.iter().map(|r| if r.tool_call_match { 1.0 } else { 0.0 }).collect::<Vec<_>>());
    let outcome_contains = avg(&triggerable.iter().map(|r| r.outcome_contains).collect::<Vec<_>>());

    SkillReport {
        skill_name: skill_name.to_string(),
        fixtures: results,
        trigger_precision,
        trigger_recall,
        tool_call_match,
        outcome_contains,
        thresholds,
    }
}

fn avg(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        1.0
    } else {
        xs.iter().sum::<f32>() / xs.len() as f32
    }
}
```

- [ ] **Step 2: Write failing test for aggregate**

Add to the test module:

```rust
#[test]
fn aggregate_computes_precision_and_recall() {
    let results = vec![
        FixtureResult {
            fixture_id: "pos-1".into(),
            skill_fired: Some("x".into()),
            expected_skill: Some("x".into()),
            trigger_correct: true,
            tool_call_match: true,
            outcome_contains: 1.0,
            timed_out: false,
            error: None,
        },
        FixtureResult {
            fixture_id: "pos-2".into(),
            skill_fired: None,
            expected_skill: Some("x".into()),
            trigger_correct: false,
            tool_call_match: false,
            outcome_contains: 0.0,
            timed_out: false,
            error: None,
        },
        FixtureResult {
            fixture_id: "neg-1".into(),
            skill_fired: None,
            expected_skill: None,
            trigger_correct: true,
            tool_call_match: true,
            outcome_contains: 1.0,
            timed_out: false,
            error: None,
        },
    ];
    let report = aggregate("x", results, Default::default());
    assert!((report.trigger_recall - 0.5).abs() < 1e-6, "1 of 2 positives fired");
    assert!((report.trigger_precision - 1.0).abs() < 1e-6, "no false positives");
    assert!((report.tool_call_match - 0.5).abs() < 1e-6);
    assert!((report.outcome_contains - 0.5).abs() < 1e-6);
}
```

- [ ] **Step 3: Run, confirm pass**

Run: `cargo test -p tengu-cluster aggregate_computes_precision_and_recall`
Expected: PASS.

- [ ] **Step 4: Write the markdown report writer**

Add to `alignment_runner.rs`:

```rust
pub(crate) fn render_report_markdown(reports: &[SkillReport]) -> String {
    let mut out = String::from("# Align Report\n\n");
    out.push_str(&format!("Generated: {}\n\n", chrono::Utc::now().to_rfc3339()));
    for r in reports {
        out.push_str(&format!("## {}\n\n", r.skill_name));
        out.push_str("| Metric | Value | Threshold | Status |\n");
        out.push_str("|---|---|---|---|\n");
        push_metric_row(&mut out, "trigger_precision", r.trigger_precision, r.thresholds.trigger_precision);
        push_metric_row(&mut out, "trigger_recall", r.trigger_recall, r.thresholds.trigger_recall);
        push_metric_row(&mut out, "tool_call_match", r.tool_call_match, r.thresholds.tool_call_match);
        push_metric_row(&mut out, "outcome_contains", r.outcome_contains, r.thresholds.outcome_contains);
        out.push_str("\n### Fixtures\n\n");
        for f in &r.fixtures {
            let status = if f.trigger_correct && f.tool_call_match && f.outcome_contains >= 0.99 {
                "PASS"
            } else if f.timed_out {
                "TIMEOUT"
            } else {
                "FAIL"
            };
            out.push_str(&format!(
                "- `{}` — {} (trigger={}, tool_match={}, outcome={:.2}){}\n",
                f.fixture_id,
                status,
                f.trigger_correct,
                f.tool_call_match,
                f.outcome_contains,
                f.error.as_ref().map(|e| format!(" — {}", e)).unwrap_or_default(),
            ));
        }
        out.push('\n');
    }
    out
}

fn push_metric_row(out: &mut String, name: &str, value: f32, threshold: Option<f32>) {
    let status = match threshold {
        Some(t) if value >= t => "OK",
        Some(_) => "FAIL",
        None => "-",
    };
    let thr = threshold.map(|t| format!("{:.2}", t)).unwrap_or_else(|| "-".into());
    out.push_str(&format!("| {} | {:.2} | {} | {} |\n", name, value, thr, status));
}
```

- [ ] **Step 5: Write the JSONL metrics appender**

```rust
pub(crate) fn append_metrics_jsonl(
    metrics_dir: &Path,
    report: &SkillReport,
) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(metrics_dir).context("create metrics dir")?;
    let path = metrics_dir.join(format!("{}.jsonl", report.skill_name));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let row = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "skill": report.skill_name,
        "trigger_precision": report.trigger_precision,
        "trigger_recall": report.trigger_recall,
        "tool_call_match": report.tool_call_match,
        "outcome_contains": report.outcome_contains,
        "fixture_count": report.fixtures.len(),
    });
    writeln!(f, "{}", row)?;
    Ok(())
}
```

- [ ] **Step 6: Test metrics appender**

Add test:

```rust
#[test]
fn appends_metrics_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let report = SkillReport {
        skill_name: "demo".into(),
        fixtures: vec![],
        trigger_precision: 0.9,
        trigger_recall: 0.8,
        tool_call_match: 1.0,
        outcome_contains: 1.0,
        thresholds: Default::default(),
    };
    append_metrics_jsonl(tmp.path(), &report).unwrap();
    append_metrics_jsonl(tmp.path(), &report).unwrap();
    let content = std::fs::read_to_string(tmp.path().join("demo.jsonl")).unwrap();
    assert_eq!(content.lines().count(), 2, "two appended lines");
}
```

- [ ] **Step 7: Run, confirm pass**

Run: `cargo test -p tengu-cluster alignment_runner`
Expected: all tests PASS.

- [ ] **Step 8: Commit**

```bash
git add src/adapters/alignment_runner.rs
git commit -m "align: add per-skill aggregation, markdown report, and rolling metrics JSONL"
```

---

### Task 8: Sandboxed session helper in channel_runtime

**Files:**
- Modify: `src/adapters/channel_runtime.rs`

- [ ] **Step 1: Inspect the existing `open_session` signature**

Run: `grep -n "pub.*fn open_session\|pub.*async fn open_session" src/adapters/channel_runtime.rs`

Note the exact return type and parameters. The helper we add calls the same session constructor but with a tempdir workspace and a restricted env.

- [ ] **Step 2: Write the test**

Append to the `#[cfg(test)] mod tests` block (or create one):

```rust
#[tokio::test]
async fn sandboxed_session_has_isolated_workspace() {
    let ctx = test_support::mock_channel_ctx().await;
    let sandbox_env = vec!["PATH".to_string()];
    let session = open_sandboxed_session(&ctx, &sandbox_env).await.unwrap();
    let ws = session.workspace_path();
    assert!(ws.exists());
    assert!(ws.to_string_lossy().contains("tengu-align-"), "tempdir prefix");
    drop(session);
    // Tempdir is cleaned up when session drops.
}
```

(`test_support::mock_channel_ctx` may need to be added — if not already present, write the minimal one that returns a `ChannelCtx` with mocks; skip the test if the existing `ChannelCtx` is too heavy and wire the sandbox helper instead to an integration test in Task 9.)

- [ ] **Step 3: Implement `open_sandboxed_session`**

Add near the existing `open_session`:

```rust
/// Open a fresh session whose workspace is a tempdir and whose env is
/// restricted to the names in `env_allowlist`. Used by the alignment runner
/// to replay fixtures without touching the user's workspace or leaking creds.
pub async fn open_sandboxed_session(
    ctx: &ChannelCtx<'_>,
    env_allowlist: &[String],
) -> anyhow::Result<Session> {
    let tmp = tempfile::Builder::new()
        .prefix("tengu-align-")
        .tempdir()
        .context("create sandbox tempdir")?;
    let workspace = tmp.path().to_path_buf();

    // Build a restricted env map from the allowlist.
    let restricted_env: std::collections::HashMap<String, String> = env_allowlist
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        .collect();

    // Delegates to the same underlying session constructor as `open_session`,
    // passing the tempdir workspace and restricted env. Session owns the
    // TempDir so cleanup happens when Session is dropped.
    open_session_with_workspace_and_env(ctx, workspace, restricted_env, Some(tmp)).await
}
```

If `open_session_with_workspace_and_env` doesn't already exist as an internal helper, factor `open_session`'s body so both call sites share it.

- [ ] **Step 4: Run tests**

Run: `cargo test -p tengu-cluster channel_runtime`
Expected: PASS, including the new sandbox test.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/channel_runtime.rs
git commit -m "channel_runtime: add open_sandboxed_session helper for align runner"
```

---

### Task 9: AlignRunner — wire fixture replay end-to-end

**Files:**
- Modify: `src/adapters/alignment_runner.rs`

- [ ] **Step 1: Add the AlignRunner struct and top-level entry point**

Append to `alignment_runner.rs`:

```rust
use std::path::PathBuf;
use std::time::Duration;

pub(crate) struct AlignOptions {
    pub skill_filter: Option<String>,
    pub fixture_filter: Option<String>,
    pub improve: bool,
    pub fixture_timeout: Duration,
    pub sandbox_env_allowlist: Vec<String>,
    pub report_dir: PathBuf,
    pub metrics_dir: PathBuf,
}

impl Default for AlignOptions {
    fn default() -> Self {
        Self {
            skill_filter: None,
            fixture_filter: None,
            improve: false,
            fixture_timeout: Duration::from_secs(120),
            sandbox_env_allowlist: vec!["PATH".into(), "HOME".into()],
            report_dir: PathBuf::from("reports"),
            metrics_dir: PathBuf::from("metrics"),
        }
    }
}

pub(crate) struct AlignRunner<'a> {
    pub ctx: &'a crate::adapters::channel_runtime::ChannelCtx<'a>,
    pub skills_dir: PathBuf,
    pub options: AlignOptions,
}

impl<'a> AlignRunner<'a> {
    pub async fn run(&self) -> Result<Vec<SkillReport>> {
        let skills = self.enumerate_skills()?;
        let mut reports = Vec::new();
        for skill in skills {
            if let Some(ref filter) = self.options.skill_filter {
                if skill.name != *filter {
                    continue;
                }
            }
            let Some(eval_cfg) = &skill.eval else {
                tracing::info!("skipping {} — no eval: block", skill.name);
                continue;
            };
            let fixtures = self.load_skill_fixtures(&skill, eval_cfg)?;
            let mut fixture_results = Vec::new();
            for fx in fixtures {
                if let Some(ref filter) = self.options.fixture_filter {
                    if !glob_match(filter, &fx.id) {
                        continue;
                    }
                }
                let result = self.run_one_fixture(&skill, &fx).await;
                fixture_results.push(result);
            }
            let report = aggregate(&skill.name, fixture_results, eval_cfg.thresholds.clone());
            append_metrics_jsonl(&self.options.metrics_dir, &report)?;
            reports.push(report);
        }
        write_report(&self.options.report_dir, &reports)?;
        if self.options.improve {
            self.spawn_improver_for_failures(&reports).await?;
        }
        Ok(reports)
    }

    fn enumerate_skills(&self) -> Result<Vec<SkillMeta>> {
        // Walk self.skills_dir for <name>/SKILL.md, parse eval: block.
        // Returns SkillMeta { name, path, eval: Option<EvalConfig>, body_path }.
        crate::adapters::alignment_runner_support::enumerate_skills(&self.skills_dir)
    }

    fn load_skill_fixtures(
        &self,
        skill: &SkillMeta,
        eval: &crate::adapters::skill_builder::EvalConfig,
    ) -> Result<Vec<Fixture>> {
        let pattern = eval.fixtures.as_deref().unwrap_or("./evals/*.jsonl");
        let base = skill.path.parent().context("skill path has parent")?;
        let glob = base.join(pattern.trim_start_matches("./"));
        let mut out = Vec::new();
        for entry in glob_files(&glob)? {
            out.extend(load_fixtures_from_file(&entry)?);
        }
        Ok(out)
    }

    async fn run_one_fixture(&self, skill: &SkillMeta, fx: &Fixture) -> FixtureResult {
        let session = match crate::adapters::channel_runtime::open_sandboxed_session(
            self.ctx,
            &self.options.sandbox_env_allowlist,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return FixtureResult {
                    fixture_id: fx.id.clone(),
                    skill_fired: None,
                    expected_skill: fx.expected_skill.clone(),
                    trigger_correct: false,
                    tool_call_match: false,
                    outcome_contains: 0.0,
                    timed_out: false,
                    error: Some(format!("session open failed: {}", e)),
                };
            }
        };
        let turn = tokio::time::timeout(
            self.options.fixture_timeout,
            session.send_user_message(&fx.prompt),
        )
        .await;
        match turn {
            Ok(Ok(trace)) => score_trace(skill, fx, &trace),
            Ok(Err(e)) => FixtureResult {
                fixture_id: fx.id.clone(),
                skill_fired: None,
                expected_skill: fx.expected_skill.clone(),
                trigger_correct: false,
                tool_call_match: false,
                outcome_contains: 0.0,
                timed_out: false,
                error: Some(format!("turn error: {}", e)),
            },
            Err(_) => FixtureResult {
                fixture_id: fx.id.clone(),
                skill_fired: None,
                expected_skill: fx.expected_skill.clone(),
                trigger_correct: false,
                tool_call_match: false,
                outcome_contains: 0.0,
                timed_out: true,
                error: Some("timeout".into()),
            },
        }
    }

    async fn spawn_improver_for_failures(&self, reports: &[SkillReport]) -> Result<()> {
        // Collect reports that fail any declared threshold, assemble the
        // skill-improver prompt, call sessions_spawn("skill-improver", prompt),
        // and write the returned proposal body to proposals/<skill>-<date>.md.
        crate::adapters::alignment_runner_support::spawn_improver(self.ctx, reports).await
    }
}

fn score_trace(
    skill: &SkillMeta,
    fx: &Fixture,
    trace: &crate::adapters::channel_runtime::TurnTrace,
) -> FixtureResult {
    let fired = trace.skills_fired.iter().any(|n| n == &skill.name);
    let skill_fired = if fired { Some(skill.name.clone()) } else { None };
    let trigger_correct = match &fx.expected_skill {
        Some(expected) => fired && expected == &skill.name,
        None => !fired,
    };
    let tool_call_match =
        match_tool_calls(&fx.expected_tool_calls, &trace.tool_calls);
    let outcome_contains =
        outcome_contains_score(&fx.expected_contains, &trace.final_output);
    FixtureResult {
        fixture_id: fx.id.clone(),
        skill_fired,
        expected_skill: fx.expected_skill.clone(),
        trigger_correct,
        tool_call_match,
        outcome_contains,
        timed_out: false,
        error: None,
    }
}

fn write_report(dir: &Path, reports: &[SkillReport]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let ts = chrono::Utc::now().format("%Y-%m-%d-%H%M%S");
    let path = dir.join(format!("{}-align.md", ts));
    std::fs::write(&path, render_report_markdown(reports))?;
    tracing::info!("wrote {}", path.display());
    Ok(())
}

fn glob_files(pattern: &Path) -> Result<Vec<PathBuf>> {
    // Minimal expand: if pattern has a `*` in the last segment, list the
    // parent dir and filter. Otherwise return [pattern] if it exists.
    let parent = pattern.parent().context("pattern has parent")?;
    let last = pattern.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !last.contains('*') {
        return Ok(if pattern.exists() { vec![pattern.to_path_buf()] } else { vec![] });
    }
    let mut out = Vec::new();
    if !parent.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if glob_match(last, &name) {
            out.push(entry.path());
        }
    }
    Ok(out)
}

pub(crate) struct SkillMeta {
    pub name: String,
    pub path: PathBuf,
    pub eval: Option<crate::adapters::skill_builder::EvalConfig>,
}
```

- [ ] **Step 2: Create the support module**

Create `src/adapters/alignment_runner_support.rs`:

```rust
//! Out-of-band helpers for alignment_runner that touch other modules.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::alignment_runner::{SkillMeta, SkillReport};

pub(crate) fn enumerate_skills(skills_dir: &Path) -> Result<Vec<SkillMeta>> {
    let mut out = Vec::new();
    if !skills_dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(skills_dir)? {
        let entry = entry?;
        let path = entry.path().join("SKILL.md");
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let name = entry.file_name().to_string_lossy().to_string();
        let eval = crate::adapters::skill_builder::parse_eval_from_frontmatter(&content);
        out.push(SkillMeta { name, path, eval });
    }
    Ok(out)
}

pub(crate) async fn spawn_improver(
    ctx: &crate::adapters::channel_runtime::ChannelCtx<'_>,
    reports: &[SkillReport],
) -> Result<()> {
    for report in reports {
        if !below_threshold(report) {
            continue;
        }
        let prompt = assemble_improver_prompt(report);
        // Use the subagent-spawning tool (Phase A): open a session for the
        // skill-improver meta-skill and capture its final output as the
        // proposal body.
        let session = crate::adapters::channel_runtime::open_sandboxed_session(ctx, &[]).await?;
        let trace = session.send_user_message(&prompt).await?;
        let proposal_dir = PathBuf::from("proposals");
        std::fs::create_dir_all(&proposal_dir)?;
        let date = chrono::Utc::now().format("%Y-%m-%d");
        let out_path = proposal_dir.join(format!("{}-{}.md", report.skill_name, date));
        std::fs::write(&out_path, &trace.final_output)
            .with_context(|| format!("write {}", out_path.display()))?;
        tracing::info!("wrote {}", out_path.display());
    }
    Ok(())
}

fn below_threshold(r: &SkillReport) -> bool {
    let t = &r.thresholds;
    t.trigger_precision.map(|x| r.trigger_precision < x).unwrap_or(false)
        || t.trigger_recall.map(|x| r.trigger_recall < x).unwrap_or(false)
        || t.tool_call_match.map(|x| r.tool_call_match < x).unwrap_or(false)
        || t.outcome_contains.map(|x| r.outcome_contains < x).unwrap_or(false)
}

fn assemble_improver_prompt(r: &SkillReport) -> String {
    let mut s = format!("Skill `{}` failed its alignment thresholds.\n\n", r.skill_name);
    s.push_str(&format!("- trigger_precision: {:.2}\n", r.trigger_precision));
    s.push_str(&format!("- trigger_recall: {:.2}\n", r.trigger_recall));
    s.push_str(&format!("- tool_call_match: {:.2}\n", r.tool_call_match));
    s.push_str(&format!("- outcome_contains: {:.2}\n\n", r.outcome_contains));
    s.push_str("Failing fixtures:\n");
    for f in &r.fixtures {
        if !(f.trigger_correct && f.tool_call_match && f.outcome_contains >= 0.99) {
            s.push_str(&format!(
                "- {}: trigger={} tool_match={} outcome={:.2}{}\n",
                f.fixture_id,
                f.trigger_correct,
                f.tool_call_match,
                f.outcome_contains,
                f.error.as_ref().map(|e| format!(" ({})", e)).unwrap_or_default(),
            ));
        }
    }
    s.push_str(&format!("\nRead `skills/{}/SKILL.md` and propose an edit.", r.skill_name));
    s
}
```

- [ ] **Step 3: Register the support module**

Add to `src/adapters/mod.rs`:

```rust
pub(crate) mod alignment_runner_support;
```

- [ ] **Step 4: Verify build**

Run: `cargo build`
Expected: compiles. The signatures `Session::workspace_path`, `Session::send_user_message`, and `TurnTrace { skills_fired, tool_calls, final_output }` must exist on the Phase A/B runtime — if any are missing, stub them on `Session`/`TurnTrace` in `channel_runtime.rs` with thin accessors. The trace struct's three fields are the bare minimum the runner needs.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/alignment_runner.rs src/adapters/alignment_runner_support.rs src/adapters/mod.rs
git commit -m "align: wire AlignRunner end-to-end with sandboxed sessions"
```

---

### Task 10: `tengu align` CLI subcommand

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Extend the `Commands` enum**

Insert after the existing `Prune` variant (around line 60):

```rust
    /// Run fixture-driven skill alignment. Scores each skill's evals/*.jsonl
    /// and writes reports/ and metrics/.
    Align {
        /// Run only this skill
        #[arg(long)]
        skill: Option<String>,
        /// Glob filter on fixture IDs
        #[arg(long)]
        fixtures: Option<String>,
        /// Invoke skill-improver on failures (default: report-only)
        #[arg(long)]
        improve: bool,
        /// Override report output directory
        #[arg(long, default_value = "reports")]
        report_dir: PathBuf,
        /// Override metrics output directory
        #[arg(long, default_value = "metrics")]
        metrics_dir: PathBuf,
    },
```

- [ ] **Step 2: Add the dispatch branch in main()**

After the other command matches, add:

```rust
Some(Commands::Align {
    skill,
    fixtures,
    improve,
    report_dir,
    metrics_dir,
}) => {
    let cfg = Config::load(cli.config.as_deref())?;
    let ctx = build_channel_ctx(&cfg).await?;
    let runner = adapters::alignment_runner::AlignRunner {
        ctx: &ctx,
        skills_dir: PathBuf::from("skills"),
        options: adapters::alignment_runner::AlignOptions {
            skill_filter: skill,
            fixture_filter: fixtures,
            improve,
            report_dir,
            metrics_dir,
            ..Default::default()
        },
    };
    let reports = runner.run().await?;
    let any_failed = reports.iter().any(|r| {
        let t = &r.thresholds;
        t.trigger_precision.map(|x| r.trigger_precision < x).unwrap_or(false)
            || t.trigger_recall.map(|x| r.trigger_recall < x).unwrap_or(false)
            || t.tool_call_match.map(|x| r.tool_call_match < x).unwrap_or(false)
            || t.outcome_contains.map(|x| r.outcome_contains < x).unwrap_or(false)
    });
    if any_failed {
        std::process::exit(2);
    }
    Ok(())
}
```

`build_channel_ctx` is whatever function Phase A/B use to assemble a `ChannelCtx` for ad-hoc session creation (CLI chat and orchestrate use equivalent plumbing). If it is not factored yet, factor it from `Commands::Chat` in this commit.

- [ ] **Step 3: Smoke-test the subcommand**

Run: `cargo run -- align --skill nonexistent`
Expected: runs, writes an empty report (no matching skill), exits 0.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "cli: add tengu align subcommand"
```

---

### Task 11: Update skill-eval to document fixture-replay mode

**Files:**
- Modify: `skills/skill-eval/SKILL.md`

- [ ] **Step 1: Insert a new section before "Limitations (Phase 0)"**

Add:

```markdown
## Fixture-replay mode (Phase E)

After Phase E lands, `skill-eval` has a second mode: running each skill's `evals/*.jsonl` fixtures through a sandboxed subagent and scoring the trace.

### How to invoke

Fixture-replay is the same engine as `tengu align` — from the CLI:

```
$ tengu align                              # all skills
$ tengu align --skill <name>               # one skill
$ tengu align --fixtures "happy-path-*"    # fixture id glob
$ tengu align --improve                    # also invoke skill-improver on failures
```

You (the agent running skill-eval) can invoke it yourself via `run_command` when asked to evaluate a skill end-to-end, and include the produced `reports/<date>-align.md` in your report.

### Interpreting the output

Each skill produces four metrics:

| Metric | What it measures |
|---|---|
| `trigger_precision` | of N fixtures where this skill *did* fire, fraction where it *should* have fired |
| `trigger_recall` | of N fixtures where this skill *should* fire, fraction where it *did* |
| `tool_call_match` | of N expected tool calls, fraction observed in order with matching args |
| `outcome_contains` | of N expected substrings, fraction present in the final output |

Each metric is compared against its declared threshold from the skill's `eval:` frontmatter block. A metric below threshold is a failure.
```

- [ ] **Step 2: Update the "Limitations" heading**

Replace `## Limitations (Phase 0)` with:

```markdown
## Limitations

This version of skill-eval does NOT support:
- **LLM-judge mode** — scoring is deterministic (trigger + tool-call + outcome substrings), no rubric grading.
- **Scheduled runs** — eval runs only on explicit trigger (`tengu align` or manual invocation).
- **Automatic remediation** — `--improve` proposes patches to `proposals/`, never applies them.
```

- [ ] **Step 3: Commit**

```bash
git add skills/skill-eval/SKILL.md
git commit -m "skill-eval: document fixture-replay mode and tengu align CLI"
```

---

### Task 12: Author `skill-improver` meta-skill

**Files:**
- Create: `skills/skill-improver/SKILL.md`
- Create: `skills/skill-improver/evals/improver-basic.jsonl`

- [ ] **Step 1: Write the SKILL.md**

```markdown
---
name: skill-improver
description: Use when an align report shows a skill failing its thresholds. Reads the skill body, failing fixtures, and traces; writes a proposal file at proposals/<skill>-<date>.md with a diagnosis, suggested SKILL.md edit, and rationale. You do NOT edit the target skill directly — the user reviews and applies.
eval:
  triggers:
    - "propose an edit for skill X whose trigger_recall is 0.5"
    - "this align report shows orchestration failing — what should change"
  non_triggers:
    - "what does the orchestration skill do"
    - "write me a new skill for Slack"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.85
    trigger_recall: 0.8
    tool_call_match: 0.8
    outcome_contains: 0.8
---

# Skill Improver

Close the measurement → proposal loop. You receive:

- A failing align report entry — which skill, which fixtures failed, which thresholds.
- The current `SKILL.md` body for the subject skill.
- The trace JSON for each failing fixture (if available via `read_file`).
- Optional: recent entries in `memory/feedback.md` mentioning this skill.

Your job: produce a `proposals/<skill>-<date>.md` with:

1. A diagnosis (1–3 sentences).
2. A proposed unified-diff-style edit to `SKILL.md`.
3. An expected-metric-after estimate.
4. A one-sentence rationale.

You do **not** edit the target skill. The user reviews and applies.

## Diagnosing by metric

- `trigger_precision` low → description over-triggers → tighten with explicit non-examples.
- `trigger_recall` low → description under-triggers → add example phrases, lower specificity barrier.
- `tool_call_match` low → body's playbook is wrong → rewrite the relevant section citing the failing fixture.
- `outcome_contains` low → body's final-output guidance is wrong → add explicit "say X when done" instruction.

## Proposal format

```
# Proposal: <skill> — <YYYY-MM-DD>

## Diagnosis
<1–3 sentences>

## Failing fixtures
- <id>: <threshold> = <observed>, expected ≥ <target>

## Suggested edit

\`\`\`diff
--- skills/<skill>/SKILL.md
+++ skills/<skill>/SKILL.md
@@ ...
-old line
+new line
\`\`\`

## Expected after
<threshold>: ≥ <estimate>

## Rationale
<1 sentence>
```

## When to stop

- If the failing fixtures look contradictory (one says "fire on X", another says "don't fire on X"), write the proposal as "fixtures conflict — recommend user review before editing skill." Do not propose an edit that can't satisfy both.
- If the skill's body is already terse and matches the fixtures, the problem may be the description. Focus on frontmatter edits only.
- If multiple skills compete for the same trigger, mention cross-skill overlap in the diagnosis; the fix may be in a different skill.

## When to use `/feedback` as a signal

If `memory/feedback.md` has a recent entry mentioning this skill, treat it as high-weight evidence. User-stated "don't do X" trumps any fixture that asks for X; surface the conflict in your proposal.
```

- [ ] **Step 2: Write the self-fixtures**

Create `skills/skill-improver/evals/improver-basic.jsonl`:

```jsonl
{"id":"propose-after-trigger-recall-drop","prompt":"Align report: skill 'orchestration' trigger_recall=0.5, expected 0.85. Two fixtures where it should have fired did not. Propose an edit.","expected_skill":"skill-improver","expected_tool_calls":[{"name":"read_file","args_pattern":{"path":"*skills/orchestration/SKILL.md*"}},{"name":"write_file","args_pattern":{"path":"*proposals/orchestration-*"}}],"expected_contains":["Diagnosis","Suggested edit","Expected after"]}
{"id":"explain-is-not-improve","prompt":"What does the orchestration skill do?","expected_skill":null}
{"id":"new-skill-is-not-improve","prompt":"Write me a new skill for Slack integration.","expected_skill":null}
```

- [ ] **Step 3: Commit**

```bash
git add skills/skill-improver/
git commit -m "skill-improver: author meta-skill for proposing SKILL.md edits from align reports"
```

---

### Task 13: Backfill evals — aura-orchestrator

**Files:**
- Modify: `skills/aura-orchestrator/SKILL.md`
- Create: `skills/aura-orchestrator/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block to frontmatter**

Insert after the `homepage` line:

```yaml
eval:
  triggers:
    - "mint an IP-NFT for /tmp/paper.pdf"
    - "register POI and publish my molecule"
  non_triggers:
    - "what's the price of ETH"
    - "list my files"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.7
    outcome_contains: 0.7
```

- [ ] **Step 2: Write the fixtures**

Create `skills/aura-orchestrator/evals/basic.jsonl`:

```jsonl
{"id":"mint-basic","prompt":"register and mint IP-NFT for /tmp/paper.pdf","expected_skill":"aura-orchestrator","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*/poi/register*"}},{"name":"sign_and_send_transaction"}],"expected_contains":["tx hash"]}
{"id":"mint-with-announce","prompt":"mint /tmp/paper.pdf and announce on X","expected_skill":"aura-orchestrator","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*/poi/register*"}},{"name":"sign_and_send_transaction"},{"name":"http_request","args_pattern":{"url":"*/announce*"}}],"expected_contains":["posted","success"]}
{"id":"not-trigger-price","prompt":"what's the price of ETH?","expected_skill":null}
{"id":"not-trigger-list","prompt":"list files in my workspace","expected_skill":null}
```

- [ ] **Step 3: Calibrate thresholds by running align once**

Run: `cargo run -- align --skill aura-orchestrator`

If the baseline metrics are below the thresholds you wrote, lower each threshold in the frontmatter to 5 points below the observed baseline and commit. If the baseline is above, keep the thresholds.

- [ ] **Step 4: Commit**

```bash
git add skills/aura-orchestrator/SKILL.md skills/aura-orchestrator/evals/
git commit -m "aura-orchestrator: add eval: block and basic fixtures"
```

---

### Task 14: Backfill evals — molecule-x402

**Files:**
- Modify: `skills/molecule-x402/SKILL.md`
- Create: `skills/molecule-x402/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block**

Insert after the `metadata` line:

```yaml
eval:
  triggers:
    - "create a molecule project with x402 payment"
    - "upload this file and pay via x402"
  non_triggers:
    - "what is x402"
    - "list my wallets"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.7
    outcome_contains: 0.7
```

- [ ] **Step 2: Write fixtures**

Create `skills/molecule-x402/evals/basic.jsonl`:

```jsonl
{"id":"create-project","prompt":"create a molecule project titled 'My Paper' with x402","expected_skill":"molecule-x402","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*x402*"}}],"expected_contains":["project","created"]}
{"id":"upload-file","prompt":"upload /tmp/paper.pdf to molecule via x402","expected_skill":"molecule-x402","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*x402*"}}],"expected_contains":["uploaded"]}
{"id":"not-trigger-explain","prompt":"what is x402","expected_skill":null}
{"id":"not-trigger-wallets","prompt":"list my wallets","expected_skill":null}
```

- [ ] **Step 3: Calibrate and commit**

```bash
cargo run -- align --skill molecule-x402
# adjust thresholds based on observed baseline
git add skills/molecule-x402/
git commit -m "molecule-x402: add eval: block and basic fixtures"
```

---

### Task 15: Backfill evals — privy-agentic-wallets

**Files:**
- Modify: `skills/privy-agentic-wallets/SKILL.md`
- Create: `skills/privy-agentic-wallets/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block**

Insert after the `capability` line:

```yaml
eval:
  triggers:
    - "create a new agentic wallet on Ethereum"
    - "send 0.1 ETH from my wallet to 0xabc"
  non_triggers:
    - "what is Privy"
    - "explain agentic wallets"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.7
    outcome_contains: 0.7
```

- [ ] **Step 2: Write fixtures**

Create `skills/privy-agentic-wallets/evals/basic.jsonl`:

```jsonl
{"id":"create-wallet","prompt":"create a new agentic wallet on Ethereum","expected_skill":"privy-agentic-wallets","expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*api.privy.io*"}}],"expected_contains":["wallet","address"]}
{"id":"send-eth","prompt":"send 0.01 ETH from wallet abc to 0x000000","expected_skill":"privy-agentic-wallets","expected_tool_calls":[{"name":"sign_and_send_transaction"}],"expected_contains":["tx hash"]}
{"id":"not-trigger-what","prompt":"what is Privy","expected_skill":null}
{"id":"not-trigger-explain","prompt":"explain agentic wallets to me","expected_skill":null}
```

- [ ] **Step 3: Calibrate and commit**

```bash
cargo run -- align --skill privy-agentic-wallets
git add skills/privy-agentic-wallets/
git commit -m "privy-agentic-wallets: add eval: block and basic fixtures"
```

---

### Task 16: Backfill evals — telegram-rag-ingest

**Files:**
- Modify: `skills/telegram-rag-ingest/SKILL.md`
- Create: `skills/telegram-rag-ingest/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block**

Insert after the `homepage` line:

```yaml
eval:
  triggers:
    - "save this PDF to my notes"
    - "what did I save about X last week"
  non_triggers:
    - "send a message to my friend"
    - "what is Telegram"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
    tool_call_match: 0.7
    outcome_contains: 0.7
```

- [ ] **Step 2: Write fixtures**

```jsonl
{"id":"save-pdf","prompt":"save /tmp/paper.pdf to my notes for later","expected_skill":"telegram-rag-ingest","expected_tool_calls":[{"name":"remember"}],"expected_contains":["saved"]}
{"id":"recall","prompt":"what did I save about RNA synthesis last week","expected_skill":"telegram-rag-ingest","expected_tool_calls":[{"name":"memory_search","args_pattern":{}}],"expected_contains":[]}
{"id":"not-trigger-send","prompt":"send 'hi' to my friend","expected_skill":null}
{"id":"not-trigger-what-is","prompt":"what is Telegram","expected_skill":null}
```

- [ ] **Step 3: Calibrate and commit**

```bash
cargo run -- align --skill telegram-rag-ingest
git add skills/telegram-rag-ingest/
git commit -m "telegram-rag-ingest: add eval: block and basic fixtures"
```

---

### Task 17: Backfill evals (trigger-only) — beach-science

**Files:**
- Modify: `skills/beach-science/SKILL.md`
- Create: `skills/beach-science/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block (no outcome thresholds)**

Insert after `homepage`:

```yaml
eval:
  triggers:
    - "post my hypothesis to beach.science"
    - "share this research with agents on beach"
  non_triggers:
    - "what is beach"
    - "list my papers"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
```

Note: no `tool_call_match` or `outcome_contains` threshold — subjective outputs skip those metrics.

- [ ] **Step 2: Write trigger-only fixtures**

```jsonl
{"id":"post-hypothesis","prompt":"post my hypothesis about topic X to beach.science","expected_skill":"beach-science"}
{"id":"share-research","prompt":"share this draft paper with the agents on beach","expected_skill":"beach-science"}
{"id":"not-trigger-what","prompt":"what is beach","expected_skill":null}
{"id":"not-trigger-list","prompt":"list my papers","expected_skill":null}
```

- [ ] **Step 3: Commit**

```bash
git add skills/beach-science/
git commit -m "beach-science: add trigger-only eval: block and fixtures"
```

---

### Task 18: Backfill evals (trigger-only) — skill-creator

**Files:**
- Modify: `skills/skill-creator/SKILL.md`
- Create: `skills/skill-creator/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block**

Insert after the `description:` line:

```yaml
eval:
  triggers:
    - "create a new skill for X"
    - "modify the aura-orchestrator skill body to add Y"
  non_triggers:
    - "what is a skill"
    - "list installed skills"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
```

- [ ] **Step 2: Write fixtures**

```jsonl
{"id":"create-new","prompt":"create a skill for interacting with our internal Jira","expected_skill":"skill-creator"}
{"id":"modify-existing","prompt":"modify the orchestration skill to mention context_fetch","expected_skill":"skill-creator"}
{"id":"not-trigger-what","prompt":"what is a skill","expected_skill":null}
{"id":"not-trigger-list","prompt":"list installed skills","expected_skill":null}
```

- [ ] **Step 3: Commit**

```bash
git add skills/skill-creator/
git commit -m "skill-creator: add trigger-only eval: block and fixtures"
```

---

### Task 19: Backfill evals (trigger-only) — skill-eval

**Files:**
- Modify: `skills/skill-eval/SKILL.md`
- Create: `skills/skill-eval/evals/basic.jsonl`

- [ ] **Step 1: Add `eval:` block**

Insert after `description:`:

```yaml
eval:
  triggers:
    - "audit all installed skills"
    - "check which skills are drifting"
  non_triggers:
    - "what is skill-eval"
    - "create a new skill"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision: 0.9
    trigger_recall: 0.85
```

- [ ] **Step 2: Write fixtures**

```jsonl
{"id":"audit-all","prompt":"audit all installed skills for drift","expected_skill":"skill-eval"}
{"id":"drift-check","prompt":"check which skills have stale tool references","expected_skill":"skill-eval"}
{"id":"not-trigger-what","prompt":"what is skill-eval","expected_skill":null}
{"id":"not-trigger-create","prompt":"create a new skill for Slack","expected_skill":"skill-creator"}
```

Note the final fixture — it expects skill-creator to fire, not skill-eval. This is a cross-skill check.

- [ ] **Step 3: Commit**

```bash
git add skills/skill-eval/
git commit -m "skill-eval: add trigger-only eval: block and fixtures"
```

---

### Task 20: `/feedback` command — core handler

**Files:**
- Create: `src/adapters/feedback.rs`
- Modify: `src/adapters/mod.rs`

- [ ] **Step 1: Register module**

Append to `src/adapters/mod.rs`:

```rust
pub(crate) mod feedback;
```

- [ ] **Step 2: Write the file with a failing test**

Create `src/adapters/feedback.rs`:

```rust
//! `/feedback` command — appends user-side skill feedback to memory/feedback.md.

use anyhow::{Context, Result};
use std::path::Path;

pub(crate) struct FeedbackEntry {
    pub skill: Option<String>,
    pub message: String,
    pub session_id: Option<String>,
}

pub(crate) fn parse_feedback_command(raw: &str) -> Option<FeedbackEntry> {
    let rest = raw.strip_prefix("/feedback")?.trim_start();
    if rest.is_empty() {
        return None;
    }
    let (skill, message) = if let Some(stripped) = rest.strip_prefix('@') {
        match stripped.find(char::is_whitespace) {
            Some(pos) => (
                Some(stripped[..pos].to_string()),
                stripped[pos..].trim().to_string(),
            ),
            None => (Some(stripped.to_string()), String::new()),
        }
    } else {
        (None, rest.to_string())
    };
    if message.is_empty() {
        return None;
    }
    Some(FeedbackEntry {
        skill,
        message,
        session_id: None,
    })
}

pub(crate) fn append_feedback(
    workspace: &Path,
    entry: &FeedbackEntry,
) -> Result<()> {
    use std::io::Write;
    let dir = workspace.join("memory");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("feedback.md");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(
        f,
        "\n## {}\n- **Skill:** {}\n- **Message:** {}\n- **Session:** {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M"),
        entry.skill.as_deref().unwrap_or("(general)"),
        entry.message,
        entry.session_id.as_deref().unwrap_or("-"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_prefix() {
        let entry = parse_feedback_command("/feedback @orchestration over-delegates on short queries").unwrap();
        assert_eq!(entry.skill.as_deref(), Some("orchestration"));
        assert_eq!(entry.message, "over-delegates on short queries");
    }

    #[test]
    fn parses_general_feedback() {
        let entry = parse_feedback_command("/feedback the whole CLI feels slow").unwrap();
        assert!(entry.skill.is_none());
        assert_eq!(entry.message, "the whole CLI feels slow");
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_feedback_command("/feedback").is_none());
        assert!(parse_feedback_command("/feedback   ").is_none());
        assert!(parse_feedback_command("/feedback @orchestration").is_none());
    }

    #[test]
    fn rejects_non_feedback() {
        assert!(parse_feedback_command("/chat hi").is_none());
        assert!(parse_feedback_command("just a message").is_none());
    }

    #[test]
    fn appends_to_memory_feedback_md() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = FeedbackEntry {
            skill: Some("orchestration".into()),
            message: "too chatty".into(),
            session_id: Some("session:test".into()),
        };
        append_feedback(tmp.path(), &entry).unwrap();
        append_feedback(tmp.path(), &entry).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("memory/feedback.md")).unwrap();
        assert_eq!(body.matches("**Skill:** orchestration").count(), 2);
        assert!(body.contains("too chatty"));
    }
}
```

- [ ] **Step 3: Run tests, confirm pass**

Run: `cargo test -p tengu-cluster feedback`
Expected: 5 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/feedback.rs src/adapters/mod.rs
git commit -m "feedback: add /feedback parser and memory/feedback.md appender"
```

---

### Task 21: Wire `/feedback` in CLI chat (chat_builder.rs)

**Files:**
- Modify: `src/adapters/chat_builder.rs`

- [ ] **Step 1: Locate the user-input dispatch point**

Run: `grep -n "on_user_message\|dispatch_command\|/clear\|/new" src/adapters/chat_builder.rs | head -10`

The CLI chat has a command-dispatch path for `/clear`, `/new`, etc. We add `/feedback` on the same path.

- [ ] **Step 2: Add the dispatch branch**

Near the existing command match, insert:

```rust
if let Some(mut entry) = crate::adapters::feedback::parse_feedback_command(&user_input) {
    entry.session_id = Some(session.id().to_string());
    crate::adapters::feedback::append_feedback(session.workspace_path(), &entry)?;
    print_system_note("feedback recorded");
    return Ok(());
}
```

(`print_system_note` is the existing helper for system output in `chat_builder.rs`; substitute whatever the file uses.)

- [ ] **Step 3: Manual test**

Run: `cargo run -- chat`, then type `/feedback @orchestration too chatty`. Confirm `memory/feedback.md` contains an entry. Type `/feedback` alone — no entry added, clear error message.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/chat_builder.rs
git commit -m "chat: dispatch /feedback to memory/feedback.md"
```

---

### Task 22: Wire `/feedback` in Telegram (telegram_builder.rs)

**Files:**
- Modify: `src/adapters/telegram_builder.rs`

- [ ] **Step 1: Locate the message-router**

Run: `grep -n "fn on_message\|match msg\|BotCommand" src/adapters/telegram_builder.rs | head -10`

- [ ] **Step 2: Add `/feedback` dispatch before the skill/agent routing**

Near the existing command dispatch:

```rust
if let Some(text) = msg.text() {
    if let Some(mut entry) = crate::adapters::feedback::parse_feedback_command(text) {
        entry.session_id = Some(session_id_for(&msg).to_string());
        if let Err(e) = crate::adapters::feedback::append_feedback(workspace_for(&msg), &entry) {
            bot.send_message(msg.chat.id, format!("feedback failed: {}", e)).await?;
        } else {
            bot.send_message(msg.chat.id, "feedback recorded").await?;
        }
        return Ok(());
    }
}
```

(`session_id_for` and `workspace_for` are whatever helpers the adapter already has to resolve per-chat session + workspace.)

- [ ] **Step 3: Manual test**

Run: `cargo run --features telegram -- telegram`. In a Telegram thread, send `/feedback @orchestration too chatty`. Confirm the bot replies "feedback recorded" and `memory/feedback.md` under the correct workspace has the entry.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/telegram_builder.rs
git commit -m "telegram: dispatch /feedback to memory/feedback.md"
```

---

### Task 23: Doctrine + phase-deltas documentation pass

**Files:**
- Modify: `docs/architecture.md:12-39`
- Modify: `docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md`
- Modify: `docs/superpowers/specs/2026-04-15-phase-c-engine-channel-store-plugins-design.md`
- Modify: `docs/superpowers/specs/2026-04-15-phase-d-context-spill-to-rag-design.md`

- [ ] **Step 1: Doctrine rewording in `docs/architecture.md`**

Replace the existing "Context is the brain" + "Skills are the logic" section headers with a combined framing:

```markdown
### 2. Context and skills are the brain

Everything the LLM knows on a given turn lives in the context window: system prompt, bootstrap files (AGENTS.md, MEMORY.md, daily logs, identity files), tool definitions, skill catalog entries, transcript history, pending tool results.

Skills are how policy reaches the brain — any strategy, workflow, playbook, or "how the agent decides what to do" is a skill, and each skill materializes into context via frontmatter catalog entries (compact) and body text (loaded on demand). The Rust core's job is **brain assembly** — deciding which skills + bootstrap + transcript enter the context, in what order, at what compression, and what to do when the window overflows (Phase D's RAG spill). The core does not decide what the brain does with that context.

Skills evolve via `skill-creator` (create), `skill-eval` (measure), and `skill-improver` (propose edits from align reports) — the Phase E feedback loop. The harness gets closer to the user over time because the brain does, not because the core does.

### 3. Tools and MCP are the hands and senses
```

Keep sections 1 and 3 unchanged. The no-compromise corollary keeps its current wording (applies to both context policy and skills).

- [ ] **Step 2: Phase B — promote skill-cleaner to critical path**

In `docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md`, Section 5 migration table:

Replace the row:

```
| B7 (follow-up, optional) | Author `skills/skill-cleaner/` via `skill-creator`. Not on the Phase B critical path; can land after B6. | `skills/skill-cleaner/` | 0 Rust | none |
```

with:

```
| B7 | Author `skills/skill-cleaner/` via `skill-creator`. Critical path — a self-improving system must also self-prune. | `skills/skill-cleaner/` | 0 Rust | none |
```

Update the ordering rationale in §5.2 accordingly: remove "B7 optional and parallelizable" and replace with "B7 ships alongside B6; skill-cleaner runs in every release of Phase B onward."

- [ ] **Step 3: Phase C — drop ChannelPlugin from scope**

In `docs/superpowers/specs/2026-04-15-phase-c-engine-channel-store-plugins-design.md`:

- Remove §4.2 `ChannelPlugin` entirely (or mark deferred with a one-line note).
- In §1 and §2, remove "channel" from the list of extension points.
- In §5 migration table, remove the C2 row.
- In §8 guardrail, remove the reference to channel-runtime shaping.
- Add a paragraph to §3 Non-goals: "**Channel plugin trait.** Dropped from Phase C scope. CLI and Telegram are the only requested channels; a `ChannelPlugin` trait is speculative extensibility. When a third channel is actually requested, re-open this scope using post-B3 CLI + Telegram as the reference shape."

- [ ] **Step 4: Phase D — expand orchestration-skill paragraph and unpin model dates**

In `docs/superpowers/specs/2026-04-15-phase-d-context-spill-to-rag-design.md`:

Replace the `## Context management` snippet in §5.3 with an expanded version that also covers inline summarization and fan-out fetching:

```markdown
## Context management

When you see a [previously-seen ref=...] block in your transcript, treat the
summary as what you saw. Continue reasoning from it without fetching.

Only call context_fetch if you literally cannot answer without the exact bytes —
for example, if the user asks "what was the exact value of X on line 47 of that
file" and the summary doesn't say, or if a tool call is retrying and the retry
needs the original payload verbatim. The summary is deliberately authoritative.
Trust it by default. Fetching costs tokens and re-inflates the item into your
turn.

If you need pieces of several spilled items at once (e.g. "compare the totals
from five past runs"), issue multiple context_fetch calls in parallel rather
than sequentially. The fetched items are eligible to re-spill on the next turn
after their one-turn grace period — do not try to "pin" anything.

When a subagent returns a large result that you will reference later in the
turn, summarize the result inline before moving on. A three-line summary that
survives into your next turn beats a 40k-line result that will spill and need
to be re-fetched.
```

Replace `summary_model = "claude-haiku-4-5-20251001"` with `summary_model = "claude-haiku-4-5"` and add a note in §4.1: "Model IDs are parameterized to the current family; upgrade by editing the TOML, not by filing a Rust PR."

- [ ] **Step 5: Commit**

```bash
git add docs/architecture.md docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md docs/superpowers/specs/2026-04-15-phase-c-engine-channel-store-plugins-design.md docs/superpowers/specs/2026-04-15-phase-d-context-spill-to-rag-design.md
git commit -m "docs: apply Phase E doctrine + phase-delta edits (B skill-cleaner mandatory, C drop ChannelPlugin, D expand orchestration paragraph, unpin model dates)"
```

---

## Self-review checklist

After executing, confirm:

- [ ] `cargo test -p tengu-cluster` runs green, including the new `alignment_runner` and `feedback` tests.
- [ ] `cargo run -- align` produces `reports/<date>-align.md` and `metrics/<skill>.jsonl` entries.
- [ ] `cargo run -- align --skill skill-improver` runs the improver against its own fixtures.
- [ ] `/feedback @skill-name message` from CLI chat appends to `<workspace>/memory/feedback.md`.
- [ ] `/feedback @skill-name message` from Telegram appends to the chat's resolved workspace.
- [ ] Each of the 7 existing skills has a populated `evals/` directory and an `eval:` block in its frontmatter.
- [ ] Running `tengu align --improve` against a deliberately-broken skill writes a `proposals/<skill>-<date>.md` file; reverting the break restores the baseline.
- [ ] `docs/architecture.md` reflects the combined brain framing.
- [ ] The three affected phase specs (B/C/D) reflect the deltas from §12 of the Phase E spec.
