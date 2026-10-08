//! Forward grade (`tengu evidence grade`, `docs/lineage-2026-10-06.md` § 3):
//! a paper ledger's raw rows ([`LedgerRows`], read-only, any schema since
//! binary 6fcb455) → per account the trades, totals, risk verdicts and the
//! reconciliation checks ([`grade_account`]). Pure; the reader is
//! `adapters/outbound/evidence/ledger_reader.rs`. A future
//! `ledger:<account>` lineage result source calls [`grade_account`] and
//! reads `totals` (`n`, `mean_net_bps`, `net_usd`). Ids in full.
//!
//! | Piece | Rule |
//! |---|---|
//! | Trade | per instrument, fills in `(ts_ms, id)` order: flat → open (side = the fill's) · same side adds · opposite side reduces · back to flat closes it · past flat (a flip) closes it and opens the rest; a flip fill's fee splits by quantity |
//! | Entry / exit | VWAP and notional of the opening / reducing fills; `qty` = the largest size held |
//! | Realized | the ledger's `fills.realized_pnl_usd` of the reducing fills (recomputed at average cost by check `fills_chain`) |
//! | Funding | `funding` rows of the instrument with `opened_ms ≤ hour_ms ≤ closed_ms` (open trade: no upper bound); `payment_usd` positive = paid (`domain/xm/ledger.rs`); a row in no trade is `unassigned` |
//! | Net | realized − fees − funding paid, USD; bps of the filled entry notional (= the `xm_weekend/1` row's per-name P&L) |
//! | Totals | Σ over trades; `mean_net_bps`, `positive`, `hit_rate` over closed trades; missing = `None`, never 0 |
//!
//! | # | Check (PASS / FAIL, numbers in `detail`) | Rule |
//! |---|---|---|
//! | 1 | `cash_sum` | Σ non-deposit cash rows = final balance − initial cash; every balance = the previous + its amount; Σ deposits = initial cash |
//! | 2 | `trades_vs_cash` | Σ trade net = final balance − initial cash; no unassigned funding row |
//! | 3 | `funding_formula` | every funding row: payment = qty × oracle × rate (relative 1e-9) |
//! | 4 | `flat_at_end` | every instrument flat after its last fill, every `positions` row qty 0, no `funding_owed` row |
//! | 5 | `risk_verdicts` | every order has its verdict row (`decision_id`, same account + client order id); every order that filled an `allow` |
//! | 6 | `cash_journal` | every fill has its cash row (ref = client order id, amount = realized − fee), every funding row its cash row (ref = `<instrument>@<hour_ms>`, amount = −payment), no other cash row but deposits |
//! | 7 | `fills_chain` | each fill's `qty_before` / `qty_after` = the running size; its realized = closed × (px − average cost) × sign |
//! | 8 | `orders_vs_fills` | each order's `filled_qty`, `avg_px`, `fee_usd` = its fills'; every fill names an order of the account |
//! | 9 | `positions_table` | each `positions` row's realized / fees / funding = Σ its fills / funding rows |
//! | 10 | `chronology` | verdict ts ≤ order ts ≤ each fill ts (decisions never after their execution) |
//! | 11 | `funding_qty` | every funding row's `qty` = the signed size the fills hold at its hour (`held_at`: after the fills before the hour — the ledger settles before a fill; flat then and opened on the hour: after the fills at it), relative 1e-9 |
//! | 12 | `funding_hours` | every hour boundary a trade held — `opened ≤ h ≤ closed` (an open trade: up to its last settled hour), after the hours an earlier trade of the instrument settled (`ledger.rs::due_funding_hours`) — has a `funding` or `funding_owed` row; no hour booked twice |
//!
//! Tolerance: `|a − b| ≤ 1e-9 × max(1, |a|, |b|)` for USD sums, `≤ 1e-9 ×
//! max(|a|, |b|)` (or 1e-15) for check 3.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::book::Side;

/// Relative tolerance of every reconciliation.
pub const REL_TOL: f64 = 1e-9;

// ── Raw rows (schema of binary 6fcb455; later columns not read) ─────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRow {
    pub account: String,
    pub initial_cash_usd: f64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CashRow {
    pub id: i64,
    pub account: String,
    pub ts_ms: i64,
    /// `deposit` · `fill` · `funding`.
    pub kind: String,
    /// `ref`: client order id, `<instrument>@<hour_ms>`, `initial`.
    pub reference: String,
    pub amount_usd: f64,
    pub balance_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRow {
    pub id: i64,
    pub account: String,
    pub client_order_id: String,
    pub call_id: Option<String>,
    pub decision_id: i64,
    pub ts_ms: i64,
    pub instrument: String,
    pub side: String,
    pub kind: String,
    pub reduce_only: bool,
    pub status: String,
    pub reason: Option<String>,
    pub filled_qty: f64,
    pub avg_px: Option<f64>,
    pub fee_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillRow {
    pub id: i64,
    pub account: String,
    pub order_id: i64,
    pub client_order_id: String,
    pub ts_ms: i64,
    pub instrument: String,
    pub side: String,
    pub qty: f64,
    pub px: f64,
    pub fee_usd: f64,
    pub realized_pnl_usd: f64,
    pub qty_before: f64,
    pub qty_after: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingRow {
    pub account: String,
    pub instrument: String,
    pub hour_ms: i64,
    pub rate_1h: f64,
    pub oracle_px: f64,
    /// Signed size held at the hour.
    pub qty: f64,
    /// Positive = paid.
    pub payment_usd: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionRow {
    pub account: String,
    pub instrument: String,
    pub qty: f64,
    pub realized_pnl_usd: f64,
    pub fees_usd: f64,
    /// Positive = paid.
    pub funding_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskRow {
    pub id: i64,
    pub ts_ms: i64,
    pub account: String,
    pub client_order_id: String,
    pub instrument: String,
    pub class: String,
    pub allow: bool,
    pub rule: String,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OwedRow {
    pub account: String,
    pub instrument: String,
    pub hour_ms: i64,
    pub qty: f64,
}

/// Every row of a ledger the grade reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerRows {
    pub accounts: Vec<AccountRow>,
    pub cash: Vec<CashRow>,
    pub orders: Vec<OrderRow>,
    pub fills: Vec<FillRow>,
    pub funding: Vec<FundingRow>,
    pub positions: Vec<PositionRow>,
    pub risk_decisions: Vec<RiskRow>,
    /// `None`: the ledger has no `funding_owed` table (binary 6fcb455).
    pub funding_owed: Option<Vec<OwedRow>>,
}

// ── Grade ───────────────────────────────────────────────────────────

/// One position from flat to flat (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub instrument: String,
    pub side: Side,
    /// Largest size held, base units.
    pub qty: f64,
    pub opened_ms: i64,
    pub closed_ms: Option<i64>,
    pub entry_vwap: f64,
    pub exit_vwap: Option<f64>,
    pub entry_notional_usd: f64,
    pub exit_notional_usd: f64,
    pub realized_usd: f64,
    pub fees_usd: f64,
    /// Positive = paid, negative = received.
    pub funding_paid_usd: f64,
    pub funding_hours: usize,
    pub net_usd: f64,
    /// Net over the filled entry notional; `None` when that is 0.
    pub net_bps: Option<f64>,
    pub hold_ms: Option<i64>,
    pub entry_orders: Vec<String>,
    pub exit_orders: Vec<String>,
}

/// Σ over an account's trades (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub trades: usize,
    pub closed: usize,
    pub open: usize,
    pub gross_usd: f64,
    pub fees_usd: f64,
    pub funding_paid_usd: f64,
    pub net_usd: f64,
    pub entry_notional_usd: f64,
    pub mean_net_bps: Option<f64>,
    pub positive: usize,
    pub hit_rate: Option<f64>,
    /// Funding rows in no trade (check 2 fails on any).
    pub unassigned_funding_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    Pass,
    Fail,
}

/// One reconciliation (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub check: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    fn new(check: &str, problems: Vec<String>, ok_detail: String) -> Self {
        let status = if problems.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        };
        let detail = if problems.is_empty() {
            ok_detail
        } else {
            let n = problems.len();
            let mut shown: Vec<String> = problems.into_iter().take(10).collect();
            if n > shown.len() {
                shown.push(format!("… {} more", n - shown.len()));
            }
            format!("{ok_detail}; FAIL: {}", shown.join("; "))
        };
        Self {
            check: check.to_string(),
            status,
            detail,
        }
    }
}

/// Risk verdicts of an account by class, allow, rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictCount {
    pub class: String,
    pub allow: bool,
    pub rule: String,
    pub n: usize,
}

/// One account's grade (module tables).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountGrade {
    pub account: String,
    pub initial_cash_usd: f64,
    /// The last cash row's balance; `None` without cash rows.
    pub final_balance_usd: Option<f64>,
    /// final − initial; `None` without cash rows.
    pub pnl_usd: Option<f64>,
    pub orders: usize,
    pub orders_by_status: BTreeMap<String, usize>,
    pub fills: usize,
    pub funding_rows: usize,
    pub trades: Vec<Trade>,
    pub totals: Totals,
    pub verdicts: Vec<VerdictCount>,
    pub checks: Vec<Check>,
}

impl AccountGrade {
    /// Every check passed.
    pub fn reconciled(&self) -> bool {
        self.checks.iter().all(|c| c.status == CheckStatus::Pass)
    }
}

fn close_usd(a: f64, b: f64) -> bool {
    (a - b).abs() <= REL_TOL * 1f64.max(a.abs()).max(b.abs())
}

fn close_rel(a: f64, b: f64) -> bool {
    let d = (a - b).abs();
    d <= 1e-15 || d <= REL_TOL * a.abs().max(b.abs())
}

/// Sum in a fixed order (the rows' order).
fn sum(xs: impl Iterator<Item = f64>) -> f64 {
    xs.fold(0.0, |a, x| a + x)
}

struct Acc {
    side: Side,
    opened_ms: i64,
    entry_qty: f64,
    entry_ntl: f64,
    exit_qty: f64,
    exit_ntl: f64,
    realized: f64,
    fees: f64,
    max_qty: f64,
    /// Lowest and highest fill price per side: one price ⇒ the VWAP is it, exactly.
    entry_px: (f64, f64),
    exit_px: (f64, f64),
    entry_orders: Vec<String>,
    exit_orders: Vec<String>,
}

impl Acc {
    fn open(side: Side, ts_ms: i64) -> Self {
        Self {
            side,
            opened_ms: ts_ms,
            entry_qty: 0.0,
            entry_ntl: 0.0,
            exit_qty: 0.0,
            exit_ntl: 0.0,
            realized: 0.0,
            fees: 0.0,
            max_qty: 0.0,
            entry_px: (f64::INFINITY, f64::NEG_INFINITY),
            exit_px: (f64::INFINITY, f64::NEG_INFINITY),
            entry_orders: Vec::new(),
            exit_orders: Vec::new(),
        }
    }

    fn finish(self, instrument: &str, closed_ms: Option<i64>) -> Trade {
        let vwap = |ntl: f64, qty: f64, (lo, hi): (f64, f64)| {
            if lo == hi {
                lo
            } else {
                ntl / qty
            }
        };
        let entry_vwap = if self.entry_qty > 0.0 {
            vwap(self.entry_ntl, self.entry_qty, self.entry_px)
        } else {
            f64::NAN
        };
        Trade {
            instrument: instrument.to_string(),
            side: self.side,
            qty: self.max_qty,
            opened_ms: self.opened_ms,
            closed_ms,
            entry_vwap,
            exit_vwap: (self.exit_qty > 0.0)
                .then(|| vwap(self.exit_ntl, self.exit_qty, self.exit_px)),
            entry_notional_usd: self.entry_ntl,
            exit_notional_usd: self.exit_ntl,
            realized_usd: self.realized,
            fees_usd: self.fees,
            funding_paid_usd: 0.0,
            funding_hours: 0,
            net_usd: 0.0,
            net_bps: None,
            hold_ms: closed_ms.map(|c| c - self.opened_ms),
            entry_orders: self.entry_orders,
            exit_orders: self.exit_orders,
        }
    }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// Trades of one instrument's fills (sorted) + `fills_chain` problems.
fn instrument_trades(
    instrument: &str,
    fills: &[&FillRow],
    problems: &mut Vec<String>,
) -> Vec<Trade> {
    let mut trades = Vec::new();
    let mut qty = 0.0f64;
    let mut avg = 0.0f64;
    let mut cur: Option<Acc> = None;
    for f in fills {
        let Some(side) = Side::parse(&f.side) else {
            problems.push(format!("fill {}: side `{}`", f.id, f.side));
            continue;
        };
        let s = side.sign();
        let scale = f.qty.abs().max(qty.abs()).max(1.0);
        let eps = REL_TOL * scale;
        if (f.qty_before - qty).abs() > eps {
            problems.push(format!(
                "fill {} {}: qty_before {} ≠ running size {}",
                f.id, instrument, f.qty_before, qty
            ));
        }
        let mut left = f.qty;
        let mut realized_re = 0.0;
        if qty.abs() > eps && qty.signum() != s {
            // Reduce (and maybe flip).
            let closed = left.min(qty.abs());
            realized_re = closed * (f.px - avg) * qty.signum();
            let share = if f.qty > 0.0 { closed / f.qty } else { 1.0 };
            let acc = cur.as_mut().expect("an open trade while not flat");
            acc.exit_qty += closed;
            acc.exit_ntl += closed * f.px;
            acc.exit_px = (acc.exit_px.0.min(f.px), acc.exit_px.1.max(f.px));
            acc.realized += f.realized_pnl_usd;
            acc.fees += f.fee_usd * share;
            push_unique(&mut acc.exit_orders, &f.client_order_id);
            qty += s * closed;
            left -= closed;
            if qty.abs() <= eps {
                qty = 0.0;
                avg = 0.0;
                trades.push(cur.take().expect("open").finish(instrument, Some(f.ts_ms)));
            }
            if left > eps {
                let mut acc = Acc::open(side, f.ts_ms);
                acc.entry_qty = left;
                acc.entry_ntl = left * f.px;
                acc.entry_px = (f.px, f.px);
                acc.fees = f.fee_usd * (1.0 - share);
                acc.max_qty = left;
                push_unique(&mut acc.entry_orders, &f.client_order_id);
                qty = s * left;
                avg = f.px;
                cur = Some(acc);
            }
        } else {
            // Open or add.
            let acc = cur.get_or_insert_with(|| Acc::open(side, f.ts_ms));
            let held = qty.abs();
            avg = (avg * held + f.px * left) / (held + left);
            acc.entry_qty += left;
            acc.entry_ntl += left * f.px;
            acc.entry_px = (acc.entry_px.0.min(f.px), acc.entry_px.1.max(f.px));
            acc.fees += f.fee_usd;
            push_unique(&mut acc.entry_orders, &f.client_order_id);
            qty += s * left;
            acc.max_qty = acc.max_qty.max(qty.abs());
        }
        if (f.qty_after - qty).abs() > eps {
            problems.push(format!(
                "fill {} {}: qty_after {} ≠ running size {}",
                f.id, instrument, f.qty_after, qty
            ));
        }
        if !close_usd(f.realized_pnl_usd, realized_re) {
            problems.push(format!(
                "fill {} {}: realized {} ≠ average-cost {}",
                f.id, instrument, f.realized_pnl_usd, realized_re
            ));
        }
    }
    if let Some(acc) = cur {
        trades.push(acc.finish(instrument, None));
    }
    trades
}

/// Grade `account` from `rows` (module tables). `Err` when the ledger has
/// no such account.
pub fn grade_account(rows: &LedgerRows, account: &str) -> Result<AccountGrade, String> {
    let acct = rows
        .accounts
        .iter()
        .find(|a| a.account == account)
        .ok_or_else(|| {
            let known: Vec<&str> = rows.accounts.iter().map(|a| a.account.as_str()).collect();
            format!(
                "no account `{account}` in the ledger (accounts: {})",
                known.join(", ")
            )
        })?;
    let mine = |a: &str| a == account;
    let mut cash: Vec<&CashRow> = rows.cash.iter().filter(|r| mine(&r.account)).collect();
    cash.sort_by_key(|r| r.id);
    let mut orders: Vec<&OrderRow> = rows.orders.iter().filter(|r| mine(&r.account)).collect();
    orders.sort_by_key(|r| r.id);
    let mut fills: Vec<&FillRow> = rows.fills.iter().filter(|r| mine(&r.account)).collect();
    fills.sort_by_key(|r| (r.ts_ms, r.id));
    let mut funding: Vec<&FundingRow> = rows.funding.iter().filter(|r| mine(&r.account)).collect();
    funding.sort_by(|a, b| {
        (a.instrument.as_str(), a.hour_ms).cmp(&(b.instrument.as_str(), b.hour_ms))
    });
    let positions: Vec<&PositionRow> = rows.positions.iter().filter(|r| mine(&r.account)).collect();
    let risk: Vec<&RiskRow> = rows
        .risk_decisions
        .iter()
        .filter(|r| mine(&r.account))
        .collect();
    let owed: Vec<&OwedRow> = rows
        .funding_owed
        .iter()
        .flatten()
        .filter(|r| mine(&r.account))
        .collect();

    // Trades.
    let mut by_instrument: BTreeMap<&str, Vec<&FillRow>> = BTreeMap::new();
    for f in &fills {
        by_instrument
            .entry(f.instrument.as_str())
            .or_default()
            .push(f);
    }
    let mut chain_problems = Vec::new();
    let mut trades = Vec::new();
    for (instrument, fs) in &by_instrument {
        trades.extend(instrument_trades(instrument, fs, &mut chain_problems));
    }
    let mut unassigned = Vec::new();
    for r in &funding {
        let hit = trades.iter_mut().find(|t| {
            t.instrument == r.instrument
                && t.opened_ms <= r.hour_ms
                && t.closed_ms.map_or(true, |c| r.hour_ms <= c)
        });
        match hit {
            Some(t) => {
                t.funding_paid_usd += r.payment_usd;
                t.funding_hours += 1;
            }
            None => unassigned.push(*r),
        }
    }
    for t in &mut trades {
        t.net_usd = t.realized_usd - t.fees_usd - t.funding_paid_usd;
        t.net_bps = (t.entry_notional_usd > 0.0).then(|| t.net_usd / t.entry_notional_usd * 1e4);
    }
    trades.sort_by(|a, b| {
        (a.opened_ms, a.instrument.as_str()).cmp(&(b.opened_ms, b.instrument.as_str()))
    });

    let closed: Vec<&Trade> = trades.iter().filter(|t| t.closed_ms.is_some()).collect();
    let closed_bps: Vec<f64> = closed.iter().filter_map(|t| t.net_bps).collect();
    let positive = closed.iter().filter(|t| t.net_usd > 0.0).count();
    let totals = Totals {
        trades: trades.len(),
        closed: closed.len(),
        open: trades.len() - closed.len(),
        gross_usd: sum(trades.iter().map(|t| t.realized_usd)),
        fees_usd: sum(trades.iter().map(|t| t.fees_usd)),
        funding_paid_usd: sum(trades.iter().map(|t| t.funding_paid_usd)),
        net_usd: sum(trades.iter().map(|t| t.net_usd)),
        entry_notional_usd: sum(trades.iter().map(|t| t.entry_notional_usd)),
        mean_net_bps: (!closed_bps.is_empty())
            .then(|| sum(closed_bps.iter().copied()) / closed_bps.len() as f64),
        positive,
        hit_rate: (!closed.is_empty()).then(|| positive as f64 / closed.len() as f64),
        unassigned_funding_rows: unassigned.len(),
    };

    let final_balance = cash.last().map(|r| r.balance_usd);
    let pnl = final_balance.map(|b| b - acct.initial_cash_usd);
    let mut checks = Vec::new();

    // 1 cash_sum.
    {
        let mut p = Vec::new();
        let mut prev: Option<f64> = None;
        for r in &cash {
            let expect = prev.unwrap_or(0.0) + r.amount_usd;
            if !close_usd(r.balance_usd, expect) {
                p.push(format!(
                    "cash {}: balance {} ≠ previous + amount {}",
                    r.id, r.balance_usd, expect
                ));
            }
            prev = Some(r.balance_usd);
        }
        let deposits = sum(cash
            .iter()
            .filter(|r| r.kind == "deposit")
            .map(|r| r.amount_usd));
        let others = sum(cash
            .iter()
            .filter(|r| r.kind != "deposit")
            .map(|r| r.amount_usd));
        if !close_usd(deposits, acct.initial_cash_usd) {
            p.push(format!(
                "Σ deposits {deposits} ≠ initial cash {}",
                acct.initial_cash_usd
            ));
        }
        match pnl {
            Some(pnl) if !close_usd(others, pnl) => p.push(format!(
                "Σ non-deposit cash {others} ≠ final − initial {pnl}"
            )),
            None => p.push("no cash rows".into()),
            _ => {}
        }
        checks.push(Check::new(
            "cash_sum",
            p,
            format!(
                "{} cash rows; Σ non-deposit {others}; final {} − initial {} = {}",
                cash.len(),
                fmt_opt(final_balance),
                acct.initial_cash_usd,
                fmt_opt(pnl)
            ),
        ));
    }
    // 2 trades_vs_cash.
    {
        let mut p = Vec::new();
        match pnl {
            Some(pnl) if !close_usd(totals.net_usd, pnl) => p.push(format!(
                "Σ trade net {} ≠ final − initial {pnl} (diff {})",
                totals.net_usd,
                totals.net_usd - pnl
            )),
            None => p.push("no cash rows".into()),
            _ => {}
        }
        for r in &unassigned {
            p.push(format!(
                "funding {}@{} in no trade (payment {})",
                r.instrument, r.hour_ms, r.payment_usd
            ));
        }
        checks.push(Check::new(
            "trades_vs_cash",
            p,
            format!(
                "Σ trade net {} over {} trades; final − initial {}",
                totals.net_usd,
                totals.trades,
                fmt_opt(pnl)
            ),
        ));
    }
    // 3 funding_formula.
    {
        let mut p = Vec::new();
        for r in &funding {
            let expect = r.qty * r.oracle_px * r.rate_1h;
            if !close_rel(r.payment_usd, expect) {
                p.push(format!(
                    "{}@{}: payment {} ≠ qty {} × oracle {} × rate {} = {expect}",
                    r.instrument, r.hour_ms, r.payment_usd, r.qty, r.oracle_px, r.rate_1h
                ));
            }
        }
        checks.push(Check::new(
            "funding_formula",
            p,
            format!(
                "{} funding rows; Σ payment {} (positive = paid)",
                funding.len(),
                sum(funding.iter().map(|r| r.payment_usd))
            ),
        ));
    }
    // 4 flat_at_end.
    {
        let mut p = Vec::new();
        for (instrument, fs) in &by_instrument {
            if let Some(last) = fs.last() {
                if last.qty_after.abs() > REL_TOL * last.qty.abs().max(1.0) {
                    p.push(format!(
                        "{instrument}: qty after the last fill {}",
                        last.qty_after
                    ));
                }
            }
        }
        for r in &positions {
            if r.qty != 0.0 {
                p.push(format!("positions {}: qty {}", r.instrument, r.qty));
            }
        }
        for r in &owed {
            p.push(format!(
                "funding owed {}@{} qty {}",
                r.instrument, r.hour_ms, r.qty
            ));
        }
        let owed_note = match rows.funding_owed {
            Some(_) => format!("{} funding_owed rows", owed.len()),
            None => "no funding_owed table (old schema)".to_string(),
        };
        checks.push(Check::new(
            "flat_at_end",
            p,
            format!(
                "{} instruments traded, {} positions rows, {owed_note}",
                by_instrument.len(),
                positions.len()
            ),
        ));
    }
    // 5 risk_verdicts.
    let risk_by_id: BTreeMap<i64, &RiskRow> =
        rows.risk_decisions.iter().map(|r| (r.id, r)).collect();
    {
        let mut p = Vec::new();
        let mut filled = 0usize;
        for o in &orders {
            let did_fill = o.filled_qty > 0.0;
            filled += usize::from(did_fill);
            match risk_by_id.get(&o.decision_id) {
                None => p.push(format!(
                    "order {} `{}`: no verdict row {}",
                    o.id, o.client_order_id, o.decision_id
                )),
                Some(r) => {
                    if r.account != o.account || r.client_order_id != o.client_order_id {
                        p.push(format!(
                            "order {} `{}`: verdict {} is for {} `{}`",
                            o.id, o.client_order_id, r.id, r.account, r.client_order_id
                        ));
                    }
                    if did_fill && !r.allow {
                        p.push(format!(
                            "order {} `{}` filled under a deny (rule {})",
                            o.id, o.client_order_id, r.rule
                        ));
                    }
                }
            }
        }
        checks.push(Check::new(
            "risk_verdicts",
            p,
            format!(
                "{} orders ({filled} filled), {} verdict rows of the account",
                orders.len(),
                risk.len()
            ),
        ));
    }
    // 6 cash_journal.
    {
        let mut p = Vec::new();
        let mut used: BTreeSet<i64> = BTreeSet::new();
        for f in &fills {
            let want = f.realized_pnl_usd - f.fee_usd;
            let hit = cash.iter().find(|c| {
                !used.contains(&c.id)
                    && c.kind == "fill"
                    && c.reference == f.client_order_id
                    && c.ts_ms == f.ts_ms
            });
            match hit {
                Some(c) => {
                    used.insert(c.id);
                    if !close_usd(c.amount_usd, want) {
                        p.push(format!(
                            "fill {}: cash {} amount {} ≠ realized − fee {want}",
                            f.id, c.id, c.amount_usd
                        ));
                    }
                }
                None => p.push(format!(
                    "fill {} `{}`: no cash row",
                    f.id, f.client_order_id
                )),
            }
        }
        for r in &funding {
            let reference = format!("{}@{}", r.instrument, r.hour_ms);
            let hit = cash
                .iter()
                .find(|c| !used.contains(&c.id) && c.kind == "funding" && c.reference == reference);
            match hit {
                Some(c) => {
                    used.insert(c.id);
                    if !close_usd(c.amount_usd, -r.payment_usd) {
                        p.push(format!(
                            "funding {reference}: cash {} amount {} ≠ −payment {}",
                            c.id, c.amount_usd, -r.payment_usd
                        ));
                    }
                }
                None => p.push(format!("funding {reference}: no cash row")),
            }
        }
        for c in &cash {
            if c.kind != "deposit" && !used.contains(&c.id) {
                p.push(format!(
                    "cash {} {} `{}` {}: no fill / funding row",
                    c.id, c.kind, c.reference, c.amount_usd
                ));
            }
        }
        checks.push(Check::new(
            "cash_journal",
            p,
            format!(
                "{} fills + {} funding rows ↔ {} non-deposit cash rows",
                fills.len(),
                funding.len(),
                cash.iter().filter(|c| c.kind != "deposit").count()
            ),
        ));
    }
    // 7 fills_chain.
    checks.push(Check::new(
        "fills_chain",
        chain_problems,
        format!(
            "{} fills over {} instruments",
            fills.len(),
            by_instrument.len()
        ),
    ));
    // 8 orders_vs_fills.
    {
        let mut p = Vec::new();
        let order_ids: BTreeMap<i64, &OrderRow> = orders.iter().map(|o| (o.id, *o)).collect();
        let mut per_order: BTreeMap<i64, Vec<&FillRow>> = BTreeMap::new();
        for f in &fills {
            match order_ids.get(&f.order_id) {
                Some(o) if o.client_order_id == f.client_order_id => {
                    per_order.entry(f.order_id).or_default().push(f)
                }
                Some(o) => p.push(format!(
                    "fill {}: order {} is `{}`, the fill says `{}`",
                    f.id, f.order_id, o.client_order_id, f.client_order_id
                )),
                None => p.push(format!(
                    "fill {}: no order {} in the account",
                    f.id, f.order_id
                )),
            }
        }
        for o in &orders {
            let fs = per_order.get(&o.id).map(Vec::as_slice).unwrap_or(&[]);
            let q = sum(fs.iter().map(|f| f.qty));
            let ntl = sum(fs.iter().map(|f| f.qty * f.px));
            let fee = sum(fs.iter().map(|f| f.fee_usd));
            if !close_usd(q, o.filled_qty) {
                p.push(format!(
                    "order {}: filled_qty {} ≠ Σ fills {q}",
                    o.id, o.filled_qty
                ));
            }
            if !close_usd(fee, o.fee_usd) {
                p.push(format!("order {}: fee {} ≠ Σ fills {fee}", o.id, o.fee_usd));
            }
            match (o.avg_px, q > 0.0) {
                (Some(px), true) if !close_usd(px, ntl / q) => p.push(format!(
                    "order {}: avg_px {px} ≠ fills VWAP {}",
                    o.id,
                    ntl / q
                )),
                (None, true) => p.push(format!("order {}: filled without avg_px", o.id)),
                _ => {}
            }
        }
        checks.push(Check::new(
            "orders_vs_fills",
            p,
            format!("{} orders, {} fills", orders.len(), fills.len()),
        ));
    }
    // 9 positions_table.
    {
        let mut p = Vec::new();
        for r in &positions {
            let fs = by_instrument
                .get(r.instrument.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let realized = sum(fs.iter().map(|f| f.realized_pnl_usd));
            let fees = sum(fs.iter().map(|f| f.fee_usd));
            let paid = sum(funding
                .iter()
                .filter(|f| f.instrument == r.instrument)
                .map(|f| f.payment_usd));
            for (what, stored, rows_sum) in [
                ("realized", r.realized_pnl_usd, realized),
                ("fees", r.fees_usd, fees),
                ("funding", r.funding_usd, paid),
            ] {
                if !close_usd(stored, rows_sum) {
                    p.push(format!(
                        "positions {}: {what} {stored} ≠ Σ rows {rows_sum}",
                        r.instrument
                    ));
                }
            }
        }
        for instrument in by_instrument.keys() {
            if !positions.iter().any(|r| r.instrument == *instrument) {
                p.push(format!("{instrument}: fills but no positions row"));
            }
        }
        checks.push(Check::new(
            "positions_table",
            p,
            format!("{} positions rows", positions.len()),
        ));
    }
    // 10 chronology.
    {
        let mut p = Vec::new();
        for o in &orders {
            if let Some(r) = risk_by_id.get(&o.decision_id) {
                if r.ts_ms > o.ts_ms {
                    p.push(format!(
                        "order {}: verdict at {} after the order at {}",
                        o.id, r.ts_ms, o.ts_ms
                    ));
                }
            }
        }
        for f in &fills {
            if let Some(o) = orders.iter().find(|o| o.id == f.order_id) {
                if o.ts_ms > f.ts_ms {
                    p.push(format!(
                        "fill {}: order {} at {} after the fill at {}",
                        f.id, o.id, o.ts_ms, f.ts_ms
                    ));
                }
            }
        }
        checks.push(Check::new(
            "chronology",
            p,
            "verdict ≤ order ≤ fill timestamps".to_string(),
        ));
    }
    // 11 funding_qty.
    {
        let mut p = Vec::new();
        for r in &funding {
            let fs = by_instrument
                .get(r.instrument.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let held = held_at(fs, r.hour_ms);
            let tol = REL_TOL * 1f64.max(held.abs()).max(r.qty.abs());
            if (r.qty - held).abs() > tol {
                p.push(format!(
                    "{}@{}: qty {} ≠ {held} held at the hour (fills chain)",
                    r.instrument, r.hour_ms, r.qty
                ));
            }
        }
        checks.push(Check::new(
            "funding_qty",
            p,
            format!(
                "{} funding rows against the size the fills hold at their hour",
                funding.len()
            ),
        ));
    }
    // 12 funding_hours.
    {
        let (p, detail) = funding_hour_problems(&trades, &funding, &owed);
        checks.push(Check::new("funding_hours", p, detail));
    }

    let mut verdict_map: BTreeMap<(String, bool, String), usize> = BTreeMap::new();
    for r in &risk {
        *verdict_map
            .entry((r.class.clone(), r.allow, r.rule.clone()))
            .or_default() += 1;
    }
    let mut orders_by_status: BTreeMap<String, usize> = BTreeMap::new();
    for o in &orders {
        *orders_by_status.entry(o.status.clone()).or_default() += 1;
    }
    Ok(AccountGrade {
        account: account.to_string(),
        initial_cash_usd: acct.initial_cash_usd,
        final_balance_usd: final_balance,
        pnl_usd: pnl,
        orders: orders.len(),
        orders_by_status,
        fills: fills.len(),
        funding_rows: funding.len(),
        trades,
        totals,
        verdicts: verdict_map
            .into_iter()
            .map(|((class, allow, rule), n)| VerdictCount {
                class,
                allow,
                rule,
                n,
            })
            .collect(),
        checks,
    })
}

/// Funding settles on the hour (HL, `domain/xm/ledger.rs`).
const HOUR_MS: i64 = 3_600_000;

/// The signed size the fills (sorted) hold at hour boundary `hour_ms`, as the
/// ledger settles it (`Position::settle_funding` runs before any fill): the
/// size after every fill before the hour; flat then and opened exactly on
/// the hour, the size after the fills at it (`due_funding_hours`: an hour
/// on `opened_ms` is due).
fn held_at(fills: &[&FillRow], hour_ms: i64) -> f64 {
    let signed = |f: &&FillRow| Side::parse(&f.side).map_or(0.0, |s| s.sign()) * f.qty;
    let before = sum(fills.iter().filter(|f| f.ts_ms < hour_ms).map(signed));
    let eps = REL_TOL * fills.iter().map(|f| f.qty.abs()).fold(1.0, f64::max);
    if before.abs() > eps {
        before
    } else {
        sum(fills.iter().filter(|f| f.ts_ms <= hour_ms).map(signed))
    }
}

/// Check 12 (module table): the hours each trade must have settled — every
/// boundary `h` with `opened ≤ h ≤ closed` (an open trade: up to its last
/// settled hour), after the hours an earlier trade of the instrument settled
/// (`due_funding_hours`: `last_funding_hour_ms`) — each booked once
/// (`funding`) or owed (`funding_owed`).
fn funding_hour_problems(
    trades: &[Trade],
    funding: &[&FundingRow],
    owed: &[&OwedRow],
) -> (Vec<String>, String) {
    let mut p = Vec::new();
    let mut booked: BTreeMap<(&str, i64), usize> = BTreeMap::new();
    for r in funding {
        *booked
            .entry((r.instrument.as_str(), r.hour_ms))
            .or_default() += 1;
    }
    for ((inst, h), n) in &booked {
        if *n > 1 {
            p.push(format!("{inst}@{h}: booked {n} times"));
        }
    }
    let owed_hours: BTreeSet<(&str, i64)> = owed
        .iter()
        .map(|r| (r.instrument.as_str(), r.hour_ms))
        .collect();
    let mut by_instrument: BTreeMap<&str, Vec<&Trade>> = BTreeMap::new();
    for t in trades {
        by_instrument
            .entry(t.instrument.as_str())
            .or_default()
            .push(t);
    }
    let (mut expected, mut covered_owed) = (0usize, 0usize);
    for (inst, mut ts) in by_instrument {
        ts.sort_by_key(|t| t.opened_ms);
        let settled_hours: Vec<i64> = booked
            .keys()
            .chain(owed_hours.iter())
            .filter(|(i, _)| *i == inst)
            .map(|(_, h)| *h)
            .collect();
        let mut next_free: Option<i64> = None;
        for t in ts {
            let ceil = t.opened_ms.div_euclid(HOUR_MS) * HOUR_MS
                + if t.opened_ms.rem_euclid(HOUR_MS) == 0 {
                    0
                } else {
                    HOUR_MS
                };
            let first = next_free.map_or(ceil, |n| ceil.max(n));
            let last = match t.closed_ms {
                Some(c) => c.div_euclid(HOUR_MS) * HOUR_MS,
                None => match settled_hours.iter().filter(|h| **h >= first).max() {
                    Some(h) => *h,
                    None => continue,
                },
            };
            let mut h = first;
            while h <= last {
                expected += 1;
                if owed_hours.contains(&(inst, h)) {
                    covered_owed += 1;
                } else if !booked.contains_key(&(inst, h)) {
                    p.push(format!(
                        "{inst}@{h}: held from {} to {} but no funding or funding_owed row",
                        t.opened_ms,
                        t.closed_ms.map_or("OPEN".to_string(), |c| c.to_string())
                    ));
                }
                next_free = Some(h + HOUR_MS);
                h += HOUR_MS;
            }
        }
    }
    let detail = format!(
        "{expected} settlement hours held, {covered_owed} owed, {} funding rows",
        funding.len()
    );
    (p, detail)
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map_or_else(|| "MISSING".to_string(), |x| x.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "acct";
    const X: &str = "hyperliquid:xyz:AAA";

    #[allow(clippy::too_many_arguments)]
    fn fill(
        id: i64,
        ts: i64,
        side: &str,
        qty: f64,
        px: f64,
        fee: f64,
        realized: f64,
        before: f64,
        after: f64,
    ) -> FillRow {
        FillRow {
            id,
            account: A.into(),
            order_id: id,
            client_order_id: format!("o{id}"),
            ts_ms: ts,
            instrument: X.into(),
            side: side.into(),
            qty,
            px,
            fee_usd: fee,
            realized_pnl_usd: realized,
            qty_before: before,
            qty_after: after,
        }
    }

    fn order(f: &FillRow) -> OrderRow {
        OrderRow {
            id: f.id,
            account: A.into(),
            client_order_id: f.client_order_id.clone(),
            call_id: None,
            decision_id: f.id,
            ts_ms: f.ts_ms,
            instrument: X.into(),
            side: f.side.clone(),
            kind: "market".into(),
            reduce_only: false,
            status: "filled".into(),
            reason: None,
            filled_qty: f.qty,
            avg_px: Some(f.px),
            fee_usd: f.fee_usd,
        }
    }

    fn verdict(f: &FillRow, allow: bool) -> RiskRow {
        RiskRow {
            id: f.id,
            ts_ms: f.ts_ms,
            account: A.into(),
            client_order_id: f.client_order_id.clone(),
            instrument: X.into(),
            class: "entry".into(),
            allow,
            rule: if allow {
                "ok".into()
            } else {
                "max_notional".into()
            },
            tool: None,
        }
    }

    /// Short 2 @ 10 (fee 0.01), funding 1 h received 0.002, cover 2 @ 9
    /// (fee 0.009): realized 2, net 2 − 0.019 + 0.002 = 1.983.
    fn tiny() -> LedgerRows {
        let f1 = fill(1, 1_000, "sell", 2.0, 10.0, 0.01, 0.0, 0.0, -2.0);
        let f2 = fill(2, 3_600_500, "buy", 2.0, 9.0, 0.009, 2.0, -2.0, 0.0);
        let fund = FundingRow {
            account: A.into(),
            instrument: X.into(),
            hour_ms: 3_600_000,
            rate_1h: 0.0001,
            oracle_px: 10.0,
            qty: -2.0,
            payment_usd: -0.002,
            ts_ms: 3_600_001,
        };
        let cash = |id: i64, ts: i64, kind: &str, r: &str, amt: f64, bal: f64| CashRow {
            id,
            account: A.into(),
            ts_ms: ts,
            kind: kind.into(),
            reference: r.into(),
            amount_usd: amt,
            balance_usd: bal,
        };
        LedgerRows {
            accounts: vec![AccountRow {
                account: A.into(),
                initial_cash_usd: 100.0,
                created_ms: 0,
            }],
            cash: vec![
                cash(1, 0, "deposit", "initial", 100.0, 100.0),
                cash(2, 1_000, "fill", "o1", -0.01, 99.99),
                cash(
                    3,
                    3_600_001,
                    "funding",
                    &format!("{X}@3600000"),
                    0.002,
                    99.992,
                ),
                cash(4, 3_600_500, "fill", "o2", 1.991, 101.983),
            ],
            orders: vec![order(&f1), order(&f2)],
            risk_decisions: vec![verdict(&f1, true), verdict(&f2, true)],
            positions: vec![PositionRow {
                account: A.into(),
                instrument: X.into(),
                qty: 0.0,
                realized_pnl_usd: 2.0,
                fees_usd: 0.019,
                funding_usd: -0.002,
            }],
            fills: vec![f1, f2],
            funding: vec![fund],
            funding_owed: None,
        }
    }

    #[test]
    fn a_tiny_ledger_grades_and_reconciles() {
        let g = grade_account(&tiny(), A).unwrap();
        assert!(g.reconciled(), "{:#?}", g.checks);
        assert_eq!(g.checks.len(), 12);
        assert_eq!(g.trades.len(), 1);
        let t = &g.trades[0];
        assert_eq!(t.side, Side::Sell);
        assert_eq!(t.qty, 2.0);
        assert_eq!(t.entry_vwap, 10.0);
        assert_eq!(t.exit_vwap, Some(9.0));
        assert_eq!(t.funding_hours, 1);
        assert!((t.net_usd - 1.983).abs() < 1e-12);
        assert!((t.net_bps.unwrap() - 1.983 / 20.0 * 1e4).abs() < 1e-9);
        assert_eq!(t.hold_ms, Some(3_599_500));
        assert!((g.pnl_usd.unwrap() - 1.983).abs() < 1e-12);
        assert_eq!(g.totals.positive, 1);
        assert_eq!(g.verdicts.len(), 1);
        assert_eq!(g.verdicts[0].n, 2);
        assert!(grade_account(&tiny(), "nobody").is_err());
    }

    #[test]
    fn a_broken_ledger_fails_the_checks_that_see_it() {
        let mut rows = tiny();
        rows.cash[3].amount_usd = 1.5; // the journal and the sum disagree
        rows.funding[0].payment_usd = -0.003; // not qty × oracle × rate
        rows.risk_decisions[1].allow = false; // filled under a deny
        rows.positions[0].qty = -1.0; // still open in the table
        let g = grade_account(&rows, A).unwrap();
        let failed: Vec<&str> = g
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .map(|c| c.check.as_str())
            .collect();
        for name in [
            "cash_sum",
            "trades_vs_cash",
            "funding_formula",
            "flat_at_end",
            "risk_verdicts",
            "cash_journal",
            "positions_table",
        ] {
            assert!(failed.contains(&name), "{name} should fail: {failed:?}");
        }
        assert!(!failed.contains(&"chronology"));
        assert!(!g.reconciled());
    }

    #[test]
    fn a_flip_splits_into_two_trades_and_funding_outside_a_trade_is_unassigned() {
        let mut rows = tiny();
        // Buy 3 @ 9 instead of 2: closes the short, opens long 1.
        rows.fills[1] = fill(2, 3_600_500, "buy", 3.0, 9.0, 0.009, 2.0, -2.0, 1.0);
        rows.funding.push(FundingRow {
            hour_ms: 100,
            ts_ms: 101,
            ..rows.funding[0].clone()
        });
        let g = grade_account(&rows, A).unwrap();
        assert_eq!(g.trades.len(), 2);
        assert_eq!(g.trades[1].side, Side::Buy);
        assert_eq!(g.trades[1].closed_ms, None);
        assert!((g.trades[0].fees_usd - (0.01 + 0.006)).abs() < 1e-12);
        assert!((g.trades[1].fees_usd - 0.003).abs() < 1e-12);
        assert_eq!(g.totals.open, 1);
        assert_eq!(g.totals.unassigned_funding_rows, 1);
        let fail = |n: &str| {
            g.checks
                .iter()
                .any(|c| c.check == n && c.status == CheckStatus::Fail)
        };
        assert!(fail("trades_vs_cash") && fail("flat_at_end"));
    }

    /// Review #7: a short booked as `qty > 0` (a received payment turned
    /// paid) with its cash row following it — every self-consistency check
    /// passes; only the fills chain shows the sign is wrong.
    #[test]
    fn a_funding_row_with_the_wrong_sign_fails_funding_qty() {
        let mut rows = tiny();
        rows.funding[0].qty = 2.0;
        rows.funding[0].payment_usd = 0.002;
        rows.cash[2].amount_usd = -0.002;
        rows.cash[2].balance_usd = 99.988;
        rows.cash[3].balance_usd = 101.979;
        rows.positions[0].funding_usd = 0.002;
        let g = grade_account(&rows, A).unwrap();
        let failed: Vec<&str> = g
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .map(|c| c.check.as_str())
            .collect();
        assert_eq!(failed, vec!["funding_qty"], "{:#?}", g.checks);
    }

    /// Review #7: a ledger that skipped a held hour (its cash journal
    /// consistent without it) fails `funding_hours`.
    #[test]
    fn a_skipped_funding_hour_fails_funding_hours() {
        let mut rows = tiny();
        // Hold across a second hour: cover at 2 h + 0.5 s instead.
        rows.fills[1].ts_ms = 7_200_500;
        rows.orders[1].ts_ms = 7_200_500;
        rows.risk_decisions[1].ts_ms = 7_200_500;
        rows.cash[3].ts_ms = 7_200_500;
        let g = grade_account(&rows, A).unwrap();
        let hours = g
            .checks
            .iter()
            .find(|c| c.check == "funding_hours")
            .unwrap();
        assert_eq!(hours.status, CheckStatus::Fail, "{hours:#?}");
        assert!(
            hours.detail.contains(&format!("{X}@7200000")),
            "{}",
            hours.detail
        );
        let others_pass = g
            .checks
            .iter()
            .filter(|c| c.check != "funding_hours")
            .all(|c| c.status == CheckStatus::Pass);
        assert!(others_pass, "{:#?}", g.checks);
        // Owed instead of booked: covered.
        rows.funding_owed = Some(vec![OwedRow {
            account: A.into(),
            instrument: X.into(),
            hour_ms: 7_200_000,
            qty: -2.0,
        }]);
        let g = grade_account(&rows, A).unwrap();
        let hours = g
            .checks
            .iter()
            .find(|c| c.check == "funding_hours")
            .unwrap();
        assert_eq!(hours.status, CheckStatus::Pass, "{hours:#?}");
    }
}
