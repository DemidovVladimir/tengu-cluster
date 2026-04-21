//! Skill eval runner — `tengu eval <skill>`.
//!
//! Spec: `docs/superpowers/specs/2026-04-19-eval-runner-design.md`.
//! Replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent,
//! scores each row pass/fail via an LLM judge, and writes a report.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct EvalArgs {
    pub skills: Vec<String>,
    pub sandbox: Option<String>,
    pub judge_model: Option<String>,
    pub concurrency: usize,
    pub format: OutputFormat,
    pub out_dir: Option<PathBuf>,
    pub filter: Option<String>,
    pub keep_workspace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

pub async fn run(args: EvalArgs) -> anyhow::Result<i32> {
    let started_at = chrono::Utc::now();
    let out_dir = args.out_dir.unwrap_or_else(|| {
        PathBuf::from("evals/runs").join(started_at.format("%Y-%m-%dT%H-%M-%SZ").to_string())
    });
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("Error: create out dir {}: {}", out_dir.display(), e);
        return Ok(2);
    }

    let roots = default_skill_roots();
    let skills = match discover_skills(&args.skills, &roots) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {}", e);
            return Ok(2);
        }
    };
    if skills.is_empty() {
        eprintln!("no skills with evals/ found in roots: {:?}", roots);
        return Ok(2);
    }

    // Build the judge engine via the shared helper.
    let judge_model = args
        .judge_model
        .unwrap_or_else(|| "anthropic/claude-opus-4-7".to_string());
    let judge: Arc<dyn crate::adapters::types::Engine> =
        match build_judge(Some(judge_model.clone())) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("Error: build judge engine: {}", e);
                return Ok(2);
            }
        };

    let sandbox_config = args
        .sandbox
        .as_ref()
        .map(|name| PathBuf::from("sandboxes").join(name).join("config.toml"));
    if let Some(ref p) = sandbox_config {
        if !p.exists() {
            eprintln!("Error: sandbox config not found: {}", p.display());
            return Ok(2);
        }
    }

    let mut skill_reports = Vec::new();
    let mut runner_exit = 0i32;
    for skill in &skills {
        // Runner-level errors (config parse, engine build, judge network, etc.) → exit 2.
        // Row-level timeouts produce a RowResult with timed_out=true and do NOT bubble
        // through here — they still set runner_exit = 1 via the verdict check below.
        let report = match run_skill(
            skill,
            Arc::clone(&judge),
            &out_dir,
            args.filter.as_deref(),
            args.concurrency,
            args.keep_workspace,
            sandbox_config.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error running skill '{}': {}", skill.name, e);
                return Ok(2);
            }
        };
        if report.rows.iter().any(|r| r.verdict != "pass") {
            runner_exit = 1;
        }
        skill_reports.push(report);
    }

    let finished_at = chrono::Utc::now();

    let summary = Summary {
        total_rows: skill_reports.iter().map(|s| s.rows.len() as u32).sum(),
        passed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict == "pass")
            .count() as u32,
        failed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict != "pass")
            .count() as u32,
        timed_out: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.timed_out)
            .count() as u32,
        total_agent_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.agent_tokens.input + r.agent_tokens.output) as u64)
            .sum(),
        total_judge_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.judge_tokens.input + r.judge_tokens.output) as u64)
            .sum(),
        wall_ms: (finished_at - started_at).num_milliseconds() as u64,
    };

    let report = Report {
        schema_version: 1,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        runner_version: format!("tengu {}", env!("CARGO_PKG_VERSION")),
        judge_model,
        concurrency: args.concurrency,
        skills: skill_reports,
        summary,
    };

    let report_path = out_dir.join("report.json");
    if let Err(e) = std::fs::write(&report_path, serde_json::to_string_pretty(&report)?) {
        eprintln!("Error: write {}: {}", report_path.display(), e);
        return Ok(2);
    }

    match args.format {
        OutputFormat::Table => print_table(&report),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }

    Ok(runner_exit)
}

fn should_colour() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn colour(s: &str, code: &str) -> String {
    if should_colour() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

fn print_table(report: &Report) {
    for skill in &report.skills {
        println!(
            "{}  ({} rows, {:.1}s)",
            colour(&skill.skill, "1"), // bold
            skill.rows.len(),
            skill.wall_ms as f64 / 1000.0
        );
        for r in &skill.rows {
            let (mark, code) = if r.verdict == "pass" {
                ("✓", "32")
            } else {
                ("✗", "31")
            };
            println!(
                "  {} {:32} {:4}  {}",
                colour(mark, code),
                r.id,
                r.verdict,
                r.rationale
            );
            if r.verdict != "pass" {
                println!("      → see {}", r.transcript_path.display());
            }
        }
        println!();
    }
    let summary_line = format!(
        "{}/{} passed ({} failed). Total wall: {:.1}s. Agent tokens: {}. Judge tokens: {}.",
        report.summary.passed,
        report.summary.total_rows,
        report.summary.failed,
        report.summary.wall_ms as f64 / 1000.0,
        report.summary.total_agent_tokens,
        report.summary.total_judge_tokens,
    );
    if report.summary.failed == 0 {
        println!("{}", colour(&summary_line, "32"));
    } else {
        println!("{}", colour(&summary_line, "31"));
    }
}

#[derive(Debug, Clone)]
pub struct PromptRow {
    pub id: String,
    pub prompt: String,
    pub expected: String,
    pub timeout_secs: u64,
    pub stubs: Vec<StubSpec>,
}

#[derive(Debug, Clone)]
pub struct StubSpec {
    pub tool: String,
    pub responses: Vec<serde_json::Value>,
}

/// Build a judge engine for eval + evolve scoring.
///
/// Called from both `eval_builder::run` and `skill_lifecycle::evolve::run_eval_and_read_metrics`.
/// Default model: `anthropic/claude-opus-4-7`, 64 k context window.
pub fn build_judge(
    model: Option<String>,
) -> Result<Arc<dyn crate::adapters::types::Engine>> {
    let judge_model = model.unwrap_or_else(|| "anthropic/claude-opus-4-7".to_string());
    let box_engine =
        crate::adapters::engine_builder::build_openrouter_engine(&judge_model, 64_000)?;
    Ok(Arc::from(box_engine))
}

pub fn derive_row_id(prompt: &str) -> String {
    let mut s: String = prompt
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let truncated: String = s.trim_matches('-').chars().take(64).collect();
    truncated.trim_end_matches('-').to_string()
}

pub fn parse_markdown_prompts(body: &str) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut lines = body.lines().peekable();
    let mut in_table = false;
    while let Some(line) = lines.next() {
        if !in_table {
            let lower = line.to_ascii_lowercase();
            if lower.contains("| prompt") && lower.contains("expected") {
                lines.next(); // skip the `|---|---|` separator
                in_table = true;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if !trimmed.starts_with('|') {
            break;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        if cells.len() < 2 {
            continue;
        }
        let prompt_cell = cells[0].trim().trim_matches('"').trim();
        let expected_cell = cells[1].trim();
        if prompt_cell.is_empty() {
            continue;
        }
        let id = derive_row_id(prompt_cell);
        if !seen.insert(id.clone()) {
            bail!("duplicate row id '{}' in markdown prompts", id);
        }
        rows.push(PromptRow {
            id,
            prompt: prompt_cell.to_string(),
            expected: expected_cell.to_string(),
            timeout_secs: default_timeout_secs(),
            stubs: Vec::new(),
        });
    }

    if !in_table {
        bail!("no prompts table found — expected '| Prompt | Expected behaviour |' header");
    }
    if rows.is_empty() {
        bail!("prompts table has zero rows");
    }
    Ok(rows)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlRow {
    id: String,
    prompt: String,
    expected: String,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    stubs: Vec<YamlStub>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlStub {
    tool: String,
    responses: Vec<serde_json::Value>,
}

fn default_timeout_secs() -> u64 {
    120
}

pub fn parse_yaml_prompts(body: &str) -> Result<Vec<PromptRow>> {
    let raw: Vec<YamlRow> = serde_yaml::from_str(body).context("yaml prompts parse failed")?;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::with_capacity(raw.len());
    for r in raw {
        if !seen.insert(r.id.clone()) {
            bail!("duplicate row id '{}' in yaml prompts", r.id);
        }
        rows.push(PromptRow {
            id: r.id,
            prompt: r.prompt,
            expected: r.expected,
            timeout_secs: r.timeout_secs,
            stubs: r
                .stubs
                .into_iter()
                .map(|s| StubSpec {
                    tool: s.tool,
                    responses: s.responses,
                })
                .collect(),
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTier {
    Managed,
    Workspace,
    Project,
}

impl SkillTier {
    pub fn label(self) -> &'static str {
        match self {
            SkillTier::Managed => "managed",
            SkillTier::Workspace => "workspace",
            SkillTier::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillUnderTest {
    pub name: String,
    pub tier: SkillTier,
    pub evals_dir: PathBuf,
    pub prompts_path: PathBuf,
    pub prompts_format: String, // "markdown" or "yaml"
    pub config_path: PathBuf,
    pub skill_md_path: PathBuf,
    pub skill_dir: PathBuf,
}

pub fn default_skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs_next::home_dir() {
        roots.push(home.join(".tengu").join("skills"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".tengu").join("skills"));
        roots.push(cwd.join("skills"));
    }
    roots
}

fn tier_for_root(root: &Path) -> SkillTier {
    let s = root.to_string_lossy();
    if s.contains("/.tengu/skills") {
        if let Some(home) = dirs_next::home_dir() {
            if root.starts_with(home.join(".tengu").join("skills")) {
                return SkillTier::Managed;
            }
        }
        return SkillTier::Workspace;
    }
    SkillTier::Project
}

pub fn discover_skills(
    filter: &[String],
    roots: &[PathBuf],
) -> anyhow::Result<Vec<SkillUnderTest>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new(); // dedup by name; first tier wins
    for root in roots {
        if !root.exists() {
            continue;
        }
        let tier = tier_for_root(root);
        for entry in
            std::fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !filter.is_empty() && !filter.iter().any(|s| s == &name) {
                continue;
            }
            let evals_dir = entry.path().join("evals");
            if !evals_dir.exists() {
                continue;
            }
            let yaml = evals_dir.join("prompts.yaml");
            let md = evals_dir.join("prompts.md");
            let (prompts_path, fmt) = if yaml.exists() {
                (yaml, "yaml")
            } else if md.exists() {
                (md, "markdown")
            } else {
                continue;
            };
            // Dedup happens last, once we know this directory is a real evaluable skill.
            if !seen.insert(name.clone()) {
                continue;
            }
            let config_path = evals_dir.join("config.toml");
            let skill_dir = entry.path().to_path_buf();
            let skill_md_path = skill_dir.join("SKILL.md");
            out.push(SkillUnderTest {
                name,
                tier,
                evals_dir: evals_dir.clone(),
                prompts_path,
                prompts_format: fmt.to_string(),
                config_path,
                skill_md_path,
                skill_dir,
            });
        }
    }
    if !filter.is_empty() {
        for wanted in filter {
            if !out.iter().any(|s| &s.name == wanted) {
                anyhow::bail!("skill '{}' has no evals/prompts.{{md,yaml}}", wanted);
            }
        }
    }
    Ok(out)
}

use crate::adapters::config::Config;

// ---------------------------------------------------------------------------
// Skill metrics frontmatter — load + validate on skill discovery
// ---------------------------------------------------------------------------

use crate::adapters::skill_lifecycle::metrics::{validate_metrics, MetricSpec};

#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    metrics: Vec<MetricSpec>,
}

pub(crate) fn load_skill_metrics(
    skill_md_path: &Path,
    skill_dir: &Path,
) -> anyhow::Result<Vec<MetricSpec>> {
    if !skill_md_path.exists() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("read {}", skill_md_path.display()))?;
    let Some(rest) = body.strip_prefix("---\n") else {
        return Ok(vec![]);
    };
    let Some(end) = rest.find("\n---") else {
        return Ok(vec![]);
    };
    let fm_yaml = &rest[..end];
    let fm: SkillFrontmatter = serde_yaml::from_str(fm_yaml)
        .with_context(|| format!("parse frontmatter of {}", skill_md_path.display()))?;
    if !fm.metrics.is_empty() {
        validate_metrics(&fm.metrics, skill_dir)?;
    }
    Ok(fm.metrics)
}

// ---------------------------------------------------------------------------
// StubbedExecutor — wraps any ToolExecutor with per-tool response queues
// ---------------------------------------------------------------------------

use crate::adapters::engine_builder::ToolExecutor;
use crate::adapters::types::ToolCall;
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct StubbedExecutor<'a> {
    inner: &'a dyn ToolExecutor,
    queues: Mutex<HashMap<String, VecDeque<serde_json::Value>>>,
}

impl<'a> StubbedExecutor<'a> {
    pub fn new(inner: &'a dyn ToolExecutor, stubs: &[StubSpec]) -> Self {
        let mut queues: HashMap<String, VecDeque<serde_json::Value>> = HashMap::new();
        for spec in stubs {
            queues
                .entry(spec.tool.clone())
                .or_default()
                .extend(spec.responses.iter().cloned());
        }
        Self {
            inner,
            queues: Mutex::new(queues),
        }
    }
}

#[async_trait]
impl<'a> ToolExecutor for StubbedExecutor<'a> {
    async fn execute(&self, call: &ToolCall) -> anyhow::Result<String> {
        {
            let mut guard = self.queues.lock().unwrap();
            if let Some(q) = guard.get_mut(&call.name) {
                let response = if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    // Last entry repeats forever once we stop popping.
                    q.front()
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!(null))
                };
                return Ok(serde_json::to_string(&response)?);
            }
        }
        self.inner.execute(call).await
    }
}

pub fn load_eval_config(path: &Path, tmp_workspace: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let expanded = raw.replace("{TMP_WORKSPACE}", &tmp_workspace.to_string_lossy());
    let cfg: Config =
        toml::from_str(&expanded).with_context(|| format!("parse {}", path.display()))?;

    for (name, agent) in &cfg.agents {
        if agent.engine == "claude_code" {
            anyhow::bail!(
                "agent '{}': claude_code engine not supported by eval runner in v1 \
                 (spec §3 non-goal). Switch to engine = \"openrouter\" or pass --sandbox \
                 with an openrouter config.",
                name
            );
        }
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Verdict + parse_verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct Verdict {
    pub verdict: String, // "pass" or "fail"
    pub rationale: String,
}

pub fn parse_verdict(raw: &str) -> anyhow::Result<Verdict> {
    #[derive(serde::Deserialize)]
    struct Raw {
        verdict: String,
        rationale: String,
    }
    fn try_parse(s: &str) -> Option<Verdict> {
        let r: Raw = serde_json::from_str(s).ok()?;
        if r.verdict == "pass" || r.verdict == "fail" {
            Some(Verdict {
                verdict: r.verdict,
                rationale: r.rationale,
            })
        } else {
            None
        }
    }

    // 1. Try the raw string as-is.
    let trimmed = raw.trim();
    if let Some(v) = try_parse(trimmed) {
        return Ok(v);
    }

    // 2. Strip markdown code fences (```json ... ``` or ``` ... ```).
    let stripped = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Some(v) = try_parse(stripped) {
        return Ok(v);
    }

    // 3. Extract the first balanced `{...}` substring and parse that.
    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
        if start < end {
            if let Some(v) = try_parse(&stripped[start..=end]) {
                return Ok(v);
            }
        }
    }

    // 4. Give up. Diagnostic fail with preview of the original output.
    let preview: String = raw.chars().take(200).collect();
    Ok(Verdict {
        verdict: "fail".into(),
        rationale: format!("judge emitted malformed output: {}", preview),
    })
}

// ---------------------------------------------------------------------------
// Observation + judge prompt helpers + judge_row
// ---------------------------------------------------------------------------

use crate::adapters::types::{Engine, EngineContext, Message, Role, StreamEvent};
use futures::StreamExt;

const JUDGE_SYSTEM_PROMPT: &str = r#"You are evaluating whether an AI agent's tool-call sequence matches an expected behaviour.

Input shape:
- An "Expected behaviour" description in natural language.
- An ordered list of the agent's observed tool calls (name + truncated args).
- The agent's final assistant text.

Decide: did the agent's behaviour match the expected behaviour?

Reply with strict JSON only. Use this exact shape:
{"verdict": "pass", "rationale": "<one short sentence>"}
or
{"verdict": "fail", "rationale": "<one short sentence>"}

Output only the JSON object. No prose, no markdown, no code fences, no preamble. `verdict` must be exactly "pass" or "fail" — nothing else."#;

#[derive(Debug, Clone)]
pub struct Observation {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}

pub fn format_judge_user_turn(
    expected: &str,
    observations: &[Observation],
    final_text: &str,
) -> String {
    let mut s = String::new();
    s.push_str("Expected behaviour: ");
    s.push_str(expected);
    s.push_str("\n\nObserved tool calls (in order):\n");
    if observations.is_empty() {
        s.push_str("(none)\n");
    } else {
        for obs in observations {
            s.push_str(&format!(
                "{}. {}({})\n",
                obs.seq, obs.name, obs.args_preview
            ));
        }
    }
    s.push_str("\nFinal assistant text:\n");
    let text_preview: String = final_text.chars().take(1024).collect();
    s.push_str(&text_preview);
    s.push_str("\n\nDid the agent's behaviour match the expected? Reply with JSON only.");
    s
}

#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub verdict: Verdict,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub async fn judge_row(
    judge: &dyn Engine,
    expected: &str,
    observations: &[Observation],
    final_text: &str,
) -> anyhow::Result<JudgeOutcome> {
    // NOTE: We used to prefill an Assistant message with `{"verdict":` to force
    // JSON start, but OpenRouter's Anthropic provider rejects that ("The
    // conversation must end with a user message."). `parse_verdict` is now
    // robust to wrapped/fenced output, which removes the need for the prefill
    // at the cost of occasionally paying a few tokens for prose the model
    // emits before the JSON object.
    let messages = vec![
        Message {
            role: Role::System,
            content: JUDGE_SYSTEM_PROMPT.to_string(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: format_judge_user_turn(expected, observations, final_text),
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    let ctx = EngineContext {
        workspace: None,
        system_prompt: None,
        bridge_tools: None,
        max_tool_rounds: Some(1),
        max_mcp_result_chars: None,
    };
    let mut stream = judge.run(&messages, &[], &ctx).await?;
    let mut output = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta { text } => output.push_str(&text),
            StreamEvent::Done => break,
            StreamEvent::Error { message } => {
                anyhow::bail!("judge engine error: {}", message);
            }
            StreamEvent::Usage {
                input_tokens: it,
                output_tokens: ot,
            } => {
                input_tokens = input_tokens.saturating_add(it);
                output_tokens = output_tokens.saturating_add(ot);
            }
            _ => {}
        }
    }
    let verdict = parse_verdict(&output)?;
    Ok(JudgeOutcome {
        verdict,
        input_tokens,
        output_tokens,
    })
}

// ---------------------------------------------------------------------------
// Per-row driver
// ---------------------------------------------------------------------------

use crate::adapters::channel_runtime;
use crate::adapters::config::AgentConfig;
use crate::adapters::engine_builder::{collect_engine_response, ToolResultObserver};
use crate::adapters::ports::ToolActivityPort;
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::skill_builder::{FileSystemSkillSource, SkillRegistry};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenCount {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservationJson {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricOutcomeJson {
    pub metric: String,
    pub pass: bool,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RowResult {
    pub id: String,
    pub prompt: String,
    pub expected: String,
    pub verdict: String,
    pub rationale: String,
    pub observed_tools: Vec<ObservationJson>,
    pub wall_ms: u64,
    pub agent_tokens: TokenCount,
    pub judge_tokens: TokenCount,
    pub transcript_path: PathBuf,
    pub timed_out: bool,
    pub stubs_used: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_outcomes: Vec<MetricOutcomeJson>,
}

// ---------------------------------------------------------------------------
// EvalJudgeClient — adapts the eval judge engine to the JudgeClient trait
// ---------------------------------------------------------------------------

use crate::adapters::skill_lifecycle::metrics::JudgeClient;

struct EvalJudgeClient {
    engine: Arc<dyn Engine>,
}

#[async_trait]
impl JudgeClient for EvalJudgeClient {
    async fn judge(
        &self,
        system: &str,
        user: &str,
        prefill: &str,
        _model: Option<&str>,
    ) -> anyhow::Result<String> {
        // Build a single-turn completion, prefilling the assistant if a prefill is given.
        // OpenRouter/Anthropic reject a conversation ending with an Assistant message, so
        // we only add the prefill entry when it is non-empty.
        let mut messages = vec![
            Message {
                role: Role::System,
                content: system.to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::User,
                content: user.to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        if !prefill.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content: prefill.to_string(),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        let ctx = EngineContext {
            workspace: None,
            system_prompt: None,
            bridge_tools: None,
            max_tool_rounds: Some(1),
            max_mcp_result_chars: None,
        };
        let mut stream = self.engine.run(&messages, &[], &ctx).await?;
        let mut output = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                StreamEvent::TextDelta { text } => output.push_str(&text),
                StreamEvent::Done => break,
                StreamEvent::Error { message } => {
                    anyhow::bail!("EvalJudgeClient engine error: {}", message);
                }
                _ => {}
            }
        }
        // Strip prefill prefix from response if present.
        let result = if !prefill.is_empty() && output.starts_with(prefill) {
            output[prefill.len()..].to_string()
        } else {
            output
        };
        Ok(result)
    }
}

pub struct RowCtx<'a> {
    pub skill: &'a SkillUnderTest,
    pub row: &'a PromptRow,
    pub judge: Arc<dyn Engine>,
    pub out_dir: &'a Path,
    pub keep_workspace: bool,
    pub config_path_override: Option<&'a Path>,
    pub skill_metrics: &'a [MetricSpec],
    pub judge_client: Arc<dyn JudgeClient>,
}

// Minimal ToolActivityPort for eval runs — no UI, no logging.
struct NoopActivity;
impl ToolActivityPort for NoopActivity {
    fn publish_tool_activity(&self, _call: &ToolCall) {}
}

// Fallback executor when the agent has no workspace (which shouldn't happen in evals).
struct NoopRuntimeToolExecutor;
#[async_trait]
impl crate::adapters::engine_builder::ToolExecutor for NoopRuntimeToolExecutor {
    async fn execute(&self, _call: &ToolCall) -> anyhow::Result<String> {
        anyhow::bail!("no-op executor: tool calls are not enabled in this run")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn write_transcript(
    out_path: &Path,
    skill: &str,
    row: &PromptRow,
    agent_model: &str,
    engine_id: &str,
    workspace: &Path,
    messages: &[Message],
    tool_outcomes: &[(String, String)],
    observations: &[Observation],
    final_text: &str,
    verdict: &Verdict,
    judge_user_turn: &str,
) -> anyhow::Result<()> {
    use std::fmt::Write;
    let mut body = String::new();
    writeln!(body, "# {} / {}", skill, row.id)?;
    writeln!(body)?;
    writeln!(body, "## Config")?;
    writeln!(
        body,
        "engine={}  model={}  workspace={}",
        engine_id,
        agent_model,
        workspace.display()
    )?;
    writeln!(body)?;
    writeln!(body, "## User prompt")?;
    writeln!(body, "{}", row.prompt)?;
    writeln!(body)?;
    writeln!(body, "## Message log")?;
    for (i, m) in messages.iter().enumerate() {
        writeln!(body, "[turn {} — {:?}]", i + 1, m.role)?;
        writeln!(body, "{}", m.content)?;
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                writeln!(
                    body,
                    "→ tool call: {}({})",
                    c.name,
                    truncate(&c.arguments.to_string(), 2048)
                )?;
            }
        }
    }
    for (name, out) in tool_outcomes {
        writeln!(body, "← tool result ({}): {}", name, truncate(out, 2048))?;
    }
    writeln!(body)?;
    writeln!(body, "## Observations (judge input)")?;
    for o in observations {
        writeln!(body, "{}. {}({})", o.seq, o.name, o.args_preview)?;
    }
    writeln!(body)?;
    writeln!(body, "## Final assistant text")?;
    writeln!(body, "{}", final_text)?;
    writeln!(body)?;
    writeln!(body, "## Judge")?;
    writeln!(body, "### Prompt")?;
    writeln!(body, "{}", judge_user_turn)?;
    writeln!(body, "### Verdict")?;
    writeln!(body, "{}: {}", verdict.verdict, verdict.rationale)?;
    std::fs::write(out_path, body)
        .with_context(|| format!("write transcript {}", out_path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillReport {
    pub skill: String,
    pub tier: String,
    pub config_source: String, // "skill-local" | "sandbox" | "override"
    pub engine: String,
    pub agent_model: String,
    pub prompts_format: String,
    pub wall_ms: u64,
    pub rows: Vec<RowResult>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
    pub total_rows: u32,
    pub passed: u32,
    pub failed: u32,
    pub timed_out: u32,
    pub total_agent_tokens: u64,
    pub total_judge_tokens: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub started_at: String,
    pub finished_at: String,
    pub runner_version: String,
    pub judge_model: String,
    pub concurrency: usize,
    pub skills: Vec<SkillReport>,
    pub summary: Summary,
}

// ---------------------------------------------------------------------------
// Skill-level driver
// ---------------------------------------------------------------------------

pub async fn run_skill(
    skill: &SkillUnderTest,
    judge: Arc<dyn crate::adapters::types::Engine>,
    out_dir: &Path,
    filter: Option<&str>,
    concurrency: usize,
    keep_workspace: bool,
    sandbox_config: Option<&Path>,
) -> anyhow::Result<SkillReport> {
    let skill_started = Instant::now();
    let skill_started_ts = chrono::Utc::now();
    let prompts_body = std::fs::read_to_string(&skill.prompts_path)
        .with_context(|| format!("read {}", skill.prompts_path.display()))?;
    let mut rows = if skill.prompts_format == "yaml" {
        parse_yaml_prompts(&prompts_body)?
    } else {
        parse_markdown_prompts(&prompts_body)?
    };
    if let Some(pattern) = filter {
        let glob = glob::Pattern::new(pattern).context("invalid --filter glob")?;
        rows.retain(|r| glob.matches(&r.id));
    }

    // Load config once to grab agent metadata (engine, model) for report header.
    let dummy_ws = std::env::temp_dir();
    let probe_path = sandbox_config.unwrap_or(&skill.config_path);
    let cfg_probe = load_eval_config(probe_path, &dummy_ws)?;
    let agent = cfg_probe
        .agents
        .values()
        .find(|a| a.default)
        .or_else(|| cfg_probe.agents.values().next())
        .ok_or_else(|| anyhow::anyhow!("eval config has no agent"))?;
    let engine_id = agent.engine.clone();
    let agent_model = agent.model.clone();

    if concurrency > 1 {
        anyhow::bail!("concurrency > 1 not yet implemented in v1 — use --concurrency 1");
    }

    // Load skill metrics from SKILL.md frontmatter (empty vec → backward compat).
    let skill_metrics = load_skill_metrics(&skill.skill_md_path, &skill.skill_dir)?;
    // Build a shared JudgeClient adapter wrapping the eval judge Arc.
    let judge_client: Arc<dyn JudgeClient> =
        Arc::new(EvalJudgeClient { engine: Arc::clone(&judge) });

    let mut row_results = Vec::new();
    for row in &rows {
        eprintln!("[{} row {}] running…", skill.name, row.id);
        let rr = run_row(RowCtx {
            skill,
            row,
            judge: Arc::clone(&judge),
            out_dir,
            keep_workspace,
            config_path_override: sandbox_config,
            skill_metrics: skill_metrics.as_slice(),
            judge_client: Arc::clone(&judge_client),
        })
        .await?;
        row_results.push(rr);
    }

    // Write metrics.json + history.jsonl if the skill declares any metrics.
    if !skill_metrics.is_empty() {
        use crate::adapters::skill_lifecycle::storage::{finalize_run, RunSample};
        let ts = skill_started_ts.format("%Y-%m-%dT%H-%M-%SZ").to_string();
        let samples: Vec<RunSample> = row_results
            .iter()
            .map(|r| {
                let mut outcomes = std::collections::BTreeMap::new();
                for mo in &r.metric_outcomes {
                    outcomes.insert(
                        mo.metric.clone(),
                        crate::adapters::skill_lifecycle::metrics::MetricOutcome {
                            pass: mo.pass,
                            score: mo.score,
                            notes: mo.notes.clone(),
                            raw: serde_json::json!({}),
                        },
                    );
                }
                RunSample {
                    fixture_id: r.id.clone(),
                    outcomes,
                }
            })
            .collect();
        let rolling_window = 10u32; // TODO: read from [skill_lifecycle] config if present.
        finalize_run(
            &skill.skill_dir,
            &skill.name,
            &ts,
            &skill_metrics,
            &samples,
            rolling_window,
        )
        .with_context(|| format!("finalize_run for {}", skill.name))?;
    }

    let config_source = if sandbox_config.is_some() {
        "sandbox".to_string()
    } else {
        "skill-local".to_string()
    };

    Ok(SkillReport {
        skill: skill.name.clone(),
        tier: skill.tier.label().to_string(),
        config_source,
        engine: engine_id,
        agent_model,
        prompts_format: skill.prompts_format.clone(),
        wall_ms: skill_started.elapsed().as_millis() as u64,
        rows: row_results,
    })
}

/// Run a single eval row through the agent + judge pipeline.
pub async fn run_row(ctx: RowCtx<'_>) -> anyhow::Result<RowResult> {
    let started = Instant::now();

    // 1. Fresh tmp workspace per row.
    let ws = tempfile::tempdir_in(std::env::temp_dir()).context("create per-row tmp workspace")?;
    let ws_path = ws.path().to_path_buf();

    // 2. Load the eval config with this workspace substituted.
    let config_path = ctx.config_path_override.unwrap_or(&ctx.skill.config_path);
    let cfg = load_eval_config(config_path, &ws_path)?;
    let agent: &AgentConfig = cfg
        .agents
        .values()
        .find(|a| a.default)
        .or_else(|| cfg.agents.values().next())
        .ok_or_else(|| anyhow::anyhow!("eval config has no agent defined"))?;

    // 3. Build the engine.
    let agent_id = cfg
        .agents
        .iter()
        .find(|(_, v)| std::ptr::eq(*v, agent))
        .map(|(k, _)| k.as_str())
        .unwrap_or("default");
    let engine_box =
        crate::adapters::engine_builder::build_engine(agent_id, agent, cfg.claude_code.as_ref())?;
    let engine: Arc<dyn Engine> = Arc::from(engine_box);

    // 4. Build tool executor using the orchestrator.rs pattern.
    let workspace_path: PathBuf = agent
        .workspace
        .as_ref()
        .cloned()
        .unwrap_or_else(|| ws_path.clone());

    let secret_registry = Arc::new(SecretRegistry::new());
    let log_activity: Arc<dyn ToolActivityPort> = Arc::new(NoopActivity);

    let base_tools = channel_runtime::compute_base_tools(
        true,
        false, // no memory in v1 eval runs
        &agent.workspace_tools,
    );

    let skill_source = FileSystemSkillSource::new(workspace_path.clone());
    let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
    let mut skill_registry =
        SkillRegistry::new(base_reserved).with_allowlist(Some(agent.skill_packages.clone()));
    skill_registry.reload(&skill_source);

    let current_tools = channel_runtime::rebuild_tools(&base_tools, &skill_registry);
    let system_prompt =
        channel_runtime::rebuild_system_prompt(agent, true, &skill_registry, &current_tools);

    let mut tool_defs = current_tools.clone();
    let inner_executor: Arc<dyn crate::adapters::engine_builder::ToolExecutor> =
        match channel_runtime::build_tool_executor(
            &workspace_path,
            &current_tools,
            &skill_registry,
            &None, // memory_manager — evals run without memory in v1
            &secret_registry,
            log_activity,
            None, // cancel
            None, // shared_http_client
            Some(&cfg.memory),
            agent,
            &cfg.mcp_servers,
        ) {
            Some(executor) => {
                let extra = executor.additional_tool_defs(&tool_defs);
                if !extra.is_empty() {
                    tool_defs.extend(extra);
                }
                Arc::new(executor) as Arc<dyn crate::adapters::engine_builder::ToolExecutor>
            }
            None => Arc::new(NoopRuntimeToolExecutor)
                as Arc<dyn crate::adapters::engine_builder::ToolExecutor>,
        };

    // 5. Wrap in StubbedExecutor for this row.
    let stubbed = StubbedExecutor::new(&*inner_executor, &ctx.row.stubs);

    // 6. Observation tap.
    let observations: Arc<std::sync::Mutex<Vec<Observation>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let observations_cloned = observations.clone();
    let seq = Arc::new(AtomicU32::new(0));
    let seq_cloned = seq.clone();
    let observer_closure: Box<dyn Fn(&ToolCall, &str) + Send + Sync> =
        Box::new(move |tc: &ToolCall, _result: &str| {
            let n = seq_cloned.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            observations_cloned.lock().unwrap().push(Observation {
                seq: n,
                name: tc.name.clone(),
                args_preview: truncate(&tc.arguments.to_string(), 2048),
            });
        });
    let observer: ToolResultObserver<'_> = &*observer_closure;

    // 7. Build user message and drive collect_engine_response.
    let messages = vec![
        Message {
            role: Role::System,
            content: system_prompt.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: ctx.row.prompt.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    let engine_context = EngineContext {
        workspace: Some(workspace_path.clone()),
        system_prompt: Some(system_prompt.clone()),
        bridge_tools: None,
        max_tool_rounds: Some(agent.limits.max_tool_rounds),
        max_mcp_result_chars: Some(agent.limits.max_mcp_result_chars),
    };

    let driver_fut = collect_engine_response(
        &*engine,
        &messages,
        &tool_defs,
        &engine_context,
        Some(&stubbed),
        Some(observer),
        None, // cancel
        None, // token_budget
        agent.limits.max_tool_rounds,
        agent.limits.max_tool_result_chars,
        agent.limits.stream_event_timeout_secs,
        agent.limits.compact_result_limit,
    );

    let (timed_out, engine_response) = match tokio::time::timeout(
        std::time::Duration::from_secs(ctx.row.timeout_secs),
        driver_fut,
    )
    .await
    {
        Ok(Ok(resp)) => (false, resp),
        Ok(Err(e)) => return Err(e),
        Err(_) => (
            true,
            crate::adapters::engine_builder::EngineResponse {
                text: String::new(),
                input_tokens_delta: 0,
                output_tokens_delta: 0,
                tool_outcomes: vec![],
            },
        ),
    };

    // 8. Judge.
    let obs_snapshot = observations.lock().unwrap().clone();
    let judge_user_turn =
        format_judge_user_turn(&ctx.row.expected, &obs_snapshot, &engine_response.text);
    let (verdict, judge_input_tokens, judge_output_tokens) = if timed_out {
        (
            Verdict {
                verdict: "fail".into(),
                rationale: format!("row timed out after {}s", ctx.row.timeout_secs),
            },
            0u32,
            0u32,
        )
    } else {
        let outcome = judge_row(
            &*ctx.judge,
            &ctx.row.expected,
            &obs_snapshot,
            &engine_response.text,
        )
        .await?;
        (outcome.verdict, outcome.input_tokens, outcome.output_tokens)
    };

    // 9. Transcript.
    std::fs::create_dir_all(ctx.out_dir)
        .with_context(|| format!("create out_dir {}", ctx.out_dir.display()))?;
    let transcript_path = ctx
        .out_dir
        .join(format!("{}-{}.md", ctx.skill.name, ctx.row.id));
    write_transcript(
        &transcript_path,
        &ctx.skill.name,
        ctx.row,
        &agent.model,
        engine.id(),
        &ws_path,
        &messages,
        &engine_response.tool_outcomes,
        &obs_snapshot,
        &engine_response.text,
        &verdict,
        &judge_user_turn,
    )?;

    if ctx.keep_workspace {
        std::mem::forget(ws); // leak TempDir guard — workspace persists on disk
    }

    // 10. Score against per-skill metric kinds (if any declared in SKILL.md frontmatter).
    let mut metric_outcomes: Vec<MetricOutcomeJson> = Vec::new();
    if !ctx.skill_metrics.is_empty() {
        use crate::adapters::skill_lifecycle::metric_kinds::{
            LlmJudgeKind, ScriptKind, ShellCheckKind, ToolAssertionKind,
        };
        use crate::adapters::skill_lifecycle::metrics::{
            FixtureContext, MetricKind, MetricOutcome, MetricRunCtx,
        };

        let shell = crate::adapters::shell_executor::LocalShellExecutor::new();
        let fixture = FixtureContext {
            prompt: &ctx.row.prompt,
            expected_outcome: Some(ctx.row.expected.as_str()),
            transcript: &engine_response.text,
        };
        let run_ctx = MetricRunCtx {
            skill_dir: &ctx.skill.skill_dir,
            workspace: &ws_path,
            shell: &shell,
            tools: None,
            judge: Some(Arc::clone(&ctx.judge_client)),
        };
        for spec in ctx.skill_metrics {
            let outcome: anyhow::Result<MetricOutcome> = match spec {
                MetricSpec::ShellCheck { .. } => ShellCheckKind.run(spec, &fixture, &run_ctx).await,
                MetricSpec::LlmJudge { .. } => LlmJudgeKind.run(spec, &fixture, &run_ctx).await,
                MetricSpec::ToolAssertion { .. } => {
                    ToolAssertionKind.run(spec, &fixture, &run_ctx).await
                }
                MetricSpec::Script { .. } => ScriptKind.run(spec, &fixture, &run_ctx).await,
            };
            let o = outcome.unwrap_or_else(|e| MetricOutcome {
                pass: false,
                score: 0.0,
                notes: Some(format!("metric runner error: {e}")),
                raw: serde_json::json!({}),
            });
            metric_outcomes.push(MetricOutcomeJson {
                metric: spec.name().to_string(),
                pass: o.pass,
                score: o.score,
                notes: o.notes,
            });
        }
    }

    Ok(RowResult {
        id: ctx.row.id.clone(),
        prompt: ctx.row.prompt.clone(),
        expected: ctx.row.expected.clone(),
        verdict: verdict.verdict,
        rationale: verdict.rationale,
        observed_tools: obs_snapshot
            .into_iter()
            .map(|o| ObservationJson {
                seq: o.seq,
                name: o.name,
                args_preview: o.args_preview,
            })
            .collect(),
        wall_ms: started.elapsed().as_millis() as u64,
        agent_tokens: TokenCount {
            input: engine_response.input_tokens_delta,
            output: engine_response.output_tokens_delta,
        },
        judge_tokens: TokenCount {
            input: judge_input_tokens,
            output: judge_output_tokens,
        },
        transcript_path,
        timed_out,
        stubs_used: !ctx.row.stubs.is_empty(),
        metric_outcomes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // Shared test helpers (used by stubbed_executor_* tests)
    // ---------------------------------------------------------------------------

    use crate::adapters::engine_builder::ToolExecutor;
    use crate::adapters::types::ToolCall;
    use async_trait::async_trait;

    struct CountingExecutor {
        counter: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ToolExecutor for CountingExecutor {
        async fn execute(&self, call: &ToolCall) -> anyhow::Result<String> {
            self.counter.lock().unwrap().push(call.name.clone());
            Ok(format!("live-result-for-{}", call.name))
        }
    }

    fn make_call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("id-{}", name),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn markdown_parses_well_formed_table() {
        let body = r#"# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
"#;
        let rows = parse_markdown_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].prompt,
            "research paper X then mint it as an IP token"
        );
        assert_eq!(
            rows[0].expected,
            "Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`."
        );
        assert_eq!(rows[0].id, "research-paper-x-then-mint-it-as-an-ip-token");
        assert_eq!(rows[1].id, "what-s-2-2");
        assert_eq!(rows[0].timeout_secs, 120);
        assert!(rows[0].stubs.is_empty());
    }

    #[test]
    fn markdown_rejects_duplicate_row_ids() {
        let body = r#"| Prompt | Expected behaviour |
|---|---|
| "hello" | do nothing |
| "hello" | do something else |
"#;
        let err = parse_markdown_prompts(body).unwrap_err();
        assert!(err.to_string().contains("duplicate row id"), "got: {}", err);
    }

    #[test]
    fn markdown_requires_header_row() {
        let body = "# just a heading, no table\n\nno rows here";
        let err = parse_markdown_prompts(body).unwrap_err();
        assert!(err.to_string().contains("no prompts table"), "got: {}", err);
    }

    #[test]
    fn row_id_kebabs_truncates_collapses_dashes() {
        assert_eq!(
            derive_row_id("Research paper X, then mint it!"),
            "research-paper-x-then-mint-it"
        );
        assert_eq!(derive_row_id(""), "");
        let long = "a".repeat(80);
        assert_eq!(derive_row_id(&long).len(), 64);
    }

    #[test]
    fn row_id_trims_trailing_dash_after_truncation() {
        // 63 alphanumerics + one separator → without the fix, this would truncate to
        // 64 chars ending in `-`. With the fix, the trailing dash is stripped.
        let prompt = format!("{}!suffix", "a".repeat(63));
        let id = derive_row_id(&prompt);
        assert!(
            !id.ends_with('-'),
            "id should not end with dash, got: {:?}",
            id
        );
        assert_eq!(
            id.len(),
            63,
            "id length after trailing-dash trim should be 63, got {}",
            id.len()
        );
    }

    #[test]
    fn yaml_parses_well_formed() {
        let body = r#"
- id: seq-research-mint
  prompt: "research paper X then mint it as an IP token"
  expected: "Sequential sessions_spawn(researcher) then sessions_spawn(minter)."
- id: fail-503-retry
  prompt: "my trade failed with HTTP 503"
  expected: "Retry the same call. No decomposition."
  timeout_secs: 60
  stubs:
    - tool: http_request
      responses:
        - { status: 503, body: "Service Unavailable" }
        - { status: 200, body: "{\"ok\": true}" }
"#;
        let rows = parse_yaml_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "seq-research-mint");
        assert_eq!(rows[0].timeout_secs, 120);
        assert_eq!(rows[1].timeout_secs, 60);
        assert_eq!(rows[1].stubs.len(), 1);
        assert_eq!(rows[1].stubs[0].tool, "http_request");
        assert_eq!(rows[1].stubs[0].responses.len(), 2);
    }

    #[test]
    fn yaml_rejects_unknown_keys() {
        let body = r#"
- id: x
  prompt: "hello"
  expected: "ok"
  oops_unknown_field: true
"#;
        let err = parse_yaml_prompts(body).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("unknown field"), "got: {}", msg);
    }

    #[test]
    fn yaml_rejects_duplicate_ids() {
        let body = r#"
- id: same
  prompt: "a"
  expected: "a"
- id: same
  prompt: "b"
  expected: "b"
"#;
        let err = parse_yaml_prompts(body).unwrap_err();
        assert!(err.to_string().contains("duplicate row id"), "got: {}", err);
    }

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
        std::fs::write(
            skill_dir.join("config.toml"),
            "runtime_profile = \"cloud\"\n",
        )
        .unwrap();

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
        // When a path contains .tengu/skills but doesn't start with $HOME,
        // it MUST be Workspace, never Project. This guards against a silent
        // misclassification if home_dir() ever returns None.
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
runtime_profile = "cloud"

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

    #[test]
    fn eval_config_rejects_claude_code_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[agents.main]
engine = "claude_code"
model = "sonnet"
default = true
workspace = "{TMP_WORKSPACE}"
"#,
        )
        .unwrap();

        let err = load_eval_config(&config_path, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("claude_code engine not supported"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn stubbed_executor_consumes_queue_then_repeats_last() {
        let inner = CountingExecutor {
            counter: std::sync::Mutex::new(Vec::new()),
        };
        let stubs = vec![StubSpec {
            tool: "http_request".into(),
            responses: vec![
                serde_json::json!({"status": 503}),
                serde_json::json!({"status": 200}),
            ],
        }];
        let stubbed = StubbedExecutor::new(&inner, &stubs);

        let r1 = stubbed.execute(&make_call("http_request")).await.unwrap();
        let r2 = stubbed.execute(&make_call("http_request")).await.unwrap();
        let r3 = stubbed.execute(&make_call("http_request")).await.unwrap();

        assert!(r1.contains("503"));
        assert!(r2.contains("200"));
        assert!(r3.contains("200")); // last entry repeats
        assert!(
            inner.counter.lock().unwrap().is_empty(),
            "stubbed calls should not reach inner executor"
        );
    }

    #[tokio::test]
    async fn stubbed_executor_delegates_unstubbed_tools() {
        let inner = CountingExecutor {
            counter: std::sync::Mutex::new(Vec::new()),
        };
        let stubs: Vec<StubSpec> = vec![];
        let stubbed = StubbedExecutor::new(&inner, &stubs);

        let r = stubbed.execute(&make_call("sessions_spawn")).await.unwrap();
        assert_eq!(r, "live-result-for-sessions_spawn");
        assert_eq!(
            inner.counter.lock().unwrap().as_slice(),
            &["sessions_spawn"]
        );
    }

    #[test]
    fn verdict_parses_pass() {
        let v = parse_verdict(r#"{"verdict": "pass", "rationale": "all good"}"#).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.rationale, "all good");
    }

    #[test]
    fn verdict_parses_fail() {
        let v = parse_verdict(r#"{"verdict":"fail","rationale":"missed sessions_spawn"}"#).unwrap();
        assert_eq!(v.verdict, "fail");
    }

    #[test]
    fn verdict_malformed_produces_fail_with_diagnostic() {
        let v = parse_verdict("not json at all").unwrap();
        assert_eq!(v.verdict, "fail");
        assert!(v.rationale.contains("judge emitted malformed output"));
    }

    #[test]
    fn verdict_rejects_unknown_verdict_value() {
        let v = parse_verdict(r#"{"verdict":"maybe","rationale":"unsure"}"#).unwrap();
        assert_eq!(v.verdict, "fail");
        assert!(v.rationale.contains("judge emitted malformed output"));
    }

    #[test]
    fn verdict_extracts_json_from_code_fence() {
        let raw = "```json\n{\"verdict\":\"pass\",\"rationale\":\"all good\"}\n```";
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.rationale, "all good");
    }

    #[test]
    fn verdict_extracts_json_from_surrounding_prose() {
        let raw =
            "The verdict is: {\"verdict\":\"fail\",\"rationale\":\"missed step\"} as shown above.";
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.rationale, "missed step");
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
}
