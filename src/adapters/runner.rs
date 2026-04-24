//! Subprocess runner — Phase 3 of the redesign.
//!
//! Spawns `tengu run-agent` as a child process, pipes the IPC JSON in and
//! the result JSON out, and returns the `AgentIpcOutput`. Phase 3 ships the
//! IPC plumbing only; the run-agent child itself is a stub that does not yet
//! call the LLM. Phase 4 replaces the child's mini-loop body and this module
//! becomes an implementation of `WorkerHandle` so the DagExecutor can drive
//! it.
//!
//! The child subprocess is the same `tengu` binary re-invoked with a dedicated
//! `run-agent` subcommand and the `TENGU_AGENT_IPC=1` environment guard. The
//! guard prevents accidental fork-bomb-style re-entry.

#![allow(dead_code)]  // SubprocessRunner is wired by the DagExecutor in Phase 4.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// JSON schema of the IPC input stream (stdin of `tengu run-agent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIpcInput {
    /// What the subagent has to accomplish.
    pub goal: String,
    /// Name of the agent spec (resolved to `agents/<name>.toml` by the child).
    pub agent_name: String,
    /// OpenRouter model slug.
    pub model: String,
    /// Tool allow-list provided by the parent. The child intersects this with
    /// the agent spec's declared tools and always appends `compress_and_store`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skills to load into the system prompt. Resolved via the three-tier loader.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Hard cap on LLM mini-loop turns.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Optional sandbox workspace. None → runner creates a temp dir.
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Parent-assigned session id for tagging memory writes.
    pub session_id: String,
    /// Parent-assigned step id for tagging memory writes.
    pub step_id: String,
}

fn default_max_turns() -> u32 {
    20
}

/// JSON schema of the IPC output stream (stdout of `tengu run-agent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum AgentIpcOutput {
    /// Step completed cleanly. `summary` was already written to `tengu_outputs`
    /// by the child (via compress_and_store).
    Ok { output: String, summary: String },
    /// Step failed. `output` contains partial text up to the failure.
    Failed { error: String, output: String },
}

/// Thin Phase 3 spawner. Phase 4 wraps this in an `impl WorkerHandle`.
pub struct SubprocessRunner {
    /// Explicit path to the tengu binary. None → `current_exe()`.
    pub tengu_path: Option<std::path::PathBuf>,
    /// Hard timeout for the child.
    pub timeout_secs: u64,
}

impl Default for SubprocessRunner {
    fn default() -> Self {
        Self {
            tengu_path: None,
            timeout_secs: 180,
        }
    }
}

impl SubprocessRunner {
    /// Run the subagent to completion and return its output.
    pub async fn run(&self, input: AgentIpcInput) -> Result<AgentIpcOutput> {
        let exe = match &self.tengu_path {
            Some(p) => p.clone(),
            None => std::env::current_exe().context("current_exe() failed")?,
        };

        let mut cmd = Command::new(exe);
        cmd.arg("run-agent")
            .env("TENGU_AGENT_IPC", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
            stdin
                .shutdown()
                .await
                .context("close child stdin")?;
        }

        // Wait with timeout.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            child.wait_with_output(),
        )
        .await
        .context("run-agent timed out")?
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

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let json = serde_json::to_string(&input).unwrap();
        let back: AgentIpcInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.goal, input.goal);
        assert_eq!(back.session_id, input.session_id);
    }

    #[test]
    fn ipc_output_ok_round_trips() {
        let out = AgentIpcOutput::Ok {
            output: "full text".to_string(),
            summary: "summary".to_string(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        let back: AgentIpcOutput = serde_json::from_str(&json).unwrap();
        matches!(back, AgentIpcOutput::Ok { .. });
    }

    #[test]
    fn ipc_output_failed_round_trips() {
        let out = AgentIpcOutput::Failed {
            error: "timeout".to_string(),
            output: "partial".to_string(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
    }
}
