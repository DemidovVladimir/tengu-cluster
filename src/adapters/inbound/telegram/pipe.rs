//! `TelegramPipe` — the teloxide wrapper (connect, send, typing), file
//! downloads and the tool-activity log adapter.

use super::*;

// ===========================================================================
// TelegramPipe — low-level Telegram API wrapper
// ===========================================================================

/// Telegram bot pipe backed by teloxide.
pub(super) struct TelegramPipe {
    /// Built once in `new` (see `build_bot`); `Bot` is `Arc`-backed, clones
    /// are cheap.
    bot: teloxide::Bot,
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    turn_cancel: Option<Arc<AtomicBool>>,
}

impl TelegramPipe {
    /// Bot client honouring `[egress]`. teloxide takes a reqwest 0.11 client
    /// (`reqwest011` alias). Under a proxy it goes through the proxy's HTTP
    /// CONNECT form (Arti serves CONNECT on the SOCKS port) with Tor-sized
    /// timeouts: teloxide's defaults (5s connect / 17s total, and 0.10 does
    /// not extend the total for long polls) time out over Tor, where `GetMe`
    /// takes 10–15s and each `getUpdates` is the 10s long poll plus that.
    pub(super) fn build_bot(token: &str) -> Result<teloxide::Bot> {
        let mut builder = teloxide::net::default_reqwest_settings();
        if let Some(proxy) = crate::adapters::outbound::egress::policy().http_connect_proxy() {
            builder = builder
                .proxy(reqwest011::Proxy::all(proxy).context("telegram proxy url")?)
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(60));
        }
        // `[keys]`: `TELEGRAM_API_URL = <proxy>/telegram` — the bot talks to
        // the seal proxy with a placeholder token (header mode: `Tengu-Route`),
        // which the Worker swaps for its secret; the real token never reaches
        // this process.
        let sealed =
            sealed_api_url().and_then(|u| crate::adapters::outbound::keys::header_mode(&u));
        if let Some((_, headers)) = &sealed {
            let mut map = reqwest011::header::HeaderMap::new();
            for (name, value) in headers {
                let mut v = reqwest011::header::HeaderValue::from_str(value)
                    .context("seal-proxy header")?;
                v.set_sensitive(true);
                map.insert(*name, v);
            }
            builder = builder.default_headers(map);
        }
        let client = builder.build().context("building telegram http client")?;
        Ok(match sealed {
            Some((base, _)) => {
                teloxide::Bot::with_client(crate::adapters::outbound::keys::PLACEHOLDER, client)
                    .set_api_url(reqwest011::Url::parse(&base).context("seal-proxy url")?)
            }
            None => teloxide::Bot::with_client(token, client),
        })
    }

    pub(super) fn bot(&self) -> anyhow::Result<teloxide::Bot> {
        Ok(self.bot.clone())
    }
}

impl TelegramPipe {
    /// Create a new pipe with the given bot token and cancellation flag.
    pub(super) fn new(token: String, cancel: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self {
            bot: Self::build_bot(&token)?,
            shutdown: Arc::new(Mutex::new(None)),
            turn_cancel: Some(cancel),
        })
    }

    /// Start the teloxide dispatcher and forward incoming messages to `inbound_tx`.
    pub(super) async fn connect(
        &self,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::requests::Requester;

        let bot = self.bot()?;
        let me = bot
            .get_me()
            .await
            .map_err(|e| anyhow::anyhow!("Invalid Telegram bot token: {}", e))?;
        info!(bot = %me.username(), "Telegram bot authenticated");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown.lock().await = Some(shutdown_tx);

        let inbound_tx = inbound_tx.clone();
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
            let msg_handler = Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
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
                            entry.media.len() == 1
                        };

                        if is_first {
                            let flush_tx = tx.clone();
                            let flush_groups = Arc::clone(&groups);
                            let flush_group_id = group_id;
                            tokio::spawn(async move {
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

            let handler = dptree::entry().branch(msg_handler);

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

    /// Signal the dispatcher to shut down gracefully.
    pub(super) async fn disconnect(&self) -> Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    /// Send a "typing…" indicator to the target chat.
    pub(super) async fn send_chat_action(&self, target: &Recipient) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::{ChatAction, ChatId};

        let bot = self.bot()?;
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

    /// Send a plain text message to the target chat.
    pub(super) async fn send_text(
        &self,
        target: &Recipient,
        text: &str,
        _opts: &DeliveryOptions,
    ) -> Result<()> {
        use teloxide::prelude::*;
        use teloxide::types::ChatId;

        let bot = self.bot()?;
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
pub(super) async fn download_telegram_file(
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
pub(super) type CurrentRecipient = Arc<std::sync::Mutex<Option<Recipient>>>;

/// Telegram adapter for tool progress — logs to tracing only, no chat messages.
pub(super) struct TelegramToolActivityAdapter {
    agent_label: String,
}

impl ToolActivityPort for TelegramToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let (title, detail) = crate::adapters::inbound::activity::build_tool_activity_text(call);
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

pub(super) fn make_tool_activity_adapter(
    agent_label: impl Into<String>,
) -> Arc<dyn ToolActivityPort> {
    Arc::new(TelegramToolActivityAdapter {
        agent_label: agent_label.into(),
    })
}
