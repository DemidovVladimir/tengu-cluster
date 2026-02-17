use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::debug;

use tengu_core::types::{DeliveryOptions, InboundMessage, MediaPayload, Recipient};
use tengu_core::{AccessPolicy, Pipe, PipeCapabilities, PipeContext};

/// Interactive CLI pipe — reads from stdin, writes to stdout.
pub struct CliPipe {
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl CliPipe {
    /// Create a new CLI pipe instance.
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Pipe for CliPipe {
    fn id(&self) -> &str {
        "cli"
    }

    fn display_name(&self) -> &str {
        "CLI"
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::Open
    }

    fn capabilities(&self) -> PipeCapabilities {
        PipeCapabilities {
            supports_media: false,
            supports_streaming: true,
            supports_threading: false,
            supports_reactions: false,
            max_text_length: None,
        }
    }

    async fn connect(&self, ctx: PipeContext) -> anyhow::Result<()> {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown.lock().await = Some(shutdown_tx);

        // TODO(epic-channel-lifecycle): Track spawned task handles so disconnect can
        // await clean shutdown and surface task errors.
        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let reader = BufReader::new(stdin);
            let mut lines = reader.lines();

            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(text)) => {
                                let text = text.trim().to_string();
                                if text.is_empty() {
                                    continue;
                                }

                                // TODO(epic-pipe-policies): Route inbound messages through
                                // access-policy enforcement middleware before forwarding.
                                debug!(input = %text, "CLI input received");

                                let msg = InboundMessage {
                                    sender: Recipient {
                                        pipe_id: "cli".to_string(),
                                        peer_id: "local".to_string(),
                                        account_id: None,
                                        thread_id: None,
                                    },
                                    content: text,
                                    timestamp: chrono::Utc::now(),
                                    media: None,
                                };

                                if ctx.inbound_tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break, // EOF
                            Err(e) => {
                                eprintln!("Error reading stdin: {}", e);
                                break;
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        // TODO(epic-channel-lifecycle): Confirm background task exited before returning.
        Ok(())
    }

    async fn send_text(
        &self,
        _target: &Recipient,
        text: &str,
        _opts: &DeliveryOptions,
    ) -> anyhow::Result<()> {
        // TODO(epic-channel-streaming): Support incremental streaming display for text
        // deltas instead of only final buffered output.
        println!("\n{}\n", text);
        Ok(())
    }

    async fn send_media(
        &self,
        _target: &Recipient,
        media: &MediaPayload,
    ) -> anyhow::Result<()> {
        // TODO(epic-channel-cli-media): Add local rendering/preview hooks for media payloads.
        println!(
            "[Media: {} ({} bytes)]",
            media.filename.as_deref().unwrap_or("unnamed"),
            media.data.len()
        );
        Ok(())
    }
}
