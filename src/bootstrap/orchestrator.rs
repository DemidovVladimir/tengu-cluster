//! Orchestrator wiring — `planner_llm` (the planner's engine from
//! `[orchestrator] agent`), `build_orchestrator` (RegistryPlanner +
//! SubprocessRunner), session-id resolution, and the `ChatServiceFactory`
//! a CLI command uses to run one agent turn (`build_cli_chat_factory`).

use std::sync::Arc;

use crate::adapters::outbound::memory::embedder::Embedder;
use crate::application::chat::service::create_chat_loop_state;
use crate::config::{AgentConfig, McpServerConfig};
// Plugin set: `outbound::tools::catalog`. McpPlugin + SkillPlugin are
// registered here, outside the catalog — they need the config's
// `[[mcp_servers]]` / a skill registry.
use crate::domain::message::{ToolCall, ToolDef};

// ---------------------------------------------------------------------------
// ChatServiceFactory — one agent turn for a CLI command
// ---------------------------------------------------------------------------
//
// `ChatRuntimeService<'a>` borrows most of its fields, so the factory owns a
// `ChatTurnInputs` per agent and builds a service per `run_turn` call.

use async_trait::async_trait;

use crate::application::chat::service::ChatRuntimeService;
use crate::application::chat::tool_loop::ToolResultObserver;
use crate::application::memory::manager::MemoryManager;
use crate::application::orchestrator::planner::{PlannerLlm, RegistryPlanner};
use crate::application::orchestrator::retry::RetryPolicy;
use crate::application::orchestrator::Orchestrator;
use crate::config::Config;
use crate::domain::session::FlowCompactionPolicy;
use crate::ports::engine::Engine;
use crate::ports::engine::ToolExecutor;
use crate::ports::orchestration::ChatServiceFactory;
use crate::ports::orchestration::Planner;

/// Owned snapshot of the inputs a `ChatRuntimeService<'a>` needs for a single
/// turn. `inputs_fn` closures produce one of these per call; the factory then
/// borrows into it to build the service for `process_user_text`.
///
/// Keeping this owned (`Arc`, `String`, `Vec`, `Box<dyn Fn ...>`) sidesteps
/// the lifetime problem discovered in Task 5.1: `ChatRuntimeService<'a>` has
/// an `'a` lifetime, so it must be constructed *inside* `run_turn` where the
/// borrow can be rooted in a stack-local `ChatTurnInputs`.
#[derive(Clone)]
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
pub(crate) type ChatInputsFn = Arc<dyn Fn(&str) -> anyhow::Result<ChatTurnInputs> + Send + Sync>;

/// The `ChatServiceFactory` of `build_cli_chat_factory`: per-call inputs
/// from `inputs_fn`.
pub(crate) struct RuntimeChatServiceFactory {
    inputs_fn: ChatInputsFn,
}

impl RuntimeChatServiceFactory {
    pub(crate) fn new(inputs_fn: ChatInputsFn) -> Self {
        Self { inputs_fn }
    }
}

#[async_trait]
impl ChatServiceFactory for RuntimeChatServiceFactory {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String> {
        let inputs = (self.inputs_fn)(agent)?;
        let observer_arc = inputs.tool_observer.clone();
        let observer_ref: Option<ToolResultObserver<'_>> =
            observer_arc.as_deref().map(|f| f as ToolResultObserver<'_>);
        let service = ChatRuntimeService {
            engine: inputs.engine.as_ref(),
            agent_id: &inputs.agent_id,
            agent_config: inputs.agent_config.as_ref(),
            history_turn_limit: inputs.history_turn_limit,
            compaction_policy: inputs.compaction_policy,
            system_prompt: inputs.system_prompt.clone(),
            tools: inputs.tools.as_slice(),
            tool_executor: inputs
                .tool_executor
                .as_deref()
                .map(|e| e as &dyn ToolExecutor),
            memory_manager: inputs.memory_manager.as_deref(),
            max_recall_entries: inputs.max_recall_entries,
            max_recall_tokens: inputs.max_recall_tokens,
            tool_observer: observer_ref,
            cancel: inputs.cancel.as_deref(),
            bridge_tools: inputs.bridge_tools.as_deref(),
            mcp_servers: &inputs.mcp_servers,
        };
        let mut state = create_chat_loop_state();
        let result = service.process_user_text(&mut state, text).await?;
        // An empty assistant response (tool-only turn, budget notice) is an
        // empty string: the caller always wants *some* string.
        Ok(result.assistant_text.unwrap_or_default())
    }
}

/// The planner's LLM call for `config` (`PlannerLlm`): the engine of
/// `[agents.<orchestrator.agent>]`. `Ok(None)` without `[orchestrator]`; an
/// error when that agent has no block or its engine does not build.
pub(crate) fn planner_llm(config: &Config) -> anyhow::Result<Option<PlannerLlm>> {
    let Some(cfg) = config.orchestrator.as_ref() else {
        return Ok(None);
    };
    let agent = config.agents.get(&cfg.agent).ok_or_else(|| {
        anyhow::anyhow!(
            "[orchestrator] agent `{}` has no [agents.{0}] block",
            cfg.agent
        )
    })?;
    let engine = crate::adapters::outbound::engines::build_engine(
        &cfg.agent,
        agent,
        config.claude_code.as_ref(),
    )?;
    Ok(Some(PlannerLlm {
        engine: Arc::from(engine),
        agent: Arc::new(agent.clone()),
    }))
}

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
fn planner_recall_store(sandbox: String) -> Option<Arc<dyn crate::ports::memory::RecallStore>> {
    #[cfg(feature = "postgres_memory")]
    {
        Some(Arc::new(
            crate::adapters::outbound::tools::agentic_memory::AgenticRecallStore { sandbox },
        ))
    }
    #[cfg(not(feature = "postgres_memory"))]
    {
        let _ = sandbox;
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
    let (model, warning) = open_brain_embedding_model(&memory.embedding_model);
    if let Some(w) = warning {
        tracing::warn!("{w}");
    }
    Some(Arc::new(Embedder::new(api_key, model.to_string())))
}

/// The model the planner embeds Open Brain recall with: always the one the
/// Postgres schema is sized for (`vector(1536)`, `DEFAULT_EMBEDDING_MODEL`),
/// like the `agentic_memory` tool. `[memory] embedding_model` stays the
/// workspace vector store's; when it differs, a warning says so (another
/// model here would make every planner recall text-only).
fn open_brain_embedding_model(configured: &str) -> (&'static str, Option<String>) {
    let pinned = crate::domain::memory::DEFAULT_EMBEDDING_MODEL;
    let warning = (configured != pinned).then(|| {
        format!(
            "[memory] embedding_model = {configured} is used for workspace memory only; \
             Open Brain recall uses {pinned} (the Postgres schema is vector(1536))"
        )
    });
    (pinned, warning)
}

/// The harness-owned `Orchestrator`: `RegistryPlanner` (calling `llm`, from
/// [`planner_llm`]) + `SubprocessRunner`. `None` without `[orchestrator]`
/// (channels fall back to the default agent) or when its `engine` is not
/// `"rag"` (warned). `trace` = the recording its plans and steps go to
/// (`application::orchestrator::trace`: `plan.*`, `step.*`,
/// `metrics.recorded`, and each step's `run-agent` `tool.*` events); `None`
/// records nothing.
pub(crate) fn build_orchestrator(
    config: &Config,
    llm: PlannerLlm,
    session_id: String,
    trace: Option<crate::application::orchestrator::trace::OrchestratorTrace>,
) -> Option<Orchestrator> {
    let cfg = config.orchestrator.as_ref()?;

    // The orchestrator event bus: the Orchestrator emits
    // `PlanCreated`/`StepStarted`/etc. on it and the metrics forwarder
    // below adds `MetricsRecorded`; subscribers (TUI, eval, the trace
    // bridge) see one unified event stream.
    let step_sink = trace.as_ref().map(|t| Arc::clone(&t.sink));
    let bus = match trace {
        Some(t) => crate::application::orchestrator::events::new_bus().traced(
            crate::application::orchestrator::trace::TraceBridge::new(t, session_id.clone()),
        ),
        None => crate::application::orchestrator::events::new_bus(),
    };

    // Metrics — install the process-global metrics sink and bridge it
    // onto the orchestrator event bus so a single subscriber can render
    // PlanCreated / StepStarted / MetricsRecorded uniformly. Idempotent;
    // subsequent `build_orchestrator` calls reuse the already-installed
    // sink. The forwarder holds a weak bus handle and ends with the
    // orchestrator (it owns the barrier; a webhook turn's orchestrator used
    // to keep its forwarder, and the bus, alive forever).
    // `Orchestrator::flush_metrics` is the barrier a surface awaits before
    // it closes its trace (an eval row, a webhook turn).
    let metrics = {
        let weak_bus = bus.downgrade();
        crate::application::metrics::forward_sink(move |record| {
            let Some(bus_tx) = weak_bus.upgrade() else {
                return false;
            };
            // `bus.send` returns Err only when zero subscribers — fine,
            // the trace bridge has recorded it; keep listening.
            let _ = bus_tx.send(
                crate::application::orchestrator::events::OrchestratorEvent::MetricsRecorded {
                    record,
                },
            );
            true
        })
    };

    // session_id is resolved by the caller and passed in: one value shared
    // between RegistryPlanner (recall reads) and SubprocessRunner
    // (compress_and_store writes via IPC).
    // Logged at info-level so users can stitch tracing output back
    // to a specific persisted thread.
    tracing::info!(
        session_id = %session_id,
        sandbox = ?config.sandbox_name,
        "orchestrator: engine=rag, planner=RegistryPlanner(file-registry), worker=SubprocessRunner"
    );
    let worker: Arc<dyn crate::ports::orchestration::WorkerHandle> = Arc::new(
        crate::adapters::outbound::subprocess_runner::SubprocessRunner::new(
            config.sandbox_name.clone(),
            session_id.clone(),
            config.agents.clone(),
        )
        .with_trace(step_sink),
    );
    let planner: Arc<dyn Planner> = Arc::new(RegistryPlanner::new(
        cfg.agent.clone(),
        llm,
        planner_prompt(),
        config.memory.clone(),
        crate::application::orchestrator::shared_files::routable_agents(&config.agents),
        Arc::new(crate::adapters::outbound::tools::CatalogDirectory {
            mcp_servers: config.mcp_servers.clone(),
        }),
        planner_recall_store(config.sandbox_owner()),
        planner_embedder(&config.memory),
        session_id,
    ));

    let policy = RetryPolicy::new(cfg.max_attempts_per_step);
    Some(Orchestrator::new(planner, worker, policy, cfg.max_replans, bus).with_metrics(metrics))
}

/// The planner's system prompt: the body of `skills/orchestrator/SKILL.md`
/// (cwd-relative, after its YAML frontmatter), else the inline fallback.
fn planner_prompt() -> String {
    const PATH: &str = "skills/orchestrator/SKILL.md";
    match std::fs::read_to_string(PATH) {
        Ok(content) => skill_body(&content).to_string(),
        Err(e) => {
            tracing::warn!(
                path = PATH,
                error = %e,
                "planner skill unreadable; using the fallback inline planner prompt"
            );
            crate::application::orchestrator::planner::FALLBACK_PLANNER_PROMPT.to_string()
        }
    }
}

/// `content` after its `---` frontmatter (the whole text when it has none).
fn skill_body(content: &str) -> &str {
    let Some(after_first) = content.strip_prefix("---") else {
        return content;
    };
    match after_first.find("\n---") {
        Some(end) => {
            let rest = &after_first[end + 4..];
            rest.strip_prefix('\n').unwrap_or(rest).trim_start()
        }
        None => content,
    }
}

/// Build a minimal `ChatServiceFactory` suitable for CLI commands that need to
/// dispatch a turn against a named agent (e.g. `tengu skill evolve` targeting
/// the skill-improver agent).
///
/// Each call builds the named agent's text-only inputs (`text_only_inputs`).
/// Tool execution is intentionally omitted — the skill-improver only needs the
/// engine + memory for text generation; workspace tools can be added later.
pub(crate) async fn build_cli_chat_factory(
    config: &Config,
    workspace: &std::path::Path,
) -> anyhow::Result<Arc<dyn ChatServiceFactory>> {
    // An empty memory manager (no vector backend for CLI — the improver only
    // needs text generation). Each agent's engine is built when it is asked
    // for: an agent the command never calls cannot fail it.
    let memory: Arc<MemoryManager> = Arc::new(MemoryManager::new());
    let config = config.clone();
    let _ = workspace; // workspace available for future tool wiring
    let inputs_fn: ChatInputsFn = Arc::new(move |name: &str| {
        let agent_cfg = config
            .agents
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no [agents.{name}] block"))?;
        text_only_inputs(&config, name, agent_cfg, Some(Arc::clone(&memory)))
    });
    Ok(Arc::new(RuntimeChatServiceFactory::new(inputs_fn)))
}

/// Turn inputs for `[agents.<name>]` with its own engine and system prompt
/// but no tools — enough for a text turn such as the skill improver's.
pub(crate) fn text_only_inputs(
    config: &Config,
    name: &str,
    agent_cfg: &AgentConfig,
    memory: Option<Arc<MemoryManager>>,
) -> anyhow::Result<ChatTurnInputs> {
    let engine_box = crate::adapters::outbound::engines::build_engine(
        name,
        agent_cfg,
        config.claude_code.as_ref(),
    )?;
    let compaction_policy = crate::application::chat::flow::resolve_flow_compaction_policy(
        &agent_cfg.flow,
        agent_cfg.limits.max_tokens_per_flow,
        engine_box.context_window(),
        engine_box.max_output_tokens_per_turn() as usize,
    );
    let engine: Arc<dyn Engine> = Arc::from(engine_box);
    Ok(ChatTurnInputs {
        engine,
        agent_id: name.to_string(),
        agent_config: Arc::new(agent_cfg.clone()),
        history_turn_limit: crate::application::chat::flow::resolve_history_turn_limit(
            &agent_cfg.flow,
        ),
        compaction_policy,
        system_prompt: crate::application::skills::registry::build_system_prompt(agent_cfg, &[]),
        tools: Vec::new(),
        tool_executor: None,
        memory_manager: memory,
        max_recall_entries: config.memory.max_recall_entries,
        max_recall_tokens: config.memory.max_recall_tokens,
        bridge_tools: None,
        mcp_servers: Vec::new(),
        tool_observer: None,
        cancel: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_brain_recall_always_embeds_with_the_pinned_model() {
        let pinned = crate::domain::memory::DEFAULT_EMBEDDING_MODEL;
        assert_eq!(open_brain_embedding_model(pinned), (pinned, None));
        let (model, warning) = open_brain_embedding_model("text-embedding-3-large");
        assert_eq!(model, pinned);
        assert!(warning.unwrap().contains("workspace memory only"));
    }

    #[test]
    fn planner_prompt_is_the_skill_body_after_its_frontmatter() {
        let skill = "---\nname: orchestrator\ndescription: plans\n---\n\n# Planner\nEmit JSON.\n";
        assert_eq!(skill_body(skill), "# Planner\nEmit JSON.\n");
        assert_eq!(skill_body("# No frontmatter\n"), "# No frontmatter\n");
        // An unclosed frontmatter keeps the whole text.
        assert_eq!(skill_body("---\nname: x\n"), "---\nname: x\n");
    }
}
