//! Metric types shared across kinds. `MetricKind` trait is the dispatch seam.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MetricSpec {
    ShellCheck {
        name: String,
        cmd: String,
        #[serde(default)]
        expect_stdout_matches: Option<String>,
        #[serde(default)]
        expect_exit_code: Option<i32>,
        #[serde(default)]
        min_pass_rate: Option<f32>,
    },
    LlmJudge {
        name: String,
        rubric_file: String,
        #[serde(default)]
        judge_model: Option<String>,
        #[serde(default)]
        min_pass_rate: Option<f32>,
    },
    ToolAssertion {
        name: String,
        tool: String,
        action: String,
        #[serde(default)]
        key: Option<String>,
        assert: serde_json::Value,
        #[serde(default)]
        min_pass_rate: Option<f32>,
    },
    Script {
        name: String,
        path: String,
        #[serde(default)]
        min_pass_rate: Option<f32>,
    },
}

impl MetricSpec {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::ShellCheck { name, .. }
            | Self::LlmJudge { name, .. }
            | Self::ToolAssertion { name, .. }
            | Self::Script { name, .. } => name,
        }
    }

    pub(crate) fn min_pass_rate(&self) -> Option<f32> {
        match self {
            Self::ShellCheck { min_pass_rate, .. }
            | Self::LlmJudge { min_pass_rate, .. }
            | Self::ToolAssertion { min_pass_rate, .. }
            | Self::Script { min_pass_rate, .. } => *min_pass_rate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetricOutcome {
    pub pass: bool,
    pub score: f32,
    pub notes: Option<String>,
    pub raw: serde_json::Value,
}

pub(crate) struct FixtureContext<'a> {
    pub prompt: &'a str,
    pub expected_outcome: Option<&'a str>,
    pub transcript: &'a str,
}

/// Runtime context passed to every metric kind.
pub(crate) struct MetricRunCtx<'a> {
    pub skill_dir: &'a Path,
    pub workspace: &'a Path,
    pub shell: &'a dyn crate::adapters::ports::ShellExecutionPort,
    // Extended in later tasks (LLM client, tool registry handle).
}

#[async_trait]
pub(crate) trait MetricKind: Send + Sync {
    async fn run(
        &self,
        spec: &MetricSpec,
        fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome>;
}

/// Validate a list of metric specs against the §6.2 invariants.
pub(crate) fn validate_metrics(specs: &[MetricSpec], skill_dir: &Path) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for spec in specs {
        let name = spec.name();
        if !seen.insert(name.to_string()) {
            bail!("duplicate metric name: {}", name);
        }
        if let Some(rate) = spec.min_pass_rate() {
            if !(0.0..=1.0).contains(&rate) {
                bail!("metric '{}' has min_pass_rate {} outside [0,1]", name, rate);
            }
        }
        match spec {
            MetricSpec::ShellCheck {
                cmd,
                expect_stdout_matches,
                expect_exit_code,
                ..
            } => {
                if cmd.trim().is_empty() {
                    bail!("metric '{}' has empty cmd", name);
                }
                if expect_stdout_matches.is_none() && expect_exit_code.is_none() {
                    bail!("metric '{}' must set expect_stdout_matches or expect_exit_code", name);
                }
            }
            MetricSpec::LlmJudge { rubric_file, .. } => {
                let p = skill_dir.join(rubric_file);
                if !p.exists() {
                    bail!("metric '{}' rubric_file missing: {}", name, p.display());
                }
            }
            MetricSpec::Script { path, .. } => {
                let p = skill_dir.join(path);
                if !p.exists() {
                    bail!("metric '{}' script path missing: {}", name, p.display());
                }
            }
            MetricSpec::ToolAssertion { tool, .. } => {
                if tool.trim().is_empty() {
                    bail!("metric '{}' tool name empty", name);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn td() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn accepts_valid_shell_check() {
        let dir = td();
        let specs = vec![MetricSpec::ShellCheck {
            name: "ok".into(),
            cmd: "echo hi".into(),
            expect_stdout_matches: Some("^hi$".into()),
            expect_exit_code: Some(0),
            min_pass_rate: Some(0.5),
        }];
        validate_metrics(&specs, dir.path()).unwrap();
    }

    #[test]
    fn rejects_duplicate_name() {
        let dir = td();
        let specs = vec![
            MetricSpec::ShellCheck {
                name: "dup".into(),
                cmd: "x".into(),
                expect_stdout_matches: None,
                expect_exit_code: Some(0),
                min_pass_rate: None,
            },
            MetricSpec::ShellCheck {
                name: "dup".into(),
                cmd: "y".into(),
                expect_stdout_matches: None,
                expect_exit_code: Some(0),
                min_pass_rate: None,
            },
        ];
        let err = validate_metrics(&specs, dir.path()).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_min_pass_rate_out_of_range() {
        let dir = td();
        let specs = vec![MetricSpec::ShellCheck {
            name: "bad".into(),
            cmd: "x".into(),
            expect_stdout_matches: None,
            expect_exit_code: Some(0),
            min_pass_rate: Some(1.5),
        }];
        let err = validate_metrics(&specs, dir.path()).unwrap_err().to_string();
        assert!(err.contains("1.5"), "{err}");
        assert!(err.contains("outside [0,1]"), "{err}");
    }

    #[test]
    fn rejects_shell_check_without_any_expect() {
        let dir = td();
        let specs = vec![MetricSpec::ShellCheck {
            name: "nope".into(),
            cmd: "x".into(),
            expect_stdout_matches: None,
            expect_exit_code: None,
            min_pass_rate: None,
        }];
        let err = validate_metrics(&specs, dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("must set expect_stdout_matches or expect_exit_code"),
            "{err}"
        );
    }

    #[test]
    fn rejects_llm_judge_without_rubric_file() {
        let dir = td();
        let specs = vec![MetricSpec::LlmJudge {
            name: "j".into(),
            rubric_file: "missing.md".into(),
            judge_model: None,
            min_pass_rate: None,
        }];
        let err = validate_metrics(&specs, dir.path()).unwrap_err().to_string();
        assert!(err.contains("rubric_file missing"), "{err}");
    }

    #[test]
    fn accepts_llm_judge_when_rubric_present() {
        let dir = td();
        std::fs::write(dir.path().join("r.md"), "# rubric\n").unwrap();
        let specs = vec![MetricSpec::LlmJudge {
            name: "j".into(),
            rubric_file: "r.md".into(),
            judge_model: None,
            min_pass_rate: Some(0.7),
        }];
        validate_metrics(&specs, dir.path()).unwrap();
    }
}
