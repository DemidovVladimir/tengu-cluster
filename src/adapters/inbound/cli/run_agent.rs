//! `tengu run-agent` — the plan-step subprocess. Reads `AgentIpcInput` from
//! stdin, runs one `[agents.<name>]` LLM-with-tools loop until
//! `compress_and_store`, writes `AgentIpcOutput` to stdout. Spawned by
//! `SubprocessRunner`.

use anyhow::{Context, Result};

use crate::bootstrap::sandbox::load_sandbox_or;
use crate::config::paths::default_config_path;
use crate::config::Config;

#[cfg(feature = "postgres_memory")]
async fn try_persist_agentic_step_summary(
    session_id: &str,
    step_id: &str,
    summary: &str,
) -> anyhow::Result<String> {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                // Open Brain is `vector(1536)`: always the pinned model, as
                // the planner and the `agentic_memory` tool embed with.
                crate::domain::memory::DEFAULT_EMBEDDING_MODEL.to_string(),
            );
            match embedder.embed(summary).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "agentic_memory: step summary embedding failed; writing text-only memory"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };
    crate::adapters::outbound::tools::agentic_memory::write_step_summary_with_embedding(
        session_id,
        step_id,
        summary,
        embedding.as_deref(),
    )
    .await
}

/// `tengu run-agent` handler — Phase 5b (real LLM + multi-turn tool loop).
///
/// 1. Verifies `TENGU_AGENT_IPC=1` is set (prevents accidental re-entry).
/// 2. Reads one JSON `AgentIpcInput` from stdin.
/// 3. Loads the parent's config (sandbox via IPC, else the default config) and
///    takes `[agents.<name>]` from it — model, engine, tools, skills, limits.
///    Its workspace (`bootstrap::tools::workspace_or_temp`): the agent's,
///    else a temp dir for this step — the executor's and the engine's (a
///    Claude Code CLI's cwd, its bridge).
/// 4. Composes the system prompt: base template + skill bodies (three-tier
///    loader) + mandatory `compress_and_store` suffix.
/// 5. Builds the engine (`engine` = `openrouter` | `local` | `claude_code`) for
///    the agent's model as a step (`build_step_engine`: a Claude Code bridge
///    grants the workspace and writes `compress_and_store` into a summary file).
/// 6. Builds the tool stack: `effective_tools = (base ∩ agent.tools) ∪
///    {compress_and_store} ∪ shell skills ∪ [[mcp_servers]] tools` plus a
///    `PluginToolExecutor` over those tools (`build_subprocess_tool_executor`).
/// 7. Drives a multi-turn loop: per turn, drain stream → if tool_calls,
///    dispatch each → append assistant + tool messages → repeat. Results
///    enter as in-process chat feeds them (`tool_loop::tool_result_content`:
///    `limits.max_tool_result_chars`, local models fitted to the window),
///    older rounds compacted to line 1 from turn 1 on. Stop on:
///    - empty tool_calls (model done; a Claude Code turn ends here — its
///      bridged `compress_and_store` summary is read from the summary file,
///      and the engine ends the CLI run right after that call)
///    - `compress_and_store` invoked (capture summary, exit clean)
///    - the agent's `limits.max_tool_rounds` exceeded (Ok when the model wrote
///      any text — it becomes the summary — else Failed)
/// 8. Emit one `AgentIpcOutput` JSON line on stdout and exit — with the
///    per-turn `metrics` and the tool activity `tools` (every call and its
///    outcome, bridged Claude Code calls included).
pub(super) async fn run_agent_subprocess() -> Result<()> {
    use crate::domain::message::{Message, Role};
    use crate::ports::engine::EngineContext;
    use tokio::io::AsyncReadExt;

    if std::env::var("TENGU_AGENT_IPC").ok().as_deref() != Some("1") {
        anyhow::bail!(
            "`tengu run-agent` is a subprocess mode not meant for direct invocation. \
             Set TENGU_AGENT_IPC=1 if you really want to run it (e.g. via tests/run_agent_ipc.rs)."
        );
    }

    // Read stdin to EOF.
    let mut buf = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut buf)
        .await
        .context("read IPC input from stdin")?;
    let input: crate::adapters::outbound::subprocess_runner::AgentIpcInput =
        serde_json::from_slice(&buf).context("parse IPC input JSON")?;

    tracing::info!(
        agent = %input.agent_name,
        session = %input.session_id,
        step = %input.step_id,
        "run-agent received"
    );

    // Phase 7.6 Bug B — expose session_id via env so plugins (notably
    // `agentic_memory` `capture`) can stamp it on their writes without needing it
    // threaded through ToolCtx. Set BEFORE building the tool executor so any
    // plugin construction that reads it sees the right value.
    std::env::set_var("TENGU_SESSION_ID", &input.session_id);

    // ----- Resolve parent config (Phase 7.2 sandbox inheritance) -----
    //
    // The child runs on the SAME config as the parent: `sandbox_config`
    // names the sandbox (`sandboxes/<name>/config.toml`), otherwise the
    // default config chain applies. `load_sandbox_or` returns `Err` only
    // when the file exists but fails to parse; we fall back to the default
    // config in that case (warn-and-continue).
    let parent_config = match load_sandbox_or(
        input.sandbox_config.clone(),
        load_config_or_default_unconditional(),
    ) {
        Ok(cfg) => {
            if let Some(ref name) = input.sandbox_config {
                tracing::info!(
                    sandbox = %name,
                    "subprocess loaded sandbox config (Phase 7.2)"
                );
            }
            cfg
        }
        Err(e) => {
            tracing::warn!(
                sandbox = ?input.sandbox_config,
                error = %e,
                "subprocess failed to load sandbox config; falling back to default"
            );
            load_config_or_default_unconditional()
        }
    };

    // Parent's `[egress]` (TENGU_EGRESS) wins; an invalid policy aborts the
    // child rather than running tools unproxied.
    crate::adapters::outbound::egress::install(&parent_config.egress)
        .context("run-agent: install egress policy")?;

    // ----- Resolve the agent: `[agents.<name>]` of that config -----
    //
    // Phase 6.7 (C→B B-half): when `input.compose` is set, the parent has
    // composed a transient agent. Take the BASE block
    // `[agents.<compose.base_agent>]` (not `[agents.<input.agent_name>]`,
    // which may be a synthetic label for events/logs), then override the
    // base's `skills` and `tools` with the values the planner picked from
    // the planner registry roster. The override is in-memory only — the config
    // on disk is unchanged. In a hardened sandbox it may only narrow the
    // base (`bootstrap::tools::compose_agent`); a widening compose fails the
    // step here, before the engine is built.
    let (spec_load_name, compose_override) = match &input.compose {
        Some(c) => {
            tracing::info!(
                base = %c.base_agent,
                label = %input.agent_name,
                skill_override_count = c.skills.len(),
                tool_override_count = c.tools.len(),
                "run-agent: composed agent (C→B B-half)"
            );
            (c.base_agent.clone(), Some(c.clone()))
        }
        None => (input.agent_name.clone(), None),
    };
    // Only routable blocks (with a `description`) may run as a plan step —
    // the same rule `SubprocessRunner::run_step` and the registry apply.
    let mut spec = parent_config
        .agents
        .get(&spec_load_name)
        .filter(|a| a.description.is_some())
        .cloned()
        .with_context(|| {
            let mut known: Vec<&str> = parent_config
                .agents
                .iter()
                .filter(|(_, a)| a.description.is_some())
                .map(|(n, _)| n.as_str())
                .collect();
            known.sort();
            format!(
                "no agent `{}` in the active config (sandbox: {}); routable agents (with a `description`): {:?}",
                spec_load_name,
                input.sandbox_config.as_deref().unwrap_or("default config"),
                known
            )
        })?;
    if let Some(c) = compose_override {
        spec = crate::bootstrap::tools::compose_agent(
            &spec_load_name,
            &spec,
            &c,
            crate::config::hardening::requires_hardened_claude_code(&parent_config),
            parent_config.memory.enabled,
        )?;
    }
    // The step's one workspace (`workspace_or_temp`): the executor, the memory
    // store and the engine — a Claude Code CLI's cwd and its bridge — all
    // use it. `step_dir` (an agent without `workspace`) is removed when the
    // step ends.
    let configured_workspace = spec.workspace.is_some();
    let (workspace, step_dir) =
        crate::bootstrap::tools::workspace_or_temp(spec.workspace.as_deref(), "tengu-step-")
            .context("run-agent: the step's workspace")?;
    spec.workspace = Some(workspace.clone());
    if let Some(dir) = &step_dir {
        tracing::info!(
            workspace = %workspace.display(),
            temp_dir = %dir.path().display(),
            "run-agent: the agent has no `workspace` — this step runs in a temp dir"
        );
    }
    // Resolved agent name (the base for composed agents) — exposed like
    // TENGU_SESSION_ID so plugins can attribute writes without ToolCtx plumbing.
    std::env::set_var("TENGU_AGENT_NAME", &spec_load_name);

    // IPC `model` overrides the agent block's `model` when non-empty (the
    // orchestrator can swap models per-step in the future).
    let model = if input.model.is_empty() {
        spec.model.clone()
    } else {
        input.model.clone()
    };

    // ----- Compose system prompt: base + identity + skill bodies + suffix -----
    let mut system_prompt = String::from(BASE_AGENT_TEMPLATE);
    let identity = identity_block(&spec);
    if !identity.is_empty() {
        system_prompt.push_str("\n\n---\n\n");
        system_prompt.push_str(&identity);
    }
    for skill_name in &spec.skill_packages {
        match load_skill_body_three_tier(skill_name, &workspace) {
            Some(body) => {
                system_prompt.push_str("\n\n---\n\n");
                system_prompt.push_str(&body);
            }
            None => {
                tracing::warn!(skill = %skill_name, "skill body not found in any tier");
            }
        }
    }
    // Plan block: the parent's per-session `plan_state` IPC field is the
    // source of truth; the global `TENGU_PLAN.md` is only a fallback for old
    // parents that don't send it (it is overwritten by every session).
    let plan_state = match input.plan_state.as_deref() {
        Some(rendered) => crate::application::orchestrator::shared_files::plan_state_block(
            rendered,
            "IPC `plan_state`",
        ),
        None => crate::application::orchestrator::shared_files::read_plan_state_block(
            &std::env::current_dir()?,
        ),
    };
    if !plan_state.is_empty() {
        system_prompt.push_str("\n\n---\n\n");
        system_prompt.push_str(&plan_state);
    }
    system_prompt.push_str(MANDATORY_SUFFIX);

    // ----- Build engine (Phase 7.3 — honour the agent's engine) -----
    let mut agent_cfg_for_engine = crate::bootstrap::tools::subagent_config(&spec);
    // The Claude Code engine ships these scopes to the MCP bridge; the child
    // workspace must be an allowed fs root there too.
    crate::bootstrap::tools::grant_workspace_root(&mut agent_cfg_for_engine.scopes, &workspace);
    // A Claude Code step's bridge serves `compress_and_store` into this file
    // (the loop below intercepts it for the other engines); read back after
    // the turn. Removed when this process ends.
    let summary_file = if spec.engine == "claude_code" {
        Some(tempfile::NamedTempFile::new().context("run-agent: step summary file")?)
    } else {
        None
    };
    // The base block's name: a Claude Code bridge loads `[agents.<it>]` and
    // grants this step's workspace, as the executor below does.
    let engine = crate::adapters::outbound::engines::build_step_engine(
        &spec_load_name,
        &agent_cfg_for_engine,
        parent_config.claude_code.as_ref(),
        crate::adapters::outbound::engines::StepOpts {
            grant_workspace: true,
            summary_file: summary_file.as_ref().map(|f| f.path().to_path_buf()),
            config_file: None,
        },
    )
    .with_context(|| {
        format!(
            "build engine for agent {} (engine={}, model={})",
            input.agent_name, spec.engine, model
        )
    })?;
    tracing::info!(
        agent = %input.agent_name,
        engine = %spec.engine,
        model = %model,
        "subprocess engine built"
    );
    let stream_event_timeout_secs = spec.limits.stream_event_timeout_secs;
    // Every result enters capped and older rounds are compacted to line 1,
    // as `collect_engine_response` does in-process (`tool_loop` module
    // table): `limits.max_tool_result_chars`, and for local models
    // (`Engine::tool_result_char_cap`) the window fit.
    let engine_result_cap = engine.tool_result_char_cap();
    let max_tool_result_chars = spec.limits.max_tool_result_chars as usize;

    // ----- Build tool stack (Phase 5b) -----
    // The parent's vault values (inherited, never prompts): tool output is
    // redacted like in the parent and in a Claude Code bridge.
    let secret_registry = std::sync::Arc::new(
        crate::adapters::outbound::secrets::process_secret_registry(None),
    );
    let activity: std::sync::Arc<dyn crate::ports::tool_activity::ToolActivityPort> =
        std::sync::Arc::new(SubprocessActivity);
    // Phase 7.6 Bug A — build a real MemoryManager from the parent config so
    // the MemoryPlugin can register persistent_store / memory_ingest as
    // callable handlers (not just advertised tool defs). Without this,
    // MCP-routed Claude Code calls to those tools fail with
    // "Tool 'X' is not available to this agent" even though the tool def is
    // in the advertised list.
    // The store sits in the agent's own workspace, else at `[memory]
    // store_path` (as in-process chat and the bridge): never in a step's
    // temp dir, which goes when the step ends.
    let memory_manager = if parent_config.memory.enabled {
        Some(
            crate::bootstrap::memory::build_memory_manager_async(
                &parent_config.memory,
                configured_workspace.then_some(workspace.as_path()),
            )
            .await,
        )
    } else {
        None
    };

    let (tools, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
        &spec,
        &parent_config,
        &workspace,
        &secret_registry,
        activity,
        memory_manager.clone(),
    );
    let executor = executor.map(|e| {
        crate::adapters::outbound::secrets::SanitizedToolExecutor::new(
            std::sync::Arc::new(e),
            std::sync::Arc::clone(&secret_registry),
        )
    });
    // Diagnostic: log the actual tool NAMES the subprocess can call, so we
    // can verify (in the parent log) whether expected tools like
    // `persistent_store` made it through the `[agents.<name>].tools` allow-list +
    // workspace_tools opt-in machinery. Critical for debugging "agent says
    // tool unavailable" symptoms — without this we have no visibility into
    // the subprocess's tool world.
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    tracing::info!(
        agent = %input.agent_name,
        engine = %spec.engine,
        tool_count = tools.len(),
        tools = ?tool_names,
        "subprocess tool stack built"
    );

    // ----- Build messages + context -----
    let mut messages = vec![
        Message {
            role: Role::System,
            content: system_prompt.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: input.goal.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    // Phase 7.4 — when running on Claude Code engine, expose tengu's plugin
    // tools to the CLI via its MCP bridge. Without this the Claude CLI only
    // has its built-in tools (Read/Write/Edit/Bash) and treats tengu tools
    // (http_request, compress_and_store, persistent_store, etc.) as unknown
    // — agents end up calling them as bash commands and failing.
    //
    // OpenRouter path leaves bridge_tools = None (the ToolDef list is
    // registered through the OpenAI-compatible function-calling API
    // instead, handled by `tools` passed to run_single_engine_turn).
    let bridge_tools_for_ctx: Option<Vec<crate::domain::message::ToolDef>> =
        if spec.engine == "claude_code" {
            Some(tools.clone())
        } else {
            None
        };
    let context = EngineContext {
        // Always set: a Claude Code engine starts its bridge (and runs the
        // CLI) only in a workspace.
        workspace: Some(workspace.clone()),
        system_prompt: Some(system_prompt),
        bridge_tools: bridge_tools_for_ctx,
        max_tool_rounds: Some(input.max_turns),
        max_mcp_result_chars: Some(spec.limits.max_mcp_result_chars),
        mcp_servers: parent_config.mcp_servers.clone(),
    };

    // ----- Multi-turn loop (Phase 5b) -----
    let mut final_text = String::new();
    let mut summary: Option<String> = None;
    let mut compress_called = false;
    // Per-turn metrics records — shipped back to the parent in the IPC
    // output so they can be re-emitted on the parent's metrics bus.
    let mut subagent_metrics: Vec<crate::domain::metrics::MetricsRecord> = Vec::new();
    // Tool activity in call order (IPC `tools`): calls dispatched below and
    // the ones a Claude Code engine ran through its bridge.
    let mut tool_runs: Vec<crate::domain::message::ToolRun> = Vec::new();

    for turn in 0..input.max_turns {
        // Compute the prompt size BEFORE the engine call so the metric
        // record reflects what we sent. UTF-8-aware char count + byte len.
        let prompt_chars: u32 = messages
            .iter()
            .map(|m| m.content.chars().count() as u32)
            .sum();
        let prompt_bytes: u32 = messages.iter().map(|m| m.content.len() as u32).sum();
        let turn_started = std::time::Instant::now();

        let drained = crate::application::chat::tool_loop::run_single_engine_turn(
            engine.as_ref(),
            &messages,
            &tools,
            &context,
            None,
            stream_event_timeout_secs,
        )
        .await
        .context("engine turn failed")?;
        tool_runs.extend(drained.engine_runs);
        let (text, tool_calls, input_delta, output_delta) = (
            drained.text,
            drained.tool_calls,
            drained.input_tokens,
            drained.output_tokens,
        );

        // Record one metric per engine turn. `input_delta`/`output_delta`
        // come from `StreamEvent::Usage` frames (OpenRouter + Claude Code
        // both supply them); they're 0 when the engine doesn't return usage.
        let rec = crate::domain::metrics::MetricsRecord {
            ts_unix: crate::domain::metrics::now_unix(),
            session_id: input.session_id.clone(),
            kind: crate::domain::metrics::MetricsKind::Subagent,
            agent: input.agent_name.clone(),
            model: model.clone(),
            prompt_tokens: input_delta,
            completion_tokens: output_delta,
            total_tokens: input_delta.saturating_add(output_delta),
            prompt_chars,
            prompt_bytes,
            response_chars: text.chars().count() as u32,
            latency_ms: turn_started.elapsed().as_millis() as u64,
            layers: Vec::new(),
            step_id: Some(input.step_id.clone()),
        };
        // Emit into the subprocess's own tracing log too (the parent forwards
        // stderr — Phase 7.5) so the user sees the same line whether they
        // grep the parent log or a future subprocess log file.
        crate::application::metrics::record(rec.clone());
        subagent_metrics.push(rec);

        // If no tool calls, the model produced its final answer. Save the
        // text and exit the loop.
        if tool_calls.is_empty() {
            final_text = text;
            break;
        }

        // Append the assistant message carrying the tool_calls so the next
        // engine turn sees the full call/result history.
        let compact_cutoff = messages.len();
        messages.push(Message {
            role: Role::Assistant,
            content: text.clone(),
            tool_call_id: None,
            tool_calls: Some(tool_calls.clone()),
        });
        if !text.is_empty() {
            final_text = text;
        }

        // Dispatch each tool call.
        for call in &tool_calls {
            let (result, observation, ok) = if call.name == "compress_and_store" {
                // Out-of-band handling: capture the summary here; with
                // `postgres_memory` it is persisted to Postgres `agentic_memory`.
                let extracted_summary = call
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                summary = Some(extracted_summary.clone());
                compress_called = true;
                #[cfg(feature = "postgres_memory")]
                {
                    let _ = try_persist_agentic_step_summary(
                        &input.session_id,
                        &input.step_id,
                        &extracted_summary,
                    )
                    .await;
                }
                ("stored".to_string(), None, true)
            } else if let Some(ref exec) = executor {
                use crate::ports::engine::ToolExecutor;
                match exec.execute_typed(call, &messages).await {
                    Ok(out) => (out.text, out.observation, true),
                    Err(e) => {
                        // The error is redacted by `SanitizedToolExecutor`;
                        // in the step's log with the model's arguments so a
                        // failed call is diagnosable (the parent forwards
                        // stderr).
                        let args = call.arguments.to_string();
                        let args = crate::domain::token::truncate_at_boundary(&args, 300)
                            .map_or(args.as_str(), |(prefix, _)| prefix);
                        tracing::warn!(tool = %call.name, error = %e, arguments = %args, "subagent tool call failed");
                        (format!("tool error: {}", e), None, false)
                    }
                }
            } else {
                (
                    format!("tool '{}' is not available in this subprocess", call.name),
                    None,
                    false,
                )
            };
            tool_runs.push(crate::domain::message::ToolRun {
                name: call.name.clone(),
                ok,
            });
            let content = crate::application::chat::tool_loop::tool_result_content(
                &result,
                observation.as_ref(),
                engine_result_cap,
                max_tool_result_chars,
            );

            messages.push(Message {
                role: Role::Tool,
                content,
                tool_call_id: Some(call.id.clone()),
                tool_calls: None,
            });
        }
        if turn >= 1 {
            crate::application::chat::tool_loop::compact_older_tool_results(
                &mut messages[..compact_cutoff],
                spec.limits.compact_result_limit as usize,
            );
        }

        if compress_called {
            tracing::info!(
                turn,
                "compress_and_store invoked; exiting subagent loop cleanly"
            );
            break;
        }

        if turn + 1 >= input.max_turns {
            tracing::warn!(
                turn,
                max_turns = input.max_turns,
                "subagent loop hit max_turns without compress_and_store"
            );
        }
    }

    // A Claude Code step called `compress_and_store` through its bridge.
    if summary.is_none() {
        if let Some(text) = summary_file
            .as_ref()
            .and_then(|f| bridged_summary(f.path()))
        {
            tracing::info!(
                chars = text.chars().count(),
                "compress_and_store called through the bridge; its summary is the step summary"
            );
            compress_called = true;
            #[cfg(feature = "postgres_memory")]
            {
                let _ = try_persist_agentic_step_summary(&input.session_id, &input.step_id, &text)
                    .await;
            }
            summary = Some(text);
        }
    }
    // If the model never called compress_and_store, treat the final
    // assistant text as the summary (graceful degradation, same as Phase 5a).
    let summary = summary.unwrap_or_else(|| final_text.clone());

    // Backstop the durable write when the subagent finished WITHOUT calling
    // compress_and_store — the path Claude Code subagents most often take
    // (they "just stop" after their last tool call). Fail-soft: errors are
    // logged and swallowed; the IPC payload still goes back to the parent
    // unchanged — recall is best-effort, not a barrier to step completion.
    #[cfg(feature = "postgres_memory")]
    if !compress_called && !summary.trim().is_empty() {
        match try_persist_agentic_step_summary(&input.session_id, &input.step_id, &summary).await {
            Ok(id) => tracing::info!(
                entry_id = %id,
                session_id = %input.session_id,
                step_id = %input.step_id,
                summary_chars = summary.chars().count(),
                "agentic_memory: backstop wrote final_text summary to Postgres"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                session_id = %input.session_id,
                step_id = %input.step_id,
                "agentic_memory: backstop write FAILED"
            ),
        }
    }

    // Choose the user-visible `output` text. Models that only emit tool
    // calls (no inline assistant text) leave `final_text` empty; in that
    // case the summary the model produced via compress_and_store is the
    // most useful thing to show.
    let output = if !final_text.is_empty() {
        final_text.clone()
    } else if !summary.is_empty() {
        summary.clone()
    } else {
        String::new()
    };

    // Phase 5c — middle-ground protocol enforcement.
    // Pass:    compress_and_store called              → Ok (the canonical good path)
    // Pass:    no compress_and_store but text produced → Ok (model answered usefully)
    // Fail:    no compress_and_store AND no text       → Failed (DagExecutor retries)
    //
    // The strict-doctrine version of the original compress_and_store spec would flip the second row
    // to Failed too; we deliberately stay pragmatic — many models produce
    // good answers in pure-text turns without calling the protocol tool.
    if !compress_called {
        tracing::warn!(
            agent = %input.agent_name,
            "model finished without calling compress_and_store"
        );
    }
    let out = if compress_called || !final_text.is_empty() {
        crate::adapters::outbound::subprocess_runner::AgentIpcOutput::Ok {
            output,
            summary,
            metrics: subagent_metrics,
            tools: tool_runs,
        }
    } else {
        // Genuinely empty run — no text, no protocol call, no useful output.
        // Surface as Failed so the orchestrator can retry / replan.
        crate::adapters::outbound::subprocess_runner::AgentIpcOutput::Failed {
            error: format!(
                "subagent '{}' produced no output and did not call compress_and_store",
                input.agent_name
            ),
            output,
            metrics: subagent_metrics,
            tools: tool_runs,
        }
    };
    let json = serde_json::to_string(&out).context("serialise IPC output")?;
    println!("{}", json);
    // The temp workspace of a step without one goes now, not earlier.
    drop(step_dir);
    Ok(())
}

/// The summary a Claude Code step's bridge wrote for `compress_and_store`
/// (`mcp_bridge::StepSummary`); `None` when the model never called it (the
/// file is empty) or it holds only whitespace.
fn bridged_summary(file: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(file)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Subprocess `ToolActivityPort` impl — silent. The parent runner sees
/// progress via the engine's StreamEvent::TextDelta path, not via this hook.
struct SubprocessActivity;
impl crate::ports::tool_activity::ToolActivityPort for SubprocessActivity {
    fn publish_tool_activity(&self, _call: &crate::domain::message::ToolCall) {}
}

/// Config loader used by the `run-agent` subprocess path — needs
/// `parent_config.default_scopes` regardless of which memory features are
/// compiled in.
/// The `run-agent` child's base config: the same file the parent resolved
/// (`$TENGU_CONFIG`, pinned by `main`), loaded with env substitution and
/// validation. Built-in defaults only when there is no file; a file that
/// fails to load is an error worth seeing, not a silent `Config::default()`.
fn load_config_or_default_unconditional() -> Config {
    let path = default_config_path();
    if !path.is_file() {
        return Config::default();
    }
    match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "run-agent: config failed to load; falling back to built-in defaults (no agents from it)"
            );
            Config::default()
        }
    }
}

/// Hardcoded base prompt that applies to every subagent. Kept short — the
/// real per-agent character comes from the loaded skill bodies.
const BASE_AGENT_TEMPLATE: &str = "You are a focused subagent run as a single-shot process. \
Read the user's goal, do exactly what is asked, and respond concisely with the result. \
Do not ask follow-up questions — make reasonable assumptions and answer the user directly.";

/// Mandatory suffix appended to every subagent system prompt — Phase 5b
/// version. Tells the model to call `compress_and_store(summary)` as its
/// final action; the harness captures the summary (persisted to Postgres
/// `agentic_memory` with `postgres_memory`) and exits the loop on that call.
const MANDATORY_SUFFIX: &str = "\n\n---\n\n\
When you have completed your task, your FINAL action MUST be to call the \
`compress_and_store` tool with a concise `summary` of what you accomplished. \
Failure to call it will be treated as task failure.";

/// A skill's SKILL.md body (frontmatter stripped) from the first of the
/// skill loader's directories that has it (`skills::registry::skill_directories`:
/// managed, workspace dotdir, workspace `skills/`, the repo's `skills/`).
fn load_skill_body_three_tier(name: &str, workspace: &std::path::Path) -> Option<String> {
    // The skill loader's directories and order (managed first): a step used
    // to read the repo's `skills/` first, so it could load another body than
    // the chat agent and the planner registry.
    let candidates: Vec<std::path::PathBuf> =
        crate::application::skills::registry::skill_directories(workspace)
            .into_iter()
            .map(|dir| dir.join(name).join("SKILL.md"))
            .collect();

    for path in &candidates {
        if !path.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "read SKILL.md failed");
                continue;
            }
        };
        // Strip frontmatter `---<yaml>---` if present.
        if content.starts_with("---") {
            let after_first = &content[3..];
            if let Some(end) = after_first.find("\n---") {
                let mut body_start = end + 4;
                let bytes = after_first.as_bytes();
                if body_start < bytes.len() && bytes[body_start] == b'\n' {
                    body_start += 1;
                }
                return Some(after_first[body_start..].trim_start().to_string());
            }
        }
        return Some(content);
    }
    None
}

/// The agent's own name, `role` and `[agents.<a>.identity] instructions`,
/// as a chat turn shows them (`skills::registry::build_system_prompt`) — a
/// plan step used to run without them. Empty when none is set.
fn identity_block(spec: &crate::config::AgentConfig) -> String {
    let mut parts = Vec::new();
    if let Some(name) = spec
        .identity
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        parts.push(format!("You are {name}."));
    }
    if let Some(role) = spec
        .role
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        parts.push(format!(
            "Your role: {}.",
            role.to_lowercase().replace('-', "_")
        ));
    }
    if let Some(text) = spec
        .identity
        .instructions
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        parts.push(
            crate::application::chat::prompt_budget::truncate_to_token_budget(
                text,
                spec.prompt_budget.max_file_tokens,
            ),
        );
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step's prompt carries the agent's identity (name, role,
    /// instructions); nothing when none is set.
    #[test]
    fn identity_reaches_the_step_prompt() {
        let spec: crate::config::AgentConfig = toml::from_str(
            "engine = \"local\"\nmodel = \"m\"\nrole = \"Crypto-Researcher\"\n[identity]\nname = \"Kai\"\ninstructions = \"Always cite the pool address in full.\"\n",
        )
        .unwrap();
        let block = identity_block(&spec);
        assert!(block.contains("You are Kai."), "{block}");
        assert!(block.contains("Your role: crypto_researcher."), "{block}");
        assert!(
            block.contains("Always cite the pool address in full."),
            "{block}"
        );
        let bare: crate::config::AgentConfig =
            toml::from_str("engine = \"local\"\nmodel = \"m\"\n").unwrap();
        assert_eq!(identity_block(&bare), "");
    }

    /// The step summary a Claude Code bridge leaves: none until
    /// `compress_and_store` wrote one; whitespace is none.
    #[test]
    fn bridged_summary_is_the_written_text() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(bridged_summary(file.path()), None);
        std::fs::write(file.path(), " \n").unwrap();
        assert_eq!(bridged_summary(file.path()), None);
        std::fs::write(file.path(), "done: 42").unwrap();
        assert_eq!(bridged_summary(file.path()).as_deref(), Some("done: 42"));
        assert_eq!(
            bridged_summary(std::path::Path::new("/nonexistent/x")),
            None
        );
    }
}
