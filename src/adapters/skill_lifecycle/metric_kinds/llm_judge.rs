//! `llm_judge` — LLM-scored rubric evaluation, prefilled to force JSON.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};

pub(crate) struct LlmJudgeKind;

const JUDGE_SYSTEM: &str = "You are a strict rubric-based judge. Emit JSON only.";

#[async_trait]
impl MetricKind for LlmJudgeKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let (rubric_file, judge_model) = match spec {
            MetricSpec::LlmJudge {
                rubric_file,
                judge_model,
                ..
            } => (rubric_file.clone(), judge_model.clone()),
            _ => anyhow::bail!("LlmJudgeKind given wrong spec"),
        };

        let Some(judge) = ctx.judge.as_ref() else {
            return Ok(MetricOutcome {
                pass: false,
                score: 0.0,
                notes: Some("judge client unavailable".into()),
                raw: json!({}),
            });
        };

        let rubric = std::fs::read_to_string(ctx.skill_dir.join(&rubric_file))?;
        let user = format!(
            "Rubric:\n{rubric}\n\nFixture prompt:\n{prompt}\n\nAssistant transcript:\n{transcript}\n\nExpected outcome (may be empty):\n{expected}\n\nReturn ONLY a single JSON object matching this exact shape, with no prose, no markdown fence, no commentary:\n{{\"verdict\":\"pass\"|\"fail\",\"score\":0..1,\"notes\":\"...\"}}",
            prompt = fixture.prompt,
            transcript = fixture.transcript,
            expected = fixture.expected_outcome.unwrap_or(""),
        );

        // NOTE: we pass an empty prefill. Anthropic models via OpenRouter reject
        // assistant-message prefills (the conversation must end with a user
        // message). Accept a less-strict parser: extract the first `{...}` block
        // from the judge output.
        let raw = judge
            .judge(JUDGE_SYSTEM, &user, "", judge_model.as_deref())
            .await?;
        let json_str = extract_json_object(&raw)
            .ok_or_else(|| anyhow!("judge output contained no JSON object: {raw}"))?;
        let parsed: Value = serde_json::from_str(json_str)
            .map_err(|e| anyhow!("judge output not JSON: {e}; got {json_str}"))?;
        let verdict = parsed
            .get("verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("fail");
        let score = parsed.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let notes = parsed
            .get("notes")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(MetricOutcome {
            pass: verdict == "pass",
            score: score.clamp(0.0, 1.0),
            notes,
            raw: parsed,
        })
    }
}

/// Extract the first balanced `{...}` JSON object from a string. Returns
/// `None` if no balanced object is found. Handles nested braces and strings
/// but does not validate JSON; the caller's `serde_json::from_str` will.
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
    use std::sync::Arc;
    use tempfile::TempDir;

    struct StubJudge(&'static str);
    #[async_trait]
    impl JudgeClient for StubJudge {
        async fn judge(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    struct NoShell;
    impl crate::adapters::ports::ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
            Ok(String::new())
        }
    }

    async fn run_with(stub: StubJudge) -> MetricOutcome {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("r.md"), "criterion: returns JSON\n").unwrap();
        let ws = std::env::temp_dir();
        let fixture = FixtureContext {
            prompt: "p",
            expected_outcome: None,
            transcript: "t",
        };
        let ctx = MetricRunCtx {
            skill_dir: dir.path(),
            workspace: &ws,
            shell: &NoShell,
            tools: None,
            judge: Some(Arc::new(stub)),
            http: None,
            memory_manager: None,
            secret_registry: None,
            activity: None,
            tool_scopes: None,
        };
        let spec = MetricSpec::LlmJudge {
            name: "j".into(),
            rubric_file: "r.md".into(),
            judge_model: None,
            min_pass_rate: None,
        };
        LlmJudgeKind.run(&spec, &fixture, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn parses_pass_verdict() {
        // Judge now returns a complete JSON object (no prefill).
        let out = run_with(StubJudge(r#"{"verdict":"pass","score":0.85,"notes":"ok"}"#)).await;
        assert!(out.pass);
        assert!((out.score - 0.85).abs() < 1e-4);
        assert_eq!(out.notes.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn parses_json_wrapped_in_prose() {
        // Real judges sometimes add prose around the JSON. extract_json_object handles it.
        let out = run_with(StubJudge(
            r#"Here is my verdict: {"verdict":"pass","score":0.9} — the skill is clean."#,
        ))
        .await;
        assert!(out.pass);
        assert!((out.score - 0.9).abs() < 1e-4);
    }

    #[tokio::test]
    async fn parses_fail_verdict() {
        let out = run_with(StubJudge(r#"{"verdict":"fail","score":0.1}"#)).await;
        assert!(!out.pass);
        assert!((out.score - 0.1).abs() < 1e-4);
    }

    #[tokio::test]
    async fn no_judge_client_yields_fail_with_notes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("r.md"), "x\n").unwrap();
        let ws = std::env::temp_dir();
        let fixture = FixtureContext {
            prompt: "",
            expected_outcome: None,
            transcript: "",
        };
        let ctx = MetricRunCtx {
            skill_dir: dir.path(),
            workspace: &ws,
            shell: &NoShell,
            tools: None,
            judge: None,
            http: None,
            memory_manager: None,
            secret_registry: None,
            activity: None,
            tool_scopes: None,
        };
        let spec = MetricSpec::LlmJudge {
            name: "j".into(),
            rubric_file: "r.md".into(),
            judge_model: None,
            min_pass_rate: None,
        };
        let out = LlmJudgeKind.run(&spec, &fixture, &ctx).await.unwrap();
        assert!(!out.pass);
        assert!(out
            .notes
            .as_deref()
            .unwrap()
            .contains("judge client unavailable"));
    }
}
