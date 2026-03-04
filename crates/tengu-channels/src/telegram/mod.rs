//! Telegram channel adapter via teloxide.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use tengu_core::types::{DeliveryOptions, InboundMessage, MediaPayload, Recipient};
use tengu_core::{AccessPolicy, Pipe, PipeCapabilities, PipeContext};

/// Telegram bot pipe backed by teloxide.
pub struct TelegramPipe {
    token: String,
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl TelegramPipe {
    /// Create a new Telegram pipe with the given bot token.
    pub fn new(token: String) -> Self {
        Self {
            token,
            shutdown: Arc::new(Mutex::new(None)),
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

        tokio::spawn(async move {
            let handler = Update::filter_message().endpoint(
                move |msg: Message, _bot: Bot| {
                    let tx = inbound_tx.clone();
                    async move {
                        let text = msg.text().unwrap_or_default().to_string();
                        if text.is_empty() {
                            return Ok::<(), teloxide::RequestError>(());
                        }

                        let chat_id = msg.chat.id.0.to_string();
                        let sender_id = msg
                            .from
                            .as_ref()
                            .map(|u| u.id.0.to_string())
                            .unwrap_or_else(|| chat_id.clone());

                        debug!(chat_id = %chat_id, sender = %sender_id, "Telegram message received");

                        let inbound = InboundMessage {
                            sender: Recipient {
                                pipe_id: "telegram".to_string(),
                                peer_id: sender_id,
                                account_id: None,
                                thread_id: Some(chat_id),
                            },
                            content: text,
                            timestamp: chrono::Utc::now(),
                            media: None,
                        };

                        if tx.send(inbound).await.is_err() {
                            error!("Failed to forward Telegram message to inbound channel");
                        }
                        Ok(())
                    }
                },
            );

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

    async fn send_media(
        &self,
        target: &Recipient,
        media: &MediaPayload,
    ) -> anyhow::Result<()> {
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
