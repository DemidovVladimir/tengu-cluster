//! Exit rules (`x-exit-rules`, PRD §25 §28 §30): when an open paper
//! position is due to close. Pure — marks and times are inputs; the exec
//! tool `xm_exits` (`adapters/outbound/tools/xm/exits.rs`) closes what
//! [`exit_due`] names through the `[risk]` gate and returns [`XmExits`]
//! (`xm_exits/1:<account>`). Knobs: `[risk.exits]` (`config/risk.rs`).
//!
//! | Reason (first due wins) | Due when | Needs a fresh mark |
//! |---|---|---|
//! | `deadline` | the position's `exit_at_ms` (set by the order that opened it: `paper_order exit_at_ms`, a strategy) ≤ now | no |
//! | `max_hold` | `opened_ms` + `max_hold_secs` ≤ now | no |
//! | `stop_loss` | P&L at mark ≤ −`stop_loss_bps` | yes |
//! | `take_profit` | P&L at mark ≥ `take_profit_bps` | yes |
//!
//! | Detail | Rule |
//! |---|---|
//! | P&L at mark | side-signed `(mark − avg_px) / avg_px × 10 000` bps; fees and funding not counted |
//! | Mark | `ledger::fresh_mark` of the `mkt_ctx/1` mark: a missing, stale or invalid mark never triggers take-profit / stop-loss; deadline and max hold still fire (the close fills against a fresh book inside the gate) |
//! | Boundaries | at the deadline / the threshold is due; P&L compared with a 1e-9 bps tolerance (f64 residue) |
//! | Order | time reasons first: once due they stay due, so a retried exit keeps its reason and its id |
//! | Idempotency key ([`exit_client_order_id`]) | `exit:<account>:<instrument>:<reason>:<opened_ms>`; attempt n ≥ 2 — the earlier id's order is stored but left the position open (rejected, partial) — appends `:<n>` |
//! | Row `xm_exits/1:<account>` (ttl 0) | `ok` nothing failed · `partial` some closes failed or a mark was stale · `error` every due close failed |

use serde::{Deserialize, Serialize};

use crate::domain::observation::{
    set_int, ErrorClass, Features, Field, ObsStatus, Observed, ReadError, MAX_LINE1_CHARS,
};
use crate::domain::xm::ledger::Position;

/// Tolerance of the take-profit / stop-loss comparison, bps.
const PNL_TOL_BPS: f64 = 1e-9;

/// `[risk.exits]`, mapped by `ExitsConfig::rules`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExitRules {
    /// P&L at mark, bps of the entry, that closes a winner (> 0).
    pub take_profit_bps: f64,
    /// Loss at mark, bps of the entry, that closes a loser (> 0).
    pub stop_loss_bps: f64,
    /// Longest a position stays open.
    pub max_hold_ms: i64,
}

/// Why a position is due (module table, in precedence order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Deadline,
    MaxHold,
    StopLoss,
    TakeProfit,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Deadline => "deadline",
            ExitReason::MaxHold => "max_hold",
            ExitReason::StopLoss => "stop_loss",
            ExitReason::TakeProfit => "take_profit",
        }
    }
}

/// Side-signed P&L of `position` at `mark_px`, bps of its entry; `None`
/// when flat, without an entry, or at an invalid px.
pub fn pnl_bps(position: &Position, mark_px: f64) -> Option<f64> {
    let avg = position.avg_px.filter(|a| a.is_finite() && *a > 0.0)?;
    if position.is_flat() || !(mark_px.is_finite() && mark_px > 0.0) {
        return None;
    }
    Some(position.qty.signum() * (mark_px - avg) / avg * 10_000.0)
}

/// The module table for one position at `now_ms`. `mark` = the position's
/// fresh mark (`ledger::fresh_mark`); `exit_at_ms` = its deadline, if any.
pub fn exit_due(
    position: &Position,
    mark: &Field<f64>,
    exit_at_ms: Option<i64>,
    now_ms: i64,
    rules: &ExitRules,
) -> Option<ExitReason> {
    if position.is_flat() {
        return None;
    }
    if exit_at_ms.is_some_and(|t| t <= now_ms) {
        return Some(ExitReason::Deadline);
    }
    if position
        .opened_ms
        .is_some_and(|o| o.saturating_add(rules.max_hold_ms) <= now_ms)
    {
        return Some(ExitReason::MaxHold);
    }
    let pnl = mark.value().and_then(|px| pnl_bps(position, *px))?;
    if pnl <= -rules.stop_loss_bps + PNL_TOL_BPS {
        Some(ExitReason::StopLoss)
    } else if pnl >= rules.take_profit_bps - PNL_TOL_BPS {
        Some(ExitReason::TakeProfit)
    } else {
        None
    }
}

/// Idempotency key of exit attempt `attempt` (1-based) for the position of
/// `instrument` opened at `opened_ms` (module table). Ids in full.
pub fn exit_client_order_id(
    account: &str,
    instrument: &str,
    reason: ExitReason,
    opened_ms: i64,
    attempt: u32,
) -> String {
    let base = format!(
        "exit:{account}:{instrument}:{}:{opened_ms}",
        reason.as_str()
    );
    if attempt <= 1 {
        base
    } else {
        format!("{base}:{attempt}")
    }
}

/// What `xm_exits` did with one open position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    /// Not due: kept open.
    Held,
    /// Closed (the reduce-only fill took the whole position).
    Filled,
    /// Partly closed; the rest is tried again next run.
    Partial,
    /// Sent, rejected by the paper venue (e.g. `stale_book`).
    Rejected,
    /// Refused by the `[risk]` gate.
    Denied,
    /// Already flat when the close ran (closed by another caller).
    Flat,
    /// Not placed (the message says why).
    Error,
}

impl ExitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitStatus::Held => "held",
            ExitStatus::Filled => "filled",
            ExitStatus::Partial => "partial",
            ExitStatus::Rejected => "rejected",
            ExitStatus::Denied => "denied",
            ExitStatus::Flat => "flat",
            ExitStatus::Error => "error",
        }
    }

    /// A due position this run did not close.
    pub fn failed(self) -> bool {
        matches!(
            self,
            ExitStatus::Partial | ExitStatus::Rejected | ExitStatus::Denied | ExitStatus::Error
        )
    }
}

/// One open position as `xm_exits` judged it (a `data` row of
/// [`XmExits`]); ids in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitCheck {
    pub instrument: String,
    /// Signed.
    pub qty: f64,
    pub avg_px: Option<f64>,
    pub opened_ms: Option<i64>,
    pub exit_at_ms: Option<i64>,
    /// The fresh mark, or why there is none (never 0).
    pub mark_px: Field<f64>,
    /// At the mark; `None` without one.
    pub pnl_bps: Option<f64>,
    /// `None` = not due.
    pub reason: Option<ExitReason>,
    pub status: ExitStatus,
    /// The close's `client_order_id` and attempt (1-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// The gate's rule (`allow_reduce_degraded`, a denial).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_px: Option<f64>,
    /// Why a close failed (the venue's reason, the refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `xm_exits/1:<account>` — one run of the exit rules. TTL 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XmExits {
    pub account: String,
    /// Every open position, by full id.
    pub positions: Vec<ExitCheck>,
    pub ts_ms: i64,
}

impl XmExits {
    fn count(&self, f: impl Fn(&ExitCheck) -> bool) -> usize {
        self.positions.iter().filter(|c| f(c)).count()
    }

    pub fn n_due(&self) -> usize {
        self.count(|c| c.reason.is_some())
    }

    pub fn n_closed(&self) -> usize {
        self.count(|c| c.status == ExitStatus::Filled)
    }

    pub fn n_failed(&self) -> usize {
        self.count(|c| c.status.failed())
    }

    pub fn n_stale_marks(&self) -> usize {
        self.count(|c| c.mark_px.value().is_none())
    }
}

impl Observed for XmExits {
    const SCHEMA: &'static str = "xm_exits/1";

    fn subject(&self) -> String {
        self.account.clone()
    }

    /// Counts, then every due position (`<reason> <full id> <status>`) when
    /// they all fit in line 1 — otherwise none (ids are never cut; `data`
    /// has them).
    fn headline(&self) -> String {
        let mut h = format!(
            "xm_exits account={} open={} due={} closed={} failed={}",
            self.account,
            self.positions.len(),
            self.n_due(),
            self.n_closed(),
            self.n_failed(),
        );
        let stale = self.n_stale_marks();
        if stale > 0 {
            h.push_str(&format!(" stale_marks={stale}"));
        }
        let due: String = self
            .positions
            .iter()
            .filter_map(|c| {
                let r = c.reason?;
                Some(format!(
                    " {} {} {}",
                    r.as_str(),
                    c.instrument,
                    c.status.as_str()
                ))
            })
            .collect();
        if h.chars().count() + due.chars().count() <= MAX_LINE1_CHARS {
            h.push_str(&due);
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "n_open", Some(self.positions.len() as i64));
        set_int(&mut f, "n_due", Some(self.n_due() as i64));
        set_int(&mut f, "n_closed", Some(self.n_closed() as i64));
        set_int(&mut f, "n_failed", Some(self.n_failed() as i64));
        set_int(&mut f, "n_stale_marks", Some(self.n_stale_marks() as i64));
        f
    }

    fn status(&self) -> ObsStatus {
        let failed = self.n_failed();
        if failed > 0 && self.n_closed() == 0 {
            ObsStatus::Error
        } else if failed > 0 || self.n_stale_marks() > 0 {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    /// Each failed close (field `exit:<id>`), then each stale mark.
    fn errors(&self) -> Vec<ReadError> {
        let failed = self
            .positions
            .iter()
            .filter(|c| c.status.failed())
            .map(|c| {
                let why = c
                    .error
                    .clone()
                    .or_else(|| c.rule.clone())
                    .unwrap_or_default();
                ReadError::new(
                    format!("exit:{}", c.instrument),
                    ErrorClass::NotApplicable,
                    format!(
                        "{} {} {}: {why}",
                        c.status.as_str(),
                        c.reason.map_or("-", |r| r.as_str()),
                        c.client_order_id.as_deref().unwrap_or("-")
                    ),
                )
            });
        let stale = self
            .positions
            .iter()
            .filter_map(|c| c.mark_px.error().cloned());
        failed.chain(stale).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation};
    use crate::domain::xm::ledger::HOUR_MS;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";
    /// 2026-10-03 00:00 UTC.
    const T0: i64 = 1_790_985_600_000;

    fn rules() -> ExitRules {
        ExitRules {
            take_profit_bps: 200.0,
            stop_loss_bps: 100.0,
            max_hold_ms: 24 * HOUR_MS,
        }
    }

    /// Open `qty` (signed) at 100 since `T0`.
    fn position(qty: f64) -> Position {
        Position {
            qty,
            avg_px: Some(100.0),
            opened_ms: Some(T0),
            ..Position::flat(TSLA, TSLA, "hyperliquid")
        }
    }

    fn fresh(px: f64) -> Field<f64> {
        Field::ok(px)
    }

    fn stale(id: &str) -> Field<f64> {
        Field::err(ReadError::new(
            format!("mark:{id}"),
            ErrorClass::Transient,
            "stale: mark age 60000 ms > 20000 ms",
        ))
    }

    /// Boundary vectors: (case, qty, mark, deadline, now, reason).
    #[test]
    fn exit_due_vectors() {
        use ExitReason::*;
        let hold = T0 + 24 * HOUR_MS;
        let cases: &[(&str, f64, Field<f64>, Option<i64>, i64, Option<ExitReason>)] = &[
            ("long flat P&L holds", 1.0, fresh(100.0), None, T0 + 1, None),
            (
                "long TP exactly 200 bps",
                1.0,
                fresh(102.0),
                None,
                T0 + 1,
                Some(TakeProfit),
            ),
            (
                "long just under TP",
                1.0,
                fresh(101.999),
                None,
                T0 + 1,
                None,
            ),
            (
                "long SL exactly -100 bps",
                1.0,
                fresh(99.0),
                None,
                T0 + 1,
                Some(StopLoss),
            ),
            ("long just above SL", 1.0, fresh(99.001), None, T0 + 1, None),
            (
                "short TP when the mark falls 200 bps",
                -1.0,
                fresh(98.0),
                None,
                T0 + 1,
                Some(TakeProfit),
            ),
            (
                "short SL when the mark rises 100 bps",
                -1.0,
                fresh(101.0),
                None,
                T0 + 1,
                Some(StopLoss),
            ),
            (
                "short just under SL",
                -1.0,
                fresh(100.999),
                None,
                T0 + 1,
                None,
            ),
            ("stale mark: no TP", 1.0, stale(TSLA), None, T0 + 1, None),
            ("absent mark: no SL", 1.0, Field::Absent, None, T0 + 1, None),
            (
                "max hold exactly",
                1.0,
                stale(TSLA),
                None,
                hold,
                Some(MaxHold),
            ),
            (
                "1 ms before max hold",
                1.0,
                fresh(100.0),
                None,
                hold - 1,
                None,
            ),
            (
                "deadline exactly",
                1.0,
                stale(TSLA),
                Some(T0 + HOUR_MS),
                T0 + HOUR_MS,
                Some(Deadline),
            ),
            (
                "1 ms before the deadline",
                1.0,
                fresh(100.0),
                Some(T0 + HOUR_MS),
                T0 + HOUR_MS - 1,
                None,
            ),
            (
                "deadline beats max hold and SL",
                1.0,
                fresh(90.0),
                Some(T0),
                hold,
                Some(Deadline),
            ),
            (
                "max hold beats TP",
                1.0,
                fresh(110.0),
                None,
                hold,
                Some(MaxHold),
            ),
            (
                "SL before a later deadline",
                1.0,
                fresh(98.0),
                Some(hold),
                T0 + 5,
                Some(StopLoss),
            ),
        ];
        for (name, qty, mark, deadline, now, want) in cases {
            let got = exit_due(&position(*qty), mark, *deadline, *now, &rules());
            assert_eq!(got, *want, "{name}");
        }
    }

    #[test]
    fn a_flat_or_unpriced_position_is_never_due_on_price() {
        let flat = Position::flat(TSLA, TSLA, "hyperliquid");
        assert_eq!(
            exit_due(&flat, &fresh(1.0), Some(T0), T0 + 1, &rules()),
            None
        );
        let no_entry = Position {
            avg_px: None,
            ..position(1.0)
        };
        assert_eq!(
            exit_due(&no_entry, &fresh(1.0), None, T0 + 1, &rules()),
            None
        );
        assert_eq!(pnl_bps(&position(1.0), 0.0), None, "a 0 mark is invalid");
        assert_eq!(pnl_bps(&position(1.0), f64::NAN), None);
        let pnl = pnl_bps(&position(-2.0), 99.5).unwrap();
        assert!((pnl - 50.0).abs() < 1e-9, "{pnl}");
    }

    #[test]
    fn exit_ids_are_deterministic_and_whole() {
        let id = exit_client_order_id("xmarket", TSLA, ExitReason::Deadline, T0, 1);
        assert_eq!(id, format!("exit:xmarket:{TSLA}:deadline:{T0}"));
        assert_eq!(
            exit_client_order_id("xmarket", TSLA, ExitReason::StopLoss, T0, 3),
            format!("exit:xmarket:{TSLA}:stop_loss:{T0}:3")
        );
        assert!(crate::domain::xm::exec::client_order_id_error(&id).is_none());
    }

    fn check(instrument: &str, reason: Option<ExitReason>, status: ExitStatus) -> ExitCheck {
        ExitCheck {
            instrument: instrument.into(),
            qty: 0.072,
            avg_px: Some(347.23),
            opened_ms: Some(T0),
            exit_at_ms: None,
            mark_px: fresh(347.2),
            pnl_bps: Some(-0.86),
            reason,
            status,
            client_order_id: reason.map(|r| exit_client_order_id("xmarket", instrument, r, T0, 1)),
            attempt: reason.map(|_| 1),
            rule: None,
            filled_qty: None,
            fill_px: None,
            error: None,
        }
    }

    #[test]
    fn the_row_counts_and_names_due_positions_in_full() {
        let mut row = XmExits {
            account: "xmarket".into(),
            positions: vec![
                check(TSLA, Some(ExitReason::Deadline), ExitStatus::Filled),
                check(NVDA, None, ExitStatus::Held),
            ],
            ts_ms: T0,
        };
        let o = Observation::of("xm_exits", &row, T0, 0, ObsSource::Live);
        assert_eq!(o.key, "xm_exits/1:xmarket");
        assert_eq!(o.status, ObsStatus::Ok);
        assert_features_ok(&o.features);
        assert_eq!(
            o.headline,
            format!(
                "xm_exits account=xmarket open=2 due=1 closed=1 failed=0 deadline {TSLA} filled"
            )
        );
        for (k, v) in [
            ("n_open", 2),
            ("n_due", 1),
            ("n_closed", 1),
            ("n_failed", 0),
            ("n_stale_marks", 0),
        ] {
            assert_eq!(o.features[k], v, "{k}");
        }
        // A rejected close and a stale mark: partial, both in errors.
        row.positions[1] = ExitCheck {
            mark_px: stale(NVDA),
            ..check(NVDA, Some(ExitReason::MaxHold), ExitStatus::Rejected)
        };
        row.positions[1].error = Some("stale_book: no book after the latency".into());
        let o = Observation::of("xm_exits", &row, T0, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Partial);
        assert_eq!(o.errors[0].field, format!("exit:{NVDA}"));
        assert!(o.errors[0].message.starts_with(&format!(
            "rejected max_hold exit:xmarket:{NVDA}:max_hold:{T0}: stale_book"
        )));
        assert_eq!(o.errors[1].field, format!("mark:{NVDA}"));
        // Every due close failed: error.
        row.positions[0].status = ExitStatus::Denied;
        assert_eq!(row.status(), ObsStatus::Error);
        // Nothing open: ok, nothing named.
        row.positions.clear();
        assert_eq!(row.status(), ObsStatus::Ok);
        assert_eq!(
            row.headline(),
            "xm_exits account=xmarket open=0 due=0 closed=0 failed=0"
        );
    }

    /// Line 1 never cuts an id: too many due positions ⇒ counts only.
    #[test]
    fn a_long_list_leaves_line_one_whole() {
        let row = XmExits {
            account: "xmarket".into(),
            positions: (0..8)
                .map(|i| {
                    check(
                        &format!("hyperliquid:xyz:NAME{i}"),
                        Some(ExitReason::TakeProfit),
                        ExitStatus::Filled,
                    )
                })
                .collect(),
            ts_ms: T0,
        };
        let h = row.headline();
        assert_eq!(h, "xm_exits account=xmarket open=8 due=8 closed=8 failed=0");
        assert!(h.chars().count() <= MAX_LINE1_CHARS);
    }
}
