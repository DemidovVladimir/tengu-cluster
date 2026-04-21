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
            "Rubric:\n{rubric}\n\nFixture prompt:\n{prompt}\n\nAssistant transcript:\n{transcript}\n\nExpected outcome (may be empty):\n{expected}\n\nReturn JSON: {{\"verdict\":\"pass\"|\"fail\",\"score\":0..1,\"notes\":\"...\"}}",
            prompt = fixture.prompt,
            transcript = fixture.transcript,
            expected = fixture.expected_outcome.unwrap_or(""),
        );

        let raw = judge
            .judge(
                JUDGE_SYSTEM,
                &user,
                "{\"verdict\":\"",
                judge_model.as_deref(),
            )
            .await?;
        // Re-attach the prefill so we always parse a complete object.
        let full = format!("{{\"verdict\":\"{raw}");
        let parsed: Value = serde_json::from_str(&full)
            .map_err(|e| anyhow!("judge output not JSON: {e}; got {full}"))?;
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
        // Judge returns the content AFTER the prefill `{"verdict":"` — so "pass\",\"score\":0.85}"
        let out = run_with(StubJudge("pass\",\"score\":0.85,\"notes\":\"ok\"}")).await;
        assert!(out.pass);
        assert!((out.score - 0.85).abs() < 1e-4);
        assert_eq!(out.notes.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn parses_fail_verdict() {
        let out = run_with(StubJudge("fail\",\"score\":0.1}")).await;
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
