//! `skill_distill` LLM-callable tool — writes a new skill directory from
//! the calling agent's in-context synthesis + mechanical transcript extraction.

use anyhow::{bail, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::adapters::skill_lifecycle::fixtures::{
    extract_fixtures, write_fixtures, ExtractOpts, FixturesFile,
};
use crate::adapters::skill_lifecycle::metrics::MetricSpec;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) const SKILL_DISTILL_TOOL_NAME: &str = "skill_distill";

pub(crate) struct SkillDistillTool {
    def: ToolDef,
}

impl SkillDistillTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef {
                name: SKILL_DISTILL_TOOL_NAME.to_string(),
                description: "Create a new skill from the current conversation: write SKILL.md, seed fixtures from the transcript, scaffold metrics files. The new skill does NOT load into the current conversation; it becomes available on next session start.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "body_markdown": { "type": "string" },
                        "metrics": { "type": "array" },
                        "from_message_index": { "type": "integer", "minimum": 0 },
                        "tier": { "type": "string", "enum": ["project", "workspace", "managed"] },
                        "fixture_hints": {
                            "type": "object",
                            "properties": {
                                "include_user_messages": { "type": "boolean" },
                                "expected_outcome": { "type": "string" },
                                "drop_tool_names": { "type": "array", "items": { "type": "string" } }
                            }
                        }
                    },
                    "required": ["name", "description", "body_markdown", "metrics", "from_message_index"]
                }),
            },
        }
    }
}

#[derive(Deserialize)]
struct DistillArgs {
    name: String,
    description: String,
    body_markdown: String,
    metrics: Vec<MetricSpec>,
    from_message_index: usize,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    fixture_hints: Option<FixtureHints>,
}

#[derive(Deserialize, Default)]
struct FixtureHints {
    #[serde(default)]
    include_user_messages: Option<bool>,
    #[serde(default)]
    expected_outcome: Option<String>,
    #[serde(default)]
    drop_tool_names: Option<Vec<String>>,
}

#[async_trait]
impl Tool for SkillDistillTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let args: DistillArgs = serde_json::from_value(args.clone())?;
        let tier = args.tier.as_deref().unwrap_or("project");

        // Resolve tier path
        let tier_root: PathBuf = match tier {
            "project" => ctx.workspace.join("skills"),
            "workspace" => ctx.workspace.join(".tengu").join("skills"),
            "managed" => {
                bail!("UnsupportedTier: managed tier not supported until check_fs_write_managed_skills lands");
            }
            other => bail!("unknown tier '{}'", other),
        };

        // Scope check — first line before any mutation
        ctx.scope.check_fs_write(&tier_root)?;

        // Name validation
        validate_name(&args.name)?;

        // Collision check (all three tiers)
        for candidate in collision_candidates(ctx.workspace, &args.name) {
            if candidate.exists() {
                return Ok(err_payload(
                    "SkillExists",
                    json!({
                        "name": args.name,
                        "tier": tier,
                        "existing_path": candidate.display().to_string(),
                    }),
                ));
            }
        }

        let skill_dir = tier_root.join(&args.name);

        // Validate metrics structurally (file-existence checks skipped — files are
        // being created by this very call).
        validate_metrics_structural(&args.metrics)?;

        // Transcript slice
        let conv_len = ctx.conversation.len();
        if args.from_message_index > conv_len {
            return Ok(err_payload(
                "InvalidTranscriptRange",
                json!({
                    "given": args.from_message_index,
                    "conversation_length": conv_len,
                }),
            ));
        }
        let slice = ctx.conversation.slice(args.from_message_index, conv_len)?;

        // Extract fixtures
        let hints = args.fixture_hints.unwrap_or_default();
        let opts = ExtractOpts {
            include_user_messages: hints.include_user_messages.unwrap_or(true),
            expected_outcome: hints.expected_outcome,
            drop_tool_names: hints.drop_tool_names.unwrap_or_default(),
            metric_names: args.metrics.iter().map(|m| m.name().to_string()).collect(),
        };
        let fixtures = extract_fixtures(slice, &opts);

        // Atomic write via tempdir + rename
        let tmp = tier_root.join(format!(".{}.tmp-{}", args.name, nanos()));
        std::fs::create_dir_all(&tmp)?;

        // Compose SKILL.md
        let metrics_yaml = serde_yaml::to_string(&args.metrics)?;
        let skill_md = format!(
            "---\nname: {}\ndescription: {}\nmetrics:\n{}---\n\n{}\n",
            args.name,
            args.description,
            indent(&metrics_yaml, 2),
            args.body_markdown.trim_end(),
        );
        std::fs::write(tmp.join("SKILL.md"), skill_md)?;

        // evals/prompts.yaml
        std::fs::create_dir_all(tmp.join("evals"))?;
        write_fixtures(
            &tmp.join("evals").join("prompts.yaml"),
            &FixturesFile {
                schema_version: 1,
                fixtures,
            },
        )?;

        // metrics/ scaffolds
        let metrics_dir = tmp.join("metrics");
        std::fs::create_dir_all(&metrics_dir)?;
        for spec in &args.metrics {
            match spec {
                MetricSpec::LlmJudge {
                    name, rubric_file, ..
                } => {
                    let stub =
                        format!("# Rubric for {name}\n\nDescribe pass criteria here.\n");
                    let target =
                        metrics_dir.join(Path::new(rubric_file).file_name().unwrap());
                    std::fs::write(target, stub)?;
                }
                MetricSpec::Script { name, path, .. } => {
                    let stub = format!(
                        "#!/bin/sh\n# Metric script for {name}\necho '{{\"pass\":false,\"score\":0.0,\"notes\":\"unimplemented\"}}'\n"
                    );
                    let target = metrics_dir.join(Path::new(path).file_name().unwrap());
                    std::fs::write(target, stub)?;
                }
                _ => {}
            }
        }

        // Atomic rename
        std::fs::rename(&tmp, &skill_dir)?;

        Ok(ToolOutput {
            text: json!({
                "path": skill_dir_relative(&skill_dir, ctx.workspace),
                "tier": tier,
                "fixtures_created": count_fixtures(&skill_dir),
                "metrics_declared": args.metrics.len(),
                "loaded_in_current_conversation": false,
            })
            .to_string(),
        })
    }
}

fn validate_name(name: &str) -> Result<()> {
    let re = Regex::new("^[a-z][a-z0-9-]{1,63}$").unwrap();
    if !re.is_match(name) {
        bail!(
            "invalid skill name '{}': expected kebab-case, ^[a-z][a-z0-9-]{{1,63}}$",
            name
        );
    }
    Ok(())
}

fn collision_candidates(workspace: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = vec![
        workspace.join("skills").join(name),
        workspace.join(".tengu").join("skills").join(name),
    ];
    if let Some(home) = dirs_next::home_dir() {
        out.push(home.join(".tengu").join("skills").join(name));
    }
    out
}

fn validate_metrics_structural(specs: &[MetricSpec]) -> Result<()> {
    // Structural-only (skips file existence checks — files are created by this tool).
    let mut seen = std::collections::HashSet::new();
    for spec in specs {
        if !seen.insert(spec.name().to_string()) {
            bail!("duplicate metric name: {}", spec.name());
        }
        if let Some(rate) = spec.min_pass_rate() {
            if !(0.0..=1.0).contains(&rate) {
                bail!(
                    "min_pass_rate {} outside [0,1] for metric '{}'",
                    rate,
                    spec.name()
                );
            }
        }
        if let MetricSpec::ShellCheck {
            cmd,
            expect_stdout_matches,
            expect_exit_code,
            ..
        } = spec
        {
            if cmd.trim().is_empty() {
                bail!("empty cmd");
            }
            if expect_stdout_matches.is_none() && expect_exit_code.is_none() {
                bail!(
                    "shell_check '{}' needs expect_stdout_matches or expect_exit_code",
                    spec.name()
                );
            }
        }
    }
    Ok(())
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn indent(s: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    s.lines().map(|l| format!("{pad}{l}\n")).collect()
}

fn err_payload(kind: &str, details: Value) -> ToolOutput {
    ToolOutput {
        text: json!({ "error": kind, "details": details }).to_string(),
    }
}

fn skill_dir_relative(dir: &Path, workspace: &Path) -> String {
    dir.strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| dir.display().to_string())
}

fn count_fixtures(skill_dir: &Path) -> usize {
    crate::adapters::skill_lifecycle::fixtures::read_fixtures(
        &skill_dir.join("evals").join("prompts.yaml"),
    )
    .map(|f| f.fixtures.len())
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolScope};
    use crate::adapters::secret_builder::SecretRegistry;
    use crate::adapters::tool_plugin::ConversationView;
    use crate::adapters::types::{Message, Role, ToolCall};
    use tempfile::TempDir;

    struct NoShell;
    impl ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
            Ok(String::new())
        }
    }

    struct NoActivity;
    impl ToolActivityPort for NoActivity {
        fn publish_tool_activity(&self, _: &ToolCall) {}
    }

    fn make_ctx<'a>(
        ws: &'a Path,
        scope: &'a ToolScope,
        shell: &'a dyn ShellExecutionPort,
        http: &'a reqwest::Client,
        secrets: &'a SecretRegistry,
        activity: &'a dyn ToolActivityPort,
        messages: &'a [Message],
    ) -> ToolCtx<'a> {
        ToolCtx {
            workspace: ws,
            scope,
            shell,
            http,
            memory: None,
            secret_registry: secrets,
            activity,
            subagents: None,
            conversation: ConversationView::new(messages),
        }
    }

    fn permissive_scope(workspace: &Path) -> ToolScope {
        ToolScope {
            fs_roots: vec![workspace.to_path_buf()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn creates_skill_with_fixtures_and_scaffolds() {
        let ws = TempDir::new().unwrap();
        let scope = permissive_scope(ws.path());
        let shell = NoShell;
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity = NoActivity;

        let messages = vec![
            Message {
                role: Role::User,
                content: "mint please".into(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Assistant,
                content: "ok".into(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let tool = SkillDistillTool::new();
        let args = json!({
            "name": "mint-ipnft",
            "description": "Use when minting an IPNFT.",
            "body_markdown": "## Procedure\n1. Call x.\n",
            "metrics": [
                { "kind": "shell_check", "name": "m1", "cmd": "echo ok", "expect_exit_code": 0 }
            ],
            "from_message_index": 0
        });
        let out = tool
            .execute(
                &args,
                &make_ctx(
                    ws.path(),
                    &scope,
                    &shell,
                    &http,
                    &secrets,
                    &activity,
                    &messages,
                ),
            )
            .await
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["fixtures_created"], 1);
        assert_eq!(v["metrics_declared"], 1);
        assert_eq!(v["loaded_in_current_conversation"], false);

        let skill_md =
            std::fs::read_to_string(ws.path().join("skills/mint-ipnft/SKILL.md")).unwrap();
        assert!(skill_md.contains("name: mint-ipnft"));
        assert!(skill_md.contains("## Procedure"));
        assert!(ws
            .path()
            .join("skills/mint-ipnft/evals/prompts.yaml")
            .exists());
    }

    #[tokio::test]
    async fn rejects_invalid_name() {
        let ws = TempDir::new().unwrap();
        let scope = permissive_scope(ws.path());
        let shell = NoShell;
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity = NoActivity;

        let tool = SkillDistillTool::new();
        let args = json!({
            "name": "Bad_Name",
            "description": "x",
            "body_markdown": "y",
            "metrics": [],
            "from_message_index": 0
        });
        let err = tool
            .execute(
                &args,
                &make_ctx(ws.path(), &scope, &shell, &http, &secrets, &activity, &[]),
            )
            .await;
        assert!(err.is_err(), "expected invalid-name error");
    }

    #[tokio::test]
    async fn rejects_collision() {
        let ws = TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join("skills/existing")).unwrap();
        std::fs::write(ws.path().join("skills/existing/SKILL.md"), "x").unwrap();

        let scope = permissive_scope(ws.path());
        let shell = NoShell;
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity = NoActivity;

        let tool = SkillDistillTool::new();
        let args = json!({
            "name": "existing",
            "description": "x",
            "body_markdown": "y",
            "metrics": [],
            "from_message_index": 0
        });
        let out = tool
            .execute(
                &args,
                &make_ctx(ws.path(), &scope, &shell, &http, &secrets, &activity, &[]),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["error"], "SkillExists");
    }
}
