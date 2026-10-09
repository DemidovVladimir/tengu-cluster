//! `tengu trace` — read the execution trace (`domain/trace.rs`) a sandbox's
//! `tengu run` / `tengu decide` recorded under
//! `<TENGU_HOME>/logs/trace/<sandbox>/`. No config, no network; read-only.
//! Stdout is JSON lines with every id in full; logs go to stderr.
//!
//! | Command | Prints |
//! |---|---|
//! | `tengu trace runs --sandbox <s>` | one `RunSummary` per run, oldest first (`run_id`, `kind`, `runtime_id`, `config_hash`, first / last ms, events, last kind / status) |
//! | `tengu trace show --sandbox <s> --run <run_id> [--after <seq>] [--follow]` | the run's events with `seq > after`, in `seq` order; `--follow` keeps printing new ones until Ctrl-C (resume later with `--after <last seq>`) |

use std::io::Write;

use anyhow::{bail, Result};
use clap::Subcommand;

use crate::ports::trace::TraceReader;

#[derive(Subcommand)]
pub(super) enum TraceAction {
    /// List the sandbox's recorded runs (JSON lines, oldest first).
    Runs,
    /// Print one run's events (JSON lines, `seq` order).
    Show {
        /// Run id, as `tengu trace runs` prints it.
        #[arg(long)]
        run: String,
        /// Only events with `seq` above this (resume point).
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// Keep printing new events as they are written, until Ctrl-C.
        #[arg(long)]
        follow: bool,
    },
}

pub(super) async fn run_trace(sandbox: Option<String>, action: TraceAction) -> Result<()> {
    let Some(sandbox) = sandbox else {
        bail!("--sandbox <name> is required (`default` for a config given with -c)");
    };
    let reader = crate::bootstrap::trace::reader(&sandbox)?;
    let mut out = std::io::stdout().lock();
    match action {
        TraceAction::Runs => {
            for run in reader.runs()? {
                writeln!(out, "{}", serde_json::to_string(&run)?)?;
            }
        }
        TraceAction::Show { run, after, follow } => {
            if follow {
                drop(out);
                return follow_run(&reader, &run, after).await;
            }
            // One read of the file: the run as it stands now.
            for ev in reader.events(&run, after, usize::MAX)? {
                writeln!(out, "{}", serde_json::to_string(&ev)?)?;
            }
        }
    }
    Ok(())
}

async fn follow_run(reader: &dyn TraceReader, run: &str, after: u64) -> Result<()> {
    // Fail fast on a run that does not exist (the tail would wait forever).
    reader.last_seq(run)?;
    let mut rx = reader.follow(run, after)?;
    // One listener for the whole tail: a Ctrl-C while a line is being
    // written is not lost between two `select!`s.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { return Ok(()) };
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", serde_json::to_string(&ev)?)?;
                out.flush()?;
            }
            _ = &mut ctrl_c => return Ok(()),
        }
    }
}
