//! Paper ledger math (`risk-paper-ledger-domain`): positions, cash,
//! average-cost P&L, mark-to-market, exposure, leverage, Hyperliquid
//! funding, and the `paper_positions/1:<account>` observation. Pure; times
//! and marks are inputs. `risk-paper-ledger-store` persists these values,
//! `risk-gate-domain` reads them.
//!
//! | Rule | Detail |
//! |---|---|
//! | Quantity | signed: + long, − short; `avg_px` / `opened_ms` are `None` when flat; a residue ≤ 1e-9 (relative) of summed lots counts as flat |
//! | Fill (average cost) | same direction ⇒ `avg_px` re-weighted; opposite ⇒ realize `closed × (px − avg) × sign(qty)`; past zero (a flip) ⇒ close all, then open the rest at the fill px with a new `opened_ms` |
//! | Cash | collateral: initial + Σ realized − Σ fees − Σ funding. A fill moves cash only by its realized P&L and fee (perp-style; a spot book gives the same equity) |
//! | Mark | unrealized = qty × (mark − avg); a missing, stale or invalid mark ⇒ `Field::Error` (`mark:<instrument>`), never 0; flat ⇒ 0 |
//! | Equity | cash + Σ unrealized (funding settles into cash each hour, as on HL) |
//! | Exposure | at mark, total / per underlying / per venue: net = Σ qty × mark, gross = Σ \|qty × mark\|; a group with a failed mark is `Error` |
//! | Leverage | gross / equity; equity ≤ 0 ⇒ `Error` |
//! | Funding | HL: at each hour boundary, payment = qty × oracle × rate_1h, positive rate ⇒ longs pay; `funding_paid` positive = paid. Book every hour in time order before any later fill; an hour already booked, or before `opened_ms`, is skipped |
//! | `paper_positions/1:<account>` | status `partial` when an open position's mark failed (its numbers omitted, never 0); per-position rows with full ids in `data` |
//!
//! Not yet: maintenance margin from the HL margin tables (P1).

// Consumers land in wave W1 (`risk-paper-ledger-store`, `risk-gate-domain`,
// `risk-paper-tools`, `x-exit-rules`).
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::book::Side;
use crate::domain::observation::{
    set_bool, set_int, set_num, ErrorClass, Features, Field, ObsStatus, Observed, ReadError,
};

/// HL pays funding on the hour.
pub const HOUR_MS: i64 = 3_600_000;

/// Relative tolerance under which a quantity counts as flat (f64 residue of
/// summed lot sizes, e.g. 0.1 + 0.2 − 0.3).
const QTY_EPS: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LedgerError {
    #[error("invalid fill for {instrument}: {what}")]
    Fill { instrument: String, what: String },
    #[error("invalid funding for {instrument}: {what}")]
    Funding { instrument: String, what: String },
    #[error("invalid account {account}: {what}")]
    Account { account: String, what: String },
}

/// One execution against a paper account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    /// Full instrument id (`hyperliquid:xyz:TSLA`).
    pub instrument: String,
    /// Full underlying id the exposure limits net across venues.
    pub underlying: String,
    pub venue: String,
    pub side: Side,
    /// Base quantity, > 0.
    pub qty: f64,
    /// Average fill price (the walk's VWAP), > 0.
    pub px: f64,
    /// USD; negative = rebate.
    pub fee_usd: f64,
    pub ts_ms: i64,
}

/// What one fill did to its position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FillEffect {
    pub qty_before: f64,
    pub qty_after: f64,
    /// Quantity closed against the prior position.
    pub closed_qty: f64,
    /// Quantity opened or added.
    pub opened_qty: f64,
    /// P&L realized by this fill, before its fee.
    pub realized_pnl: f64,
    pub fee_usd: f64,
    /// Crossed zero: closed one side, opened the other.
    pub flipped: bool,
}

/// One instrument's position in a paper account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Full instrument id (`hyperliquid:xyz:TSLA`).
    pub instrument: String,
    /// Full underlying id (`company:tesla`).
    pub underlying: String,
    /// Venue id (`hyperliquid`).
    pub venue: String,
    /// Signed: + long, − short, 0 flat.
    pub qty: f64,
    /// Average entry price of the open quantity; `None` when flat.
    pub avg_px: Option<f64>,
    /// Cumulative realized P&L before fees and funding (USD).
    pub realized_pnl: f64,
    /// Cumulative fees (USD; negative = net rebates).
    pub fees_paid: f64,
    /// Cumulative funding (USD; positive = paid, negative = received).
    pub funding_paid: f64,
    /// When the open quantity was opened (set on open and on a flip);
    /// `None` when flat.
    pub opened_ms: Option<i64>,
    /// Last hour boundary whose funding was booked.
    pub last_funding_hour_ms: Option<i64>,
}

impl Position {
    pub fn flat(instrument: &str, underlying: &str, venue: &str) -> Self {
        Self {
            instrument: instrument.to_string(),
            underlying: underlying.to_string(),
            venue: venue.to_string(),
            qty: 0.0,
            avg_px: None,
            realized_pnl: 0.0,
            fees_paid: 0.0,
            funding_paid: 0.0,
            opened_ms: None,
            last_funding_hour_ms: None,
        }
    }

    pub fn is_flat(&self) -> bool {
        self.qty == 0.0
    }

    /// `Buy` = long, `Sell` = short, `None` = flat.
    pub fn side(&self) -> Option<Side> {
        if self.qty > 0.0 {
            Some(Side::Buy)
        } else if self.qty < 0.0 {
            Some(Side::Sell)
        } else {
            None
        }
    }

    /// Average-cost update (module table). The position is unchanged on error.
    pub fn apply_fill(&mut self, fill: &Fill) -> Result<FillEffect, LedgerError> {
        let bad = |what: String| LedgerError::Fill {
            instrument: fill.instrument.clone(),
            what,
        };
        if fill.instrument != self.instrument {
            return Err(bad(format!("position is {}", self.instrument)));
        }
        if fill.underlying != self.underlying || fill.venue != self.venue {
            return Err(bad(format!(
                "underlying {} / venue {}, position has {} / {}",
                fill.underlying, fill.venue, self.underlying, self.venue
            )));
        }
        if !(fill.qty.is_finite() && fill.qty > 0.0) {
            return Err(bad(format!("qty {} is not > 0", fill.qty)));
        }
        if !(fill.px.is_finite() && fill.px > 0.0) {
            return Err(bad(format!("px {} is not > 0", fill.px)));
        }
        if !fill.fee_usd.is_finite() {
            return Err(bad(format!("fee_usd {} is not finite", fill.fee_usd)));
        }
        let before = self.qty;
        let delta = fill.side.sign() * fill.qty;
        let mut effect = FillEffect {
            qty_before: before,
            qty_after: before,
            closed_qty: 0.0,
            opened_qty: 0.0,
            realized_pnl: 0.0,
            fee_usd: fill.fee_usd,
            flipped: false,
        };
        if before == 0.0 || before.signum() == delta.signum() {
            // Open or increase.
            let new_qty = before + delta;
            let avg = match self.avg_px {
                Some(a) if before != 0.0 => (before.abs() * a + fill.qty * fill.px) / new_qty.abs(),
                _ => fill.px,
            };
            if before == 0.0 {
                self.opened_ms = Some(fill.ts_ms);
            }
            self.qty = new_qty;
            self.avg_px = Some(avg);
            effect.opened_qty = fill.qty;
        } else {
            // Reduce, close or flip.
            let Some(avg) = self.avg_px else {
                return Err(bad("open position has no avg_px".to_string()));
            };
            let open = before.abs();
            let eps = QTY_EPS * open.max(fill.qty);
            let (closed, rest) = if (fill.qty - open).abs() <= eps {
                (open, 0.0)
            } else if fill.qty < open {
                (fill.qty, 0.0)
            } else {
                (open, fill.qty - open)
            };
            let realized = closed * (fill.px - avg) * before.signum();
            self.realized_pnl += realized;
            effect.closed_qty = closed;
            effect.realized_pnl = realized;
            if rest > 0.0 {
                self.qty = delta.signum() * rest;
                self.avg_px = Some(fill.px);
                self.opened_ms = Some(fill.ts_ms);
                effect.opened_qty = rest;
                effect.flipped = true;
            } else if closed == open {
                self.qty = 0.0;
                self.avg_px = None;
                self.opened_ms = None;
            } else {
                self.qty = before.signum() * (open - closed);
            }
        }
        self.fees_paid += fill.fee_usd;
        effect.qty_after = self.qty;
        Ok(effect)
    }

    /// Books the funding of hour boundary `hour_ms`: HL payment = qty ×
    /// oracle × rate_1h, positive = paid. `Ok(None)` = nothing to book
    /// (flat, opened after `hour_ms`, or that hour already booked).
    pub fn accrue_funding(
        &mut self,
        rate_1h: f64,
        oracle_px: f64,
        hour_ms: i64,
    ) -> Result<Option<f64>, LedgerError> {
        let bad = |what: String| LedgerError::Funding {
            instrument: self.instrument.clone(),
            what,
        };
        if hour_ms.rem_euclid(HOUR_MS) != 0 {
            return Err(bad(format!("hour_ms {hour_ms} is not on the hour")));
        }
        if !rate_1h.is_finite() {
            return Err(bad(format!("rate_1h {rate_1h} is not finite")));
        }
        if !(oracle_px.is_finite() && oracle_px > 0.0) {
            return Err(bad(format!("oracle_px {oracle_px} is not > 0")));
        }
        if self
            .last_funding_hour_ms
            .is_some_and(|last| hour_ms <= last)
        {
            return Ok(None);
        }
        match self.opened_ms {
            Some(opened) if !self.is_flat() && opened <= hour_ms => {}
            _ => return Ok(None),
        }
        let payment = self.qty * oracle_px * rate_1h;
        self.funding_paid += payment;
        self.last_funding_hour_ms = Some(hour_ms);
        Ok(Some(payment))
    }
}

/// A mark price and when it was observed (the `mkt_ctx/1` row time).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub px: f64,
    pub at_ms: i64,
}

fn mark_field(instrument: &str) -> String {
    format!("mark:{instrument}")
}

/// `Ok(px)` for a finite px > 0 at most `max_age_ms` old at `now_ms`;
/// otherwise `Error` on field `mark:<instrument>` — never 0.
pub fn fresh_mark(
    instrument: &str,
    mark: &Field<Mark>,
    now_ms: i64,
    max_age_ms: u64,
) -> Field<f64> {
    let field = mark_field(instrument);
    match mark {
        Field::Ok { value: m } if !(m.px.is_finite() && m.px > 0.0) => Field::err(ReadError::new(
            field,
            ErrorClass::Decode,
            format!("mark px {} is not > 0", m.px),
        )),
        Field::Ok { value: m } => {
            let age_ms = now_ms.saturating_sub(m.at_ms).max(0) as u64;
            if age_ms > max_age_ms {
                Field::err(ReadError::new(
                    field,
                    ErrorClass::Transient,
                    format!("stale: mark age {age_ms} ms > {max_age_ms} ms"),
                ))
            } else {
                Field::ok(m.px)
            }
        }
        Field::Absent => Field::err(ReadError::new(field, ErrorClass::Transient, "no mark")),
        Field::Error { error } => Field::err(ReadError {
            field,
            ..error.clone()
        }),
    }
}

/// Unrealized P&L of `position` at `mark_px`: qty × (mark − avg). Flat ⇒
/// `Ok(0)`; a missing / failed / invalid mark ⇒ `Error` — never 0.
pub fn mark(position: &Position, mark_px: &Field<f64>) -> Field<f64> {
    if position.is_flat() {
        return Field::ok(0.0);
    }
    let field = mark_field(&position.instrument);
    let Some(avg) = position.avg_px else {
        return Field::err(ReadError::new(
            field,
            ErrorClass::Fatal,
            "open position has no avg_px",
        ));
    };
    match mark_px {
        Field::Ok { value } if value.is_finite() && *value > 0.0 => {
            Field::ok(position.qty * (value - avg))
        }
        Field::Ok { value } => Field::err(ReadError::new(
            field,
            ErrorClass::Decode,
            format!("mark px {value} is not > 0"),
        )),
        Field::Absent => Field::err(ReadError::new(field, ErrorClass::Transient, "no mark")),
        Field::Error { error } => Field::err(error.clone()),
    }
}

/// One paper account: cash plus a position per instrument ever traded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperAccount {
    pub account: String,
    pub initial_cash_usd: f64,
    /// Collateral: initial + Σ realized − Σ fees − Σ funding.
    pub cash_usd: f64,
    /// Keyed by full instrument id; flat positions keep their history.
    pub positions: BTreeMap<String, Position>,
}

impl PaperAccount {
    pub fn new(account: &str, initial_cash_usd: f64) -> Result<Self, LedgerError> {
        if account.trim().is_empty() || !(initial_cash_usd.is_finite() && initial_cash_usd > 0.0) {
            return Err(LedgerError::Account {
                account: account.to_string(),
                what: format!("needs a name and initial cash > 0 (got {initial_cash_usd})"),
            });
        }
        Ok(Self {
            account: account.to_string(),
            initial_cash_usd,
            cash_usd: initial_cash_usd,
            positions: BTreeMap::new(),
        })
    }

    /// Applies `fill` to its position (created flat on first use); cash
    /// moves by the realized P&L minus the fee. Unchanged on error.
    pub fn apply_fill(&mut self, fill: &Fill) -> Result<FillEffect, LedgerError> {
        let mut position = self
            .positions
            .get(&fill.instrument)
            .cloned()
            .unwrap_or_else(|| Position::flat(&fill.instrument, &fill.underlying, &fill.venue));
        let effect = position.apply_fill(fill)?;
        self.positions.insert(fill.instrument.clone(), position);
        self.cash_usd += effect.realized_pnl - effect.fee_usd;
        Ok(effect)
    }

    /// [`Position::accrue_funding`] for `instrument`; cash moves by the
    /// payment. `Ok(None)` for an instrument never traded.
    pub fn accrue_funding(
        &mut self,
        instrument: &str,
        rate_1h: f64,
        oracle_px: f64,
        hour_ms: i64,
    ) -> Result<Option<f64>, LedgerError> {
        let Some(position) = self.positions.get_mut(instrument) else {
            return Ok(None);
        };
        let paid = position.accrue_funding(rate_1h, oracle_px, hour_ms)?;
        if let Some(p) = paid {
            self.cash_usd -= p;
        }
        Ok(paid)
    }

    pub fn open_positions(&self) -> impl Iterator<Item = &Position> {
        self.positions.values().filter(|p| !p.is_flat())
    }

    pub fn realized_pnl(&self) -> f64 {
        self.positions.values().map(|p| p.realized_pnl).sum()
    }

    pub fn fees_paid(&self) -> f64 {
        self.positions.values().map(|p| p.fees_paid).sum()
    }

    pub fn funding_paid(&self) -> f64 {
        self.positions.values().map(|p| p.funding_paid).sum()
    }
}

/// Net and gross notional at mark (USD).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    pub gross_usd: f64,
    pub net_usd: f64,
}

/// One open position at mark (a `paper_positions/1` data row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionRow {
    pub instrument: String,
    pub underlying: String,
    pub venue: String,
    pub qty: f64,
    pub avg_px: Option<f64>,
    pub opened_ms: Option<i64>,
    pub mark_px: Field<f64>,
    /// qty × mark (signed).
    pub notional_usd: Field<f64>,
    pub upnl_usd: Field<f64>,
    pub realized_pnl_usd: f64,
    pub fees_usd: f64,
    pub funding_usd: f64,
}

/// Exposure of one underlying or venue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupExposure {
    pub key: String,
    pub exposure: Field<Exposure>,
}

/// `paper_positions/1:<account>` — account totals and open positions at mark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperPositions {
    pub account: String,
    pub initial_cash_usd: f64,
    pub cash_usd: f64,
    /// Σ realized P&L before fees and funding.
    pub realized_pnl_usd: f64,
    pub fees_usd: f64,
    /// Positive = paid.
    pub funding_usd: f64,
    pub upnl_usd: Field<f64>,
    pub equity_usd: Field<f64>,
    pub exposure: Field<Exposure>,
    pub leverage: Field<f64>,
    /// Equity − day-start equity; `Absent` without a day start.
    pub daily_pnl_usd: Field<f64>,
    /// Risk halt (kill switch / loss trip); `None` = not known here.
    pub halted: Option<bool>,
    /// Open positions whose mark failed.
    pub marks_stale: usize,
    pub positions: Vec<PositionRow>,
    pub by_underlying: Vec<GroupExposure>,
    pub by_venue: Vec<GroupExposure>,
}

/// First error among `fields`, else the sum.
fn sum_fields<'a>(fields: impl IntoIterator<Item = &'a Field<f64>>) -> Field<f64> {
    let mut total = 0.0;
    for f in fields {
        match f {
            Field::Ok { value } => total += value,
            Field::Error { error } => return Field::err(error.clone()),
            Field::Absent => {}
        }
    }
    Field::ok(total)
}

fn exposure_of<'a>(rows: impl IntoIterator<Item = &'a PositionRow>) -> Field<Exposure> {
    let mut e = Exposure {
        gross_usd: 0.0,
        net_usd: 0.0,
    };
    for row in rows {
        match &row.notional_usd {
            Field::Ok { value } => {
                e.gross_usd += value.abs();
                e.net_usd += value;
            }
            Field::Error { error } => return Field::err(error.clone()),
            Field::Absent => {}
        }
    }
    Field::ok(e)
}

fn grouped(rows: &[PositionRow], key: fn(&PositionRow) -> &str) -> Vec<GroupExposure> {
    let mut groups: BTreeMap<&str, Vec<&PositionRow>> = BTreeMap::new();
    for row in rows {
        groups.entry(key(row)).or_default().push(row);
    }
    groups
        .into_iter()
        .map(|(k, rows)| GroupExposure {
            key: k.to_string(),
            exposure: exposure_of(rows),
        })
        .collect()
}

impl PaperPositions {
    /// Values `account` at `marks` (by full instrument id; a missing entry is
    /// a missing mark). `day_start_equity_usd` / `halted` come from the
    /// store's risk state.
    pub fn build(
        account: &PaperAccount,
        marks: &BTreeMap<String, Field<Mark>>,
        now_ms: i64,
        max_mark_age_ms: u64,
        day_start_equity_usd: Option<f64>,
        halted: Option<bool>,
    ) -> Self {
        let rows: Vec<PositionRow> = account
            .open_positions()
            .map(|p| {
                let raw = marks.get(&p.instrument).cloned().unwrap_or(Field::Absent);
                let mark_px = fresh_mark(&p.instrument, &raw, now_ms, max_mark_age_ms);
                let upnl_usd = mark(p, &mark_px);
                let notional_usd = match &mark_px {
                    Field::Ok { value } => Field::ok(p.qty * value),
                    other => other.clone(),
                };
                PositionRow {
                    instrument: p.instrument.clone(),
                    underlying: p.underlying.clone(),
                    venue: p.venue.clone(),
                    qty: p.qty,
                    avg_px: p.avg_px,
                    opened_ms: p.opened_ms,
                    mark_px,
                    notional_usd,
                    upnl_usd,
                    realized_pnl_usd: p.realized_pnl,
                    fees_usd: p.fees_paid,
                    funding_usd: p.funding_paid,
                }
            })
            .collect();
        let upnl_usd = sum_fields(rows.iter().map(|r| &r.upnl_usd));
        let equity_usd = match &upnl_usd {
            Field::Ok { value } => Field::ok(account.cash_usd + value),
            other => other.clone(),
        };
        let exposure = exposure_of(&rows);
        let leverage = match (&exposure, &equity_usd) {
            (Field::Error { error }, _) | (_, Field::Error { error }) => Field::err(error.clone()),
            (Field::Ok { value: e }, Field::Ok { value: eq }) if *eq > 0.0 => {
                Field::ok(e.gross_usd / eq)
            }
            (_, Field::Ok { value: eq }) => Field::err(ReadError::new(
                "leverage",
                ErrorClass::NotApplicable,
                format!("equity {eq} <= 0"),
            )),
            _ => Field::Absent,
        };
        let daily_pnl_usd = match (day_start_equity_usd, &equity_usd) {
            (None, _) => Field::Absent,
            (Some(d), _) if !d.is_finite() => Field::err(ReadError::new(
                "day_start_equity",
                ErrorClass::Decode,
                format!("day-start equity {d} is not finite"),
            )),
            (Some(d), Field::Ok { value }) => Field::ok(value - d),
            (Some(_), other) => other.clone(),
        };
        Self {
            account: account.account.clone(),
            initial_cash_usd: account.initial_cash_usd,
            cash_usd: account.cash_usd,
            realized_pnl_usd: account.realized_pnl(),
            fees_usd: account.fees_paid(),
            funding_usd: account.funding_paid(),
            marks_stale: rows.iter().filter(|r| r.mark_px.is_error()).count(),
            by_underlying: grouped(&rows, |r| r.underlying.as_str()),
            by_venue: grouped(&rows, |r| r.venue.as_str()),
            upnl_usd,
            equity_usd,
            exposure,
            leverage,
            daily_pnl_usd,
            halted,
            positions: rows,
        }
    }

    /// Root causes only: each failed mark, then leverage / day-start errors
    /// that are not copies of one.
    fn field_errors(&self) -> Vec<ReadError> {
        let mut out: Vec<ReadError> = self
            .positions
            .iter()
            .filter_map(|r| r.mark_px.error().cloned())
            .collect();
        for e in [self.leverage.error(), self.daily_pnl_usd.error()]
            .into_iter()
            .flatten()
        {
            if !out
                .iter()
                .any(|o| o.field == e.field && o.message == e.message)
            {
                out.push(e.clone());
            }
        }
        out
    }
}

impl Observed for PaperPositions {
    const SCHEMA: &'static str = "paper_positions/1";

    fn subject(&self) -> String {
        self.account.clone()
    }

    fn headline(&self) -> String {
        let usd = |f: &Field<f64>| f.value().map_or("error".to_string(), |v| format!("{v:.2}"));
        let gross = self
            .exposure
            .value()
            .map_or("error".to_string(), |e| format!("{:.2}", e.gross_usd));
        let lev = self
            .leverage
            .value()
            .map_or("error".to_string(), |l| format!("{l:.3}x"));
        let mut s = format!(
            "paper_positions account={} open={} equity={} upnl={} gross={gross} lev={lev} cash={:.2}",
            self.account,
            self.positions.len(),
            usd(&self.equity_usd),
            usd(&self.upnl_usd),
            self.cash_usd,
        );
        if self.marks_stale > 0 {
            s.push_str(&format!(" stale_marks={}", self.marks_stale));
        }
        if self.halted == Some(true) {
            s.push_str(" halted");
        }
        s
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "n_positions", Some(self.positions.len() as i64));
        set_num(&mut f, "cash_usd", Some(self.cash_usd));
        set_num(&mut f, "rpnl_usd", Some(self.realized_pnl_usd));
        set_num(&mut f, "fees_usd", Some(self.fees_usd));
        set_num(&mut f, "funding_usd", Some(self.funding_usd));
        set_num(&mut f, "upnl_usd", self.upnl_usd.value().copied());
        set_num(&mut f, "equity_usd", self.equity_usd.value().copied());
        set_num(
            &mut f,
            "gross_exposure_usd",
            self.exposure.value().map(|e| e.gross_usd),
        );
        set_num(
            &mut f,
            "net_exposure_usd",
            self.exposure.value().map(|e| e.net_usd),
        );
        set_num(&mut f, "leverage", self.leverage.value().copied());
        set_num(&mut f, "daily_pnl_usd", self.daily_pnl_usd.value().copied());
        set_bool(&mut f, "marks_stale", Some(self.marks_stale > 0));
        set_int(&mut f, "n_marks_stale", Some(self.marks_stale as i64));
        set_bool(&mut f, "halted", self.halted);
        f
    }

    /// `Partial` when any valued field failed (the account itself always
    /// reads: cash, realized P&L, fees and funding are ledger values).
    fn status(&self) -> ObsStatus {
        let failed = self.marks_stale > 0
            || self.upnl_usd.is_error()
            || self.equity_usd.is_error()
            || self.exposure.is_error()
            || self.leverage.is_error()
            || self.daily_pnl_usd.is_error();
        if failed {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.field_errors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_LINE1_CHARS};

    const ACCOUNT: &str = "paper-test";
    const TSLA_HL: &str = "hyperliquid:xyz:TSLA";
    const TSLA_RH: &str = "robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d";
    const TESLA: &str = "company:tesla";
    /// 2026-10-03 00:00 UTC (= Fri 2026-10-02 20:00 ET), on the hour.
    const T0: i64 = 1_790_985_600_000;
    const MIN: i64 = 60_000;

    fn close(a: f64, b: f64, what: &str) {
        assert!(
            (a - b).abs() <= 1e-9 * b.abs().max(1.0),
            "{what}: got {a}, want {b}"
        );
    }

    fn fill(instrument: &str, side: Side, qty: f64, px: f64, fee_usd: f64, ts_ms: i64) -> Fill {
        let venue = instrument.split(':').next().unwrap();
        Fill {
            instrument: instrument.to_string(),
            underlying: TESLA.to_string(),
            venue: venue.to_string(),
            side,
            qty,
            px,
            fee_usd,
            ts_ms,
        }
    }

    fn marks(entries: &[(&str, f64, i64)]) -> BTreeMap<String, Field<Mark>> {
        entries
            .iter()
            .map(|&(id, px, at_ms)| (id.to_string(), Field::ok(Mark { px, at_ms })))
            .collect()
    }

    /// The hand-computed sequence (bc): open, increase, funding H1, partial
    /// close, funding H2, flip, funding H3.
    ///
    /// | Step | qty | avg | realized | cash |
    /// |---|---|---|---|---|
    /// | buy 2 @ 100, fee 0.02 | 2 | 100 | 0 | 999.98 |
    /// | buy 1 @ 106, fee 0.01 | 3 | 102 | 0 | 999.97 |
    /// | H1: rate 0.0000125, oracle 104 ⇒ pays 0.0039 | 3 | 102 | 0 | 999.9661 |
    /// | sell 1.5 @ 110, fee 0.015 | 1.5 | 102 | 12 | 1011.9511 |
    /// | H2: rate −0.00002, oracle 108 ⇒ receives 0.00324 | 1.5 | 102 | 12 | 1011.95434 |
    /// | sell 4 @ 98, fee 0.04 (flip) | −2.5 | 98 | 6 | 1005.91434 |
    /// | H3: rate 0.0001, oracle 97 ⇒ short receives 0.02425 | −2.5 | 98 | 6 | 1005.93859 |
    fn sequence() -> PaperAccount {
        let mut a = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        let e = a
            .apply_fill(&fill(TSLA_HL, Side::Buy, 2.0, 100.0, 0.02, T0 + 10 * MIN))
            .unwrap();
        assert_eq!((e.qty_before, e.qty_after, e.opened_qty), (0.0, 2.0, 2.0));
        let e = a
            .apply_fill(&fill(TSLA_HL, Side::Buy, 1.0, 106.0, 0.01, T0 + 20 * MIN))
            .unwrap();
        assert_eq!((e.qty_after, e.closed_qty, e.realized_pnl), (3.0, 0.0, 0.0));
        let paid = a
            .accrue_funding(TSLA_HL, 0.000_012_5, 104.0, T0 + 60 * MIN)
            .unwrap();
        close(paid.unwrap(), 0.0039, "H1 payment");
        let e = a
            .apply_fill(&fill(TSLA_HL, Side::Sell, 1.5, 110.0, 0.015, T0 + 70 * MIN))
            .unwrap();
        assert_eq!((e.qty_after, e.closed_qty, e.flipped), (1.5, 1.5, false));
        close(e.realized_pnl, 12.0, "partial close realized");
        let paid = a
            .accrue_funding(TSLA_HL, -0.000_02, 108.0, T0 + 120 * MIN)
            .unwrap();
        close(paid.unwrap(), -0.003_24, "H2 payment");
        let e = a
            .apply_fill(&fill(TSLA_HL, Side::Sell, 4.0, 98.0, 0.04, T0 + 130 * MIN))
            .unwrap();
        assert!(e.flipped);
        assert_eq!((e.qty_before, e.qty_after), (1.5, -2.5));
        assert_eq!((e.closed_qty, e.opened_qty), (1.5, 2.5));
        close(e.realized_pnl, -6.0, "flip realized");
        let paid = a
            .accrue_funding(TSLA_HL, 0.0001, 97.0, T0 + 180 * MIN)
            .unwrap();
        close(paid.unwrap(), -0.024_25, "H3 payment");
        a
    }

    #[test]
    fn open_increase_partial_close_flip_and_funding() {
        let a = sequence();
        let p = &a.positions[TSLA_HL];
        assert_eq!(p.qty, -2.5);
        assert_eq!(p.avg_px, Some(98.0));
        assert_eq!(
            p.opened_ms,
            Some(T0 + 130 * MIN),
            "the flip opens a new position"
        );
        assert_eq!(p.side(), Some(Side::Sell));
        close(p.realized_pnl, 6.0, "realized");
        close(p.fees_paid, 0.085, "fees");
        close(p.funding_paid, -0.023_59, "funding");
        close(a.cash_usd, 1_005.938_59, "cash");
        // Cash reconciles: initial + realized − fees − funding.
        close(
            a.cash_usd,
            a.initial_cash_usd + a.realized_pnl() - a.fees_paid() - a.funding_paid(),
            "cash invariant",
        );
    }

    #[test]
    fn funding_books_each_hour_once_and_only_while_open() {
        let mut a = sequence();
        let cash = a.cash_usd;
        // H3 again, and an older hour: nothing.
        assert_eq!(
            a.accrue_funding(TSLA_HL, 0.0001, 97.0, T0 + 180 * MIN),
            Ok(None)
        );
        assert_eq!(
            a.accrue_funding(TSLA_HL, 0.0001, 97.0, T0 + 120 * MIN),
            Ok(None)
        );
        assert_eq!(a.cash_usd, cash);
        // Not on the hour / bad inputs.
        for (rate, oracle, hour) in [
            (0.0001, 97.0, T0 + 181 * MIN),
            (f64::NAN, 97.0, T0 + 240 * MIN),
            (0.0001, 0.0, T0 + 240 * MIN),
        ] {
            assert!(a.accrue_funding(TSLA_HL, rate, oracle, hour).is_err());
        }
        // Never traded ⇒ nothing.
        assert_eq!(a.accrue_funding(TSLA_RH, 0.0001, 97.0, T0), Ok(None));
        // Opened after the hour ⇒ that hour is not charged.
        let mut b = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        b.apply_fill(&fill(TSLA_HL, Side::Buy, 1.0, 100.0, 0.0, T0 + 10 * MIN))
            .unwrap();
        assert_eq!(b.accrue_funding(TSLA_HL, 0.0001, 100.0, T0), Ok(None));
        // Flat ⇒ nothing.
        b.apply_fill(&fill(TSLA_HL, Side::Sell, 1.0, 100.0, 0.0, T0 + 20 * MIN))
            .unwrap();
        assert_eq!(
            b.accrue_funding(TSLA_HL, 0.0001, 100.0, T0 + 60 * MIN),
            Ok(None)
        );
    }

    #[test]
    fn close_to_flat_short_side_and_lot_residue() {
        let mut a = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        a.apply_fill(&fill(TSLA_HL, Side::Buy, 3.0, 100.0, 0.0, T0))
            .unwrap();
        let e = a
            .apply_fill(&fill(TSLA_HL, Side::Sell, 3.0, 101.0, 0.0, T0 + MIN))
            .unwrap();
        close(e.realized_pnl, 3.0, "close realized");
        let p = &a.positions[TSLA_HL];
        assert_eq!(
            (p.qty, p.avg_px, p.opened_ms, p.side()),
            (0.0, None, None, None)
        );
        assert_eq!(a.open_positions().count(), 0);

        // Short: add at a lower price, then buy back part.
        let mut s = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        s.apply_fill(&fill(TSLA_HL, Side::Sell, 1.0, 100.0, 0.0, T0))
            .unwrap();
        s.apply_fill(&fill(TSLA_HL, Side::Sell, 1.0, 90.0, 0.0, T0 + MIN))
            .unwrap();
        assert_eq!(
            (s.positions[TSLA_HL].qty, s.positions[TSLA_HL].avg_px),
            (-2.0, Some(95.0))
        );
        let e = s
            .apply_fill(&fill(TSLA_HL, Side::Buy, 0.5, 80.0, 0.0, T0 + 2 * MIN))
            .unwrap();
        close(e.realized_pnl, 7.5, "short partial close: 0.5 × (95 − 80)");
        assert_eq!(s.positions[TSLA_HL].qty, -1.5);

        // 0.1 + 0.2 then sell 0.3: flat, not a 5e-17 short.
        let mut r = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        r.apply_fill(&fill(TSLA_HL, Side::Buy, 0.1, 100.0, 0.0, T0))
            .unwrap();
        r.apply_fill(&fill(TSLA_HL, Side::Buy, 0.2, 100.0, 0.0, T0))
            .unwrap();
        let e = r
            .apply_fill(&fill(TSLA_HL, Side::Sell, 0.3, 100.0, 0.0, T0))
            .unwrap();
        assert!(!e.flipped);
        assert_eq!(r.positions[TSLA_HL].qty, 0.0);
        assert_eq!(r.positions[TSLA_HL].avg_px, None);
    }

    #[test]
    fn invalid_fills_leave_the_account_unchanged() {
        let mut a = sequence();
        let before = a.clone();
        let mut wrong_underlying = fill(TSLA_HL, Side::Buy, 1.0, 100.0, 0.0, T0);
        wrong_underlying.underlying = "company:nvidia".to_string();
        let bad = [
            fill(TSLA_HL, Side::Buy, 0.0, 100.0, 0.0, T0),
            fill(TSLA_HL, Side::Buy, 1.0, f64::NAN, 0.0, T0),
            fill(TSLA_HL, Side::Buy, 1.0, 100.0, f64::INFINITY, T0),
            wrong_underlying,
        ];
        for f in &bad {
            let e = a.apply_fill(f).unwrap_err();
            assert!(e.to_string().contains(TSLA_HL), "{e}");
        }
        assert_eq!(a, before);
        // A rejected first fill does not leave a flat position behind.
        let mut fresh = PaperAccount::new(ACCOUNT, 1_000.0).unwrap();
        assert!(fresh.apply_fill(&bad[0]).is_err());
        assert!(fresh.positions.is_empty());
        assert!(PaperAccount::new("", 1_000.0).is_err());
        assert!(PaperAccount::new(ACCOUNT, 0.0).is_err());
    }

    #[test]
    fn marks_never_become_zero() {
        let a = sequence();
        let p = &a.positions[TSLA_HL];
        close(
            mark(p, &Field::ok(96.0)).value().copied().unwrap(),
            5.0,
            "short upnl",
        );
        let absent = mark(p, &Field::Absent);
        assert_eq!(absent.error().unwrap().field, format!("mark:{TSLA_HL}"));
        let err = ReadError::new("ctx", ErrorClass::Timeout, "slow");
        assert_eq!(mark(p, &Field::err(err.clone())), Field::err(err));
        assert!(mark(p, &Field::ok(0.0)).is_error());
        assert_eq!(
            mark(
                &Position::flat(TSLA_HL, TESLA, "hyperliquid"),
                &Field::Absent
            ),
            Field::ok(0.0)
        );

        let now = T0 + 200 * MIN;
        let m = |at_ms| Field::ok(Mark { px: 96.0, at_ms });
        assert_eq!(
            fresh_mark(TSLA_HL, &m(now - 5_000), now, 5_000),
            Field::ok(96.0)
        );
        let stale = fresh_mark(TSLA_HL, &m(now - 5_001), now, 5_000);
        let e = stale.error().unwrap();
        assert_eq!(
            (e.class, e.field.as_str()),
            (ErrorClass::Transient, "mark:hyperliquid:xyz:TSLA")
        );
        assert!(e.message.contains("stale"), "{}", e.message);
        assert!(fresh_mark(TSLA_HL, &Field::Absent, now, 5_000).is_error());
        let timeout = Field::err(ReadError::new("mkt_ctx", ErrorClass::Timeout, "slow"));
        let e = fresh_mark(TSLA_HL, &timeout, now, 5_000);
        assert_eq!(e.error().unwrap().class, ErrorClass::Timeout);
        assert_eq!(e.error().unwrap().field, format!("mark:{TSLA_HL}"));
    }

    /// Two venues, one underlying: short HL xyz:TSLA −2.5 @ 98 marked 96,
    /// long RH TSLA token 2 @ 97.5 marked 96.5.
    fn two_venue_account() -> PaperAccount {
        let mut a = sequence();
        a.apply_fill(&fill(TSLA_RH, Side::Buy, 2.0, 97.5, 0.05, T0 + 150 * MIN))
            .unwrap();
        close(a.cash_usd, 1_005.888_59, "cash after the RH buy");
        a
    }

    #[test]
    fn paper_positions_values_equity_exposure_and_leverage() {
        let a = two_venue_account();
        let now = T0 + 200 * MIN;
        let m = marks(&[(TSLA_HL, 96.0, now - 1_000), (TSLA_RH, 96.5, now - 2_000)]);
        let pp = PaperPositions::build(&a, &m, now, 5_000, Some(1_000.0), Some(false));
        assert_eq!(pp.status(), ObsStatus::Ok);
        assert!(pp.errors().is_empty());
        assert_eq!(pp.positions.len(), 2);
        close(*pp.upnl_usd.value().unwrap(), 3.0, "upnl 5 − 2");
        close(*pp.equity_usd.value().unwrap(), 1_008.888_59, "equity");
        let e = pp.exposure.value().unwrap();
        close(e.gross_usd, 433.0, "gross 240 + 193");
        close(e.net_usd, -47.0, "net −240 + 193");
        close(
            *pp.leverage.value().unwrap(),
            0.429_185_149_174_895_5,
            "leverage",
        );
        close(*pp.daily_pnl_usd.value().unwrap(), 8.888_59, "daily pnl");
        // Per underlying: both legs net against each other.
        assert_eq!(pp.by_underlying.len(), 1);
        assert_eq!(pp.by_underlying[0].key, TESLA);
        let u = pp.by_underlying[0].exposure.value().unwrap();
        close(u.net_usd, -47.0, "tesla net");
        close(u.gross_usd, 433.0, "tesla gross");
        // Per venue.
        let venues: Vec<(&str, f64)> = pp
            .by_venue
            .iter()
            .map(|g| (g.key.as_str(), g.exposure.value().unwrap().net_usd))
            .collect();
        assert_eq!(venues, [("hyperliquid", -240.0), ("robinhood", 193.0)]);

        let f = pp.features();
        assert_features_ok(&f);
        assert_eq!(f["n_positions"], 2);
        assert_eq!(f["marks_stale"], false);
        assert_eq!(f["halted"], false);
        for k in [
            "equity_usd",
            "upnl_usd",
            "gross_exposure_usd",
            "leverage",
            "daily_pnl_usd",
        ] {
            assert!(f.contains_key(k), "{k}");
        }

        let obs = Observation::of("paper_positions", &pp, now, 2_000, ObsSource::Live);
        assert_eq!(obs.key, format!("paper_positions/1:{ACCOUNT}"));
        assert_eq!(obs.typed::<PaperPositions>().unwrap(), pp);
        let text = obs.render_text(now);
        let line1 = text.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert!(line1.contains(ACCOUNT), "{line1}");
        // Full ids in data, never shortened.
        assert!(text.contains(TSLA_RH) && text.contains(TSLA_HL), "{text}");
    }

    #[test]
    fn single_position_matches_the_hand_computed_valuation() {
        let a = sequence();
        let now = T0 + 200 * MIN;
        let m = marks(&[(TSLA_HL, 96.0, now)]);
        let pp = PaperPositions::build(&a, &m, now, 5_000, Some(1_000.0), None);
        close(*pp.equity_usd.value().unwrap(), 1_010.938_59, "equity");
        close(
            *pp.leverage.value().unwrap(),
            0.237_403_144_339_360_9,
            "leverage 240 / equity",
        );
        close(*pp.daily_pnl_usd.value().unwrap(), 10.938_59, "daily");
        let row = &pp.positions[0];
        assert_eq!(row.notional_usd, Field::ok(-240.0));
        assert_eq!(row.upnl_usd, Field::ok(5.0));
        assert!(
            !pp.features().contains_key("halted"),
            "unknown halt state is omitted"
        );
    }

    #[test]
    fn stale_mark_makes_the_row_partial_and_carries_no_numbers() {
        let a = two_venue_account();
        let now = T0 + 200 * MIN;
        // The RH mark is 10 s old with a 5 s limit.
        let m = marks(&[(TSLA_HL, 96.0, now), (TSLA_RH, 96.5, now - 10_000)]);
        let pp = PaperPositions::build(&a, &m, now, 5_000, Some(1_000.0), Some(true));
        assert_eq!(pp.status(), ObsStatus::Partial);
        assert_eq!(pp.marks_stale, 1);
        for f in [
            &pp.upnl_usd,
            &pp.equity_usd,
            &pp.leverage,
            &pp.daily_pnl_usd,
        ] {
            assert!(f.is_error(), "{f:?}");
        }
        assert!(pp.exposure.is_error());
        let errors = pp.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].field, format!("mark:{TSLA_RH}"));
        // The HL venue still values; the shared underlying does not.
        let venue = |k: &str| pp.by_venue.iter().find(|g| g.key == k).unwrap();
        assert_eq!(
            venue("hyperliquid").exposure.value().unwrap().net_usd,
            -240.0
        );
        assert!(venue("robinhood").exposure.is_error());
        assert!(pp.by_underlying[0].exposure.is_error());

        let f = pp.features();
        assert_features_ok(&f);
        for k in [
            "upnl_usd",
            "equity_usd",
            "gross_exposure_usd",
            "net_exposure_usd",
            "leverage",
            "daily_pnl_usd",
        ] {
            assert!(!f.contains_key(k), "{k} must be omitted, never 0");
        }
        for k in ["cash_usd", "rpnl_usd", "fees_usd", "funding_usd"] {
            assert!(f.contains_key(k), "{k}");
        }
        assert_eq!(
            (f["marks_stale"].clone(), f["n_marks_stale"].clone()),
            (true.into(), 1.into())
        );
        let head = pp.headline();
        assert!(
            head.contains("equity=error")
                && head.contains("stale_marks=1")
                && head.ends_with("halted"),
            "{head}"
        );

        // A missing mark is the same: an error, never 0.
        let only_hl = marks(&[(TSLA_HL, 96.0, now)]);
        let pp = PaperPositions::build(&a, &only_hl, now, 5_000, None, None);
        assert_eq!(pp.status(), ObsStatus::Partial);
        assert_eq!(pp.daily_pnl_usd, Field::Absent);
        let rh = pp
            .positions
            .iter()
            .find(|r| r.instrument == TSLA_RH)
            .unwrap();
        assert!(rh.upnl_usd.is_error() && rh.notional_usd.is_error());
    }

    #[test]
    fn flat_account_and_negative_equity() {
        let a = PaperAccount::new(ACCOUNT, 100.0).unwrap();
        let pp = PaperPositions::build(&a, &BTreeMap::new(), T0, 5_000, None, None);
        assert_eq!(pp.status(), ObsStatus::Ok);
        assert_eq!(pp.equity_usd, Field::ok(100.0));
        assert_eq!(pp.leverage, Field::ok(0.0));
        assert_eq!(pp.features()["n_positions"], 0);

        // 10 @ 100 on $100, marked at 89: equity 100 − 0.09 − 110 < 0.
        let mut b = PaperAccount::new(ACCOUNT, 100.0).unwrap();
        b.apply_fill(&fill(TSLA_HL, Side::Buy, 10.0, 100.0, 0.09, T0))
            .unwrap();
        let m = marks(&[(TSLA_HL, 89.0, T0)]);
        let pp = PaperPositions::build(&b, &m, T0, 5_000, None, None);
        close(*pp.equity_usd.value().unwrap(), -10.09, "equity");
        let e = pp.leverage.error().unwrap();
        assert_eq!(e.class, ErrorClass::NotApplicable);
        assert_eq!(pp.status(), ObsStatus::Partial);
        assert_eq!(pp.errors().len(), 1);
        assert!(!pp.features().contains_key("leverage"));
    }
}
