//! `backtest` read mode — a stored run's rows by run id (xlab,
//! `docs/xlab-2026-10-01.md` § 8). The run dirs sit in the state dir,
//! outside every fs root (`config/xmarket.rs`): the Architect reads its runs
//! through the tool, never by path. No new run, no network, no `market.db`.
//!
//! | Arg | Rule |
//! |---|---|
//! | `run_id` | a run id as a `backtest` call printed it (`<YYYYMMDDTHHMMSSZ>-<strategy>[-N]`): `[A-Za-z0-9_-]`, ≤ [`MAX_RUN_ID`] chars, no leading `-` — a plain name inside `<state dir>/backtests/`, never a path; the dir and the files read must be real (a symlink is refused) |
//! | `view` | `periods` (default) · `instruments` · `trades` · `notes` |
//! | `arm` | `research` (default) or another arm of the run (`capped`, a gate arm) |
//! | `limit` | 1–[`MAX_LIMIT`], default [`DEFAULT_LIMIT`]: the `limit` best and the `limit` worst rows by Σ net USD (every row when they fit), fewer when the rows would pass [`TABLE_MAX_CHARS`] — said |
//! | Row format | one line per row, self-describing: `#<rank> <field>=<value> …`, `… N more` between the best and the worst (a weak model misreads the columns of a wide aligned table: live, flash-lite quoted a signal as a gross bps) |
//! | `holdout` | a run with a `split` (the operator's CLI, a counted read) shows its in-sample trades only, and its `notes` without the arms' refusals and drops (they count holdout trades: a loss limit refuses after holdout losses); `true` shows the holdout too — a holdout read, recorded (`holdout.rs`); `true` on a run without a split is an error |
//!
//! | View | Rows |
//! |---|---|
//! | `periods` · `instruments` | `trades-<arm>.jsonl`, streamed: per period / instrument key n, Σ net USD, mean net bps, hit rate, share of the shown Σ net USD (when it is > 0); with the holdout shown, how many of the row's trades are holdout |
//! | `trades` | the same file: decided at, instrument key, side, signal, entry / exit px (one leg), gross / net bps, net USD, exit reason, period; with the holdout shown, its half |
//! | `notes` | `report.json`: data notes (≤ [`MAX_NOTES`]: share splits, missing series, a hidden holdout), candidate skips by reason, refusals and drops per arm (a split run's: with the holdout read only) |

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::holdout::{self, HoldoutRead};
use super::{field, opt_str, whole};
use crate::domain::backtest::spec::{valid_name, SplitSpec};
use crate::domain::marketdata::fmt_time;
use crate::domain::tools as names;

/// `view` values, wire names.
pub(crate) const VIEWS: [&str; 4] = ["periods", "instruments", "trades", "notes"];
pub(crate) const DEFAULT_LIMIT: usize = 10;
pub(crate) const MAX_LIMIT: usize = 25;
/// A run id: a 16-char stamp, `-`, a ≤ 48-char name, a `-N` suffix.
pub(crate) const MAX_RUN_ID: usize = 80;
/// The table's budget: the whole text stays well inside a 16k local
/// model's 8 192-char result cap.
pub(crate) const TABLE_MAX_CHARS: usize = 4_000;
/// Data notes the `notes` view lists before "… and N more".
pub(crate) const MAX_NOTES: usize = 30;
/// Run ids a missing run's error names.
const RECENT_RUNS: usize = 5;

/// The table a read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Periods,
    Instruments,
    Trades,
    Notes,
}

impl View {
    fn parse(s: &str) -> Result<View> {
        Ok(match s {
            "periods" => View::Periods,
            "instruments" => View::Instruments,
            "trades" => View::Trades,
            "notes" => View::Notes,
            _ => bail!(
                "{}: 'view' `{s}` is one of {}",
                names::BACKTEST,
                VIEWS.join(", ")
            ),
        })
    }

    fn as_str(self) -> &'static str {
        VIEWS[self as usize]
    }
}

/// One read (module table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowsRequest {
    pub run: String,
    pub view: View,
    pub arm: String,
    pub limit: usize,
    pub holdout: bool,
}

/// `run_id` is one plain name (module table).
pub(crate) fn check_run_id(run: &str) -> Result<()> {
    let plain = !run.is_empty()
        && run.len() <= MAX_RUN_ID
        && !run.starts_with('-')
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if !plain {
        bail!(
            "{}: 'run_id' `{run}` is not a run id — give the id a backtest call printed (e.g. \
             20261001T171021Z-weekend_fade): a name inside the state dir's backtests/, never a \
             path",
            names::BACKTEST
        );
    }
    Ok(())
}

/// The read `o` asks for (`run.rs` routed it here: `run_id` is set and no run
/// argument is).
pub(crate) fn parse(o: &Map<String, Value>, holdout: bool) -> Result<RowsRequest> {
    let tool = names::BACKTEST;
    let run = opt_str(tool, o, "run_id")?
        .ok_or_else(|| anyhow!("{tool}: 'run_id' must be a run id"))?
        .to_string();
    check_run_id(&run)?;
    let view = match opt_str(tool, o, "view")? {
        None => View::Periods,
        Some(v) => View::parse(v)?,
    };
    let arm = opt_str(tool, o, "arm")?.unwrap_or("research").to_string();
    if !valid_name(&arm) {
        bail!("{tool}: 'arm' `{arm}` is an arm name ([a-z0-9_]): research, capped, …");
    }
    let limit = match field(o, "limit") {
        None => DEFAULT_LIMIT,
        Some(v) => whole(v)
            .filter(|n| (1..=MAX_LIMIT as i64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                anyhow!("{tool}: 'limit' must be an integer from 1 to {MAX_LIMIT}, got {v}")
            })?,
    };
    Ok(RowsRequest {
        run,
        view,
        arm,
        limit,
        holdout,
    })
}

// ── The run on disk ────────────────────────────────────────────────

/// A path inside the run that must be a real file / dir, never a symlink.
fn real(path: &Path, dir: bool) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => bail!(
            "{}: {} is a symlink — a run's files are read only as written",
            names::BACKTEST,
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().to_string()
            )
        ),
        Ok(m) => Ok(if dir { m.is_dir() } else { m.is_file() }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// The newest run ids under `backtests_dir` (names sort by their stamp).
fn recent_runs(backtests_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(backtests_dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| check_run_id(n).is_ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names.truncate(RECENT_RUNS);
    names
}

/// `<backtests dir>/<run>` — a real dir holding `report.json`.
fn run_dir(backtests_dir: &Path, run: &str) -> Result<PathBuf> {
    check_run_id(run)?;
    let dir = backtests_dir.join(run);
    if !real(&dir, true)? || !real(&dir.join("report.json"), false)? {
        let recent = recent_runs(backtests_dir);
        bail!(
            "{}: no run `{run}` in the state dir's backtests/ (the newest: {}) — give the run id a \
             backtest call printed",
            names::BACKTEST,
            if recent.is_empty() {
                "none".to_string()
            } else {
                recent.join(", ")
            }
        );
    }
    Ok(dir)
}

/// What a read takes from `report.json` (read as JSON: a field added later
/// never breaks an old run).
struct ReportHead {
    run_id: String,
    strategy: String,
    kind: String,
    interval: String,
    spec_sha256: String,
    split: Option<SplitSpec>,
    raw: Value,
}

fn read_report(dir: &Path) -> Result<ReportHead> {
    let path = dir.join("report.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("{}: report.json of the run: {e}", names::BACKTEST))?;
    let s = |k: &str| raw.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let split = match raw.get("split").and_then(Value::as_str) {
        None => None,
        Some(t) => Some(
            SplitSpec::parse(t)
                .map_err(|e| anyhow!("{}: report.json split: {e}", names::BACKTEST))?,
        ),
    };
    Ok(ReportHead {
        run_id: s("run_id"),
        strategy: s("strategy"),
        kind: s("kind"),
        interval: s("interval"),
        spec_sha256: s("spec_sha256"),
        split,
        raw,
    })
}

/// One trade as `trades-<arm>.jsonl` holds it — the fields a read shows
/// (`domain/backtest/engine.rs::Trade`; others ignored).
#[derive(Debug, Clone, Deserialize)]
struct TradeRow {
    seq: usize,
    instrument: String,
    side: String,
    #[serde(default)]
    legs: Vec<LegRow>,
    signal_bps: f64,
    decided_at_ms: i64,
    gross_bps: f64,
    net_bps: f64,
    net_usd: f64,
    period: String,
    #[serde(default)]
    exit_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LegRow {
    instrument: String,
    entry_px: f64,
    exit_px: f64,
}

/// One row of `periods` / `instruments`.
#[derive(Debug, Clone, Default)]
struct Agg {
    n: usize,
    net_usd: f64,
    sum_bps: f64,
    wins: usize,
    holdout: usize,
}

impl Agg {
    fn add(&mut self, net_usd: f64, net_bps: f64, holdout: bool) {
        self.n += 1;
        self.net_usd += net_usd;
        self.sum_bps += net_bps;
        self.wins += usize::from(net_bps > 0.0);
        self.holdout += usize::from(holdout);
    }
}

/// Per key (period / instrument) of the trades of `trades` (any order).
pub(crate) fn per_key<'a>(
    trades: impl Iterator<Item = (&'a str, f64, f64)>,
) -> Vec<(String, usize, f64, f64)> {
    let mut by: BTreeMap<&str, Agg> = BTreeMap::new();
    for (key, usd, bps) in trades {
        by.entry(key).or_default().add(usd, bps, false);
    }
    let mut rows: Vec<(String, usize, f64, f64)> = by
        .into_iter()
        .map(|(k, a)| (k.to_string(), a.n, a.net_usd, a.sum_bps / a.n as f64))
        .collect();
    rows.sort_by(|x, y| y.2.total_cmp(&x.2).then_with(|| x.0.cmp(&y.0)));
    rows
}

/// The `k` best and `k` worst of `rows` (already best first), with their
/// 1-based ranks; every row when `2k` covers them.
fn ends<T>(rows: &[T], k: usize) -> (Vec<(usize, &T)>, usize) {
    let n = rows.len();
    if n <= 2 * k {
        return (
            rows.iter().enumerate().map(|(i, r)| (i + 1, r)).collect(),
            0,
        );
    }
    let mut out: Vec<(usize, &T)> = rows[..k]
        .iter()
        .enumerate()
        .map(|(i, r)| (i + 1, r))
        .collect();
    out.extend(
        rows[n - k..]
            .iter()
            .enumerate()
            .map(|(i, r)| (n - k + i + 1, r)),
    );
    (out, n - 2 * k)
}

/// One row, self-describing: `#<rank> <field>=<value> …` (`cells[0]` is the
/// rank) — a weak model misreads the columns of a wide aligned table.
fn kv_row(fields: &[&str], cells: &[String]) -> String {
    let mut out = format!("#{}", cells.first().map_or("", String::as_str));
    for (f, c) in fields.iter().zip(cells).skip(1) {
        out.push(' ');
        out.push_str(f);
        out.push('=');
        out.push_str(c);
    }
    out
}

/// The rows of `shown` (rank, row) with a `… N more` line after the best
/// `k` when `cut` rows are left out.
fn with_gap<T>(
    fields: &[&str],
    shown: &[(usize, T)],
    k: usize,
    cut: usize,
    cells: impl Fn(usize, &T) -> Vec<String>,
) -> Vec<String> {
    let mut body = Vec::new();
    for (i, (rank, row)) in shown.iter().enumerate() {
        body.push(kv_row(fields, &cells(*rank, row)));
        if cut > 0 && i + 1 == k {
            body.push(format!("… {cut} more"));
        }
    }
    body
}

/// A price cell: as stored when short, else 8 significant digits.
fn px(x: f64) -> String {
    let s = x.to_string();
    if s.len() <= 10 || !x.is_finite() || x == 0.0 {
        return s;
    }
    let mag = x.abs().log10().floor() as i32;
    let decimals = (7 - mag).clamp(0, 20) as usize;
    let r = format!("{x:.decimals$}");
    if r.contains('.') {
        r.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        r
    }
}

/// The rows of `body(k)` (the `k` best and `k` worst), `k` lowered from
/// `limit` until they fit [`TABLE_MAX_CHARS`]; returns them and the `k` used.
fn fitted_rows(limit: usize, body: impl Fn(usize) -> Vec<String>) -> (String, usize) {
    let mut k = limit.max(1);
    loop {
        let text = body(k).join("\n");
        if text.chars().count() <= TABLE_MAX_CHARS || k == 1 {
            return (text, k);
        }
        k -= 1;
    }
}

fn signed(x: f64, decimals: usize) -> String {
    format!("{x:+.decimals$}")
}

/// What a read of `trades-<arm>.jsonl` gathers, streamed.
#[derive(Default)]
struct Gathered {
    /// Trades shown (the holdout's only with `holdout`).
    shown: usize,
    /// Holdout trades left out (hidden) or shown (read).
    holdout: usize,
    net_usd: f64,
    periods: BTreeMap<String, Agg>,
    instruments: BTreeMap<String, Agg>,
    /// The `limit` best / worst shown trades by net USD (+ holdout flag).
    best: Vec<(TradeRow, bool)>,
    worst: Vec<(TradeRow, bool)>,
}

/// Net USD descending, then decision order: the trades view's ranking.
fn trade_order(a: &TradeRow, b: &TradeRow) -> std::cmp::Ordering {
    b.net_usd
        .total_cmp(&a.net_usd)
        .then(a.decided_at_ms.cmp(&b.decided_at_ms))
        .then(a.seq.cmp(&b.seq))
}

impl Gathered {
    fn keep(&mut self, t: TradeRow, holdout: bool, limit: usize) {
        let cut = |list: &mut Vec<(TradeRow, bool)>, best: bool| {
            let at = list.partition_point(|(x, _)| {
                let o = trade_order(x, &t);
                if best {
                    o.is_lt()
                } else {
                    o.is_gt()
                }
            });
            if at < limit {
                list.insert(at, (t.clone(), holdout));
                list.truncate(limit);
            }
        };
        cut(&mut self.best, true);
        cut(&mut self.worst, false);
    }
}

fn gather(
    path: &Path,
    split: Option<&SplitSpec>,
    show_holdout: bool,
    limit: usize,
) -> Result<Gathered> {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().to_string());
    let file = File::open(path).with_context(|| format!("open {name}"))?;
    let mut g = Gathered::default();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("read {name}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let t: TradeRow = serde_json::from_str(&line)
            .map_err(|e| anyhow!("{}: {name} line {}: {e}", names::BACKTEST, i + 1))?;
        let on_holdout = split.is_some_and(|s| {
            let legs: Vec<&str> = if t.legs.is_empty() {
                vec![t.instrument.as_str()]
            } else {
                t.legs.iter().map(|l| l.instrument.as_str()).collect()
            };
            s.is_holdout(t.decided_at_ms, &legs)
        });
        g.holdout += usize::from(on_holdout);
        if on_holdout && !show_holdout {
            continue;
        }
        g.shown += 1;
        g.net_usd += t.net_usd;
        g.periods
            .entry(t.period.clone())
            .or_default()
            .add(t.net_usd, t.net_bps, on_holdout);
        g.instruments
            .entry(t.instrument.clone())
            .or_default()
            .add(t.net_usd, t.net_bps, on_holdout);
        g.keep(t, on_holdout, limit);
    }
    Ok(g)
}

/// A read's text, recording a holdout read first (`holdout.rs`): `now_ms`
/// stamps the ledger line, `call_id` names the call.
pub(crate) fn read(
    backtests_dir: &Path,
    req: &RowsRequest,
    call_id: Option<&str>,
    now_ms: i64,
) -> Result<String> {
    let tool = names::BACKTEST;
    let dir = run_dir(backtests_dir, &req.run)?;
    let head = read_report(&dir)?;
    if req.holdout && head.split.is_none() {
        bail!(
            "{tool}: \"holdout\": true — run {} has no split: every row is shown without it",
            req.run
        );
    }
    let title = format!(
        "backtest rows {} view={} · {} {} {}",
        req.run,
        req.view.as_str(),
        head.strategy,
        head.kind,
        head.interval
    );
    if req.view == View::Notes {
        let read = match &head.split {
            Some(s) if req.holdout => Some(record_read(backtests_dir, &head, s, call_id, now_ms)?),
            _ => None,
        };
        return Ok(notes(&title, &head, read));
    }
    let arms: Vec<String> = head
        .raw
        .get("arms")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if !arms.contains(&req.arm) {
        bail!(
            "{tool}: run {} has no arm `{}` (it has: {})",
            req.run,
            req.arm,
            arms.join(", ")
        );
    }
    let trades = dir.join(format!("trades-{}.jsonl", req.arm));
    if !real(&trades, false)? {
        bail!(
            "{tool}: run {} has no trades file for arm `{}`",
            req.run,
            req.arm
        );
    }
    let split = head.split.as_ref();
    let g = gather(&trades, split, req.holdout, req.limit)?;
    let mut lines = vec![format!(
        "{title} arm={} · {} trade(s) · Σ net_usd {}",
        req.arm,
        g.shown,
        signed(g.net_usd, 2)
    )];
    if let Some(s) = split {
        if req.holdout {
            let count = record_read(backtests_dir, &head, s, call_id, now_ms)?;
            lines.push(format!(
                "split {}: both halves shown ({} holdout trade(s)) · {}",
                holdout::split_label(s),
                g.holdout,
                holdout::read_line(count, s)
            ));
        } else {
            lines.push(format!(
                "split {}: holdout hidden — its {} trade(s) left out, the rows are the in-sample \
                 half; \"holdout\": true shows them (a holdout read, counted per spec)",
                holdout::split_label(s),
                g.holdout
            ));
        }
    }
    let with_half = split.is_some() && req.holdout;
    let share = |usd: f64| {
        if g.net_usd > 0.0 {
            format!("{:+.3}", usd / g.net_usd)
        } else {
            "—".to_string()
        }
    };
    let (table, k, total, what) = match req.view {
        View::Periods | View::Instruments => {
            let (map, key, what) = if req.view == View::Periods {
                (&g.periods, "period", "period(s)")
            } else {
                (&g.instruments, "instrument", "instrument(s)")
            };
            let mut rows: Vec<(&String, &Agg)> = map.iter().collect();
            rows.sort_by(|x, y| {
                y.1.net_usd
                    .total_cmp(&x.1.net_usd)
                    .then_with(|| x.0.cmp(y.0))
            });
            let mut header = vec!["rank", key, "n", "net_usd", "mean_net_bps", "hit", "share"];
            if with_half {
                header.push("holdout_n");
            }
            let cells = |rank: usize, (key, a): &(&String, &Agg)| {
                let mut c = vec![
                    rank.to_string(),
                    key.to_string(),
                    a.n.to_string(),
                    signed(a.net_usd, 2),
                    signed(a.sum_bps / a.n as f64, 2),
                    format!("{:.2}", a.wins as f64 / a.n as f64),
                    share(a.net_usd),
                ];
                if with_half {
                    c.push(a.holdout.to_string());
                }
                c
            };
            let (table, k) = fitted_rows(req.limit, |k| {
                let (shown, cut) = ends(&rows, k);
                let shown: Vec<(usize, &(&String, &Agg))> = shown;
                with_gap(&header, &shown, k, cut, |rank, r| cells(rank, r))
            });
            (table, k, rows.len(), what)
        }
        View::Trades => {
            let mut header = vec![
                "rank",
                "decided_at",
                "instrument",
                "side",
                "signal_bps",
                "entry_px",
                "exit_px",
                "gross_bps",
                "net_bps",
                "net_usd",
                "exit",
                "period",
            ];
            if with_half {
                header.push("half");
            }
            let cells = |rank: usize, (t, h): &&(TradeRow, bool)| {
                let one = (t.legs.len() == 1).then(|| &t.legs[0]);
                let mut c = vec![
                    rank.to_string(),
                    fmt_time(t.decided_at_ms),
                    t.instrument.clone(),
                    t.side.clone(),
                    signed(t.signal_bps, 1),
                    one.map_or_else(|| "pair".to_string(), |l| px(l.entry_px)),
                    one.map_or_else(|| "pair".to_string(), |l| px(l.exit_px)),
                    signed(t.gross_bps, 2),
                    signed(t.net_bps, 2),
                    signed(t.net_usd, 2),
                    t.exit_reason.clone(),
                    t.period.clone(),
                ];
                if with_half {
                    c.push(if *h { "holdout" } else { "in-sample" }.to_string());
                }
                c
            };
            let n = g.shown;
            let (table, k) = fitted_rows(req.limit, |k| {
                // `best` / `worst` hold the `limit` ends (worst first in
                // `worst`): when 2k covers every trade, their union is all.
                let best = &g.best[..k.min(g.best.len())];
                let worst = &g.worst[..k.min(g.worst.len())];
                if n <= 2 * k {
                    let mut all: Vec<&(TradeRow, bool)> = best.iter().collect();
                    all.extend(
                        worst
                            .iter()
                            .filter(|(w, _)| !best.iter().any(|(b, _)| b.seq == w.seq)),
                    );
                    all.sort_by(|a, b| trade_order(&a.0, &b.0));
                    let shown: Vec<(usize, &(TradeRow, bool))> = all
                        .into_iter()
                        .enumerate()
                        .map(|(i, r)| (i + 1, r))
                        .collect();
                    return with_gap(&header, &shown, k, 0, |rank, r| cells(rank, r));
                }
                let mut shown: Vec<(usize, &(TradeRow, bool))> =
                    best.iter().enumerate().map(|(i, r)| (i + 1, r)).collect();
                // The worst last: rank n is worst[0].
                shown.extend(worst.iter().enumerate().rev().map(|(j, r)| (n - j, r)));
                with_gap(&header, &shown, k, n - 2 * k, |rank, r| cells(rank, r))
            });
            (table, k, n, "trade(s)")
        }
        View::Notes => unreachable!("handled above"),
    };
    let order = if total <= 2 * k {
        format!("all {total} {what} by Σ net USD, best first")
    } else {
        format!("the {k} best and {k} worst of {total} {what} by Σ net USD")
    };
    let fewer = if k < req.limit && total > 2 * k {
        format!(" (limit {} asked; {k} fit the text)", req.limit)
    } else {
        String::new()
    };
    lines.push(format!("{order}{fewer}:"));
    lines.push(table);
    Ok(lines.join("\n"))
}

/// Record a holdout read of the stored run `head` (`via = "rows"`); an
/// unrecordable read is refused, never shown.
fn record_read(
    backtests_dir: &Path,
    head: &ReportHead,
    split: &SplitSpec,
    call_id: Option<&str>,
    now_ms: i64,
) -> Result<holdout::ReadCount> {
    let read = HoldoutRead::new(
        "rows",
        &head.run_id,
        &head.spec_sha256,
        &head.strategy,
        split,
        call_id,
        now_ms,
    );
    holdout::record(backtests_dir, &read).map_err(|e| {
        anyhow!(
            "{}: the holdout read could not be recorded ({e:#}) — not shown",
            names::BACKTEST
        )
    })
}

/// The `notes` view (module table): a split run's per-arm refusals and
/// drops count its holdout trades too (a loss limit refuses after holdout
/// losses), so they show only with the holdout read (`read`).
fn notes(title: &str, head: &ReportHead, read: Option<holdout::ReadCount>) -> String {
    let r = &head.raw;
    let mut lines = vec![title.to_string()];
    let counts = |v: Option<&Value>| -> String {
        v.and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let arms_hidden = head.split.is_some() && read.is_none();
    if let Some(s) = &head.split {
        lines.push(match read {
            Some(count) => format!(
                "split {}: both halves · {}",
                holdout::split_label(s),
                holdout::read_line(count, s)
            ),
            None => format!(
                "split {}: holdout hidden — the arms' refusals and drops count holdout trades \
                 too, so they are left out; \"holdout\": true shows them (a holdout read, \
                 counted per spec)",
                holdout::split_label(s)
            ),
        });
    }
    let notes: Vec<&str> = r
        .get("data_notes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    lines.push(format!("data notes ({}):", notes.len()));
    let mut used = 0;
    let mut listed = 0;
    for n in &notes {
        if listed == MAX_NOTES || used + n.len() > TABLE_MAX_CHARS {
            break;
        }
        lines.push(format!("- {n}"));
        used += n.len();
        listed += 1;
    }
    if listed < notes.len() {
        lines.push(format!("… and {} more", notes.len() - listed));
    }
    let skipped = counts(r.get("skipped"));
    if !skipped.is_empty() {
        lines.push(format!("candidates skipped: {skipped}"));
    }
    if let Some(arms) = r
        .get("arms")
        .and_then(Value::as_object)
        .filter(|_| !arms_hidden)
    {
        for (name, a) in arms {
            let refused = counts(a.get("refusals"));
            let dropped = counts(a.get("skipped"));
            if !refused.is_empty() || !dropped.is_empty() {
                lines.push(format!(
                    "{name}: refused {} · dropped {}",
                    if refused.is_empty() { "—" } else { &refused },
                    if dropped.is_empty() { "—" } else { &dropped }
                ));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_ids_are_plain_names() {
        for ok in [
            "20261001T171021Z-weekend_fade",
            "20261001T171021Z-weekend_fade-2",
            "x",
        ] {
            assert!(check_run_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "..",
            "../conf/market.db",
            "a/b",
            "/etc",
            "decision-cache.db",
            "holdout-reads.jsonl",
            "-rf",
            "a b",
            "a\\b",
            &"x".repeat(MAX_RUN_ID + 1),
        ] {
            let e = check_run_id(bad).unwrap_err().to_string();
            assert!(e.contains("is not a run id"), "{bad}: {e}");
        }
    }

    #[test]
    fn arguments_parse_strictly() {
        let o = |v: Value| v.as_object().unwrap().clone();
        let r = parse(&o(json!({"run_id": "r1"})), false).unwrap();
        assert_eq!(
            (r.view, r.arm.as_str(), r.limit, r.holdout),
            (View::Periods, "research", DEFAULT_LIMIT, false)
        );
        let r = parse(
            &o(json!({"run_id": "r1", "view": "trades", "arm": "capped", "limit": 25.0})),
            true,
        )
        .unwrap();
        assert_eq!(
            (r.view, r.arm.as_str(), r.limit),
            (View::Trades, "capped", 25)
        );
        for (v, holdout, needle) in [
            (
                json!({"run_id": "r1", "view": "days"}),
                false,
                "'view' `days` is one of",
            ),
            (
                json!({"run_id": "r1", "arm": "../x"}),
                false,
                "'arm' `../x` is an arm name",
            ),
            (json!({"run_id": "r1", "limit": 0}), false, "from 1 to 25"),
            (json!({"run_id": "r1", "limit": 26}), false, "from 1 to 25"),
            (json!({"run_id": "../r1"}), true, "is not a run id"),
        ] {
            let e = parse(&o(v.clone()), holdout).unwrap_err().to_string();
            assert!(e.starts_with("backtest: "), "{e}");
            assert!(e.contains(needle), "{v}: {e}");
        }
    }

    /// A split run's `notes` leave the arms' refusals and drops out (a loss
    /// limit refuses after holdout losses) until the holdout is read, and
    /// that read is counted.
    #[test]
    fn a_split_runs_notes_hide_the_arms_until_the_holdout_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backtests");
        let run = "20261001T120000Z-w";
        std::fs::create_dir_all(dir.join(run)).unwrap();
        let report = json!({"run_id": run, "strategy": "w", "kind": "weekend_window",
            "interval": "1h", "spec_sha256": "ab".repeat(32),
            "split": "time:2026-09-15T00:00:00Z", "data_notes": ["note a"],
            "skipped": {"missing_anchor": 2},
            "arms": {"capped": {"refusals": {"daily_loss_limit_usd": 3}},
                     "research": {"skipped": {"missing_exit": 1}}}});
        std::fs::write(dir.join(run).join("report.json"), report.to_string()).unwrap();
        let req = |holdout| RowsRequest {
            run: run.into(),
            view: View::Notes,
            arm: "research".into(),
            limit: DEFAULT_LIMIT,
            holdout,
        };
        let hidden = read(&dir, &req(false), None, 0).unwrap();
        assert!(
            hidden.contains(
                "\nsplit time:2026-09-15T00:00:00Z: holdout hidden — the arms' refusals and drops \
                 count holdout trades too"
            ),
            "{hidden}"
        );
        assert!(
            hidden.contains("\n- note a\n")
                && hidden.ends_with("candidates skipped: missing_anchor 2"),
            "{hidden}"
        );
        assert!(!hidden.contains("daily_loss_limit_usd"), "{hidden}");
        assert!(!holdout::ledger_path(&dir).exists());
        let shown = read(&dir, &req(true), Some("chat:1"), 0).unwrap();
        for want in [
            "\nsplit time:2026-09-15T00:00:00Z: both halves · holdout read #1 for this spec",
            "\ncapped: refused daily_loss_limit_usd 3 · dropped —",
            "\nresearch: refused — · dropped missing_exit 1",
        ] {
            assert!(shown.contains(want), "{want}: {shown}");
        }
        let ledger = std::fs::read_to_string(holdout::ledger_path(&dir)).unwrap();
        let line: HoldoutRead = serde_json::from_str(ledger.trim()).unwrap();
        assert_eq!(
            (
                line.via.as_str(),
                line.run_id.as_str(),
                line.call_id.as_deref()
            ),
            ("rows", run, Some("chat:1"))
        );
    }

    #[test]
    fn the_best_and_worst_ends_keep_their_ranks() {
        let rows: Vec<i32> = (0..10).collect();
        let (shown, cut) = ends(&rows, 3);
        let ranks: Vec<usize> = shown.iter().map(|(r, _)| *r).collect();
        assert_eq!((ranks, cut), (vec![1, 2, 3, 8, 9, 10], 4));
        let (shown, cut) = ends(&rows, 5);
        assert_eq!((shown.len(), cut), (10, 0));
        assert_eq!(px(372.33), "372.33");
        assert_eq!(px(121.713_333_333_333_33), "121.71333");
    }
}
