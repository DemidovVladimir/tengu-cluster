//! Market data for history-first research (xlab, `docs/xlab-2026-10-01.md`
//! § 4): bars, funding and per-minute contexts by full instrument id, and the
//! time-integrity rule every reader follows. Pure — the store is
//! `ports/market_data.rs` (`<state dir>/market.db`), the fetchers
//! `adapters/outbound/backfill/`.
//!
//! | Row | Observable at |
//! |---|---|
//! | [`Bar`] | its close, `t_open_ms + interval` — never earlier |
//! | [`FundingPoint`] | its `t_ms` (HL settles hourly; `rate_1h` > 0 = longs pay) |
//! | [`CtxPoint`] | its `t_ms` |
//! | [`MarketEvent`] | its `published_ms` (an SEC filing: its acceptance time); [`EventCoverage`] says which spans a source read — a NOISE label needs one (Phase 7) |
//!
//! A series is ascending by time with one row per instant (the last one
//! given wins); readers take prices only through [`BarSeries::close_at`] /
//! [`BarSeries::observable_at`], so a decision at `t` never sees a bar that
//! closes after `t`.
//!
//! | Share split ([`StockSplit`], `[backtest.splits]`) | Adjusted rows (before `at_ms`) |
//! |---|---|
//! | [`BarSeries::adjust_for_split`] | bars closed at or before it: o / h / l / c ÷ ratio, volume × ratio; `n` (trades) stays. A bar opening before it and closing after it (a day bar around an intraday split) is dropped: its open / high / low / volume mix both share counts — a decision priced there is skipped as missing ([`SplitAdjusted`]) |
//! | [`CtxSeries::adjust_for_split`] | rows before it: mark / oracle / mid / impact bid / ask ÷ ratio, open interest (base units) × ratio; funding, premium, notional volume stay |
//! | Funding | never (a rate per hour, not a price) |

use std::fmt;

use serde::{Deserialize, Serialize};

/// Bar interval. Wire names follow Hyperliquid's `candleSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Interval {
    M1,
    M5,
    M15,
    H1,
    H4,
    D1,
}

impl Interval {
    pub const ALL: [Interval; 6] = [
        Interval::M1,
        Interval::M5,
        Interval::M15,
        Interval::H1,
        Interval::H4,
        Interval::D1,
    ];

    /// Length in milliseconds.
    pub fn ms(self) -> i64 {
        match self {
            Interval::M1 => 60_000,
            Interval::M5 => 300_000,
            Interval::M15 => 900_000,
            Interval::H1 => 3_600_000,
            Interval::H4 => 14_400_000,
            Interval::D1 => 86_400_000,
        }
    }

    /// `1m 5m 15m 1h 4h 1d`.
    pub fn as_str(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::H1 => "1h",
            Interval::H4 => "4h",
            Interval::D1 => "1d",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Interval::ALL
            .into_iter()
            .find(|i| i.as_str() == s.trim())
            .ok_or_else(|| format!("interval `{s}` is not one of 1m, 5m, 15m, 1h, 4h, 1d"))
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for Interval {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Interval::parse(&s)
    }
}

impl From<Interval> for String {
    fn from(i: Interval) -> String {
        i.as_str().to_string()
    }
}

/// One OHLCV bar; `t_open_ms` is the start of its interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub t_open_ms: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    /// Volume in the venue's unit (HL: base size; Gecko: USD).
    pub v: f64,
    /// Trades in the bar, when the venue counts them (HL `n`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
}

impl Bar {
    /// When the bar becomes observable.
    pub fn t_close_ms(&self, interval: Interval) -> i64 {
        self.t_open_ms.saturating_add(interval.ms())
    }

    /// Prices finite and > 0, `l ≤ min(o, c)`, `h ≥ max(o, c)`, `v ≥ 0`.
    pub fn validate(&self) -> Result<(), String> {
        let px = [self.o, self.h, self.l, self.c];
        if px.iter().any(|p| !p.is_finite() || *p <= 0.0) {
            return Err(format!(
                "bar {}: prices must be finite and > 0",
                self.t_open_ms
            ));
        }
        if self.l > self.o.min(self.c) || self.h < self.o.max(self.c) || self.l > self.h {
            return Err(format!(
                "bar {}: low / high do not bound open / close",
                self.t_open_ms
            ));
        }
        if !self.v.is_finite() || self.v < 0.0 {
            return Err(format!(
                "bar {}: volume must be finite and ≥ 0",
                self.t_open_ms
            ));
        }
        Ok(())
    }
}

/// One funding settlement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingPoint {
    pub t_ms: i64,
    /// Rate per hour (HL `fundingRate`); > 0 = longs pay shorts.
    pub rate_1h: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium: Option<f64>,
}

/// One asset-context sample (HL archive `asset_ctxs`: one per minute).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CtxPoint {
    pub t_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact_bid: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact_ask: Option<f64>,
    /// Open interest in base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_ntl_vlm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium: Option<f64>,
}

impl CtxPoint {
    /// Half the impact spread in bps of the mid (else of the mark):
    /// `(impact_ask − impact_bid) / 2 / mid × 10⁴`; `None` without the
    /// prices or for a crossed / non-positive spread.
    pub fn impact_half_spread_bps(&self) -> Option<f64> {
        let (bid, ask) = (self.impact_bid?, self.impact_ask?);
        let mid = self.mid.or(self.mark)?;
        (ask >= bid && bid > 0.0 && mid > 0.0).then(|| (ask - bid) / 2.0 / mid * 10_000.0)
    }
}

/// Bars of one instrument at one interval, ascending, one per `t_open_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarSeries {
    pub instrument: String,
    pub interval: Interval,
    pub bars: Vec<Bar>,
}

impl BarSeries {
    /// Sorts by `t_open_ms`; a repeated open time keeps the last bar given.
    pub fn new(instrument: impl Into<String>, interval: Interval, mut bars: Vec<Bar>) -> Self {
        bars.reverse();
        bars.sort_by_key(|b| b.t_open_ms); // stable: the last given now leads its run
        bars.dedup_by_key(|b| b.t_open_ms);
        Self {
            instrument: instrument.into(),
            interval,
            bars,
        }
    }

    /// The bar ending exactly at `instant_ms` (`t_open = instant −
    /// interval`): a decision's entry bar.
    pub fn bar_ending_at(&self, instant_ms: i64) -> Option<&Bar> {
        let t_open = instant_ms.checked_sub(self.interval.ms())?;
        self.bars
            .binary_search_by_key(&t_open, |b| b.t_open_ms)
            .ok()
            .map(|i| &self.bars[i])
    }

    /// The close of the bar ending exactly at `instant_ms` (`t_open =
    /// instant − interval`); `None` when that bar is missing or its close is
    /// not > 0.
    pub fn close_at(&self, instant_ms: i64) -> Option<f64> {
        self.bar_ending_at(instant_ms)
            .map(|b| b.c)
            .filter(|c| c.is_finite() && *c > 0.0)
    }

    /// Adjust for `split` (module table): every bar closed at or before
    /// `split.at_ms` gets its prices ÷ ratio and its volume × ratio, as if the
    /// post-split share had always traded; a bar opening before the split
    /// and closing after it is dropped — its open, high, low and volume mix
    /// both share counts, and a half-adjusted bar reads as a fake move (the
    /// KIOXIA day bar of 2026-09-28: open 356.5 pre-split, close 114.12
    /// post). A decision priced on it is skipped as missing, never guessed.
    /// A ratio that is not finite and > 0 changes nothing.
    pub fn adjust_for_split(&mut self, split: StockSplit) -> SplitAdjusted {
        if !split.is_valid() {
            return SplitAdjusted::default();
        }
        let (r, at, iv) = (split.ratio, split.at_ms, self.interval.ms());
        let mut dropped = Vec::new();
        self.bars.retain(|b| {
            let straddles = b.t_open_ms < at && at < b.t_open_ms.saturating_add(iv);
            if straddles {
                dropped.push(b.t_open_ms);
            }
            !straddles
        });
        let n = self
            .bars
            .partition_point(|b| b.t_open_ms.saturating_add(iv) <= at);
        for b in &mut self.bars[..n] {
            b.o /= r;
            b.h /= r;
            b.l /= r;
            b.c /= r;
            b.v *= r;
        }
        SplitAdjusted {
            adjusted: n,
            dropped_open_ms: dropped,
        }
    }

    /// The bars observable at `t_ms` (close ≤ t), oldest first.
    pub fn observable_at(&self, t_ms: i64) -> &[Bar] {
        let iv = self.interval.ms();
        let n = self
            .bars
            .partition_point(|b| b.t_open_ms.saturating_add(iv) <= t_ms);
        &self.bars[..n]
    }

    /// The latest bar observable at `t_ms`.
    #[cfg_attr(not(test), allow(dead_code))] // readers: the `market_history` tool (xlab)
    pub fn last_at(&self, t_ms: i64) -> Option<&Bar> {
        self.observable_at(t_ms).last()
    }
}

/// Funding settlements of one instrument, ascending by `t_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingSeries {
    pub instrument: String,
    pub points: Vec<FundingPoint>,
}

impl FundingSeries {
    /// Sorts by `t_ms`; a repeated time keeps the last point given.
    pub fn new(instrument: impl Into<String>, mut points: Vec<FundingPoint>) -> Self {
        points.reverse();
        points.sort_by_key(|p| p.t_ms);
        points.dedup_by_key(|p| p.t_ms);
        Self {
            instrument: instrument.into(),
            points,
        }
    }

    /// Points observable at `t_ms` (`t_ms` of the point ≤ t).
    pub fn observable_at(&self, t_ms: i64) -> &[FundingPoint] {
        let n = self.points.partition_point(|p| p.t_ms <= t_ms);
        &self.points[..n]
    }

    /// Points with `from_ms < t_ms ≤ to_ms` — the settlements a position
    /// held over `(from, to]` pays or receives.
    pub fn between(&self, from_ms: i64, to_ms: i64) -> &[FundingPoint] {
        let lo = self.points.partition_point(|p| p.t_ms <= from_ms);
        let hi = self.points.partition_point(|p| p.t_ms <= to_ms);
        &self.points[lo..hi.max(lo)]
    }
}

/// Context samples of one instrument, ascending by `t_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CtxSeries {
    pub instrument: String,
    pub points: Vec<CtxPoint>,
}

impl CtxSeries {
    /// Sorts by `t_ms`; a repeated time keeps the last point given.
    pub fn new(instrument: impl Into<String>, mut points: Vec<CtxPoint>) -> Self {
        points.reverse();
        points.sort_by_key(|p| p.t_ms);
        points.dedup_by_key(|p| p.t_ms);
        Self {
            instrument: instrument.into(),
            points,
        }
    }

    /// The latest sample with `t_ms ≤ t`.
    pub fn last_at(&self, t_ms: i64) -> Option<&CtxPoint> {
        let n = self.points.partition_point(|p| p.t_ms <= t_ms);
        n.checked_sub(1).map(|i| &self.points[i])
    }

    /// Adjust for `split` (module table): rows before `split.at_ms` get
    /// their prices ÷ ratio and open interest × ratio; returns how many rows
    /// changed. A ratio that is not finite and > 0 changes nothing.
    pub fn adjust_for_split(&mut self, split: StockSplit) -> usize {
        if !split.is_valid() {
            return 0;
        }
        let r = split.ratio;
        let n = self.points.partition_point(|p| p.t_ms < split.at_ms);
        for p in &mut self.points[..n] {
            for px in [
                &mut p.mark,
                &mut p.oracle,
                &mut p.mid,
                &mut p.impact_bid,
                &mut p.impact_ask,
            ] {
                if let Some(x) = px.as_mut() {
                    *x /= r;
                }
            }
            if let Some(oi) = p.oi.as_mut() {
                *oi *= r;
            }
        }
        n
    }
}

/// What [`BarSeries::adjust_for_split`] did to one series.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitAdjusted {
    /// Bars closed at or before the split: prices ÷ ratio, volume × ratio.
    pub adjusted: usize,
    /// Open times of the bars that straddled the split (opened before it,
    /// closed after it): dropped. At most one on a bar grid — a day bar
    /// around an intraday split; none when the split is on the grid.
    pub dropped_open_ms: Vec<i64>,
}

/// The longest event title kept (chars).
pub const EVENT_TITLE_MAX_CHARS: usize = 240;

/// One dated information event of an instrument (roadmap Phase 7): an SEC
/// filing, a split, a listing. Observable at `published_ms` — never earlier:
/// a decision at `t` reads only events published at or before `t`. The
/// fetch time is the store's (`fetched_at_ms`): a backfilled event was
/// received long after it was public.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketEvent {
    pub instrument: String,
    /// Publication instant (SEC: the filing's `acceptanceDateTime`, UTC).
    pub published_ms: i64,
    /// `filing` · `split` · `listing`.
    pub kind: String,
    /// The source's own id, in full (SEC: the accession number).
    pub id: String,
    /// SEC form (`8-K`, `6-K`, `10-Q`, …), else the event's own label.
    pub form: String,
    /// One line (SEC: the 8-K items or the primary document's
    /// description), at most [`EVENT_TITLE_MAX_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl MarketEvent {
    /// The event kinds a store keeps.
    pub const KINDS: [&'static str; 3] = ["filing", "split", "listing"];

    pub fn validate(&self) -> Result<(), String> {
        if self.instrument.is_empty() || self.id.is_empty() || self.form.is_empty() {
            return Err(format!(
                "event `{}` of `{}`: instrument, id and form are required",
                self.id, self.instrument
            ));
        }
        if !Self::KINDS.contains(&self.kind.as_str()) {
            return Err(format!(
                "event `{}` of `{}`: kind `{}` is none of {}",
                self.id,
                self.instrument,
                self.kind,
                Self::KINDS.join(", ")
            ));
        }
        if self
            .title
            .as_ref()
            .is_some_and(|t| t.chars().count() > EVENT_TITLE_MAX_CHARS)
        {
            return Err(format!(
                "event `{}` of `{}`: title over {EVENT_TITLE_MAX_CHARS} chars",
                self.id, self.instrument
            ));
        }
        Ok(())
    }
}

/// What one source covers of one instrument: `[from_ms, to_ms)` was asked
/// and read; `covered = false` ⇒ the source has nothing for the instrument
/// at all (SEC: no CIK for the ticker) — its silence is no evidence of no
/// news. Phase 7's labels read NOISE only inside a covered span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCoverage {
    pub instrument: String,
    pub source: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub covered: bool,
    /// Why not covered, or what the source mapped the instrument to (SEC:
    /// `cik 0000320193 AAPL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub fetched_at_ms: i64,
}

/// A share split (module table): from `at_ms` on, `ratio` new shares per old
/// share — 3.0 is a 3-for-1 split, 0.1 a 1-for-10 reverse split.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StockSplit {
    /// The first instant at the new share count: rows before it are adjusted.
    pub at_ms: i64,
    pub ratio: f64,
}

impl StockSplit {
    /// A ratio finite, > 0 and ≠ 1 (`[backtest.splits]` refuses the rest at
    /// load).
    pub fn is_valid(&self) -> bool {
        self.ratio.is_finite() && self.ratio > 0.0 && self.ratio != 1.0
    }
}

/// Epoch ms, RFC 3339 (`2026-10-02T20:00:00-04:00`) or a UTC date
/// (`2026-10-02` = its midnight UTC).
pub fn parse_time(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(t) = d.and_hms_opt(0, 0, 0) {
            return Ok(t.and_utc().timestamp_millis());
        }
    }
    Err(format!(
        "`{s}` is not epoch ms, RFC 3339 (2026-10-02T20:00:00-04:00) or a date (2026-10-02)"
    ))
}

/// RFC 3339 in UTC with seconds (`2026-10-03T00:00:00Z`) — how reports show
/// instants.
pub fn fmt_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    fn bar(t: i64, c: f64) -> Bar {
        Bar {
            t_open_ms: t,
            o: c,
            h: c,
            l: c,
            c,
            v: 1.0,
            n: Some(1),
        }
    }

    #[test]
    fn intervals_round_trip_their_wire_names() {
        for i in Interval::ALL {
            assert_eq!(Interval::parse(i.as_str()).unwrap(), i);
            let json = serde_json::to_string(&i).unwrap();
            assert_eq!(serde_json::from_str::<Interval>(&json).unwrap(), i);
        }
        assert_eq!(Interval::H1.ms(), H);
        assert!(Interval::parse("2h").unwrap_err().contains("not one of"));
    }

    #[test]
    fn bars_validate_their_shape() {
        assert!(bar(0, 10.0).validate().is_ok());
        let mut b = bar(0, 10.0);
        b.h = 9.0;
        assert!(b.validate().is_err(), "high below the close");
        b = bar(0, 10.0);
        b.l = 0.0;
        assert!(b.validate().is_err(), "zero low");
        b = bar(0, 10.0);
        b.v = f64::NAN;
        assert!(b.validate().is_err(), "NaN volume");
    }

    #[test]
    fn a_bar_is_observable_only_at_its_close() {
        let s = BarSeries::new(
            "hyperliquid:xyz:TSLA",
            Interval::H1,
            vec![bar(2 * H, 102.0), bar(0, 100.0), bar(H, 101.0)],
        );
        assert_eq!(s.bars.len(), 3);
        // The bar [H, 2H) closes at 2H.
        assert_eq!(s.close_at(2 * H), Some(101.0));
        assert_eq!(s.close_at(2 * H + 1), None, "not on a bar end");
        assert_eq!(s.observable_at(2 * H - 1).len(), 1, "[H, 2H) still open");
        assert_eq!(s.observable_at(2 * H).len(), 2);
        assert_eq!(s.last_at(3 * H).map(|b| b.c), Some(102.0));
        assert!(s.last_at(H - 1).is_none());
    }

    /// Bars opening before the split: prices ÷ ratio, volume × ratio, trades
    /// kept; the bar opening at `at` and later untouched; an invalid ratio
    /// changes nothing; two splits compound.
    #[test]
    fn splits_adjust_the_bars_before_them() {
        let raw = |c: f64, v: f64, n: u64| Bar {
            v,
            n: Some(n),
            ..bar(0, c)
        };
        let series = || {
            BarSeries::new(
                "hyperliquid:xyz:KIOXIA",
                Interval::H1,
                [
                    (0, 340.79, 10.0, 7),
                    (H, 340.79, 0.0, 0),
                    (2 * H, 114.0, 30.0, 9),
                ]
                .iter()
                .map(|&(t, c, v, n)| Bar {
                    t_open_ms: t,
                    ..raw(c, v, n)
                })
                .collect(),
            )
        };
        let split = |at_ms: i64, ratio: f64| StockSplit { at_ms, ratio };
        // (splits, bars changed per split, closes, volumes)
        let cases: Vec<(Vec<StockSplit>, Vec<usize>, [f64; 3], [f64; 3])> = vec![
            (
                vec![split(2 * H, 3.0)],
                vec![2],
                [340.79 / 3.0, 340.79 / 3.0, 114.0],
                [30.0, 0.0, 30.0],
            ),
            // At the first bar's open: nothing opens before it.
            (
                vec![split(0, 3.0)],
                vec![0],
                [340.79, 340.79, 114.0],
                [10.0, 0.0, 30.0],
            ),
            // After every bar: all adjusted (returns unchanged).
            (
                vec![split(10 * H, 2.0)],
                vec![3],
                [340.79 / 2.0, 340.79 / 2.0, 57.0],
                [20.0, 0.0, 60.0],
            ),
            // Reverse 1-for-10.
            (
                vec![split(H, 0.1)],
                vec![1],
                [340.79 / 0.1, 340.79, 114.0],
                [1.0, 0.0, 30.0],
            ),
            // Two splits compound on the bars before both.
            (
                vec![split(H, 2.0), split(2 * H, 3.0)],
                vec![1, 2],
                [340.79 / 6.0, 340.79 / 3.0, 114.0],
                [60.0, 0.0, 30.0],
            ),
            // Not a split: ratio 1, ≤ 0, not finite.
            (
                vec![split(2 * H, 1.0), split(2 * H, 0.0), split(2 * H, -2.0)],
                vec![0, 0, 0],
                [340.79, 340.79, 114.0],
                [10.0, 0.0, 30.0],
            ),
            (
                vec![split(2 * H, f64::NAN)],
                vec![0],
                [340.79, 340.79, 114.0],
                [10.0, 0.0, 30.0],
            ),
        ];
        for (splits, changed, closes, volumes) in cases {
            let mut s = series();
            let got: Vec<usize> = splits
                .iter()
                .map(|x| {
                    let a = s.adjust_for_split(*x);
                    assert!(a.dropped_open_ms.is_empty(), "on the grid: none straddles");
                    a.adjusted
                })
                .collect();
            assert_eq!(got, changed, "{splits:?}");
            for (i, b) in s.bars.iter().enumerate() {
                assert!((b.c - closes[i]).abs() < 1e-9, "{splits:?} bar {i}: {b:?}");
                assert!((b.v - volumes[i]).abs() < 1e-9, "{splits:?} bar {i}: {b:?}");
                assert!(b.o == b.c && b.h == b.c && b.l == b.c, "{b:?}");
                assert!(b.validate().is_ok(), "{b:?}");
            }
            assert_eq!(
                s.bars.iter().map(|b| b.n).collect::<Vec<_>>(),
                vec![Some(7), Some(0), Some(9)],
                "trade counts stay"
            );
        }
        // The entry bar of an instant.
        let s = series();
        assert_eq!(s.bar_ending_at(2 * H).map(|b| b.n), Some(Some(0)));
        assert!(s.bar_ending_at(2 * H + 1).is_none());
        assert!(s.bar_ending_at(i64::MIN).is_none());
        assert!(StockSplit {
            at_ms: 0,
            ratio: 3.0
        }
        .is_valid());
        assert!(!StockSplit {
            at_ms: 0,
            ratio: 1.0
        }
        .is_valid());
    }

    /// A day bar around an intraday split (KIOXIA 2026-09-28 08:00 UTC):
    /// the days before are adjusted, the day bar that opened pre-split and
    /// closed post-split is dropped, the days after stay — never a close ÷ 3
    /// on a bar that closed post-split.
    #[test]
    fn a_bar_straddling_a_split_is_dropped_not_half_adjusted() {
        const D: i64 = 86_400_000;
        let day = |i: i64, o: f64, c: f64| Bar {
            t_open_ms: i * D,
            o,
            h: o.max(c),
            l: o.min(c),
            c,
            v: 30.0,
            n: Some(9),
        };
        let mut s = BarSeries::new(
            "hyperliquid:xyz:KIOXIA",
            Interval::D1,
            vec![
                day(0, 360.0, 356.5),
                day(1, 356.5, 114.12),
                day(2, 114.12, 110.63),
            ],
        );
        let split = StockSplit {
            at_ms: D + 8 * H,
            ratio: 3.0,
        };
        assert_eq!(
            s.adjust_for_split(split),
            SplitAdjusted {
                adjusted: 1,
                dropped_open_ms: vec![D],
            }
        );
        let closes: Vec<(i64, f64)> = s.bars.iter().map(|b| (b.t_open_ms, b.c)).collect();
        assert_eq!(closes, vec![(0, 356.5 / 3.0), (2 * D, 110.63)]);
        assert_eq!(s.bars[0].v, 90.0);
        // The same split on hourly bars is on the grid: nothing dropped.
        let mut h = BarSeries::new(
            "hyperliquid:xyz:KIOXIA",
            Interval::H1,
            (0..12).map(|i| bar(D + i * H, 300.0)).collect(),
        );
        let a = h.adjust_for_split(split);
        assert_eq!((a.adjusted, a.dropped_open_ms.len()), (8, 0));
        assert_eq!(h.bars.len(), 12);
    }

    #[test]
    fn splits_adjust_ctx_prices_and_open_interest_only() {
        let p = |t_ms: i64| CtxPoint {
            t_ms,
            mark: Some(300.0),
            oracle: Some(301.0),
            mid: Some(300.5),
            impact_bid: Some(299.0),
            impact_ask: Some(302.0),
            oi: Some(1_000.0),
            day_ntl_vlm: Some(5e6),
            funding_1h: Some(1e-5),
            premium: Some(0.001),
        };
        let mut s = CtxSeries::new(
            "x",
            vec![
                p(0),
                p(60_000),
                CtxPoint {
                    t_ms: 1,
                    ..Default::default()
                },
            ],
        );
        assert_eq!(
            s.adjust_for_split(StockSplit {
                at_ms: 60_000,
                ratio: 3.0
            }),
            2
        );
        let a = s.points[0];
        assert_eq!(
            (a.mark, a.oracle, a.mid, a.impact_bid, a.impact_ask),
            (
                Some(100.0),
                Some(301.0 / 3.0),
                Some(300.5 / 3.0),
                Some(299.0 / 3.0),
                Some(302.0 / 3.0)
            )
        );
        assert_eq!(a.oi, Some(3_000.0));
        assert_eq!(
            (a.day_ntl_vlm, a.funding_1h, a.premium),
            (Some(5e6), Some(1e-5), Some(0.001)),
            "USD volume, rates and premium are not per share"
        );
        assert_eq!(
            s.points[1],
            CtxPoint {
                t_ms: 1,
                ..Default::default()
            },
            "absent stays absent"
        );
        assert_eq!(s.points[2], p(60_000), "at the split: post-split already");
        // Half the impact spread in bps of the mid is unchanged by a split.
        let before = p(0).impact_half_spread_bps().unwrap();
        assert!((a.impact_half_spread_bps().unwrap() - before).abs() < 1e-9);
    }

    #[test]
    fn a_repeated_bar_keeps_the_last_given() {
        let s = BarSeries::new("x", Interval::H1, vec![bar(0, 1.0), bar(0, 2.0)]);
        assert_eq!(s.bars, vec![bar(0, 2.0)]);
    }

    #[test]
    fn funding_between_is_open_closed() {
        let p = |t, r| FundingPoint {
            t_ms: t,
            rate_1h: r,
            premium: None,
        };
        let f = FundingSeries::new("x", vec![p(3 * H, 0.3), p(H, 0.1), p(2 * H, 0.2)]);
        let rates = |s: &[FundingPoint]| s.iter().map(|p| p.rate_1h).collect::<Vec<_>>();
        assert_eq!(rates(f.between(H, 3 * H)), vec![0.2, 0.3]);
        assert_eq!(rates(f.between(0, H)), vec![0.1]);
        assert!(f.between(3 * H, 3 * H).is_empty());
        assert!(f.between(5 * H, 2 * H).is_empty(), "reversed range");
        assert_eq!(f.observable_at(2 * H).len(), 2);
    }

    #[test]
    fn ctx_spread_and_lookup() {
        let c = CtxPoint {
            t_ms: 60_000,
            mid: Some(100.0),
            impact_bid: Some(99.9),
            impact_ask: Some(100.1),
            ..Default::default()
        };
        assert!((c.impact_half_spread_bps().unwrap() - 10.0).abs() < 1e-9);
        let crossed = CtxPoint {
            impact_bid: Some(100.2),
            ..c
        };
        assert_eq!(crossed.impact_half_spread_bps(), None);
        let s = CtxSeries::new("x", vec![c]);
        assert!(s.last_at(59_999).is_none());
        assert_eq!(s.last_at(60_000).map(|p| p.t_ms), Some(60_000));
    }

    #[test]
    fn times_parse_and_format() {
        assert_eq!(parse_time("1759449600000").unwrap(), 1_759_449_600_000);
        assert_eq!(
            parse_time("2026-10-02T20:00:00-04:00").unwrap(),
            parse_time("2026-10-03").unwrap()
        );
        assert_eq!(
            fmt_time(parse_time("2026-10-03").unwrap()),
            "2026-10-03T00:00:00Z"
        );
        assert!(parse_time("friday").is_err());
    }
}
