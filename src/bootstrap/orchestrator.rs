//! Orchestrator wiring — the `ChatServiceFactory` that runs one agent turn,
//! the per-channel snapshot bridge, session-id resolution, and
//! `build_orchestrator` (RagPlanner + SubprocessRunner + ports).

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapters::outbound::memory::embedder::Embedder;
use crate::application::chat::service::create_chat_loop_state;
use crate::config::{AgentConfig, McpServerConfig};
// Plugin set: `outbound::tools::catalog`. McpPlugin + SkillPlugin are
// registered here, outside the catalog — they need the config's
// `[[mcp_servers]]` / a skill registry.
use crate::domain::message::{ToolCall, ToolDef};

// ---------------------------------------------------------------------------
// ChatServiceFactory — per-agent, per-call ChatRuntimeService construction
// ---------------------------------------------------------------------------
//
// `ChatRuntimeService<'a>` borrows most of its fields (`engine`, `agent_config`,
// `tools`, ...) so it cannot live behind `Arc<dyn ChatServiceFactory>` on its
// own. The factory here owns `Arc`-held snapshots of everything a service
// needs and rebuilds one service per `run_turn` call. Channels construct the
// factory once at startup by supplying an `inputs_fn` closure that knows how
// to produce per-agent inputs (the same logic they currently run inline when
// building a `ChatRuntimeService`).
//
// This is the escape hatch called out in Task 5.2: per-channel fidelity
// (`tool_observer`, `cancel`, channel-specific tool wrapping, multi-agent
// activity context) is preserved because each channel supplies its own
// `inputs_fn`. The generic helper `build_orchestrator` below only needs the
// factory trait, not the channel-specific shape.

use async_trait::async_trait;

use crate::application::chat::service::ChatRuntimeService;
use crate::application::chat::tool_loop::ToolResultObserver;
use crate::application::memory::manager::MemoryManager;
use crate::application::orchestrator::planner::RagPlanner;
use crate::application::orchestrator::retry::RetryPolicy;
use crate::config::Config;
use crate::ports::engine::ToolExecutor;
use crate::ports::orchestration::Planner;
// Phase 7.1 (full) — `OrchestratorAgentPlanner`, `ChatWorker`, and the
// `render_roster` helper were deleted along with the static-mode path.
// `build_orchestrator` now constructs only `RagPlanner` + `SubprocessRunner`.
use crate::application::orchestrator::wiring::ChatOrchestratorPortImpl;
use crate::application::orchestrator::Orchestrator;
use crate::domain::session::FlowCompactionPolicy;
use crate::ports::engine::Engine;
use crate::ports::orchestration::ChatServiceFactory;

/// Owned snapshot of the inputs a `ChatRuntimeService<'a>` needs for a single
/// turn. `inputs_fn` closures produce one of these per call; the factory then
/// borrows into it to build the service for `process_user_text`.
///
/// Keeping this owned (`Arc`, `String`, `Vec`, `Box<dyn Fn ...>`) sidesteps
/// the lifetime problem discovered in Task 5.1: `ChatRuntimeService<'a>` has
/// an `'a` lifetime, so it must be constructed *inside* `run_turn` where the
/// borrow can be rooted in a stack-local `ChatTurnInputs`.
pub(crate) struct ChatTurnInputs {
    pub engine: Arc<dyn Engine>,
    pub agent_id: String,
    pub agent_config: Arc<AgentConfig>,
    pub history_turn_limit: usize,
    pub compaction_policy: FlowCompactionPolicy,
    pub system_prompt: String,
    pub tools: Vec<ToolDef>,
    pub tool_executor: Option<Arc<dyn ToolExecutor>>,
    pub memory_manager: Option<Arc<MemoryManager>>,
    pub max_recall_entries: usize,
    pub max_recall_tokens: usize,
    pub bridge_tools: Option<Vec<ToolDef>>,
    /// `[[mcp_servers]]` behind `{server}__{tool}` bridge entries.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Optional per-turn callback invoked after each tool executes.
    /// Boxed so channels can close over their own event channels.
    pub tool_observer: Option<Arc<dyn Fn(&ToolCall, &str) + Send + Sync>>,
    /// Optional cancellation flag (shared). Cloned into each turn by the
    /// channel; the factory just borrows from it.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Closure that produces the per-turn inputs for a named agent.
///
/// Each channel (TUI, Telegram, CLI orchestrator) supplies its own closure so
/// channel-specific details (secret-redacting tool executor wrapper,
/// tool-activity observer, typing-indicator cancel flag, multi-agent
/// activity context appended to the system prompt) are preserved. A closure
/// is used instead of a second trait to keep the factory construction
/// site-local and avoid leaking channel internals into the factory API.
pub(crate) type ChatInputsFn = Arc<dyn Fn(&str) -> anyhow::Result<ChatTurnInputs> + Send + Sync>;

/// Concrete `ChatServiceFactory` used by the orchestrator wiring.
///
/// Cheap to clone behind an `Arc`. All state is immutable after construction;
/// per-call variability lives in `inputs_fn`.
pub(crate) struct RuntimeChatServiceFactory {
    inputs_fn: ChatInputsFn,
}

impl RuntimeChatServiceFactory {
    pub(crate) fn new(inputs_fn: ChatInputsFn) -> Self {
        Self { inputs_fn }
    }
}

// ---------------------------------------------------------------------------
// OrchestratorSnapshots — shared state bridge for channel factory closures.
// ---------------------------------------------------------------------------
//
// Channels (Telegram, TUI) hold per-agent runtime state that mutates between
// messages (skill hot-reload + per-turn system-prompt tweaks). The orchestrator
// holds an `Arc<dyn ChatServiceFactory>` whose `run_turn` method is spawned
// into a tokio task, so the factory cannot borrow channel-local state.
//
// `OrchestratorSnapshots` is an `Arc<RwLock<HashMap<String, ChatTurnInputs>>>`
// shared between the channel and the factory. Before each `orchestrator.handle`
// call the channel writes a fresh snapshot for each agent; the factory closure
// reads back from the same map. This keeps the factory `'static` while letting
// per-message state (hot-reloaded tools, current system prompt, per-turn
// tool_observer / cancel) flow in through owned clones.
pub(crate) type OrchestratorSnapshots = Arc<std::sync::RwLock<HashMap<String, ChatTurnInputs>>>;

/// Build a `ChatInputsFn` closure that resolves agent inputs from an
/// `OrchestratorSnapshots` table. Returns a cheap error when the agent is not
/// present — the orchestrator surfaces this as a step failure which triggers a
/// replan.
pub(crate) fn snapshots_inputs_fn(state: OrchestratorSnapshots) -> ChatInputsFn {
    Arc::new(move |agent: &str| {
        let guard = state
            .read()
            .map_err(|e| anyhow::anyhow!("orchestrator snapshot lock poisoned: {}", e))?;
        let snap = guard.get(agent).ok_or_else(|| {
            anyhow::anyhow!(
                "orchestrator snapshot missing for agent '{}' (populate snapshots before handle())",
                agent
            )
        })?;
        Ok(clone_chat_turn_inputs(snap))
    })
}

/// Clone a `ChatTurnInputs` by cloning Arcs and owned fields.
/// `Arc<dyn Fn>` / `Arc<dyn ToolExecutor>` / `Arc<dyn Engine>` all clone cheaply.
fn clone_chat_turn_inputs(src: &ChatTurnInputs) -> ChatTurnInputs {
    ChatTurnInputs {
        engine: Arc::clone(&src.engine),
        agent_id: src.agent_id.clone(),
        agent_config: Arc::clone(&src.agent_config),
        history_turn_limit: src.history_turn_limit,
        compaction_policy: src.compaction_policy,
        system_prompt: src.system_prompt.clone(),
        tools: src.tools.clone(),
        tool_executor: src.tool_executor.clone(),
        memory_manager: src.memory_manager.clone(),
        max_recall_entries: src.max_recall_entries,
        max_recall_tokens: src.max_recall_tokens,
        bridge_tools: src.bridge_tools.clone(),
        mcp_servers: src.mcp_servers.clone(),
        tool_observer: src.tool_observer.clone(),
        cancel: src.cancel.clone(),
    }
}

#[async_trait]
impl ChatServiceFactory for RuntimeChatServiceFactory {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String> {
        let (reply, _) = self.run_turn_inner(agent, None, text).await?;
        Ok(reply)
    }

    async fn run_turn_with_system(
        &self,
        agent: &str,
        system_prompt: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        let (reply, _) = self
            .run_turn_inner(agent, Some(system_prompt), text)
            .await?;
        Ok(reply)
    }

    async fn run_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: Option<&str>,
        text: &str,
    ) -> anyhow::Result<(String, crate::ports::orchestration::TurnTelemetry)> {
        self.run_turn_inner(agent, system_prompt, text).await
    }
}

impl RuntimeChatServiceFactory {
    /// Shared body for `run_turn` and `run_turn_with_system`. When
    /// `system_override` is `Some`, it replaces `inputs.system_prompt` for
    /// this single call (Phase 4c — used by the RAG planner to inject
    /// `skills/orchestrator/SKILL.md` instead of the agent's identity).
    async fn run_turn_inner(
        &self,
        agent: &str,
        system_override: Option<&str>,
        text: &str,
    ) -> anyhow::Result<(String, crate::ports::orchestration::TurnTelemetry)> {
        let inputs = (self.inputs_fn)(agent)?;
        let model_slug = inputs.agent_config.model.clone();
        let started = std::time::Instant::now();

        // Wrap tool_observer Arc into the `&dyn Fn` form the service expects.
        let observer_arc = inputs.tool_observer.clone();
        let observer_ref: Option<ToolResultObserver<'_>> =
            observer_arc.as_deref().map(|f| f as ToolResultObserver<'_>);

        let (system_prompt, tools_slice) = match system_override {
            Some(s) => {
                // Phase 4c: a system-prompt override means this is a planner call.
                // Strip tools entirely — we don't want the LLM dispatching
                // http_request etc. when its job is to emit plan JSON. Memory
                // injection is also disabled by passing memory_manager: None.
                (s.to_string(), &[][..])
            }
            None => (inputs.system_prompt.clone(), inputs.tools.as_slice()),
        };

        let service = ChatRuntimeService {
            engine: inputs.engine.as_ref(),
            agent_id: &inputs.agent_id,
            agent_config: inputs.agent_config.as_ref(),
            history_turn_limit: inputs.history_turn_limit,
            compaction_policy: inputs.compaction_policy,
            system_prompt,
            tools: tools_slice,
            tool_executor: if system_override.is_some() {
                None
            } else {
                inputs
                    .tool_executor
                    .as_deref()
                    .map(|e| e as &dyn ToolExecutor)
            },
            memory_manager: if system_override.is_some() {
                None
            } else {
                inputs.memory_manager.as_deref()
            },
            max_recall_entries: inputs.max_recall_entries,
            max_recall_tokens: inputs.max_recall_tokens,
            tool_observer: observer_ref,
            cancel: inputs.cancel.as_deref(),
            bridge_tools: inputs.bridge_tools.as_deref(),
            mcp_servers: &inputs.mcp_servers,
            suppress_grounding_nudge: system_override.is_some(),
        };

        let mut state = create_chat_loop_state(inputs.agent_config.as_ref());
        let result = service.process_user_text(&mut state, text).await?;
        // Per §5.1 note: an empty assistant response (tool-only turn, budget
        // exhaustion notice) degrades to an empty string; the caller can
        // inspect `system_notice` via a dedicated path when that matters.
        // The orchestrator always wants *some* string to feed back into the
        // next step.
        let reply = result.assistant_text.unwrap_or_default();
        let telemetry = crate::ports::orchestration::TurnTelemetry {
            // `process_user_text` writes per-turn deltas onto state.total_*;
            // we read them here so the value reflects only this turn (the
            // caller mints a fresh ChatLoopState per call).
            prompt_tokens: state.total_input_tokens,
            completion_tokens: state.total_output_tokens,
            model: model_slug,
            latency_ms: started.elapsed().as_millis() as u64,
            response_chars: reply.chars().count() as u32,
        };
        Ok((reply, telemetry))
    }
}

/// Build the harness-owned `Orchestrator` from config + factory + memory.
///
/// Phase 7.1 (full) — only one path remains: `RagPlanner` + `SubprocessRunner`.
/// Returns `None` when:
///   - `config.orchestrator` is absent (orchestration disabled — channels
///     fall back to direct default-agent dispatch).
///
/// `cfg.engine` is left in the config schema for forward-compatibility (a
/// future engine variant could land here without a config break) but the
/// only currently-supported value is `"rag"`. Anything else logs a warning
/// and disables orchestration for that channel.
/// Resolve the planner / runner `session_id` from env-or-fresh.
///
/// Reused across surfaces — `tengu chat` and `tengu telegram` resolve
/// once per process at startup; `tengu webhooks` resolves per request
/// (so each inbound POST has its own recall key). Empty / whitespace-only
/// `TENGU_SESSION_ID` falls through to a fresh UUID so callers don't
/// need to defend against malformed env input.
pub fn resolve_session_id() -> String {
    std::env::var("TENGU_SESSION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// The planner's durable-memory lanes: Postgres `agentic_memory` when built
/// with `postgres_memory`, otherwise none (those prompt blocks stay empty).
fn planner_recall_store() -> Option<Arc<dyn crate::ports::memory::RecallStore>> {
    #[cfg(feature = "postgres_memory")]
    {
        Some(Arc::new(
            crate::adapters::outbound::tools::agentic_memory::AgenticRecallStore,
        ))
    }
    #[cfg(not(feature = "postgres_memory"))]
    {
        None
    }
}

/// Embedder for the planner's recall lanes — only when they exist and
/// `OPENROUTER_API_KEY` is set.
fn planner_embedder(
    memory: &crate::config::MemoryConfig,
) -> Option<Arc<dyn crate::ports::memory::Embedding>> {
    if !cfg!(feature = "postgres_memory") {
        return None;
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").ok()?;
    Some(Arc::new(Embedder::new(
        api_key,
        memory.embedding_model.clone(),
    )))
}

pub(crate) fn build_orchestrator(
    config: &Config,
    chat_factory: Arc<dyn ChatServiceFactory>,
    memory: Arc<MemoryManager>,
    session_id: String,
) -> Option<Orchestrator> {
    let cfg = config.orchestrator.as_ref()?;

    if cfg.engine != "rag" {
        tracing::warn!(
            engine = %cfg.engine,
            "[orchestrator] engine != \"rag\" — only the rag engine remains since Phase 7.1; \
             orchestration disabled for this channel"
        );
        return None;
    }

    let chat_port = Arc::new(ChatOrchestratorPortImpl::new(
        Arc::clone(&chat_factory),
        Arc::clone(&memory),
    ));

    // Phase 6.1 (full) — mint the orchestrator event bus *before* the
    // planner so we can hand the same bus to both. RagPlanner emits
    // `OrchestratorEvent::RagQueried` on it; Orchestrator emits
    // `PlanCreated`/`StepStarted`/etc. on the same channel; subscribers
    // (TUI, Telegram adapter) see one unified event stream.
    let bus = crate::application::orchestrator::events::new_bus();

    // Metrics — install the process-global metrics sink and bridge it
    // onto the orchestrator event bus so a single subscriber can render
    // PlanCreated / RagQueried / MetricsRecorded uniformly. Idempotent;
    // subsequent `build_orchestrator` calls reuse the already-installed
    // sink. Bridge task lives as long as the metrics sink exists.
    let metrics_tx = crate::application::metrics::install_global_sink();
    {
        let mut metrics_rx = metrics_tx.subscribe();
        let bus_tx = bus.clone();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match metrics_rx.recv().await {
                    Ok(record) => {
                        // `bus.send` returns Err only when zero
                        // subscribers — fine, drop and keep listening.
                        let _ = bus_tx.send(
                            crate::application::orchestrator::events::OrchestratorEvent::MetricsRecorded {
                                record,
                            },
                        );
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "metrics sink subscriber lagged");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    // Fix B (2026-05-09) — session_id is resolved by the caller and
    // passed in. Single value shared between RagPlanner (recall reads)
    // and SubprocessRunner (compress_and_store writes via IPC).
    // Logged at info-level so users can stitch tracing output back
    // to a specific persisted thread.
    tracing::info!(
        session_id = %session_id,
        sandbox = ?config.sandbox_name,
        "orchestrator: engine=rag, planner=RagPlanner(file-registry), worker=SubprocessRunner"
    );
    let worker: Arc<dyn crate::ports::orchestration::WorkerHandle> = Arc::new(
        crate::adapters::outbound::subprocess_runner::SubprocessRunner::new(
            config.sandbox_name.clone(),
            session_id.clone(),
            config.agents.clone(),
        ),
    );
    let planner: Arc<dyn Planner> = Arc::new(RagPlanner::new(
        cfg.agent.clone(),
        chat_port,
        config.memory.clone(),
        crate::application::orchestrator::shared_files::routable_agents(&config.agents),
        Arc::new(crate::adapters::outbound::tools::CatalogDirectory {
            mcp_servers: config.mcp_servers.clone(),
        }),
        planner_recall_store(),
        planner_embedder(&config.memory),
        Some(bus.clone()),
        session_id,
    ));

    let policy = RetryPolicy::new(cfg.max_attempts_per_step);
    Some(Orchestrator::new(
        planner,
        worker,
        policy,
        cfg.max_replans,
        memory,
        bus,
    ))
}

/// Build a minimal `ChatServiceFactory` suitable for CLI commands that need to
/// dispatch a turn against a named agent (e.g. `tengu skill evolve` targeting
/// the skill-improver agent).
///
/// Pre-populates an `OrchestratorSnapshots` table with one `ChatTurnInputs` per
/// agent in `config.agents`, then wraps it in a `RuntimeChatServiceFactory`.
/// Tool execution is intentionally omitted — the skill-improver only needs the
/// engine + memory for text generation; workspace tools can be added later.
pub(crate) async fn build_cli_chat_factory(
    config: &Config,
    workspace: &std::path::Path,
) -> anyhow::Result<Arc<dyn ChatServiceFactory>> {
    use crate::adapters::outbound::engines::build_engine;
    use crate::application::memory::manager::MemoryManager;

    // Build a shared memory manager (no vector backend for CLI — acceptable
    // degradation; the improver only needs text generation context).
    let memory: Arc<MemoryManager> = Arc::new(MemoryManager::new());

    let snapshots: OrchestratorSnapshots =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    for (name, agent_cfg) in &config.agents {
        let engine_box = build_engine(name, agent_cfg, config.claude_code.as_ref())?;
        let compaction_policy = crate::application::chat::flow::resolve_flow_compaction_policy(
            &agent_cfg.flow,
            agent_cfg.limits.max_tokens_per_flow,
            engine_box.context_window(),
            engine_box.max_output_tokens_per_turn() as usize,
        );
        let engine: Arc<dyn Engine> = Arc::from(engine_box);
        let system_prompt =
            crate::application::skills::registry::build_system_prompt(agent_cfg, false, &[]);
        let history_turn_limit =
            crate::application::chat::flow::resolve_history_turn_limit(&agent_cfg.flow);
        let inputs = ChatTurnInputs {
            engine,
            agent_id: name.clone(),
            agent_config: Arc::new(agent_cfg.clone()),
            history_turn_limit,
            compaction_policy,
            system_prompt,
            tools: Vec::new(),
            tool_executor: None,
            memory_manager: Some(Arc::clone(&memory)),
            max_recall_entries: 10,
            max_recall_tokens: 2000,
            bridge_tools: None,
            mcp_servers: Vec::new(),
            tool_observer: None,
            cancel: None,
        };
        snapshots
            .write()
            .map_err(|e| anyhow::anyhow!("snapshots lock poisoned: {e}"))?
            .insert(name.clone(), inputs);
    }

    let _ = workspace; // workspace available for future tool wiring
    let inputs_fn = snapshots_inputs_fn(snapshots);
    Ok(Arc::new(RuntimeChatServiceFactory::new(inputs_fn)))
}
