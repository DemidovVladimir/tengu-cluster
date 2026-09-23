//! `tengu run-agent` — the plan-step subprocess. Reads `AgentIpcInput` from
//! stdin, runs one `[agents.<name>]` LLM-with-tools loop until
//! `compress_and_store`, writes `AgentIpcOutput` to stdout. Spawned by
//! `SubprocessRunner`.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::bootstrap::sandbox::load_sandbox_or;
use crate::config::paths::default_config_path;
use crate::config::Config;

#[cfg(feature = "postgres_memory")]
async fn try_persist_agentic_step_summary(
    parent_config: &crate::config::Config,
    session_id: &str,
    step_id: &str,
    summary: &str,
) -> anyhow::Result<String> {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                parent_config.memory.embedding_model.clone(),
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
/// 4. Composes the system prompt: base template + skill bodies (three-tier
///    loader) + mandatory `compress_and_store` suffix.
/// 5. Builds the engine (`engine` = `openrouter` | `claude_code`) for the agent's model.
/// 6. Builds the tool stack: `effective_tools = (base ∩ agent.tools) ∪ {compress_and_store}`
///    plus a `PluginToolExecutor` over those tools.
/// 7. Drives a multi-turn loop: per turn, drain stream → if tool_calls,
///    dispatch each → append assistant + tool messages → repeat. Stop on:
///    - empty tool_calls (model done)
///    - `compress_and_store` invoked (capture summary, exit clean)
///    - the agent's `limits.max_tool_rounds` exceeded (return Failed status)
/// 8. Emit one `AgentIpcOutput` JSON line on stdout and exit.
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
    // on disk is unchanged.
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
    // `~` in `workspace` is expanded by every in-process consumer; do the
    // same here so scopes / memory / the engine agree on one absolute path.
    spec.workspace = spec
        .workspace
        .as_ref()
        .map(|p| crate::config::paths::expand_tilde(p));
    if let Some(c) = compose_override {
        spec.skill_packages = c.skills;
        spec.tools = c.tools;
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

    // ----- Compose system prompt: base + skill bodies + suffix -----
    let mut system_prompt = String::from(BASE_AGENT_TEMPLATE);
    for skill_name in &spec.skill_packages {
        match load_skill_body_three_tier(skill_name) {
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
    let workspace = spec
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut agent_cfg_for_engine = crate::bootstrap::tools::subagent_config(&spec);
    // The Claude Code engine ships these scopes to the MCP bridge; the child
    // workspace must be an allowed fs root there too.
    crate::bootstrap::tools::grant_workspace_root(&mut agent_cfg_for_engine.scopes, &workspace);
    let engine = crate::adapters::outbound::engines::build_engine(
        &input.agent_name,
        &agent_cfg_for_engine,
        parent_config.claude_code.as_ref(),
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

    // ----- Build tool stack (Phase 5b) -----
    let secret_registry = std::sync::Arc::new(crate::domain::secrets::SecretRegistry::new());
    let activity: std::sync::Arc<dyn crate::ports::tool_activity::ToolActivityPort> =
        std::sync::Arc::new(SubprocessActivity);
    // Phase 7.6 Bug A — build a real MemoryManager from the parent config so
    // the MemoryPlugin can register persistent_store / memory_ingest as
    // callable handlers (not just advertised tool defs). Without this,
    // MCP-routed Claude Code calls to those tools fail with
    // "Tool 'X' is not available to this agent" even though the tool def is
    // in the advertised list.
    let memory_manager = if parent_config.memory.enabled {
        Some(
            crate::bootstrap::memory::build_memory_manager_async(
                &parent_config.memory,
                Some(&workspace),
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
        workspace: spec.workspace.clone(),
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

    for turn in 0..input.max_turns {
        // Compute the prompt size BEFORE the engine call so the metric
        // record reflects what we sent. UTF-8-aware char count + byte len.
        let prompt_chars: u32 = messages
            .iter()
            .map(|m| m.content.chars().count() as u32)
            .sum();
        let prompt_bytes: u32 = messages.iter().map(|m| m.content.len() as u32).sum();
        let turn_started = std::time::Instant::now();

        let (text, tool_calls, input_delta, output_delta) =
            crate::application::chat::tool_loop::run_single_engine_turn(
                engine.as_ref(),
                &messages,
                &tools,
                &context,
                None,
                stream_event_timeout_secs,
            )
            .await
            .context("engine turn failed")?;

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
            let result = if call.name == "compress_and_store" {
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
                        &parent_config,
                        &input.session_id,
                        &input.step_id,
                        &extracted_summary,
                    )
                    .await;
                }
                "stored".to_string()
            } else if let Some(ref exec) = executor {
                use crate::ports::engine::ToolExecutor;
                match exec.execute(call, &messages).await {
                    Ok(s) => s,
                    Err(e) => format!("tool error: {}", e),
                }
            } else {
                format!("tool '{}' is not available in this subprocess", call.name)
            };

            messages.push(Message {
                role: Role::Tool,
                content: result,
                tool_call_id: Some(call.id.clone()),
                tool_calls: None,
            });
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
        match try_persist_agentic_step_summary(
            &parent_config,
            &input.session_id,
            &input.step_id,
            &summary,
        )
        .await
        {
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
    // The strict-doctrine version of REDESIGN §7 would flip the second row
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
        }
    };
    let json = serde_json::to_string(&out).context("serialise IPC output")?;
    println!("{}", json);
    Ok(())
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

/// Three-tier skill loader: workspace root → workspace dotdir → managed
/// (~/.tengu/skills). Returns the SKILL.md body with frontmatter stripped,
/// from the FIRST tier that has the file (highest precedence wins).
fn load_skill_body_three_tier(name: &str) -> Option<String> {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        std::path::PathBuf::from("skills")
            .join(name)
            .join("SKILL.md"),
        std::path::PathBuf::from(".tengu")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
    ];
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(
            home.join(".tengu")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
    }

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
