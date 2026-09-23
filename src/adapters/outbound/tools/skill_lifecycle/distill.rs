//! `skill_distill` LLM-callable tool — writes a new skill directory from
//! the calling agent's in-context synthesis + mechanical transcript extraction.

use anyhow::{bail, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::application::skills::lifecycle::fixtures::{
    extract_fixtures, write_fixtures, ExtractOpts, Fixture, FixturesFile,
};
use crate::application::skills::lifecycle::metrics::MetricSpec;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

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
        let tmp = tier_root.join(format!(".{}.tmp-{}", args.name, unique_suffix()));
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
                fixtures: fixtures.clone(),
            },
        )?;

        // evals/config.toml — auto-seeded so the freshly distilled skill is
        // immediately runnable via `tengu eval <name>`. When `agent_config`
        // is threaded through ToolCtx (Stream M), the seeded engine + model
        // mirror the calling agent so the eval runs through the same backend
        // that produced the distillation. Falls back to sonnet+openrouter
        // when agent_config is None (e.g. distill called directly via the
        // test path).
        let engine_model = ctx
            .agent_config
            .map(|a| (a.engine.as_str(), a.model.as_str()));
        let eval_config = build_eval_config_toml(&args.name, &fixtures, engine_model);
        std::fs::write(tmp.join("evals").join("config.toml"), eval_config)?;

        // metrics/ scaffolds
        let metrics_dir = tmp.join("metrics");
        std::fs::create_dir_all(&metrics_dir)?;
        for spec in &args.metrics {
            match spec {
                MetricSpec::LlmJudge {
                    name, rubric_file, ..
                } => {
                    let stub = format!("# Rubric for {name}\n\nDescribe pass criteria here.\n");
                    let target = metrics_dir.join(Path::new(rubric_file).file_name().unwrap());
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

/// Suffix for temp / quarantine names: unique per call. (Clock-based names
/// collided between concurrent writers — macOS ticks in microseconds.)
fn unique_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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

/// Build the contents of `evals/config.toml` for a freshly distilled skill.
///
/// `workspace_tools` is the union of tool names observed in the extracted
/// fixtures' `expected_tool_calls`. `engine_model` (Stream M) is the
/// calling agent's `(engine, model)` pulled from `ToolCtx::agent_config` —
/// when `None`, falls back to the legacy sonnet+openrouter default.
fn build_eval_config_toml(
    skill_name: &str,
    fixtures: &[Fixture],
    engine_model: Option<(&str, &str)>,
) -> String {
    let agent_block = format!("{skill_name}-agent");
    let identity_name = format!("{} Eval Agent", title_case_kebab(skill_name));

    // Union of tool names across all fixtures, deterministic order.
    let mut seen = std::collections::BTreeSet::new();
    for f in fixtures {
        for c in &f.expected_tool_calls {
            seen.insert(c.tool.clone());
        }
    }
    let workspace_tools = seen
        .into_iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let (engine, model) = engine_model.unwrap_or(("openrouter", "anthropic/claude-sonnet-4-6"));

    format!(
        "# Auto-generated by skill_distill. Edit freely.\n\
         #\n\
         # Consumed by `tengu eval {skill_name}`.\n\
         \n\
         runtime_profile = \"cloud\"\n\
         \n\
         [agents.{agent_block}]\n\
         default = true\n\
         engine = \"{engine}\"\n\
         model = \"{model}\"\n\
         workspace = \"{{TMP_WORKSPACE}}\"\n\
         workspace_tools = [{workspace_tools}]\n\
         \n\
         [agents.{agent_block}.identity]\n\
         name = \"{identity_name}\"\n\
         instructions = \"Execute the fixture; call tools as needed.\"\n\
         \n\
         [agents.{agent_block}.limits]\n\
         max_tokens_per_flow = 50000\n",
    )
}

/// `eth-balance-check` -> `Eth Balance Check`. Assumes input is already
/// validated kebab-case (see `validate_name`).
fn title_case_kebab(s: &str) -> String {
    s.split('-')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn count_fixtures(skill_dir: &Path) -> usize {
    crate::application::skills::lifecycle::fixtures::read_fixtures(
        &skill_dir.join("evals").join("prompts.yaml"),
    )
    .map(|f| f.fixtures.len())
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{Message, Role, ToolCall};
    use crate::domain::scope::ToolScope;
    use crate::domain::secrets::SecretRegistry;
    use crate::ports::shell::ShellExecutionPort;
    use crate::ports::tool::ConversationView;
    use crate::ports::tool_activity::ToolActivityPort;
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
            memory_manager: None,
            secret_registry: secrets,
            activity,
            conversation: ConversationView::new(messages),
            agent_config: None,
        }
    }

    fn make_ctx_with_agent<'a>(
        ws: &'a Path,
        scope: &'a ToolScope,
        shell: &'a dyn ShellExecutionPort,
        http: &'a reqwest::Client,
        secrets: &'a SecretRegistry,
        activity: &'a dyn ToolActivityPort,
        messages: &'a [Message],
        agent_config: &'a crate::config::AgentConfig,
    ) -> ToolCtx<'a> {
        ToolCtx {
            workspace: ws,
            scope,
            shell,
            http,
            memory_manager: None,
            secret_registry: secrets,
            activity,
            conversation: ConversationView::new(messages),
            agent_config: Some(agent_config),
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

        // evals/config.toml is auto-seeded (G3) so the skill is immediately
        // runnable via `tengu eval mint-ipnft`.
        let eval_config_path = ws.path().join("skills/mint-ipnft/evals/config.toml");
        assert!(eval_config_path.exists(), "evals/config.toml not created");
        let eval_config = std::fs::read_to_string(&eval_config_path).unwrap();
        assert!(
            eval_config.contains("[agents.mint-ipnft-agent]"),
            "expected [agents.mint-ipnft-agent] in config.toml, got: {eval_config}"
        );
        assert!(eval_config.contains("runtime_profile = \"cloud\""));
        assert!(eval_config.contains("name = \"Mint Ipnft Eval Agent\""));
    }

    #[tokio::test]
    async fn seeds_eval_config_workspace_tools_from_fixture_tool_calls() {
        let ws = TempDir::new().unwrap();
        let scope = permissive_scope(ws.path());
        let shell = NoShell;
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity = NoActivity;

        let messages = vec![
            Message {
                role: Role::User,
                content: "fetch and store".into(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "http_request".into(),
                        arguments: json!({"url": "https://api", "method": "GET"}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "persistent_store".into(),
                        arguments: json!({"key": "k", "value": "v"}),
                    },
                ]),
            },
        ];

        let tool = SkillDistillTool::new();
        let args = json!({
            "name": "fetch-and-store",
            "description": "Use when fetching then persisting.",
            "body_markdown": "## Procedure\n1. fetch.\n2. store.\n",
            "metrics": [],
            "from_message_index": 0
        });
        tool.execute(
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

        let eval_config =
            std::fs::read_to_string(ws.path().join("skills/fetch-and-store/evals/config.toml"))
                .unwrap();

        // Both observed tool names must appear in the workspace_tools array.
        let line = eval_config
            .lines()
            .find(|l| l.starts_with("workspace_tools ="))
            .expect("missing workspace_tools line");
        assert!(
            line.contains("\"http_request\""),
            "http_request missing from workspace_tools: {line}"
        );
        assert!(
            line.contains("\"persistent_store\""),
            "persistent_store missing from workspace_tools: {line}"
        );
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

    #[tokio::test]
    async fn seeds_eval_config_engine_and_model_from_calling_agent() {
        // Stream M — when ToolCtx.agent_config is Some(_), the seeded
        // evals/config.toml mirrors the calling agent's engine + model so
        // `tengu eval <name>` runs through the same backend that produced
        // the distillation. Falls back to sonnet+openrouter when None
        // (covered by `creates_skill_with_fixtures_and_scaffolds`).
        let ws = TempDir::new().unwrap();
        let scope = permissive_scope(ws.path());
        let shell = NoShell;
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity = NoActivity;

        // Fake calling-agent config: claude_code engine + opus model.
        let mut fake_agent = crate::config::Config::default()
            .agents
            .remove("main")
            .expect("default config has 'main' agent");
        fake_agent.engine = "claude_code".to_string();
        fake_agent.model = "claude-opus-4-7".to_string();

        let messages = vec![Message {
            role: Role::User,
            content: "do the thing".into(),
            tool_call_id: None,
            tool_calls: None,
        }];

        let tool = SkillDistillTool::new();
        let args = json!({
            "name": "opus-distilled",
            "description": "Use when distilling under opus.",
            "body_markdown": "## Procedure\n1. Do it.\n",
            "metrics": [],
            "from_message_index": 0,
        });
        tool.execute(
            &args,
            &make_ctx_with_agent(
                ws.path(),
                &scope,
                &shell,
                &http,
                &secrets,
                &activity,
                &messages,
                &fake_agent,
            ),
        )
        .await
        .unwrap();

        let eval_config =
            std::fs::read_to_string(ws.path().join("skills/opus-distilled/evals/config.toml"))
                .unwrap();
        assert!(
            eval_config.contains("engine = \"claude_code\""),
            "expected engine = claude_code, got:\n{eval_config}"
        );
        assert!(
            eval_config.contains("model = \"claude-opus-4-7\""),
            "expected model = claude-opus-4-7, got:\n{eval_config}"
        );
        // Make sure the sonnet default is NOT present.
        assert!(
            !eval_config.contains("anthropic/claude-sonnet-4-6"),
            "expected calling-agent model to override sonnet default"
        );
    }
}
