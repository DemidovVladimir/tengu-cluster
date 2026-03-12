//! Telegram channel adapter via teloxide.
//!
//! Provides the `TelegramPipe` which implements the core `Pipe` trait for
//! receiving and sending messages through a Telegram bot. Supports:
//!
//! - Text messages and media (documents, photos) with automatic download
//! - Inline keyboard callbacks for tool approval (Approve/Deny buttons)
//! - Typing indicator chat actions
//! - Message captions for media messages (photos/documents with text)

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use tengu_core::types::{DeliveryOptions, InboundMessage, MediaPayload, Recipient};
use tengu_core::{AccessPolicy, Pipe, PipeCapabilities, PipeContext};

/// A callback query response: (callback_data, chat_id).
pub type CallbackEvent = (String, i64);

/// Map of approval_id → oneshot sender for resolving inline keyboard responses.
pub type PendingApprovals =
    Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

/// Telegram bot pipe backed by teloxide.
pub struct TelegramPipe {
    token: String,
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    pending_approvals: Option<PendingApprovals>,
    turn_cancel: Option<Arc<AtomicBool>>,
}

impl TelegramPipe {
    /// Create a new Telegram pipe with the given bot token.
    pub fn new(token: String) -> Self {
        Self {
            token,
            shutdown: Arc::new(Mutex::new(None)),
            pending_approvals: None,
            turn_cancel: None,
        }
    }

    /// Create a new Telegram pipe with inline keyboard approval support
    /// and an optional turn-cancellation flag.
    pub fn with_approvals(
        token: String,
        pending: PendingApprovals,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            token,
            shutdown: Arc::new(Mutex::new(None)),
            pending_approvals: Some(pending),
            turn_cancel: Some(cancel),
        }
    }

    /// Send a "typing..." chat action to the given recipient.
    pub async fn send_chat_action(&self, target: &Recipient) -> anyhow::Result<()> {
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

    /// Send an inline keyboard approval request and return a oneshot receiver
    /// that resolves to `true` (approved) or `false` (denied).
    ///
    /// The `approval_id` is used as callback_data prefix to match responses.
    pub async fn send_inline_approval(
        &self,
        target: &Recipient,
        approval_id: &str,
        text: &str,
    ) -> anyhow::Result<tokio::sync::oneshot::Receiver<bool>> {
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
}

#[async_trait]
impl Pipe for TelegramPipe {
    fn id(&self) -> &str {
        "telegram"
    }

    fn display_name(&self) -> &str {
        "Telegram"
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::Open
    }

    fn capabilities(&self) -> PipeCapabilities {
        PipeCapabilities {
            supports_media: true,
            supports_streaming: false,
            supports_threading: true,
            supports_reactions: false,
            max_text_length: Some(4096),
        }
    }

    async fn connect(&self, ctx: PipeContext) -> anyhow::Result<()> {
        use teloxide::prelude::*;
        use teloxide::requests::Requester;

        let bot = Bot::new(&self.token);

        // Validate the token before starting the dispatcher — teloxide panics
        // inside dispatch() if the token is invalid, so we catch it early.
        let me = bot
            .get_me()
            .await
            .map_err(|e| anyhow::anyhow!("Invalid Telegram bot token: {}", e))?;
        info!(bot = %me.username(), "Telegram bot authenticated");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown.lock().await = Some(shutdown_tx);

        let inbound_tx = ctx.inbound_tx.clone();
        let pending_approvals = self.pending_approvals.clone();
        let turn_cancel = self.turn_cancel.clone();

        tokio::spawn(async move {
            // Media-group accumulator: Telegram sends multi-file uploads as
            // separate Message objects sharing the same `media_group_id`.
            // We buffer them for a short window and merge into one InboundMessage.
            type MediaGroupBuffer = Arc<Mutex<HashMap<String, MediaGroupState>>>;

            struct MediaGroupState {
                text: String,
                media: Vec<MediaPayload>,
                chat_id: String,
                sender_id: String,
            }

            let media_groups: MediaGroupBuffer = Arc::new(Mutex::new(HashMap::new()));

            // Branch 1: handle normal messages.
            let cancel_for_handler = turn_cancel.clone();
            let groups_ref = Arc::clone(&media_groups);
            let msg_handler = Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
                let tx = inbound_tx.clone();
                let cancel = cancel_for_handler.clone();
                let groups = Arc::clone(&groups_ref);
                async move {
                    // Text comes from text() for plain messages, caption() for media messages.
                    let text = msg.text().or(msg.caption()).unwrap_or_default().to_string();

                    // Intercept /stop — set the cancel flag and don't forward.
                    // Use starts_with to handle Telegram's @botname suffix.
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

                    // Download attached documents and photos.
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
                        // Take the largest available resolution (last in array).
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

                    // If this message belongs to a media group, buffer it
                    // and wait for the rest of the group.
                    if let Some(group_id) = msg.media_group_id() {
                        let group_id = group_id.to_string();
                        let is_first = {
                            let mut map = groups.lock().await;
                            let entry =
                                map.entry(group_id.clone())
                                    .or_insert_with(|| MediaGroupState {
                                        text: String::new(),
                                        media: Vec::new(),
                                        chat_id: chat_id.clone(),
                                        sender_id: sender_id.clone(),
                                    });
                            if !text.is_empty() && entry.text.is_empty() {
                                entry.text = text;
                            }
                            entry.media.extend(media_payloads);
                            entry.media.len() == 1 // first file in group
                        };

                        // Only the first message in the group spawns the flush task.
                        if is_first {
                            let flush_tx = tx.clone();
                            let flush_groups = Arc::clone(&groups);
                            let flush_group_id = group_id;
                            tokio::spawn(async move {
                                // Wait for remaining group members to arrive.
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                let state = flush_groups.lock().await.remove(&flush_group_id);
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
                                        error!("Failed to forward media group to inbound channel");
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

            // Branch 2: handle inline keyboard callback queries.
            let callback_handler = Update::filter_callback_query().endpoint(
                move |q: teloxide::types::CallbackQuery, bot: Bot| {
                    let pending = pending_approvals.clone();
                    async move {
                        if let Some(data) = q.data {
                            // Parse "approve:<id>" or "deny:<id>".
                            let (approved, approval_id) =
                                if let Some(id) = data.strip_prefix("approve:") {
                                    (true, id.to_string())
                                } else if let Some(id) = data.strip_prefix("deny:") {
                                    (false, id.to_string())
                                } else {
                                    return Ok::<(), teloxide::RequestError>(());
                                };

                            // Answer the callback to dismiss the spinner.
                            let answer_text = if approved { "Approved" } else { "Denied" };
                            let _ = bot.answer_callback_query(&q.id).text(answer_text).await;

                            // Edit the original message to remove the keyboard.
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

                            // Resolve the pending approval.
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

    async fn disconnect(&self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    async fn send_text(
        &self,
        target: &Recipient,
        text: &str,
        _opts: &DeliveryOptions,
    ) -> anyhow::Result<()> {
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

    async fn send_media(&self, target: &Recipient, media: &MediaPayload) -> anyhow::Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::{ChatId, InputFile};

        let bot = Bot::new(&self.token);
        let chat_id: i64 = target
            .thread_id
            .as_deref()
            .or(Some(&target.peer_id))
            .unwrap()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Telegram chat_id"))?;

        let filename = media.filename.clone().unwrap_or_else(|| "file".to_string());
        let input_file = InputFile::memory(media.data.clone()).file_name(filename);

        let mime = &media.mime_type;
        if mime.starts_with("image/") {
            bot.send_photo(ChatId(chat_id), input_file)
                .await
                .map_err(|e| anyhow::anyhow!("Telegram send_photo failed: {}", e))?;
        } else {
            bot.send_document(ChatId(chat_id), input_file)
                .await
                .map_err(|e| anyhow::anyhow!("Telegram send_document failed: {}", e))?;
        }

        Ok(())
    }
}

/// Download a file from Telegram servers given its file_id.
#[cfg(feature = "telegram")]
async fn download_telegram_file(
    bot: &teloxide::Bot,
    file_id: &str,
    mime_type: Option<String>,
    filename: Option<String>,
) -> Result<MediaPayload, Box<dyn std::error::Error + Send + Sync>> {
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
