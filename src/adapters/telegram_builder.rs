//! Telegram adapter — pipe, commands, and runtime in one module.
//!
//! Provides the full Telegram integration:
//!
//! - **TelegramPipe** — concrete struct for chat actions, inline approvals,
//!   text/media sending, and teloxide-based message dispatch.
//! - **Inline keyboard approval** — tools with `requires_approval: true` prompt
//!   the user with Approve/Deny buttons. 60-second timeout auto-denies.
//! - **Typing indicator** — runs as an independent `tokio::spawn` task.
//! - **File attachments** — documents and photos downloaded and saved to
//!   `{workspace}/.tengu-attachments/`.
//! - **Message chunking** — long responses split at `\n\n` boundaries, max 4000
//!   chars per chunk (Telegram's 4096 limit with safety margin).
//! - **Per-user state** — each Telegram user gets their own `ChatLoopState`.
//! - **Secret redaction** — all outbound text passes through `SecretRegistry::redact`.
//! - **Hot-reload** — skills are re-scanned on each message if files changed.
//! - **Multi-agent routing** — `@role: message` targeting, automatic classification,
//!   `/team` orchestration via event-bus.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::adapters::channel_runtime;
use crate::adapters::engine_builder::build_engine;
use crate::adapters::skill_builder::{
    FileSystemSkillSource, SkillCommandMatch, SkillCommandRouter, SkillRegistry,
};
use crate::adapters::chat_builder::{
    needs_fresh_history_grounding, handle_chat_command, CommandResult,
    ChatRuntimeService, EngineInfo,
};
use crate::adapters::engine_builder::{SanitizedToolExecutor, ToolExecutor};
use crate::adapters::flow_builder::{resolve_flow_compaction_policy, resolve_history_turn_limit};
use crate::adapters::memory_builder::MemoryService;
use crate::adapters::ports::{ToolActivityPort, ToolApprovalPort};
use crate::adapters::approval::AllowAllApproval;
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::config::Config;
use crate::adapters::types::{
    ChatLoopState, DeliveryOptions, InboundMessage, MediaPayload, Recipient, RegisteredTool,
    ToolCall,
};
use crate::adapters::Engine;

// ===========================================================================
// Constants
// ===========================================================================

/// Maximum characters per Telegram message (with safety margin).
const TELEGRAM_MAX_LEN: usize = 4000;

/// Timeout for waiting on user approval via inline keyboard.
const APPROVAL_TIMEOUT_SECS: u64 = 60;

/// How often to sweep for idle user states.
const EVICTION_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Idle threshold before evicting a user state.
const IDLE_EVICTION_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(1800);

// ===========================================================================
// TelegramPipe — low-level Telegram API wrapper
// ===========================================================================

/// Map of approval_id → oneshot sender for resolving inline keyboard responses.
type PendingApprovals = Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

/// Telegram bot pipe backed by teloxide.
struct TelegramPipe {
    token: String,
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    pending_approvals: Option<PendingApprovals>,
    turn_cancel: Option<Arc<AtomicBool>>,
}

impl TelegramPipe {
    fn with_approvals(token: String, pending: PendingApprovals, cancel: Arc<AtomicBool>) -> Self {
        Self {
            token,
            shutdown: Arc::new(Mutex::new(None)),
            pending_approvals: Some(pending),
            turn_cancel: Some(cancel),
        }
    }

    async fn connect(
        &self,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::requests::Requester;

        let bot = Bot::new(&self.token);
        let me = bot
            .get_me()
            .await
            .map_err(|e| anyhow::anyhow!("Invalid Telegram bot token: {}", e))?;
        info!(bot = %me.username(), "Telegram bot authenticated");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown.lock().await = Some(shutdown_tx);

        let inbound_tx = inbound_tx.clone();
        let pending_approvals = self.pending_approvals.clone();
        let turn_cancel = self.turn_cancel.clone();

        tokio::spawn(async move {
            type MediaGroupBuffer = Arc<Mutex<HashMap<String, MediaGroupState>>>;

            struct MediaGroupState {
                text: String,
                media: Vec<MediaPayload>,
                chat_id: String,
                sender_id: String,
            }

            let media_groups: MediaGroupBuffer = Arc::new(Mutex::new(HashMap::new()));

            let cancel_for_handler = turn_cancel.clone();
            let groups_ref = Arc::clone(&media_groups);
            let msg_handler =
                Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
                    let tx = inbound_tx.clone();
                    let cancel = cancel_for_handler.clone();
                    let groups = Arc::clone(&groups_ref);
                    async move {
                        let text = msg.text().or(msg.caption()).unwrap_or_default().to_string();

                        if text == "/stop" || text.starts_with("/stop@") {
                            if let Some(ref flag) = cancel {
                                flag.store(true, Ordering::Relaxed);
                            }
                            let chat_id = msg.chat.id.0;
                            let _ = bot
                                .send_message(
                                    teloxide::types::ChatId(chat_id),
                                    "⏹ Stopping current operation...",
                                )
                                .await;
                            return Ok(());
                        }

                        let mut media_payloads: Vec<MediaPayload> = Vec::new();

                        if let Some(doc) = msg.document() {
                            match download_telegram_file(
                                &bot,
                                &doc.file.id,
                                doc.mime_type.as_ref().map(|m| m.to_string()),
                                doc.file_name.clone(),
                            )
                            .await
                            {
                                Ok(payload) => media_payloads.push(payload),
                                Err(e) => error!("Failed to download Telegram document: {}", e),
                            }
                        }

                        if let Some(photos) = msg.photo() {
                            if let Some(photo) = photos.last() {
                                match download_telegram_file(
                                    &bot,
                                    &photo.file.id,
                                    Some("image/jpeg".to_string()),
                                    Some("photo.jpg".to_string()),
                                )
                                .await
                                {
                                    Ok(payload) => media_payloads.push(payload),
                                    Err(e) => error!("Failed to download Telegram photo: {}", e),
                                }
                            }
                        }

                        if text.is_empty() && media_payloads.is_empty() {
                            return Ok::<(), teloxide::RequestError>(());
                        }

                        let chat_id = msg.chat.id.0.to_string();
                        let sender_id = msg
                            .from
                            .as_ref()
                            .map(|u| u.id.0.to_string())
                            .unwrap_or_else(|| chat_id.clone());

                        if let Some(group_id) = msg.media_group_id() {
                            let group_id = group_id.to_string();
                            let is_first = {
                                let mut map = groups.lock().await;
                                let entry = map.entry(group_id.clone()).or_insert_with(|| {
                                    MediaGroupState {
                                        text: String::new(),
                                        media: Vec::new(),
                                        chat_id: chat_id.clone(),
                                        sender_id: sender_id.clone(),
                                    }
                                });
                                if !text.is_empty() && entry.text.is_empty() {
                                    entry.text = text;
                                }
                                entry.media.extend(media_payloads);
                                entry.media.len() == 1
                            };

                            if is_first {
                                let flush_tx = tx.clone();
                                let flush_groups = Arc::clone(&groups);
                                let flush_group_id = group_id;
                                tokio::spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_millis(1500))
                                        .await;
                                    let state =
                                        flush_groups.lock().await.remove(&flush_group_id);
                                    if let Some(state) = state {
                                        debug!(
                                            group_id = %flush_group_id,
                                            attachments = state.media.len(),
                                            "Flushing media group"
                                        );
                                        let inbound = InboundMessage {
                                            sender: Recipient {
                                                pipe_id: "telegram".to_string(),
                                                peer_id: state.sender_id,
                                                account_id: None,
                                                thread_id: Some(state.chat_id),
                                            },
                                            content: state.text,
                                            timestamp: chrono::Utc::now(),
                                            media: if state.media.is_empty() {
                                                None
                                            } else {
                                                Some(state.media)
                                            },
                                        };
                                        if flush_tx.send(inbound).await.is_err() {
                                            error!(
                                                "Failed to forward media group to inbound channel"
                                            );
                                        }
                                    }
                                });
                            }
                            return Ok(());
                        }

                        debug!(
                            chat_id = %chat_id,
                            sender = %sender_id,
                            attachments = media_payloads.len(),
                            "Telegram message received"
                        );

                        let inbound = InboundMessage {
                            sender: Recipient {
                                pipe_id: "telegram".to_string(),
                                peer_id: sender_id,
                                account_id: None,
                                thread_id: Some(chat_id),
                            },
                            content: text,
                            timestamp: chrono::Utc::now(),
                            media: if media_payloads.is_empty() {
                                None
                            } else {
                                Some(media_payloads)
                            },
                        };

                        if tx.send(inbound).await.is_err() {
                            error!("Failed to forward Telegram message to inbound channel");
                        }
                        Ok(())
                    }
                });

            let callback_handler = Update::filter_callback_query().endpoint(
                move |q: teloxide::types::CallbackQuery, bot: Bot| {
                    let pending = pending_approvals.clone();
                    async move {
                        if let Some(data) = q.data {
                            let (approved, approval_id) =
                                if let Some(id) = data.strip_prefix("approve:") {
                                    (true, id.to_string())
                                } else if let Some(id) = data.strip_prefix("deny:") {
                                    (false, id.to_string())
                                } else {
                                    return Ok::<(), teloxide::RequestError>(());
                                };

                            let answer_text = if approved { "Approved" } else { "Denied" };
                            let _ = bot.answer_callback_query(&q.id).text(answer_text).await;

                            if let Some(msg) = q.message {
                                if let Some(text) = msg.regular_message().and_then(|m| m.text()) {
                                    let status = if approved {
                                        "✅ Approved"
                                    } else {
                                        "❌ Denied"
                                    };
                                    let _ = bot
                                        .edit_message_text(
                                            msg.chat().id,
                                            msg.id(),
                                            format!("{}\n\n{}", text, status),
                                        )
                                        .await;
                                }
                            }

                            if let Some(ref p) = pending {
                                if let Some(tx) = p.lock().unwrap().remove(&approval_id) {
                                    let _ = tx.send(approved);
                                }
                            }
                        }
                        Ok(())
                    }
                },
            );

            let handler = dptree::entry().branch(msg_handler).branch(callback_handler);

            let mut dispatcher = Dispatcher::builder(bot, handler)
                .enable_ctrlc_handler()
                .build();

            tokio::select! {
                _ = dispatcher.dispatch() => {}
                _ = &mut shutdown_rx => {
                    info!("Telegram pipe shutting down");
                }
            }
        });

        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    async fn send_chat_action(&self, target: &Recipient) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::{ChatAction, ChatId};

        let bot = Bot::new(&self.token);
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id"))?;
        bot.send_chat_action(ChatId(chat_id), ChatAction::Typing)
            .await
            .map_err(|e| anyhow::anyhow!("send_chat_action failed: {}", e))?;
        Ok(())
    }

    fn remove_pending_approval(&self, approval_id: &str) {
        if let Some(ref pending) = self.pending_approvals {
            pending.lock().unwrap().remove(approval_id);
        }
    }

    async fn send_inline_approval(
        &self,
        target: &Recipient,
        approval_id: &str,
        text: &str,
    ) -> Result<tokio::sync::oneshot::Receiver<bool>> {
        use teloxide::prelude::*;
        use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup};

        let pending = self
            .pending_approvals
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Approval support not configured"))?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.lock().unwrap().insert(approval_id.to_string(), tx);

        let bot = Bot::new(&self.token);
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id"))?;

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("✅ Approve", format!("approve:{}", approval_id)),
            InlineKeyboardButton::callback("❌ Deny", format!("deny:{}", approval_id)),
        ]]);

        bot.send_message(ChatId(chat_id), text)
            .reply_markup(keyboard)
            .await
            .map_err(|e| anyhow::anyhow!("send_inline_approval failed: {}", e))?;

        Ok(rx)
    }

    async fn send_text(&self, target: &Recipient, text: &str, _opts: &DeliveryOptions) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::ChatId;

        let bot = Bot::new(&self.token);
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Telegram chat_id"))?;

        bot.send_message(ChatId(chat_id), text)
            .await
            .map_err(|e| anyhow::anyhow!("Telegram send_text failed: {}", e))?;

        Ok(())
    }
}

/// Download a file from Telegram servers given its file_id.
async fn download_telegram_file(
    bot: &teloxide::Bot,
    file_id: &str,
    mime_type: Option<String>,
    filename: Option<String>,
) -> std::result::Result<MediaPayload, Box<dyn std::error::Error + Send + Sync>> {
    use futures::TryStreamExt;
    use teloxide::net::Download;
    use teloxide::requests::Requester;

    let tf = bot.get_file(file_id).await?;
    let bytes: Vec<u8> = bot
        .download_file_stream(&tf.path)
        .try_fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await?;

    debug!(
        file_id = %file_id,
        size = bytes.len(),
        filename = ?filename,
        "Downloaded Telegram file"
    );

    Ok(MediaPayload {
        mime_type: mime_type.unwrap_or_else(|| "application/octet-stream".to_string()),
        data: bytes,
        filename,
    })
}

// ===========================================================================
// Port adapters
// ===========================================================================

/// Shared state for the current recipient — updated before each engine turn.
type CurrentRecipient = Arc<std::sync::Mutex<Option<Recipient>>>;

/// Telegram adapter for tool progress — logs to tracing only, no chat messages.
struct TelegramToolActivityAdapter {
    agent_label: String,
    tools: Vec<RegisteredTool>,
}

impl ToolActivityPort for TelegramToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let (title, detail) =
            crate::adapters::tool_builder::build_tool_activity_text(call, &self.tools);
        let mut text = format!("[{}] {}", self.agent_label, title);
        if let Some(detail) = detail {
            text.push_str(": ");
            text.push_str(&detail);
        }
        tracing::info!(
            agent = %self.agent_label,
            tool = %call.name,
            message = %text,
            "Tool activity"
        );
    }
}

fn make_tool_activity_adapter(
    agent_label: impl Into<String>,
    tools: &[RegisteredTool],
) -> Arc<dyn ToolActivityPort> {
    Arc::new(TelegramToolActivityAdapter {
        agent_label: agent_label.into(),
        tools: tools.to_vec(),
    })
}

/// Telegram inline keyboard approval adapter.
struct TelegramInlineApprovalAdapter {
    pipe: Arc<TelegramPipe>,
    current_recipient: CurrentRecipient,
    cancel: Arc<AtomicBool>,
}

impl ToolApprovalPort for TelegramInlineApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        let recipient = self
            .current_recipient
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No recipient set for approval"))?;

        let (title, description, preview) = crate::adapters::tool_builder::build_approval_text(call);
        let approval_id = format!("tool_{}", uuid::Uuid::new_v4().simple());
        let mut text = format!("🔐 *{}*\n{}", title, description);
        if !preview.is_empty() {
            let truncated = if preview.len() > 500 {
                let mut end = 500;
                while end > 0 && !preview.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &preview[..end])
            } else {
                preview
            };
            text.push_str(&format!("\n```\n{}\n```", truncated));
        }

        let pipe = Arc::clone(&self.pipe);
        let aid = approval_id.clone();

        tokio::task::block_in_place(move || {
            let handle = tokio::runtime::Handle::current();
            handle.block_on(async {
                match pipe.send_inline_approval(&recipient, &aid, &text).await {
                    Ok(rx) => {
                        let cancel_flag = Arc::clone(&self.cancel);
                        let cancel_check = async {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                if cancel_flag.load(Ordering::Relaxed) {
                                    return;
                                }
                            }
                        };

                        let result = tokio::select! {
                            approval = tokio::time::timeout(
                                std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                rx,
                            ) => {
                                match approval {
                                    Ok(Ok(approved)) => {
                                        info!(tool = %call.name, approved, "Inline keyboard approval response");
                                        if !approved {
                                            self.cancel.store(true, Ordering::Relaxed);
                                        }
                                        approved
                                    }
                                    Ok(Err(_)) => {
                                        warn!(tool = %call.name, "Approval channel closed — denying");
                                        self.cancel.store(true, Ordering::Relaxed);
                                        false
                                    }
                                    Err(_) => {
                                        warn!(tool = %call.name, "Approval timed out — denying");
                                        self.cancel.store(true, Ordering::Relaxed);
                                        false
                                    }
                                }
                            }
                            _ = cancel_check => {
                                info!(tool = %call.name, "Approval cancelled by /stop");
                                false
                            }
                        };
                        pipe.remove_pending_approval(&aid);
                        Ok(result)
                    }
                    Err(e) => {
                        error!(tool = %call.name, error = %e, "Failed to send approval request — denying");
                        Ok(false)
                    }
                }
            })
        })
    }
}

/// Approval adapter that only prompts the user for tools in the `approve_only` set.
struct FilteredApprovalAdapter {
    inner: Arc<dyn ToolApprovalPort>,
    approve_only: HashSet<String>,
}

impl ToolApprovalPort for FilteredApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        if self.approve_only.contains(&call.name) {
            self.inner.request_tool_approval(call)
        } else {
            Ok(true)
        }
    }
}

// ===========================================================================
// Per-agent runtime state
// ===========================================================================

/// Per-agent runtime state for multi-agent Telegram support.
struct TelegramAgentState {
    agent_id: String,
    agent_config: crate::adapters::config::AgentConfig,
    engine: Arc<dyn Engine>,
    engine_info: EngineInfo,
    workspace: Option<std::path::PathBuf>,
    base_tools: Vec<RegisteredTool>,
    skill_source: Option<FileSystemSkillSource>,
    skill_registry: SkillRegistry,
    current_tools: Vec<RegisteredTool>,
    current_system_prompt: String,
    advertise_workspace_tools: bool,
    history_turn_limit: usize,
    compaction_policy: crate::adapters::types::FlowCompactionPolicy,
    role: Option<String>,
}

// ===========================================================================
// TelegramTaskExecutor (for event-bus orchestration)
// ===========================================================================

struct TelegramTaskExecutor {
    engine: Arc<dyn Engine>,
    tools: Vec<crate::adapters::types::ToolDef>,
    tool_executor: Option<Arc<dyn ToolExecutor>>,
    system_prompt: String,
    workspace: Option<std::path::PathBuf>,
    secret_registry: Arc<SecretRegistry>,
    cancel: Arc<AtomicBool>,
    token_budget: Option<u32>,
    pipe: Arc<TelegramPipe>,
    sender: Recipient,
    agent_label: String,
}

#[async_trait::async_trait]
impl crate::adapters::types::AgentTaskExecutor for TelegramTaskExecutor {
    async fn execute(
        &self,
        description: &str,
    ) -> std::result::Result<(String, Vec<(String, String)>), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("Stopped by user".into());
        }

        let typing_pipe = Arc::clone(&self.pipe);
        let typing_sender = self.sender.clone();
        let typing_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                let _ = typing_pipe.send_chat_action(&typing_sender).await;
            }
        });

        let obs_sr = Arc::clone(&self.secret_registry);
        let obs_label = self.agent_label.clone();
        let observer = move |call: &ToolCall, result: &str| {
            let preview = channel_runtime::truncate_summary(&obs_sr.redact(result), 500);
            tracing::debug!(
                agent = %obs_label,
                tool = %call.name,
                result = %preview,
                "Orchestrated Telegram tool result"
            );
        };

        let messages = vec![crate::adapters::types::Message {
            role: crate::adapters::types::Role::User,
            content: description.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];

        let context = crate::adapters::EngineContext {
            workspace: self.workspace.clone(),
            system_prompt: Some(self.system_prompt.clone()),
        };

        let sanitized = self
            .tool_executor
            .as_ref()
            .map(|e| SanitizedToolExecutor::new(e.as_ref(), &self.secret_registry));

        let result = crate::adapters::engine_builder::collect_engine_response(
            self.engine.as_ref(),
            &messages,
            &self.tools,
            &context,
            sanitized.as_ref().map(|s| s as &dyn ToolExecutor),
            Some(&observer),
            Some(&self.cancel),
            self.token_budget,
        )
        .await;

        typing_handle.abort();

        match result {
            Ok(resp) => {
                let mut combined = resp.text;
                if !resp.tool_outcomes.is_empty() {
                    combined.push_str("\n\n## Tool Results\n");
                    for (name, result) in &resp.tool_outcomes {
                        combined.push_str(&format!(
                            "### {}\n{}\n",
                            name,
                            channel_runtime::truncate_output(result, 2000),
                        ));
                    }
                }
                Ok((combined, resp.tool_outcomes))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

// ===========================================================================
// Command handlers
// ===========================================================================

/// Orchestrate a multi-agent goal via the event-bus architecture.
async fn handle_team(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    goal: &str,
    media: Option<&Vec<MediaPayload>>,
    delivery_opts: &DeliveryOptions,
    agent_states: &mut HashMap<String, TelegramAgentState>,
    default_agent_id: &str,
    agent_descriptions: &HashMap<String, String>,
    role_to_agent: &HashMap<String, String>,
    current_recipient: &CurrentRecipient,
    memory_handle: &Option<Arc<crate::adapters::memory_builder::MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval_adapter: &Arc<dyn ToolApprovalPort>,
    turn_cancel: &Arc<AtomicBool>,
    dedicated_planner_engine: Option<&dyn Engine>,
) -> Result<()> {
    use crate::adapters::agent_builder::agent_worker;
    use crate::adapters::event_orchestrator::{self, OrchestratorConfig};
    use crate::adapters::types::{AgentTaskExecutor, EventBus, Plan, Task, TaskStatus};

    *current_recipient.lock().unwrap() = Some(sender.clone());

    // Handle attachments.
    let mut attachment_notes: Vec<String> = Vec::new();
    if let Some(media) = media {
        let ws = agent_states
            .get(default_agent_id)
            .and_then(|a| a.workspace.as_ref());
        if let Some(ws) = ws {
            let attachments_dir = ws.join(".tengu-attachments");
            std::fs::create_dir_all(&attachments_dir).ok();
            for m in media {
                let fname =
                    sanitize_attachment_filename(m.filename.as_deref().unwrap_or("attachment"));
                let path = attachments_dir.join(&fname);
                match std::fs::write(&path, &m.data) {
                    Ok(()) => {
                        info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment for orchestration");
                        attachment_notes.push(format!(
                            "[Attached file: {} ({}, {} bytes)]",
                            path.display(),
                            m.mime_type,
                            m.data.len()
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to save Telegram attachment for orchestration");
                    }
                }
            }
        }
    }

    let goal = if attachment_notes.is_empty() {
        goal.to_string()
    } else {
        format!("{}\n{}", attachment_notes.join("\n"), goal)
    };

    pipe.send_text(sender, "Planning…", delivery_opts).await?;

    let fallback_engine;
    let planner_engine: &dyn Engine = if let Some(dedicated) = dedicated_planner_engine {
        dedicated
    } else {
        match agent_states.get(default_agent_id) {
            Some(a) => {
                fallback_engine = a.engine.clone();
                fallback_engine.as_ref()
            }
            None => {
                pipe.send_text(sender, "Planner engine unavailable.", delivery_opts)
                    .await?;
                return Ok(());
            }
        }
    };

    // RAG: recall orchestrator topic overviews for planner context.
    let enriched_goal = if let Some(ref handle) = memory_handle {
        let mem_svc = MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
        let mut filter = std::collections::HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("source".into(), "orchestrator".into());
        match mem_svc.recall_filtered(&goal, 3, 600, &filter).await {
            Ok(results) if !results.is_empty() => {
                let mut enriched = String::from(
                    "## Relevant Prior Work\n\
                     Background only. Use this for continuity or implementation hints.\n\
                     Do NOT treat it as additional requested deliverables, and do NOT expand scope beyond the current goal.\n",
                );
                for r in &results {
                    enriched.push_str(&format!("- {}\n", r.entry.content));
                }
                enriched.push_str(&format!("\n## Current Goal\n{}", goal));
                enriched
            }
            _ => goal.clone(),
        }
    } else {
        goal.clone()
    };

    let mut tasks = match crate::adapters::task_builder::generate_plan(
        planner_engine,
        &enriched_goal,
        agent_descriptions,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            pipe.send_text(
                sender,
                &format!("Failed to generate plan: {}", e),
                delivery_opts,
            )
            .await?;
            return Ok(());
        }
    };

    // Build role dependency constraints from agent configs.
    let role_deps: crate::adapters::types::RoleDependencies = agent_states
        .iter()
        .filter_map(|(_, astate)| {
            let role_key = astate.role.as_deref()?;
            if astate.agent_config.requires.is_empty() {
                return None;
            }
            Some((role_key.to_string(), astate.agent_config.requires.clone()))
        })
        .collect();

    let role_refs = crate::adapters::task_builder::resolve_role_refs_in_depends(&mut tasks);
    if role_refs > 0 {
        tracing::info!(role_refs, "Resolved role-name references in depends_on");
    }
    let repaired = crate::adapters::task_builder::repair_plan_dependencies(&mut tasks, &role_deps);
    if repaired > 0 {
        tracing::info!(repaired, "Auto-repaired plan: added {} dependency edges", repaired);
    }
    if let Err(e) =
        crate::adapters::task_builder::validate_plan_dependencies(&tasks, &role_deps)
    {
        pipe.send_text(sender, &format!("Plan rejected: {}", e), delivery_opts)
            .await?;
        return Ok(());
    }

    // Convert PlanTask → Plan for event-bus execution.
    let live_tasks: Vec<Task> = tasks
        .iter()
        .map(|pt| Task {
            id: pt.id.clone(),
            role: pt.role.clone(),
            description: pt.task.clone(),
            depends_on: pt.depends_on.clone(),
            status: TaskStatus::Pending,
            output: None,
            artifacts: HashMap::new(),
            assigned_agent: None,
            started_at: None,
            attempt: 0,
        })
        .collect();
    let live_plan = Plan::new(enriched_goal.clone(), live_tasks);

    // Send plan summary.
    let mut plan_text = format!("Plan ({} tasks, DAG dispatch):\n", tasks.len());
    for pt in &tasks {
        let deps = if pt.depends_on.is_empty() {
            String::new()
        } else {
            format!(" (after: {})", pt.depends_on.join(", "))
        };
        plan_text.push_str(&format!("  - [{}] {}{}\n", pt.role, pt.task, deps));
    }
    pipe.send_text(sender, &plan_text, delivery_opts).await?;

    turn_cancel.store(false, Ordering::Relaxed);

    // Determine which agents are needed and hot-reload skills.
    let needed_agent_ids: Vec<String> = tasks
        .iter()
        .filter_map(|t| role_to_agent.get(&t.role).cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    for agent_id in &needed_agent_ids {
        if let Some(agent) = agent_states.get_mut(agent_id) {
            if let Some(ref src) = agent.skill_source {
                if agent.skill_registry.reload(src) {
                    agent.current_tools = channel_runtime::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                    );
                    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
                    );
                }
            }
        }
    }

    // Build TelegramTaskExecutor per agent.
    let mut executors: HashMap<String, Arc<dyn AgentTaskExecutor>> = HashMap::new();
    for agent_id in &needed_agent_ids {
        let agent = match agent_states.get(agent_id) {
            Some(a) => a,
            None => continue,
        };
        let agent_label = agent
            .agent_config
            .identity
            .name
            .clone()
            .unwrap_or_else(|| agent.agent_id.clone());

        let activity_adapter =
            make_tool_activity_adapter(agent_label.clone(), &agent.current_tools);
        let tool_executor: Option<Arc<dyn ToolExecutor>> =
            agent.workspace.as_ref().and_then(|ws| {
                channel_runtime::build_tool_executor(
                    ws,
                    &agent.current_tools,
                    &agent.skill_registry,
                    memory_handle,
                    secret_registry,
                    Arc::clone(approval_adapter),
                    activity_adapter,
                    Some(Arc::clone(turn_cancel)),
                    None,
                )
                .map(|e| Arc::new(e) as Arc<dyn ToolExecutor>)
            });

        executors.insert(
            agent_id.clone(),
            Arc::new(TelegramTaskExecutor {
                engine: Arc::clone(&agent.engine),
                tools: channel_runtime::tool_defs(&agent.current_tools),
                tool_executor,
                system_prompt: agent.current_system_prompt.clone(),
                workspace: agent.workspace.clone(),
                secret_registry: Arc::clone(secret_registry),
                cancel: Arc::clone(turn_cancel),
                token_budget: Some(agent.agent_config.limits.max_tokens_per_flow as u32),
                pipe: Arc::clone(pipe),
                sender: sender.clone(),
                agent_label,
            }),
        );
    }

    // Wire EventBus and spawn agent workers.
    let EventBus {
        mut orchestrator_rx,
        orchestrator_tx,
        agent_txs,
        mut agent_rxs,
    } = EventBus::new(&needed_agent_ids, 32);

    let mut worker_handles = vec![];
    for (agent_id, executor) in executors {
        let rx = agent_rxs.remove(&agent_id).unwrap();
        let tx = orchestrator_tx.clone();
        worker_handles.push(tokio::spawn(agent_worker(agent_id, rx, tx, executor)));
    }

    drop(orchestrator_tx);

    let ev_config = OrchestratorConfig {
        max_retries: 2,
        task_timeout: std::time::Duration::from_secs(300),
        cancel: Some(Arc::clone(turn_cancel)),
        ..OrchestratorConfig::default()
    };
    let outcome = event_orchestrator::run_orchestrator(
        live_plan,
        &mut orchestrator_rx,
        &agent_txs,
        role_to_agent,
        &ev_config,
    )
    .await;

    drop(agent_txs);
    for handle in worker_handles {
        let _ = handle.await;
    }

    match outcome {
        Ok(plan_outcome) => {
            // Auto-summarize in memory.
            if let Some(ref handle) = memory_handle {
                let mut mem_summary = format!("Goal: {}\n\nResults:\n", goal);
                for t in &plan_outcome.tasks {
                    let status = match t.status {
                        TaskStatus::Completed => "ok",
                        TaskStatus::Failed => "failed",
                        TaskStatus::Skipped => "skipped",
                        _ => "unknown",
                    };
                    let output_preview = t
                        .output
                        .as_deref()
                        .map(|o| channel_runtime::truncate_output(o, 500))
                        .unwrap_or_default();
                    mem_summary
                        .push_str(&format!("- {} ({}): {}\n", t.id, status, output_preview));
                }
                let mem_svc =
                    MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
                let mut meta = std::collections::HashMap::new();
                meta.insert("kind".into(), "topic_overview".into());
                meta.insert("source".into(), "orchestrator".into());
                meta.insert("goal".into(), goal.to_string());
                if let Some(ws) = agent_states
                    .get(default_agent_id)
                    .and_then(|a| a.workspace.as_ref())
                {
                    if let Some(name) = ws.file_name() {
                        meta.insert(
                            "workspace_id".into(),
                            name.to_string_lossy().to_string(),
                        );
                    }
                }
                match mem_svc
                    .remember_with_metadata(&mem_summary, "orchestrator", meta)
                    .await
                {
                    Ok(id) => tracing::debug!(id, "Stored topic overview in memory"),
                    Err(e) => tracing::warn!(error = %e, "Failed to store topic overview"),
                }
            }

            // Build Telegram summary.
            let completed = plan_outcome
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Completed)
                .count();
            let mut summary = format!(
                "Team completed — {}/{} tasks done.\n",
                completed,
                plan_outcome.tasks.len()
            );
            for t in &plan_outcome.tasks {
                let mark = match t.status {
                    TaskStatus::Completed => "✓",
                    TaskStatus::Failed => "✗",
                    TaskStatus::Skipped => "⊘",
                    _ => "?",
                };
                summary.push_str(&format!("\n{} {}", mark, t.id));
                if let Some(ref out) = t.output {
                    summary.push_str(&format!(
                        ": {}",
                        channel_runtime::truncate_output(out, 300)
                    ));
                }
                summary.push('\n');
            }

            let summary = secret_registry.redact(&summary);
            for chunk in channel_runtime::chunk_message(&summary, TELEGRAM_MAX_LEN) {
                let _ = pipe.send_text(sender, chunk, delivery_opts).await;
            }
        }
        Err(e) => {
            pipe.send_text(
                sender,
                &format!("Orchestration failed: {}", e),
                delivery_opts,
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_project(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    name: &str,
    delivery_opts: &DeliveryOptions,
    agent_states: &mut HashMap<String, TelegramAgentState>,
    default_agent_id: &str,
    base_workspaces: &HashMap<String, std::path::PathBuf>,
    config: &Config,
    user_states: &mut HashMap<String, ChatLoopState>,
    sender_id: &str,
) -> Result<()> {
    if name.is_empty() {
        let current = agent_states
            .get(default_agent_id)
            .and_then(|a| a.workspace.as_ref())
            .map(|w| w.display().to_string())
            .unwrap_or_else(|| "(no workspace)".into());
        pipe.send_text(
            sender,
            &format!("Current workspace: {}\n\nUsage: /project <name>", current),
            delivery_opts,
        )
        .await?;
        return Ok(());
    }

    let sanitized: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();

    let mut created = false;

    if let Some(base) = base_workspaces.values().next() {
        let project_dir = base.join(&sanitized);
        let project_template = config
            .scaffold
            .as_ref()
            .and_then(|s| s.project.as_ref());
        let project_scaffold = crate::adapters::config::ScaffoldConfig {
            root: project_dir.to_string_lossy().to_string(),
            directories: project_template
                .map(|p| p.directories.clone())
                .unwrap_or_default(),
            files: project_template
                .map(|p| p.files.clone())
                .unwrap_or_default(),
            project: None,
        };
        if let Err(e) = crate::adapters::scaffold::apply_scaffold(&project_scaffold) {
            pipe.send_text(
                sender,
                &format!("Scaffold failed: {}", e),
                delivery_opts,
            )
            .await?;
            return Ok(());
        }
    }

    for (aid, agent) in agent_states.iter_mut() {
        if let Some(base) = base_workspaces.get(aid) {
            let project_dir = base.join(&sanitized);
            agent.workspace = Some(project_dir);
            agent.skill_source = agent
                .workspace
                .as_ref()
                .map(|ws| FileSystemSkillSource::new(ws.clone()));
            if let Some(ref src) = agent.skill_source {
                agent.skill_registry.reload(src);
            }
            agent.current_tools = channel_runtime::rebuild_tools(
                &agent.base_tools,
                &agent.skill_registry,
            );
            created = true;
        }
    }

    if created {
        let prefix = format!("{}:", sender_id);
        for (key, state) in user_states.iter_mut() {
            if key.starts_with(&prefix) {
                state.reset_for_new_session();
            }
        }
        let ws_display = agent_states
            .get(default_agent_id)
            .and_then(|a| a.workspace.as_ref())
            .map(|w| w.display().to_string())
            .unwrap_or_default();
        pipe.send_text(
            sender,
            &format!(
                "Project '{}' created.\nWorkspace: {}\nConversations reset.",
                sanitized, ws_display
            ),
            delivery_opts,
        )
        .await?;
    } else {
        pipe.send_text(sender, "No workspaces configured.", delivery_opts)
            .await?;
    }
    Ok(())
}

async fn handle_agents(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    agent_states: &HashMap<String, TelegramAgentState>,
    default_agent_id: &str,
) -> Result<()> {
    let mut lines = vec!["Available agents:".to_string()];
    for (aid, astate) in agent_states {
        let role_label = astate.role.as_deref().unwrap_or("-");
        let is_default = if *aid == *default_agent_id {
            " [default]"
        } else {
            ""
        };
        let tool_count = astate.current_tools.len();
        lines.push(format!(
            "  {} (role: {}, {} tools){}",
            aid, role_label, tool_count, is_default
        ));
    }
    lines.push(String::new());
    lines.push("Direct: @role: message  or  role: message".to_string());
    lines.push("Auto:    plain messages  (plan & execute across agents)".to_string());
    lines.push("Team:    /team <goal>     (explicit team planning command)".to_string());
    lines.push("Project: /project <name>  (new project subfolder)".to_string());
    lines.push("Example: @backend_engineer: add rate limiting".to_string());
    lines.push("Example: register POI and mint the IP-NFT".to_string());
    lines.push("Example: /team build a full stack Rust app".to_string());
    pipe.send_text(sender, &lines.join("\n"), delivery_opts)
        .await?;
    Ok(())
}

async fn handle_reset(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    user_states: &mut HashMap<String, ChatLoopState>,
    sender_id: &str,
) -> Result<()> {
    let prefix = format!("{}:", sender_id);
    let mut reset_count = 0usize;
    for (key, state) in user_states.iter_mut() {
        if key.starts_with(&prefix) {
            state.reset_for_new_session();
            reset_count += 1;
        }
    }
    pipe.send_text(
        sender,
        &format!(
            "Session reset — {} agent conversation(s) cleared.",
            reset_count
        ),
        delivery_opts,
    )
    .await?;
    Ok(())
}

async fn handle_purge(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    user_states: &mut HashMap<String, ChatLoopState>,
    sender_id: &str,
    agent_states: &HashMap<String, TelegramAgentState>,
    memory_handle: &Option<Arc<crate::adapters::memory_builder::MemoryServiceHandle>>,
) -> Result<()> {
    let prefix = format!("{}:", sender_id);
    for (key, state) in user_states.iter_mut() {
        if key.starts_with(&prefix) {
            state.reset_for_new_session();
        }
    }
    let mut lines = vec!["All conversations cleared.".to_string()];
    if let Some(ref handle) = memory_handle {
        match handle.store.clear_all().await {
            Ok(()) => lines.push("Persistent memory purged.".to_string()),
            Err(e) => lines.push(format!("Memory clear failed: {}", e)),
        }
    } else {
        lines.push("No persistent memory active.".to_string());
    }

    let ws_set: HashSet<std::path::PathBuf> = agent_states
        .values()
        .filter_map(|s| s.workspace.clone())
        .collect();
    for ws in &ws_set {
        for subdir in &[".tengu-tasks", ".tengu-attachments"] {
            let p = ws.join(subdir);
            if p.exists() {
                match std::fs::remove_dir_all(&p) {
                    Ok(()) => lines.push(format!("Cleaned {}", p.display())),
                    Err(e) => lines.push(format!("Failed to clean {}: {}", p.display(), e)),
                }
            }
        }
    }

    pipe.send_text(sender, &lines.join("\n"), delivery_opts)
        .await?;
    Ok(())
}

async fn handle_reload(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    agent: &mut TelegramAgentState,
    skill_command_router: &mut SkillCommandRouter,
) -> Result<()> {
    let mut lines = Vec::new();
    if let Some(ref src) = agent.skill_source {
        if agent.skill_registry.reload(src) {
            lines.push("Skills reloaded (changes detected).".to_string());
        } else {
            lines.push("Skills reloaded (no changes).".to_string());
        }
    } else {
        lines.push("No workspace — skills unavailable.".to_string());
    }
    agent.current_tools = channel_runtime::rebuild_tools(
        &agent.base_tools,
        &agent.skill_registry,
    );
    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
        &agent.agent_config,
        agent.advertise_workspace_tools,
        &agent.skill_registry,
        &agent.current_tools,
    );
    *skill_command_router = SkillCommandRouter::from_registry(&agent.skill_registry);
    lines.push(format!("{} tool(s) active.", agent.current_tools.len()));
    pipe.send_text(sender, &lines.join("\n"), delivery_opts)
        .await?;
    Ok(())
}

async fn handle_skill_command(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    agent: &mut TelegramAgentState,
    skill_command_router: &mut SkillCommandRouter,
    skill_name: &str,
    cmd_name: &str,
    args: &str,
    user_states: &mut HashMap<String, ChatLoopState>,
    user_state_last_active: &mut HashMap<String, std::time::Instant>,
    sender_id: &str,
    active_aid: &str,
    current_recipient: &CurrentRecipient,
    memory_handle: &Option<Arc<crate::adapters::memory_builder::MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval_adapter: &Arc<dyn ToolApprovalPort>,
    turn_cancel: &Arc<AtomicBool>,
    memory_service_instance: Option<&MemoryService<'_>>,
    memory_config: &crate::adapters::config::MemoryConfig,
) -> Result<()> {
    let injected = format!(
        "[System: User invoked /{cmd} {args}. Follow the instructions in the ## Commands section of the {skill} skill.]",
        cmd = cmd_name,
        args = args,
        skill = skill_name,
    );

    if let Some(ref src) = agent.skill_source {
        if agent.skill_registry.reload(src) {
            agent.current_tools = channel_runtime::rebuild_tools(
                &agent.base_tools,
                &agent.skill_registry,
            );
            agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                &agent.agent_config,
                agent.advertise_workspace_tools,
                &agent.skill_registry,
                &agent.current_tools,
            );
            *skill_command_router = SkillCommandRouter::from_registry(&agent.skill_registry);
        }
    }

    *current_recipient.lock().unwrap() = Some(sender.clone());

    let agent_label = agent
        .agent_config
        .identity
        .name
        .clone()
        .unwrap_or_else(|| agent.agent_id.clone());
    let activity_adapter = make_tool_activity_adapter(agent_label, &agent.current_tools);
    let current_executor = agent.workspace.as_ref().and_then(|ws| {
        channel_runtime::build_tool_executor(
            ws,
            &agent.current_tools,
            &agent.skill_registry,
            memory_handle,
            secret_registry,
            Arc::clone(approval_adapter),
            activity_adapter,
            Some(Arc::clone(turn_cancel)),
            None,
        )
    });
    let sanitized_executor = current_executor
        .as_ref()
        .map(|e| SanitizedToolExecutor::new(e as &dyn ToolExecutor, secret_registry));

    turn_cancel.store(false, Ordering::Relaxed);

    let state_key = format!("{}:{}", sender_id, active_aid);
    user_state_last_active.insert(state_key.clone(), std::time::Instant::now());
    let state = user_states
        .entry(state_key)
        .or_insert_with(|| channel_runtime::create_chat_loop_state(&agent.agent_config));

    let _ = pipe.send_chat_action(sender).await;
    let t_pipe = Arc::clone(pipe);
    let t_sender = sender.clone();
    let typing_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let _ = t_pipe.send_chat_action(&t_sender).await;
        }
    });

    let active_tools = channel_runtime::tool_defs(&agent.current_tools);
    let chat_runtime = ChatRuntimeService {
        engine: agent.engine.as_ref(),
        agent_id: &agent.agent_id,
        agent_config: &agent.agent_config,
        history_turn_limit: agent.history_turn_limit,
        compaction_policy: agent.compaction_policy,
        system_prompt: agent.current_system_prompt.clone(),
        tools: &active_tools,
        tool_executor: sanitized_executor
            .as_ref()
            .map(|e| e as &dyn ToolExecutor),
        memory_service: memory_service_instance,
        max_recall_entries: memory_config.max_recall_entries,
        max_recall_tokens: memory_config.max_recall_tokens,
        tool_observer: None,
        cancel: Some(turn_cancel),
    };

    let result = chat_runtime.process_user_text(state, &injected).await;
    typing_handle.abort();

    match result {
        Ok(res) => {
            if let Some(notice) = res.system_notice {
                let _ = pipe.send_text(sender, &notice, delivery_opts).await;
            }
            if let Some(ref text) = res.assistant_text {
                let reply = secret_registry.redact(text);
                for chunk in channel_runtime::chunk_message(&reply, TELEGRAM_MAX_LEN) {
                    let _ = pipe.send_text(sender, chunk, delivery_opts).await;
                }
            }
        }
        Err(e) => {
            let _ = pipe
                .send_text(sender, &format!("Error: {}", e), delivery_opts)
                .await;
        }
    }
    Ok(())
}

async fn handle_builtin_command(
    pipe: &Arc<TelegramPipe>,
    sender: &Recipient,
    delivery_opts: &DeliveryOptions,
    text: &str,
    state: &mut ChatLoopState,
    agent: &TelegramAgentState,
    skill_command_router: &SkillCommandRouter,
) -> Result<()> {
    let skill_cmds = skill_command_router.list();
    let cmd_result = handle_chat_command(
        text,
        state,
        &agent.engine_info,
        &agent.agent_config,
        agent.history_turn_limit,
        agent.compaction_policy,
        &skill_cmds,
    );
    match cmd_result {
        CommandResult::Handled(output) => {
            let reply = output.lines.join("\n");
            pipe.send_text(sender, &reply, delivery_opts).await?;
        }
        CommandResult::NotHandled => {
            pipe.send_text(
                sender,
                "Unknown command. Type /help or /agents for available commands.",
                delivery_opts,
            )
            .await?;
        }
    }
    Ok(())
}

// ===========================================================================
// Helpers
// ===========================================================================

fn build_allowed_users(config: &Config) -> HashSet<String> {
    let mut allowed: HashSet<String> = config.telegram.allowed_users.iter().cloned().collect();
    if let Ok(env_users) = std::env::var("TENGU_TELEGRAM_ALLOWED_USERS") {
        for uid in env_users.split(',') {
            let uid = uid.trim();
            if !uid.is_empty() {
                allowed.insert(uid.to_string());
            }
        }
    }
    allowed
}

fn sanitize_attachment_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ===========================================================================
// Entry point
// ===========================================================================

/// Run the headless Telegram bot adapter (sync — call from `block_in_place`).
///
/// Creates its own tokio runtime internally so that state containing nested
/// runtimes drops in a sync context, avoiding the "Cannot drop a runtime in a
/// context where blocking is not allowed" panic.
pub(crate) fn run_telegram(config: Config, secret_registry: Arc<SecretRegistry>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Telegram runtime");

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("TELEGRAM_BOT_TOKEN env var is required"))?;

    let allowed_users = build_allowed_users(&config);
    if allowed_users.is_empty() {
        warn!("No allowed Telegram users configured — all messages will be rejected");
    } else {
        info!(count = allowed_users.len(), "Telegram allowed users loaded");
    }

    let memory_config = config.memory.clone();

    let first_workspace: Option<std::path::PathBuf> = config.agents.values().find_map(|ac| {
        ac.workspace
            .as_ref()
            .map(|p| crate::adapters::tool_builder::expand_tilde(p))
    });

    let memory_handle =
        channel_runtime::build_memory_handle(&memory_config, &rt, first_workspace.as_deref());
    let has_memory = memory_handle.is_some();

    crate::adapters::scaffold::maybe_apply_scaffold(&config);

    // Build per-agent runtime state.
    let mut agent_states: HashMap<String, TelegramAgentState> = HashMap::new();
    let mut role_to_agent: HashMap<String, String> = HashMap::new();
    let mut default_agent_id: Option<String> = None;

    for (agent_id, agent_config) in &config.agents {
        let engine: Arc<dyn Engine> = match build_engine(agent_id, agent_config) {
            Ok(e) => Arc::from(e),
            Err(e) => {
                warn!(agent_id = %agent_id, error = %e, "Failed to build engine, skipping");
                continue;
            }
        };

        let engine_info = EngineInfo {
            context_window: engine.context_window(),
            diagnostics: engine.diagnostics(),
        };

        let workspace: Option<std::path::PathBuf> = agent_config
            .workspace
            .as_ref()
            .map(|p| crate::adapters::tool_builder::expand_tilde(p));

        let advertise_workspace_tools =
            engine.supports_tool_use() && !engine.manages_own_workspace();
        let uses_tools = advertise_workspace_tools && workspace.is_some();
        let base_tools = channel_runtime::compute_base_tools(
            uses_tools,
            has_memory,
            &agent_config.workspace_tools,
        );

        let skill_source: Option<FileSystemSkillSource> = workspace
            .as_ref()
            .map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.def.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(agent_config.skill_packages.clone()));
        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        let current_tools = channel_runtime::rebuild_tools(&base_tools, &skill_registry);
        let current_system_prompt = channel_runtime::rebuild_system_prompt(
            agent_config,
            advertise_workspace_tools,
            &skill_registry,
            &current_tools,
        );

        let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
        let compaction_policy = resolve_flow_compaction_policy(
            &agent_config.flow,
            agent_config.limits.max_tokens_per_flow,
            engine.context_window(),
            engine.max_output_tokens_per_turn() as usize,
        );

        let role = agent_config.role.clone();
        if let Some(ref role_str) = role {
            let role_key = role_str.trim().to_lowercase().replace('-', "_");
            if !role_key.is_empty() {
                role_to_agent.insert(role_key, agent_id.clone());
            }
        }
        role_to_agent.insert(agent_id.clone(), agent_id.clone());

        if agent_config.default || default_agent_id.is_none() {
            if agent_config.default {
                default_agent_id = Some(agent_id.clone());
            } else if default_agent_id.is_none() {
                default_agent_id = Some(agent_id.clone());
            }
        }

        info!(
            agent_id = %agent_id,
            role = ?role,
            tools = current_tools.len(),
            "Registered Telegram agent"
        );

        agent_states.insert(
            agent_id.clone(),
            TelegramAgentState {
                agent_id: agent_id.clone(),
                agent_config: agent_config.clone(),
                engine,
                engine_info,
                workspace,
                base_tools,
                skill_source,
                skill_registry,
                current_tools,
                current_system_prompt,
                advertise_workspace_tools,
                history_turn_limit,
                compaction_policy,
                role,
            },
        );
    }

    let default_agent_id =
        default_agent_id.ok_or_else(|| anyhow::anyhow!("No agents configured"))?;

    let base_workspaces: HashMap<String, std::path::PathBuf> = agent_states
        .iter()
        .filter_map(|(id, a)| a.workspace.as_ref().map(|w| (id.clone(), w.clone())))
        .collect();

    // Inject team awareness into each agent's system prompt.
    if agent_states.len() > 1 {
        let mut team_block = String::from("\n\n## Team Members\n");
        team_block.push_str(
            "If a request is outside your expertise, suggest the user route to the right agent.\n",
        );
        team_block.push_str("Format: @role: message\n\n");
        for state in agent_states.values() {
            let role_key = state.role.as_deref().unwrap_or(&state.agent_id);
            let name = state
                .agent_config
                .identity
                .name
                .as_deref()
                .unwrap_or(&state.agent_id);
            team_block.push_str(&format!("- @{}: {}\n", role_key, name));
        }
        for state in agent_states.values_mut() {
            state.current_system_prompt.push_str(&team_block);
        }
    }

    // Build agent descriptions for the planner prompt.
    let agent_descriptions: HashMap<String, String> = agent_states
        .iter()
        .map(|(aid, astate)| {
            let role_key = astate.role.as_deref().unwrap_or(aid).to_string();
            let name = astate.agent_config.identity.name.as_deref().unwrap_or(aid);
            let instructions = astate
                .agent_config
                .identity
                .instructions
                .as_deref()
                .unwrap_or("AI assistant");
            let truncated = if instructions.len() > 500 {
                let mut end = 500;
                while end > 0 && !instructions.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &instructions[..end])
            } else {
                instructions.to_string()
            };
            let mut desc = format!("{}\nInstructions: {}", name, truncated);
            if !astate.agent_config.requires.is_empty() {
                desc.push_str(&format!(
                    "\nREQUIRES (must depend on): {}",
                    astate.agent_config.requires.join(", ")
                ));
            }
            (role_key, desc)
        })
        .collect();

    // Build dedicated planner engine if configured.
    let planner_engine: Option<Arc<dyn Engine>> = match (
        config
            .orchestrator
            .as_ref()
            .and_then(|o| o.planner_engine.as_ref()),
        config
            .orchestrator
            .as_ref()
            .and_then(|o| o.planner_model.as_ref()),
    ) {
        (Some(engine_type), Some(model)) => {
            match crate::adapters::engine_builder::build_planner_engine(engine_type, model) {
                Ok(e) => {
                    info!(engine = %engine_type, model = %model, "Built dedicated planner engine");
                    Some(Arc::from(e))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to build planner engine, falling back to default agent");
                    None
                }
            }
        }
        _ => None,
    };

    info!(
        agents = agent_states.len(),
        default = %default_agent_id,
        planner = planner_engine.as_ref().map(|_| "dedicated").unwrap_or("default agent"),
        "Telegram multi-agent setup complete"
    );

    let pending_approvals: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let turn_cancel = Arc::new(AtomicBool::new(false));

    let pipe = Arc::new(TelegramPipe::with_approvals(
        bot_token,
        Arc::clone(&pending_approvals),
        Arc::clone(&turn_cancel),
    ));
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(256);
    rt.block_on(pipe.connect(inbound_tx))?;

    let current_recipient: CurrentRecipient = Arc::new(std::sync::Mutex::new(None));
    let approval_adapter: Arc<dyn ToolApprovalPort> = if config.telegram.tool_approvals {
        let inner: Arc<dyn ToolApprovalPort> = Arc::new(TelegramInlineApprovalAdapter {
            pipe: Arc::clone(&pipe),
            current_recipient: Arc::clone(&current_recipient),
            cancel: Arc::clone(&turn_cancel),
        });
        if config.telegram.approve_only.is_empty() {
            inner
        } else {
            let filter: HashSet<String> =
                config.telegram.approve_only.iter().cloned().collect();
            info!(
                tools = ?filter,
                "Telegram approval filter: only these tools require approval"
            );
            Arc::new(FilteredApprovalAdapter {
                inner,
                approve_only: filter,
            })
        }
    } else {
        Arc::new(AllowAllApproval)
    };

    let memory_service_instance = memory_handle
        .as_ref()
        .map(|h| MemoryService::new(h.embedding.as_ref(), h.store.as_ref()));

    let mut user_states: HashMap<String, ChatLoopState> = HashMap::new();
    let mut user_state_last_active: HashMap<String, std::time::Instant> = HashMap::new();
    let mut last_eviction_check = std::time::Instant::now();
    let mut user_active_agent: HashMap<String, String> = HashMap::new();

    let mut skill_command_router = {
        let default_agent = agent_states.get(&default_agent_id);
        default_agent
            .map(|a| SkillCommandRouter::from_registry(&a.skill_registry))
            .unwrap_or_else(|| SkillCommandRouter::from_registry(&SkillRegistry::new(vec![])))
    };

    let mut activity_log: Vec<channel_runtime::ActivityEntry> = Vec::new();
    let is_multi_agent = agent_states.len() > 1;

    info!("Telegram bot started — waiting for messages (Ctrl+C to stop)");

    rt.block_on(async {
        let delivery_opts = DeliveryOptions::default();
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        loop {
            let msg = tokio::select! {
                msg = inbound_rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                _ = &mut ctrl_c => {
                    info!("Received Ctrl+C, shutting down Telegram bot");
                    break;
                }
            };

            let sender_id = &msg.sender.peer_id;

            // Periodic eviction sweep.
            if last_eviction_check.elapsed() >= EVICTION_SWEEP_INTERVAL {
                let now = std::time::Instant::now();
                let idle_keys: Vec<String> = user_state_last_active
                    .iter()
                    .filter(|(_, &last)| now.duration_since(last) >= IDLE_EVICTION_THRESHOLD)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in &idle_keys {
                    user_states.remove(key);
                    user_state_last_active.remove(key);
                }
                if !idle_keys.is_empty() {
                    info!(
                        evicted = idle_keys.len(),
                        remaining = user_states.len(),
                        "Evicted idle user states"
                    );
                }
                last_eviction_check = now;
            }

            // Access control.
            if !allowed_users.is_empty() && !allowed_users.contains(sender_id) {
                warn!(sender = %sender_id, "Unauthorized Telegram user");
                let _ = pipe
                    .send_text(&msg.sender, "Unauthorized.", &delivery_opts)
                    .await;
                continue;
            }

            // Slash commands.
            if msg.content.starts_with('/') {
                if msg.content == "/stop" || msg.content.starts_with("/stop@") {
                    turn_cancel.store(true, Ordering::Relaxed);
                    let _ = pipe
                        .send_text(&msg.sender, "⏹ Stop requested.", &delivery_opts)
                        .await;
                    continue;
                }

                if msg.content.starts_with("/team") {
                    let goal = msg.content.trim_start_matches("/team").trim();
                    if goal.is_empty() || !is_multi_agent {
                        let hint = if !is_multi_agent {
                            "Only one agent configured — /team requires multiple agents."
                        } else {
                            "Usage: /team <goal>\nExample: /team build a full stack Rust app"
                        };
                        let _ = pipe.send_text(&msg.sender, hint, &delivery_opts).await;
                        continue;
                    }
                    let _ = handle_team(
                        &pipe, &msg.sender, goal, msg.media.as_ref(),
                        &delivery_opts, &mut agent_states, &default_agent_id,
                        &agent_descriptions, &role_to_agent, &current_recipient,
                        &memory_handle, &secret_registry, &approval_adapter,
                        &turn_cancel, planner_engine.as_deref(),
                    ).await;
                    continue;
                }

                if msg.content.starts_with("/project") {
                    let name = msg.content.trim_start_matches("/project").trim();
                    let _ = handle_project(
                        &pipe, &msg.sender, name, &delivery_opts,
                        &mut agent_states, &default_agent_id, &base_workspaces,
                        &config, &mut user_states, sender_id,
                    ).await;
                    continue;
                }

                if msg.content == "/agents" || msg.content.starts_with("/agents@") {
                    let _ = handle_agents(
                        &pipe, &msg.sender, &delivery_opts, &agent_states, &default_agent_id,
                    ).await;
                    continue;
                }

                let active_aid = user_active_agent
                    .get(sender_id)
                    .cloned()
                    .unwrap_or_else(|| default_agent_id.clone());
                let agent = match agent_states.get_mut(&active_aid) {
                    Some(a) => a,
                    None => continue,
                };

                if msg.content == "/reset" || msg.content.starts_with("/reset@") {
                    let _ = handle_reset(
                        &pipe, &msg.sender, &delivery_opts, &mut user_states, sender_id,
                    ).await;
                    continue;
                }

                if msg.content == "/purge" || msg.content.starts_with("/purge@") {
                    let _ = handle_purge(
                        &pipe, &msg.sender, &delivery_opts, &mut user_states, sender_id,
                        &agent_states, &memory_handle,
                    ).await;
                    continue;
                }

                if msg.content == "/reload" {
                    let _ = handle_reload(
                        &pipe, &msg.sender, &delivery_opts, agent, &mut skill_command_router,
                    ).await;
                    continue;
                }

                if let SkillCommandMatch::Matched {
                    skill_name,
                    command: cmd_name,
                    args,
                } = skill_command_router.route(&msg.content)
                {
                    user_active_agent.insert(sender_id.clone(), active_aid.clone());
                    let _ = handle_skill_command(
                        &pipe, &msg.sender, &delivery_opts, agent,
                        &mut skill_command_router, &skill_name, &cmd_name, &args,
                        &mut user_states, &mut user_state_last_active,
                        sender_id, &active_aid, &current_recipient,
                        &memory_handle, &secret_registry, &approval_adapter,
                        &turn_cancel,
                        memory_service_instance.as_ref(), &memory_config,
                    ).await;
                    continue;
                }

                let state_key = format!("{}:{}", sender_id, active_aid);
                user_state_last_active.insert(state_key.clone(), std::time::Instant::now());
                let state = user_states.entry(state_key).or_insert_with(|| {
                    channel_runtime::create_chat_loop_state(&agent.agent_config)
                });
                let _ = handle_builtin_command(
                    &pipe, &msg.sender, &delivery_opts, &msg.content,
                    state, agent, &skill_command_router,
                ).await;
                continue;
            }

            // Route message to an agent.
            let (mut routed_role, user_text) =
                channel_runtime::parse_agent_routing(&msg.content, Some(&role_to_agent));

            if routed_role.is_none() && is_multi_agent {
                let decision = {
                    let engine_ref: Option<&dyn Engine> =
                        if let Some(ref dedicated) = planner_engine {
                            Some(dedicated.as_ref())
                        } else {
                            agent_states.get(&default_agent_id).map(|a| a.engine.as_ref())
                        };
                    match engine_ref {
                        Some(eng) => {
                            crate::adapters::task_builder::classify_request(
                                eng,
                                &user_text,
                                &agent_descriptions,
                            )
                            .await
                        }
                        None => Ok(crate::adapters::types::RouteDecision::MultiAgent),
                    }
                };

                match decision {
                    Ok(crate::adapters::types::RouteDecision::SingleAgent(role_key)) => {
                        if role_to_agent.contains_key(&role_key) {
                            info!(role = %role_key, "Classifier routed to single agent");
                            routed_role = Some(role_key);
                        } else {
                            warn!(role = %role_key, "Classifier returned unknown role, falling back to planner");
                            let _ = handle_team(
                                &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                                &delivery_opts, &mut agent_states, &default_agent_id,
                                &agent_descriptions, &role_to_agent, &current_recipient,
                                &memory_handle, &secret_registry, &approval_adapter,
                                &turn_cancel, planner_engine.as_deref(),
                            ).await;
                            continue;
                        }
                    }
                    Ok(crate::adapters::types::RouteDecision::MultiAgent) => {
                        let _ = handle_team(
                            &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                            &delivery_opts, &mut agent_states, &default_agent_id,
                            &agent_descriptions, &role_to_agent, &current_recipient,
                            &memory_handle, &secret_registry, &approval_adapter,
                            &turn_cancel, planner_engine.as_deref(),
                        ).await;
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "Classifier failed, falling back to planner");
                        let _ = handle_team(
                            &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                            &delivery_opts, &mut agent_states, &default_agent_id,
                            &agent_descriptions, &role_to_agent, &current_recipient,
                            &memory_handle, &secret_registry, &approval_adapter,
                            &turn_cancel, planner_engine.as_deref(),
                        ).await;
                        continue;
                    }
                }
            }

            let target_agent_id = if let Some(ref role_key) = routed_role {
                match role_to_agent.get(role_key) {
                    Some(aid) => aid.clone(),
                    None => {
                        let available: Vec<&str> = agent_states.values()
                            .filter_map(|a| a.role.as_deref())
                            .collect();
                        let _ = pipe
                            .send_text(
                                &msg.sender,
                                &format!(
                                    "Unknown agent role: {}\nAvailable: {}",
                                    role_key,
                                    available.join(", ")
                                ),
                                &delivery_opts,
                            )
                            .await;
                        continue;
                    }
                }
            } else {
                default_agent_id.clone()
            };

            user_active_agent.insert(sender_id.clone(), target_agent_id.clone());

            let agent = match agent_states.get_mut(&target_agent_id) {
                Some(a) => a,
                None => continue,
            };

            // Hot-reload skills.
            if let Some(ref src) = agent.skill_source {
                if agent.skill_registry.reload(src) {
                    agent.current_tools = channel_runtime::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                    );
                    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
                    );
                }
            }

            // Save attached files.
            let user_content = if let (Some(ref ws), Some(ref media)) =
                (&agent.workspace, &msg.media)
            {
                let attachments_dir = ws.join(".tengu-attachments");
                std::fs::create_dir_all(&attachments_dir).ok();

                let mut file_notes = Vec::new();
                for m in media {
                    let fname = sanitize_attachment_filename(
                        m.filename.as_deref().unwrap_or("attachment"),
                    );
                    let path = attachments_dir.join(&fname);
                    match std::fs::write(&path, &m.data) {
                        Ok(()) => {
                            info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment");
                            file_notes.push(format!(
                                "[Attached file: {} ({}, {} bytes)]",
                                path.display(),
                                m.mime_type,
                                m.data.len()
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to save Telegram attachment");
                        }
                    }
                }

                if file_notes.is_empty() {
                    user_text.clone()
                } else {
                    format!("{}\n{}", file_notes.join("\n"), user_text)
                }
            } else {
                user_text.clone()
            };

            *current_recipient.lock().unwrap() = Some(msg.sender.clone());

            let activity_adapter = make_tool_activity_adapter(
                agent.agent_config
                    .identity
                    .name
                    .clone()
                    .unwrap_or_else(|| agent.agent_id.clone()),
                &agent.current_tools,
            );
            let current_executor =
                agent.workspace.as_ref().and_then(|ws| {
                    channel_runtime::build_tool_executor(
                        ws,
                        &agent.current_tools,
                        &agent.skill_registry,
                        &memory_handle,
                        &secret_registry,
                        Arc::clone(&approval_adapter),
                        activity_adapter,
                        Some(Arc::clone(&turn_cancel)),
                        None,
                    )
                });

            let sanitized_executor = current_executor.as_ref().map(|e| {
                SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
            });

            let turn_tool_log: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));

            let observer_secrets = Arc::clone(&secret_registry);
            let observer_agent_label = agent
                .agent_config
                .identity
                .name
                .clone()
                .unwrap_or_else(|| agent.agent_id.clone());
            let turn_tool_log_ref = Arc::clone(&turn_tool_log);
            let observer_tools = channel_runtime::tool_defs(&agent.current_tools);
            let tool_result_observer = move |call: &ToolCall, result: &str| {
                if let Some(entry) = channel_runtime::format_tool_for_activity(call, &observer_tools) {
                    turn_tool_log_ref.lock().unwrap().push(entry);
                }
                let redacted = observer_secrets.redact(result);
                let mut end = redacted.len().min(1500);
                while end < redacted.len() && !redacted.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated = if redacted.len() > 1500 {
                    format!("{}…", &redacted[..end])
                } else {
                    redacted
                };
                tracing::debug!(
                    agent = %observer_agent_label,
                    tool = %call.name,
                    result = %truncated,
                    "Telegram tool result"
                );
            };

            turn_cancel.store(false, Ordering::Relaxed);

            let state_key = format!("{}:{}", sender_id, target_agent_id);
            user_state_last_active.insert(state_key.clone(), std::time::Instant::now());
            let state = user_states.entry(state_key).or_insert_with(|| {
                channel_runtime::create_chat_loop_state(&agent.agent_config)
            });

            if is_multi_agent {
                let agent_label = agent.agent_config.identity.name.as_deref()
                    .unwrap_or(&agent.agent_id);
                let _ = pipe
                    .send_text(
                        &msg.sender,
                        &format!("[{}]", agent_label),
                        &delivery_opts,
                    )
                    .await;
            }

            let mut turn_system_prompt = agent.current_system_prompt.clone();
            if is_multi_agent && !needs_fresh_history_grounding(&user_content) {
                let activity_ctx =
                    channel_runtime::build_activity_context(&activity_log, &target_agent_id);
                if !activity_ctx.is_empty() {
                    turn_system_prompt.push_str(&activity_ctx);
                }
            } else if is_multi_agent {
                turn_system_prompt.push_str(
                    "\n\n## Grounding Rule\nFor questions about last/latest/most recent work, do not answer from Recent Team Activity. Verify against current workspace files, conversation state, or tool results first.\n",
                );
            }

            let turn_tools = channel_runtime::tool_defs(&agent.current_tools);
            let chat_runtime = ChatRuntimeService {
                engine: agent.engine.as_ref(),
                agent_id: &agent.agent_id,
                agent_config: &agent.agent_config,
                history_turn_limit: agent.history_turn_limit,
                compaction_policy: agent.compaction_policy,
                system_prompt: turn_system_prompt,
                tools: &turn_tools,
                tool_executor: sanitized_executor
                    .as_ref()
                    .map(|e| e as &dyn ToolExecutor),
                memory_service: memory_service_instance.as_ref(),
                max_recall_entries: memory_config.max_recall_entries,
                max_recall_tokens: memory_config.max_recall_tokens,
                tool_observer: Some(&tool_result_observer),
                cancel: Some(&turn_cancel),
            };

            let _ = pipe.send_chat_action(&msg.sender).await;
            let typing_pipe = Arc::clone(&pipe);
            let typing_sender = msg.sender.clone();
            let typing_handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    let _ = typing_pipe.send_chat_action(&typing_sender).await;
                }
            });

            let result = chat_runtime.process_user_text(state, &user_content).await;
            typing_handle.abort();

            match result {
                Ok(result) => {
                    if let Some(notice) = result.system_notice {
                        let _ = pipe.send_text(&msg.sender, &notice, &delivery_opts).await;
                    }
                    if let Some(ref text) = result.assistant_text {
                        let reply = secret_registry.redact(text);
                        for chunk in channel_runtime::chunk_message(&reply, TELEGRAM_MAX_LEN) {
                            if let Err(e) =
                                pipe.send_text(&msg.sender, chunk, &delivery_opts).await
                            {
                                error!(error = %e, "Failed to send Telegram reply chunk");
                            }
                        }
                    }

                    if is_multi_agent {
                        let tools_used = match Arc::try_unwrap(turn_tool_log) {
                            Ok(mutex) => mutex.into_inner().unwrap_or_default(),
                            Err(arc) => arc.lock().unwrap().clone(),
                        };
                        let response_summary = result
                            .assistant_text
                            .as_deref()
                            .map(|t| channel_runtime::truncate_summary(t, channel_runtime::MAX_ACTIVITY_SUMMARY_CHARS))
                            .unwrap_or_default();
                        let label = agent
                            .agent_config
                            .identity
                            .name
                            .clone()
                            .unwrap_or_else(|| agent.agent_id.clone());
                        if !tools_used.is_empty() || !response_summary.is_empty() {
                            activity_log.push(channel_runtime::ActivityEntry {
                                agent_label: label,
                                agent_id: target_agent_id.clone(),
                                tools_used,
                                response_summary,
                                tool_outcomes: result.tool_outcomes,
                            });
                            if activity_log.len() > channel_runtime::MAX_ACTIVITY_ENTRIES {
                                activity_log
                                    .drain(..activity_log.len() - channel_runtime::MAX_ACTIVITY_ENTRIES);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Engine error");
                    let _ = pipe
                        .send_text(
                            &msg.sender,
                            &format!("Error: {}", e),
                            &delivery_opts,
                        )
                        .await;
                }
            }
        }

        pipe.disconnect().await.ok();
    });

    Ok(())
}
