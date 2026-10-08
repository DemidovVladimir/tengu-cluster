//! `tengu ranking` — strategy rankings of a `[strategy_ranking]` sandbox
//! (`docs/strategy-ranking-automation-2026-10-08.md` SR-4 / SR-5): `run` one
//! ranking date of a sealed contract through the coordinator
//! (`application/ranking/`), `show` a published one. No LLM, no network: the
//! deterministic entry for an external cron.
//!
//! | Command | Rule |
//! |---|---|
//! | `run [--contract <id>] [--date YYYY-MM-DD] [--format table\|json]` | `--contract` default = the only contract listed; `--date` default = the newest date whose cutoff has passed; lease holder `cli:<pid>`; a missing `market.db` is an error (never created) · table: the ranking's compact lines, whether it ran now, `latest` replaced or kept, the date dir · json: `ranking.json` on stdout, the date dir on stderr · exit 1 on an `INCOMPLETE` ranking (after printing it), a failed run or a refusal (`contract_unsealed`, `contract_changed`, `ranking_busy`, `not_a_ranking_day`, `cutoff_not_reached`, …) |
//! | `show [--contract <id>] [--date YYYY-MM-DD]` | `latest.md`, or the date's `ranking.md`; nothing published there ⇒ an error saying so |
//!
//! Sandbox resolution and logging (stderr) are `tengu backtest`'s
//! (`cli/mod.rs`).

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::NaiveDate;
use clap::{Args, Subcommand, ValueEnum};

use super::history;
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::market_data::{market_state_dir, open_market_data};
use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
use crate::application::ranking::store::published_markdown;
use crate::application::ranking::{
    pick_contract, ranking_section, run_ranking, RankOutcome, RankRequest, RankingEnv,
};
use crate::config::xmarket::market_db;
use crate::config::Config;
use crate::domain::backtest::ranking::RankingStatus;

/// Output of `tengu ranking run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum OutputFormat {
    /// The compact lines, then the date dir.
    Table,
    /// `ranking.json` on stdout; the date dir on stderr.
    Json,
}

/// `tengu ranking` (module table).
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub(super) enum RankingAction {
    /// Run (or resume) one ranking date of a sealed contract and publish it
    /// under <state dir>/strategy-rankings/<contract>/<date>/; a published
    /// date returns as it is. Exit 1 when it is INCOMPLETE.
    Run(RunArgs),
    /// Print a published ranking.md: latest, or --date's.
    Show(ShowArgs),
}

#[derive(Debug, Clone, PartialEq, Args)]
pub(super) struct RunArgs {
    /// A contract of [strategy_ranking] contracts; default: the only one.
    #[arg(long)]
    pub(super) contract: Option<String>,
    /// The ranking date (YYYY-MM-DD, local to the contract's tz); default:
    /// the newest date whose cutoff has passed.
    #[arg(long, value_parser = parse_date)]
    pub(super) date: Option<NaiveDate>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub(super) format: OutputFormat,
}

#[derive(Debug, Clone, PartialEq, Args)]
pub(super) struct ShowArgs {
    /// A contract of [strategy_ranking] contracts; default: the only one.
    #[arg(long)]
    pub(super) contract: Option<String>,
    /// A published date (YYYY-MM-DD); default: latest.
    #[arg(long, value_parser = parse_date)]
    pub(super) date: Option<NaiveDate>,
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("`{s}`: a date, YYYY-MM-DD"))
}

/// `tengu ranking` (module table).
pub(super) async fn run_ranking_command(config: &Config, action: RankingAction) -> Result<()> {
    let sections = history::sections(config);
    let section = ranking_section(&sections)?;
    let state_dir = market_state_dir(&sections)?.to_path_buf();
    match action {
        RankingAction::Run(args) => {
            let contract = pick_contract(section, args.contract.as_deref())?;
            if !market_db(&state_dir).exists() {
                bail!(
                    "nothing stored yet: no {} — backfill first (tengu history backfill …)",
                    market_db(&state_dir).display()
                );
            }
            let env = RankingEnv {
                store: open_market_data(&sections)?,
                sections: Arc::clone(&sections),
                runtime: Arc::new(SqliteRuntimeStore::open(&state_dir)?),
                clock: Arc::new(SystemClock),
                state_dir,
            };
            let req = RankRequest {
                contract,
                date: args.date,
                holder: format!("cli:{}", std::process::id()),
            };
            let out = run_ranking(&env, req).await?;
            match args.format {
                OutputFormat::Table => print!("{}", summary(&out)),
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&out.ranking)?);
                    eprintln!("ranking dir: {}", out.dir.display());
                }
            }
            if out.status == RankingStatus::Incomplete {
                bail!(
                    "ranking {} of `{}` is INCOMPLETE: {} listed strateg(ies) without a run — the \
                     dated files are written, latest is kept",
                    out.ranking.date,
                    out.ranking.contract,
                    out.ranking.failed.len()
                );
            }
            Ok(())
        }
        RankingAction::Show(args) => {
            let contract = pick_contract(section, args.contract.as_deref())?;
            print!("{}", show(&state_dir, &contract, args.date)?);
            Ok(())
        }
    }
}

/// `run`'s table output: the compact ranking + what this call did.
fn summary(out: &RankOutcome) -> String {
    let mut s = out.ranking.render_compact();
    s.push_str(match (out.published, out.latest_replaced) {
        (true, true) => "published now · latest replaced\n",
        (true, false) => "published now · latest kept\n",
        (false, _) => "already published by an earlier run — nothing ran\n",
    });
    s.push_str(&format!("ranking dir: {}\n", out.dir.display()));
    s
}

/// `show` (module table).
fn show(state_dir: &Path, contract: &str, date: Option<NaiveDate>) -> Result<String> {
    match published_markdown(state_dir, contract, date)? {
        Some((_, text)) => Ok(text),
        None => match date {
            Some(d) => bail!(
                "no published ranking of `{contract}` for {d} under {} (a run still RUNNING or \
                 FAILED publishes nothing)",
                state_dir.display()
            ),
            None => bail!(
                "no published ranking of `{contract}` yet under {} — `tengu ranking run` \
                 publishes one (latest moves on COMPLETE only)",
                state_dir.display()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::super::{Cli, Commands};
    use super::*;

    fn parse(args: &[&str]) -> Result<(Option<String>, RankingAction), String> {
        let cli = Cli::try_parse_from(std::iter::once("tengu").chain(args.iter().copied()))
            .map_err(|e| e.to_string())?;
        match cli.command {
            Some(Commands::Ranking { sandbox, action }) => Ok((sandbox, action)),
            _ => Err("not a ranking command".into()),
        }
    }

    #[test]
    fn args_parse() {
        let (sandbox, a) = parse(&[
            "ranking",
            "run",
            "--sandbox",
            "xlab-w2",
            "--contract",
            "rank.xlab-w2.daily.v1",
            "--date",
            "2026-10-09",
            "--format",
            "json",
        ])
        .unwrap();
        assert_eq!(sandbox.as_deref(), Some("xlab-w2"));
        assert_eq!(
            a,
            RankingAction::Run(RunArgs {
                contract: Some("rank.xlab-w2.daily.v1".into()),
                date: NaiveDate::from_ymd_opt(2026, 10, 9),
                format: OutputFormat::Json,
            })
        );
        // Defaults; --sandbox before the subcommand (global).
        let (sandbox, a) = parse(&["ranking", "--sandbox", "xlab-w2", "run"]).unwrap();
        assert_eq!(sandbox.as_deref(), Some("xlab-w2"));
        assert_eq!(
            a,
            RankingAction::Run(RunArgs {
                contract: None,
                date: None,
                format: OutputFormat::Table,
            })
        );
        let (_, a) = parse(&["ranking", "show", "--date", "2026-11-02"]).unwrap();
        assert_eq!(
            a,
            RankingAction::Show(ShowArgs {
                contract: None,
                date: NaiveDate::from_ymd_opt(2026, 11, 2),
            })
        );
        for (argv, needle) in [
            (
                &["ranking", "run", "--date", "2026-10-9x"][..],
                "YYYY-MM-DD",
            ),
            (
                &["ranking", "run", "--date", "10/09/2026"][..],
                "YYYY-MM-DD",
            ),
            (&["ranking", "run", "--format", "csv"][..], "csv"),
            (&["ranking"][..], "Usage"),
        ] {
            let e = parse(argv).unwrap_err();
            assert!(e.contains(needle), "{argv:?}: {e}");
        }
    }

    #[test]
    fn show_without_a_ranking_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let e = show(tmp.path(), "rank.fixture.v1", None)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("no published ranking of `rank.fixture.v1` yet under"),
            "{e}"
        );
        let d = NaiveDate::from_ymd_opt(2026, 10, 9);
        let e = show(tmp.path(), "rank.fixture.v1", d)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("no published ranking of `rank.fixture.v1` for 2026-10-09"),
            "{e}"
        );
        // Published: latest.md and the dated ranking.md are printed as written.
        let dir = tmp.path().join("strategy-rankings/rank.fixture.v1");
        std::fs::create_dir_all(dir.join("2026-10-09")).unwrap();
        std::fs::write(dir.join("latest.md"), "# latest\n").unwrap();
        std::fs::write(dir.join("2026-10-09/ranking.md"), "# dated\n").unwrap();
        assert_eq!(
            show(tmp.path(), "rank.fixture.v1", None).unwrap(),
            "# latest\n"
        );
        assert_eq!(show(tmp.path(), "rank.fixture.v1", d).unwrap(), "# dated\n");
    }
}
