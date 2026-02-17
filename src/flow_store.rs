//! Persistent flow/session storage for the CLI runtime.
//!
//! This module stores per-flow transcripts (`transcript.jsonl`) and maintains
//! a compact metadata index (`index.json`) with lock-safe and atomic updates.
//!
//! Potential use case:
//! Resume the same customer conversation after process restart without losing context.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{Message, Recipient};

const INDEX_FILENAME: &str = "index.json";
const INDEX_LOCK_FILENAME: &str = "index.lock";
const TRANSCRIPT_FILENAME: &str = "transcript.jsonl";
const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowMetadata {
    pub flow_key: String,
    pub agent_id: String,
    pub transcript_relpath: String,
    pub message_count: u64,
    pub token_estimate: u64,
    pub updated_at_epoch_s: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FlowIndex {
    version: u32,
    flows: HashMap<String, FlowMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TranscriptLine {
    ts_epoch_s: u64,
    message: Message,
}

struct IndexLockGuard {
    path: PathBuf,
}

impl Drop for IndexLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct FlowStore {
    flows_root: PathBuf,
    index_path: PathBuf,
    lock_path: PathBuf,
}

impl FlowStore {
    /// Initialize flow store under `<home>/state/flows`.
    pub fn new(home: &Path) -> Result<Self> {
        let flows_root = home.join("state").join("flows");
        fs::create_dir_all(&flows_root)
            .with_context(|| format!("failed to create flows root at {}", flows_root.display()))?;

        let index_path = flows_root.join(INDEX_FILENAME);
        if !index_path.exists() {
            let index = FlowIndex {
                version: INDEX_VERSION,
                flows: HashMap::new(),
            };
            Self::write_index_atomic(&index_path, &index)?;
        }

        Ok(Self {
            lock_path: flows_root.join(INDEX_LOCK_FILENAME),
            flows_root,
            index_path,
        })
    }

    /// Run lightweight flow-store health checks (index load + writable probe).
    pub fn health_check(&self) -> Result<()> {
        let _ = self.load_index()?;
        let probe_dir = self.flows_root.join(".probe");
        fs::create_dir_all(&probe_dir)?;
        fs::remove_dir_all(&probe_dir)?;
        Ok(())
    }

    /// Load recent transcript messages for a flow.
    pub fn load_messages(&self, flow_key: &str, max_messages: usize) -> Result<Vec<Message>> {
        let index = self.load_index()?;
        let Some(meta) = index.flows.get(flow_key) else {
            return Ok(Vec::new());
        };

        let transcript_path = self.flows_root.join(&meta.transcript_relpath);
        if !transcript_path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&transcript_path).with_context(|| {
            format!("failed to open transcript at {}", transcript_path.display())
        })?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: TranscriptLine = serde_json::from_str(&line).with_context(|| {
                format!("failed to parse transcript line for flow '{}'", flow_key)
            })?;
            messages.push(parsed.message);
        }

        if messages.len() > max_messages {
            let start = messages.len() - max_messages;
            return Ok(messages.split_off(start));
        }
        Ok(messages)
    }

    /// Append one message to a flow transcript and update flow index metadata.
    pub fn append_message(&self, flow_key: &str, agent_id: &str, message: &Message) -> Result<()> {
        let _guard = self.acquire_index_lock()?;
        let mut index = self.load_index()?;
        let now = epoch_s_now();

        let relpath = self.resolve_transcript_relpath(flow_key);
        let flow_dir = self.flows_root.join(
            Path::new(&relpath)
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        );
        fs::create_dir_all(&flow_dir)?;

        let meta = index
            .flows
            .entry(flow_key.to_string())
            .or_insert_with(|| FlowMetadata {
                flow_key: flow_key.to_string(),
                agent_id: agent_id.to_string(),
                transcript_relpath: relpath.clone(),
                message_count: 0,
                token_estimate: 0,
                updated_at_epoch_s: now,
            });

        if meta.transcript_relpath.is_empty() {
            meta.transcript_relpath = relpath;
        }

        let transcript_path = self.flows_root.join(&meta.transcript_relpath);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
            .with_context(|| {
                format!(
                    "failed to open transcript for append at {}",
                    transcript_path.display()
                )
            })?;

        let line = TranscriptLine {
            ts_epoch_s: now,
            message: message.clone(),
        };
        let encoded = serde_json::to_string(&line)?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;

        meta.message_count += 1;
        meta.token_estimate += estimate_tokens_approx_min1(&message.content) as u64;
        meta.updated_at_epoch_s = now;

        self.write_index(&index)?;
        Ok(())
    }

    /// Resolve a deterministic flow key from scope and sender identity.
    pub fn resolve_flow_key(
        agent_id: &str,
        scope: &str,
        sender: &Recipient,
        manual_session_id: Option<&str>,
    ) -> String {
        let base = match scope {
            "main" => format!("{}:main", agent_id),
            "per-pipe-sender" => format!("{}:{}:{}", agent_id, sender.pipe_id, sender.peer_id),
            "per-group" => format!(
                "{}:{}:{}",
                agent_id,
                sender.pipe_id,
                sender
                    .thread_id
                    .as_deref()
                    .or(sender.account_id.as_deref())
                    .unwrap_or(sender.peer_id.as_str())
            ),
            _ => format!("{}:{}", agent_id, sender.peer_id), // per-sender default
        };

        match manual_session_id {
            Some(session) if !session.is_empty() => format!("{}:{}", base, session),
            _ => base,
        }
    }

    fn resolve_transcript_relpath(&self, flow_key: &str) -> String {
        let safe = sanitize_flow_component(flow_key);
        format!("{}/{}", safe, TRANSCRIPT_FILENAME)
    }

    fn acquire_index_lock(&self) -> Result<IndexLockGuard> {
        let max_attempts = 80;
        let sleep = Duration::from_millis(25);
        for _ in 0..max_attempts {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.lock_path)
            {
                Ok(_) => {
                    return Ok(IndexLockGuard {
                        path: self.lock_path.clone(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(sleep);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(anyhow::anyhow!(
            "timed out acquiring flow index lock at {}",
            self.lock_path.display()
        ))
    }

    fn load_index(&self) -> Result<FlowIndex> {
        let raw = fs::read_to_string(&self.index_path)
            .with_context(|| format!("failed to read {}", self.index_path.display()))?;
        let mut index: FlowIndex = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.index_path.display()))?;
        if index.version == 0 {
            index.version = INDEX_VERSION;
        }
        Ok(index)
    }

    fn write_index(&self, index: &FlowIndex) -> Result<()> {
        Self::write_index_atomic(&self.index_path, index)
    }

    fn write_index_atomic(path: &Path, index: &FlowIndex) -> Result<()> {
        let tmp_path = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(index)?;
        fs::write(&tmp_path, body)
            .with_context(|| format!("failed to write tmp index {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to replace index {}", path.display()))?;
        Ok(())
    }
}

fn sanitize_flow_component(flow_key: &str) -> String {
    let mut prefix = String::new();
    for ch in flow_key.chars().take(48) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            prefix.push(ch);
        } else {
            prefix.push('_');
        }
    }
    if prefix.is_empty() {
        prefix.push_str("flow");
    }

    let mut hasher = DefaultHasher::new();
    flow_key.hash(&mut hasher);
    let hash = hasher.finish();

    format!("{}-{:x}", prefix, hash)
}

fn epoch_s_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tengu_core::types::Recipient;

    #[test]
    fn resolve_flow_key_per_sender_scope() {
        let sender = Recipient {
            pipe_id: "cli".to_string(),
            peer_id: "local".to_string(),
            account_id: None,
            thread_id: None,
        };
        let key = FlowStore::resolve_flow_key("main", "per-sender", &sender, None);
        assert_eq!(key, "main:local");
    }

    #[test]
    fn sanitize_flow_component_is_stable_and_safe() {
        let a = sanitize_flow_component("agent:peer/with spaces");
        let b = sanitize_flow_component("agent:peer/with spaces");
        assert_eq!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }
}
