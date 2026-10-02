//! Market-history statistics and the `mkt_history/1` row (xlab,
//! `docs/xlab-2026-10-01.md` § 8): pure numbers over the stored bars and
//! funding of one instrument in a window, the bar sample a reader sees, and
//! the typed row the `market_history` tool returns
//! (`adapters/outbound/tools/xlab/history.rs`). No IO: the window, the rows
//! and `now_ms` are inputs.
//!
//! | Number | Rule — too few rows ⇒ omitted, never 0 |
//! |---|---|
//! | `ret_bps` | ln(last close / first close) × 10⁴ — ≥ 2 bars |
//! | `vol_bps` | sample standard deviation of the log returns between consecutive stored bars × 10⁴ — ≥ 3 bars |
//! | `max_drawdown_bps` | largest fall from a running peak close: (peak − close) / peak × 10⁴ — ≥ 2 bars |
//! | `avg_volume` | mean bar `v`, in the venue's unit (HL base size, Gecko USD) — ≥ 1 bar |
//! | `funding_mean_apr_pct` | mean `rate_1h` × 24 × 365 × 100 — ≥ 1 funding row |
//! | `gaps` | interval grid slots in the window with a closed bar time and no stored bar ([`gap_count`]): no-trade hours (HL skips them), an unfilled head or tail, the time before a listing |
//! | sample | [`sample_indices`]: `points` stored bars evenly by index, the first and the last included — bars as stored, never re-aggregated; every stat uses every bar |
//! | share splits | the reader adjusts the bars first (`[backtest.splits]`, as backtests read them: `MarketData::adjust_for_splits`); every number and the sample are of the adjusted bars; one `notes` line per applied split, `splits_applied` counts them |
//!
//! | `mkt_history/1` status | When |
//! |---|---|
//! | `ok` | bars in the window, nothing failed |
//! | `partial` | bars in the window, a fetch (backfill) failed |
//! | `absent` | no bar stored in the window, nothing failed |
//! | `error` | no bar and a fetch or the store read failed |

use serde::{Deserialize, Serialize};

use crate::domain::marketdata::{fmt_time, Bar, FundingPoint, Interval};
use crate::domain::observation::{set_int, set_num, Features, ObsStatus, Observed, ReadError};

/// Bar-level statistics of a window (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarStats {
    pub n_bars: usize,
    /// `t_open_ms` of the first / last stored bar.
    pub first_ms: i64,
    pub last_ms: i64,
    pub last_close: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ret_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vol_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_volume: Option<f64>,
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// Stats of `bars` (ascending, as a `BarSeries` holds them); `None` without
/// a bar.
pub fn bar_stats(bars: &[Bar]) -> Option<BarStats> {
    let (first, last) = (bars.first()?, bars.last()?);
    let n = bars.len();
    let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
    let ret_bps = (n >= 2)
        .then(|| (last.c / first.c).ln() * 10_000.0)
        .and_then(finite);
    let rets: Vec<f64> = closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let vol_bps = sample_sd(&rets).map(|s| s * 10_000.0).and_then(finite);
    let max_drawdown_bps = (n >= 2)
        .then(|| max_drawdown(&closes) * 10_000.0)
        .and_then(finite);
    let avg_volume = finite(bars.iter().map(|b| b.v).sum::<f64>() / n as f64);
    Some(BarStats {
        n_bars: n,
        first_ms: first.t_open_ms,
        last_ms: last.t_open_ms,
        last_close: last.c,
        ret_bps,
        vol_bps,
        max_drawdown_bps,
        avg_volume,
    })
}

/// Sample standard deviation (n − 1); `None` below 2 values.
fn sample_sd(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    finite(var.sqrt())
}

/// Largest (peak − close) / peak over the closes in order (0 when they
/// never fall).
fn max_drawdown(closes: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst: f64 = 0.0;
    for &c in closes {
        if c > peak {
            peak = c;
        } else if peak > 0.0 {
            worst = worst.max((peak - c) / peak);
        }
    }
    worst
}

/// Mean hourly funding rate as an APR in percent; `None` without a row.
pub fn funding_mean_apr_pct(points: &[FundingPoint]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let mean = points.iter().map(|p| p.rate_1h).sum::<f64>() / points.len() as f64;
    finite(mean * 24.0 * 365.0 * 100.0)
}

/// `points` indices into `n` rows, evenly spaced, the first and the last
/// included (all of them when `n ≤ points`; the last alone for one point).
pub fn sample_indices(n: usize, points: usize) -> Vec<usize> {
    match (n, points) {
        (0, _) | (_, 0) => Vec::new(),
        _ if n <= points => (0..n).collect(),
        (_, 1) => vec![n - 1],
        _ => {
            // Integer rounding of k · (n − 1) / (points − 1): exact ends,
            // strictly increasing (the step is > 1 when n > points).
            let (span, steps) = (n - 1, points - 1);
            (0..points)
                .map(|k| (k * span + steps / 2) / steps)
                .collect()
        }
    }
}

/// The bars at [`sample_indices`], as stored.
pub fn sample_bars(bars: &[Bar], points: usize) -> Vec<Bar> {
    sample_indices(bars.len(), points)
        .into_iter()
        .map(|i| bars[i])
        .collect()
}

fn grid_floor(t: i64, step: i64) -> i64 {
    t.div_euclid(step) * step
}

fn grid_ceil(t: i64, step: i64) -> i64 {
    let f = grid_floor(t, step);
    if f == t {
        t
    } else {
        f.saturating_add(step)
    }
}

/// The grid opens `t` of the bars a window can hold: `from ≤ t < to` and
/// closed at `now_ms` (`t + interval ≤ now`) — `[start, end)` and the slot
/// count.
pub fn closed_slots(interval: Interval, from_ms: i64, to_ms: i64, now_ms: i64) -> (i64, i64, u64) {
    let iv = interval.ms();
    let start = grid_ceil(from_ms, iv);
    // A grid bar is closed at `now` exactly when it opens before the
    // current bar (`grid_floor(now)`).
    let end = to_ms.min(grid_floor(now_ms, iv));
    let slots = if end > start {
        ((end - start - 1) / iv + 1) as u64
    } else {
        0
    };
    (start, end, slots)
}

/// Window slots ([`closed_slots`]) holding no bar of `bars`.
pub fn gap_count(bars: &[Bar], interval: Interval, from_ms: i64, to_ms: i64, now_ms: i64) -> u64 {
    let (start, end, slots) = closed_slots(interval, from_ms, to_ms, now_ms);
    let iv = interval.ms();
    let held = bars
        .iter()
        .filter(|b| (start..end).contains(&b.t_open_ms) && b.t_open_ms.rem_euclid(iv) == 0)
        .count() as u64;
    slots.saturating_sub(held)
}

/// One stored series of the instrument (`ports::market_data::CoverageRow`
/// without the instrument): what else a reader can ask for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSeries {
    /// `bars` | `funding` | `ctx`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    pub first_ms: i64,
    pub last_ms: i64,
    pub rows: u64,
    pub sources: Vec<String>,
}

/// What a backfill before the read wrote (`fetch = true`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchSummary {
    /// Bars upserted.
    pub bars: usize,
    /// Funding rows upserted; `None` = the source has no funding (Gecko).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<usize>,
    /// The source (`hl`, `gecko:<network>:<pool>`, …), as stored.
    pub source: String,
    /// Clamps, resumes, up to date — one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// `mkt_history/1:<instrument>:<interval>` — one instrument's stored history
/// in a window (module tables). Ttl 0: a history read, never cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketHistory {
    /// Full instrument id.
    pub instrument: String,
    pub interval: Interval,
    /// The window `[from_ms, to_ms)` of bar opens and funding times.
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<BarStats>,
    pub funding_points: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_mean_apr_pct: Option<f64>,
    pub gaps: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<FetchSummary>,
    /// The sampled bars ([`sample_bars`]), ascending.
    pub bars: Vec<Bar>,
    /// Every series the store holds for the instrument.
    pub coverage: Vec<StoredSeries>,
    /// One line per share split applied to the bars (module table), ids in
    /// full.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

/// A count as a feature value.
fn count(n: impl TryInto<i64>) -> Option<i64> {
    Some(n.try_into().unwrap_or(i64::MAX))
}

impl MarketHistory {
    /// The row for the window's stored `bars` and `funding` (each ascending
    /// and inside `[from_ms, to_ms)`), `points` of the bars sampled.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instrument: &str,
        interval: Interval,
        from_ms: i64,
        to_ms: i64,
        bars: &[Bar],
        funding: &[FundingPoint],
        points: usize,
        now_ms: i64,
    ) -> Self {
        Self {
            instrument: instrument.to_string(),
            interval,
            from_ms,
            to_ms,
            stats: bar_stats(bars),
            funding_points: funding.len(),
            funding_mean_apr_pct: funding_mean_apr_pct(funding),
            gaps: gap_count(bars, interval, from_ms, to_ms, now_ms),
            fetch: None,
            bars: sample_bars(bars, points),
            coverage: Vec::new(),
            notes: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// A row whose read failed: no numbers, the errors.
    pub fn failed(
        instrument: &str,
        interval: Interval,
        from_ms: i64,
        to_ms: i64,
        errors: Vec<ReadError>,
    ) -> Self {
        Self {
            instrument: instrument.to_string(),
            interval,
            from_ms,
            to_ms,
            stats: None,
            funding_points: 0,
            funding_mean_apr_pct: None,
            gaps: 0,
            fetch: None,
            bars: Vec::new(),
            coverage: Vec::new(),
            notes: Vec::new(),
            errors,
        }
    }
}

impl Observed for MarketHistory {
    const SCHEMA: &'static str = "mkt_history/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.instrument, self.interval)
    }

    /// `mkt_history <id> <interval> bars=<n> <first> … <last> last_close=<c>
    /// ret_bps=<r>` (bar opens, UTC); without bars the window instead.
    fn headline(&self) -> String {
        let head = format!("mkt_history {} {}", self.instrument, self.interval);
        match &self.stats {
            Some(s) => {
                let mut line = format!(
                    "{head} bars={} {} … {} last_close={}",
                    s.n_bars,
                    fmt_time(s.first_ms),
                    fmt_time(s.last_ms),
                    s.last_close
                );
                if let Some(r) = s.ret_bps {
                    line.push_str(&format!(" ret_bps={r:.1}"));
                }
                line
            }
            None => {
                let what = if self.errors.is_empty() {
                    "none stored"
                } else {
                    "read failed"
                };
                format!(
                    "{head} bars=0 in {} … {}: {what}",
                    fmt_time(self.from_ms),
                    fmt_time(self.to_ms)
                )
            }
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let n = self.stats.as_ref().map_or(0, |s| s.n_bars);
        set_int(&mut f, "n_bars", count(n));
        set_int(&mut f, "from_ms", Some(self.from_ms));
        set_int(&mut f, "to_ms", Some(self.to_ms));
        set_int(&mut f, "points", count(self.bars.len()));
        set_int(&mut f, "gaps", count(self.gaps));
        set_int(&mut f, "funding_points", count(self.funding_points));
        set_num(&mut f, "funding_mean_apr_pct", self.funding_mean_apr_pct);
        if let Some(s) = &self.stats {
            set_int(&mut f, "first_ms", Some(s.first_ms));
            set_int(&mut f, "last_ms", Some(s.last_ms));
            set_num(&mut f, "last_close", Some(s.last_close));
            set_num(&mut f, "ret_bps", s.ret_bps);
            set_num(&mut f, "vol_bps", s.vol_bps);
            set_num(&mut f, "max_drawdown_bps", s.max_drawdown_bps);
            set_num(&mut f, "avg_volume", s.avg_volume);
        }
        if let Some(fetch) = &self.fetch {
            set_int(&mut f, "fetched_bars", count(fetch.bars));
            set_int(&mut f, "fetched_funding", fetch.funding.and_then(count));
        }
        if !self.notes.is_empty() {
            set_int(&mut f, "splits_applied", count(self.notes.len()));
        }
        f
    }

    fn status(&self) -> ObsStatus {
        match (self.stats.is_some(), self.errors.is_empty()) {
            (true, true) => ObsStatus::Ok,
            (true, false) => ObsStatus::Partial,
            (false, true) => ObsStatus::Absent,
            (false, false) => ObsStatus::Error,
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{
        assert_features_ok, ErrorClass, ObsSource, Observation, MAX_LINE1_CHARS,
    };

    const H: i64 = 3_600_000;
    const TSLA: &str = "hyperliquid:xyz:TSLA";

    fn bar(t: i64, c: f64) -> Bar {
        Bar {
            t_open_ms: t,
            o: c,
            h: c,
            l: c,
            c,
            v: 10.0,
            n: Some(1),
        }
    }

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| bar(i as i64 * H, *c))
            .collect()
    }

    fn near(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn stats_follow_the_module_table() {
        let ln = |x: f64| x.ln() * 10_000.0;
        // (closes, ret, vol, max drawdown) — None = omitted.
        type Row<'a> = (&'a [f64], Option<f64>, Option<f64>, Option<f64>);
        let cases: [Row; 5] = [
            (&[100.0], None, None, None),
            (&[100.0, 110.0], Some(ln(1.1)), None, Some(0.0)),
            (
                &[100.0, 110.0, 99.0],
                Some(ln(0.99)),
                Some({
                    let (a, b) = (1.1f64.ln(), (99.0f64 / 110.0).ln());
                    let m = (a + b) / 2.0;
                    (((a - m).powi(2) + (b - m).powi(2)) / 1.0).sqrt() * 10_000.0
                }),
                Some(1_000.0), // 110 → 99
            ),
            (&[100.0, 100.0, 100.0], Some(0.0), Some(0.0), Some(0.0)),
            // The worst fall is from the later, higher peak.
            (
                &[100.0, 80.0, 120.0, 90.0, 95.0],
                Some(ln(0.95)),
                None,
                Some(2_500.0),
            ),
        ];
        for (closes, ret, vol, mdd) in cases {
            let s = bar_stats(&bars(closes)).unwrap();
            assert_eq!(s.n_bars, closes.len());
            assert_eq!(s.last_close, *closes.last().unwrap());
            assert_eq!((s.first_ms, s.last_ms), (0, (closes.len() as i64 - 1) * H));
            for (what, got, want) in [("ret", s.ret_bps, ret), ("mdd", s.max_drawdown_bps, mdd)] {
                match want {
                    Some(w) => assert!(near(got, w), "{closes:?} {what}: {got:?} vs {w}"),
                    None => assert_eq!(got, None, "{closes:?} {what}"),
                }
            }
            if let Some(v) = vol {
                assert!(near(s.vol_bps, v), "{closes:?}: {:?} vs {v}", s.vol_bps);
            } else if closes.len() < 3 {
                assert_eq!(s.vol_bps, None, "{closes:?}");
            }
            assert!(near(s.avg_volume, 10.0));
        }
        assert!(bar_stats(&[]).is_none());
    }

    #[test]
    fn funding_apr_is_the_mean_hourly_rate_annualised() {
        let p = |t, r| FundingPoint {
            t_ms: t,
            rate_1h: r,
            premium: None,
        };
        assert_eq!(funding_mean_apr_pct(&[]), None);
        // 0.00125 % an hour (HL's default) = 10.95 % a year.
        let apr = funding_mean_apr_pct(&[p(0, 0.0000125)]).unwrap();
        assert!((apr - 10.95).abs() < 1e-9, "{apr}");
        let apr = funding_mean_apr_pct(&[p(0, 0.00002), p(H, -0.00001)]).unwrap();
        assert!((apr - 0.000005 * 876_000.0).abs() < 1e-9, "{apr}");
    }

    #[test]
    fn samples_are_even_and_keep_both_ends() {
        for (n, points, want) in [
            (0, 48, vec![]),
            (5, 0, vec![]),
            (3, 48, vec![0, 1, 2]),
            (48, 48, (0..48).collect::<Vec<_>>()),
            (10, 1, vec![9]),
            (10, 2, vec![0, 9]),
            (10, 4, vec![0, 3, 6, 9]),
            (11, 3, vec![0, 5, 10]),
            (5, 4, vec![0, 1, 3, 4]),
        ] {
            assert_eq!(sample_indices(n, points), want, "n={n} points={points}");
        }
        for (n, points) in [(67, 48), (5000, 200), (201, 200), (1000, 7)] {
            let idx = sample_indices(n, points);
            assert_eq!(idx.len(), points, "n={n}");
            assert_eq!((idx[0], *idx.last().unwrap()), (0, n - 1), "n={n}");
            assert!(idx.windows(2).all(|w| w[0] < w[1]), "n={n}: {idx:?}");
        }
        let b = bars(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let s = sample_bars(&b, 2);
        assert_eq!((s[0].c, s[1].c), (1.0, 5.0));
    }

    #[test]
    fn gaps_count_closed_grid_slots_without_a_bar() {
        let iv = Interval::H1;
        let now = 10 * H + 5; // the 10:00 bar is open
                              // (bar opens, from, to, slots, gaps)
        let cases: [(&[i64], i64, i64, u64, u64); 6] = [
            (&[0, 1, 2, 3], 0, 4 * H, 4, 0),
            (&[0, 2, 3], 0, 4 * H, 4, 1),
            // A from inside a bar starts at the next one; a to inside a
            // bar keeps it (its open is before to).
            (&[1, 2, 3], H / 2, 3 * H + 1, 3, 0),
            // The open bar and the future are no slots.
            (&[8, 9], 8 * H, 20 * H, 2, 0),
            (&[], 0, 4 * H, 4, 4),
            (&[], 12 * H, 20 * H, 0, 0),
        ];
        for (opens, from, to, slots, gaps) in cases {
            let b: Vec<Bar> = opens.iter().map(|h| bar(h * H, 1.0)).collect();
            assert_eq!(
                closed_slots(iv, from, to, now).2,
                slots,
                "{opens:?} {from}..{to}"
            );
            assert_eq!(
                gap_count(&b, iv, from, to, now),
                gaps,
                "{opens:?} {from}..{to}"
            );
        }
        // Daily bars sit on UTC midnights.
        let d = Interval::D1.ms();
        assert_eq!(closed_slots(Interval::D1, d / 2, 5 * d, 10 * d).2, 4);
    }

    fn row() -> MarketHistory {
        let p = |t, r| FundingPoint {
            t_ms: t + 94,
            rate_1h: r,
            premium: Some(0.0005),
        };
        let mut r = MarketHistory::new(
            TSLA,
            Interval::H1,
            0,
            4 * H,
            &bars(&[372.33, 371.0, 365.5, 360.2]),
            &[p(H, 0.0000177206), p(2 * H, 0.0000171458)],
            2,
            100 * H,
        );
        r.coverage.push(StoredSeries {
            kind: "bars".into(),
            interval: Some(Interval::H1),
            first_ms: 0,
            last_ms: 3 * H,
            rows: 4,
            sources: vec!["hl".into()],
        });
        r
    }

    #[test]
    fn the_row_keys_by_instrument_and_interval_with_full_ids() {
        let r = row();
        let o = Observation::of("market_history", &r, 5 * H, 0, ObsSource::Live);
        assert_eq!(o.key, format!("mkt_history/1:{TSLA}:1h"));
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Ok, 0));
        assert_eq!(
            o.headline,
            "mkt_history hyperliquid:xyz:TSLA 1h bars=4 1970-01-01T00:00:00Z … \
             1970-01-01T03:00:00Z last_close=360.2 ret_bps=-331.2"
        );
        assert_features_ok(&o.features);
        let f = &o.features;
        for (k, v) in [
            ("n_bars", 4),
            ("points", 2),
            ("gaps", 0),
            ("funding_points", 2),
            ("last_ms", 3 * H),
            ("to_ms", 4 * H),
        ] {
            assert_eq!(f[k], v, "{k}");
        }
        assert_eq!(f["last_close"], 360.2);
        assert!(f.contains_key("vol_bps") && f.contains_key("max_drawdown_bps"));
        assert!(!f.contains_key("fetched_bars"), "no fetch ran");
        // Mean of the two hourly rates × 24 × 365 × 100.
        let apr = f["funding_mean_apr_pct"].as_f64().unwrap();
        assert!((apr - 15.271_483_2).abs() < 1e-6, "{apr}");
        // The sample keeps both ends; data carries it with the coverage.
        let back: MarketHistory = o.typed().unwrap();
        assert_eq!(back.bars.len(), 2);
        assert_eq!((back.bars[0].c, back.bars[1].c), (372.33, 360.2));
        assert_eq!(o.data["coverage"][0]["sources"][0], "hl");
        let line1 = o.render_text(5 * H);
        let line1 = line1.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
    }

    #[test]
    fn line1_fits_with_the_longest_ids() {
        // A Robinhood Chain token address and a Solana mint, in full.
        for id in [
            "robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d",
            "solana:So11111111111111111111111111111111111111112",
        ] {
            let mut r = row();
            r.instrument = id.into();
            let o = Observation::of("market_history", &r, 5 * H, 0, ObsSource::Live);
            let text = o.render_text(5 * H);
            let line1 = text.lines().next().unwrap();
            assert!(line1.contains(id), "{line1}");
            assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        }
    }

    #[test]
    fn status_and_headline_without_bars_or_with_failures() {
        let empty = MarketHistory::new(TSLA, Interval::H1, 0, 4 * H, &[], &[], 48, 100 * H);
        let o = Observation::of("market_history", &empty, 0, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Absent);
        assert!(o.headline.ends_with(": none stored"), "{}", o.headline);
        assert_eq!(
            (o.features["n_bars"].clone(), o.features["gaps"].clone()),
            (0.into(), 4.into())
        );
        for k in ["last_close", "ret_bps", "funding_mean_apr_pct", "first_ms"] {
            assert!(!o.features.contains_key(k), "{k} omitted, never 0");
        }
        let e = ReadError::new("fetch_bars", ErrorClass::Transient, "HTTP 502");
        let mut partial = row();
        partial.errors.push(e.clone());
        partial.fetch = Some(FetchSummary {
            bars: 0,
            funding: Some(3),
            source: "hl".into(),
            notes: vec![],
        });
        let o = Observation::of("market_history", &partial, 0, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Partial);
        assert_eq!(o.errors, vec![e.clone()]);
        assert_eq!(
            (
                o.features["fetched_bars"].clone(),
                o.features["fetched_funding"].clone()
            ),
            (0.into(), 3.into())
        );
        let failed = MarketHistory::failed(TSLA, Interval::H1, 0, H, vec![e]);
        let o = Observation::of("market_history", &failed, 0, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Error);
        assert!(o.headline.ends_with(": read failed"), "{}", o.headline);
    }
}
