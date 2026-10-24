//! The eval logic of `tengu eval` / `tengu skill evolve`, no IO: eval rows
//! (the `prompts.md` table, `prompts.yaml` rows or the fixtures file the
//! skill tools write), per-row tool stubs (`StubbedExecutor`), the LLM judge
//! (`judge_row`, `parse_verdict`, `EvalJudgeClient` for metric kinds), the
//! report types, the transcript text and the judge's view of orchestrator
//! events. The runner — files, engines, the agent loop, the orchestrator —
//! is `adapters/inbound/eval/`.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::application::skills::lifecycle::metrics::JudgeClient;
use crate::domain::message::{Message, Role, StreamEvent, ToolCall};
use crate::ports::engine::{Engine, EngineContext, ToolExecutor};

#[derive(Debug, Clone)]
pub(crate) struct PromptRow {
    pub id: String,
    pub prompt: String,
    pub expected: String,
    pub timeout_secs: u64,
    pub stubs: Vec<StubSpec>,
}

#[derive(Debug, Clone)]
pub(crate) struct StubSpec {
    pub tool: String,
    pub responses: Vec<serde_json::Value>,
}

pub(crate) fn derive_row_id(prompt: &str) -> String {
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

pub(crate) fn parse_markdown_prompts(body: &str) -> Result<Vec<PromptRow>> {
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

/// A fixture as an eval row: its expected outcome, plus the tools it expects
/// called, is what the judge checks.
fn fixture_row(f: crate::application::skills::lifecycle::fixtures::Fixture) -> YamlRow {
    let mut expected = f.expected_outcome.unwrap_or_default().trim().to_string();
    if !f.expected_tool_calls.is_empty() {
        let tools: Vec<&str> = f
            .expected_tool_calls
            .iter()
            .map(|c| c.tool.as_str())
            .collect();
        let calls = format!("Calls: {}.", tools.join(", "));
        expected = if expected.is_empty() {
            calls
        } else {
            format!("{expected} {calls}")
        };
    }
    YamlRow {
        id: f.id,
        prompt: f.prompt,
        expected,
        timeout_secs: default_timeout_secs(),
        stubs: Vec::new(),
    }
}

pub(crate) fn parse_yaml_prompts(body: &str) -> Result<Vec<PromptRow>> {
    // Either a plain list of rows, or the `{schema_version, fixtures}` file
    // that `skill_distill`, `tengu skill seed` and `manage_skill create` write
    // (`lifecycle::fixtures::FixturesFile`).
    let is_fixtures_file = serde_yaml::from_str::<serde_yaml::Value>(body)
        .ok()
        .and_then(|v| v.as_mapping().map(|m| m.contains_key("fixtures")))
        .unwrap_or(false);
    let raw: Vec<YamlRow> = if is_fixtures_file {
        let file: crate::application::skills::lifecycle::fixtures::FixturesFile =
            serde_yaml::from_str(body).context("yaml fixtures parse failed")?;
        file.fixtures.into_iter().map(fixture_row).collect()
    } else {
        serde_yaml::from_str(body).context("yaml prompts parse failed")?
    };
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
pub(crate) enum SkillTier {
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
pub(crate) struct SkillUnderTest {
    pub name: String,
    pub tier: SkillTier,
    /// Resolved during discovery for completeness; the eval runner navigates
    /// from `skill_dir` rather than this directly. Kept on the struct so
    /// downstream tools (skill-distill, telemetry) can reach the eval bundle.
    #[allow(dead_code)]
    pub evals_dir: PathBuf,
    pub prompts_path: PathBuf,
    pub prompts_format: String, // "markdown" or "yaml"
    pub config_path: PathBuf,
    pub skill_md_path: PathBuf,
    pub skill_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// StubbedExecutor — wraps any ToolExecutor with per-tool response queues
// ---------------------------------------------------------------------------

pub(crate) struct StubbedExecutor<'a> {
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

    /// The next stubbed response for `call`'s tool, serialised; `None` when
    /// the tool has no stub (the real executor runs it).
    fn stubbed(&self, call: &ToolCall) -> anyhow::Result<Option<String>> {
        let mut guard = self.queues.lock().unwrap();
        let Some(q) = guard.get_mut(&call.name) else {
            return Ok(None);
        };
        let response = if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            // Last entry repeats forever once we stop popping.
            q.front()
                .cloned()
                .unwrap_or_else(|| serde_json::json!(null))
        };
        Ok(Some(serde_json::to_string(&response)?))
    }
}

#[async_trait]
impl<'a> ToolExecutor for StubbedExecutor<'a> {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> anyhow::Result<String> {
        match self.stubbed(call)? {
            Some(text) => Ok(text),
            None => self.inner.execute(call, messages).await,
        }
    }

    /// Unstubbed tools keep their typed observation, so a local engine's
    /// compact rendering (`tool_loop::fit_tool_result`) works in eval too.
    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> anyhow::Result<crate::ports::tool::ToolOutput> {
        match self.stubbed(call)? {
            Some(text) => Ok(crate::ports::tool::ToolOutput::from(text)),
            None => self.inner.execute_typed(call, messages).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Verdict + parse_verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Verdict {
    pub verdict: String, // "pass" or "fail"
    pub rationale: String,
}

pub(crate) fn parse_verdict(raw: &str) -> anyhow::Result<Verdict> {
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

const JUDGE_SYSTEM_PROMPT: &str = r#"You are evaluating whether an AI agent's behaviour matches an expected behaviour.

Input shape:
- An "Expected behaviour" description in natural language.
- An ordered list of observations (name + truncated detail). Each observation is ONE of:
  * A tool call made by a worker: name is the tool (e.g. `http_request`, `write_file`).
  * An orchestrator event: name is prefixed `orchestrator:` — e.g.:
      - `orchestrator:plan_created` (detail lists step ids + agents + depends_on — this
        tells you the DAG shape: which steps are parallel, which are sequential,
        which is the synthesizer)
      - `orchestrator:step_started` / `step_succeeded` / `step_failed` / `step_exhausted`
      - `orchestrator:replan_triggered`
      - `orchestrator:plan_completed`
- The agent's final assistant text.

The orchestrator has NO tools — any tool call in the list was made by a WORKER step. Tool calls
belong to the step whose `orchestrator:step_started` event immediately precedes them.

Judge based on the `orchestrator:plan_created` event's DAG shape (parallel vs sequential,
single leaf, correct agents) AND the final text quality — not on whether http_request or
write_file appears (workers legitimately call those).

Decide: did the agent's behaviour match the expected behaviour?

Reply with strict JSON only. Use this exact shape:
{"verdict": "pass", "rationale": "<one short sentence>"}
or
{"verdict": "fail", "rationale": "<one short sentence>"}

Output only the JSON object. No prose, no markdown, no code fences, no preamble. `verdict` must be exactly "pass" or "fail" — nothing else."#;

#[derive(Debug, Clone)]
pub(crate) struct Observation {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}

pub(crate) fn format_judge_user_turn(
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
pub(crate) struct JudgeOutcome {
    pub verdict: Verdict,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub(crate) async fn judge_row(
    judge: &dyn Engine,
    expected: &str,
    observations: &[Observation],
    final_text: &str,
) -> anyhow::Result<JudgeOutcome> {
    // No assistant prefill (`{"verdict":`): OpenRouter's Anthropic provider
    // rejects it ("The conversation must end with a user message.").
    // `parse_verdict` takes wrapped / fenced output instead, at the cost of
    // a few tokens of prose the model may write before the JSON object.
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
        mcp_servers: Vec::new(),
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
                ..
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

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct TokenCount {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ObservationJson {
    pub seq: u32,
    pub name: String,
    pub args_preview: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct MetricOutcomeJson {
    pub metric: String,
    pub pass: bool,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RowResult {
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

pub(crate) struct EvalJudgeClient {
    pub(crate) engine: Arc<dyn Engine>,
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
            mcp_servers: Vec::new(),
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

/// `s` cut to `max` characters, `…` marking a cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// A row's transcript (Markdown): config, prompt, message log, the judge's
/// observations, the final text, the judge prompt and verdict.
pub(crate) fn render_transcript(
    skill: &str,
    row: &PromptRow,
    agent_model: &str,
    engine_id: &str,
    workspace: &std::path::Path,
    messages: &[Message],
    tool_outcomes: &[(String, String)],
    observations: &[Observation],
    final_text: &str,
    verdict: &Verdict,
    judge_user_turn: &str,
) -> anyhow::Result<String> {
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
    Ok(body)
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SkillReport {
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
pub(crate) struct Summary {
    pub total_rows: u32,
    pub passed: u32,
    pub failed: u32,
    pub timed_out: u32,
    pub total_agent_tokens: u64,
    pub total_judge_tokens: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Report {
    pub schema_version: u32,
    pub started_at: String,
    pub finished_at: String,
    pub runner_version: String,
    pub judge_model: String,
    pub concurrency: usize,
    pub skills: Vec<SkillReport>,
    pub summary: Summary,
}

/// Options controlling persistence side-effects of `run_skill`. Kept as a
/// struct so callers that don't care (tests, evolve's cycle re-eval) can
/// pass `RunSkillOptions::default()` without threading a growing positional
/// argument list.
#[derive(Debug, Clone)]
pub(crate) struct RunSkillOptions {
    /// When `false`, `run_skill` writes no per-row transcripts, no
    /// `report.json`, and does not update `metrics.json`/`history.jsonl`.
    /// The returned `SkillReport` is still populated for the in-memory
    /// caller (e.g. a CLI that renders a table).
    pub persist: bool,
    /// Retention cap for per-run detail directories under
    /// `skills/<name>/metrics/runs/`. `0` disables pruning. Honoured only
    /// when `persist` is `true`.
    pub max_per_run_reports: u32,
}

impl Default for RunSkillOptions {
    fn default() -> Self {
        Self {
            persist: true,
            max_per_run_reports: 10,
        }
    }
}

/// One `OrchestratorEvent` as the judge's observation `(name, detail)`.
pub(crate) fn event_observation(
    ev: crate::application::orchestrator::OrchestratorEvent,
) -> (String, String) {
    use crate::application::orchestrator::OrchestratorEvent;
    match ev {
        OrchestratorEvent::PlanCreated { plan } => {
            let steps: Vec<String> = plan
                .steps
                .iter()
                .map(|s| {
                    let deps: Vec<String> = s.depends_on.iter().map(|d| d.0.clone()).collect();
                    if deps.is_empty() {
                        format!("{}:{}(deps=[])", s.id.0, s.agent)
                    } else {
                        format!("{}:{}(deps=[{}])", s.id.0, s.agent, deps.join(","))
                    }
                })
                .collect();
            (
                "orchestrator:plan_created".to_string(),
                format!("steps=[{}]", steps.join(", ")),
            )
        }
        OrchestratorEvent::StepStarted { step_id, agent } => (
            "orchestrator:step_started".to_string(),
            format!("{}:{}", step_id.0, agent),
        ),
        OrchestratorEvent::StepSucceeded { step_id, output } => (
            "orchestrator:step_succeeded".to_string(),
            format!("{}:{}", step_id.0, truncate(&output, 200)),
        ),
        OrchestratorEvent::StepFailed {
            step_id,
            attempt,
            error,
            ..
        } => (
            "orchestrator:step_failed".to_string(),
            format!(
                "{}:attempt={} err={}",
                step_id.0,
                attempt,
                truncate(&error, 200)
            ),
        ),
        OrchestratorEvent::StepExhausted {
            step_id,
            final_error,
        } => (
            "orchestrator:step_exhausted".to_string(),
            format!("{}:err={}", step_id.0, truncate(&final_error, 200)),
        ),
        OrchestratorEvent::ReplanTriggered { reason } => (
            "orchestrator:replan_triggered".to_string(),
            truncate(&reason, 200),
        ),
        OrchestratorEvent::PlanCompleted {
            final_response,
            cancelled,
            ..
        } => (
            "orchestrator:plan_completed".to_string(),
            format!("cancelled={} final_len={}", cancelled, final_response.len()),
        ),
        // Metrics — record one observation per LLM/embedding call so
        // eval rows can compare token consumption across runs. The
        // canonical analysis surface is the tracing log + global
        // metrics bus; this is the eval-judge mirror.
        OrchestratorEvent::MetricsRecorded { record } => (
            "orchestrator:metrics_recorded".to_string(),
            format!(
                "kind={} agent={} model={} tok in/out={}/{} latency_ms={}",
                record.kind.as_str(),
                record.agent,
                record.model,
                record.prompt_tokens,
                record.completion_tokens,
                record.latency_ms,
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // Shared test helpers (used by stubbed_executor_* tests)
    // ---------------------------------------------------------------------------

    struct CountingExecutor {
        counter: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ToolExecutor for CountingExecutor {
        async fn execute(
            &self,
            call: &ToolCall,
            _messages: &[crate::domain::message::Message],
        ) -> anyhow::Result<String> {
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
| "research paper X then mint it as an IP token" | Delegate to subagent via `memory_ingest` to store findings; then reply. |
| "what's 2+2?" | Direct answer. No `memory_ingest` call. |
"#;
        let rows = parse_markdown_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].prompt,
            "research paper X then mint it as an IP token"
        );
        assert_eq!(
            rows[0].expected,
            "Delegate to subagent via `memory_ingest` to store findings; then reply."
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
  expected: "Delegate to subagent via memory_ingest to store findings; then reply."
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

    /// The fixture file the skill tools write (`{schema_version, fixtures}`)
    /// is read too — eval used to refuse it (skills/german-teacher).
    #[test]
    fn yaml_reads_the_fixtures_file_the_skill_tools_write() {
        let body = r#"
schema_version: 1
fixtures:
- id: f1
  prompt: "conjugate sein"
  expected_tool_calls:
  - tool: view_skill
    args_schema: {}
  expected_outcome: "Lists ich bin, du bist."
  metrics: []
- id: f2
  prompt: "hi"
"#;
        let rows = parse_yaml_prompts(body).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].expected,
            "Lists ich bin, du bist. Calls: view_skill."
        );
        assert_eq!(rows[1].expected, "");
        assert_eq!(rows[0].timeout_secs, 120);
        parse_yaml_prompts(
            &std::fs::read_to_string("skills/german-teacher/evals/prompts.yaml").unwrap(),
        )
        .expect("the shipped german-teacher fixtures");
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

        let r1 = stubbed
            .execute(&make_call("http_request"), &[])
            .await
            .unwrap();
        let r2 = stubbed
            .execute(&make_call("http_request"), &[])
            .await
            .unwrap();
        let r3 = stubbed
            .execute(&make_call("http_request"), &[])
            .await
            .unwrap();

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

        let r = stubbed
            .execute(&make_call("memory_ingest"), &[])
            .await
            .unwrap();
        assert_eq!(r, "live-result-for-memory_ingest");
        assert_eq!(inner.counter.lock().unwrap().as_slice(), &["memory_ingest"]);
    }

    /// The typed path: an unstubbed tool's observation survives (local
    /// engines render it compactly); a stub is text only.
    #[tokio::test]
    async fn stubbed_executor_keeps_typed_results_of_unstubbed_tools() {
        struct Typed;
        #[async_trait]
        impl ToolExecutor for Typed {
            async fn execute(
                &self,
                call: &ToolCall,
                m: &[crate::domain::message::Message],
            ) -> anyhow::Result<String> {
                Ok(self.execute_typed(call, m).await?.text)
            }
            async fn execute_typed(
                &self,
                _call: &ToolCall,
                _m: &[crate::domain::message::Message],
            ) -> anyhow::Result<crate::ports::tool::ToolOutput> {
                let obs = crate::domain::observation::Observation {
                    key: "mkt_ctx/1:hyperliquid:xyz:TSLA".into(),
                    schema: "mkt_ctx/1".into(),
                    tool: "hl_ctx".into(),
                    observed_at_ms: 0,
                    slot: None,
                    ttl_ms: 15_000,
                    source: crate::domain::observation::ObsSource::Live,
                    status: crate::domain::observation::ObsStatus::Ok,
                    errors: vec![],
                    headline: "mkt hyperliquid:xyz:TSLA mark=347.19".into(),
                    features: Default::default(),
                    data: serde_json::json!({"levels": [1, 2, 3]}),
                };
                Ok(crate::ports::tool::ToolOutput::observed(obs, 0))
            }
        }
        let stubs = vec![StubSpec {
            tool: "http_request".into(),
            responses: vec![serde_json::json!({"status": 200})],
        }];
        let stubbed = StubbedExecutor::new(&Typed, &stubs);
        let live = stubbed
            .execute_typed(&make_call("hl_ctx"), &[])
            .await
            .unwrap();
        assert_eq!(
            live.observation.map(|o| o.key).as_deref(),
            Some("mkt_ctx/1:hyperliquid:xyz:TSLA")
        );
        let stub = stubbed
            .execute_typed(&make_call("http_request"), &[])
            .await
            .unwrap();
        assert!(stub.observation.is_none());
        assert!(stub.text.contains("200"));
    }

    #[test]
    fn verdict_parses_pass() {
        let v = parse_verdict(r#"{"verdict": "pass", "rationale": "all good"}"#).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.rationale, "all good");
    }

    #[test]
    fn verdict_parses_fail() {
        let v = parse_verdict(r#"{"verdict":"fail","rationale":"missed memory_ingest"}"#).unwrap();
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
}
