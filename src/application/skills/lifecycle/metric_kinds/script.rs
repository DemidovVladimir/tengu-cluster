//! `script` — `sh <path>` through the shell port (the `[egress]` wrapper,
//! the shell timeout) in `ctx.workspace`, with `PROMPT`, `TRANSCRIPT` and
//! `EXPECTED_OUTCOME` set; parse stdout JSON `{pass, score, notes?}`. A
//! script that fails to run or exits non-zero is a failed outcome.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::application::skills::lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};
use crate::domain::scope::shell_quote;

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
            MetricSpec::Script { path, .. } => std::path::absolute(ctx.skill_dir.join(path))?,
            _ => anyhow::bail!("ScriptKind given wrong spec"),
        };
        let command = format!("sh {}", shell_quote(&path.to_string_lossy()));
        let env = [
            ("PROMPT", fixture_prompt_env(fixture)),
            ("TRANSCRIPT", fixture_transcript_env(fixture)),
            ("EXPECTED_OUTCOME", fixture_expected_env(fixture)),
        ];
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let stdout = match ctx.shell.execute_shell_env(&command, ctx.workspace, &env) {
            Ok(stdout) => stdout,
            Err(e) => {
                return Ok(MetricOutcome {
                    pass: false,
                    score: 0.0,
                    notes: Some(format!("script failed: {e}")),
                    raw: json!({ "error": e.to_string() }),
                })
            }
        };

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

// The script's env contract: the row's prompt, the agent's reply and the
// expected outcome.
fn fixture_prompt_env(f: &FixtureContext<'_>) -> String {
    f.prompt.to_string()
}
fn fixture_transcript_env(f: &FixtureContext<'_>) -> String {
    f.transcript.to_string()
}
fn fixture_expected_env(f: &FixtureContext<'_>) -> String {
    f.expected_outcome.unwrap_or_default().to_string()
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

    // Scripts run for real: the local shell (egress wrapper, timeout).
    use crate::adapters::outbound::shell::LocalShellExecutor;

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
            shell: &LocalShellExecutor::new(),
            tools: None,
            judge: None,
            http: None,
            secret_registry: None,
            tool_scopes: None,
            conversation: None,
            sibling_metrics: None,
        };
        let spec = MetricSpec::Script {
            name: "m".into(),
            path: spec_path,
            min_pass_rate: None,
        };
        ScriptKind.run(&spec, &fixture, &ctx).await.unwrap()
    }

    /// The script sees the row's prompt, the reply and the expected outcome
    /// (all three used to be empty strings).
    #[tokio::test]
    async fn script_gets_prompt_transcript_and_expected_outcome() {
        let dir = TempDir::new().unwrap();
        let path = write_script(
            dir.path(),
            "#!/bin/sh\nprintf '{\"pass\": true, \"score\": 1, \"notes\": \"%s|%s|%s\"}' \"$PROMPT\" \"$TRANSCRIPT\" \"$EXPECTED_OUTCOME\"\n",
        );
        let ws = std::env::temp_dir();
        let fixture = FixtureContext {
            prompt: "conjugate sein",
            expected_outcome: Some("ich bin"),
            transcript: "ich bin, du bist",
        };
        let ctx = MetricRunCtx {
            skill_dir: dir.path(),
            workspace: &ws,
            shell: &LocalShellExecutor::new(),
            tools: None,
            judge: None,
            http: None,
            secret_registry: None,
            tool_scopes: None,
            conversation: None,
            sibling_metrics: None,
        };
        let spec = MetricSpec::Script {
            name: "m".into(),
            path,
            min_pass_rate: None,
        };
        let out = ScriptKind.run(&spec, &fixture, &ctx).await.unwrap();
        assert_eq!(
            out.notes.as_deref(),
            Some("conjugate sein|ich bin, du bist|ich bin")
        );
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

    /// A non-zero exit is a failed outcome with the reason, not an error.
    #[tokio::test]
    async fn failing_script_yields_fail() {
        let dir = TempDir::new().unwrap();
        let path = write_script(dir.path(), "#!/bin/sh\necho 'boom' >&2\nexit 3\n");
        let out = run_script(&dir, path).await;
        assert!(!out.pass);
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("exit 3") && notes.contains("boom"),
            "{notes}"
        );
    }

    /// A shell that cannot pass variables refuses; the env is never dropped.
    #[test]
    fn a_shell_without_env_refuses_variables() {
        use crate::ports::shell::ShellExecutionPort;
        struct NoEnv;
        impl ShellExecutionPort for NoEnv {
            fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
                Ok(String::new())
            }
        }
        let ws = std::env::temp_dir();
        assert!(NoEnv.execute_shell_env("true", &ws, &[]).is_ok());
        assert!(NoEnv
            .execute_shell_env("true", &ws, &[("PROMPT", "x")])
            .is_err());
    }
}
