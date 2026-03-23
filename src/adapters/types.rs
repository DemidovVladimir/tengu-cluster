//! Core types shared by all Tengu components.
//!
//! Consolidates all shared data types, traits, and enums: messages, engine
//! contracts, capability/tool types, orchestration events, agent roles,
//! task/plan state machines, memory types, and chat session state.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Role of a chat message passed to model engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
}

/// Normalized chat message exchanged with engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Message role.
    pub role: Role,
    /// Message text content.
    pub content: String,
    /// Optional tool-call ID when responding to a tool invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional model-generated tool calls attached to the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Tool call emitted by a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique tool call identifier.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON arguments for the tool.
    pub arguments: serde_json::Value,
}

/// Tool definition exposed to model providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema of accepted parameters.
    pub parameters: serde_json::Value,
}

/// Provider/model metadata published to runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Provider-native model ID.
    pub id: String,
    /// Provider name.
    pub provider: String,
    /// Display name suitable for CLI/status output.
    pub display_name: String,
    /// Model context window in tokens.
    pub context_window: usize,
    /// Whether tool calls are supported.
    pub supports_tools: bool,
    /// Whether streaming responses are supported.
    pub supports_streaming: bool,
}

/// Channel-recipient identity used by pipes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    /// Pipe identifier (for example: `cli`, `telegram`).
    pub pipe_id: String,
    /// Peer/user/channel identifier.
    pub peer_id: String,
    /// Optional account/server identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Optional thread/group identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Inbound message envelope emitted by pipes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Sender identity.
    pub sender: Recipient,
    /// Text payload.
    pub content: String,
    /// Inbound timestamp.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional attached media payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<MediaPayload>>,
}

/// Raw media payload attached to an inbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPayload {
    /// MIME type of the payload.
    pub mime_type: String,
    /// Raw media bytes.
    pub data: Vec<u8>,
    /// Optional original filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Delivery options for outbound messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeliveryOptions {
    /// Optional message ID to reply to.
    pub reply_to_message_id: Option<String>,
    /// Optional parse/render mode defined by the target pipe.
    pub parse_mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

/// Incremental events produced during model generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Text delta chunk from the model.
    TextDelta { text: String },

    /// Start of a tool call emitted by the model.
    ToolCallStart { id: String, name: String },

    /// Incremental tool-call argument payload.
    ToolCallDelta { id: String, arguments_delta: String },

    /// End of the current tool call.
    ToolCallEnd { id: String },

    /// Optional thinking/reasoning text chunk.
    ThinkingDelta { text: String },

    /// Usage accounting snapshot for the current turn.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },

    /// Terminal event indicating successful completion.
    Done,

    /// Terminal event indicating generation failure.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Engine — the AI backend powering an agent
// ---------------------------------------------------------------------------

/// Runtime-discoverable engine capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub context_window: usize,
    pub max_output_tokens_per_turn: u32,
    pub supports_tool_use: bool,
    pub supports_streaming: bool,
    pub manages_own_workspace: bool,
}

/// Runtime diagnostics metadata surfaced by engines for status/doctor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDiagnostics {
    pub engine_id: String,
    pub configured_model: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub capabilities: EngineCapabilities,
}

pub struct EngineContext {
    #[allow(dead_code)] // Set by adapters, read by future engine impls.
    pub workspace: Option<std::path::PathBuf>,
    pub system_prompt: Option<String>,
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn max_output_tokens_per_turn(&self) -> u32 {
        ((self.context_window() / 8).clamp(256, 16_384)) as u32
    }
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    fn supports_streaming(&self) -> bool {
        false
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            context_window: self.context_window(),
            max_output_tokens_per_turn: self.max_output_tokens_per_turn(),
            supports_tool_use: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
            manages_own_workspace: self.manages_own_workspace(),
        }
    }
    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: self.available_models().first().map(|m| m.id.clone()),
            endpoint: None,
            transport: None,
            capabilities: self.capabilities(),
        }
    }
    fn available_models(&self) -> Vec<ModelInfo>;

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
}

// ---------------------------------------------------------------------------
// Lens — user-controlled precision mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    Eco,
    Standard,
    Precise,
}

impl Lens {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lens::Eco => "eco",
            Lens::Standard => "standard",
            Lens::Precise => "precise",
        }
    }
}

impl FromStr for Lens {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "standard" => Lens::Standard,
            "precise" => Lens::Precise,
            "eco" => Lens::Eco,
            _ => return Err(()),
        })
    }
}

// ---------------------------------------------------------------------------
// Tool definition helpers
// ---------------------------------------------------------------------------

impl ToolDef {
    pub(crate) fn new(
        name: &str,
        description: &str,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration events
// ---------------------------------------------------------------------------

pub(crate) type TaskId = String;
pub(crate) type AgentId = String;

/// Token usage reported by an agent after completing a task.
#[derive(Debug, Clone, Default)]
pub(crate) struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Describes how to modify a live plan at runtime.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Protocol variants — constructed by agent responses at runtime.
pub(crate) enum PlanModification {
    AddTask {
        id: TaskId,
        role: String,
        description: String,
        depends_on: Vec<TaskId>,
    },
    RemoveTask {
        id: TaskId,
    },
    UpdateDependencies {
        id: TaskId,
        new_depends_on: Vec<TaskId>,
    },
}

/// Events flowing through the orchestrator event bus.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Protocol variants — constructed by agent responses at runtime.
pub(crate) enum OrchestratorEvent {
    TaskAssignment {
        task_id: TaskId,
        agent_id: AgentId,
        description: String,
        context: HashMap<String, serde_json::Value>,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },
    TaskCompletion {
        task_id: TaskId,
        agent_id: AgentId,
        output: String,
        artifacts: HashMap<String, serde_json::Value>,
        token_usage: TokenUsage,
        duration: Duration,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },
    TaskError {
        task_id: TaskId,
        agent_id: AgentId,
        error: String,
        retryable: bool,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },
    PlanModificationRequest {
        requested_by: AgentId,
        kind: PlanModification,
        reason: String,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },
    Progress {
        task_id: TaskId,
        agent_id: AgentId,
        message: String,
        percent: Option<u8>,
        timestamp: DateTime<Utc>,
    },
    TaskCancellation {
        task_id: TaskId,
        reason: String,
        timestamp: DateTime<Utc>,
    },
    Shutdown {
        reason: String,
        timestamp: DateTime<Utc>,
    },
}

// ---------------------------------------------------------------------------
// Agent role & executor
// ---------------------------------------------------------------------------

/// The functional role assigned to an agent in the fleet.
///
/// Wraps an arbitrary string from config. Role names are normalized
/// to lowercase with hyphens replaced by underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AgentRole(String);

impl AgentRole {
    /// Human-readable label for display/logging.
    pub fn label(&self) -> &str {
        &self.0
    }

    /// The canonical key used for role-based routing (lowercase, underscored).
    pub fn key(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AgentRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_lowercase().replace('-', "_");
        if normalized.is_empty() {
            return Err("agent role cannot be empty".to_string());
        }
        Ok(Self(normalized))
    }
}

/// Port for executing an agent task. Implemented by the adapter layer
/// using the existing `execute_agent_task()` function.
#[async_trait]
pub(crate) trait AgentTaskExecutor: Send + Sync {
    /// Execute a task given a fully-rendered prompt.
    /// Returns `(combined_output, tool_outcomes)`.
    async fn execute(&self, description: &str) -> Result<(String, Vec<(String, String)>), String>;
}

// ---------------------------------------------------------------------------
// Tool allow-list
// ---------------------------------------------------------------------------

/// Simple set of allowed tool names.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolAllowList {
    allowed: HashSet<String>,
}

impl ToolAllowList {
    pub(crate) fn from_tools(tools: &[ToolDef]) -> Self {
        Self {
            allowed: tools.iter().map(|t| t.name.clone()).collect(),
        }
    }

    pub(crate) fn is_allowed(&self, tool_name: &str) -> bool {
        self.allowed.contains(tool_name)
    }
}

// ---------------------------------------------------------------------------
// Memory types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub agent_id: String,
    pub created_at_epoch_s: u64,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct MemorySearchResult {
    pub entry: MemoryEntry,
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Chat / flow session types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptAssemblyReport {
    pub system_tokens: usize,
    pub history_tokens: usize,
    pub dropped_history_messages: usize,
    pub reserved_output_tokens: usize,
    pub output_token_cap: usize,
    pub total_input_budget: usize,
    pub flow_budget_remaining: usize,
    pub compaction_applied: bool,
    pub compacted_messages: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryAssembly {
    pub messages: Vec<Message>,
    #[allow(dead_code)] // read in tests only
    pub used_tokens: usize,
    #[allow(dead_code)] // read in tests only
    pub dropped_messages: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlowCompactionPolicy {
    pub threshold_tokens: u64,
    pub keep_turns: usize,
    pub summary_max_tokens: u32,
}

/// Mutable per-session runtime state for chat loop execution.
#[derive(Debug, Clone)]
pub(crate) struct ChatLoopState {
    pub messages: Vec<Message>,
    pub active_flow_key: Option<String>,
    pub manual_session_id: Option<String>,
    pub flow_token_usage: u64,
    pub active_lens: Lens,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub last_prompt_report: Option<PromptAssemblyReport>,
}

impl ChatLoopState {
    pub(crate) fn reset_for_new_session(&mut self) {
        self.manual_session_id = Some(uuid::Uuid::new_v4().to_string());
        self.active_flow_key = None;
        self.messages.clear();
        self.flow_token_usage = 0;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
    }
}

// ---------------------------------------------------------------------------
// Task & Plan
// ---------------------------------------------------------------------------

/// Runtime status of a task within a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped
        )
    }
}

/// A task within a plan.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub id: TaskId,
    pub role: String,
    pub description: String,
    pub depends_on: Vec<TaskId>,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub artifacts: HashMap<String, serde_json::Value>,
    pub assigned_agent: Option<AgentId>,
    pub started_at: Option<Instant>,
    pub attempt: u32,
    /// Error from the previous attempt — included in retry prompts so the agent
    /// adapts its approach instead of repeating the same failure.
    pub last_error: Option<String>,
}

/// A mutable plan that tracks task states and supports runtime modifications.
#[derive(Debug)]
pub(crate) struct Plan {
    pub goal: String,
    pub tasks: HashMap<TaskId, Task>,
    pub revision: u64,
}

impl Plan {
    pub fn new(goal: String, tasks: Vec<Task>) -> Self {
        let task_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        tracing::info!(
            goal = %goal,
            task_count = tasks.len(),
            task_ids = ?task_ids,
            "Plan::new — creating plan"
        );
        let task_map = tasks.into_iter().map(|t| (t.id.clone(), t)).collect();
        Self {
            goal,
            tasks: task_map,
            revision: 0,
        }
    }

    /// Move tasks from Pending -> Ready when all their dependencies are satisfied
    /// (completed or skipped). Returns the IDs of newly-ready tasks.
    pub fn dispatch_ready_tasks(&mut self) -> Vec<TaskId> {
        let satisfied: Vec<TaskId> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| {
                t.depends_on.iter().all(|dep_id| {
                    self.tasks
                        .get(dep_id)
                        .map(|d| d.status.is_terminal())
                        .unwrap_or(false)
                })
            })
            .map(|t| t.id.clone())
            .collect();

        for id in &satisfied {
            if let Some(task) = self.tasks.get_mut(id) {
                tracing::info!(
                    task = %id,
                    role = %task.role,
                    depends_on = ?task.depends_on,
                    "dispatch_ready_tasks — Pending → Ready"
                );
                task.status = TaskStatus::Ready;
            }
        }

        if !satisfied.is_empty() {
            self.revision += 1;
            tracing::info!(
                revision = self.revision,
                newly_ready = ?satisfied,
                "dispatch_ready_tasks — {} tasks became ready",
                satisfied.len()
            );
        }

        satisfied
    }

    /// Apply a plan modification. Returns `Err` with a reason if invalid.
    pub fn apply_modification(&mut self, modification: &PlanModification) -> Result<(), String> {
        tracing::info!(modification = ?modification, revision = self.revision, "apply_modification — evaluating");
        match modification {
            PlanModification::AddTask {
                id,
                role,
                description,
                depends_on,
            } => {
                if self.tasks.contains_key(id) {
                    return Err(format!("task {id} already exists"));
                }
                if depends_on.contains(id) {
                    return Err(format!("task {id} cannot depend on itself"));
                }
                for dep in depends_on {
                    if !self.tasks.contains_key(dep) {
                        return Err(format!("dependency {dep} does not exist"));
                    }
                }
                self.tasks.insert(
                    id.clone(),
                    Task {
                        id: id.clone(),
                        role: role.clone(),
                        description: description.clone(),
                        depends_on: depends_on.clone(),
                        status: TaskStatus::Pending,
                        output: None,
                        artifacts: HashMap::new(),
                        assigned_agent: None,
                        started_at: None,
                        attempt: 0,
                        last_error: None,
                    },
                );
                self.revision += 1;
                tracing::debug!(task = %id, role = %role, depends_on = ?depends_on, revision = self.revision, "apply_modification — AddTask succeeded");
                Ok(())
            }
            PlanModification::RemoveTask { id } => {
                let task = self
                    .tasks
                    .get(id)
                    .ok_or_else(|| format!("task {id} not found"))?;

                if task.status == TaskStatus::Running {
                    return Err(format!("cannot remove running task {id}"));
                }

                let has_dependents = self.tasks.values().any(|t| {
                    t.id != *id && t.depends_on.contains(id) && !t.status.is_terminal()
                });
                if has_dependents {
                    return Err(format!(
                        "cannot remove task {id}: other non-terminal tasks depend on it"
                    ));
                }

                self.tasks.remove(id);
                self.revision += 1;
                tracing::debug!(task = %id, revision = self.revision, "apply_modification — RemoveTask succeeded");
                Ok(())
            }
            PlanModification::UpdateDependencies {
                id,
                new_depends_on,
            } => {
                let task = self
                    .tasks
                    .get(id)
                    .ok_or_else(|| format!("task {id} not found"))?;

                if task.status != TaskStatus::Pending {
                    return Err(format!(
                        "cannot update dependencies of non-pending task {id}"
                    ));
                }

                for dep in new_depends_on {
                    if !self.tasks.contains_key(dep) {
                        return Err(format!("dependency {dep} does not exist"));
                    }
                }

                // Cycle detection.
                let old_deps = {
                    let task = self.tasks.get_mut(id).unwrap();
                    let old = task.depends_on.clone();
                    task.depends_on.clear();
                    old
                };
                let has_cycle = self.would_create_cycle(id, new_depends_on);
                self.tasks.get_mut(id).unwrap().depends_on = old_deps;
                if has_cycle {
                    return Err(format!(
                        "updating dependencies of {id} would create a cycle"
                    ));
                }

                let task = self.tasks.get_mut(id).unwrap();
                task.depends_on = new_depends_on.clone();
                self.revision += 1;
                tracing::debug!(task = %id, new_depends_on = ?new_depends_on, revision = self.revision, "apply_modification — UpdateDependencies succeeded");
                Ok(())
            }
        }
    }

    /// Check whether adding edges from `source` to `deps` would create a cycle.
    pub fn would_create_cycle(&self, source: &TaskId, deps: &[TaskId]) -> bool {
        for dep in deps {
            let mut stack = vec![dep.clone()];
            let mut visited = std::collections::HashSet::new();
            while let Some(node) = stack.pop() {
                if node == *source {
                    return true;
                }
                if !visited.insert(node.clone()) {
                    continue;
                }
                if let Some(task) = self.tasks.get(&node) {
                    for d in &task.depends_on {
                        stack.push(d.clone());
                    }
                }
            }
        }
        false
    }

    /// Returns `true` when every task is in a terminal state.
    pub fn is_complete(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.values().all(|t| t.status.is_terminal())
    }
}

/// Routing decision from the lightweight classifier.
pub(crate) enum RouteDecision {
    /// Request can be handled by a single agent. Contains the role key.
    SingleAgent(String),
    /// Request requires multi-agent planning.
    MultiAgent,
}

/// A single task in an execution plan (LLM output, before conversion to Task).
pub(crate) struct PlanTask {
    /// Unique identifier for this task (e.g. "research", "mint").
    pub id: String,
    /// Agent role key that should execute this task.
    pub role: String,
    /// What the agent should do.
    pub task: String,
    /// IDs of tasks that must complete before this one starts.
    pub depends_on: Vec<String>,
}

/// Declared dependency constraints from agent config.
/// Maps role_key -> list of role_keys it must depend on.
pub(crate) type RoleDependencies = HashMap<String, Vec<String>>;

/// A recorded task entry for the CLI history display.
#[derive(Debug, Clone)]
pub(crate) struct TaskHistoryEntry {
    pub id: String,
    pub description: String,
    pub role: String,
    pub assigned_agent: Option<String>,
    pub status: String,
}

/// Simple in-memory task history for the CLI `/tasks` command.
pub(crate) struct TaskHistory {
    entries: RwLock<Vec<TaskHistoryEntry>>,
}

impl TaskHistory {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    /// Record a new task.
    pub fn record(&self, id: String, description: String, role: String) {
        self.entries
            .write()
            .unwrap()
            .push(TaskHistoryEntry {
                id,
                description,
                role,
                assigned_agent: None,
                status: "pending".into(),
            });
    }

    /// Mark a task as assigned to an agent and in-progress.
    pub fn assign(&self, id: &str, agent: &str) {
        if let Some(entry) = self.entries.write().unwrap().iter_mut().find(|e| e.id == id) {
            entry.assigned_agent = Some(agent.to_string());
            entry.status = "in-progress".into();
        }
    }

    /// Mark a task as completed.
    pub fn complete(&self, id: &str) {
        if let Some(entry) = self.entries.write().unwrap().iter_mut().find(|e| e.id == id) {
            entry.status = "completed".into();
        }
    }

    /// Get all recorded entries.
    pub fn all(&self) -> Vec<TaskHistoryEntry> {
        self.entries.read().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Event bus
// ---------------------------------------------------------------------------

/// Bidirectional channel hub: one inbox for the orchestrator, one per agent.
#[derive(Debug)]
pub(crate) struct EventBus {
    pub orchestrator_rx: mpsc::Receiver<OrchestratorEvent>,
    pub orchestrator_tx: mpsc::Sender<OrchestratorEvent>,
    pub agent_txs: HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    pub agent_rxs: HashMap<AgentId, mpsc::Receiver<OrchestratorEvent>>,
}

impl EventBus {
    /// Create a new event bus with one inbox per agent plus the orchestrator inbox.
    ///
    /// `per_agent_buffer` controls the capacity of each agent's receive channel.
    /// The orchestrator inbox is sized as `(agent_ids.len() / 2).max(32).min(512)`.
    pub fn new(agent_ids: &[AgentId], per_agent_buffer: usize) -> Self {
        let orch_buf = (agent_ids.len() / 2).max(32).min(512);
        tracing::info!(
            agent_count = agent_ids.len(),
            orchestrator_buffer = orch_buf,
            per_agent_buffer = per_agent_buffer,
            agent_ids = ?agent_ids,
            "EventBus::new — creating channels"
        );
        let (orchestrator_tx, orchestrator_rx) = mpsc::channel(orch_buf);

        let mut agent_txs = HashMap::new();
        let mut agent_rxs = HashMap::new();

        for id in agent_ids {
            let (tx, rx) = mpsc::channel(per_agent_buffer);
            agent_txs.insert(id.clone(), tx);
            agent_rxs.insert(id.clone(), rx);
        }

        Self {
            orchestrator_rx,
            orchestrator_tx,
            agent_txs,
            agent_rxs,
        }
    }
}

