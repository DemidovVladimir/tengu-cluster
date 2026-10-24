//! Subprocess runner — the `WorkerHandle` that runs each plan step.
//!
//! Spawns `tengu run-agent` as a child process, pipes the IPC JSON in and
//! the result JSON out, and returns the `AgentIpcOutput`. `SubprocessRunner`
//! implements `WorkerHandle` so the DagExecutor drives it; the child runs the
//! real LLM mini-loop (`inbound/cli/run_agent.rs::run_agent_subprocess`). The plan
//! the step belongs to reaches the child as `AgentIpcInput.plan_state`
//! (`WorkerHandle::run_step(step, inputs, plan)`). Each step gets a journal
//! (`AgentIpcInput.journal`, `trace_store::JournalSink`): a recording
//! orchestrator (`with_trace`) asks the child for its tool events
//! (`AgentIpcInput.trace`: the step's `step.started`), which the child and
//! its Claude Code / Codex bridge append there as they run; the child also
//! appends each engine turn's metrics. Read once the child has exited —
//! however: an engine error or the timeout kill leaves no IPC reply — the
//! events go into this run (`trace_exec::replay_child`) and, without a
//! reply, the metrics onto the bus.
//!
//! The child subprocess is the same `tengu` binary re-invoked with a dedicated
//! `run-agent` subcommand and the `TENGU_AGENT_IPC=1` environment guard. The
//! guard prevents accidental fork-bomb-style re-entry.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::AgentConfig;
use crate::domain::observation::ErrorClass;
use crate::domain::plan::StepFailure;

/// JSON schema of the IPC input stream (stdin of `tengu run-agent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIpcInput {
    /// What the subagent has to accomplish.
    pub goal: String,
    /// Name of the agent — the child resolves `[agents.<name>]` from the
    /// same config the parent runs on (`sandbox_config` / default config).
    pub agent_name: String,
    /// Hard cap on LLM mini-loop turns: the parent sends the agent's
    /// `limits.max_tool_rounds` (the engine matrix a smaller cap).
    /// Required — a payload without it is refused, never run on a default.
    pub max_turns: u32,
    /// Parent-assigned session id for tagging memory writes.
    pub session_id: String,
    /// Parent-assigned step id for tagging memory writes.
    pub step_id: String,
    /// When set, the child loads
    /// `[agents.<compose.base_agent>]` and overrides its `skills` and
    /// `tools` with the values supplied here. `agent_name` above remains
    /// the label used for events/logs (typically the same as
    /// `compose.base_agent`, but kept separate so the parent can pass
    /// a synthetic label like "composed-researcher" if it wants).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<crate::domain::plan::AgentCompose>,
    /// Name of the sandbox the parent was loaded from
    /// (`--sandbox <name>` on the parent CLI). When `Some`, the child
    /// re-loads `sandboxes/<name>/config.toml` instead of falling
    /// through to the default user config — without this the child
    /// loses sandbox-specific scopes/secrets/MCP servers and tool
    /// calls scope-deny in the child even when the parent allows them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_config: Option<String>,
    /// Rendered markdown of the plan this step belongs to
    /// (`shared_files::render_plan_state`): the plan block of the child's
    /// system prompt. `None` (an SOE stage: no plan) ⇒ no block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_state: Option<String>,
    /// The parent's execution trace this step runs under: set (with a
    /// `journal`) ⇒ the child and its bridge record its tool calls in the
    /// journal; the parent writes them into this run under
    /// `parent_event_id` (the step's `step.started`). `None` (a surface that
    /// does not record) ⇒ nothing is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<IpcTraceRef>,
    /// The step's journal (`trace_store::JournalSink`, module docs): a 0600
    /// file the parent made and reads back on any exit — the tool events
    /// (`trace` set) and each engine turn's metrics. `None` (the SOE stage
    /// runner) ⇒ the child writes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<std::path::PathBuf>,
}

/// [`AgentIpcInput::trace`]: which recording, under which event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTraceRef {
    pub run_id: String,
    pub parent_event_id: String,
}

/// JSON schema of the IPC output stream (stdout of `tengu run-agent`).
///
/// `metrics` carries per-turn LLM telemetry collected during the subagent's
/// inner tool loop — empty for the legacy/test paths that don't fill it in
/// (the step's journal has them too, for an exit without this reply).
/// A step's tool events travel in its journal, not here.
/// `tools` is the step's tool activity in call order (`ToolRun`: name + ok):
/// calls the child ran (`compress_and_store` included) and the ones a Claude
/// Code engine ran through the bridge (`StreamEvent::ToolRan`) — what
/// `tests/engine_matrix.rs` asserts on.
/// `#[serde(default, skip_serializing_if = "Vec::is_empty")]` keeps the IPC
/// payload byte-compatible with prior versions of the binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum AgentIpcOutput {
    /// Step completed cleanly. `summary` is the `compress_and_store` text the
    /// child captured (persisted to Postgres `agentic_memory` with `postgres_memory`).
    Ok {
        output: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        metrics: Vec<crate::domain::metrics::MetricsRecord>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools: Vec<crate::domain::message::ToolRun>,
    },
    /// Step failed. `output` contains partial text up to the failure.
    Failed {
        error: String,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        metrics: Vec<crate::domain::metrics::MetricsRecord>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools: Vec<crate::domain::message::ToolRun>,
    },
}

/// The `WorkerHandle` that spawns `tengu run-agent` per step. Each
/// `SubprocessRunner` instance carries one session id (generated at
/// construction); step ids come from each `Step` the executor passes in.
pub struct SubprocessRunner {
    /// Explicit path to the tengu binary. None → `current_exe()`.
    pub tengu_path: Option<std::path::PathBuf>,
    /// `[agents.*]` of the parent config — fail-fast on unknown agents and
    /// per-step `max_tool_rounds` / `step_timeout_secs`.
    pub agents: std::collections::HashMap<String, AgentConfig>,
    /// Stable session id for tagging memory writes from this orchestrator
    /// instance. Generated once per runner construction.
    pub session_id: String,
    /// Name of the sandbox the parent was loaded from
    /// (`--sandbox <name>`). Forwarded into every IPC payload so the child
    /// process re-resolves the same `sandboxes/<name>/config.toml` and
    /// inherits the parent's scopes/secrets/MCP servers. `None` when the
    /// parent is running with the default user config.
    pub sandbox_name: Option<String>,
    /// The recording a step's child tool events go to (`with_trace`): asked
    /// for over `AgentIpcInput.trace` when the step runs caused by a
    /// `step.started` (`trace_exec::cause`), read from the step's journal and
    /// written by `trace_exec::replay_child`. `None` = no child records.
    pub trace: Option<std::sync::Arc<dyn crate::ports::trace::TraceSink>>,
}

impl Default for SubprocessRunner {
    fn default() -> Self {
        Self {
            tengu_path: None,
            agents: std::collections::HashMap::new(),
            session_id: uuid::Uuid::new_v4().to_string(),
            sandbox_name: None,
            trace: None,
        }
    }
}

impl SubprocessRunner {
    /// Convenience constructor that propagates the parent's sandbox name AND
    /// the parent planner's session_id into the IPC payload of every step.
    ///
    /// `build_orchestrator` resolves one id (env or fresh) and hands it to
    /// both this runner and the `RegistryPlanner`, so the child's
    /// `compress_and_store` outputs carry the id the planner's
    /// within-session recall (`search_outputs_for_session`) filters on.
    ///
    /// Standalone test harnesses can still call `SubprocessRunner::default()`
    /// to get a runner with an independently-minted UUID — the unified-id
    /// guarantee is at the `build_orchestrator` boundary, not the runner's.
    pub fn new(
        sandbox_name: Option<String>,
        session_id: String,
        agents: std::collections::HashMap<String, AgentConfig>,
    ) -> Self {
        Self {
            sandbox_name,
            session_id,
            agents,
            ..Self::default()
        }
    }

    /// `AgentIpcInput.trace` for the step running now: this runner records
    /// and the step runs caused by its `step.started` (`trace_exec::cause`).
    fn trace_ref(&self) -> Option<IpcTraceRef> {
        let run_id = self.trace.as_ref()?.run_id()?.to_string();
        let parent_event_id = crate::application::trace_exec::cause()?.parent?;
        Some(IpcTraceRef {
            run_id,
            parent_event_id,
        })
    }

    /// Record each step's child tool events into `sink` (module docs).
    pub(crate) fn with_trace(
        mut self,
        sink: Option<std::sync::Arc<dyn crate::ports::trace::TraceSink>>,
    ) -> Self {
        self.trace = sink;
        self
    }
}

impl SubprocessRunner {
    /// `[agents.*]` names with a `description` (the ones a plan step may
    /// use), sorted.
    fn routable_agents(&self) -> Vec<String> {
        let mut known: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.description.is_some())
            .map(|(n, _)| n.clone())
            .collect();
        known.sort();
        known
    }

    /// `run` with an explicit wall-clock cap for the child.
    pub async fn run_with_timeout(
        &self,
        input: AgentIpcInput,
        timeout_secs: u64,
    ) -> Result<AgentIpcOutput> {
        let exe = match &self.tengu_path {
            Some(p) => p.clone(),
            None => std::env::current_exe().context("current_exe() failed")?,
        };

        let mut cmd = Command::new(exe);
        cmd.arg("run-agent")
            .env("TENGU_AGENT_IPC", "1")
            // Child applies the parent's `[egress]` policy verbatim.
            .env(
                crate::adapters::outbound::egress::EGRESS_ENV,
                crate::adapters::outbound::egress::policy().child_env(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The child's log lines go through the parent's own log
            // (`forward_child_log`): to the log file in the TUI, not over
            // its screen; the last lines explain a failed exit.
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("spawn tengu run-agent")?;
        let stderr_tail = forward_child_log(
            child.stderr.take().context("child stderr missing")?,
            input.agent_name.clone(),
        );

        // Write stdin JSON.
        let stdin_payload = serde_json::to_vec(&input).context("serialise IPC input")?;
        {
            let mut stdin = child.stdin.take().context("child stdin missing")?;
            stdin
                .write_all(&stdin_payload)
                .await
                .context("write IPC input to child stdin")?;
            stdin.shutdown().await.context("close child stdin")?;
        }

        // Wait with timeout. The error classes (`StepFailure`) tell the
        // step's retry what another attempt can change.
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(waited) => waited.context("wait_with_output failed")?,
            Err(_) => {
                let why = format!("run-agent timed out after {timeout_secs}s");
                return Err(StepFailure::new(ErrorClass::Timeout, why).into());
            }
        };

        if !output.status.success() {
            // EOF on the pipe ends the reader: the tail is complete.
            let tail = stderr_tail.await.unwrap_or_default();
            // An exit code = the child failed before its loop (config,
            // compose, engine build): the same next time. A signal (no
            // code, e.g. OOM-killed) may not be.
            let class = match output.status.code() {
                Some(_) => ErrorClass::Fatal,
                None => ErrorClass::Transient,
            };
            let why = format!(
                "run-agent exited with non-zero status ({}): {}",
                output.status,
                tail.trim()
            );
            return Err(StepFailure::new(class, why).into());
        }

        let stdout_str = String::from_utf8(output.stdout).context("non-utf8 stdout")?;
        serde_json::from_str(stdout_str.trim()).map_err(|e| {
            let why = format!("parse IPC output: {}: {e}", stdout_str.trim());
            StepFailure::new(ErrorClass::Decode, why).into()
        })
    }
}

/// Lines of a failed child's log kept for the error message.
const STDERR_TAIL_LINES: usize = 40;

/// Read the child's stderr (its tracing log) line by line into the parent's
/// log at `info` (target `tengu::run_agent`); the task
/// returns the last `STDERR_TAIL_LINES` lines once the pipe closes.
fn forward_child_log(
    stderr: tokio::process::ChildStderr,
    agent: String,
) -> tokio::task::JoinHandle<String> {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut tail = std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES);
        while let Ok(Some(line)) = lines.next_line().await {
            // The child logs without colours: its stderr is not a terminal
            // (`cli::init_logging`).
            if line.trim().is_empty() {
                continue;
            }
            tracing::info!(target: "tengu::run_agent", agent = %agent, "{line}");
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        Vec::from(tail).join("\n")
    })
}

// =====================================================================
// WorkerHandle impl — each plan step spawns `tengu run-agent` as a child
// process over the JSON IPC above; the child runs the real LLM + tools loop
// for its `[agents.<name>]` block and returns one `AgentIpcOutput`.
// =====================================================================

#[async_trait::async_trait]
impl crate::ports::orchestration::WorkerHandle for SubprocessRunner {
    /// Only blocks with a `description` run as plan steps (the child checks
    /// too).
    fn known_agents(&self) -> Option<Vec<String>> {
        Some(self.routable_agents())
    }

    fn input_cap(&self, step: &crate::domain::plan::Step) -> Option<usize> {
        let agent = step.compose.as_ref().map_or(&step.agent, |c| &c.base_agent);
        self.agents.get(agent).map(AgentConfig::tool_result_cap)
    }

    async fn run_step(
        &self,
        step: &crate::domain::plan::Step,
        step_inputs: &str,
        plan: &crate::domain::plan::Plan,
    ) -> anyhow::Result<String> {
        // Fail fast when the planner picked a name with no `[agents.<name>]`
        // block: no subprocess is spawned. `replan::drive` already rejects
        // such a plan before any step runs (`known_agents`, `Plan::validate`);
        // this is the backstop for direct callers. The most common cause is
        // the planner LLM putting a SKILL or TOOL name in the `agent` field
        // (the orchestrator SKILL.md forbids it but enforcement is still useful).
        // Composed plans (C→B B-half) resolve `compose.base_agent` instead.
        let spec_name = step
            .compose
            .as_ref()
            .map(|c| c.base_agent.as_str())
            .unwrap_or(step.agent.as_str());
        let Some(agent_cfg) = self
            .agents
            .get(spec_name)
            .filter(|a| a.description.is_some())
        else {
            let known = self.routable_agents();
            let why = format!(
                "no agent '{}' in the active config — the planner picked '{}' but there is \
                 no `[agents.{}]` block (routable agents: {:?}). Likely cause: a SKILL or \
                 TOOL name was placed in the plan's `agent` field. The agent field MUST \
                 come from the `## Available agents` section of the roster.",
                spec_name, step.agent, spec_name, known
            );
            return Err(StepFailure::new(ErrorClass::Fatal, why).into());
        };
        let max_turns = agent_cfg.limits.max_tool_rounds;
        let step_timeout_secs = agent_cfg.limits.step_timeout_secs;

        let goal = if step_inputs.is_empty() {
            step.goal.clone()
        } else {
            format!("{}\n\nUpstream context:\n{}", step.goal, step_inputs)
        };

        // The step's journal (module docs): made here (0600), read once the
        // child has exited, removed with `journal`.
        let journal = tempfile::NamedTempFile::new().context("run-agent: step journal")?;
        // The child re-loads the same config (`sandbox_config`) and takes
        // `[agents.<agent_name>]` from it (model, tools, skills); only the
        // per-agent turn cap travels in the payload.
        // `step.compose` travels too: a composed agent reaches the child as
        // an explicit override, not a renamed agent block.
        let input = AgentIpcInput {
            goal,
            agent_name: step.agent.clone(),
            max_turns,
            session_id: self.session_id.clone(),
            step_id: step.id.0.clone(),
            compose: step.compose.clone(),
            // Propagate the parent's sandbox into the child so
            // the subagent sees the same scopes/secrets/MCP servers as the
            // parent process did.
            sandbox_config: self.sandbox_name.clone(),
            plan_state: Some(
                crate::application::orchestrator::shared_files::render_plan_state(plan),
            ),
            trace: self.trace_ref(),
            journal: Some(journal.path().to_path_buf()),
        };
        let step_event = input.trace.as_ref().map(|t| t.parent_event_id.clone());
        let result = self.run_with_timeout(input, step_timeout_secs).await;
        let written = crate::adapters::outbound::trace_store::read_journal(journal.path());
        if let (Some(sink), Some(parent)) = (&self.trace, &step_event) {
            crate::application::trace_exec::replay_child(
                sink.as_ref(),
                written.events,
                parent,
                &self.session_id,
            );
        }
        // Re-emit per-turn subagent telemetry on the parent's global metrics
        // sink: the reply's, else the journal's (no reply came). The child
        // logged each one already (`forward_child_log`).
        let metrics = match &result {
            Ok(AgentIpcOutput::Ok { metrics, .. } | AgentIpcOutput::Failed { metrics, .. }) => {
                metrics.clone()
            }
            Err(_) => written.metrics,
        };
        for rec in metrics {
            crate::application::metrics::forward(rec);
        }
        match result? {
            AgentIpcOutput::Ok { output, .. } => Ok(output),
            AgentIpcOutput::Failed { error, output, .. } => {
                anyhow::bail!("subagent failed: {}\npartial output:\n{}", error, output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child's stderr reaches the parent log, and its last lines come back
    /// for the error of a failed exit (they used to be empty: stderr was
    /// inherited).
    #[tokio::test]
    async fn child_stderr_tail_explains_a_failed_exit() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("for i in $(seq 0 44); do echo \"line $i\" >&2; done; echo 'ERROR boom' >&2; exit 3")
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let tail = forward_child_log(child.stderr.take().unwrap(), "a".into());
        assert!(!child.wait().await.unwrap().success());
        let tail = tail.await.unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines.last(), Some(&"ERROR boom"));
        assert!(!tail.contains("line 0\n"), "older lines dropped: {tail}");
    }

    /// `SubprocessRunner::new` keeps the explicit session_id (the planner's)
    /// instead of minting a fresh UUID — within-session output recall in
    /// `RegistryPlanner::plan` depends on it.
    #[test]
    fn new_uses_explicit_session_id() {
        let runner = SubprocessRunner::new(
            Some("lping".to_string()),
            "sess-from-planner".to_string(),
            Default::default(),
        );
        assert_eq!(runner.session_id, "sess-from-planner");
        assert_eq!(runner.sandbox_name.as_deref(), Some("lping"));
    }

    /// A step naming an agent with no `[agents.<name>]` block fails before
    /// any subprocess is spawned, and the message lists the routable agents.
    #[tokio::test]
    async fn run_step_fails_fast_on_unknown_agent() {
        use crate::domain::plan::{Step, StepId};
        use crate::ports::orchestration::WorkerHandle;
        let mut agents = std::collections::HashMap::new();
        let mut researcher = crate::config::Config::default()
            .agents
            .remove("main")
            .unwrap();
        researcher.description = Some("web research".into());
        agents.insert("researcher".to_string(), researcher);
        let runner = SubprocessRunner::new(None, "s".into(), agents);
        let step = Step {
            id: StepId("s1".into()),
            agent: "spanish-teacher".into(),
            goal: "g".into(),
            depends_on: vec![],
            compose: None,
        };
        let plan = crate::domain::plan::Plan {
            steps: vec![step.clone()],
        };
        let err = runner.run_step(&step, "", &plan).await.unwrap_err();
        let class = err.downcast_ref::<StepFailure>().map(|f| f.class);
        assert_eq!(class, Some(ErrorClass::Fatal), "never retried");
        let err = err.to_string();
        assert!(err.contains("no agent 'spanish-teacher'"), "{err}");
        assert!(err.contains("researcher"), "{err}");
    }

    /// Default still mints a fresh UUID for standalone use cases (tests,
    /// one-off CLI invocations). The unified-id guarantee lives at the
    /// `build_orchestrator` boundary, not the runner's.
    #[test]
    fn default_mints_fresh_uuid() {
        let a = SubprocessRunner::default();
        let b = SubprocessRunner::default();
        assert_ne!(a.session_id, b.session_id);
        assert!(!a.session_id.is_empty());
    }

    #[test]
    fn ipc_input_round_trips() {
        let input = AgentIpcInput {
            goal: "say hello".to_string(),
            agent_name: "researcher".to_string(),
            max_turns: 10,
            session_id: "s1".to_string(),
            step_id: "step-1".to_string(),
            compose: None,
            sandbox_config: None,
            plan_state: None,
            trace: None,
            journal: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        // `None` is skipped on the wire.
        assert!(!json.contains("\"plan_state\""));
        let back: AgentIpcInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.goal, input.goal);
        assert_eq!(back.session_id, input.session_id);
        assert_eq!(back.plan_state, None);
    }

    #[test]
    fn ipc_input_plan_state_round_trips() {
        let input = AgentIpcInput {
            goal: "g".to_string(),
            agent_name: "researcher".to_string(),
            max_turns: 1,
            session_id: "s1".to_string(),
            step_id: "step-1".to_string(),
            compose: None,
            sandbox_config: None,
            plan_state: Some("## Steps\n".to_string()),
            trace: None,
            journal: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"plan_state\""));
        let back: AgentIpcInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plan_state.as_deref(), Some("## Steps\n"));
    }

    /// The optional fields default: no plan block, nothing recorded; the
    /// turn cap has no default (it used to be 20 against the config's 70).
    #[test]
    fn ipc_input_parses_without_optional_fields() {
        let json = r#"{"goal":"g","agent_name":"a","max_turns":7,"session_id":"s","step_id":"x"}"#;
        let back: AgentIpcInput = serde_json::from_str(json).unwrap();
        assert_eq!(back.max_turns, 7);
        assert!(back.plan_state.is_none());
        assert!(back.trace.is_none());
        let no_cap = r#"{"goal":"g","agent_name":"a","session_id":"s","step_id":"x"}"#;
        let err = serde_json::from_str::<AgentIpcInput>(no_cap).unwrap_err();
        assert!(err.to_string().contains("max_turns"), "{err}");
    }

    /// The trace ref and the journal cross only when set (an old child
    /// never sees the keys); an old child's output with `trace` drafts still
    /// parses (tool events travel in the journal now).
    #[test]
    fn ipc_trace_is_compatible_both_ways() {
        let json = r#"{"goal":"g","agent_name":"a","max_turns":3,"session_id":"s","step_id":"x"}"#;
        let mut input: AgentIpcInput = serde_json::from_str(json).unwrap();
        let wire = serde_json::to_string(&input).unwrap();
        assert!(!wire.contains("\"trace\"") && !wire.contains("\"journal\""));
        input.trace = Some(IpcTraceRef {
            run_id: "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e".into(),
            parent_event_id: "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e:7".into(),
        });
        input.journal = Some("/tmp/tengu-journal-x".into());
        let back: AgentIpcInput =
            serde_json::from_str(&serde_json::to_string(&input).unwrap()).unwrap();
        assert_eq!(back.trace, input.trace);
        assert_eq!(back.journal, input.journal);

        let old: AgentIpcOutput = serde_json::from_str(
            r#"{"status":"ok","output":"o","summary":"s","trace":[{"component":"tool","kind":"tool.started","status":"running"}]}"#,
        )
        .unwrap();
        assert!(matches!(old, AgentIpcOutput::Ok { output, .. } if output == "o"));
    }

    /// A child that dies without its IPC reply — an exit code (an engine
    /// error), or killed at the step timeout — still leaves the tool events
    /// it and its bridge journalled: replayed under the step, the finish
    /// under its start; the step fails with the same class as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dead_child_still_reports_its_tool_events() {
        use crate::application::trace_exec::tests::MemTrace;
        use crate::application::trace_exec::{caused_by, Cause};
        use crate::domain::trace::{Component, EventDraft, Status};
        use crate::ports::orchestration::WorkerHandle;
        use std::os::unix::fs::PermissionsExt;

        let line = |id: &str, kind: &str, status, parent: Option<&str>| {
            let mut d = EventDraft::new(Component::Tool, kind, status).at(1);
            d.parent_event_id = parent.map(str::to_string);
            serde_json::json!({"event": {"id": id, "draft": d}}).to_string()
        };
        let lines = [
            line("ipc:9:1", "tool.started", Status::Running, None),
            line("ipc:9:2", "tool.completed", Status::Ok, Some("ipc:9:1")),
        ];
        for (exit, failure) in [
            ("exit 3", ErrorClass::Fatal),
            ("sleep 5", ErrorClass::Timeout),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let exe = tmp.path().join("fake-tengu");
            std::fs::write(
                &exe,
                format!(
                    "#!/bin/sh\nj=$(sed -n 's/.*\"journal\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '%s\\n' '{}' '{}' >> \"$j\"\n{exit}\n",
                    lines[0], lines[1]
                ),
            )
            .unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut agent = crate::config::Config::default()
                .agents
                .remove("main")
                .unwrap();
            agent.description = Some("web research".into());
            agent.limits.step_timeout_secs = 1;
            let sink = std::sync::Arc::new(MemTrace::default());
            let mut runner = SubprocessRunner::new(
                None,
                "chat".into(),
                [("researcher".to_string(), agent)].into(),
            )
            .with_trace(Some(sink.clone()));
            runner.tengu_path = Some(exe);
            let step = crate::domain::plan::Step {
                id: crate::domain::plan::StepId("s1".into()),
                agent: "researcher".into(),
                goal: "g".into(),
                depends_on: vec![],
                compose: None,
            };
            let plan = crate::domain::plan::Plan {
                steps: vec![step.clone()],
            };
            let err = caused_by(
                Cause::new(Some("run:7".into()), "chat"),
                runner.run_step(&step, "", &plan),
            )
            .await
            .unwrap_err();
            let class = err.downcast_ref::<StepFailure>().map(|f| f.class);
            assert_eq!(class, Some(failure), "{err:#}");
            let d = sink.all();
            assert_eq!(sink.kinds(), ["tool.started", "tool.completed"], "{exit}");
            assert_eq!(d[0].parent_event_id.as_deref(), Some("run:7"));
            assert_eq!(d[1].parent_event_id, Some(MemTrace::id(0)));
            assert!(d.iter().all(|e| e.session_id.as_deref() == Some("chat")));
        }
    }

    #[test]
    fn ipc_output_ok_round_trips() {
        let out = AgentIpcOutput::Ok {
            output: "full text".to_string(),
            summary: "summary".to_string(),
            metrics: Vec::new(),
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        // `metrics` / `tools` are `skip_serializing_if = "Vec::is_empty"` so
        // the empty case stays byte-compatible with prior IPC payloads.
        assert!(!json.contains("\"metrics\""));
        assert!(!json.contains("\"tools\""));
        let back: AgentIpcOutput = serde_json::from_str(&json).unwrap();
        matches!(back, AgentIpcOutput::Ok { .. });
    }

    #[test]
    fn ipc_output_failed_round_trips() {
        let out = AgentIpcOutput::Failed {
            error: "timeout".to_string(),
            output: "partial".to_string(),
            metrics: Vec::new(),
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
    }

    /// The step's tool activity crosses the boundary; an old child's
    /// payload (no `tools`) parses to an empty list.
    #[test]
    fn ipc_output_tools_round_trip_and_default() {
        let run = |name: &str, ok| crate::domain::message::ToolRun {
            name: name.into(),
            ok,
        };
        let out = AgentIpcOutput::Ok {
            output: "o".into(),
            summary: "s".into(),
            metrics: Vec::new(),
            tools: vec![run("read_file", true), run("lp_decide", false)],
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains(
                r#""tools":[{"name":"read_file","ok":true},{"name":"lp_decide","ok":false}]"#
            ),
            "{json}"
        );
        let AgentIpcOutput::Ok { tools, .. } = serde_json::from_str(&json).unwrap() else {
            panic!("expected Ok");
        };
        assert_eq!(tools, [run("read_file", true), run("lp_decide", false)]);
        let old: AgentIpcOutput =
            serde_json::from_str(r#"{"status":"failed","error":"e","output":""}"#).unwrap();
        assert!(matches!(old, AgentIpcOutput::Failed { tools, .. } if tools.is_empty()));
    }

    #[test]
    fn ipc_output_with_metrics_round_trips() {
        let m = crate::domain::metrics::MetricsRecord {
            ts_unix: 0,
            session_id: "s".into(),
            kind: crate::domain::metrics::MetricsKind::Subagent,
            agent: "researcher".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            prompt_tokens: 100,
            cached_input_tokens: 0,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_chars: 500,
            prompt_bytes: 600,
            response_chars: 200,
            latency_ms: 1234,
            layers: Vec::new(),
            step_id: Some("s1".into()),
        };
        let out = AgentIpcOutput::Ok {
            output: "full text".to_string(),
            summary: "summary".to_string(),
            metrics: vec![m],
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"metrics\""));
        let back: AgentIpcOutput = serde_json::from_str(&json).unwrap();
        if let AgentIpcOutput::Ok { metrics, .. } = back {
            assert_eq!(metrics.len(), 1);
            assert_eq!(metrics[0].total_tokens, 150);
        } else {
            panic!("expected Ok variant");
        }
    }
}
