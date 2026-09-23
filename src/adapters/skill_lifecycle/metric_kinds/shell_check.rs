//! `shell_check` metric kind — run a command, match exit code + stdout regex.
//!
//! **Implicit exit-code default:** when `expect_exit_code` is omitted, the kind
//! still requires the command to succeed (exit 0). Specifying only
//! `expect_stdout_matches` does NOT disable the exit-code check. If you need to
//! score a stdout match regardless of exit code, open an issue — there is no
//! knob for it today.

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};
use crate::ports::shell::ShellExecutionPort;

pub(crate) struct ShellCheckKind;

#[async_trait]
impl MetricKind for ShellCheckKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        _fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let (cmd, expect_stdout, expect_exit) = match spec {
            MetricSpec::ShellCheck {
                cmd,
                expect_stdout_matches,
                expect_exit_code,
                ..
            } => (
                cmd.clone(),
                expect_stdout_matches.clone(),
                *expect_exit_code,
            ),
            _ => anyhow::bail!("ShellCheckKind given wrong spec"),
        };

        let (stdout, exit_ok) = run_cmd(ctx.shell, &cmd, ctx.workspace);

        let expected_exit = expect_exit.unwrap_or(0);
        let exit_match = exit_ok == (expected_exit == 0);

        let stdout_match = match expect_stdout {
            Some(pat) => {
                let re = Regex::new(&pat)?;
                re.is_match(&stdout)
            }
            None => true,
        };

        let pass = exit_match && stdout_match;
        Ok(MetricOutcome {
            pass,
            score: if pass { 1.0 } else { 0.0 },
            notes: if pass {
                None
            } else {
                Some(format!(
                    "exit_match={}, stdout_match={}, stdout={:?}",
                    exit_match,
                    stdout_match,
                    stdout.chars().take(120).collect::<String>()
                ))
            },
            raw: json!({ "stdout": stdout, "exit_ok": exit_ok }),
        })
    }
}

fn run_cmd(
    shell: &dyn ShellExecutionPort,
    cmd: &str,
    workspace: &std::path::Path,
) -> (String, bool) {
    match shell.execute_shell(cmd, workspace) {
        Ok(out) => (out, true),
        Err(e) => (format!("ERR: {e}"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct MockShell {
        stdout: String,
        ok: bool,
    }
    impl ShellExecutionPort for MockShell {
        fn execute_shell(&self, _cmd: &str, _ws: &Path) -> Result<String> {
            if self.ok {
                Ok(self.stdout.clone())
            } else {
                Err(anyhow::anyhow!("nonzero"))
            }
        }
    }

    fn spec_regex_exit(pat: Option<&str>, exit: Option<i32>) -> MetricSpec {
        MetricSpec::ShellCheck {
            name: "m".into(),
            cmd: "x".into(),
            expect_stdout_matches: pat.map(str::to_string),
            expect_exit_code: exit,
            min_pass_rate: None,
        }
    }

    async fn run(shell: MockShell, spec: MetricSpec) -> MetricOutcome {
        let ws = std::env::temp_dir();
        let fixture = FixtureContext {
            prompt: "",
            expected_outcome: None,
            transcript: "",
        };
        let ctx = MetricRunCtx {
            skill_dir: &ws,
            workspace: &ws,
            shell: &shell,
            tools: None,
            judge: None,
            http: None,
            memory_manager: None,
            secret_registry: None,
            activity: None,
            tool_scopes: None,
            conversation: None,
            sibling_metrics: None,
        };
        ShellCheckKind.run(&spec, &fixture, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn passes_when_regex_matches_and_exit_ok() {
        let out = run(
            MockShell {
                stdout: "0xabc".into(),
                ok: true,
            },
            spec_regex_exit(Some("^0x[a-f0-9]+$"), Some(0)),
        )
        .await;
        assert!(out.pass);
        assert_eq!(out.score, 1.0);
    }

    #[tokio::test]
    async fn fails_when_regex_mismatches() {
        let out = run(
            MockShell {
                stdout: "nope".into(),
                ok: true,
            },
            spec_regex_exit(Some("^0x[a-f0-9]+$"), Some(0)),
        )
        .await;
        assert!(!out.pass);
        assert_eq!(out.score, 0.0);
        assert!(out.notes.as_deref().unwrap().contains("stdout_match=false"));
    }

    #[tokio::test]
    async fn fails_when_exit_nonzero_and_exit_expected_zero() {
        let out = run(
            MockShell {
                stdout: "".into(),
                ok: false,
            },
            spec_regex_exit(None, Some(0)),
        )
        .await;
        assert!(!out.pass);
    }

    #[tokio::test]
    async fn exit_only_mode_passes_on_ok() {
        let out = run(
            MockShell {
                stdout: "".into(),
                ok: true,
            },
            spec_regex_exit(None, Some(0)),
        )
        .await;
        assert!(out.pass);
    }

    #[tokio::test]
    async fn stdout_only_spec_still_requires_exit_ok() {
        // When expect_exit_code is omitted, the default of 0 still applies.
        // A command that fails but produces matching stdout still fails the metric.
        let out = run(
            MockShell {
                stdout: "0xabc".into(),
                ok: false,
            },
            spec_regex_exit(Some("^0x[a-f0-9]+$"), None),
        )
        .await;
        assert!(!out.pass, "stdout-only spec must still require exit 0");
        assert!(out.notes.as_deref().unwrap().contains("exit_match=false"));
    }
}
