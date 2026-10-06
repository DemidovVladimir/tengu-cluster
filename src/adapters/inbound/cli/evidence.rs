//! `tengu evidence …` — preserve and grade forward evidence
//! (`docs/lineage-2026-10-06.md` § 3, roadmap Phase 0); no config, no LLM,
//! no network. Every reader is read-only (`adapters/outbound/evidence/`);
//! the only write is a new vault + the record file `snapshot` fills.
//!
//! | Subcommand | Does |
//! |---|---|
//! | `snapshot --record <file>` | copy the record's items into `<TENGU_HOME>/state/evidence/<vault>/` (refused when it exists or the record is captured), hash, `MANIFEST.json`, `chmod a-w`; rewrites the record with the captured fields |
//! | `verify <record file>` | re-hash the vault: MATCH · MISMATCH · ABSENT · EXTRA; exit 1 unless all MATCH |
//! | `coverage --history <dir>… --schema <s> --from --to --cadence-secs N [--bars <market.db> --interval 1m] [--instruments ids…]` | cadence slots, gaps, `LIVE_RECORDED` / `BACKFILLED` / `MISSING` intervals |
//! | `grade --ledger <ledger.db> [--account A…]` | trades, totals, verdicts, 10 reconciliation checks per account; exit 1 when one fails |
//! | `regrade --history <dir>… --anchor [--signal-at] --entry --exit --notional-usd N [--top-n N] [--min-abs-signal-bps X] [--direction fade\|follow] [--taker-fee-bps F \| --fees recorded] [--market-db <db>] [--instruments ids…] [--fill-book as-of\|next] [--compare-ledger <db> --compare-account A] [--expect-signals <file>]` | rule W or a variant from recorded rows (`domain/xm/regrade.rs`) |
//!
//! Every command takes `--format text|json`. Times: RFC 3339, epoch ms or a
//! UTC date (`domain::marketdata::parse_time`); printed RFC 3339 UTC with
//! ms. Ids and hashes are printed in full.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;

use crate::adapters::outbound::backfill::text_table;
use crate::adapters::outbound::evidence::ledger_reader::SqliteLedgerReader;
use crate::adapters::outbound::evidence::recorded::{DayFiles, MarketDb};
use crate::adapters::outbound::evidence::vault::FsVault;
use crate::application::evidence::{self as uc, Verdict};
use crate::config::paths::resolve_tengu_home;
use crate::domain::evidence::EvidenceRecord;
use crate::domain::evidence_coverage::{Coverage, Window};
use crate::domain::marketdata::{parse_time, Interval};
use crate::domain::xm::grade::{AccountGrade, CheckStatus};
use crate::domain::xm::regrade::{
    check_signals, compare_with_ledger, parse_signal_lines, BookPick, Direction, FeeModel,
    Instants, Limits, Regrade, RegradeRule,
};
use crate::ports::evidence::{BackfillSource, LedgerSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum Format {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum DirectionArg {
    Fade,
    Follow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum FeesArg {
    /// Each name's `taker_fee_bps` from its recorded `mkt_ctx/1` rows.
    Recorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum FillBookArg {
    /// The latest book observed at or before the instant.
    AsOf,
    /// The first book observed at or after the instant (what an order sent then meets).
    Next,
}

#[derive(Subcommand)]
pub(super) enum EvidenceAction {
    /// Copy a plan record's items into a new read-only vault and fill the record.
    Snapshot {
        /// lineage/evidence/<id>.toml (a plan: no captured_at).
        #[arg(long)]
        record: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Re-hash a captured record's vault: MATCH / MISMATCH / ABSENT / EXTRA.
    Verify {
        record: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Cadence coverage of one schema in recorder day files, gaps labelled.
    Coverage {
        /// A recorder history dir (<state dir>/history); repeat to merge recorders.
        #[arg(long = "history", required = true)]
        history: Vec<PathBuf>,
        /// Observation schema, e.g. mkt_ctx/1.
        #[arg(long)]
        schema: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        cadence_secs: u64,
        /// A market.db whose bars count as the second (backfilled) source —
        /// for price streams (mkt_ctx/1), never for books.
        #[arg(long)]
        bars: Option<PathBuf>,
        /// Bar interval of --bars.
        #[arg(long, default_value = "1m")]
        interval: String,
        /// Only these full instrument ids.
        #[arg(long, num_args = 1..)]
        instruments: Vec<String>,
        /// List every key's own gaps in text output too.
        #[arg(long)]
        per_key: bool,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Grade a paper ledger: trades, totals, verdicts, reconciliation.
    Grade {
        #[arg(long)]
        ledger: PathBuf,
        /// Accounts to grade; default every account.
        #[arg(long = "account")]
        accounts: Vec<String>,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Replay rule W (or a variant) from recorded rows.
    Regrade(RegradeArgs),
}

#[derive(Args)]
pub(super) struct RegradeArgs {
    #[arg(long = "history", required = true)]
    history: Vec<PathBuf>,
    #[arg(long)]
    anchor: String,
    #[arg(long)]
    entry: String,
    #[arg(long)]
    exit: String,
    /// Read the signal here instead of at --entry (a prereg that froze its signals at another sweep).
    #[arg(long)]
    signal_at: Option<String>,
    /// The N largest |s|; default every name.
    #[arg(long)]
    top_n: Option<usize>,
    #[arg(long, default_value_t = 0.0)]
    min_abs_signal_bps: f64,
    #[arg(long)]
    notional_usd: f64,
    #[arg(long, value_enum, default_value_t = DirectionArg::Fade)]
    direction: DirectionArg,
    /// Flat taker fee, bps per side (conflicts with --fees).
    #[arg(long, conflicts_with = "fees")]
    taker_fee_bps: Option<f64>,
    #[arg(long, value_enum)]
    fees: Option<FeesArg>,
    /// market.db: backfilled funding when a recorded rate is missing.
    #[arg(long)]
    market_db: Option<PathBuf>,
    /// Full ids; default every hl_book/1 key recorded around entry and exit.
    #[arg(long, num_args = 1..)]
    instruments: Vec<String>,
    #[arg(long, value_enum, default_value_t = FillBookArg::AsOf)]
    fill_book: FillBookArg,
    #[arg(long, default_value_t = 600)]
    anchor_max_age_secs: i64,
    #[arg(long, default_value_t = 120)]
    ctx_max_age_secs: i64,
    #[arg(long, default_value_t = 60)]
    book_max_age_secs: i64,
    #[arg(long, default_value_t = 120)]
    funding_max_age_secs: i64,
    /// Compare per leg with this ledger's trades of --compare-account.
    #[arg(long, requires = "compare_account")]
    compare_ledger: Option<PathBuf>,
    #[arg(long, requires = "compare_ledger")]
    compare_account: Option<String>,
    /// Check the replay's signals against `<id>|<anchor>|<now>|<s bps>` lines in this file.
    #[arg(long)]
    expect_signals: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

pub(super) fn run_evidence(action: EvidenceAction) -> Result<()> {
    match action {
        EvidenceAction::Snapshot { record, format } => snapshot(&record, format),
        EvidenceAction::Verify { record, format } => verify(&record, format),
        EvidenceAction::Coverage {
            history,
            schema,
            from,
            to,
            cadence_secs,
            bars,
            interval,
            instruments,
            per_key,
            format,
        } => {
            let window = Window::new(
                time(&from)?,
                time(&to)?,
                i64::try_from(cadence_secs)?.saturating_mul(1000),
            )
            .map_err(anyhow::Error::msg)?;
            let h = DayFiles::new(history);
            let set: Option<BTreeSet<String>> =
                (!instruments.is_empty()).then(|| instruments.into_iter().collect());
            let market = bars.map(MarketDb::open).transpose()?;
            let iv = Interval::parse(&interval).map_err(anyhow::Error::msg)?;
            let second = market
                .as_ref()
                .map(|m| (m as &dyn BackfillSource, iv.as_str(), iv.ms()));
            let c = uc::coverage(&h, &schema, &window, set.as_ref(), second)?;
            match format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema": schema,
                        "history": crate::ports::evidence::RecordedHistory::describe(&h),
                        "coverage": c,
                    }))?
                ),
                Format::Text => print!("{}", coverage_text(&schema, &c, per_key)),
            }
            Ok(())
        }
        EvidenceAction::Grade {
            ledger,
            accounts,
            format,
        } => {
            let reader = SqliteLedgerReader::new(ledger);
            let grades = uc::grade(&reader, &accounts)?;
            match format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ledger": reader.describe(),
                        "accounts": grades,
                    }))?
                ),
                Format::Text => {
                    println!("ledger {}", reader.describe());
                    for g in &grades {
                        print!("{}", grade_text(g));
                    }
                }
            }
            let failed: Vec<&str> = grades
                .iter()
                .filter(|g| !g.reconciled())
                .map(|g| g.account.as_str())
                .collect();
            if !failed.is_empty() {
                bail!("reconciliation FAIL: {}", failed.join(", "));
            }
            Ok(())
        }
        EvidenceAction::Regrade(args) => regrade(args),
    }
}

fn time(s: &str) -> Result<i64> {
    parse_time(s).map_err(anyhow::Error::msg)
}

fn ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| ms.to_string())
}

fn num(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "MISSING".to_string(), |x| format!("{x:.digits$}"))
}

fn read_record(path: &Path) -> Result<EvidenceRecord> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let rec: EvidenceRecord =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if stem != rec.id {
        bail!(
            "{}: the file stem `{stem}` is not the record id `{}`",
            path.display(),
            rec.id
        );
    }
    Ok(rec)
}

fn snapshot(path: &Path, format: Format) -> Result<()> {
    let plan = read_record(path)?;
    let vault = FsVault::new(&resolve_tengu_home(), &plan.vault);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let (record, manifest) = uc::snapshot(&plan, &vault, &now)?;
    let text = toml::to_string(&record)?;
    std::fs::write(path, &text).with_context(|| {
        format!(
            "write {} (the vault is sealed; the captured record follows on stdout)\n{text}",
            path.display()
        )
    })?;
    match format {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "vault": crate::ports::evidence::Vault::root_display(&vault),
                "record": record,
                "files": manifest.len(),
                "bytes": manifest.iter().map(|e| e.bytes).sum::<u64>(),
            }))?
        ),
        Format::Text => {
            println!(
                "vault {}",
                crate::ports::evidence::Vault::root_display(&vault)
            );
            println!("record {} rewritten (captured_at {now})", path.display());
            println!(
                "manifest_sha256 {}  ({} files, {} bytes)",
                record.manifest_sha256.as_deref().unwrap_or("MISSING"),
                manifest.len(),
                manifest.iter().map(|e| e.bytes).sum::<u64>()
            );
            let rows: Vec<Vec<String>> = record
                .items
                .iter()
                .map(|i| {
                    vec![
                        i.path.clone(),
                        format!("{:?}", i.kind).to_uppercase(),
                        i.files.map_or("MISSING".into(), |f| f.to_string()),
                        i.bytes.map_or("MISSING".into(), |b| b.to_string()),
                        i.sha256.clone().unwrap_or_else(|| "MISSING".into()),
                    ]
                })
                .collect();
            print!(
                "{}",
                text_table(&["path", "kind", "files", "bytes", "sha256"], &rows)
            );
        }
    }
    Ok(())
}

fn verify(path: &Path, format: Format) -> Result<()> {
    let record = read_record(path)?;
    let vault = FsVault::new(&resolve_tengu_home(), &record.vault);
    let rep = uc::verify(&record, &vault)?;
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&rep)?),
        Format::Text => {
            println!("vault {}", rep.vault);
            println!(
                "MANIFEST.json {}  expected {}  actual {}",
                format!("{:?}", rep.manifest.status).to_uppercase(),
                rep.manifest.expected.as_deref().unwrap_or("-"),
                rep.manifest.actual.as_deref().unwrap_or("ABSENT")
            );
            let rows: Vec<Vec<String>> = rep
                .items
                .iter()
                .map(|v| {
                    vec![
                        v.path.clone(),
                        format!("{:?}", v.status).to_uppercase(),
                        v.actual.clone().unwrap_or_else(|| "ABSENT".into()),
                    ]
                })
                .collect();
            print!(
                "{}",
                text_table(&["item", "status", "sha256 files bytes"], &rows)
            );
            println!("files {} MATCH of {} listed", rep.files_match, rep.files);
            for p in &rep.problems {
                println!(
                    "{} {}  expected {}  actual {}",
                    format!("{:?}", p.status).to_uppercase(),
                    p.path,
                    p.expected.as_deref().unwrap_or("-"),
                    p.actual.as_deref().unwrap_or("-")
                );
            }
        }
    }
    if !rep.all_match() {
        let n = rep.problems.len()
            + rep
                .items
                .iter()
                .filter(|i| i.status != Verdict::Match)
                .count();
        bail!("verify FAIL: {n} item(s) / file(s) not MATCH");
    }
    Ok(())
}

fn coverage_text(schema: &str, c: &Coverage, per_key: bool) -> String {
    let mut out = format!(
        "{schema}  {} → {}  cadence {} s\nslots {}  swept {}  keys {} (live {}, complete {}, never covered {})\n",
        ts(c.window.from_ms),
        ts(c.window.to_ms),
        c.window.cadence_ms / 1000,
        c.slots,
        c.swept_slots,
        c.keys,
        c.live_keys,
        c.complete_keys,
        c.never_covered.len()
    );
    if let Some(s) = &c.second_source {
        out.push_str(&format!("second source {s}\n"));
    }
    if !c.never_covered.is_empty() {
        out.push_str(&format!("never covered: {}\n", c.never_covered.join(", ")));
    }
    out.push_str("timeline:\n");
    let rows: Vec<Vec<String>> = c
        .timeline
        .iter()
        .map(|i| {
            vec![
                ts(i.from_ms),
                ts(i.to_ms),
                format!("{:?}", i.label),
                i.keys.to_string(),
                i.note.clone(),
            ]
        })
        .map(|mut r| {
            r[2] = match r[2].as_str() {
                "LiveRecorded" => "LIVE_RECORDED".into(),
                "Backfilled" => "BACKFILLED".into(),
                "Missing" => "MISSING".into(),
                other => other.into(),
            };
            r
        })
        .collect();
    out.push_str(&text_table(&["from", "to", "label", "keys", "note"], &rows));
    out.push_str(&format!("sweep gaps: {}\n", c.sweep_gaps.len()));
    for g in &c.sweep_gaps {
        out.push_str(&format!(
            "  {} → {}  {} slot(s)  backfilled keys {}  missing keys {}  bars {} (flat {})\n",
            ts(g.gap.from_ms),
            ts(g.gap.to_ms),
            g.gap.slots,
            g.backfilled_keys,
            g.missing_keys.len(),
            g.bars,
            g.flat_bars
        ));
    }
    let own: Vec<_> = c
        .per_key
        .iter()
        .filter(|k| !k.own_gaps.is_empty())
        .collect();
    let n_own: usize = own.iter().map(|k| k.own_gaps.len()).sum();
    let slots_own: usize = own
        .iter()
        .flat_map(|k| &k.own_gaps)
        .map(|g| g.gap.slots)
        .sum();
    out.push_str(&format!(
        "key gaps outside sweep gaps: {n_own} over {} keys ({slots_own} slots)\n",
        own.len()
    ));
    if per_key {
        for k in own {
            for g in &k.own_gaps {
                out.push_str(&format!(
                    "  {}  {} → {}  {} slot(s)  {:?}\n",
                    k.key,
                    ts(g.gap.from_ms),
                    ts(g.gap.to_ms),
                    g.gap.slots,
                    g.label
                ));
            }
        }
    }
    out
}

fn grade_text(g: &AccountGrade) -> String {
    let t = &g.totals;
    let mut out = format!(
        "\naccount {}  initial {}  final {}  P&L {}\norders {} {:?}  fills {}  funding rows {}\n",
        g.account,
        g.initial_cash_usd,
        num(g.final_balance_usd, 12),
        num(g.pnl_usd, 12),
        g.orders,
        g.orders_by_status,
        g.fills,
        g.funding_rows
    );
    let rows: Vec<Vec<String>> = g
        .trades
        .iter()
        .map(|tr| {
            vec![
                tr.instrument.clone(),
                tr.side.as_str().into(),
                format!("{}", tr.qty),
                ts(tr.opened_ms),
                tr.closed_ms.map_or("OPEN".into(), ts),
                format!("{}", tr.entry_vwap),
                tr.exit_vwap.map_or("MISSING".into(), |x| x.to_string()),
                format!("{:.6}", tr.entry_notional_usd),
                format!("{:.6}", tr.realized_usd),
                format!("{:.6}", tr.fees_usd),
                format!("{:.6}", -tr.funding_paid_usd),
                format!("{:.6}", tr.net_usd),
                num(tr.net_bps, 2),
            ]
        })
        .collect();
    out.push_str(&text_table(
        &[
            "instrument",
            "side",
            "qty",
            "opened",
            "closed",
            "entry px",
            "exit px",
            "entry USD",
            "realized",
            "fees",
            "funding recv",
            "net USD",
            "net bps",
        ],
        &rows,
    ));
    out.push_str(&format!(
        "trades {} (closed {}, open {})  gross {}  fees {}  funding received {}  net {}  entry notional {}\nmean net bps {}  positive {} / {}  hit rate {}\n",
        t.trades,
        t.closed,
        t.open,
        t.gross_usd,
        t.fees_usd,
        -t.funding_paid_usd,
        t.net_usd,
        t.entry_notional_usd,
        num(t.mean_net_bps, 6),
        t.positive,
        t.closed,
        num(t.hit_rate, 4)
    ));
    let verdicts: Vec<String> = g
        .verdicts
        .iter()
        .map(|v| {
            format!(
                "{} {} {} × {}",
                v.class,
                if v.allow { "allow" } else { "deny" },
                v.rule,
                v.n
            )
        })
        .collect();
    out.push_str(&format!("risk verdicts: {}\n", verdicts.join(" · ")));
    for c in &g.checks {
        out.push_str(&format!(
            "  {} {}  {}\n",
            match c.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Fail => "FAIL",
            },
            c.check,
            c.detail
        ));
    }
    out
}

fn regrade(a: RegradeArgs) -> Result<()> {
    let inst = Instants {
        anchor_ms: time(&a.anchor)?,
        entry_ms: time(&a.entry)?,
        exit_ms: time(&a.exit)?,
        signal_ms: a.signal_at.as_deref().map(time).transpose()?,
    };
    let fees = match (a.taker_fee_bps, a.fees) {
        (Some(b), None) => FeeModel::Flat { taker_bps: b },
        (None, Some(FeesArg::Recorded)) => FeeModel::Recorded,
        _ => bail!("give --taker-fee-bps F or --fees recorded"),
    };
    let rule = RegradeRule {
        direction: match a.direction {
            DirectionArg::Fade => Direction::Fade,
            DirectionArg::Follow => Direction::Follow,
        },
        top_n: a.top_n,
        min_abs_signal_bps: a.min_abs_signal_bps,
        notional_usd: a.notional_usd,
        fees,
    };
    let lim = Limits {
        anchor_max_age_ms: a.anchor_max_age_secs * 1000,
        ctx_max_age_ms: a.ctx_max_age_secs * 1000,
        book_max_age_ms: a.book_max_age_secs * 1000,
        book_pick: match a.fill_book {
            FillBookArg::AsOf => BookPick::AsOf,
            FillBookArg::Next => BookPick::Next,
        },
        funding_max_age_ms: a.funding_max_age_secs * 1000,
    };
    let h = DayFiles::new(a.history.clone());
    let market = a.market_db.clone().map(MarketDb::open).transpose()?;
    let universe = if a.instruments.is_empty() {
        uc::regrade_universe(&h, &inst)?
    } else {
        a.instruments.clone()
    };
    if universe.is_empty() {
        bail!("no instruments: none given and no hl_book/1 key recorded around entry and exit");
    }
    let r = uc::regrade(
        &h,
        market.as_ref().map(|m| m as &dyn BackfillSource),
        &universe,
        &rule,
        &inst,
        &lim,
    )?;
    let ledger = match (&a.compare_ledger, &a.compare_account) {
        (Some(path), Some(account)) => {
            let g = uc::grade(
                &SqliteLedgerReader::new(path.clone()),
                std::slice::from_ref(account),
            )?;
            g.into_iter().next().map(|g| (path.clone(), g))
        }
        _ => None,
    };
    let compare = ledger.as_ref().map(|(_, g)| compare_with_ledger(&r, g));
    let signals = match &a.expect_signals {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let lines = parse_signal_lines(&text);
            if lines.is_empty() {
                bail!("{}: no `<id>|<anchor>|<now>|<s>` line", path.display());
            }
            Some(check_signals(&r, &lines))
        }
        None => None,
    };
    match a.format {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "history": crate::ports::evidence::RecordedHistory::describe(&h),
                "market_db": market.as_ref().map(|m| m.describe()),
                "regrade": r,
                "vs_ledger": compare.as_ref().map(|c| json!({
                    "ledger": ledger.as_ref().map(|(p, g)| json!({"path": p, "account": g.account})),
                    "legs": c,
                })),
                "signal_check": signals,
            }))?
        ),
        Format::Text => {
            print!("{}", regrade_text(&r, &h, market.as_ref()));
            if let (Some(c), Some((path, g))) = (&compare, &ledger) {
                println!("\nvs ledger {} account {}:", path.display(), g.account);
                let rows: Vec<Vec<String>> = c
                    .iter()
                    .map(|l| {
                        vec![
                            l.instrument.clone(),
                            l.replay_side.map_or("-".into(), |s| s.as_str().into()),
                            l.ledger_side.map_or("-".into(), |s| s.as_str().into()),
                            num(l.replay_entry_vwap, 6),
                            num(l.ledger_entry_vwap, 6),
                            num(l.replay_exit_vwap, 6),
                            num(l.ledger_exit_vwap, 6),
                            num(l.replay_net_bps, 2),
                            num(l.ledger_net_bps, 2),
                            num(l.diff_net_bps, 2),
                            num(l.replay_funding_paid_usd.map(|x| -x), 6),
                            num(l.ledger_funding_paid_usd.map(|x| -x), 6),
                            l.note.clone(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    text_table(
                        &[
                            "instrument",
                            "side r",
                            "side l",
                            "entry r",
                            "entry l",
                            "exit r",
                            "exit l",
                            "net bps r",
                            "net bps l",
                            "diff",
                            "fund recv r",
                            "fund recv l",
                            "note"
                        ],
                        &rows
                    )
                );
                println!(
                    "ledger mean net bps {}  net USD {}",
                    num(g.totals.mean_net_bps, 6),
                    g.totals.net_usd
                );
            }
            if let Some(s) = &signals {
                println!(
                    "\nsignal check: {} of {} expected lines match; {} mismatch; not expected {}",
                    s.matched,
                    s.expected,
                    s.mismatches.len(),
                    s.not_expected.len()
                );
                for m in &s.mismatches {
                    println!("  {m}");
                }
                if !s.not_expected.is_empty() {
                    println!("  not expected: {}", s.not_expected.join(", "));
                }
            }
        }
    }
    Ok(())
}

fn regrade_text(r: &Regrade, h: &DayFiles, market: Option<&MarketDb>) -> String {
    use crate::ports::evidence::RecordedHistory;
    let s = &r.summary;
    let mut out = format!(
        "history {}\nmarket.db {}\nanchor {}  signal {}  entry {}  exit {}\nrule {:?}  top_n {}  |s| ≥ {}  ${} per name  fees {:?}  fill book {:?}\n",
        h.describe(),
        market.map_or("none".into(), |m| m.describe()),
        ts(r.instants.anchor_ms),
        ts(r.instants.signal_at()),
        ts(r.instants.entry_ms),
        ts(r.instants.exit_ms),
        r.rule.direction,
        r.rule.top_n.map_or("all".into(), |n| n.to_string()),
        r.rule.min_abs_signal_bps,
        r.rule.notional_usd,
        r.rule.fees,
        r.limits.book_pick
    );
    out.push_str(&format!(
        "names {}  signals {}  selected {}  graded {}  MISSING {}\nmean net bps {}  mean gross bps {}  net USD {:.6}  gross {:.6}  fees {:.6}  funding received {:.6}  positive {} / {}\nmean slippage vs mid: entry {} bps, exit {} bps  partial entries {}\n",
        s.names,
        s.signals,
        s.selected,
        s.graded,
        s.missing,
        num(s.mean_net_bps, 4),
        num(s.mean_gross_bps, 4),
        s.net_usd,
        s.gross_usd,
        s.fees_usd,
        -s.funding_paid_usd,
        s.positive,
        s.graded,
        num(s.mean_entry_slippage_bps, 3),
        num(s.mean_exit_slippage_bps, 3),
        s.partial_entries
    ));
    out.push_str(&format!("selected: {}\n", r.selected.join(", ")));
    let rows: Vec<Vec<String>> = r
        .legs
        .iter()
        .filter(|l| l.selected)
        .map(|l| {
            let sig = l.signal.as_ref();
            let e = l.entry.as_ref();
            let x = l.exit.as_ref();
            vec![
                l.instrument.clone(),
                num(sig.map(|s| s.s_bps), 2),
                l.side.map_or("-".into(), |s| s.as_str().into()),
                num(e.map(|e| e.vwap), 6),
                num(e.and_then(|e| e.slippage_bps_vs_mid), 2),
                e.map_or("-".into(), |e| {
                    format!("{:+.1}", e.book_distance_ms as f64 / 1000.0)
                }),
                num(x.map(|x| x.vwap), 6),
                num(x.and_then(|x| x.slippage_bps_vs_mid), 2),
                num(l.fees.as_ref().map(|f| f.usd), 6),
                l.funding
                    .as_ref()
                    .map_or("-".into(), |f| format!("{:.6} {}", -f.paid_usd, f.source)),
                num(l.net_usd, 6),
                num(l.net_bps, 2),
                num(l.mid_move_bps, 2),
                l.missing.clone().unwrap_or_default(),
            ]
        })
        .collect();
    out.push_str(&text_table(
        &[
            "instrument",
            "s bps",
            "side",
            "entry vwap",
            "slip",
            "book s",
            "exit vwap",
            "slip",
            "fees",
            "funding recv",
            "net USD",
            "net bps",
            "mid bps",
            "MISSING",
        ],
        &rows,
    ));
    let skipped: Vec<String> = r
        .legs
        .iter()
        .filter_map(|l| l.skip.as_ref().map(|s| format!("{} ({s})", l.instrument)))
        .collect();
    if !skipped.is_empty() {
        out.push_str(&format!("no signal: {}\n", skipped.join(", ")));
    }
    out
}
