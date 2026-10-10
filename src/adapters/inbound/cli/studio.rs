//! `tengu studio` — Tengu Studio (`TENGU_STUDIO_PLAN.md`). Logs go to stderr.
//!
//! | Command | Does |
//! |---|---|
//! | `tengu studio --sandbox <s> [--port <n>] [--bind <ip>] [--allow-control] [--allow-edit]` | the local browser UI (`adapters/inbound/studio`, feature `studio`, on by default): loopback only (`127.0.0.1` default, `::1`; anything else refused), port 0 (default) = any free one; prints `Studio: http://127.0.0.1:<port>/#t=<token>` on stdout; SIGINT / SIGTERM drain a runtime it started, then stop it. Play / Stop / send-event when the sandbox sets `[studio] control = true` or with `--allow-control` — never for a `[generation]`-bound or hardened sandbox (`config/studio.rs`). `--allow-edit` (needs `--sandbox`): also the drag-and-drop builder at `/builder` (`studio/builder.rs`), printed as `Builder: …/builder#t=<token>`. A build without it (`--no-default-features`): an error naming the build flag |
//! | `tengu studio graph --sandbox <s> [--map <file\|->]` | the sandbox's `WorkflowGraph` (`domain/workflow.rs`) as JSON on stdout: validated config + catalog tools, narrowed by an execution map when given (a refused map lists every reason), attrs redacted |

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};

use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;

/// Default `--bind` (loopback; the only kind the server accepts).
const DEFAULT_BIND: &str = "127.0.0.1";

#[derive(Subcommand)]
pub(super) enum StudioAction {
    /// Print the sandbox's workflow graph (nodes + edges) as JSON.
    Graph {
        /// Execution map JSON file (`-` = stdin): its loop narrowed as
        /// `tengu decide --map` would run it; dropped actions are greyed.
        #[arg(long)]
        map: Option<PathBuf>,
    },
}

/// The server's flags (no subcommand).
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub(super) struct ServeArgs {
    /// Port to listen on (0 or omitted = any free port; the URL names it).
    #[arg(long)]
    port: Option<u16>,
    /// Loopback address to bind (127.0.0.1 default, or ::1). Any other
    /// address is refused: remote access needs authentication + TLS.
    #[arg(long)]
    bind: Option<String>,
    /// Allow Play / Stop / send-event for this sandbox in this process
    /// (default: only when its config sets `[studio] control = true`).
    /// Never for a `[generation]`-bound or hardened sandbox.
    #[arg(long)]
    allow_control: bool,
    /// Serve the drag-and-drop builder (`/builder`) for this sandbox: edit
    /// its canvas (`sandboxes/<s>/builder.json`) and Finalise it into
    /// `config.toml`. Only a sandbox `tengu sandbox new` made is editable;
    /// any other is view-only.
    #[arg(long)]
    allow_edit: bool,
    /// Open the page (the builder with --allow-edit) in the default browser.
    #[arg(long)]
    open: bool,
}

impl ServeArgs {
    /// `tengu sandbox new`: the builder, opened in the browser.
    pub(super) fn builder(port: Option<u16>) -> Self {
        Self {
            port,
            allow_edit: true,
            open: true,
            ..Self::default()
        }
    }
}

pub(super) async fn run_studio(
    config: Config,
    serve: ServeArgs,
    action: Option<StudioAction>,
    secrets: Arc<SecretRegistry>,
) -> Result<()> {
    match action {
        Some(StudioAction::Graph { map }) => {
            if serve != ServeArgs::default() {
                bail!("--port / --bind / --allow-control / --allow-edit are the server's flags: `tengu studio --sandbox <s> [--port <n>] [--bind <ip>] [--allow-control] [--allow-edit]` (no subcommand)");
            }
            let map = match map {
                None => None,
                Some(p) => Some(
                    ExecutionMap::parse(&super::decide::read_input(&p, "execution map")?)
                        .map_err(|e| anyhow!(e))?,
                ),
            };
            let graph = crate::bootstrap::studio::workflow_graph(&config, map.as_ref(), &secrets)?;
            println!("{}", serde_json::to_string_pretty(&graph)?);
            Ok(())
        }
        None => serve_studio(config, serve, secrets).await,
    }
}

#[cfg(feature = "studio")]
async fn serve_studio(
    config: Config,
    serve: ServeArgs,
    secrets: Arc<SecretRegistry>,
) -> Result<()> {
    if serve.allow_edit && config.sandbox_name.is_none() {
        bail!("--allow-edit needs --sandbox <name> (the builder edits sandboxes/<name>/; `tengu sandbox new` makes one)");
    }
    let opts = crate::adapters::inbound::studio::ServeOpts {
        bind: serve.bind.unwrap_or_else(|| DEFAULT_BIND.to_string()),
        port: serve.port.unwrap_or(0),
        allow_control: serve.allow_control,
        allow_edit: serve.allow_edit,
        open: serve.open,
    };
    crate::adapters::inbound::studio::run_studio(config, secrets, opts).await
}

#[cfg(not(feature = "studio"))]
async fn serve_studio(
    _config: Config,
    serve: ServeArgs,
    _secrets: Arc<SecretRegistry>,
) -> Result<()> {
    let _ = (
        serve.port,
        serve.bind,
        serve.allow_control,
        serve.allow_edit,
        serve.open,
        DEFAULT_BIND,
    );
    bail!(
        "the Studio server requires: cargo build --features studio \
         (`tengu studio graph` works in every build)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        serve: ServeArgs,
        #[command(subcommand)]
        action: Option<StudioAction>,
    }

    fn secrets() -> Arc<SecretRegistry> {
        Arc::new(SecretRegistry::new())
    }

    /// `tengu studio [--port] [--bind]` = the server; `graph` takes no
    /// server flag.
    #[tokio::test]
    async fn studio_server_flags_only_without_a_subcommand() {
        let cli = Cli::try_parse_from(["studio", "--port", "0", "--bind", "::1"]).unwrap();
        assert!(cli.action.is_none());
        assert_eq!(
            cli.serve,
            ServeArgs {
                port: Some(0),
                bind: Some("::1".into()),
                allow_control: false,
                allow_edit: false,
                open: false,
            }
        );
        let cli = Cli::try_parse_from(["studio", "--allow-control"]).unwrap();
        assert!(cli.serve.allow_control && cli.action.is_none());
        let cli = Cli::try_parse_from(["studio", "--port", "8080", "graph"]).unwrap();
        let err = run_studio(Config::default(), cli.serve, cli.action, secrets())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server's flags"), "{err:#}");
    }

    /// A build without `studio` (`--no-default-features`) names the flag
    /// that adds the server.
    #[cfg(not(feature = "studio"))]
    #[tokio::test]
    async fn studio_server_needs_the_feature() {
        let err = run_studio(Config::default(), ServeArgs::default(), None, secrets())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--features studio"), "{err:#}");
    }
}
