//! `tengu history …` — the sandbox's history stores, no LLM.
//!
//! | Subcommand | Store | Does |
//! |---|---|---|
//! | `range <key> --from --to` · `asof <keys…> --at` | recorder day files (`[recorder]`, `<state dir>/history/`) | JSON lines with full keys; a `SqliteHistoryStore::reader` creates and deletes nothing |
//! | `backfill --instruments <items> [--source hl\|gecko] [--interval 1h] --from [--to] [--funding] [--no-bars]` | `<state dir>/market.db` (xlab) | HL bars (+ funding) or GeckoTerminal pool bars (`outbound/backfill/`); resumes; prints the run table; exit 1 when any row failed |
//! | `import-hl-archive --dir <dir>` | `market.db` `ctx` | HL S3 `asset_ctxs` files (`*.csv.lz4`, `*.csv`) |
//! | `import-json --file <path>` | `market.db` `bars` / `funding` | a JSON dataset file |
//! | `coverage [--instrument <id>]` | `market.db` | instrument, kind, interval, first, last, rows, sources (full ids) |
//!
//! | Rule | Value |
//! |---|---|
//! | Times | epoch ms, RFC 3339 or `YYYY-MM-DD` (`domain::marketdata::parse_time`) |
//! | Items | comma-separated full ids; `@<name>` = `[backtest.universes.<name>]`; Gecko: `<id>@<pool address>` |
//! | Network | the process's installed `[egress]` policy (`cli/mod.rs` installs the sandbox's): proxy, `allow_hosts`, audit lines attributed `agent = cli`, `session = history-backfill:<start ms>`, `call_id = <instrument>` |

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use clap::{Subcommand, ValueEnum};

use crate::adapters::outbound::backfill::gecko::{
    gecko_bars, network_of, operator_gecko, GeckoPlan,
};
use crate::adapters::outbound::backfill::hl::{hl_bars, hl_funding_history, operator_hl, HlPlan};
use crate::adapters::outbound::backfill::hl_archive::import_hl_archive;
use crate::adapters::outbound::backfill::json::import_json;
use crate::adapters::outbound::backfill::{text_table, BackfillReport, Retry};
use crate::adapters::outbound::egress::{audited_as, CallScope};
use crate::adapters::outbound::history_sqlite::SqliteHistoryStore;
use crate::adapters::outbound::market_data::{market_state_dir, open_market_data};
use crate::config::backtest::BacktestConfig;
use crate::config::paths::resolve_tengu_home;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::market_db;
use crate::config::Config;
use crate::domain::market::InstrumentId;
use crate::domain::marketdata::{fmt_time, parse_time, Interval};
use crate::domain::marketdata_decode::hl_coin;
use crate::domain::observation::now_ms;
use crate::ports::history::HistoryStore;

#[derive(Subcommand)]
pub(super) enum HistoryAction {
    /// Rows of one key with from <= observed_at < to, oldest first.
    Range {
        /// Observation key, e.g. mkt_ctx/1:hyperliquid:xyz:TSLA
        key: String,
        /// Start, inclusive: epoch ms, RFC 3339 (2026-10-02T20:00:00-04:00) or a UTC date.
        #[arg(long)]
        from: String,
        /// End, exclusive: epoch ms, RFC 3339 or a UTC date.
        #[arg(long)]
        to: String,
    },
    /// Per key, the latest row at or before --at (`"row": null` when none).
    Asof {
        /// One or more observation keys.
        #[arg(required = true)]
        keys: Vec<String>,
        /// Epoch ms, RFC 3339 or a UTC date.
        #[arg(long)]
        at: String,
        /// Ignore rows older than this; default: any age.
        #[arg(long)]
        max_age_secs: Option<u64>,
    },
    /// Fill <state dir>/market.db from Hyperliquid (bars, --funding) or
    /// GeckoTerminal (pool bars). Resumes: only what is not stored yet.
    Backfill {
        /// Comma-separated full ids (hyperliquid:xyz:TSLA), `@<name>` =
        /// [backtest.universes.<name>]; gecko: `<id>@<pool address>`.
        #[arg(long)]
        instruments: String,
        #[arg(long, value_enum, default_value_t = BackfillSource::Hl)]
        source: BackfillSource,
        /// 1m 5m 15m 1h 4h 1d (HL keeps the newest 5000 bars per interval).
        #[arg(long, default_value = "1h")]
        interval: String,
        /// Start, inclusive: epoch ms, RFC 3339 or a UTC date (2026-03-01).
        #[arg(long)]
        from: String,
        /// End, exclusive; default now.
        #[arg(long)]
        to: Option<String>,
        /// Also HL funding history (hl only).
        #[arg(long)]
        funding: bool,
        /// Skip bars (with --funding: funding only).
        #[arg(long)]
        no_bars: bool,
    },
    /// Import HL S3 archive asset contexts (`*.csv.lz4`, `*.csv` under --dir,
    /// downloaded with `aws s3 cp --request-payer requester`) into market.db.
    ImportHlArchive {
        #[arg(long)]
        dir: PathBuf,
    },
    /// Import a JSON dataset file ([{instrument, interval, source?, bars?,
    /// funding?}]) into market.db; nothing is written unless every row checks.
    ImportJson {
        #[arg(long)]
        file: PathBuf,
    },
    /// What market.db holds: instrument, kind, interval, first, last, rows, sources.
    Coverage {
        /// One full instrument id; default all.
        #[arg(long)]
        instrument: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum BackfillSource {
    /// Hyperliquid `POST /info` (candleSnapshot, fundingHistory).
    Hl,
    /// GeckoTerminal pool OHLCV.
    Gecko,
}

pub(super) async fn run_history(config: &Config, action: HistoryAction) -> Result<()> {
    match action {
        HistoryAction::Range { key, from, to } => {
            let (store, dir) = recorder(config)?;
            let rows = store.range(&key, time(&from)?, time(&to)?).await?;
            for row in &rows {
                println!("{}", serde_json::to_string(row)?);
            }
            eprintln!("{} rows from {}", rows.len(), dir.display());
        }
        HistoryAction::Asof {
            keys,
            at,
            max_age_secs,
        } => {
            let (store, _) = recorder(config)?;
            let max_age_ms = max_age_secs.map_or(u64::MAX, |s| s.saturating_mul(1000));
            let rows = store.asof(&keys, time(&at)?, max_age_ms).await?;
            for (key, row) in keys.iter().zip(rows) {
                println!("{}", serde_json::json!({ "key": key, "row": row }));
            }
        }
        HistoryAction::Backfill {
            instruments,
            source,
            interval,
            from,
            to,
            funding,
            no_bars,
        } => {
            let now = now_ms();
            let req = BackfillArgs {
                source,
                interval: Interval::parse(&interval).map_err(|e| anyhow!(e))?,
                from_ms: time(&from)?,
                to_ms: to.as_deref().map(time).transpose()?.unwrap_or(now),
                bars: !no_bars,
                funding,
            };
            let sections = sections(config);
            let targets = targets(&instruments, source, sections.backtest.as_ref())?;
            req.check()?;
            let report = backfill(&sections, &req, &targets, now).await?;
            finish(report)?;
        }
        HistoryAction::ImportHlArchive { dir } => {
            let store = open_market_data(&sections(config))?;
            finish(import_hl_archive(&dir, store.as_ref()).await?)?;
        }
        HistoryAction::ImportJson { file } => {
            let store = open_market_data(&sections(config))?;
            finish(import_json(&file, store.as_ref(), now_ms()).await?)?;
        }
        HistoryAction::Coverage { instrument } => {
            let sections = sections(config);
            let path = market_db(market_state_dir(&sections)?);
            if !path.exists() {
                println!("nothing stored yet: no {}", path.display());
                return Ok(());
            }
            let rows = open_market_data(&sections)?
                .coverage(instrument.as_deref())
                .await?;
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    vec![
                        r.instrument.clone(),
                        r.kind.clone(),
                        r.interval.map_or("-".to_string(), |i| i.to_string()),
                        fmt_time(r.first_ms),
                        fmt_time(r.last_ms),
                        r.rows.to_string(),
                        r.sources.join(", "),
                    ]
                })
                .collect();
            print!(
                "{}",
                text_table(
                    &[
                        "instrument",
                        "kind",
                        "interval",
                        "first",
                        "last",
                        "rows",
                        "sources"
                    ],
                    &cells
                )
            );
            eprintln!("{} row(s) from {}", rows.len(), path.display());
        }
    }
    Ok(())
}

/// The recorder's day files (read-only).
fn recorder(config: &Config) -> Result<(SqliteHistoryStore, PathBuf)> {
    let xmarket = config.xmarket.as_ref().ok_or_else(|| {
        anyhow!("no [xmarket] section: history lives in <TENGU_HOME>/state/<xmarket.state>/history")
    })?;
    let dir = xmarket.history_dir(&resolve_tengu_home());
    Ok((SqliteHistoryStore::reader(&dir), dir))
}

/// The sandbox sections every agent of `config` shares (`Config::load`).
pub(super) fn sections(config: &Config) -> Arc<SandboxSections> {
    config
        .agents
        .values()
        .next()
        .map(|a| Arc::clone(&a.sandbox))
        .unwrap_or_default()
}

fn time(s: &str) -> Result<i64> {
    parse_time(s).map_err(|e| anyhow!(e))
}

/// Print the run table; exit 1 (an error) when any row failed.
fn finish(report: BackfillReport) -> Result<()> {
    print!("{}", report.render());
    match report.error_count() {
        0 => Ok(()),
        n => bail!("{n} error(s) — see the table above; rows written stay (a re-run resumes)"),
    }
}

/// The parsed `backfill` flags (`tengu backtest --fetch` builds them too).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BackfillArgs {
    pub(super) source: BackfillSource,
    pub(super) interval: Interval,
    pub(super) from_ms: i64,
    pub(super) to_ms: i64,
    pub(super) bars: bool,
    pub(super) funding: bool,
}

impl BackfillArgs {
    pub(super) fn check(&self) -> Result<()> {
        if self.from_ms >= self.to_ms {
            bail!(
                "--from {} is not before --to {}",
                fmt_time(self.from_ms),
                fmt_time(self.to_ms)
            );
        }
        if !self.bars && !self.funding {
            bail!("nothing to do: --no-bars without --funding");
        }
        if self.source == BackfillSource::Gecko && (self.funding || !self.bars) {
            bail!("GeckoTerminal has bars only: drop --funding / --no-bars");
        }
        Ok(())
    }
}

/// One backfill target: a full id, and its pool for Gecko.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Target {
    pub(super) instrument: String,
    pub(super) pool: Option<String>,
}

/// `--instruments` → targets, in order, without repeats; universes
/// expanded; each id checked for the source before any request.
fn targets(
    items: &str,
    source: BackfillSource,
    backtest: Option<&BacktestConfig>,
) -> Result<Vec<Target>> {
    let mut expanded: Vec<String> = Vec::new();
    for item in items.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(name) = item.strip_prefix('@') {
            let bt = backtest.ok_or_else(|| {
                anyhow!("`{item}` needs [backtest.universes.{name}]: the sandbox has no [backtest]")
            })?;
            expanded.extend(bt.resolve_universe(item).map_err(|e| anyhow!(e))?);
        } else {
            expanded.push(item.to_string());
        }
    }
    let mut out: Vec<Target> = Vec::new();
    for item in expanded {
        let target = match source {
            BackfillSource::Hl => {
                hl_coin(&item).map_err(|e| anyhow!(e))?;
                Target {
                    instrument: item,
                    pool: None,
                }
            }
            BackfillSource::Gecko => {
                let (id, pool) = item
                    .rsplit_once('@')
                    .filter(|(id, pool)| !id.is_empty() && !pool.is_empty())
                    .ok_or_else(|| {
                        anyhow!("gecko item `{item}` is not `<instrument id>@<pool address>`")
                    })?;
                let parsed = InstrumentId::parse(id).map_err(|e| anyhow!(e))?;
                network_of(parsed.venue()).map_err(|e| anyhow!("`{id}`: {e}"))?;
                Target {
                    instrument: id.to_string(),
                    pool: Some(pool.to_string()),
                }
            }
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    if out.is_empty() {
        bail!("--instruments names no instrument");
    }
    Ok(out)
}

/// Fill `market.db` for `targets` per `req` (module table); also `tengu
/// backtest --fetch`'s.
pub(super) async fn backfill(
    sections: &SandboxSections,
    req: &BackfillArgs,
    targets: &[Target],
    now: i64,
) -> Result<BackfillReport> {
    let store = open_market_data(sections)?;
    let retry = Retry::BACKFILL;
    let mut report = BackfillReport::default();
    let session = format!("history-backfill:{now}");
    let scope = |instrument: &str| CallScope {
        agent: "cli".to_string(),
        session: session.clone(),
        call_id: instrument.to_string(),
    };
    match req.source {
        BackfillSource::Hl => {
            let hl = operator_hl(sections)?;
            for t in targets {
                let plan = HlPlan {
                    instrument: t.instrument.clone(),
                    interval: req.interval,
                    from_ms: req.from_ms,
                    to_ms: req.to_ms,
                };
                let rows = audited_as(scope(&t.instrument), async {
                    let mut rows = Vec::new();
                    if req.bars {
                        rows.push(hl_bars(&hl, store.as_ref(), &plan, &retry, now).await);
                    }
                    if req.funding {
                        rows.push(
                            hl_funding_history(&hl, store.as_ref(), &plan, &retry, now).await,
                        );
                    }
                    rows
                })
                .await;
                for row in rows {
                    progress(&row);
                    report.rows.push(row);
                }
            }
        }
        BackfillSource::Gecko => {
            let gecko = operator_gecko(sections)?;
            for t in targets {
                let plan = GeckoPlan {
                    instrument: t.instrument.clone(),
                    pool: t.pool.clone().unwrap_or_default(),
                    interval: req.interval,
                    from_ms: req.from_ms,
                    to_ms: req.to_ms,
                };
                let row = audited_as(
                    scope(&t.instrument),
                    gecko_bars(&gecko, store.as_ref(), &plan, &retry, now),
                )
                .await;
                progress(&row);
                report.rows.push(row);
            }
        }
    }
    Ok(report)
}

/// One stderr line per finished row (a long backfill shows its pace).
fn progress(row: &crate::adapters::outbound::backfill::ReportRow) {
    let status = match row.errors.first() {
        None => "ok".to_string(),
        Some(e) => format!("error: {e}"),
    };
    eprintln!(
        "{}: {} rows, {} request(s) — {status}",
        row.label(),
        row.rows,
        row.reads
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: &str = "So11111111111111111111111111111111111111112";
    const POOL: &str = "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj";

    fn bt() -> BacktestConfig {
        toml::from_str(
            r#"
            [universes]
            crypto = ["hyperliquid:BTC", "hyperliquid:SOL"]
            pools = ["solana:So11111111111111111111111111111111111111112@8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn times_parse_as_ms_rfc3339_or_a_date() {
        assert_eq!(time("1759449600000").unwrap(), 1_759_449_600_000);
        // Fri 2026-10-02 20:00 EDT = Sat 00:00 UTC.
        assert_eq!(
            time("2026-10-02T20:00:00-04:00").unwrap(),
            time("2026-10-03").unwrap()
        );
        assert!(time("friday")
            .unwrap_err()
            .to_string()
            .contains("is not epoch ms"));
    }

    #[test]
    fn items_expand_universes_and_split_gecko_pools() {
        let hl = targets(
            " hyperliquid:xyz:TSLA, @crypto ,hyperliquid:SOL",
            BackfillSource::Hl,
            Some(&bt()),
        )
        .unwrap();
        let ids: Vec<&str> = hl.iter().map(|t| t.instrument.as_str()).collect();
        assert_eq!(
            ids,
            vec!["hyperliquid:xyz:TSLA", "hyperliquid:BTC", "hyperliquid:SOL"],
            "order kept, no repeats"
        );
        assert!(hl.iter().all(|t| t.pool.is_none()));

        let g = targets(
            &format!("solana:{MINT}@{POOL},@pools"),
            BackfillSource::Gecko,
            Some(&bt()),
        )
        .unwrap();
        assert_eq!(
            g,
            vec![Target {
                instrument: format!("solana:{MINT}"),
                pool: Some(POOL.into())
            }]
        );

        for (items, source, needle) in [
            ("@crypto", BackfillSource::Hl, "has no [backtest]"),
            (
                &*format!("solana:{MINT}"),
                BackfillSource::Hl,
                "not a Hyperliquid instrument",
            ),
            (
                &*format!("solana:{MINT}"),
                BackfillSource::Gecko,
                "not `<instrument id>@<pool address>`",
            ),
            (
                "hyperliquid:SOL@pool1",
                BackfillSource::Gecko,
                "no GeckoTerminal network",
            ),
            (" , ", BackfillSource::Hl, "names no instrument"),
        ] {
            let backtest = (needle != "has no [backtest]").then(bt);
            let e = targets(items, source, backtest.as_ref()).unwrap_err();
            assert!(e.to_string().contains(needle), "{items}: {e}");
        }
        let e = targets("@nope", BackfillSource::Hl, Some(&bt())).unwrap_err();
        assert!(
            e.to_string()
                .contains("no [backtest.universes] entry `nope`"),
            "{e}"
        );
    }

    #[test]
    fn flags_are_checked_before_any_request() {
        let ok = BackfillArgs {
            source: BackfillSource::Hl,
            interval: Interval::H1,
            from_ms: 0,
            to_ms: 1,
            bars: true,
            funding: true,
        };
        assert!(ok.check().is_ok());
        for (args, needle) in [
            (
                BackfillArgs {
                    to_ms: 0,
                    ..ok.clone()
                },
                "is not before --to",
            ),
            (
                BackfillArgs {
                    bars: false,
                    funding: false,
                    ..ok.clone()
                },
                "nothing to do",
            ),
            (
                BackfillArgs {
                    source: BackfillSource::Gecko,
                    ..ok.clone()
                },
                "bars only",
            ),
        ] {
            assert!(
                args.check().unwrap_err().to_string().contains(needle),
                "{needle}"
            );
        }
    }
}
