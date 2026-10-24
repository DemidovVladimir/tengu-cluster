//! Tests of `adapters/inbound/eval/`: discovery, eval config, rows end to end
//! (mock engines and chat server), skill metrics, the orchestrated row's close.

use super::*;

use crate::domain::message::{Message, StreamEvent};
use crate::ports::engine::EngineContext;
use crate::ports::trace::TraceSink;
use async_trait::async_trait;
use std::sync::atomic::AtomicU32;

#[test]
fn discover_finds_skill_with_markdown_prompts() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let skill_dir = tmp.path().join("skills").join("demo").join("evals");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("prompts.md"),
        "| Prompt | Expected |\n|---|---|\n| \"hi\" | ok |\n",
    )
    .unwrap();
    std::fs::write(skill_dir.join("config.toml"), "# eval config\n").unwrap();

    let skills = discover_skills(&[], &[tmp.path().join("skills")]).expect("discover");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "demo");
    assert_eq!(skills[0].tier, SkillTier::Project);
    assert!(skills[0].prompts_path.ends_with("prompts.md"));
    assert_eq!(skills[0].prompts_format, "markdown");
}

#[test]
fn discover_prefers_yaml_when_both_exist() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let skill_dir = tmp.path().join("skills").join("demo").join("evals");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("prompts.md"), "").unwrap();
    std::fs::write(skill_dir.join("prompts.yaml"), "[]").unwrap();
    std::fs::write(skill_dir.join("config.toml"), "").unwrap();

    let skills = discover_skills(&[], &[tmp.path().join("skills")]).unwrap();
    assert_eq!(skills[0].prompts_format, "yaml");
}

#[test]
fn discover_filters_by_name() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    for name in ["alpha", "beta"] {
        let evals = tmp.path().join("skills").join(name).join("evals");
        std::fs::create_dir_all(&evals).unwrap();
        std::fs::write(
            evals.join("prompts.md"),
            "| Prompt | Expected |\n|---|---|\n| \"x\" | y |\n",
        )
        .unwrap();
        std::fs::write(evals.join("config.toml"), "").unwrap();
    }

    let skills = discover_skills(&["beta".to_string()], &[tmp.path().join("skills")]).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "beta");
}

#[test]
fn discover_does_not_shadow_lower_tier_when_higher_tier_lacks_evals() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let higher = tmp.path().join("higher");
    let lower = tmp.path().join("lower");

    // Higher tier: a skill directory named "demo" but WITHOUT an evals/ folder.
    std::fs::create_dir_all(higher.join("demo")).unwrap();

    // Lower tier: "demo" with a valid evals/ folder.
    let lower_evals = lower.join("demo").join("evals");
    std::fs::create_dir_all(&lower_evals).unwrap();
    std::fs::write(
        lower_evals.join("prompts.md"),
        "| Prompt | Expected |\n|---|---|\n| \"hi\" | ok |\n",
    )
    .unwrap();
    std::fs::write(lower_evals.join("config.toml"), "").unwrap();

    let skills = discover_skills(&[], &[higher.clone(), lower.clone()]).unwrap();
    assert_eq!(
        skills.len(),
        1,
        "expected lower tier's demo to be discovered"
    );
    assert!(
        skills[0].evals_dir.starts_with(&lower),
        "expected lower tier, got {:?}",
        skills[0].evals_dir
    );
}

#[test]
fn tier_for_root_classifies_tenu_skills_as_workspace_when_home_unknown() {
    // A `.tengu/skills` dir outside the managed tier
    // (`<TENGU_HOME>/skills`) MUST be Workspace, never Project.
    let root = std::path::PathBuf::from("/tmp/unrelated/.tengu/skills");
    let tier = tier_for_root(&root);
    assert_eq!(tier, SkillTier::Workspace);
}

#[test]
fn tier_for_root_classifies_plain_skills_as_project() {
    let root = std::path::PathBuf::from("/some/repo/skills");
    let tier = tier_for_root(&root);
    assert_eq!(tier, SkillTier::Project);
}

#[test]
fn eval_config_expands_tmp_workspace_placeholder() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[agents.main]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
default = true
workspace = "{TMP_WORKSPACE}"
skill_packages = ["orchestration"]
"#,
    )
    .unwrap();

    let ws = tmp.path().join("row-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = load_eval_config(&config_path, &ws).expect("load");

    let agent = cfg.agents.get("main").expect("main agent");
    assert_eq!(agent.workspace.as_deref(), Some(ws.as_path()));
}

/// `[default_scopes]` reach eval agents as on every other surface.
#[test]
fn eval_config_folds_default_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[default_scopes.read_file]
fs_roots = ["/nonexistent/eval-scope"]

[agents.main]
engine = "openrouter"
model = "x/y"
default = true
workspace = "{TMP_WORKSPACE}"
"#,
    )
    .unwrap();
    let cfg = load_eval_config(&config_path, tmp.path()).unwrap();
    let scope = &cfg.agents["main"].scopes["read_file"];
    assert_eq!(
        scope.fs_roots,
        [std::path::PathBuf::from("/nonexistent/eval-scope")]
    );
}

const CLAUDE_EVAL: &str = r#"
[agents.cc-eval]
engine = "claude_code"
model = "claude-haiku-4-5"
default = true
workspace = "{TMP_WORKSPACE}"
tools = ["read_file", "list_directory"]

[agents.cc-eval.claude_code]
builtin_tools_profile = "none"
"#;

/// A `claude_code` eval agent loads (it used to be refused): its bridge
/// gets the expanded eval config as a file; a config `Config::load`
/// refuses fails with the reason; no `claude_code` agent = no file.
#[test]
fn claude_code_eval_agent_gets_a_bridge_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    let ws = tmp.path().join("row-ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The file the bridge of the row's default agent (direct dispatch)
    // would get.
    let file_for = |body: &str| {
        std::fs::write(&config_path, body).unwrap();
        let cfg = load_eval_config(&config_path, &ws).expect("the eval config loads");
        let (name, agent) = cfg
            .agents
            .iter()
            .find(|(_, a)| a.default)
            .map(|(n, a)| (n.as_str(), a))
            .unwrap();
        let runnable = runnable_agents(&cfg, (name, agent));
        bridge_config_file(&config_path, &ws, &runnable)
    };
    let file = file_for(CLAUDE_EVAL)
        .unwrap()
        .expect("a bridge config file");
    let loaded = Config::load(file.path()).unwrap();
    assert_eq!(
        loaded.agents["cc-eval"].workspace.as_deref(),
        Some(ws.as_path())
    );

    // `workspace_tools` takes opt-in names only: the bridge would exit.
    let invalid = CLAUDE_EVAL.replace(
        "tools = [\"read_file\", \"list_directory\"]",
        "workspace_tools = [\"http_request\"]",
    );
    let err = format!("{:#}", file_for(&invalid).unwrap_err());
    assert!(
        err.contains("agent 'cc-eval' runs claude_code") && err.contains("http_request"),
        "{err}"
    );

    // The row runs only the default (openrouter) agent: no bridge, so a
    // claude_code agent beside it is never checked.
    let openrouter = "[agents.main]\nengine = \"openrouter\"\nmodel = \"x/y\"\ndefault = true\n";
    assert!(file_for(openrouter).unwrap().is_none());
    let beside = format!(
        "{openrouter}\n{}",
        invalid.replace("default = true", "default = false")
    );
    assert!(file_for(&beside).unwrap().is_none());
}

/// Judge stand-in: every row passes.
struct PassJudge;

#[async_trait]
impl Engine for PassJudge {
    fn id(&self) -> &str {
        "pass-judge"
    }
    fn context_window(&self) -> usize {
        8_000
    }
    fn supports_tool_use(&self) -> bool {
        false
    }
    fn manages_own_workspace(&self) -> bool {
        false
    }
    fn available_models(&self) -> Vec<crate::domain::message::ModelInfo> {
        Vec::new()
    }
    async fn run(
        &self,
        _messages: &[Message],
        _tools: &[crate::domain::message::ToolDef],
        _context: &EngineContext,
    ) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> {
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta {
                text: r#"{"verdict": "pass", "rationale": "ok"}"#.into(),
            },
            StreamEvent::Done,
        ])))
    }
}

/// `run_row` on `config` (no transcript, no metrics), the judge passing.
async fn run_one_row(
    config: &Path,
    out_dir: &Path,
    stubs: Vec<StubSpec>,
) -> anyhow::Result<RowResult> {
    let skill = SkillUnderTest {
        name: "probe".into(),
        tier: SkillTier::Project,
        evals_dir: out_dir.to_path_buf(),
        prompts_path: out_dir.join("prompts.yaml"),
        prompts_format: "yaml".into(),
        config_path: config.to_path_buf(),
        skill_md_path: out_dir.join("SKILL.md"),
        skill_dir: out_dir.to_path_buf(),
    };
    let row = PromptRow {
        id: "r1".into(),
        prompt: "read secret.txt".into(),
        expected: "the file is read".into(),
        timeout_secs: 20,
        stubs,
    };
    let judge: Arc<dyn Engine> = Arc::new(PassJudge);
    run_row(RowCtx {
        skill: &skill,
        row: &row,
        judge: Arc::clone(&judge),
        out_dir,
        keep_workspace: false,
        config_path_override: None,
        skill_metrics: &[],
        judge_client: Arc::new(EvalJudgeClient { engine: judge }),
        persist_transcript: false,
    })
    .await
}

/// A stubbed row never reaches a `claude_code` agent: its tools run in
/// the MCP bridge, which has no stub layer — refused before any engine
/// runs, naming the stubbed tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stubbed_row_on_a_claude_code_agent_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, CLAUDE_EVAL).unwrap();
    let stubs = vec![StubSpec {
        tool: "http_request".into(),
        responses: vec![serde_json::json!({"status": 200})],
    }];
    let err = run_one_row(&config, tmp.path(), stubs)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("stubs (http_request)") && err.contains("agent 'cc-eval' runs claude_code"),
        "{err}"
    );
}

/// OpenAI-compatible mock on a loopback port: answers `replies` in order
/// (one connection each) and returns the request bodies.
async fn mock_chat_server(replies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16_384];
            let body = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break String::new();
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                if buf.len() >= end + 4 + len {
                    break String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
                }
            };
            bodies.push(body);
            let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
        bodies
    });
    (url, handle)
}

/// Tool output reaches the eval model redacted: a `local` agent (scripted
/// mock server) reads a file holding a `.env`-style credential
/// (`*_API_KEY` in the env, no vault); the next request carries
/// `[REDACTED]`, never the value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_tool_output_reaches_the_model_redacted() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let secret = format!("eval-probe-{}", uuid::Uuid::new_v4().simple());
    let var = format!("TENGU_EVAL_PROBE_{}_API_KEY", uuid::Uuid::new_v4().simple());
    std::env::set_var(&var, &secret);
    std::fs::write(ws.join("secret.txt"), format!("key={secret}\n")).unwrap();
    let (url, server) = mock_chat_server(vec![
            serde_json::json!({"choices": [{"message": {"content": null, "tool_calls": [{
                "id": "c1", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\": \"secret.txt\"}"}
            }]}, "finish_reason": "tool_calls"}]})
            .to_string(),
            serde_json::json!({"choices": [{"message": {"content": "read it"}, "finish_reason": "stop"}]})
                .to_string(),
        ])
        .await;
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[agents.main]\nengine = \"local\"\nmodel = \"mock\"\ndefault = true\n\
                 workspace = \"{}\"\ntools = [\"read_file\"]\n\n\
                 [agents.main.local]\nbase_url = \"{url}\"\napi_key_env = \"\"\n\n\
                 [agents.main.limits]\ncontext_window = 16384\n",
            ws.display()
        ),
    )
    .unwrap();
    let row = run_one_row(&config, tmp.path(), Vec::new()).await;
    std::env::remove_var(&var);
    let row = row.unwrap();
    assert_eq!(row.verdict, "pass");
    let bodies = server.await.unwrap();
    assert_eq!(bodies.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    let tool_msg = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap()
        .to_string();
    assert!(
        tool_msg.contains("key=[REDACTED]") && !tool_msg.contains(&secret),
        "{tool_msg}"
    );
    assert!(!bodies[1].contains(&secret));
}

/// A `tool_assertion` metric dispatches the tool live in `tengu eval` —
/// it used to fail every row ("tool registry unavailable").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_tool_assertion_dispatches_the_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("probe.txt"), "x").unwrap();
    let (url, _server) = mock_chat_server(vec![serde_json::json!({
        "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}]
    })
    .to_string()])
    .await;
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[agents.main]\nengine = \"local\"\nmodel = \"mock\"\ndefault = true\n\
                 workspace = \"{}\"\ntools = [\"list_directory\"]\n\n\
                 [agents.main.local]\nbase_url = \"{url}\"\napi_key_env = \"\"\n\n\
                 [agents.main.limits]\ncontext_window = 16384\n",
            ws.display()
        ),
    )
    .unwrap();
    let metrics = vec![MetricSpec::ToolAssertion {
        name: "lists-probe".into(),
        tool: "list_directory".into(),
        action: "ls".into(),
        key: None,
        assert: serde_json::json!({"value_matches": "probe\\.txt"}),
        min_pass_rate: None,
    }];
    let skill = SkillUnderTest {
        name: "probe".into(),
        tier: SkillTier::Project,
        evals_dir: tmp.path().to_path_buf(),
        prompts_path: tmp.path().join("prompts.yaml"),
        prompts_format: "yaml".into(),
        config_path: config.clone(),
        skill_md_path: tmp.path().join("SKILL.md"),
        skill_dir: tmp.path().to_path_buf(),
    };
    let row = PromptRow {
        id: "r1".into(),
        prompt: "list".into(),
        expected: "lists".into(),
        timeout_secs: 20,
        stubs: Vec::new(),
    };
    let judge: Arc<dyn Engine> = Arc::new(PassJudge);
    let result = run_row(RowCtx {
        skill: &skill,
        row: &row,
        judge: Arc::clone(&judge),
        out_dir: tmp.path(),
        keep_workspace: false,
        config_path_override: None,
        skill_metrics: &metrics,
        judge_client: Arc::new(EvalJudgeClient { engine: judge }),
        persist_transcript: false,
    })
    .await
    .unwrap();
    let outcome = &result.metric_outcomes[0];
    assert!(outcome.pass, "{outcome:?}");
}

/// A `claude_code` eval agent gets its tengu tools through the MCP
/// bridge, like webhooks: a stub `claude` CLI logs its argv and the
/// `--mcp-config` it was handed — `--mcp-config` + `--allowedTools` for
/// the agent's tools, the bridge running as `cc-eval` of the expanded
/// eval config (not the base config in the env).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_code_eval_agent_gets_its_tools_through_the_bridge() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("cli.log");
    let cli = tmp.path().join("fake-claude.sh");
    std::fs::write(
        &cli,
        format!(
            r#"#!/bin/sh
LOG="{log}"
: > "$LOG"
next=""
cfg=""
for a in "$@"; do
  printf 'arg=%s\n' "$a" >> "$LOG"
  if [ "$next" = "mcp" ]; then cfg="$a"; fi
  next=""
  if [ "$a" = "--mcp-config" ]; then next="mcp"; fi
done
if [ -n "$cfg" ]; then
  printf 'mcp-config=%s\n' "$(cat "$cfg")" >> "$LOG"
  conf=$(sed -n 's/.*"TENGU_CONFIG":"\([^"]*\)".*/\1/p' "$cfg")
  printf -- '--- tengu-config\n' >> "$LOG"
  cat "$conf" >> "$LOG"
fi
cat > /dev/null
echo '{{"type":"result","subtype":"success","result":"done","is_error":false,"num_turns":1}}'
"#,
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[claude_code]\ncli_path = \"{}\"\n{CLAUDE_EVAL}",
            cli.display()
        ),
    )
    .unwrap();

    let row = run_one_row(&config, tmp.path(), Vec::new()).await.unwrap();
    assert_eq!(row.verdict, "pass");
    let logged = std::fs::read_to_string(&log).unwrap();
    let args: Vec<&str> = logged
        .lines()
        .filter_map(|l| l.strip_prefix("arg="))
        .collect();
    for want in [
        "--strict-mcp-config",
        "--mcp-config",
        "--allowedTools",
        "mcp__tengu-tools__read_file",
        "mcp__tengu-tools__list_directory",
    ] {
        assert!(args.contains(&want), "{want} missing: {args:?}");
    }
    let mcp: serde_json::Value = serde_json::from_str(
        logged
            .lines()
            .find_map(|l| l.strip_prefix("mcp-config="))
            .expect("the CLI got an --mcp-config"),
    )
    .unwrap();
    let env = &mcp["mcpServers"]["tengu-tools"]["env"];
    assert_eq!(env["TENGU_BRIDGE_AGENT"], "cc-eval");
    let bridged: Vec<String> = serde_json::from_str::<Vec<crate::domain::message::ToolDef>>(
        env["TENGU_BRIDGE_TOOLS"].as_str().unwrap(),
    )
    .unwrap()
    .into_iter()
    .map(|t| t.name)
    .collect();
    assert!(
        bridged.contains(&"read_file".to_string())
            && bridged.contains(&"list_directory".to_string()),
        "{bridged:?}"
    );
    let tengu_config = env["TENGU_CONFIG"].as_str().unwrap();
    assert_ne!(
        std::path::Path::new(tengu_config),
        crate::config::paths::default_config_path(),
        "the bridge must load the eval config"
    );
    let bridge_config = logged.split("--- tengu-config\n").nth(1).unwrap_or("");
    assert!(bridge_config.contains("[agents.cc-eval]"), "{logged}");
    assert!(
        !bridge_config.contains("{TMP_WORKSPACE}"),
        "{bridge_config}"
    );
}

#[tokio::test]
async fn run_returns_2_when_sandbox_path_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let args = EvalArgs {
        skills: vec!["orchestration".to_string()],
        sandbox: Some("does-not-exist".to_string()),
        judge_model: None,
        concurrency: 1,
        format: OutputFormat::Table,
        out_dir: Some(tmp.path().to_path_buf()),
        filter: None,
        keep_workspace: false,
        keep_runs: None,
        no_persist: false,
        max_per_run_reports: 10,
    };
    std::env::set_var("OPENROUTER_API_KEY", "sk-test-not-used");
    let exit = run(args).await.unwrap();
    assert_eq!(exit, 2, "missing sandbox config must exit 2");
}

#[tokio::test]
async fn run_returns_2_when_filter_misses() {
    let tmp = tempfile::tempdir().unwrap();
    // Pass a filter for a nonexistent skill so discover_skills errors
    // out with "skill 'X' has no evals/prompts.{md,yaml}" before we ever
    // reach judge construction. Runner-level error → exit 2.
    let args = EvalArgs {
        skills: vec!["does-not-exist".to_string()],
        sandbox: None,
        judge_model: None,
        concurrency: 1,
        format: OutputFormat::Table,
        out_dir: Some(tmp.path().to_path_buf()),
        filter: None,
        keep_workspace: false,
        keep_runs: None,
        no_persist: false,
        max_per_run_reports: 10,
    };
    // Defensive: set a fake key so that if execution ever did reach the
    // judge builder, it would not fail with an env-var error for a
    // different reason than what we're testing.
    std::env::set_var("OPENROUTER_API_KEY", "sk-test-not-used");
    let exit = run(args).await.unwrap();
    assert_eq!(
        exit, 2,
        "expected exit 2 for filter miss (runner-level error)"
    );
}

// ---------------------------------------------------------------------------
// load_skill_metrics tests (α.8)
// ---------------------------------------------------------------------------

#[test]
fn load_skill_metrics_empty_when_no_frontmatter() {
    // A SKILL.md without frontmatter should return an empty vec (backward compat).
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path();
    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_md,
        "# My Skill\n\nThis skill has no metrics frontmatter.\n",
    )
    .unwrap();
    let metrics = load_skill_metrics(&skill_md, skill_dir).expect("load_skill_metrics");
    assert!(
        metrics.is_empty(),
        "expected empty metrics, got {:?}",
        metrics
    );
}

#[test]
fn load_skill_metrics_parses_all_four_kinds() {
    // Happy-path: SKILL.md with all 4 metric kinds.
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path();

    // Create the files validate_metrics needs on disk.
    std::fs::write(skill_dir.join("rubric.md"), "# Rubric\n").unwrap();
    std::fs::write(skill_dir.join("check.sh"), "#!/bin/sh\nexit 0\n").unwrap();

    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_md,
        r#"---
metrics:
  - kind: shell_check
    name: lints-clean
    cmd: "echo ok"
    expect_exit_code: 0
  - kind: llm_judge
    name: quality
    rubric_file: rubric.md
    min_pass_rate: 0.8
  - kind: tool_assertion
    name: tool-called
    tool: http_request
    action: call
    assert: {"status": 200}
  - kind: script
    name: custom-script
    path: check.sh
---

# My Skill

Skill body here.
"#,
    )
    .unwrap();

    let metrics = load_skill_metrics(&skill_md, skill_dir).expect("load_skill_metrics");
    assert_eq!(metrics.len(), 4, "expected 4 metric specs");
    let names: Vec<&str> = metrics.iter().map(|m| m.name()).collect();
    assert!(names.contains(&"lints-clean"));
    assert!(names.contains(&"quality"));
    assert!(names.contains(&"tool-called"));
    assert!(names.contains(&"custom-script"));
}

#[test]
fn load_skill_metrics_bails_on_missing_rubric_file() {
    // validate_metrics should surface an error when rubric_file doesn't exist on disk.
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path();
    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_md,
        r#"---
metrics:
  - kind: llm_judge
    name: quality
    rubric_file: nonexistent.md
---

# Skill
"#,
    )
    .unwrap();

    let err = load_skill_metrics(&skill_md, skill_dir)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("rubric_file missing") || err.contains("nonexistent.md"),
        "unexpected error: {}",
        err
    );
}

/// Finding P2 (trace closure): a metric put on the process sink just
/// before the row ends is in the trace (and with the judge's observer)
/// before `run.closed` — the metrics barrier, no timed sleep. On a
/// current-thread runtime the forwarder has not run once when the row
/// closes, so the old 100 ms sleep was all that ordered them. After
/// `close_row` the row's gate is shut: a straggler lands nowhere.
#[tokio::test]
async fn close_row_writes_a_late_metric_before_run_closed() {
    use crate::application::trace_exec::tests::MemTrace;
    use crate::application::trace_exec::GatedTrace;
    use crate::domain::metrics::{MetricsKind, MetricsRecord};
    let cfg = Config {
        orchestrator: Some(toml::from_str("agent = \"planner\"").unwrap()),
        ..Config::default()
    };
    let sink = Arc::new(MemTrace::default());
    let trace: Arc<dyn TraceSink> = sink.clone();
    let gate = GatedTrace::new(Arc::clone(&trace));
    let via: Arc<dyn TraceSink> = gate.clone();
    let session = format!("eval-barrier-{}", uuid::Uuid::new_v4());
    // Never called: the barrier test runs no turn.
    use crate::application::orchestrator::planner::testkit;
    let orchestrator = crate::bootstrap::orchestrator::build_orchestrator(
        &cfg,
        testkit::llm(testkit::TextEngine::new("", 0)),
        session.clone(),
        crate::application::orchestrator::trace::OrchestratorTrace::of(&via, None),
    )
    .unwrap();
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tokens = Arc::new(std::sync::Mutex::new((0, 0)));
    let observer = RowObserver::start(
        orchestrator.subscribe(),
        Arc::clone(&observations),
        Arc::new(AtomicU32::new(0)),
        Arc::clone(&tokens),
        session.clone(),
    );
    crate::application::metrics::record(MetricsRecord {
        ts_unix: 0,
        session_id: session.clone(),
        kind: MetricsKind::Planner,
        agent: "planner/planner".into(),
        model: session.clone(),
        prompt_tokens: 10,
        cached_input_tokens: 0,
        completion_tokens: 2,
        total_tokens: 12,
        prompt_chars: 40,
        prompt_bytes: 40,
        response_chars: 8,
        latency_ms: 5,
        layers: Vec::new(),
        step_id: None,
    });
    close_row(&orchestrator, observer, Some(&gate), Some(&trace), false).await;
    // The trace bridge keeps this session's record only.
    assert_eq!(sink.kinds(), ["metrics.recorded", "run.closed"]);
    assert_eq!(sink.all()[0].payload["total_tokens"], serde_json::json!(12));
    // The observer mirrors every record on the bus (other tests share the
    // process sink): this one is among them.
    let model = format!("model={session} ");
    let seen = observations
        .lock()
        .unwrap()
        .iter()
        .filter(|o| o.args_preview.contains(&model))
        .map(|o| o.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(seen, ["orchestrator:metrics_recorded"]);
    // The row's tokens are its own session's records only.
    assert_eq!(*tokens.lock().unwrap(), (10, 2));
    let late = crate::domain::trace::EventDraft::new(
        crate::domain::trace::Component::Orchestrator,
        "plan.completed",
        crate::domain::trace::Status::Ok,
    );
    assert_eq!(via.emit(late), None);
    assert_eq!(sink.kinds().last().map(String::as_str), Some("run.closed"));
}
