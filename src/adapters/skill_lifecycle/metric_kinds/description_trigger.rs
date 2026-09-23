//! `description_trigger` — re-expressed Cowork `run_loop.py` pattern.
//!
//! Asks an LLM judge whether a skill's `description` plausibly fits a labeled
//! set of "should/shouldn't trigger" user queries, with a stratified 60/40
//! train/test split (mirrors `skills/skill-creator/scripts/run_loop.py` from
//! Cowork, but here it's pure Rust). Voting threshold is strict majority over
//! `runs_per_query` judge calls (default 3) to wash out variance.
//!
//! Doctrine notes:
//! - Fail-soft. Missing queries file, parse error, or empty queries return a
//!   `MetricOutcome { pass: false, ... }` with explanatory notes — not a
//!   `bail!`. The eval runner shouldn't crash because the queries fixture is
//!   missing.
//! - In v1 we score the *test* split only. The train split is reserved for a
//!   future `improve_description.py`-equivalent agent step.
//! - Stable shuffle (LCG seeded with `42`) so train/test partition is
//!   reproducible across runs without pulling in `rand`.
//!
//! ## Required `MetricSpec::DescriptionTrigger` variant (parent agent: paste
//! into `metrics.rs`, mirror the other variants' field set):
//!
//! ```ignore
//! DescriptionTrigger {
//!     name: String,
//!     /// Path (relative to skill_dir) to a YAML queries file.
//!     queries_file: String,
//!     #[serde(default)]
//!     judge_model: Option<String>,
//!     #[serde(default = "default_runs_per_query")]
//!     runs_per_query: u32,
//!     #[serde(default = "default_holdout")]
//!     holdout: f32,
//!     #[serde(default)]
//!     min_pass_rate: Option<f32>,
//! }
//! ```
//!
//! Add the variant to `MetricSpec::name()` and `MetricSpec::min_pass_rate()`
//! match arms, and a `validate_metrics` arm that checks `queries_file` exists
//! on disk (mirrors the `LlmJudge` `rubric_file` check).

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};

pub(crate) struct DescriptionTriggerKind;

const JUDGE_SYSTEM: &str =
    "You are evaluating whether a described skill triggers on a user query. Emit JSON only.";

/// Default judge calls per query — strict-majority voting over 3 wash out
/// most one-off model variance (mirrors Cowork's `run_loop.py`).
//
// Used via `#[serde(default = "...")]` in `metrics.rs`; rustc's dead-code
// pass doesn't follow string-named attribute references, so allow.
#[allow(dead_code)]
pub(crate) fn default_runs_per_query() -> u32 {
    3
}

/// Default 60/40 train/test split (the `holdout` is the *test* fraction).
/// 0.0 disables and uses every query as test.
#[allow(dead_code)]
pub(crate) fn default_holdout() -> f32 {
    0.4
}

#[derive(Debug, Deserialize)]
struct Queries {
    #[serde(default)]
    queries: Vec<TriggerQuery>,
}

#[derive(Debug, Clone, Deserialize)]
struct TriggerQuery {
    query: String,
    should_trigger: bool,
}

#[async_trait]
impl MetricKind for DescriptionTriggerKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        _fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let (queries_file, judge_model, runs_per_query, holdout, min_pass_rate) = match spec {
            MetricSpec::DescriptionTrigger {
                queries_file,
                judge_model,
                runs_per_query,
                holdout,
                min_pass_rate,
                ..
            } => (
                queries_file.clone(),
                judge_model.clone(),
                *runs_per_query,
                *holdout,
                *min_pass_rate,
            ),
            _ => anyhow::bail!("DescriptionTriggerKind given wrong spec"),
        };

        // 1. Load queries file. Fail-soft on read/parse errors.
        let queries_path = ctx.skill_dir.join(&queries_file);
        let queries_raw = match std::fs::read_to_string(&queries_path) {
            Ok(s) => s,
            Err(e) => {
                return Ok(fail(&format!(
                    "queries file unreadable ({}): {}",
                    queries_path.display(),
                    e
                )));
            }
        };
        let queries: Queries = match serde_yaml::from_str(&queries_raw) {
            Ok(q) => q,
            Err(e) => {
                return Ok(fail(&format!(
                    "queries file parse error ({}): {}",
                    queries_path.display(),
                    e
                )));
            }
        };
        if queries.queries.is_empty() {
            return Ok(fail("queries file is empty"));
        }

        // 2. Judge client must be present.
        let Some(judge) = ctx.judge.as_ref() else {
            return Ok(fail("judge client unavailable"));
        };

        // 3. Read this skill's SKILL.md for name + description (so the judge
        //    knows what skill to gauge against).
        let skill_md_path = ctx.skill_dir.join("SKILL.md");
        let (skill_name, skill_description) = match read_skill_frontmatter(&skill_md_path) {
            Ok(pair) => pair,
            Err(e) => {
                return Ok(fail(&format!(
                    "SKILL.md frontmatter read failed ({}): {}",
                    skill_md_path.display(),
                    e
                )));
            }
        };

        // 4. Stratified shuffle + split. Stable seed.
        let test_split = stratified_test_split(&queries.queries, holdout);
        if test_split.is_empty() {
            return Ok(fail("test split is empty after stratification"));
        }

        // 5. Per-query majority vote.
        let runs = runs_per_query.max(1);
        let mut per_query: Vec<Value> = Vec::with_capacity(test_split.len());
        let mut passes: u32 = 0;
        for q in &test_split {
            let user = build_user_prompt(&skill_name, &skill_description, &q.query);
            let mut votes_true = 0u32;
            let mut raw_runs: Vec<Value> = Vec::with_capacity(runs as usize);
            for _ in 0..runs {
                let raw = match judge
                    .judge(JUDGE_SYSTEM, &user, "", judge_model.as_deref())
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        raw_runs.push(json!({ "error": e.to_string() }));
                        continue;
                    }
                };
                let json_str = match extract_json_object(&raw) {
                    Some(s) => s,
                    None => {
                        raw_runs.push(json!({ "raw": raw, "error": "no JSON object" }));
                        continue;
                    }
                };
                let parsed: Value = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(e) => {
                        raw_runs.push(json!({ "raw": raw, "error": e.to_string() }));
                        continue;
                    }
                };
                let select = parsed
                    .get("select")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if select {
                    votes_true += 1;
                }
                raw_runs.push(parsed);
            }
            // Strict majority: votes_true * 2 > runs.
            let actual_trigger = votes_true * 2 > runs;
            let pass = actual_trigger == q.should_trigger;
            if pass {
                passes += 1;
            }
            per_query.push(json!({
                "query": q.query,
                "should_trigger": q.should_trigger,
                "actual_trigger": actual_trigger,
                "votes_true": votes_true,
                "runs": runs,
                "pass": pass,
                "judge_runs": raw_runs,
            }));
        }

        let total = test_split.len() as u32;
        let pass_rate = if total == 0 {
            0.0
        } else {
            passes as f32 / total as f32
        };
        let threshold = min_pass_rate.unwrap_or(0.85);
        Ok(MetricOutcome {
            pass: pass_rate >= threshold,
            score: pass_rate,
            notes: Some(format!(
                "{}/{} test queries triggered as expected",
                passes, total
            )),
            raw: json!({
                "kind": "description_trigger",
                "skill_name": skill_name,
                "runs_per_query": runs,
                "holdout": holdout,
                "test_size": total,
                "passes": passes,
                "pass_rate": pass_rate,
                "threshold": threshold,
                "per_query": per_query,
            }),
        })
    }
}

fn fail(note: &str) -> MetricOutcome {
    MetricOutcome {
        pass: false,
        score: 0.0,
        notes: Some(note.to_string()),
        raw: json!({ "kind": "description_trigger", "error": note }),
    }
}

fn build_user_prompt(skill_name: &str, skill_description: &str, query: &str) -> String {
    format!(
        "You are deciding whether the planner should select skill `{name}` for the user's query.\n\
         \n\
         Skill name: {name}\n\
         Skill description: {desc}\n\
         \n\
         User query: {query}\n\
         \n\
         Answer ONLY in JSON:\n\
         {{\"select\": true|false, \"rationale\": \"...\"}}\n\
         \n\
         Return `select: true` if the description plausibly fits the query (the planner would route to this skill). `false` otherwise.",
        name = skill_name,
        desc = skill_description,
        query = query,
    )
}

/// Read `SKILL.md` and return `(name, description)` parsed from its YAML
/// frontmatter. Mirrors `evolve.rs::split_frontmatter` for the split, then
/// uses `serde_yaml` to extract the two string fields.
fn read_skill_frontmatter(skill_md_path: &std::path::Path) -> Result<(String, String)> {
    let body = std::fs::read_to_string(skill_md_path)?;
    let rest = body
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow!("no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("frontmatter not closed"))?;
    let fm_block = &rest[..end + 1];
    let v: serde_yaml::Value = serde_yaml::from_str(fm_block)?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("frontmatter missing 'name'"))?
        .to_string();
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("frontmatter missing 'description'"))?
        .to_string();
    Ok((name, description))
}

/// Stratified split: shuffle `should_trigger==true` and `==false` independently
/// with a stable seed, take `holdout * len` from each as test, return the
/// concatenated test set.
///
/// `holdout == 0.0` returns ALL queries as test (train is empty).
/// `holdout == 1.0` also returns all queries (no train).
fn stratified_test_split(queries: &[TriggerQuery], holdout: f32) -> Vec<TriggerQuery> {
    let h = holdout.clamp(0.0, 1.0);
    if h <= 0.0 {
        return queries.to_vec();
    }

    let mut pos: Vec<TriggerQuery> = queries
        .iter()
        .filter(|q| q.should_trigger)
        .cloned()
        .collect();
    let mut neg: Vec<TriggerQuery> = queries
        .iter()
        .filter(|q| !q.should_trigger)
        .cloned()
        .collect();
    stable_shuffle(&mut pos, 42);
    stable_shuffle(&mut neg, 43);

    let take_pos = ((pos.len() as f32) * h).ceil() as usize;
    let take_neg = ((neg.len() as f32) * h).ceil() as usize;
    let take_pos = take_pos.min(pos.len());
    let take_neg = take_neg.min(neg.len());

    let mut test = Vec::with_capacity(take_pos + take_neg);
    test.extend(pos.into_iter().take(take_pos));
    test.extend(neg.into_iter().take(take_neg));
    test
}

/// Fisher-Yates shuffle driven by a tiny LCG seeded with `seed`. We avoid the
/// `rand` crate (not currently a dep). Sufficient for "give me a stable
/// permutation across runs" — not for crypto, not for unbiased statistics.
fn stable_shuffle<T>(slice: &mut [T], seed: u64) {
    if slice.len() < 2 {
        return;
    }
    let mut state = seed.wrapping_add(0x9E3779B97F4A7C15);
    for i in (1..slice.len()).rev() {
        // Numerical Recipes LCG constants.
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        slice.swap(i, j);
    }
}

/// Extract the first balanced `{...}` JSON object from a string. Returns
/// `None` if no balanced object is found. Copied verbatim from
/// `llm_judge.rs::extract_json_object` — that's the canonical pattern for
/// pulling a JSON object out of an Anthropic-via-OpenRouter judge response
/// that may include stray prose.
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if *b == b'\\' {
                escape = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match *b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::skill_lifecycle::metrics::JudgeClient;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // -- fixtures ---------------------------------------------------------

    /// Returns a fixed string for every judge call.
    struct StubJudge(&'static str);
    #[async_trait]
    impl JudgeClient for StubJudge {
        async fn judge(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// Returns `select: true` for queries whose text appears in `should_true`,
    /// else `select: false`. Lets us simulate a perfect judge.
    struct OracleJudge {
        should_true_substrings: Vec<&'static str>,
    }
    #[async_trait]
    impl JudgeClient for OracleJudge {
        async fn judge(&self, _: &str, user: &str, _: &str, _: Option<&str>) -> Result<String> {
            let select = self.should_true_substrings.iter().any(|s| user.contains(s));
            Ok(format!(r#"{{"select":{},"rationale":"x"}}"#, select))
        }
    }

    /// Returns pre-programmed responses in order, looping if exhausted.
    struct ScriptedJudge {
        responses: Mutex<Vec<&'static str>>,
        cursor: Mutex<usize>,
    }
    impl ScriptedJudge {
        fn new(responses: Vec<&'static str>) -> Self {
            Self {
                responses: Mutex::new(responses),
                cursor: Mutex::new(0),
            }
        }
    }
    #[async_trait]
    impl JudgeClient for ScriptedJudge {
        async fn judge(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<String> {
            let responses = self.responses.lock().unwrap();
            let mut cursor = self.cursor.lock().unwrap();
            let idx = *cursor % responses.len();
            *cursor += 1;
            Ok(responses[idx].to_string())
        }
    }

    struct NoShell;
    impl crate::ports::shell::ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
            Ok(String::new())
        }
    }

    fn write_skill_md(dir: &Path) {
        let body = "---\nname: test-skill\ndescription: Use when testing the description trigger metric.\n---\n\n# Test\n";
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn write_queries(dir: &Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn dummy_fixture() -> FixtureContext<'static> {
        FixtureContext {
            prompt: "",
            expected_outcome: None,
            transcript: "",
        }
    }

    fn ctx_with_judge<'a>(
        skill_dir: &'a Path,
        workspace: &'a Path,
        shell: &'a dyn crate::ports::shell::ShellExecutionPort,
        judge: Arc<dyn JudgeClient>,
    ) -> MetricRunCtx<'a> {
        MetricRunCtx {
            skill_dir,
            workspace,
            shell,
            tools: None,
            judge: Some(judge),
            http: None,
            memory_manager: None,
            secret_registry: None,
            activity: None,
            tool_scopes: None,
            conversation: None,
            sibling_metrics: None,
        }
    }

    fn spec(file: &str, runs_per_query: u32, holdout: f32) -> MetricSpec {
        MetricSpec::DescriptionTrigger {
            name: "triggers".into(),
            queries_file: file.into(),
            judge_model: None,
            runs_per_query,
            holdout,
            min_pass_rate: None,
        }
    }

    // -- tests ------------------------------------------------------------

    #[tokio::test]
    async fn passes_when_judge_agrees_on_all_test_queries() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        // 4 queries: 2 should-trigger, 2 should-not. Holdout 1.0 -> all are
        // test (so we're scoring deterministically against every query).
        write_queries(
            dir.path(),
            "q.yaml",
            "queries:\n\
             - query: \"yes one\"\n  should_trigger: true\n\
             - query: \"yes two\"\n  should_trigger: true\n\
             - query: \"no one\"\n  should_trigger: false\n\
             - query: \"no two\"\n  should_trigger: false\n",
        );
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> = Arc::new(OracleJudge {
            should_true_substrings: vec!["yes one", "yes two"],
        });
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("q.yaml", 1, 1.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        assert!(out.pass, "expected pass; outcome: {out:?}");
        assert!((out.score - 1.0).abs() < 1e-4);
        assert_eq!(
            out.notes.as_deref(),
            Some("4/4 test queries triggered as expected")
        );
    }

    #[tokio::test]
    async fn fails_when_judge_disagrees() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        // Stub always says select=true. The two should-not-trigger queries
        // will fail. With 4 queries total and 2 failing, pass_rate = 0.5,
        // below the 0.85 default threshold.
        write_queries(
            dir.path(),
            "q.yaml",
            "queries:\n\
             - query: \"a\"\n  should_trigger: true\n\
             - query: \"b\"\n  should_trigger: true\n\
             - query: \"c\"\n  should_trigger: false\n\
             - query: \"d\"\n  should_trigger: false\n",
        );
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> = Arc::new(StubJudge(r#"{"select":true,"rationale":"x"}"#));
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("q.yaml", 1, 1.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        assert!(!out.pass);
        assert!(out.score < 0.85);
    }

    #[tokio::test]
    async fn voting_majority_handles_one_dissent() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        // One query, should_trigger=true, runs_per_query=3.
        // Pre-programmed: true, true, false -> majority true -> pass.
        write_queries(
            dir.path(),
            "q.yaml",
            "queries:\n\
             - query: \"only one\"\n  should_trigger: true\n",
        );
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> = Arc::new(ScriptedJudge::new(vec![
            r#"{"select":true,"rationale":"a"}"#,
            r#"{"select":true,"rationale":"b"}"#,
            r#"{"select":false,"rationale":"c"}"#,
        ]));
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("q.yaml", 3, 1.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        assert!(out.pass, "outcome: {out:?}");
        assert!((out.score - 1.0).abs() < 1e-4);
    }

    #[tokio::test]
    async fn rejects_when_queries_file_missing() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> = Arc::new(StubJudge(r#"{"select":true}"#));
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("nope.yaml", 1, 1.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        assert!(!out.pass);
        assert_eq!(out.score, 0.0);
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("unreadable") || notes.contains("queries file"),
            "got: {notes}"
        );
    }

    #[tokio::test]
    async fn rejects_when_queries_empty() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        write_queries(dir.path(), "q.yaml", "queries: []\n");
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> = Arc::new(StubJudge(r#"{"select":true}"#));
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("q.yaml", 1, 1.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        assert!(!out.pass);
        assert_eq!(out.score, 0.0);
        assert!(out.notes.as_deref().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn holdout_zero_uses_all_queries_as_test() {
        let dir = TempDir::new().unwrap();
        write_skill_md(dir.path());
        write_queries(
            dir.path(),
            "q.yaml",
            "queries:\n\
             - query: \"a\"\n  should_trigger: true\n\
             - query: \"b\"\n  should_trigger: true\n\
             - query: \"c\"\n  should_trigger: false\n\
             - query: \"d\"\n  should_trigger: false\n\
             - query: \"e\"\n  should_trigger: false\n",
        );
        let ws = std::env::temp_dir();
        let shell = NoShell;
        let judge: Arc<dyn JudgeClient> =
            Arc::new(StubJudge(r#"{"select":false,"rationale":"x"}"#));
        let ctx = ctx_with_judge(dir.path(), &ws, &shell, judge);
        let out = DescriptionTriggerKind
            .run(&spec("q.yaml", 1, 0.0), &dummy_fixture(), &ctx)
            .await
            .unwrap();
        // 5 queries, judge always says false. should_trigger=true (2) fail,
        // should_trigger=false (3) pass. notes confirms total=5.
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("/5 test queries"),
            "expected total=5 in notes; got: {notes}"
        );
    }
}
