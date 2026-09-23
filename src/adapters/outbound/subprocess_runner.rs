//! Subprocess runner — Phase 3 of the redesign.
//!
//! Spawns `tengu run-agent` as a child process, pipes the IPC JSON in and
//! the result JSON out, and returns the `AgentIpcOutput`. `SubprocessRunner`
//! implements `WorkerHandle` so the DagExecutor drives it; the child runs the
//! real LLM mini-loop (`inbound/cli/run_agent.rs::run_agent_subprocess`). The accepted plan
//! reaches the child as `AgentIpcInput.plan_state` (per session), not via
//! the global `TENGU_PLAN.md`.
//!
//! The child subprocess is the same `tengu` binary re-invoked with a dedicated
//! `run-agent` subcommand and the `TENGU_AGENT_IPC=1` environment guard. The
//! guard prevents accidental fork-bomb-style re-entry.

#![allow(dead_code)] // SubprocessRunner is wired by the DagExecutor in Phase 4.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::AgentConfig;

/// JSON schema of the IPC input stream (stdin of `tengu run-agent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIpcInput {
    /// What the subagent has to accomplish.
    pub goal: String,
    /// Name of the agent — the child resolves `[agents.<name>]` from the
    /// same config the parent runs on (`sandbox_config` / default config).
    pub agent_name: String,
    /// OpenRouter model slug.
    pub model: String,
    /// Tool allow-list provided by the parent. The child intersects this with
    /// the `[agents.<name>]` block's `tools` and always appends `compress_and_store`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skills to load into the system prompt. Resolved via the three-tier loader.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Hard cap on LLM mini-loop turns (the agent's `limits.max_tool_rounds`).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Optional sandbox workspace. None → runner creates a temp dir.
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Parent-assigned session id for tagging memory writes.
    pub session_id: String,
    /// Parent-assigned step id for tagging memory writes.
    pub step_id: String,
    /// Phase 6.7 (C→B B-half) — when set, the child loads
    /// `[agents.<compose.base_agent>]` and overrides its `skills` and
    /// `tools` with the values supplied here. `agent_name` above remains
    /// the label used for events/logs (typically the same as
    /// `compose.base_agent`, but kept separate so the parent can pass
    /// a synthetic label like "composed-researcher" if it wants).
    /// Skipped on the wire when None so existing IPC payloads stay
    /// byte-compatible with prior versions of the binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<crate::domain::plan::AgentCompose>,
    /// Phase 7.2 — name of the sandbox the parent was loaded from
    /// (`--sandbox <name>` on the parent CLI). When `Some`, the child
    /// re-loads `sandboxes/<name>/config.toml` instead of falling
    /// through to the default user config — without this the child
    /// loses sandbox-specific scopes/secrets/MCP servers and tool
    /// calls scope-deny in the child even when the parent allows them.
    /// Distinct from the legacy `sandbox` field above (which was a
    /// workspace-path override, never wired up — kept for backward IPC
    /// compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_config: Option<String>,
    /// Rendered markdown of the accepted plan for THIS session
    /// (`shared_files::render_plan_state`). Source of truth for the plan
    /// block in the child's system prompt — replaces reading the global
    /// `TENGU_PLAN.md`, which concurrent webhook/telegram sessions overwrite.
    /// `None` (old parents) → the child falls back to the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_state: Option<String>,
}

fn default_max_turns() -> u32 {
    20
}

/// JSON schema of the IPC output stream (stdout of `tengu run-agent`).
///
/// `metrics` carries per-turn LLM telemetry collected during the subagent's
/// inner tool loop — empty for the legacy/test paths that don't fill it in.
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
    },
    /// Step failed. `output` contains partial text up to the failure.
    Failed {
        error: String,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        metrics: Vec<crate::domain::metrics::MetricsRecord>,
    },
}

/// Phase 3 spawner promoted to a `WorkerHandle` impl in Phase 4b. Each
/// `SubprocessRunner` instance carries one session id (generated at
/// construction); step ids come from each `Step` the executor passes in.
pub struct SubprocessRunner {
    /// Explicit path to the tengu binary. None → `current_exe()`.
    pub tengu_path: Option<std::path::PathBuf>,
    /// Hard timeout for the child when the step's agent is unknown
    /// (`run` direct callers); `run_step` uses the agent's
    /// `limits.step_timeout_secs`.
    pub timeout_secs: u64,
    /// `[agents.*]` of the parent config — fail-fast on unknown agents and
    /// per-step `max_tool_rounds` / `step_timeout_secs`.
    pub agents: std::collections::HashMap<String, AgentConfig>,
    /// Stable session id for tagging memory writes from this orchestrator
    /// instance. Generated once per runner construction.
    pub session_id: String,
    /// Phase 7.2 — name of the sandbox the parent was loaded from
    /// (`--sandbox <name>`). Forwarded into every IPC payload so the child
    /// process re-resolves the same `sandboxes/<name>/config.toml` and
    /// inherits the parent's scopes/secrets/MCP servers. `None` when the
    /// parent is running with the default user config.
    pub sandbox_name: Option<String>,
}

impl Default for SubprocessRunner {
    fn default() -> Self {
        Self {
            tengu_path: None,
            timeout_secs: crate::config::default_step_timeout_secs(),
            agents: std::collections::HashMap::new(),
            session_id: uuid::Uuid::new_v4().to_string(),
            sandbox_name: None,
        }
    }
}

impl SubprocessRunner {
    /// Convenience constructor that propagates the parent's sandbox name AND
    /// the parent planner's session_id into the IPC payload of every step.
    ///
    /// Pre Fix-B (2026-05-09) this minted a fresh UUID per construction,
    /// which meant the planner's `RagPlanner.session_id` and the runner's
    /// `SubprocessRunner.session_id` were independent. The child wrote
    /// `compress_and_store` outputs tagged with the runner's id, which the
    /// planner could never find via `search_outputs_for_session`. Now
    /// `build_orchestrator` resolves one id (env-or-fresh) and threads it
    /// through to both, so within-session output recall (Fix A) works.
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
}

impl SubprocessRunner {
    /// Run the subagent to completion and return its output.
    pub async fn run(&self, input: AgentIpcInput) -> Result<AgentIpcOutput> {
        self.run_with_timeout(input, self.timeout_secs).await
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
            // Forward subprocess stderr (tracing logs) to the parent's
            // stderr so users running with RUST_LOG=debug can see what the
            // subagent did — tool calls, engine spawn, etc. Previously
            // piped, which hid all subprocess detail unless the child
            // exited non-zero. Phase 7.5 diagnostic.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("spawn tengu run-agent")?;

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

        // Wait with timeout.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        .with_context(|| format!("run-agent timed out after {timeout_secs}s"))?
        .context("wait_with_output failed")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            bail!(
                "run-agent exited with non-zero status ({}): {}",
                output.status,
                stderr.trim()
            );
        }

        let stdout_str = String::from_utf8(output.stdout).context("non-utf8 stdout")?;
        let parsed: AgentIpcOutput = serde_json::from_str(stdout_str.trim())
            .with_context(|| format!("parse IPC output: {}", stdout_str.trim()))?;
        Ok(parsed)
    }
}

// =====================================================================
// WorkerHandle impl — each plan step spawns `tengu run-agent` as a child
// process over the JSON IPC above; the child runs the real LLM + tools loop
// for its `[agents.<name>]` block and returns one `AgentIpcOutput`.
// =====================================================================

#[async_trait::async_trait]
impl crate::ports::orchestration::WorkerHandle for SubprocessRunner {
    async fn run_step(
        &self,
        step: &crate::domain::plan::Step,
        step_inputs: &str,
    ) -> anyhow::Result<String> {
        // Fail fast when the planner picked a name with no `[agents.<name>]`
        // block. Without this we burn 3 retry attempts × subprocess spawn cost
        // before the orchestrator gives up and replans. The most common cause is
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
            let mut known: Vec<&str> = self
                .agents
                .iter()
                .filter(|(_, a)| a.description.is_some())
                .map(|(n, _)| n.as_str())
                .collect();
            known.sort();
            anyhow::bail!(
                "no agent '{}' in the active config — the planner picked '{}' but there is \
                 no `[agents.{}]` block (routable agents: {:?}). Likely cause: a SKILL or \
                 TOOL name was placed in the plan's `agent` field. The agent field MUST \
                 come from the `## Available agents` section of the roster.",
                spec_name,
                step.agent,
                spec_name,
                known
            );
        };
        let max_turns = agent_cfg.limits.max_tool_rounds;
        let step_timeout_secs = agent_cfg.limits.step_timeout_secs;

        let goal = if step_inputs.is_empty() {
            step.goal.clone()
        } else {
            format!("{}\n\nUpstream context:\n{}", step.goal, step_inputs)
        };

        // The child re-loads the same config (`sandbox_config`) and takes
        // `[agents.<agent_name>]` from it, so model/tools/skills stay empty
        // here; only the per-agent turn cap travels in the payload.
        // Phase 6.7: forward `step.compose` so a composed agent (C→B B-half)
        // reaches the child as an explicit override rather than a renamed
        // agent block.
        let input = AgentIpcInput {
            goal,
            agent_name: step.agent.clone(),
            model: String::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            max_turns,
            sandbox: None,
            session_id: self.session_id.clone(),
            step_id: step.id.0.clone(),
            compose: step.compose.clone(),
            // Phase 7.2 — propagate the parent's sandbox into the child so
            // the subagent sees the same scopes/secrets/MCP servers as the
            // parent process did.
            sandbox_config: self.sandbox_name.clone(),
            // Per-session plan registered by `replan::drive` under the same
            // session_id this runner carries (see `build_orchestrator`).
            plan_state: crate::application::orchestrator::shared_files::active_plan(
                &self.session_id,
            ),
        };

        match self.run_with_timeout(input, step_timeout_secs).await? {
            AgentIpcOutput::Ok {
                output, metrics, ..
            } => {
                // Re-emit per-turn subagent telemetry on the parent's global
                // metrics sink. The IPC boundary is the only path these
                // records can take from the child to the TUI / aggregator.
                for rec in metrics {
                    crate::application::metrics::record(rec);
                }
                Ok(output)
            }
            AgentIpcOutput::Failed {
                error,
                output,
                metrics,
            } => {
                for rec in metrics {
                    crate::application::metrics::record(rec);
                }
                anyhow::bail!("subagent failed: {}\npartial output:\n{}", error, output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix B (2026-05-09) — confirm `SubprocessRunner::new` plumbs the
    /// explicit session_id through instead of minting a fresh UUID.
    /// Pre-Fix-B the runner ignored the parent's id, breaking
    /// within-session output recall in `RagPlanner::plan`.
    #[test]
    fn new_uses_explicit_session_id() {
        let runner = SubprocessRunner::new(
            Some("aura".to_string()),
            "sess-from-planner".to_string(),
            Default::default(),
        );
        assert_eq!(runner.session_id, "sess-from-planner");
        assert_eq!(runner.sandbox_name.as_deref(), Some("aura"));
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
        let err = runner.run_step(&step, "").await.unwrap_err().to_string();
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
            model: "openai/gpt-4o".to_string(),
            tools: vec!["http_request".to_string()],
            skills: vec!["web-research".to_string()],
            max_turns: 10,
            sandbox: None,
            session_id: "s1".to_string(),
            step_id: "step-1".to_string(),
            compose: None,
            sandbox_config: None,
            plan_state: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        // `None` is skipped on the wire so old children keep parsing.
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
            model: String::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            max_turns: 1,
            sandbox: None,
            session_id: "s1".to_string(),
            step_id: "step-1".to_string(),
            compose: None,
            sandbox_config: None,
            plan_state: Some("# Tengu Current Plan\n".to_string()),
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"plan_state\""));
        let back: AgentIpcInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plan_state.as_deref(), Some("# Tengu Current Plan\n"));
    }

    /// Old parents send payloads without `plan_state`; the field defaults.
    #[test]
    fn ipc_input_parses_without_plan_state() {
        let json = r#"{"goal":"g","agent_name":"a","model":"m","session_id":"s","step_id":"x"}"#;
        let back: AgentIpcInput = serde_json::from_str(json).unwrap();
        assert!(back.plan_state.is_none());
    }

    #[test]
    fn ipc_output_ok_round_trips() {
        let out = AgentIpcOutput::Ok {
            output: "full text".to_string(),
            summary: "summary".to_string(),
            metrics: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        // `metrics` is `skip_serializing_if = "Vec::is_empty"` so the empty
        // case stays byte-compatible with prior IPC payloads.
        assert!(!json.contains("\"metrics\""));
        let back: AgentIpcOutput = serde_json::from_str(&json).unwrap();
        matches!(back, AgentIpcOutput::Ok { .. });
    }

    #[test]
    fn ipc_output_failed_round_trips() {
        let out = AgentIpcOutput::Failed {
            error: "timeout".to_string(),
            output: "partial".to_string(),
            metrics: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
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
