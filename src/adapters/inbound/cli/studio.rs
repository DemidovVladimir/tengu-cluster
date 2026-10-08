//! `tengu studio` — Tengu Studio from the terminal (`TENGU_STUDIO_PLAN.md`).
//! Read-only; stdout is JSON, logs go to stderr.
//!
//! | Command | Prints |
//! |---|---|
//! | `tengu studio graph --sandbox <s> [--map <file\|->]` | the sandbox's `WorkflowGraph` (`domain/workflow.rs`): validated config + catalog tools, narrowed by an execution map when given (a refused map lists every reason), attrs redacted |

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Subcommand;

use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;

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

pub(super) fn run_studio(
    config: &Config,
    action: StudioAction,
    secrets: Arc<SecretRegistry>,
) -> Result<()> {
    match action {
        StudioAction::Graph { map } => {
            let map = match map {
                None => None,
                Some(p) => Some(
                    ExecutionMap::parse(&super::decide::read_input(&p, "execution map")?)
                        .map_err(|e| anyhow!(e))?,
                ),
            };
            let graph = crate::bootstrap::studio::workflow_graph(config, map.as_ref(), &secrets)?;
            println!("{}", serde_json::to_string_pretty(&graph)?);
            Ok(())
        }
    }
}
