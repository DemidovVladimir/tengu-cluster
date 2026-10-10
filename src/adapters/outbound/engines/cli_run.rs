//! What the CLI engines share (`claude_code.rs`, `codex.rs`): the run's
//! working directory, the stdin prompt, and the transcript file the bridge
//! reads per call (`TENGU_BRIDGE_TRANSCRIPT_FILE`). Each engine parses its
//! own stream into the transcript ([`Transcript::update`]).

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::warn;

use crate::domain::message::{Message, Role};

/// The CLI's working directory for one run: the configured workspace (`~`
/// expanded), else — when Tengu bridge tools are offered — a temp dir for
/// this run named `<prefix>…` (returned so the caller keeps it alive), as a
/// `run-agent` step without a workspace gets one. Without either the bridge
/// is not written, so a chat agent with no `workspace` had no Tengu tools.
pub(crate) fn run_workspace(
    configured: Option<&Path>,
    has_bridge: bool,
    prefix: &str,
) -> std::io::Result<(Option<PathBuf>, Option<tempfile::TempDir>)> {
    if let Some(ws) = configured {
        return Ok((Some(crate::config::paths::expand_tilde(ws)), None));
    }
    if !has_bridge {
        return Ok((None, None));
    }
    let dir = tempfile::Builder::new().prefix(prefix).tempdir()?;
    // Canonical: scope checks compare resolved paths (macOS /var → /private/var).
    let path = std::fs::canonicalize(dir.path())?;
    Ok((Some(path), Some(dir)))
}

/// The conversation history as one stdin prompt. A system message equal to
/// `system_prompt` is left out: the CLI gets that one separately (Claude
/// Code `--system-prompt`, Codex `developer_instructions`).
pub(crate) fn format_prompt(messages: &[Message], system_prompt: Option<&str>) -> String {
    let system_prompt = system_prompt.filter(|s| !s.trim().is_empty());
    let mut parts = Vec::new();
    for msg in messages {
        if matches!(msg.role, Role::System) && Some(msg.content.as_str()) == system_prompt {
            continue;
        }
        match msg.role {
            Role::User => parts.push(format!("User: {}", msg.content)),
            Role::Assistant => parts.push(format!("Assistant: {}", msg.content)),
            Role::Tool => {
                if let Some(ref id) = msg.tool_call_id {
                    parts.push(format!("[Tool result for {}]: {}", id, msg.content));
                }
            }
            Role::System => parts.push(msg.content.clone()),
        }
    }
    parts.join("\n\n")
}

/// The conversation a bridged tool sees (`ToolCtx.conversation` — what
/// `skill_distill` seeds fixtures from), kept for the bridge in a temp file
/// (`TENGU_BRIDGE_TRANSCRIPT_FILE`; mode 0600, removed when the run ends):
/// the messages this run was given — what an in-process engine's tools see —
/// then what the engine's stream adds ([`Transcript::update`]). Rewritten
/// after every change, atomically (a sibling file renamed over it): the
/// bridge reads it per call and never sees half a write.
pub(crate) struct Transcript {
    path: tempfile::TempPath,
    messages: Vec<Message>,
    /// Index of the first streamed message: a streamed line merges into a
    /// streamed message only.
    streamed_from: usize,
}

impl Transcript {
    pub(crate) fn create(messages: &[Message]) -> Result<Self> {
        let path = tempfile::Builder::new()
            .prefix("tengu-transcript-")
            .tempfile()?
            .into_temp_path();
        let transcript = Self {
            path,
            messages: messages.to_vec(),
            streamed_from: messages.len(),
        };
        transcript.write()?;
        Ok(transcript)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `change(messages, streamed_from)` → `true` when it changed them: the
    /// file is rewritten. A failed write leaves the last one (warned).
    pub(crate) fn update(&mut self, change: impl FnOnce(&mut Vec<Message>, usize) -> bool) {
        if change(&mut self.messages, self.streamed_from) {
            if let Err(e) = self.write() {
                warn!(error = %e, file = %self.path.display(), "CLI engine: transcript for the bridge not updated");
            }
        }
    }

    fn write(&self) -> Result<()> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut next = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut next, &self.messages)?;
        next.persist(&*self.path)?;
        Ok(())
    }
}
