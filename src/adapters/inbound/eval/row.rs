//! One skill's eval rows: [`run_skill`] runs [`run_row`] per row — direct
//! dispatch (the default agent's tool loop, the row's stubs) or, with
//! `[orchestrator]`, `Orchestrator::handle` with the bus's events as the
//! judge's observations — then the judge and the skill's metric kinds.

use super::*;

use crate::adapters::outbound::noop::{NoopActivity, NoopRuntimeToolExecutor};
use crate::adapters::outbound::secrets::process_secret_registry;
use crate::application::chat::tool_loop::{collect_engine_response, LoopOpts, ToolResultObserver};
use crate::application::skills::lifecycle::metrics::JudgeClient;
use crate::application::skills::registry::{FileSystemSkillSource, SkillRegistry};
use crate::domain::message::{Message, Role, ToolCall};
use crate::ports::engine::EngineContext;
use crate::ports::tool_activity::ToolActivityPort;
use crate::ports::trace::TraceSink;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

pub(crate) struct RowCtx<'a> {
    pub skill: &'a SkillUnderTest,
    pub row: &'a PromptRow,
    pub judge: Arc<dyn Engine>,
    pub out_dir: &'a Path,
    pub keep_workspace: bool,
    pub config_path_override: Option<&'a Path>,
    pub skill_metrics: &'a [MetricSpec],
    pub judge_client: Arc<dyn JudgeClient>,
    /// When false, `run_row` does not write its per-row transcript. Other
    /// side effects (agent dispatch, judge call, metric scoring) still run —
    /// only the on-disk persistence is elided.
    pub persist_transcript: bool,
}

/// Writes [`render_transcript`] to `out_path`.
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
    let body = render_transcript(
        skill,
        row,
        agent_model,
        engine_id,
        workspace,
        messages,
        tool_outcomes,
        observations,
        final_text,
        verdict,
        judge_user_turn,
    )?;
    std::fs::write(out_path, body)
        .with_context(|| format!("write transcript {}", out_path.display()))
}

/// The `metrics.json` rolling window: `[skill_lifecycle] default_rolling_window`,
/// else 10 (its default).
fn rolling_window_of(cfg: &Config) -> u32 {
    cfg.skill_lifecycle
        .as_ref()
        .map_or(10, |s| s.default_rolling_window)
}

pub(crate) async fn run_skill(
    skill: &SkillUnderTest,
    judge: Arc<dyn crate::ports::engine::Engine>,
    out_dir: &Path,
    filter: Option<&str>,
    concurrency: usize,
    keep_workspace: bool,
    sandbox_config: Option<&Path>,
    options: RunSkillOptions,
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
    // `[skill_lifecycle] default_rolling_window` of the eval config (10 without one).
    let rolling_window = rolling_window_of(&cfg_probe);

    if concurrency > 1 {
        anyhow::bail!("concurrency > 1 not yet implemented in v1 — use --concurrency 1");
    }

    // Load skill metrics from SKILL.md frontmatter (empty vec → backward compat).
    let skill_metrics = load_skill_metrics(&skill.skill_md_path, &skill.skill_dir)?;
    // Build a shared JudgeClient adapter wrapping the eval judge Arc.
    let judge_client: Arc<dyn JudgeClient> = Arc::new(EvalJudgeClient {
        engine: Arc::clone(&judge),
    });

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
            persist_transcript: options.persist,
        })
        .await?;
        row_results.push(rr);
    }

    // Write metrics.json + history.jsonl if the skill declares any metrics.
    if !skill_metrics.is_empty() {
        use crate::application::skills::lifecycle::storage::{finalize_run, RunSample};
        let ts = skill_started_ts.format("%Y-%m-%dT%H-%M-%SZ").to_string();
        let samples: Vec<RunSample> = row_results
            .iter()
            .map(|r| {
                let mut outcomes = std::collections::BTreeMap::new();
                for mo in &r.metric_outcomes {
                    outcomes.insert(
                        mo.metric.clone(),
                        crate::application::skills::lifecycle::metrics::MetricOutcome {
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
        if options.persist {
            finalize_run(
                &skill.skill_dir,
                &skill.name,
                &ts,
                &skill_metrics,
                &samples,
                rolling_window,
                options.max_per_run_reports,
            )
            .with_context(|| format!("finalize_run for {}", skill.name))?;
        }
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
///
/// Two execution paths:
///
/// - **Direct dispatch** (default): the config's default agent receives the
///   prompt and drives the LLM tool loop directly via `collect_engine_response`.
///   This is the path the eval runner was originally written for (pre
///   PR #6), and it still runs for configs without an `[orchestrator]` block.
///
/// - **Orchestrator dispatch**: when `cfg.orchestrator.is_some()`, the row
///   goes through `Orchestrator::handle` — the planner plans, the DAG
///   executor runs each step as a `run-agent` child. The judge sees the
///   orchestrator's events (plan, steps, replans) as observations; tokens are
///   the row session's `metrics.recorded`. Per-row stubs do not reach the
///   children. This is the path the `skills/orchestration-e2e` evals rely on.
pub(crate) async fn run_row(ctx: RowCtx<'_>) -> anyhow::Result<RowResult> {
    let started = Instant::now();

    // 1. Fresh tmp workspace per row.
    let ws = tempfile::tempdir_in(std::env::temp_dir()).context("create per-row tmp workspace")?;
    let ws_path = ws.path().to_path_buf();

    // 2. Load the eval config with this workspace substituted (+ the file a
    //    `claude_code` agent's bridge loads).
    let config_path = ctx.config_path_override.unwrap_or(&ctx.skill.config_path);
    let cfg = load_eval_config(config_path, &ws_path)?;
    let agent: &AgentConfig = cfg
        .agents
        .values()
        .find(|a| a.default)
        .or_else(|| cfg.agents.values().next())
        .ok_or_else(|| anyhow::anyhow!("eval config has no agent defined"))?;
    let agent_id = cfg
        .agents
        .iter()
        .find(|(_, v)| std::ptr::eq(*v, agent))
        .map(|(k, _)| k.as_str())
        .unwrap_or("default");
    let runnable = runnable_agents(&cfg, (agent_id, agent));
    refuse_stubs_on_bridge(ctx.row, &runnable)?;
    let bridge_config = bridge_config_file(config_path, &ws_path, &runnable)?;

    // 2b. Branch: orchestrator vs direct dispatch.
    if cfg.orchestrator.is_some() {
        return run_row_via_orchestrator(ctx, started, ws, ws_path, cfg).await;
    }

    // 3. Build the engine.
    let engine: Arc<dyn Engine> = Arc::from(build_eval_engine(
        agent_id,
        agent,
        &cfg,
        bridge_config.as_ref().map(|f| f.path()),
    )?);

    // 4. Build tool executor using the orchestrator.rs pattern.
    // Tilde expansion required — sandbox configs commonly write
    // `workspace = "~/foo"`. Without expand_tilde the literal `~` is
    // joined into the cache plugin's `<workspace>/.tengu/cache.db` path
    // and pollutes the repo with a `~` directory. Matches the
    // inbound/telegram.rs / inbound/webhooks.rs convention.
    let workspace_path: PathBuf = agent
        .workspace
        .as_ref()
        .map(|p| crate::config::paths::expand_tilde(p))
        .unwrap_or_else(|| ws_path.clone());

    // The process registry (vault names, `.env` credentials, never prompts):
    // tool output reaches the model and the judge redacted, as on every
    // surface.
    let secret_registry = Arc::new(process_secret_registry(None));
    let log_activity: Arc<dyn ToolActivityPort> = Arc::new(NoopActivity);

    // The agent's `tools` list, as on every surface.
    let base_tools = crate::bootstrap::tools::agent_base_tools(
        agent, true, false, // no memory in v1 eval runs
    );

    let skill_source = FileSystemSkillSource::new(workspace_path.clone());
    let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
    let mut skill_registry = SkillRegistry::new(base_reserved)
        .with_allowlist(Some(agent.skill_packages.clone()))
        .with_shell_skills(!agent.no_shell_fallback);
    skill_registry.reload(&skill_source);

    let current_tools = crate::bootstrap::tools::rebuild_tools(&base_tools, &skill_registry);
    let system_prompt = crate::bootstrap::tools::rebuild_system_prompt(agent, &skill_registry);

    let mut tool_defs = current_tools.clone();
    // The agent's own executor, kept for `tool_assertion` metrics (its
    // registry, HTTP client, scopes): without it they always fail.
    let mut plugin_executor: Option<Arc<crate::application::tools::registry::PluginToolExecutor>> =
        None;
    let inner_executor: Arc<dyn crate::ports::engine::ToolExecutor> =
        match crate::bootstrap::tools::build_tool_executor(
            &workspace_path,
            &current_tools,
            &skill_registry,
            &None, // memory_manager — evals run without memory in v1
            &secret_registry,
            log_activity,
            None, // cancel
            Some(&cfg.memory),
            agent,
            &cfg.mcp_servers,
        ) {
            Some(stack) => {
                tool_defs = stack.tools;
                plugin_executor = Some(stack.raw);
                stack.executor
            }
            None => {
                Arc::new(NoopRuntimeToolExecutor) as Arc<dyn crate::ports::engine::ToolExecutor>
            }
        };

    // 5. Wrap in StubbedExecutor for this row (in-process engines only:
    //    `refuse_stubs_on_bridge` above).
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

    // A Claude Code agent reaches its tools through the MCP bridge: the
    // executor's tool list + the `[[mcp_servers]]` behind it, as webhooks do.
    let (bridge_tools, mcp_servers) = crate::bootstrap::tools::bridge_inputs(
        engine.manages_own_workspace(),
        &tool_defs,
        &cfg.mcp_servers,
    );
    let engine_context = EngineContext {
        workspace: Some(workspace_path.clone()),
        system_prompt: Some(system_prompt.clone()),
        bridge_tools,
        max_tool_rounds: Some(agent.limits.max_tool_rounds),
        max_mcp_result_chars: Some(agent.limits.max_mcp_result_chars),
        mcp_servers,
    };

    let opts = LoopOpts {
        observer: Some(observer),
        ..LoopOpts::from_limits(&agent.limits)
    };
    let driver_fut = collect_engine_response(
        &*engine,
        &messages,
        &tool_defs,
        &engine_context,
        Some(&stubbed),
        &opts,
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
            crate::application::chat::tool_loop::EngineResponse {
                text: String::new(),
                input_tokens_delta: 0,
                output_tokens_delta: 0,
                cached_input_tokens_delta: 0,
                tool_outcomes: vec![],
                tool_runs: vec![],
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

    // 9. Transcript (skipped when --no-persist). Path still computed so
    // the RowResult has a stable identifier, even when the file isn't
    // actually written.
    let transcript_path = ctx
        .out_dir
        .join(format!("{}-{}.md", ctx.skill.name, ctx.row.id));
    if ctx.persist_transcript {
        std::fs::create_dir_all(ctx.out_dir)
            .with_context(|| format!("create out_dir {}", ctx.out_dir.display()))?;
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
    }

    if ctx.keep_workspace {
        std::mem::forget(ws); // leak TempDir guard — workspace persists on disk
    }

    // 10. Score against per-skill metric kinds (if any declared in SKILL.md frontmatter).
    let mut metric_outcomes: Vec<MetricOutcomeJson> = Vec::new();
    if !ctx.skill_metrics.is_empty() {
        use crate::application::skills::lifecycle::metric_kinds::{
            DescriptionTriggerKind, DialogReplayKind, LlmJudgeKind, ScriptKind, ShellCheckKind,
            ToolAssertionKind,
        };
        use crate::application::skills::lifecycle::metrics::{
            FixtureContext, MetricKind, MetricOutcome, MetricRunCtx,
        };

        let shell = crate::adapters::outbound::shell::LocalShellExecutor::new();
        let fixture = FixtureContext {
            prompt: &ctx.row.prompt,
            expected_outcome: Some(ctx.row.expected.as_str()),
            transcript: &engine_response.text,
        };
        let exec = plugin_executor.as_deref();
        let run_ctx = MetricRunCtx {
            skill_dir: &ctx.skill.skill_dir,
            // Where the agent's tools ran (its configured workspace, else the
            // row's temp dir) — what the metrics inspect and dispatch in.
            workspace: &workspace_path,
            shell: &shell,
            // The row agent's registry + dispatch context: `tool_assertion`
            // runs the tool live, as in `tengu skill evolve`.
            tools: exec.map(|e| &e.registry),
            judge: Some(Arc::clone(&ctx.judge_client)),
            http: exec.map(|e| &e.http),
            secret_registry: exec.map(|e| &*e.secret_registry),
            tool_scopes: exec.map(|e| &e.scopes),
            // Pre-authored fixture path: no live conversation, no sibling lookup.
            conversation: None,
            sibling_metrics: Some(ctx.skill_metrics),
        };
        for spec in ctx.skill_metrics {
            let outcome: anyhow::Result<MetricOutcome> = match spec {
                MetricSpec::ShellCheck { .. } => ShellCheckKind.run(spec, &fixture, &run_ctx).await,
                MetricSpec::LlmJudge { .. } => LlmJudgeKind.run(spec, &fixture, &run_ctx).await,
                MetricSpec::ToolAssertion { .. } => {
                    ToolAssertionKind.run(spec, &fixture, &run_ctx).await
                }
                MetricSpec::Script { .. } => ScriptKind.run(spec, &fixture, &run_ctx).await,
                MetricSpec::DialogReplay { .. } => {
                    DialogReplayKind.run(spec, &fixture, &run_ctx).await
                }
                MetricSpec::DescriptionTrigger { .. } => {
                    DescriptionTriggerKind.run(spec, &fixture, &run_ctx).await
                }
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

// ---------------------------------------------------------------------------
// Orchestrator dispatch path for eval rows.
// ---------------------------------------------------------------------------
//
// When an eval config declares `[orchestrator]`, rows are run through
// `Orchestrator::handle` instead of directly against the default agent's
// engine: the planner (`PlannerLlm`, the `[orchestrator]` agent's engine)
// plans and every step is a `run-agent` child (`SubprocessRunner`).
// `RowObserver` turns the bus's events into the judge's observations and
// sums the row session's token records.

/// What an orchestrated row collects while it runs.
struct EvalRowAccum {
    observations: Arc<std::sync::Mutex<Vec<Observation>>>,
    seq: Arc<AtomicU32>,
    /// (input, output) tokens of the row's planner calls and steps: every
    /// `metrics.recorded` of its session ([`RowObserver`]).
    tokens: Arc<std::sync::Mutex<(u32, u32)>>,
}

/// An orchestrated row's event observer: each `OrchestratorEvent` as a
/// synthetic observation for the judge ([`event_observation`]), on a task of
/// its own (the bus lags no one). [`Self::finish`] ends it after every event
/// sent so far — no timed wait.
pub(super) struct RowObserver {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl RowObserver {
    /// `tokens` sums the records of `session` (other sessions' records
    /// reach this bus too: the metrics sink is process-wide).
    pub(super) fn start(
        mut events: crate::application::orchestrator::EventReceiver,
        observations: Arc<std::sync::Mutex<Vec<Observation>>>,
        seq: Arc<AtomicU32>,
        tokens: Arc<std::sync::Mutex<(u32, u32)>>,
        session: String,
    ) -> Self {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let push = move |ev| {
            if let crate::application::orchestrator::OrchestratorEvent::MetricsRecorded { record } =
                &ev
            {
                if record.session_id == session {
                    let mut t = tokens.lock().unwrap();
                    t.0 = t.0.saturating_add(record.prompt_tokens);
                    t.1 = t.1.saturating_add(record.completion_tokens);
                }
            }
            let (name, detail) = event_observation(ev);
            let n = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            observations.lock().unwrap().push(Observation {
                seq: n,
                name,
                args_preview: detail,
            });
        };
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    got = events.recv() => match got {
                        Ok(ev) => push(ev),
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(dropped = n, "eval row observer lagged");
                        }
                        Err(RecvError::Closed) => return,
                    },
                    _ = &mut stopped => {
                        // Everything sent before the stop is in the channel.
                        loop {
                            match events.try_recv() {
                                Ok(ev) => push(ev),
                                Err(TryRecvError::Lagged(_)) => {}
                                Err(_) => return,
                            }
                        }
                    }
                }
            }
        });
        Self { stop, task }
    }

    /// Observe every event sent so far, then stop.
    pub(super) async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

/// The end of an orchestrated row, in order and with no timed wait: the
/// metrics barrier (every record put on the process sink so far is on the
/// bus — in the trace and with the observer), the observer drained to
/// everything sent, the row's trace gate shut (a straggler of an aborted
/// step lands nowhere), then `run.closed`, the run's last event.
pub(super) async fn close_row(
    orchestrator: &crate::application::orchestrator::Orchestrator,
    observer: RowObserver,
    gate: Option<&crate::application::trace_exec::GatedTrace>,
    trace: Option<&Arc<dyn TraceSink>>,
    timed_out: bool,
) {
    orchestrator.flush_metrics().await;
    observer.finish().await;
    if let Some(g) = gate {
        g.close();
    }
    if let Some(t) = trace {
        let reason = if timed_out {
            "row timed out"
        } else {
            "row done"
        };
        crate::bootstrap::trace::close(&**t, reason, timed_out);
    }
}

async fn run_row_via_orchestrator(
    ctx: RowCtx<'_>,
    started: Instant,
    _ws: tempfile::TempDir,
    ws_path: PathBuf,
    cfg: Config,
) -> anyhow::Result<RowResult> {
    let accum = Arc::new(EvalRowAccum {
        observations: Arc::new(std::sync::Mutex::new(Vec::new())),
        seq: Arc::new(AtomicU32::new(0)),
        tokens: Arc::new(std::sync::Mutex::new((0, 0))),
    });

    let cfg_arc = Arc::new(cfg);
    // The planner calls its own engine (no tools); each step is a
    // `run-agent` child.
    let llm = crate::bootstrap::orchestrator::planner_llm(&cfg_arc)?
        .ok_or_else(|| anyhow::anyhow!("eval row via orchestrator without [orchestrator]"))?;
    let engine_id_str = llm.engine.id().to_string();
    let session = crate::bootstrap::orchestrator::resolve_session_id();

    // One recording per orchestrated row (`RunKind::Eval`): its plans,
    // steps and their `run-agent` tool calls; `run.closed` at the end. The
    // orchestrator writes through a gate the row shuts before `run.closed`
    // (`close_row`).
    let trace = crate::bootstrap::trace::open_orchestration(
        &cfg_arc,
        crate::domain::trace::RunKind::Eval,
        &Arc::new(crate::adapters::outbound::secrets::process_secret_registry(
            None,
        )),
    );
    let gate = trace
        .as_ref()
        .map(|s| crate::application::trace_exec::GatedTrace::new(Arc::clone(s)));
    let orchestrator = crate::bootstrap::orchestrator::build_orchestrator(
        &cfg_arc,
        llm,
        session.clone(),
        gate.as_ref().and_then(|g| {
            let sink: Arc<dyn TraceSink> = Arc::clone(g) as Arc<dyn TraceSink>;
            crate::application::orchestrator::trace::OrchestratorTrace::of(&sink, None)
        }),
    )
    .ok_or_else(|| {
        anyhow::anyhow!("build_orchestrator returned None despite [orchestrator] block")
    })?;

    // Subscribe to orchestrator events and convert each into a synthetic
    // observation. The judge receives the full trace: plan shape + each
    // step boundary interleaved with tool calls. Without this, the judge
    // sees only the worker tool calls and can't distinguish "orchestrator
    // fanned out to 3 parallel researchers" from "one researcher made 3
    // sequential calls" — the plan semantics are invisible at the tool
    // layer alone.
    let observer = RowObserver::start(
        orchestrator.subscribe(),
        Arc::clone(&accum.observations),
        Arc::clone(&accum.seq),
        Arc::clone(&accum.tokens),
        session,
    );

    // Drive with the row's timeout.
    let prompt = ctx.row.prompt.clone();
    let driver_fut = orchestrator.handle(prompt);
    let (timed_out, final_text) = match tokio::time::timeout(
        std::time::Duration::from_secs(ctx.row.timeout_secs),
        driver_fut,
    )
    .await
    {
        Ok(text) => (false, text),
        Err(_) => (true, String::new()),
    };
    close_row(
        &orchestrator,
        observer,
        gate.as_deref(),
        trace.as_ref(),
        timed_out,
    )
    .await;

    // Snapshot results from the accumulator.
    let obs_snapshot = accum.observations.lock().unwrap().clone();
    let (input_tokens, output_tokens) = *accum.tokens.lock().unwrap();

    // Judge.
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
        let outcome = judge_row(&*ctx.judge, &ctx.row.expected, &obs_snapshot, &final_text).await?;
        (outcome.verdict, outcome.input_tokens, outcome.output_tokens)
    };

    let judge_user_turn = format_judge_user_turn(&ctx.row.expected, &obs_snapshot, &final_text);

    // Transcript (skipped when --no-persist).
    let transcript_path = ctx
        .out_dir
        .join(format!("{}-{}.md", ctx.skill.name, ctx.row.id));
    if ctx.persist_transcript {
        std::fs::create_dir_all(ctx.out_dir)
            .with_context(|| format!("create out_dir {}", ctx.out_dir.display()))?;
    }

    // The orchestrator path doesn't have a single `messages` array like
    // the direct path — we synthesize a minimal one for the transcript.
    let default_agent_id = cfg_arc
        .agents
        .iter()
        .find(|(_, v)| v.default)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "default".into());
    let default_agent_model = cfg_arc
        .agents
        .get(&default_agent_id)
        .map(|a| a.model.clone())
        .unwrap_or_default();
    let stub_messages = vec![
        Message {
            role: Role::System,
            content: format!(
                "[orchestrator dispatch — actual prompts per-step; \
                 default agent was `{}`]",
                default_agent_id
            ),
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

    if ctx.persist_transcript {
        write_transcript(
            &transcript_path,
            &ctx.skill.name,
            ctx.row,
            &default_agent_model,
            &engine_id_str,
            &ws_path,
            &stub_messages,
            &[], // tool_outcomes — orchestrator path observations already include them
            &obs_snapshot,
            &final_text,
            &verdict,
            &judge_user_turn,
        )?;
    }

    if ctx.keep_workspace {
        std::mem::forget(_ws);
    }

    // Metric scoring (parity with the direct path) — skipped here; eval
    // row scoring currently lives in the direct-path section. Follow-up
    // if orchestrator evals need metric scoring too.
    let metric_outcomes: Vec<MetricOutcomeJson> = Vec::new();

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
            input: input_tokens,
            output: output_tokens,
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
