//! `tengu backtest` — one strategy spec on the sandbox's market-data
//! warehouse `<state dir>/market.db` (xlab, `docs/xlab-2026-10-01.md` § 6,
//! § 10): the rules arms (`research`, + `capped` with `[risk]` + `[paper]`),
//! a run dir, no LLM. The use case is `application/backtest/`.
//!
//! | Flag | Rule |
//! |---|---|
//! | `--strategy <name>` xor `--spec <file.json>` | a `[backtest.strategies]` name, or a JSON object (named by its `name`, else the file stem) |
//! | `--from` / `--to` | decisions in `[from, to)`: epoch ms, RFC 3339 or a UTC date (`domain::marketdata::parse_time`); default the earliest stored bar of the run's instruments at the spec's interval / now |
//! | `--split time:<t>` · `instruments:<id,…>` | in-sample vs holdout, side by side |
//! | `--format table` (default) · `json` | the compact summary + the run dir · `report.json` on stdout (the run dir on stderr) |
//! | `--fetch` | first `tengu history backfill` (same code: resume, budgets, egress audit) of the run's Hyperliquid instruments at the spec's interval + funding, over the run's data range (from HL's oldest bar without `--from`); prints its run table (stderr with `json`); a failed row is a warning — the run goes on with what is stored |
//!
//! Sandbox resolution, the vault prompt and the installed `[egress]` policy
//! are `tengu history`'s (`cli/mod.rs`); logs go to stderr. Without
//! `--fetch` a missing `market.db` is an error, never created. The Jev gate
//! arm (`--gate`, § 7) slots in between `prepare` and `evaluate`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::Value;

use super::history::{self, BackfillArgs, BackfillSource, Target};
use crate::adapters::outbound::backfill::hl::hl_reach_ms;
use crate::adapters::outbound::market_data::{market_state_dir, open_market_data};
use crate::application::backtest::{
    evaluate, prepare, resolve, write_run_dir, BacktestEnv, BacktestJob, SpecSource,
};
use crate::config::sections::SandboxSections;
use crate::config::xmarket::{backtests_dir, market_db};
use crate::config::Config;
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::marketdata::{fmt_time, parse_time};
use crate::domain::marketdata_decode::hl_coin;
use crate::domain::observation::now_ms;

/// Output of `tengu backtest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum OutputFormat {
    /// The compact summary, then the run dir.
    Table,
    /// `report.json` on stdout; the run dir on stderr.
    Json,
}

/// `tengu backtest` flags (module table).
#[derive(Debug, Clone, PartialEq, Args)]
pub(super) struct BacktestArgs {
    /// A [backtest.strategies.<name>] of the sandbox.
    #[arg(long, conflicts_with = "spec", required_unless_present = "spec")]
    pub(super) strategy: Option<String>,
    /// A JSON strategy spec file (docs/xlab-2026-10-01.md § 5); named by its
    /// "name", else the file stem.
    #[arg(long)]
    pub(super) spec: Option<PathBuf>,
    /// First decision, inclusive: epoch ms, RFC 3339 or a UTC date. Default:
    /// the earliest stored bar of the run's instruments.
    #[arg(long)]
    pub(super) from: Option<String>,
    /// End of decisions, exclusive; default now.
    #[arg(long)]
    pub(super) to: Option<String>,
    /// time:<RFC 3339 | date | ms> (holdout = decided from then) or
    /// instruments:<id,id,…> (holdout = those ids).
    #[arg(long)]
    pub(super) split: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub(super) format: OutputFormat,
    /// Backfill the run's Hyperliquid instruments (bars at the spec's
    /// interval + funding) before running, like `tengu history backfill`.
    #[arg(long)]
    pub(super) fetch: bool,
}

/// The JSON spec of `path` and the name it falls back to (the file stem).
fn read_spec(path: &Path) -> Result<SpecSource> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    Ok(SpecSource::Json {
        value,
        fallback_name: path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string),
    })
}

fn time(s: &str) -> Result<i64> {
    parse_time(s).map_err(|e| anyhow!(e))
}

impl BacktestArgs {
    /// The job these flags ask for; the spec file is read here.
    fn job(&self) -> Result<BacktestJob> {
        let spec = match (&self.strategy, &self.spec) {
            (Some(name), None) => SpecSource::Strategy(name.clone()),
            (None, Some(path)) => read_spec(path)?,
            _ => bail!("give --strategy <name> or --spec <file.json>"),
        };
        Ok(BacktestJob {
            spec,
            from_ms: self.from.as_deref().map(time).transpose()?,
            to_ms: self.to.as_deref().map(time).transpose()?,
            split: self
                .split
                .as_deref()
                .map(SplitSpec::parse)
                .transpose()
                .map_err(|e| anyhow!(e))?,
        })
    }
}

/// `--fetch` (module table): backfill the run's Hyperliquid instruments.
async fn fetch(
    sections: &SandboxSections,
    job: &BacktestJob,
    now: i64,
    format: OutputFormat,
) -> Result<()> {
    let bt = sections.backtest.clone().unwrap_or_default();
    let r = resolve(&bt, &job.spec)?;
    let interval = r.spec.interval;
    let to = job.to_ms.unwrap_or(now);
    let (lo, hi) = match job.from_ms {
        Some(from) => r.data_window(&bt.costs, from, to),
        None => {
            let reach = hl_reach_ms(interval, now);
            (reach, r.data_window(&bt.costs, reach, to).1)
        }
    };
    let targets: Vec<Target> = r
        .instruments
        .iter()
        .filter(|id| hl_coin(id).is_ok())
        .map(|id| Target {
            instrument: id.clone(),
            pool: None,
        })
        .collect();
    if targets.is_empty() {
        eprintln!(
            "--fetch: `{}` trades no Hyperliquid instrument — nothing fetched",
            r.spec.name
        );
        return Ok(());
    }
    let req = BackfillArgs {
        source: BackfillSource::Hl,
        interval,
        from_ms: lo,
        to_ms: hi.min(now),
        bars: true,
        funding: true,
    };
    req.check()?;
    eprintln!(
        "--fetch: {} Hyperliquid instrument(s), {interval} bars + funding, {} → {}",
        targets.len(),
        fmt_time(req.from_ms),
        fmt_time(req.to_ms)
    );
    let report = history::backfill(sections, &req, &targets, now).await?;
    match format {
        OutputFormat::Table => print!("{}", report.render()),
        OutputFormat::Json => eprint!("{}", report.render()),
    }
    let errors = report.error_count();
    if errors > 0 {
        eprintln!(
            "warning: {errors} fetch error(s) (table above) — the backtest runs on what market.db holds"
        );
    }
    Ok(())
}

/// `tengu backtest` (module table).
pub(super) async fn run_backtest(config: &Config, args: BacktestArgs) -> Result<()> {
    let job = args.job()?;
    let sections = history::sections(config);
    let state_dir = market_state_dir(&sections)?.to_path_buf();
    let now = now_ms();
    if args.fetch {
        fetch(&sections, &job, now, args.format).await?;
    } else if !market_db(&state_dir).exists() {
        bail!(
            "nothing stored yet: no {} — backfill first (tengu history backfill --instruments … \
             --interval … --from … --funding) or pass --fetch",
            market_db(&state_dir).display()
        );
    }
    let env = BacktestEnv {
        store: open_market_data(&sections)?,
        sections: sections.clone(),
        backtests_dir: backtests_dir(&state_dir),
        now_ms: now,
    };
    let prepared = prepare(&env, job).await?;
    eprintln!(
        "backtest `{}` ({}, {}): {} instrument(s), decisions {} → {}, {} candidate(s)",
        prepared.spec.name,
        prepared.spec.kind_name(),
        prepared.spec.interval,
        prepared.instruments.len(),
        fmt_time(prepared.params.from_ms),
        fmt_time(prepared.params.to_ms),
        prepared.set.candidates.len()
    );
    // The Jev gate arm (`--gate`, docs/xlab-2026-10-01.md § 7) picks from
    // `prepared.set.candidates` here and passes its takes to `evaluate` as
    // extra arms; its `decisions.jsonl` goes in `run.extra_files`.
    let mut run = evaluate(&prepared, Vec::new())?;
    let dir = write_run_dir(&prepared, &mut run)?;
    match args.format {
        OutputFormat::Table => {
            println!("{}", run.report.render_compact());
            println!("run dir: {}", dir.display());
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&run.report)?);
            eprintln!("run dir: {}", dir.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::super::{Cli, Commands};
    use super::*;

    fn parse(args: &[&str]) -> Result<(Option<String>, BacktestArgs), String> {
        let cli = Cli::try_parse_from(std::iter::once("tengu").chain(args.iter().copied()))
            .map_err(|e| e.to_string())?;
        match cli.command {
            Some(Commands::Backtest { sandbox, args }) => Ok((sandbox, args)),
            _ => Err("not a backtest command".into()),
        }
    }

    #[test]
    fn flags_parse_into_a_job() {
        let (sandbox, a) = parse(&[
            "backtest",
            "--sandbox",
            "xlab",
            "--strategy",
            "weekend_fade",
            "--from",
            "2026-03-07",
            "--to",
            "2026-10-01T00:00:00Z",
            "--split",
            "time:2026-07-01T00:00:00Z",
            "--format",
            "json",
            "--fetch",
        ])
        .unwrap();
        assert_eq!(sandbox.as_deref(), Some("xlab"));
        assert_eq!(
            (a.format, a.fetch, a.strategy.as_deref()),
            (OutputFormat::Json, true, Some("weekend_fade"))
        );
        let job = a.job().unwrap();
        assert_eq!(job.spec, SpecSource::Strategy("weekend_fade".into()));
        assert_eq!(job.from_ms, Some(parse_time("2026-03-07").unwrap()));
        assert_eq!(job.to_ms, Some(parse_time("2026-10-01").unwrap()));
        assert_eq!(
            job.split,
            Some(SplitSpec::Time(parse_time("2026-07-01").unwrap()))
        );
        // Defaults: table, no fetch, no range, no split.
        let (sandbox, a) = parse(&["backtest", "--strategy", "w"]).unwrap();
        assert_eq!(sandbox, None);
        assert_eq!((a.format, a.fetch), (OutputFormat::Table, false));
        let job = a.job().unwrap();
        assert_eq!((job.from_ms, job.to_ms, job.split), (None, None, None));
    }

    #[test]
    fn exactly_one_spec_source_and_valid_values() {
        let e = parse(&["backtest"]).unwrap_err();
        assert!(e.contains("--strategy"), "{e}");
        let e = parse(&["backtest", "--strategy", "a", "--spec", "b.json"]).unwrap_err();
        assert!(e.contains("cannot be used with"), "{e}");
        let e = parse(&["backtest", "--strategy", "a", "--format", "csv"]).unwrap_err();
        assert!(e.contains("csv"), "{e}");
        // Values are checked before any IO.
        for (flag, value, needle) in [
            ("--from", "friday", "is not epoch ms"),
            ("--split", "holdout:x", "split `holdout:x`"),
            ("--split", "instruments:TSLA", "split instruments"),
        ] {
            let (_, a) = parse(&["backtest", "--strategy", "a", flag, value]).unwrap();
            let e = a.job().unwrap_err().to_string();
            assert!(e.contains(needle), "{flag} {value}: {e}");
        }
    }

    #[test]
    fn a_spec_file_is_named_by_its_name_or_its_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("my_fade.json");
        std::fs::write(&path, r#"{"kind": "weekend_window"}"#).unwrap();
        let (_, a) = parse(&["backtest", "--spec", path.to_str().unwrap()]).unwrap();
        assert_eq!(
            a.job().unwrap().spec,
            SpecSource::Json {
                value: serde_json::json!({"kind": "weekend_window"}),
                fallback_name: Some("my_fade".into()),
            }
        );
        std::fs::write(&path, "not json").unwrap();
        let e = a.job().unwrap_err().to_string();
        assert!(e.contains("is not JSON"), "{e}");
        let (_, a) = parse(&["backtest", "--spec", "/nonexistent/x.json"]).unwrap();
        assert!(a
            .job()
            .unwrap_err()
            .to_string()
            .contains("read /nonexistent/x.json"));
    }
}
