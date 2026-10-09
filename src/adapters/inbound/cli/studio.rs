//! `tengu studio` — Tengu Studio (`TENGU_STUDIO_PLAN.md`). Read-only; logs
//! go to stderr.
//!
//! | Command | Does |
//! |---|---|
//! | `tengu studio --sandbox <s> [--port <n>] [--bind <ip>]` | the local browser UI (`adapters/inbound/studio`, `--features studio`): loopback only (`127.0.0.1` default, `::1`; anything else refused), port 0 (default) = any free one; prints `Studio: http://127.0.0.1:<port>/#t=<token>` on stdout; SIGINT / SIGTERM stop it. Without the feature: an error naming the build flag |
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
                bail!("--port / --bind are the server's flags: `tengu studio --sandbox <s> [--port <n>] [--bind <ip>]` (no subcommand)");
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
    let opts = crate::adapters::inbound::studio::ServeOpts {
        bind: serve.bind.unwrap_or_else(|| DEFAULT_BIND.to_string()),
        port: serve.port.unwrap_or(0),
    };
    crate::adapters::inbound::studio::run_studio(config, secrets, opts).await
}

#[cfg(not(feature = "studio"))]
async fn serve_studio(
    _config: Config,
    serve: ServeArgs,
    _secrets: Arc<SecretRegistry>,
) -> Result<()> {
    let _ = (serve.port, serve.bind, DEFAULT_BIND);
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
                bind: Some("::1".into())
            }
        );
        let cli = Cli::try_parse_from(["studio", "--port", "8080", "graph"]).unwrap();
        let err = run_studio(Config::default(), cli.serve, cli.action, secrets())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server's flags"), "{err:#}");
    }

    /// The default build names the flag that adds the server.
    #[cfg(not(feature = "studio"))]
    #[tokio::test]
    async fn studio_server_needs_the_feature() {
        let err = run_studio(Config::default(), ServeArgs::default(), None, secrets())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--features studio"), "{err:#}");
    }
}
