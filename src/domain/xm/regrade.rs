//! Book replay (`tengu evidence regrade`, `docs/lineage-2026-10-06.md` § 3):
//! rule W — or a variant — re-graded from recorded rows: signals from
//! `mkt_ctx/1` prices ([`ctx_price`], [`signal_of`]), the selection
//! ([`select_capped`]), fills by a depth walk of the recorded `hl_book/1`
//! books (`domain/book.rs`), fees, funding. Pure: the rows are inputs
//! (`application/evidence.rs` reads them, read-only). The independent check
//! of the paper fills and the grader of rules nobody traded (a prereg).
//!
//! | Piece | Rule |
//! |---|---|
//! | Price at T ([`pick_ctx`]) | the latest `mkt_ctx/1` row observed ≤ T, at most its max age old (anchor: `anchor_max_age`, entry / exit: `ctx_max_age`); a row that is not `ok` / `partial` or has no `mid` / `mark` > 0 ⇒ MISSING with the reason (never 0) — as the live rule (`weekend_fade::price_point`) |
//! | Signal | s = ln(P_entry / P_anchor) bps ([`signal_of`]); s = 0 ⇒ `flat` |
//! | Selection | `top_n`: [`select_capped`] (largest \|s\| ≥ `min_abs_signal_bps`, ties by id); else every signal with \|s\| ≥ `min_abs_signal_bps` |
//! | Side | fade = −sign(s), follow = sign(s) |
//! | Book at T ([`pick_book`]) | `as_of`: the latest `hl_book/1` row observed ≤ T; `next`: the first observed ≥ T (the book an order sent at T meets — the paper engine's); within `book_max_age` either way; `ok` / `partial` with a valid book, else MISSING |
//! | Entry | walk `Notional(notional_usd)` on the side; depth that runs out fills less (`entry.unfilled`) |
//! | Exit | walk `Qty(entry qty)` on the other side at the exit book; not all of it ⇒ MISSING |
//! | Fees | `flat`: `taker_bps` per side on each side's filled notional · `recorded`: the `taker_fee_bps` of the price row at entry / exit |
//! | Funding | every hour boundary h with entry < h ≤ exit: `recorded` = the latest `mkt_ctx/1` row ≤ h within `funding_max_age` (`funding_1h`, `oracle`); else `backfilled` = the `market.db` rate at [h, h + 1 min) × the close of the 1 m bar ending at h; else MISSING; payment = signed qty × px × rate, positive = paid (`domain/xm/ledger.rs`); the leg says `recorded`, `backfilled` or `mixed` |
//! | P&L | gross = signed qty × (exit VWAP − entry VWAP); net = gross − fees − funding paid; bps of the filled entry notional; `mid_move_bps` = side × (exit mid / entry mid − 1) |
//! | Summary | over graded legs: n, mean net bps, Σ USD; MISSING legs listed with the reason, never counted as 0 |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::book::{L2Book, Side, Walk, WalkTarget};
use crate::domain::observation::{Features, ObsStatus};
use crate::domain::xm::grade::AccountGrade;
use crate::domain::xm::weekend_fade::{
    ctx_price, select_capped, signal_of, FadeRule, Signal, Skip,
};

/// Funding is paid on the hour (HL).
const HOUR_MS: i64 = 3_600_000;
/// A backfilled funding row's stamp sits within this of its hour.
const FUNDING_STAMP_SLACK_MS: i64 = 60_000;
/// The backfilled price bar (1 m).
const BAR_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Fade,
    Follow,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FeeModel {
    /// The same taker fee on every name, bps per side.
    Flat { taker_bps: f64 },
    /// Each name's `taker_fee_bps` as recorded in its `mkt_ctx/1` rows.
    Recorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookPick {
    AsOf,
    Next,
}

/// What is replayed (module table).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RegradeRule {
    pub direction: Direction,
    /// `None` = every selected signal.
    pub top_n: Option<usize>,
    pub min_abs_signal_bps: f64,
    pub notional_usd: f64,
    pub fees: FeeModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instants {
    pub anchor_ms: i64,
    pub entry_ms: i64,
    pub exit_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub anchor_max_age_ms: i64,
    pub ctx_max_age_ms: i64,
    pub book_max_age_ms: i64,
    pub book_pick: BookPick,
    pub funding_max_age_ms: i64,
}

/// One recorded `mkt_ctx/1` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CtxSample {
    pub observed_at_ms: i64,
    pub status: ObsStatus,
    pub features: Features,
}

/// One recorded `hl_book/1` row: its book, or why there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookSample {
    pub observed_at_ms: i64,
    pub status: ObsStatus,
    pub book: Result<L2Book, String>,
}

/// One backfilled funding rate (`market.db` `funding`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingRate {
    pub t_ms: i64,
    pub rate_1h: f64,
}

/// One backfilled bar's close (`market.db` `bars`, 1 m).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BarClose {
    pub t_open_ms: i64,
    pub close: f64,
}

/// Everything read for one name, each list sorted by time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LegData {
    pub instrument: String,
    pub ctx: Vec<CtxSample>,
    pub books: Vec<BookSample>,
    pub funding: Vec<FundingRate>,
    pub bars: Vec<BarClose>,
}

/// The latest `mkt_ctx/1` row at or before `t_ms` within `max_age_ms`
/// (module table).
pub fn pick_ctx(samples: &[CtxSample], t_ms: i64, max_age_ms: i64) -> Result<&CtxSample, String> {
    let row = samples
        .iter()
        .rev()
        .find(|s| s.observed_at_ms <= t_ms)
        .ok_or_else(|| "no mkt_ctx/1 row at or before".to_string())?;
    let age = t_ms - row.observed_at_ms;
    if age > max_age_ms {
        return Err(format!(
            "stale: the last mkt_ctx/1 row is {age} ms old > {max_age_ms} ms"
        ));
    }
    if !matches!(row.status, ObsStatus::Ok | ObsStatus::Partial) {
        return Err(format!("mkt_ctx/1 row status {}", row.status.as_str()));
    }
    Ok(row)
}

fn feature(row: &CtxSample, name: &str) -> Option<f64> {
    row.features
        .get(name)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
}

/// The `hl_book/1` row a fill at `t_ms` uses (module table) and its signed
/// distance from `t_ms` (`as_of`: age ≥ 0; `next`: lateness ≥ 0).
pub fn pick_book(
    samples: &[BookSample],
    t_ms: i64,
    max_age_ms: i64,
    pick: BookPick,
) -> Result<(&BookSample, &L2Book, i64), String> {
    let row = match pick {
        BookPick::AsOf => samples.iter().rev().find(|s| s.observed_at_ms <= t_ms),
        BookPick::Next => samples.iter().find(|s| s.observed_at_ms >= t_ms),
    }
    .ok_or_else(|| match pick {
        BookPick::AsOf => "no hl_book/1 row at or before".to_string(),
        BookPick::Next => "no hl_book/1 row at or after".to_string(),
    })?;
    let dist = (row.observed_at_ms - t_ms).abs();
    if dist > max_age_ms {
        return Err(format!(
            "stale: the nearest hl_book/1 row is {dist} ms from the instant > {max_age_ms} ms"
        ));
    }
    if !matches!(row.status, ObsStatus::Ok | ObsStatus::Partial) {
        let why = row.book.as_ref().err().cloned().unwrap_or_default();
        return Err(format!(
            "hl_book/1 row status {} {why}",
            row.status.as_str()
        ));
    }
    match &row.book {
        Ok(b) => match b.validate() {
            Ok(()) => Ok((row, b, dist)),
            Err(e) => Err(format!("hl_book/1 book invalid: {e}")),
        },
        Err(e) => Err(format!("hl_book/1 row has no book: {e}")),
    }
}

/// One side of a replayed trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    pub side: Side,
    pub qty: f64,
    pub vwap: f64,
    pub notional_usd: f64,
    pub mid: Option<f64>,
    pub slippage_bps_vs_mid: Option<f64>,
    pub levels_used: usize,
    /// Target left over (USD for the entry, qty for the exit).
    pub unfilled: f64,
    pub book_observed_at_ms: i64,
    pub book_venue_ts_ms: i64,
    /// Distance of the book from the instant, ms (module table).
    pub book_distance_ms: i64,
}

fn exec_of(walk: &Walk, row: &BookSample, book: &L2Book, dist: i64) -> Option<Exec> {
    Some(Exec {
        side: walk.side,
        qty: walk.filled_qty,
        vwap: walk.vwap?,
        notional_usd: walk.filled_notional,
        mid: book.mid(),
        slippage_bps_vs_mid: walk.slippage_bps_vs_mid,
        levels_used: walk.levels_used,
        unfilled: walk.unfilled,
        book_observed_at_ms: row.observed_at_ms,
        book_venue_ts_ms: book.venue_ts_ms,
        book_distance_ms: dist,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegSignal {
    pub anchor_px: f64,
    pub anchor_at_ms: i64,
    pub entry_px: f64,
    pub entry_at_ms: i64,
    pub s_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegFees {
    pub entry_bps: f64,
    pub exit_bps: f64,
    pub usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegFunding {
    pub hours: usize,
    pub recorded_hours: usize,
    pub backfilled_hours: usize,
    /// `recorded` · `backfilled` · `mixed`.
    pub source: String,
    /// Positive = paid.
    pub paid_usd: f64,
}

/// One name of the replay (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub instrument: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<LegSignal>,
    /// Why there is no signal (`missing_anchor: …`, `flat`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Exec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fees: Option<LegFees>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<LegFunding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gross_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gross_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_move_bps: Option<f64>,
    /// A selected leg that cannot be graded, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub names: usize,
    pub signals: usize,
    pub selected: usize,
    pub graded: usize,
    pub missing: usize,
    pub mean_net_bps: Option<f64>,
    pub mean_gross_bps: Option<f64>,
    pub net_usd: f64,
    pub gross_usd: f64,
    pub fees_usd: f64,
    pub funding_paid_usd: f64,
    pub positive: usize,
    pub mean_entry_slippage_bps: Option<f64>,
    pub mean_exit_slippage_bps: Option<f64>,
    /// Graded legs whose entry book ran out before the notional.
    pub partial_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regrade {
    pub rule: RegradeRule,
    pub instants: Instants,
    pub limits: Limits,
    /// Selected names in selection order.
    pub selected: Vec<String>,
    pub legs: Vec<Leg>,
    pub summary: Summary,
}

fn skip_name(skip: Skip) -> &'static str {
    match skip {
        Skip::Excluded => "excluded",
        Skip::MissingAnchor => "missing_anchor",
        Skip::MissingEntry => "missing_entry",
        Skip::Stale => "stale",
        Skip::MissingExit => "missing_exit",
        Skip::Flat => "flat",
    }
}

fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

/// The funding of a position of signed `qty` held over (entry, exit]
/// (module table); `Err` names the first hour with neither source.
fn funding_of(
    data: &LegData,
    qty: f64,
    inst: &Instants,
    max_age_ms: i64,
) -> Result<LegFunding, String> {
    let mut h = (inst.entry_ms.div_euclid(HOUR_MS) + 1) * HOUR_MS;
    let (mut hours, mut rec, mut back, mut paid) = (0, 0, 0, 0.0);
    while h <= inst.exit_ms {
        hours += 1;
        let recorded = pick_ctx(&data.ctx, h, max_age_ms).ok().and_then(|row| {
            Some((
                feature(row, "funding_1h")?,
                feature(row, "oracle").filter(|p| *p > 0.0)?,
            ))
        });
        let (rate, px) = match recorded {
            Some(rp) => {
                rec += 1;
                rp
            }
            None => {
                let rate = data
                    .funding
                    .iter()
                    .find(|f| f.t_ms >= h && f.t_ms < h + FUNDING_STAMP_SLACK_MS)
                    .map(|f| f.rate_1h);
                let px = data
                    .bars
                    .iter()
                    .find(|b| b.t_open_ms == h - BAR_MS)
                    .map(|b| b.close)
                    .filter(|p| p.is_finite() && *p > 0.0);
                match (rate, px) {
                    (Some(r), Some(p)) => {
                        back += 1;
                        (r, p)
                    }
                    _ => {
                        return Err(format!(
                            "funding hour {h}: no recorded mkt_ctx/1 rate within {max_age_ms} ms and no backfilled rate + 1 m close"
                        ))
                    }
                }
            }
        };
        paid += qty * px * rate;
        h += HOUR_MS;
    }
    let source = match (rec, back) {
        (_, 0) => "recorded",
        (0, _) => "backfilled",
        _ => "mixed",
    };
    Ok(LegFunding {
        hours,
        recorded_hours: rec,
        backfilled_hours: back,
        source: source.to_string(),
        paid_usd: paid,
    })
}

fn fee_bps(rule: &RegradeRule, row: Option<&CtxSample>) -> Result<f64, String> {
    match rule.fees {
        FeeModel::Flat { taker_bps } => Ok(taker_bps),
        FeeModel::Recorded => row
            .and_then(|r| feature(r, "taker_fee_bps"))
            .ok_or_else(|| "no recorded taker_fee_bps".to_string()),
    }
}

/// Grade one selected leg from its side on; fills `leg` in place.
fn grade_leg(
    leg: &mut Leg,
    data: &LegData,
    side: Side,
    rule: &RegradeRule,
    inst: &Instants,
    lim: &Limits,
) {
    let fail = |leg: &mut Leg, why: String| leg.missing = Some(why);
    let entry = match pick_book(
        &data.books,
        inst.entry_ms,
        lim.book_max_age_ms,
        lim.book_pick,
    ) {
        Ok((row, book, dist)) => {
            match book.walk(side, WalkTarget::Notional(rule.notional_usd), None) {
                Ok(w) => exec_of(&w, row, book, dist),
                Err(e) => return fail(leg, format!("entry walk: {e}")),
            }
        }
        Err(e) => return fail(leg, format!("entry book: {e}")),
    };
    let Some(entry) = entry else {
        return fail(leg, "entry book: the side it takes is empty".into());
    };
    let exit = match pick_book(
        &data.books,
        inst.exit_ms,
        lim.book_max_age_ms,
        lim.book_pick,
    ) {
        Ok((row, book, dist)) => match book.walk(side.opposite(), WalkTarget::Qty(entry.qty), None)
        {
            Ok(w) if w.is_complete() => exec_of(&w, row, book, dist),
            Ok(w) => {
                leg.entry = Some(entry);
                return fail(
                    leg,
                    format!(
                        "exit book depth: {} of {} filled",
                        w.filled_qty,
                        w.filled_qty + w.unfilled
                    ),
                );
            }
            Err(e) => return fail(leg, format!("exit walk: {e}")),
        },
        Err(e) => {
            leg.entry = Some(entry);
            return fail(leg, format!("exit book: {e}"));
        }
    };
    let Some(exit) = exit else {
        leg.entry = Some(entry);
        return fail(leg, "exit book: the side it takes is empty".into());
    };
    let entry_row = pick_ctx(&data.ctx, inst.entry_ms, lim.ctx_max_age_ms).ok();
    let exit_row = pick_ctx(&data.ctx, inst.exit_ms, lim.ctx_max_age_ms).ok();
    let fees = match (fee_bps(rule, entry_row), fee_bps(rule, exit_row)) {
        (Ok(a), Ok(b)) => LegFees {
            entry_bps: a,
            exit_bps: b,
            usd: entry.notional_usd * a / 1e4 + exit.notional_usd * b / 1e4,
        },
        (Err(e), _) => {
            leg.entry = Some(entry);
            leg.exit = Some(exit);
            return fail(leg, format!("entry fee: {e}"));
        }
        (_, Err(e)) => {
            leg.entry = Some(entry);
            leg.exit = Some(exit);
            return fail(leg, format!("exit fee: {e}"));
        }
    };
    let qty = side.sign() * entry.qty;
    let funding = match funding_of(data, qty, inst, lim.funding_max_age_ms) {
        Ok(f) => f,
        Err(e) => {
            leg.entry = Some(entry);
            leg.exit = Some(exit);
            leg.fees = Some(fees);
            return fail(leg, e);
        }
    };
    let gross = qty * (exit.vwap - entry.vwap);
    let net = gross - fees.usd - funding.paid_usd;
    let ntl = entry.notional_usd;
    leg.gross_usd = Some(gross);
    leg.net_usd = Some(net);
    leg.gross_bps = (ntl > 0.0).then(|| gross / ntl * 1e4);
    leg.net_bps = (ntl > 0.0).then(|| net / ntl * 1e4);
    leg.mid_move_bps = match (entry.mid, exit.mid) {
        (Some(a), Some(b)) if a > 0.0 => Some(side.sign() * (b / a - 1.0) * 1e4),
        _ => None,
    };
    leg.entry = Some(entry);
    leg.exit = Some(exit);
    leg.fees = Some(fees);
    leg.funding = Some(funding);
}

/// Replay `rule` over `data` (one entry per name; module table).
pub fn regrade(data: &[LegData], rule: &RegradeRule, inst: &Instants, lim: &Limits) -> Regrade {
    let mut legs: Vec<Leg> = Vec::new();
    let mut signals: Vec<Signal> = Vec::new();
    let mut sorted: Vec<&LegData> = data.iter().collect();
    sorted.sort_by(|a, b| a.instrument.cmp(&b.instrument));
    for d in &sorted {
        let anchor = pick_ctx(&d.ctx, inst.anchor_ms, lim.anchor_max_age_ms);
        let entry = pick_ctx(&d.ctx, inst.entry_ms, lim.ctx_max_age_ms);
        let anchor_px = anchor
            .as_ref()
            .ok()
            .and_then(|r| ctx_price(&r.features))
            .map(|p| p.0);
        let entry_px = entry
            .as_ref()
            .ok()
            .and_then(|r| ctx_price(&r.features))
            .map(|p| p.0);
        let mut leg = Leg {
            instrument: d.instrument.clone(),
            signal: None,
            skip: None,
            selected: false,
            side: None,
            entry: None,
            exit: None,
            fees: None,
            funding: None,
            gross_usd: None,
            net_usd: None,
            gross_bps: None,
            net_bps: None,
            mid_move_bps: None,
            missing: None,
        };
        match signal_of(&d.instrument, false, anchor_px, entry_px) {
            Ok(s) => {
                leg.signal = Some(LegSignal {
                    anchor_px: s.anchor_px,
                    anchor_at_ms: anchor
                        .as_ref()
                        .map(|r| r.observed_at_ms)
                        .unwrap_or_default(),
                    entry_px: s.entry_px,
                    entry_at_ms: entry.as_ref().map(|r| r.observed_at_ms).unwrap_or_default(),
                    s_bps: s.s_bps,
                });
                signals.push(s);
            }
            Err(skip) => {
                let why = match skip {
                    Skip::MissingAnchor => {
                        anchor.err().unwrap_or_else(|| "no mid or mark > 0".into())
                    }
                    Skip::MissingEntry => {
                        entry.err().unwrap_or_else(|| "no mid or mark > 0".into())
                    }
                    _ => String::new(),
                };
                leg.skip = Some(if why.is_empty() {
                    skip_name(skip).to_string()
                } else {
                    format!("{}: {why}", skip_name(skip))
                });
            }
        }
        legs.push(leg);
    }
    let selected: Vec<String> = match rule.top_n {
        Some(n) => select_capped(
            &signals,
            &FadeRule {
                capped_top_n: n,
                min_abs_signal_bps: rule.min_abs_signal_bps,
            },
        ),
        None => signals
            .iter()
            .filter(|s| s.s_bps.abs() >= rule.min_abs_signal_bps)
            .map(|s| s.instrument.clone())
            .collect(),
    };
    let by_id: BTreeMap<&str, &LegData> =
        sorted.iter().map(|d| (d.instrument.as_str(), *d)).collect();
    for leg in &mut legs {
        if !selected.contains(&leg.instrument) {
            continue;
        }
        leg.selected = true;
        let s = leg.signal.as_ref().map_or(0.0, |s| s.s_bps);
        let fade = if s > 0.0 { Side::Sell } else { Side::Buy };
        let side = match rule.direction {
            Direction::Fade => fade,
            Direction::Follow => fade.opposite(),
        };
        leg.side = Some(side);
        if let Some(d) = by_id.get(leg.instrument.as_str()) {
            grade_leg(leg, d, side, rule, inst, lim);
        }
    }
    let graded: Vec<&Leg> = legs
        .iter()
        .filter(|l| l.selected && l.net_usd.is_some())
        .collect();
    let net_bps: Vec<f64> = graded.iter().filter_map(|l| l.net_bps).collect();
    let gross_bps: Vec<f64> = graded.iter().filter_map(|l| l.gross_bps).collect();
    let entry_slip: Vec<f64> = graded
        .iter()
        .filter_map(|l| l.entry.as_ref()?.slippage_bps_vs_mid)
        .collect();
    let exit_slip: Vec<f64> = graded
        .iter()
        .filter_map(|l| l.exit.as_ref()?.slippage_bps_vs_mid)
        .collect();
    let summary = Summary {
        names: legs.len(),
        signals: signals.len(),
        selected: selected.len(),
        graded: graded.len(),
        missing: legs.iter().filter(|l| l.missing.is_some()).count(),
        mean_net_bps: mean(&net_bps),
        mean_gross_bps: mean(&gross_bps),
        net_usd: graded.iter().filter_map(|l| l.net_usd).sum(),
        gross_usd: graded.iter().filter_map(|l| l.gross_usd).sum(),
        fees_usd: graded
            .iter()
            .filter_map(|l| l.fees.as_ref().map(|f| f.usd))
            .sum(),
        funding_paid_usd: graded
            .iter()
            .filter_map(|l| l.funding.as_ref().map(|f| f.paid_usd))
            .sum(),
        positive: graded
            .iter()
            .filter(|l| l.net_usd.is_some_and(|n| n > 0.0))
            .count(),
        mean_entry_slippage_bps: mean(&entry_slip),
        mean_exit_slippage_bps: mean(&exit_slip),
        partial_entries: graded
            .iter()
            .filter(|l| l.entry.as_ref().is_some_and(|e| e.unfilled > 0.0))
            .count(),
    };
    Regrade {
        rule: *rule,
        instants: *inst,
        limits: *lim,
        selected,
        legs,
        summary,
    }
}

// ── Checks against other evidence ───────────────────────────────────

/// One line of a preregistered signal list: `<full id>|<anchor>|<now>|<s bps>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedSignal {
    pub instrument: String,
    pub anchor_px: f64,
    pub now_px: f64,
    pub s_bps: f64,
}

/// Every line of `text` shaped `<id with ':'>|<num>|<num>|<num>`; other
/// lines are ignored.
pub fn parse_signal_lines(text: &str) -> Vec<ExpectedSignal> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim().split('|').map(str::trim).collect();
            if f.len() != 4 || !f[0].contains(':') || f[0].contains(' ') {
                return None;
            }
            Some(ExpectedSignal {
                instrument: f[0].to_string(),
                anchor_px: f[1].parse().ok()?,
                now_px: f[2].parse().ok()?,
                s_bps: f[3].parse().ok()?,
            })
        })
        .collect()
}

/// The replay's signals against an expected list: prices equal (relative
/// 1e-9), s within half the list's last digit (0.05 bps) + 1e-9.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalCheck {
    pub expected: usize,
    pub matched: usize,
    pub mismatches: Vec<String>,
    pub not_expected: Vec<String>,
}

fn same_px(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

pub fn check_signals(r: &Regrade, expected: &[ExpectedSignal]) -> SignalCheck {
    let mut mismatches = Vec::new();
    let mut matched = 0;
    for e in expected {
        let Some(leg) = r.legs.iter().find(|l| l.instrument == e.instrument) else {
            mismatches.push(format!("{}: not in the replay", e.instrument));
            continue;
        };
        let (anchor, now, s) = match &leg.signal {
            Some(sig) => (sig.anchor_px, sig.entry_px, sig.s_bps),
            None => {
                // A flat name has prices but no signal.
                if e.s_bps == 0.0 && leg.skip.as_deref() == Some("flat") {
                    matched += 1;
                } else {
                    mismatches.push(format!(
                        "{}: no signal ({}), expected s {}",
                        e.instrument,
                        leg.skip.as_deref().unwrap_or("?"),
                        e.s_bps
                    ));
                }
                continue;
            }
        };
        let mut why = Vec::new();
        if !same_px(anchor, e.anchor_px) {
            why.push(format!("anchor {anchor} vs {}", e.anchor_px));
        }
        if !same_px(now, e.now_px) {
            why.push(format!("now {now} vs {}", e.now_px));
        }
        if (s - e.s_bps).abs() > 0.05 + 1e-9 {
            why.push(format!("s {s:.4} vs {}", e.s_bps));
        }
        if why.is_empty() {
            matched += 1;
        } else {
            mismatches.push(format!("{}: {}", e.instrument, why.join(", ")));
        }
    }
    let not_expected = r
        .legs
        .iter()
        .filter(|l| l.signal.is_some() && !expected.iter().any(|e| e.instrument == l.instrument))
        .map(|l| l.instrument.clone())
        .collect();
    SignalCheck {
        expected: expected.len(),
        matched,
        mismatches,
        not_expected,
    }
}

/// One name: the replay against the ledger's trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegVsLedger {
    pub instrument: String,
    pub replay_side: Option<Side>,
    pub ledger_side: Option<Side>,
    pub replay_entry_vwap: Option<f64>,
    pub ledger_entry_vwap: Option<f64>,
    pub replay_exit_vwap: Option<f64>,
    pub ledger_exit_vwap: Option<f64>,
    pub replay_net_bps: Option<f64>,
    pub ledger_net_bps: Option<f64>,
    /// replay − ledger.
    pub diff_net_bps: Option<f64>,
    pub replay_net_usd: Option<f64>,
    pub ledger_net_usd: Option<f64>,
    pub replay_funding_paid_usd: Option<f64>,
    pub ledger_funding_paid_usd: Option<f64>,
    pub note: String,
}

/// Per name over the replay's selected legs and the ledger's trades.
pub fn compare_with_ledger(r: &Regrade, g: &AccountGrade) -> Vec<LegVsLedger> {
    let mut ids: Vec<String> = r.selected.clone();
    for t in &g.trades {
        if !ids.contains(&t.instrument) {
            ids.push(t.instrument.clone());
        }
    }
    ids.sort();
    ids.into_iter()
        .map(|id| {
            let leg = r.legs.iter().find(|l| l.instrument == id && l.selected);
            let trades: Vec<_> = g.trades.iter().filter(|t| t.instrument == id).collect();
            let t = trades.first();
            let mut note = Vec::new();
            if leg.is_none() {
                note.push("not selected by the replay".to_string());
            }
            if let Some(m) = leg.and_then(|l| l.missing.as_ref()) {
                note.push(format!("replay MISSING: {m}"));
            }
            if trades.is_empty() {
                note.push("no ledger trade".to_string());
            }
            if trades.len() > 1 {
                note.push(format!(
                    "{} ledger trades, the first compared",
                    trades.len()
                ));
            }
            let diff = match (leg.and_then(|l| l.net_bps), t.and_then(|t| t.net_bps)) {
                (Some(a), Some(b)) => Some(a - b),
                _ => None,
            };
            LegVsLedger {
                instrument: id,
                replay_side: leg.and_then(|l| l.side),
                ledger_side: t.map(|t| t.side),
                replay_entry_vwap: leg.and_then(|l| l.entry.as_ref().map(|e| e.vwap)),
                ledger_entry_vwap: t.map(|t| t.entry_vwap),
                replay_exit_vwap: leg.and_then(|l| l.exit.as_ref().map(|e| e.vwap)),
                ledger_exit_vwap: t.and_then(|t| t.exit_vwap),
                replay_net_bps: leg.and_then(|l| l.net_bps),
                ledger_net_bps: t.and_then(|t| t.net_bps),
                diff_net_bps: diff,
                replay_net_usd: leg.and_then(|l| l.net_usd),
                ledger_net_usd: t.map(|t| t.net_usd),
                replay_funding_paid_usd: leg.and_then(|l| l.funding.as_ref().map(|f| f.paid_usd)),
                ledger_funding_paid_usd: t.map(|t| t.funding_paid_usd),
                note: note.join("; "),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::book::L2Level;
    use serde_json::json;

    const H: i64 = HOUR_MS;

    fn ctx(at: i64, mid: f64, rate: f64, fee: f64) -> CtxSample {
        let mut features = Features::new();
        features.insert("mid".into(), json!(mid));
        features.insert("oracle".into(), json!(mid));
        features.insert("funding_1h".into(), json!(rate));
        features.insert("taker_fee_bps".into(), json!(fee));
        CtxSample {
            observed_at_ms: at,
            status: ObsStatus::Ok,
            features,
        }
    }

    fn lvl(px: f64, sz: f64) -> L2Level {
        L2Level { px, sz, n: 1 }
    }

    /// Bids 99.9 × 0.5, 99.8 × 10; asks 100.1 × 0.5, 100.2 × 10 around `c`.
    fn book(at: i64, c: f64) -> BookSample {
        let b = L2Book::new(
            vec![lvl(c - 0.1, 0.5), lvl(c - 0.2, 10.0)],
            vec![lvl(c + 0.1, 0.5), lvl(c + 0.2, 10.0)],
            at - 100,
        )
        .unwrap();
        BookSample {
            observed_at_ms: at,
            status: ObsStatus::Ok,
            book: Ok(b),
        }
    }

    fn rule(top_n: Option<usize>) -> RegradeRule {
        RegradeRule {
            direction: Direction::Fade,
            top_n,
            min_abs_signal_bps: 50.0,
            notional_usd: 100.0,
            fees: FeeModel::Flat { taker_bps: 1.0 },
        }
    }

    const INST: Instants = Instants {
        anchor_ms: 0,
        entry_ms: 10 * H,
        exit_ms: 12 * H,
    };
    const LIM: Limits = Limits {
        anchor_max_age_ms: 600_000,
        ctx_max_age_ms: 120_000,
        book_max_age_ms: 60_000,
        book_pick: BookPick::Next,
        funding_max_age_ms: 120_000,
    };

    /// Up 2 % over the weekend, back to 100 at the exit.
    fn up_name(id: &str) -> LegData {
        LegData {
            instrument: id.into(),
            ctx: vec![
                ctx(-1_000, 100.0, 0.0, 0.9),
                ctx(10 * H - 30_000, 102.0, 0.0001, 0.9),
                ctx(11 * H - 30_000, 101.0, 0.0001, 0.9),
                ctx(12 * H - 30_000, 100.0, 0.0001, 0.9),
            ],
            books: vec![
                book(10 * H - 50_000, 101.5),
                book(10 * H + 400, 102.0),
                book(12 * H + 300, 100.0),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_fade_walks_the_books_and_books_fees_and_funding() {
        let r = regrade(&[up_name("x:UP")], &rule(Some(4)), &INST, &LIM);
        assert_eq!(r.selected, vec!["x:UP".to_string()]);
        let leg = &r.legs[0];
        assert_eq!(leg.side, Some(Side::Sell));
        let e = leg.entry.as_ref().unwrap();
        // Sell $100 into bids 101.9 × 0.5 then 101.8.
        let q2 = (100.0 - 101.9 * 0.5) / 101.8;
        assert!((e.qty - (0.5 + q2)).abs() < 1e-12);
        assert_eq!(e.book_distance_ms, 400);
        assert!(e.slippage_bps_vs_mid.unwrap() > 0.0);
        let x = leg.exit.as_ref().unwrap();
        assert_eq!(x.side, Side::Buy);
        assert!((x.qty - e.qty).abs() < 1e-12);
        let f = leg.funding.as_ref().unwrap();
        assert_eq!(
            (f.hours, f.recorded_hours, f.source.as_str()),
            (2, 2, "recorded")
        );
        // Short pays nothing on a positive rate: paid = −qty × px × rate < 0.
        assert!(f.paid_usd < 0.0);
        let gross = -e.qty * (x.vwap - e.vwap);
        let fees = (e.notional_usd + x.notional_usd) * 1.0 / 1e4;
        assert!((leg.net_usd.unwrap() - (gross - fees - f.paid_usd)).abs() < 1e-12);
        assert!(leg.net_bps.unwrap() > 100.0);
        assert_eq!(r.summary.graded, 1);
        assert_eq!(r.summary.missing, 0);
    }

    #[test]
    fn as_of_takes_the_book_before_and_a_stale_or_missing_book_is_missing() {
        let lim = Limits {
            book_pick: BookPick::AsOf,
            ..LIM
        };
        let r = regrade(&[up_name("x:UP")], &rule(Some(4)), &INST, &lim);
        let e = r.legs[0].entry.as_ref().unwrap();
        assert_eq!(e.book_observed_at_ms, 10 * H - 50_000);
        // The exit has no book at or before 12 h within 60 s.
        assert!(r.legs[0]
            .missing
            .as_deref()
            .unwrap()
            .starts_with("exit book: "));
        assert_eq!(r.summary.graded, 0);
        assert_eq!(r.summary.missing, 1);
        assert_eq!(r.summary.mean_net_bps, None);
        // No books at all: the entry is MISSING, never 0.
        let mut bare = up_name("x:UP");
        bare.books.clear();
        let r = regrade(&[bare], &rule(None), &INST, &LIM);
        assert!(r.legs[0]
            .missing
            .as_deref()
            .unwrap()
            .starts_with("entry book: "));
        assert_eq!(r.legs[0].net_usd, None);
    }

    #[test]
    fn selection_skips_and_backfilled_funding() {
        let mut flat = up_name("x:FLAT");
        flat.ctx[1] = ctx(10 * H - 30_000, 100.0, 0.0001, 0.9);
        let mut small = up_name("x:SMALL");
        small.ctx[1] = ctx(10 * H - 30_000, 100.2, 0.0001, 0.9); // 20 bps
        let mut stale = up_name("x:STALE");
        stale.ctx[1].observed_at_ms = 10 * H - 200_000;
        let mut gap = up_name("x:GAP");
        gap.ctx.remove(2); // no recorded row before 11 h …
        gap.funding = vec![FundingRate {
            t_ms: 11 * H + 40,
            rate_1h: 0.0002,
        }];
        gap.bars = vec![BarClose {
            t_open_ms: 11 * H - BAR_MS,
            close: 101.0,
        }];
        let data = vec![flat, small, stale, gap, up_name("x:UP")];
        let r = regrade(&data, &rule(None), &INST, &LIM);
        let leg = |id: &str| r.legs.iter().find(|l| l.instrument == id).unwrap();
        assert_eq!(leg("x:FLAT").skip.as_deref(), Some("flat"));
        assert!(leg("x:STALE")
            .skip
            .as_deref()
            .unwrap()
            .starts_with("missing_entry: stale"));
        assert!(!leg("x:SMALL").selected);
        assert_eq!(r.selected, vec!["x:GAP".to_string(), "x:UP".to_string()]);
        let f = leg("x:GAP").funding.clone().unwrap();
        assert_eq!(
            (f.recorded_hours, f.backfilled_hours, f.source.as_str()),
            (1, 1, "mixed")
        );
        // Without the backfill the leg is MISSING.
        let mut nofill = up_name("x:GAP");
        nofill.ctx.remove(2);
        let r = regrade(&[nofill], &rule(None), &INST, &LIM);
        assert!(r.legs[0]
            .missing
            .as_deref()
            .unwrap()
            .starts_with("funding hour "));
        // Follow takes the other side.
        let follow = RegradeRule {
            direction: Direction::Follow,
            ..rule(None)
        };
        let r = regrade(&[up_name("x:UP")], &follow, &INST, &LIM);
        assert_eq!(r.legs[0].side, Some(Side::Buy));
    }

    #[test]
    fn signal_lines_parse_and_check() {
        let text =
            "intro\n```\nx:UP|100|102|198.0\nx:FLAT|100|100|0.0\nnot|a|line\nx:NO|1|2|3\n```\n";
        let lines = parse_signal_lines(text);
        assert_eq!(lines.len(), 3);
        let mut flat = up_name("x:FLAT");
        flat.ctx[1] = ctx(10 * H - 30_000, 100.0, 0.0001, 0.9);
        let r = regrade(&[up_name("x:UP"), flat], &rule(None), &INST, &LIM);
        let c = check_signals(&r, &lines);
        assert_eq!(c.expected, 3);
        assert_eq!(c.matched, 2, "{:?}", c.mismatches);
        assert_eq!(c.mismatches.len(), 1);
        assert!(c.mismatches[0].starts_with("x:NO: not in the replay"));
    }
}
