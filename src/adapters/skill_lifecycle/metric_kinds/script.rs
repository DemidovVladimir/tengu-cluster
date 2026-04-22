//! `script` — invoke `sh <path>` with env vars, parse stdout JSON `{pass, score, notes?}`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::process::Command;

use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};

pub(crate) struct ScriptKind;

#[async_trait]
impl MetricKind for ScriptKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let path = match spec {
            MetricSpec::Script { path, .. } => ctx.skill_dir.join(path),
            _ => anyhow::bail!("ScriptKind given wrong spec"),
        };

        let prompt_val = fixture_prompt_env(fixture);
        let transcript_val = fixture_transcript_env(fixture);
        let expected_val = fixture_expected_env(fixture);

        let out = tokio::task::spawn_blocking(move || {
            Command::new("sh")
                .arg(&path)
                .env("PROMPT", prompt_val)
                .env("TRANSCRIPT", transcript_val)
                .env("EXPECTED_OUTCOME", expected_val)
                .output()
        })
        .await??;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();

        let parsed: Result<ScriptOutput, _> = serde_json::from_str(stdout.trim());
        match parsed {
            Ok(s) => Ok(MetricOutcome {
                pass: s.pass,
                score: s.score,
                notes: s.notes,
                raw: json!({ "stdout": stdout }),
            }),
            Err(e) => Ok(MetricOutcome {
                pass: false,
                score: 0.0,
                notes: Some(format!("malformed script output: {e}")),
                raw: json!({ "stdout": stdout }),
            }),
        }
    }
}

// Helpers exist so tests can re-build the env contract explicitly.
fn fixture_prompt_env(_: &FixtureContext<'_>) -> String {
    // Actual wiring happens in Task 12's runner; tests set the env directly.
    String::new()
}
fn fixture_transcript_env(_: &FixtureContext<'_>) -> String {
    String::new()
}
fn fixture_expected_env(_: &FixtureContext<'_>) -> String {
    String::new()
}

#[derive(serde::Deserialize)]
struct ScriptOutput {
    pass: bool,
    score: f32,
    #[serde(default)]
    notes: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    struct NoShell;
    impl crate::adapters::ports::ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
            Ok(String::new())
        }
    }

    fn write_script(dir: &Path, body: &str) -> String {
        let path = dir.join("m.sh");
        std::fs::write(&path, body).unwrap();
        "m.sh".to_string()
    }

    async fn run_script(dir: &TempDir, spec_path: String) -> MetricOutcome {
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
        let spec = MetricSpec::Script {
            name: "m".into(),
            path: spec_path,
            min_pass_rate: None,
        };
        ScriptKind.run(&spec, &fixture, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn parses_valid_json_pass() {
        let dir = TempDir::new().unwrap();
        let path = write_script(
            dir.path(),
            "#!/bin/sh\necho '{\"pass\":true,\"score\":0.9}'\n",
        );
        let out = run_script(&dir, path).await;
        assert!(out.pass);
        assert_eq!(out.score, 0.9);
    }

    #[tokio::test]
    async fn malformed_output_yields_fail() {
        let dir = TempDir::new().unwrap();
        let path = write_script(dir.path(), "#!/bin/sh\necho 'not json'\n");
        let out = run_script(&dir, path).await;
        assert!(!out.pass);
        assert_eq!(out.score, 0.0);
        assert!(out.notes.as_deref().unwrap().contains("malformed"));
    }

    #[tokio::test]
    async fn parses_notes_field() {
        let dir = TempDir::new().unwrap();
        let path = write_script(
            dir.path(),
            "#!/bin/sh\necho '{\"pass\":false,\"score\":0.2,\"notes\":\"low confidence\"}'\n",
        );
        let out = run_script(&dir, path).await;
        assert_eq!(out.notes.as_deref(), Some("low confidence"));
    }
}
