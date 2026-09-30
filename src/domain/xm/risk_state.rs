//! Account risk state (`risk-kill-switch`, tracker § 7 #7 #8): the stored
//! halt and the UTC day's starting equity, with pure transitions. The
//! ledger keeps one per account (`risk_state` table,
//! `outbound/paper_store.rs`); every gate call and every `risk_status` read
//! advance it inside a ledger transaction; `tengu risk` shows and changes
//! it. The `risk_state/1:<account>` row is [`RiskStatus`].
//!
//! | Halt | Trips when | Clears |
//! |---|---|---|
//! | `daily_loss` | day-start equity − equity > `daily_loss_limit_usd` | at the next 00:00 UTC, or `tengu risk resume` |
//! | `total_loss` | initial cash − equity > `total_loss_limit_usd` | `tengu risk resume` only |
//! | `operator` | `tengu risk halt` | `tengu risk resume` only |
//! | `file` | `kill_switch_file` present — checked on every gate call and `risk_status` read; present ⇒ halted even before it is recorded | `tengu risk resume` only, once the file is gone |
//!
//! | Transition | Rule |
//! |---|---|
//! | [`RiskState::effective_halt`] | the stored halt, except a `daily_loss` halt tripped before today's 00:00 UTC |
//! | [`RiskState::roll`] | a new UTC day ⇒ `day_start_equity_usd` = the equity now (unknown ⇒ none, set at the first known equity that day; until then entries deny `missing:day_start_equity`); an expired `daily_loss` halt is dropped |
//! | [`RiskState::trip`] | records halts: a sticky halt (`total_loss`, `operator`, `file`) is never replaced; a `daily_loss` halt yields to a sticky one; among new trips `file` > `total_loss` > `operator` > `daily_loss` |
//! | [`RiskState::resume`] | clears any halt; refused while the kill-switch file exists. A loss still over its limit halts again at the next valuation |
//! | [`RiskState::value`] | the account at marks with the rolled day — what the gate (`RiskContext.account`) and `risk_status` read |
//!
//! Every transition returns a new state; `updated_ms` moves only when
//! something changed, so `new != old` means "store it".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError,
};
use crate::domain::xm::ledger::{Mark, PaperAccount, PaperPositions};
use crate::domain::xm::risk::{Halt, HaltReason, LossState, RiskLimits};

/// One UTC day.
pub const DAY_MS: i64 = 86_400_000;

/// 00:00 UTC of the day `now_ms` falls in.
pub fn utc_day_start(now_ms: i64) -> i64 {
    now_ms - now_ms.rem_euclid(DAY_MS)
}

/// The stored risk state of one ledger account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    /// The recorded halt; `None` = trading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halt: Option<Halt>,
    /// 00:00 UTC (ms) of the day `day_start_equity_usd` belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_utc_ms: Option<i64>,
    /// Equity at the first valuation of that day — the daily-loss base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_start_equity_usd: Option<f64>,
    /// Last change, ms; 0 = never changed.
    pub updated_ms: i64,
}

/// Halt precedence among trips of one valuation (module table).
const TRIP_ORDER: [HaltReason; 4] = [
    HaltReason::File,
    HaltReason::TotalLoss,
    HaltReason::Operator,
    HaltReason::DailyLoss,
];

impl RiskState {
    /// The halt in force at `now_ms` (module table).
    pub fn effective_halt(&self, now_ms: i64) -> Option<&Halt> {
        self.halt
            .as_ref()
            .filter(|h| h.reason != HaltReason::DailyLoss || h.since_ms >= utc_day_start(now_ms))
    }

    fn changed(&self, mut next: RiskState, now_ms: i64) -> RiskState {
        if next != *self {
            next.updated_ms = now_ms;
        }
        next
    }

    /// Roll the UTC day at `now_ms` with the equity now (module table).
    pub fn roll(&self, now_ms: i64, equity_usd: Option<f64>) -> RiskState {
        let today = utc_day_start(now_ms);
        let equity = equity_usd.filter(|e| e.is_finite());
        let mut next = self.clone();
        next.halt = self.effective_halt(now_ms).cloned();
        if next.day_utc_ms != Some(today) {
            next.day_utc_ms = Some(today);
            next.day_start_equity_usd = equity;
        } else if next.day_start_equity_usd.is_none() {
            next.day_start_equity_usd = equity;
        }
        self.changed(next, now_ms)
    }

    /// Record the halts `trips` call for (module table).
    pub fn trip(&self, trips: &[HaltReason], now_ms: i64) -> RiskState {
        let current = self.effective_halt(now_ms).cloned();
        let new = TRIP_ORDER.into_iter().find(|r| trips.contains(r));
        let halt = match (current, new) {
            (current, None) => current,
            (Some(h), Some(r)) if h.reason.is_sticky() || !r.is_sticky() => Some(h),
            (_, Some(reason)) => Some(Halt {
                reason,
                since_ms: now_ms,
            }),
        };
        let next = RiskState {
            halt,
            ..self.clone()
        };
        self.changed(next, now_ms)
    }

    /// `tengu risk halt`: an `operator` halt (a sticky halt stays as is).
    pub fn halt_operator(&self, now_ms: i64) -> RiskState {
        self.trip(&[HaltReason::Operator], now_ms)
    }

    /// `tengu risk resume` (module table). `Err` = refused, nothing changes.
    pub fn resume(&self, now_ms: i64, kill_switch_present: bool) -> Result<RiskState, String> {
        if kill_switch_present {
            return Err(
                "the kill-switch file is present: remove it first, then resume".to_string(),
            );
        }
        let next = RiskState {
            halt: None,
            ..self.clone()
        };
        Ok(self.changed(next, now_ms))
    }

    /// The account at `marks` with the day rolled at `now_ms` (module
    /// table): the valued account and the rolled state.
    pub fn value(
        &self,
        account: &PaperAccount,
        marks: &BTreeMap<String, Field<Mark>>,
        now_ms: i64,
        max_mark_age_ms: u64,
    ) -> (PaperPositions, RiskState) {
        let probe = PaperPositions::build(account, marks, now_ms, max_mark_age_ms, None, None);
        let rolled = self.roll(now_ms, probe.equity_usd.value().copied());
        let halted = Some(rolled.effective_halt(now_ms).is_some());
        let valued = PaperPositions::build(
            account,
            marks,
            now_ms,
            max_mark_age_ms,
            rolled.day_start_equity_usd,
            halted,
        );
        (valued, rolled)
    }
}

/// Halts a valuation calls for outside an order: the kill-switch file and
/// the loss limits (what `risk_status` records; a gate call records its
/// verdict's `trips`, the same rules).
pub fn valuation_trips(
    positions: &PaperPositions,
    kill_switch: &Field<bool>,
    limits: &RiskLimits,
) -> Vec<HaltReason> {
    let mut out = Vec::new();
    if kill_switch.value() == Some(&true) {
        out.push(HaltReason::File);
    }
    out.extend(LossState::of(positions).breaches(limits));
    out
}

/// `risk_state/1:<account>` — halt, kill switch, equity and P&L at fresh
/// marks, loss headroom, exposure, order rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskStatus {
    pub account: String,
    /// The state as stored after this read's roll and trips.
    pub state: RiskState,
    /// `kill_switch_file` present; `Error` = could not tell (halted).
    pub kill_switch: Field<bool>,
    /// The account at fresh marks, valued with the day's start equity.
    pub positions: PaperPositions,
    /// Equity − initial cash.
    pub total_pnl_usd: Field<f64>,
    /// Room under the tighter of the daily and total loss limits.
    pub loss_headroom_usd: Field<f64>,
    pub daily_loss_limit_usd: f64,
    pub total_loss_limit_usd: f64,
    pub orders_last_min: u32,
    pub open_orders: u32,
    pub as_of_ms: i64,
}

impl RiskStatus {
    pub fn new(
        positions: PaperPositions,
        state: RiskState,
        kill_switch: Field<bool>,
        limits: &RiskLimits,
        orders_last_min: u32,
        open_orders: u32,
        now_ms: i64,
    ) -> Self {
        let total_pnl_usd = match &positions.equity_usd {
            Field::Ok { value } => Field::ok(value - positions.initial_cash_usd),
            other => other.clone(),
        };
        let loss = LossState::of(&positions);
        let loss_headroom_usd = match (&loss.daily_loss_usd, &loss.total_loss_usd) {
            (Field::Ok { value: d }, Field::Ok { value: t }) => {
                Field::ok((limits.daily_loss_limit_usd - d).min(limits.total_loss_limit_usd - t))
            }
            (Field::Error { error }, _) | (_, Field::Error { error }) => Field::err(error.clone()),
            _ => Field::Absent,
        };
        Self {
            account: positions.account.clone(),
            state,
            kill_switch,
            total_pnl_usd,
            loss_headroom_usd,
            daily_loss_limit_usd: limits.daily_loss_limit_usd,
            total_loss_limit_usd: limits.total_loss_limit_usd,
            orders_last_min,
            open_orders,
            as_of_ms: now_ms,
            positions,
        }
    }

    /// Entries are blocked: a halt in force, the kill-switch file present,
    /// or its state unknown.
    pub fn halted(&self) -> bool {
        self.state.effective_halt(self.as_of_ms).is_some()
            || self.kill_switch.value() != Some(&false)
    }

    /// Why (`None` while trading, or when only the kill-switch state is
    /// unknown).
    pub fn reason(&self) -> Option<&'static str> {
        match self.state.effective_halt(self.as_of_ms) {
            Some(h) => Some(h.reason.as_str()),
            None => (self.kill_switch.value() == Some(&true)).then_some(HaltReason::File.as_str()),
        }
    }
}

impl Observed for RiskStatus {
    const SCHEMA: &'static str = "risk_state/1";

    fn subject(&self) -> String {
        self.account.clone()
    }

    fn headline(&self) -> String {
        let usd = |f: &Field<f64>| f.value().map_or("error".to_string(), |v| format!("{v:.2}"));
        let halt = if self.halted() {
            self.reason().unwrap_or("unknown")
        } else {
            "none"
        };
        let p = &self.positions;
        let gross = p
            .exposure
            .value()
            .map_or("error".to_string(), |e| format!("{:.2}", e.gross_usd));
        let lev = p
            .leverage
            .value()
            .map_or("error".to_string(), |l| format!("{l:.3}x"));
        let daily = match &p.daily_pnl_usd {
            Field::Absent => "none".to_string(),
            f => usd(f),
        };
        let headroom = match &self.loss_headroom_usd {
            Field::Absent => "none".to_string(),
            f => usd(f),
        };
        format!(
            "risk account={} halt={halt} equity={} daily_pnl={daily} total_pnl={} headroom={headroom} gross={gross} lev={lev} orders_1m={}",
            self.account,
            usd(&p.equity_usd),
            usd(&self.total_pnl_usd),
            self.orders_last_min,
        )
    }

    fn features(&self) -> Features {
        let p = &self.positions;
        let mut f = Features::new();
        set_bool(&mut f, "halted", Some(self.halted()));
        set_str(&mut f, "reason", self.reason());
        set_bool(&mut f, "kill_switch", self.kill_switch.value().copied());
        set_num(&mut f, "equity_usd", p.equity_usd.value().copied());
        set_num(&mut f, "cash_usd", Some(p.cash_usd));
        set_num(&mut f, "daily_pnl_usd", p.daily_pnl_usd.value().copied());
        set_num(&mut f, "total_pnl_usd", self.total_pnl_usd.value().copied());
        set_num(
            &mut f,
            "loss_headroom_usd",
            self.loss_headroom_usd.value().copied(),
        );
        set_num(
            &mut f,
            "day_start_equity_usd",
            self.state.day_start_equity_usd,
        );
        set_num(
            &mut f,
            "gross_exposure_usd",
            p.exposure.value().map(|e| e.gross_usd),
        );
        set_num(
            &mut f,
            "net_exposure_usd",
            p.exposure.value().map(|e| e.net_usd),
        );
        set_num(&mut f, "leverage", p.leverage.value().copied());
        set_int(&mut f, "n_positions", Some(p.positions.len() as i64));
        set_bool(&mut f, "marks_stale", Some(p.marks_stale > 0));
        set_int(
            &mut f,
            "orders_last_min",
            Some(i64::from(self.orders_last_min)),
        );
        set_int(&mut f, "open_orders", Some(i64::from(self.open_orders)));
        f
    }

    /// `Partial` when a mark, the day's start equity or the kill-switch
    /// state is unknown (the numbers it feeds are omitted, never 0).
    fn status(&self) -> ObsStatus {
        let unknown = self.positions.status() != ObsStatus::Ok
            || self.positions.daily_pnl_usd.value().is_none()
            || self.kill_switch.value().is_none();
        if unknown {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        let mut out = self.positions.errors();
        if let Some(e) = self.kill_switch.error() {
            out.push(e.clone());
        }
        if self.positions.daily_pnl_usd == Field::Absent {
            out.push(ReadError::new(
                "day_start_equity",
                ErrorClass::Transient,
                "no equity known yet today (a mark is missing or stale)",
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::book::Side;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_LINE1_CHARS};
    use crate::domain::xm::ledger::Fill;
    use crate::domain::xm::risk::MaxAges;

    const ACCOUNT: &str = "xmarket";
    const TSLA: &str = "hyperliquid:xyz:TSLA";
    /// 2026-10-03 00:00 UTC.
    const T: i64 = 1_790_985_600_000;
    const H: i64 = 3_600_000;

    fn halt(reason: HaltReason, since_ms: i64) -> Option<Halt> {
        Some(Halt { reason, since_ms })
    }

    fn state(h: Option<Halt>) -> RiskState {
        RiskState {
            halt: h,
            day_utc_ms: Some(T),
            day_start_equity_usd: Some(100.0),
            updated_ms: T,
        }
    }

    #[test]
    fn a_new_utc_day_takes_the_equity_then_and_expires_daily_halts() {
        let fresh = RiskState::default().roll(T + H, Some(100.0));
        assert_eq!(
            (
                fresh.day_utc_ms,
                fresh.day_start_equity_usd,
                fresh.updated_ms
            ),
            (Some(T), Some(100.0), T + H)
        );
        // Same day: the start stays, nothing changes.
        assert_eq!(fresh.roll(T + 5 * H, Some(90.0)), fresh);
        // Next day.
        let next = fresh.roll(T + 24 * H + 5, Some(95.0));
        assert_eq!(
            (next.day_utc_ms, next.day_start_equity_usd),
            (Some(T + 24 * H), Some(95.0))
        );
        // No equity at the roll: none until one is known that day.
        let blind = fresh.roll(T + 24 * H, None);
        assert_eq!(blind.day_start_equity_usd, None);
        assert_eq!(
            blind.roll(T + 25 * H, Some(97.0)).day_start_equity_usd,
            Some(97.0)
        );
        assert_eq!(
            fresh.roll(T + 24 * H, Some(f64::NAN)).day_start_equity_usd,
            None
        );
    }

    /// § 7 #8: a daily-loss halt clears at 00:00 UTC; the others only by
    /// resume.
    #[test]
    fn only_daily_loss_halts_clear_at_midnight_utc() {
        let midnight = T + 24 * H;
        for (reason, clears) in [
            (HaltReason::DailyLoss, true),
            (HaltReason::TotalLoss, false),
            (HaltReason::Operator, false),
            (HaltReason::File, false),
        ] {
            let s = state(halt(reason, T + 20 * H));
            assert!(
                s.effective_halt(midnight - 1).is_some(),
                "{reason:?} before"
            );
            assert_eq!(
                s.effective_halt(midnight).is_none(),
                clears,
                "{reason:?} after"
            );
            let rolled = s.roll(midnight, Some(90.0));
            assert_eq!(rolled.halt.is_none(), clears, "{reason:?} rolled");
        }
    }

    /// (stored halt, trips, expected halt) at `now` = T + 12 h.
    #[test]
    fn trips_keep_sticky_halts_and_rank_new_ones() {
        let now = T + 12 * H;
        let t0 = T + 2 * H;
        use HaltReason::*;
        let rows: [(Option<Halt>, &[HaltReason], Option<Halt>); 11] = [
            (None, &[], None),
            (None, &[DailyLoss], halt(DailyLoss, now)),
            (None, &[DailyLoss, TotalLoss], halt(TotalLoss, now)),
            (None, &[TotalLoss, File], halt(File, now)),
            (None, &[Operator, DailyLoss], halt(Operator, now)),
            (halt(DailyLoss, t0), &[TotalLoss], halt(TotalLoss, now)),
            (halt(DailyLoss, t0), &[DailyLoss], halt(DailyLoss, t0)),
            (halt(TotalLoss, t0), &[File], halt(TotalLoss, t0)),
            (halt(Operator, t0), &[DailyLoss], halt(Operator, t0)),
            (halt(File, t0), &[], halt(File, t0)),
            // Yesterday's daily halt expired: a new one starts now.
            (halt(DailyLoss, T - H), &[DailyLoss], halt(DailyLoss, now)),
        ];
        for (stored, trips, want) in rows {
            let s = state(stored.clone());
            let next = s.trip(trips, now);
            assert_eq!(next.halt, want, "{stored:?} + {trips:?}");
            assert_eq!(next != s, next.updated_ms == now, "{stored:?} + {trips:?}");
        }
    }

    #[test]
    fn operator_halt_and_resume() {
        let now = T + 3 * H;
        let s = state(None).halt_operator(now);
        assert_eq!(s.halt, halt(HaltReason::Operator, now));
        assert_eq!(
            state(halt(HaltReason::TotalLoss, T))
                .halt_operator(now)
                .halt,
            halt(HaltReason::TotalLoss, T),
            "a sticky halt stays"
        );
        let e = s.resume(now + 1, true).unwrap_err();
        assert!(e.contains("kill-switch file is present"), "{e}");
        let resumed = s.resume(now + 1, false).unwrap();
        assert_eq!((resumed.halt.clone(), resumed.updated_ms), (None, now + 1));
        assert_eq!(resumed.day_start_equity_usd, Some(100.0), "the day stays");
        assert_eq!(
            resumed.resume(now + 2, false).unwrap(),
            resumed,
            "nothing to resume"
        );
    }

    fn limits() -> RiskLimits {
        RiskLimits {
            account: ACCOUNT.into(),
            venues: vec!["hyperliquid".into()],
            min_lifecycle: None,
            instruments_allow: vec![TSLA.into()],
            instruments_deny: vec![],
            max_order_notional_usd: 25.0,
            max_position_notional_usd: 50.0,
            max_asset_exposure_usd: 50.0,
            max_venue_exposure_usd: 100.0,
            max_gross_exposure_usd: 100.0,
            max_net_exposure_usd: 100.0,
            max_leverage: 1.0,
            daily_loss_limit_usd: 10.0,
            total_loss_limit_usd: 25.0,
            min_edge_bps: 10.0,
            max_slippage_bps: 30.0,
            min_depth_usd: 250.0,
            require_hedge_for: vec![],
            max_data_age_ms: MaxAges {
                book: 5_000,
                ctx: 20_000,
                reference: 60_000,
                quote: 20_000,
            },
            max_skew_ms: 5_000,
            max_orders_per_min: 6,
            max_open_orders: 4,
            allow_reduce_degraded: true,
        }
    }

    /// $100, long 0.1 TSLA @ 400 (no fee).
    fn account() -> PaperAccount {
        let mut a = PaperAccount::new(ACCOUNT, 100.0).unwrap();
        a.apply_fill(&Fill {
            instrument: TSLA.into(),
            underlying: "company:tesla".into(),
            venue: "hyperliquid".into(),
            side: Side::Buy,
            qty: 0.1,
            px: 400.0,
            fee_usd: 0.0,
            ts_ms: T,
        })
        .unwrap();
        a
    }

    fn marks(px: f64, at_ms: i64) -> BTreeMap<String, Field<Mark>> {
        BTreeMap::from([(TSLA.to_string(), Field::ok(Mark { px, at_ms }))])
    }

    /// A read at `now`: value, record trips, build the row.
    fn status(s: &RiskState, px: Option<f64>, kill: Field<bool>, now: i64) -> RiskStatus {
        let m = px.map_or_else(BTreeMap::new, |px| marks(px, now - 1_000));
        let (valued, rolled) = s.value(&account(), &m, now, 20_000);
        let next = rolled.trip(&valuation_trips(&valued, &kill, &limits()), now);
        let (valued, _) = next.value(&account(), &m, now, 20_000);
        RiskStatus::new(valued, next, kill, &limits(), 2, 0, now)
    }

    #[test]
    fn losses_over_a_limit_trip_on_a_valuation() {
        let now = T + 12 * H;
        let day_start = state(None);
        // 0.1 × (300 − 400) = −10: at the daily limit, no trip.
        let at = status(&day_start, Some(300.0), Field::ok(false), now);
        assert_eq!(
            (at.halted(), at.loss_headroom_usd.clone()),
            (false, Field::ok(0.0))
        );
        // −10.01: daily.
        let over = status(&day_start, Some(299.9), Field::ok(false), now);
        assert_eq!(over.state.halt, halt(HaltReason::DailyLoss, now));
        // −26 from inception with a day start of 90: total (sticky) wins.
        let mut s = day_start.clone();
        s.day_start_equity_usd = Some(90.0);
        let total = status(&s, Some(140.0), Field::ok(false), now);
        assert_eq!(total.state.halt, halt(HaltReason::TotalLoss, now));
        assert_eq!(total.reason(), Some("total_loss"));
    }

    #[test]
    fn the_row_carries_state_pnl_and_headroom_with_full_ids() {
        let now = T + 12 * H;
        // Day start 100, mark 410: equity 101, +1 today, +1 total.
        let st = status(&state(None), Some(410.0), Field::ok(false), now);
        assert_eq!(st.status(), ObsStatus::Ok, "{:?}", st.errors());
        assert!(!st.halted());
        let f = st.features();
        assert_features_ok(&f);
        assert!(f.len() <= 32);
        assert_eq!(f["halted"], false);
        assert_eq!(f["kill_switch"], false);
        assert!(!f.contains_key("reason"));
        for (k, v) in [
            ("equity_usd", 101.0),
            ("daily_pnl_usd", 1.0),
            ("total_pnl_usd", 1.0),
            ("loss_headroom_usd", 11.0),
            ("gross_exposure_usd", 41.0),
            ("leverage", 41.0 / 101.0),
        ] {
            let got = f[k].as_f64().unwrap();
            assert!((got - v).abs() < 1e-9, "{k}: {got} vs {v}");
        }
        assert_eq!(
            (f["orders_last_min"].clone(), f["n_positions"].clone()),
            (2.into(), 1.into())
        );
        let obs = Observation::of("risk_status", &st, now, 2_000, ObsSource::Live);
        assert_eq!(obs.key, format!("risk_state/1:{ACCOUNT}"));
        assert_eq!(obs.typed::<RiskStatus>().unwrap(), st);
        let text = obs.render_text(now);
        let line1 = text.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert!(
            line1.starts_with("risk account=xmarket halt=none equity=101.00"),
            "{line1}"
        );
        assert!(text.contains(TSLA), "full id in data");
    }

    #[test]
    fn missing_marks_and_an_unknown_kill_switch_are_partial_never_zero() {
        let now = T + 12 * H;
        let blind = status(&state(None), None, Field::ok(false), now);
        assert_eq!(blind.status(), ObsStatus::Partial);
        let f = blind.features();
        for k in [
            "equity_usd",
            "daily_pnl_usd",
            "total_pnl_usd",
            "loss_headroom_usd",
            "leverage",
        ] {
            assert!(!f.contains_key(k), "{k} must be omitted");
        }
        assert_eq!(f["marks_stale"], true);
        assert!(blind
            .errors()
            .iter()
            .any(|e| e.field == format!("mark:{TSLA}")));
        // A fresh day without any equity yet: no day start, partial.
        let s = RiskState::default();
        let no_start = status(&s, None, Field::ok(false), now);
        assert!(no_start.state.day_start_equity_usd.is_none());
        assert!(no_start
            .errors()
            .iter()
            .any(|e| e.field == "day_start_equity"));
        // Kill switch present ⇒ halted `file`, recorded; unknown ⇒ halted, partial.
        let file = status(&state(None), Some(400.0), Field::ok(true), now);
        assert!(file.halted());
        assert_eq!(file.state.halt, halt(HaltReason::File, now));
        assert_eq!(file.features()["reason"], "file");
        assert!(file.headline().contains("halt=file"), "{}", file.headline());
        let e = ReadError::new("kill_switch", ErrorClass::Fatal, "permission denied");
        let unknown = status(&state(None), Some(400.0), Field::err(e), now);
        assert!(unknown.halted() && unknown.reason().is_none());
        assert_eq!(unknown.status(), ObsStatus::Partial);
        assert!(!unknown.features().contains_key("kill_switch"));
        assert!(unknown.headline().contains("halt=unknown"));
        assert!(
            unknown.state.halt.is_none(),
            "an unknown file state trips nothing"
        );
    }
}
