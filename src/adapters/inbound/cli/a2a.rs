//! `tengu a2a` — A2A (Agent2Agent) for the operator: serve this sandbox's
//! agents, or talk to a remote agent as the `a2a` tool would (no scope:
//! the operator's own call; `[egress]` and the remote's host pin still
//! apply). Logs go to stderr; text (or `--json`) on stdout.
//!
//! | Command | Does |
//! |---|---|
//! | `tengu a2a serve --sandbox <s>` | the A2A server of `[a2a.server]` (`inbound/a2a.rs`, feature `a2a`, on by default); SIGINT / SIGTERM stop it |
//! | `tengu a2a cards --sandbox <s>` | the cards `serve` would publish, as JSON (nothing is bound) |
//! | `tengu a2a remotes --sandbox <s>` | the `[a2a.remotes.<name>]` entries |
//! | `tengu a2a card --sandbox <s> (--remote <name> \| --url <url>) [--json]` | a remote's card |
//! | `tengu a2a send --sandbox <s> (--remote <name> \| --url <url>) <text> [--data <json>] [--context <id>] [--task <id>] [--wait <s>] [--json]` | send a message, wait for the task (default: the remote's `timeout_secs`) |
//! | `tengu a2a get … --task <id> [--wait <s>]` · `tengu a2a cancel … --task <id>` | read / cancel a task |
//!
//! `--url` = an unconfigured remote (its card URL or base URL), no
//! credential — a quick look before writing the `[a2a.remotes]` entry.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::adapters::outbound::a2a::A2aClient;
use crate::config::a2a::A2aRemoteConfig;
use crate::config::Config;
use crate::domain::a2a::model::{Message, Part, Role, SendMessageRequest, SendResult};
use crate::domain::a2a::render;
use crate::domain::secrets::SecretRegistry;

#[derive(Subcommand)]
pub(super) enum A2aAction {
    /// Serve this sandbox's planner / agents (`[a2a.server]`).
    Serve,
    /// Print the cards `serve` would publish (JSON).
    Cards,
    /// List the configured remotes.
    Remotes,
    /// Read a remote's agent card.
    Card {
        #[command(flatten)]
        to: Remote,
        /// Print the card JSON instead of the summary.
        #[arg(long)]
        json: bool,
    },
    /// Send a message and wait for the answer.
    Send {
        #[command(flatten)]
        to: Remote,
        /// The text to send.
        text: String,
        /// JSON sent as a data part.
        #[arg(long)]
        data: Option<String>,
        /// Continue this conversation.
        #[arg(long)]
        context: Option<String>,
        /// Continue this task (it asked for input).
        #[arg(long)]
        task: Option<String>,
        /// Seconds to wait for the task to settle (default: the remote's
        /// `timeout_secs`; 0 = the first answer).
        #[arg(long)]
        wait: Option<u64>,
        /// Print the result JSON instead of the summary.
        #[arg(long)]
        json: bool,
    },
    /// Read a task.
    Get {
        #[command(flatten)]
        to: Remote,
        #[arg(long)]
        task: String,
        /// Seconds to wait for it to settle.
        #[arg(long, default_value_t = 0)]
        wait: u64,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a task.
    Cancel {
        #[command(flatten)]
        to: Remote,
        #[arg(long)]
        task: String,
        #[arg(long)]
        json: bool,
    },
}

/// Which remote: a configured one, or a URL.
#[derive(Debug, Clone, Args)]
pub(super) struct Remote {
    /// A `[a2a.remotes.<name>]` entry.
    #[arg(long, conflicts_with = "url")]
    remote: Option<String>,
    /// An unconfigured remote: its base URL or card URL (no credential).
    #[arg(long)]
    url: Option<String>,
}

impl Remote {
    /// The client of this remote (`--url`: ad hoc, no credential).
    fn client<'a>(
        &self,
        config: &Config,
        secrets: &'a SecretRegistry,
        wait: Option<u64>,
    ) -> Result<(A2aClient<'a>, A2aRemoteConfig, Duration)> {
        let (name, cfg) = self.resolve(config)?;
        let wait = Duration::from_secs(wait.unwrap_or(cfg.timeout_secs));
        let client = A2aClient::new(
            &name,
            &cfg,
            None,
            secrets,
            None,
            wait.max(Duration::from_secs(5)),
        )?;
        let client = if self.remote.is_none() {
            client.adhoc()
        } else {
            client
        };
        Ok((client, cfg, wait))
    }

    fn resolve(&self, config: &Config) -> Result<(String, A2aRemoteConfig)> {
        match (&self.remote, &self.url) {
            (Some(name), _) => config
                .a2a
                .as_ref()
                .and_then(|a| a.remotes.get(name))
                .map(|r| (name.clone(), r.clone()))
                .ok_or_else(|| anyhow!("no [a2a.remotes.{name}] in this sandbox config")),
            (None, Some(url)) => Ok((
                "url".into(),
                serde_json::from_value(serde_json::json!({"url": url}))?,
            )),
            (None, None) => bail!("name the remote: --remote <name> or --url <url>"),
        }
    }
}

pub(super) async fn run_a2a(
    config: Config,
    action: A2aAction,
    secrets: Arc<SecretRegistry>,
) -> Result<()> {
    match action {
        A2aAction::Serve => serve(config, secrets).await,
        A2aAction::Cards => {
            let cards = crate::bootstrap::a2a::cards(&config)?;
            let v = serde_json::json!({
                "planner": cards.planner,
                "agents": cards.agents,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        A2aAction::Remotes => {
            let remotes = config.a2a.as_ref().map(|a| &a.remotes);
            match remotes.filter(|r| !r.is_empty()) {
                None => println!("no [a2a.remotes.<name>] in this sandbox config"),
                Some(r) => {
                    for (name, cfg) in r {
                        println!(
                            "{name}\t{}\t{}",
                            cfg.url,
                            cfg.description.as_deref().unwrap_or("")
                        );
                    }
                }
            }
            Ok(())
        }
        A2aAction::Card { to, json } => {
            let (client, _, _) = to.client(&config, &secrets, Some(30))?;
            let name = client.name().to_string();
            let card = client.card().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&card)?);
            } else {
                println!("{}", render::card(&name, &card));
            }
            Ok(())
        }
        A2aAction::Send {
            to,
            text,
            data,
            context,
            task,
            wait,
            json,
        } => {
            let (client, cfg, wait) = to.client(&config, &secrets, wait)?;
            let name = client.name().to_string();
            let (_, ep) = client.connect().await?;
            let mut parts = vec![Part::text(text)];
            if let Some(d) = data {
                let v: Value =
                    serde_json::from_str(&d).map_err(|e| anyhow!("--data is not JSON: {e}"))?;
                parts.push(Part::data(v));
            }
            let req = SendMessageRequest {
                message: Message {
                    message_id: uuid::Uuid::new_v4().to_string(),
                    context_id: context,
                    task_id: task,
                    role: Role::User,
                    parts,
                    ..Default::default()
                },
                ..Default::default()
            };
            let r = client.send_and_wait(&ep, &req, wait).await?;
            print_result(&name, &r, json, cfg.max_result_chars)
        }
        A2aAction::Get {
            to,
            task,
            wait,
            json,
        } => {
            let (client, cfg, wait) = to.client(&config, &secrets, Some(wait))?;
            let name = client.name().to_string();
            let (_, ep) = client.connect().await?;
            let t = client.get_and_wait(&ep, &task, wait).await?;
            print_result(&name, &SendResult::Task(t), json, cfg.max_result_chars)
        }
        A2aAction::Cancel { to, task, json } => {
            let (client, cfg, _) = to.client(&config, &secrets, Some(30))?;
            let name = client.name().to_string();
            let (_, ep) = client.connect().await?;
            let t = client.cancel(&ep, &task).await?;
            print_result(&name, &SendResult::Task(t), json, cfg.max_result_chars)
        }
    }
}

fn print_result(name: &str, r: &SendResult, json: bool, max: usize) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
    } else {
        println!("{}", render::send_result(name, r, max));
    }
    Ok(())
}

#[cfg(feature = "a2a")]
async fn serve(config: Config, secrets: Arc<SecretRegistry>) -> Result<()> {
    crate::adapters::inbound::a2a::run_a2a_server(config, secrets).await
}

#[cfg(not(feature = "a2a"))]
async fn serve(_: Config, _: Arc<SecretRegistry>) -> Result<()> {
    bail!("the A2A server needs the `a2a` feature (on by default): cargo build --features a2a")
}
