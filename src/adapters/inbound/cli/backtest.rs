//! `tengu backtest` — one strategy spec on the sandbox's market-data
//! warehouse `<state dir>/market.db` (xlab, `docs/xlab-2026-10-01.md` § 6,
//! § 7, § 10): the rules arms (`research`, + `capped` with `[risk]` +
//! `[paper]`), with `--gate` the Jev gate arm, a run dir. The use case is
//! `application/backtest/`.
//!
//! | Flag | Rule |
//! |---|---|
//! | `--strategy <name>` xor `--spec <file.json>` | a `[backtest.strategies]` name, or a JSON object (named by its `name`, else the file stem) |
//! | `--from` / `--to` | decisions in `[from, to)`: epoch ms, RFC 3339 or a UTC date (`domain::marketdata::parse_time`); default the earliest stored bar of the run's instruments at the spec's interval / now |
//! | `--split time:<t>` · `instruments:<id,…>` | in-sample vs holdout, side by side |
//! | `--format table` (default) · `json` | the compact summary (+ the gate's two lines) + the run dir · `report.json` on stdout (the run dir on stderr) |
//! | `--fetch` | a kind outside the sandbox's `[generation]` is refused before anything is fetched (`capability_unavailable`); then `tengu history backfill` (same code: resume, budgets, egress audit) of the run's Hyperliquid instruments at the spec's interval + funding, over the run's data range (from HL's oldest bar without `--from`); prints its run table (stderr with `json`); a failed row is a warning — the run goes on with what is stored |
//! | `--gate [<loop>]` | the Jev gate arm (§ 7) with `[decision_loops.<loop>]`; no value = `[backtest] gate` (neither ⇒ an error). Built before `--fetch` and `prepare` — an unknown or tool-calling loop, or a missing `OPENROUTER_API_KEY` online, fails before any work |
//! | `--max-decisions <n>` | 500: the first n candidates by seq are decided, the rest counted as cut (`report.md`, the CLI lines) |
//! | `--concurrency <k>` | 4, 1–16: replay loops deciding at once (the decisions never depend on k) |
//! | `--offline` | decision cache only: a miss is a class-`error` decision, nothing is called, no key needed; est. cost 0 |
//!
//! Sandbox resolution, the vault prompt and the installed `[egress]` policy
//! are `tengu history`'s (`cli/mod.rs`); logs go to stderr. Without
//! `--fetch` a missing `market.db` is an error, never created. The gate's
//! audit goes to a temp file while it runs (`application::backtest::gate::
//! GateAudit`) and lands in the claimed run dir as `decisions.jsonl` — a run
//! that fails leaves no file behind; the decision cache
//! `<state dir>/backtests/decision-cache.db` keeps every answered call.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::Value;

use super::history::{self, BackfillArgs, BackfillSource, Target};
use crate::adapters::outbound::backfill::hl::hl_reach_ms;
use crate::adapters::outbound::market_data::{market_state_dir, open_market_data};
use crate::application::backtest::gate::{describe, evaluate_gated, run_gate, Gate, GateAudit};
use crate::application::backtest::{
    capability_refusal, evaluate, prepare, resolve, write_run_dir, BacktestEnv, BacktestJob,
    BacktestRun, Prepared, SpecSource,
};
use crate::bootstrap::decision::build_gate;
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::{backtests_dir, market_db};
use crate::config::Config;
use crate::domain::backtest::gate::{GateClass, COST_PER_DECISION_USD};
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::marketdata::{fmt_time, parse_time};
use crate::domain::marketdata_decode::hl_coin;
use crate::domain::observation::now_ms;

/// Failed gate calls printed (stderr) before "… and N more".
const SHOW_FAILED: usize = 5;

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
    /// Add the Jev gate arm (docs/xlab-2026-10-01.md § 7):
    /// [decision_loops.<LOOP>] decides every candidate through the decision
    /// cache; no value = [backtest] gate.
    #[arg(long, value_name = "LOOP")]
    pub(super) gate: Option<Option<String>>,
    /// Candidates the gate decides at most (the earliest by seq); the rest
    /// are counted as cut.
    #[arg(long, default_value_t = 500, requires = "gate")]
    pub(super) max_decisions: usize,
    /// Gate decisions in flight (1-16).
    #[arg(long, default_value_t = 4, requires = "gate",
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=16))]
    pub(super) concurrency: usize,
    /// Gate from the decision cache only: a miss is a class-error decision;
    /// nothing is called and no API key is needed.
    #[arg(long, requires = "gate")]
    pub(super) offline: bool,
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

    /// The `[decision_loops.<name>]` `--gate` runs: its value, else `[backtest]
    /// gate`; `None` without `--gate`.
    fn gate_loop(&self, bt: Option<&BacktestConfig>) -> Result<Option<String>> {
        match &self.gate {
            None => Ok(None),
            Some(Some(name)) if !name.trim().is_empty() => Ok(Some(name.trim().to_string())),
            Some(_) => bt.and_then(|b| b.gate.clone()).map(Some).ok_or_else(|| {
                anyhow!(
                    "--gate without a loop name runs [backtest] gate, which this sandbox does \
                     not set — give --gate <loop> (a [decision_loops.<loop>] of the sandbox)"
                )
            }),
        }
    }
}

/// `--gate` (module table): build the gate before any work, decide, then
/// evaluate with its arms; failed calls are listed on stderr.
struct GateJob {
    loop_name: String,
    gate: Gate,
    audit: GateAudit,
}

impl GateJob {
    fn build(
        config: &Config,
        args: &BacktestArgs,
        loop_name: String,
        state_dir: &Path,
    ) -> Result<Self> {
        let audit = GateAudit::new()?;
        let gate = build_gate(
            config,
            &loop_name,
            state_dir,
            audit.path(),
            args.concurrency,
            args.offline,
        )
        .with_context(|| format!("jev gate `{loop_name}`"))?;
        Ok(Self {
            loop_name,
            gate,
            audit,
        })
    }

    async fn run(&self, prepared: &Prepared, args: &BacktestArgs) -> Result<BacktestRun> {
        let n = prepared.set.candidates.len();
        eprintln!(
            "jev gate `{}` ({}): deciding {} of {n} candidate(s), {} at a time{}",
            self.loop_name,
            self.gate.engine.model(),
            n.min(args.max_decisions),
            args.concurrency,
            if args.offline {
                ", offline (decision cache only)"
            } else {
                ""
            }
        );
        let started = Instant::now();
        let decided = run_gate(
            &prepared.set.candidates,
            &prepared.spec.name,
            &self.gate,
            args.max_decisions,
        )
        .await?;
        eprintln!(
            "jev gate `{}`: {} decided in {:.1} s",
            decided.loop_name,
            decided.decided,
            started.elapsed().as_secs_f64()
        );
        let failed: Vec<_> = decided
            .decisions
            .iter()
            .filter(|d| d.class == GateClass::Error)
            .collect();
        for d in failed.iter().take(SHOW_FAILED) {
            let id = prepared
                .set
                .candidates
                .iter()
                .find(|c| c.seq == d.seq)
                .map_or("", |c| c.instrument.as_str());
            eprintln!("  {id} {}", describe(d));
        }
        if failed.len() > SHOW_FAILED {
            eprintln!("  … and {} more failed call(s)", failed.len() - SHOW_FAILED);
        }
        let cost = if args.offline {
            0.0
        } else {
            COST_PER_DECISION_USD
        };
        evaluate_gated(prepared, &decided, &self.audit, cost)
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
    // A kind outside the bound generation fetches nothing (`prepare` refuses it too).
    if let Some(refusal) = capability_refusal(sections, &r.spec) {
        bail!("{refusal}");
    }
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
    let gate_loop = args.gate_loop(sections.backtest.as_ref())?;
    let now = now_ms();
    if !args.fetch && !market_db(&state_dir).exists() {
        bail!(
            "nothing stored yet: no {} — backfill first (tengu history backfill --instruments … \
             --interval … --from … --funding) or pass --fetch",
            market_db(&state_dir).display()
        );
    }
    // Before any fetch or run: an unknown or tool-calling loop, or no key
    // online, fails here.
    let gate = gate_loop
        .map(|name| GateJob::build(config, &args, name, &state_dir))
        .transpose()?;
    if args.fetch {
        fetch(&sections, &job, now, args.format).await?;
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
    let mut run = match &gate {
        Some(g) => g.run(&prepared, &args).await?,
        None => evaluate(&prepared, Vec::new())?,
    };
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

    /// `--gate [<loop>]`: a value names the loop, no value reads `[backtest]
    /// gate` (neither ⇒ an error); the gate's knobs default to 500 / 4 /
    /// online, need `--gate`, and `--concurrency` stays within 1–16.
    #[test]
    fn gate_flags_parse_and_resolve_the_loop() {
        let bt = |gate: Option<&str>| BacktestConfig {
            gate: gate.map(str::to_string),
            ..Default::default()
        };
        // (flags, the loop with [backtest] gate = "xl_gate", without one)
        let cases: [(&[&str], Option<&str>, Result<Option<&str>, &str>); 5] = [
            (&[], None, Ok(None)),
            (&["--gate"], Some("xl_gate"), Err("does not set")),
            (&["--gate", "other"], Some("other"), Ok(Some("other"))),
            (&["--gate=other"], Some("other"), Ok(Some("other"))),
            (
                &["--gate", "--offline"],
                Some("xl_gate"),
                Err("--gate <loop>"),
            ),
        ];
        for (flags, with, without) in cases {
            let mut argv = vec!["backtest", "--strategy", "w"];
            argv.extend_from_slice(flags);
            let (_, a) = parse(&argv).unwrap_or_else(|e| panic!("{flags:?}: {e}"));
            assert_eq!(
                a.gate_loop(Some(&bt(Some("xl_gate")))).unwrap().as_deref(),
                with,
                "{flags:?}"
            );
            match (a.gate_loop(Some(&bt(None))), without) {
                (Ok(got), Ok(want)) => assert_eq!(got.as_deref(), want, "{flags:?}"),
                (Err(e), Err(want)) => assert!(e.to_string().contains(want), "{flags:?}: {e}"),
                (got, want) => panic!("{flags:?}: {got:?} vs {want:?}"),
            }
            assert!(a.gate_loop(None).is_ok() == (flags.is_empty() || with == Some("other")));
        }
        let (_, a) = parse(&["backtest", "--strategy", "w", "--gate"]).unwrap();
        assert_eq!((a.max_decisions, a.concurrency, a.offline), (500, 4, false));
        let (_, a) = parse(&[
            "backtest",
            "--strategy",
            "w",
            "--gate",
            "xl_gate",
            "--max-decisions",
            "200",
            "--concurrency",
            "16",
            "--offline",
        ])
        .unwrap();
        assert_eq!(
            (a.gate.clone(), a.max_decisions, a.concurrency, a.offline),
            (Some(Some("xl_gate".to_string())), 200, 16, true)
        );
        for (flags, needle) in [
            (&["--gate", "--concurrency", "0"][..], "1..=16"),
            (&["--gate", "--concurrency", "17"][..], "1..=16"),
            (&["--max-decisions", "10"][..], "--gate"),
            (&["--offline"][..], "--gate"),
            (&["--concurrency", "2"][..], "--gate"),
        ] {
            let mut argv = vec!["backtest", "--strategy", "w"];
            argv.extend_from_slice(flags);
            let e = parse(&argv).unwrap_err();
            assert!(e.contains(needle), "{flags:?}: {e}");
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

    /// Review #16: `--fetch` of a kind the bound generation lacks writes
    /// nothing into `market.db` — refused before the backfill.
    #[tokio::test]
    async fn fetch_refuses_a_kind_outside_the_generation_before_fetching() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/registry");
        let reg = crate::config::lineage::load_registry(&dir).unwrap();
        let sections = SandboxSections {
            generation: Some(std::sync::Arc::new(
                crate::domain::lineage::generation::GenerationScope::of(&reg, "W1").unwrap(),
            )),
            ..Default::default()
        };
        let job = BacktestJob {
            spec: SpecSource::Json {
                value: serde_json::json!({"name": "news", "kind": "event_window", "interval": "1h",
                    "events": [{"instrument": "hyperliquid:xyz:TSLA", "t": "2026-09-10T14:00:00Z"}],
                    "direction": "follow", "exit_after_mins": 120}),
                fallback_name: None,
            },
            from_ms: Some(parse_time("2026-09-01").unwrap()),
            to_ms: Some(parse_time("2026-09-20").unwrap()),
            split: None,
        };
        let e = fetch(&sections, &job, now_ms(), OutputFormat::Table)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("capability_unavailable: strategy `news`"),
            "{e}"
        );
    }
}
