//! Venue-neutral L2 order book: levels, the depth walk (VWAP, worst price,
//! slippage vs mid and vs touch) and depth within N bps of mid. The one
//! source for `hl_book` (`hl-book-tool`), the paper fill engine
//! (`risk-paper-fill-engine`) and the cost model (`domain/xm/cost.rs`).
//! Pure; no IO.
//!
//! | Rule | Detail |
//! |---|---|
//! | Sides | `bids` best (highest) first, strictly descending; `asks` best (lowest) first, strictly ascending; `px`, `sz` finite and > 0; best bid < best ask — [`L2Book::new`] rejects anything else |
//! | Taker side | [`Side::Buy`] consumes asks, [`Side::Sell`] consumes bids |
//! | Visible depth only | a walk that runs out of levels ends `depth` with `unfilled > 0` — never assumes hidden liquidity (HL `l2Book` shows ≤ 20 levels per side) |
//! | Limit | optional price bound: a buy stops before the first ask above it, a sell before the first bid below it (`end = limit`) |
//! | Slippage sign | bps, positive = cost to the taker: buy `(vwap − ref) / ref`, sell `(ref − vwap) / ref`; `ref` = mid or the touch |
//! | Missing | no fill ⇒ `vwap`, `worst_px` and slippage are `None` — never 0 |
//! | Wire | [`L2Level`] reads HL's `{px, sz, n}` with decimal strings (numbers too) |

// Consumers land in wave W1 (`hl-book-tool`, `risk-paper-fill-engine`,
// `risk-calc-tools`).
#![allow(dead_code)]

use serde::{Deserialize, Deserializer, Serialize};

/// Relative tolerance under which a remaining target counts as filled
/// (f64 residue of summed decimal sizes).
const FILL_EPS: f64 = 1e-12;

/// Taker side of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// `+1` buy (long), `−1` sell (short).
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s.trim() {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            _ => None,
        }
    }
}

/// One price level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct L2Level {
    #[serde(deserialize_with = "de_decimal")]
    pub px: f64,
    #[serde(deserialize_with = "de_decimal")]
    pub sz: f64,
    /// Orders resting at this level (HL `n`); 0 when the venue does not say.
    #[serde(default)]
    pub n: u32,
}

impl L2Level {
    pub fn notional(&self) -> f64 {
        self.px * self.sz
    }
}

/// A validated L2 snapshot (see the module table for the invariants).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L2Book {
    pub bids: Vec<L2Level>,
    pub asks: Vec<L2Level>,
    /// Venue timestamp of the snapshot (HL `time`), ms since the epoch.
    pub venue_ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BookError {
    #[error("{side} level {index}: {what}")]
    Level {
        side: &'static str,
        index: usize,
        what: String,
    },
    #[error("crossed book: best bid {bid} >= best ask {ask}")]
    Crossed { bid: f64, ask: f64 },
    #[error("no mid: a side of the book is empty")]
    NoMid,
    #[error("invalid argument: {0}")]
    Arg(String),
}

impl L2Book {
    /// Checked constructor: every level valid, sides ordered, not crossed.
    /// Empty sides are allowed (delisted / halted markets).
    pub fn new(
        bids: Vec<L2Level>,
        asks: Vec<L2Level>,
        venue_ts_ms: i64,
    ) -> Result<Self, BookError> {
        let book = Self {
            bids,
            asks,
            venue_ts_ms,
        };
        book.validate()?;
        Ok(book)
    }

    /// The [`L2Book::new`] invariants, for books built another way (serde).
    pub fn validate(&self) -> Result<(), BookError> {
        check_side("bid", &self.bids, |prev, px| px < prev)?;
        check_side("ask", &self.asks, |prev, px| px > prev)?;
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if bid >= ask {
                return Err(BookError::Crossed { bid, ask });
            }
        }
        Ok(())
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|l| l.px)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|l| l.px)
    }

    /// `(best bid + best ask) / 2`; `None` when either side is empty.
    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }

    /// `(ask − bid) / mid` in bps; `None` without a mid.
    pub fn spread_bps(&self) -> Option<f64> {
        let (bid, ask, mid) = (self.best_bid()?, self.best_ask()?, self.mid()?);
        Some((ask - bid) / mid * 1e4)
    }

    /// Both sides empty (HL shows `levels: [[], []]` for a delisted market).
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Levels a taker on `side` consumes: buy → asks, sell → bids.
    pub fn liquidity(&self, side: Side) -> &[L2Level] {
        match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        }
    }

    /// Best price a taker on `side` hits (the touch).
    pub fn touch(&self, side: Side) -> Option<f64> {
        self.liquidity(side).first().map(|l| l.px)
    }

    /// Snapshot age at `now_ms` by the venue clock (saturating at 0).
    pub fn age_ms(&self, now_ms: i64) -> u64 {
        now_ms.saturating_sub(self.venue_ts_ms).max(0) as u64
    }

    /// [`walk`] over the side a taker on `side` consumes, slippage vs this
    /// book's mid.
    pub fn walk(
        &self,
        side: Side,
        target: WalkTarget,
        limit_px: Option<f64>,
    ) -> Result<Walk, BookError> {
        walk(self.liquidity(side), side, target, limit_px, self.mid())
    }
}

fn check_side(
    name: &'static str,
    levels: &[L2Level],
    ordered: impl Fn(f64, f64) -> bool,
) -> Result<(), BookError> {
    let err = |index: usize, what: String| BookError::Level {
        side: name,
        index,
        what,
    };
    for (i, l) in levels.iter().enumerate() {
        if !(l.px.is_finite() && l.px > 0.0) {
            return Err(err(i, format!("px {} is not > 0", l.px)));
        }
        if !(l.sz.is_finite() && l.sz > 0.0) {
            return Err(err(i, format!("sz {} is not > 0", l.sz)));
        }
        if i > 0 && !ordered(levels[i - 1].px, l.px) {
            return Err(err(
                i,
                format!("px {} out of order after {}", l.px, levels[i - 1].px),
            ));
        }
    }
    Ok(())
}

/// How much to take: base quantity or quote notional (USD).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkTarget {
    Qty(f64),
    Notional(f64),
}

/// Why a walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkEnd {
    /// The whole target was taken.
    Filled,
    /// The next level is beyond `limit_px`.
    Limit,
    /// Visible levels ran out.
    Depth,
}

/// Quantity taken at one level (`level` = index into the walked side).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LevelFill {
    pub level: usize,
    pub px: f64,
    pub qty: f64,
}

/// Result of a depth walk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Walk {
    pub side: Side,
    pub target: WalkTarget,
    pub filled_qty: f64,
    /// Σ px × qty over the fills (quote units, USD).
    pub filled_notional: f64,
    pub vwap: Option<f64>,
    /// Last (least favourable) level price touched.
    pub worst_px: Option<f64>,
    pub levels_used: usize,
    pub slippage_bps_vs_mid: Option<f64>,
    pub slippage_bps_vs_touch: Option<f64>,
    /// Target left over, in the target's unit (qty or USD); 0 when filled.
    pub unfilled: f64,
    pub end: WalkEnd,
    pub fills: Vec<LevelFill>,
}

impl Walk {
    pub fn is_complete(&self) -> bool {
        self.end == WalkEnd::Filled
    }
}

/// Take `target` from `levels` (best first) as a taker on `side`, stopping
/// before the first level beyond `limit_px`. `mid` only feeds
/// `slippage_bps_vs_mid`. A target of 0 fills nothing and ends `filled`.
pub fn walk(
    levels: &[L2Level],
    side: Side,
    target: WalkTarget,
    limit_px: Option<f64>,
    mid: Option<f64>,
) -> Result<Walk, BookError> {
    let (want, by_notional) = match target {
        WalkTarget::Qty(q) => (q, false),
        WalkTarget::Notional(n) => (n, true),
    };
    if !(want.is_finite() && want >= 0.0) {
        return Err(BookError::Arg(format!("walk target {want} is not >= 0")));
    }
    if let Some(l) = limit_px {
        if !(l.is_finite() && l > 0.0) {
            return Err(BookError::Arg(format!("limit_px {l} is not > 0")));
        }
    }
    if let Some(m) = mid {
        if !(m.is_finite() && m > 0.0) {
            return Err(BookError::Arg(format!("mid {m} is not > 0")));
        }
    }
    let tol = want * FILL_EPS;
    let mut rem = want;
    let mut end = WalkEnd::Depth;
    let mut fills = Vec::new();
    for (i, lvl) in levels.iter().enumerate() {
        if rem <= tol {
            break;
        }
        if let Some(limit) = limit_px {
            let beyond = match side {
                Side::Buy => lvl.px > limit,
                Side::Sell => lvl.px < limit,
            };
            if beyond {
                end = WalkEnd::Limit;
                break;
            }
        }
        let qty = if by_notional {
            let lvl_notional = lvl.notional();
            if lvl_notional <= rem {
                rem -= lvl_notional;
                lvl.sz
            } else {
                let q = rem / lvl.px;
                rem = 0.0;
                q
            }
        } else {
            let q = lvl.sz.min(rem);
            rem -= q;
            q
        };
        fills.push(LevelFill {
            level: i,
            px: lvl.px,
            qty,
        });
    }
    if rem <= tol {
        rem = 0.0;
        end = WalkEnd::Filled;
    }
    let filled_qty: f64 = fills.iter().map(|f| f.qty).sum();
    let filled_notional: f64 = fills.iter().map(|f| f.px * f.qty).sum();
    let vwap = (filled_qty > 0.0).then(|| filled_notional / filled_qty);
    let cost_bps = |reference: f64| -> Option<f64> {
        let v = vwap?;
        Some(side.sign() * (v - reference) / reference * 1e4)
    };
    Ok(Walk {
        side,
        target,
        filled_qty,
        filled_notional,
        vwap,
        worst_px: fills.last().map(|f| f.px),
        levels_used: fills.len(),
        slippage_bps_vs_mid: mid.and_then(cost_bps),
        slippage_bps_vs_touch: levels.first().and_then(|l| cost_bps(l.px)),
        unfilled: rem,
        end,
        fills,
    })
}

/// Resting depth within `bps` of mid, in quote units (USD).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Depth {
    pub bps: f64,
    /// Σ px × sz of bids at or above `mid × (1 − bps / 1e4)`.
    pub bid_usd: f64,
    /// Σ px × sz of asks at or below `mid × (1 + bps / 1e4)`.
    pub ask_usd: f64,
    pub bid_levels: usize,
    pub ask_levels: usize,
}

impl Depth {
    /// `(bid − ask) / (bid + ask)` ∈ [−1, 1]; `None` when both are 0.
    pub fn imbalance(&self) -> Option<f64> {
        let total = self.bid_usd + self.ask_usd;
        (total > 0.0).then(|| (self.bid_usd - self.ask_usd) / total)
    }

    /// Depth a taker on `side` can take: buy → asks, sell → bids.
    pub fn for_taker(&self, side: Side) -> f64 {
        match side {
            Side::Buy => self.ask_usd,
            Side::Sell => self.bid_usd,
        }
    }
}

/// Depth within `bps` of mid on each side (bounds inclusive). Errors: no
/// mid (a side is empty), `bps` negative or not finite.
pub fn depth_within(book: &L2Book, bps: f64) -> Result<Depth, BookError> {
    if !(bps.is_finite() && bps >= 0.0) {
        return Err(BookError::Arg(format!("bps {bps} is not >= 0")));
    }
    let mid = book.mid().ok_or(BookError::NoMid)?;
    let lo = mid * (1.0 - bps / 1e4);
    let hi = mid * (1.0 + bps / 1e4);
    let within = |levels: &[L2Level], inside: &dyn Fn(f64) -> bool| -> (f64, usize) {
        levels
            .iter()
            .take_while(|l| inside(l.px))
            .fold((0.0, 0), |(usd, n), l| (usd + l.notional(), n + 1))
    };
    let (bid_usd, bid_levels) = within(&book.bids, &|px| px >= lo);
    let (ask_usd, ask_levels) = within(&book.asks, &|px| px <= hi);
    Ok(Depth {
        bps,
        bid_usd,
        ask_usd,
        bid_levels,
        ask_levels,
    })
}

/// A decimal given as a string (HL wire) or a number; finite only.
fn de_decimal<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        Text(String),
        Number(f64),
    }
    let v = match Num::deserialize(d)? {
        Num::Text(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|e| serde::de::Error::custom(format!("decimal {s:?}: {e}")))?,
        Num::Number(n) => n,
    };
    if v.is_finite() {
        Ok(v)
    } else {
        Err(serde::de::Error::custom("decimal is not finite"))
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! `tests/fixtures/xm/l2_xyz_TSLA.json` — live `POST
    //! https://api.hyperliquid.xyz/info {"type":"l2Book","coin":"xyz:TSLA"}`,
    //! 2026-09-30T13:35:52Z; provenance + hand-computed goldens in
    //! `tests/fixtures/xm/meta.json`. Shared with `xm::cost` tests.
    use super::*;

    pub(crate) const L2_XYZ_TSLA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xm/l2_xyz_TSLA.json"
    ));

    #[derive(Deserialize)]
    struct HlL2Book {
        coin: String,
        time: i64,
        levels: [Vec<L2Level>; 2],
    }

    /// The fixture as an [`L2Book`] (bids = `levels[0]`, asks = `levels[1]`).
    pub(crate) fn tsla_book() -> L2Book {
        let raw: HlL2Book = serde_json::from_str(L2_XYZ_TSLA).unwrap();
        assert_eq!(raw.coin, "xyz:TSLA");
        let [bids, asks] = raw.levels;
        L2Book::new(bids, asks, raw.time).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::tsla_book;
    use super::*;

    const MID: f64 = 347.195;

    fn close(a: f64, b: f64, what: &str) {
        let tol = 1e-9 * b.abs().max(1.0);
        assert!((a - b).abs() <= tol, "{what}: got {a}, want {b}");
    }

    fn lvl(px: f64, sz: f64) -> L2Level {
        L2Level { px, sz, n: 1 }
    }

    #[test]
    fn fixture_decodes_as_a_valid_book() {
        let b = tsla_book();
        assert_eq!((b.bids.len(), b.asks.len()), (20, 20));
        assert_eq!(b.venue_ts_ms, 1_790_775_352_605);
        assert_eq!(
            b.bids[0],
            L2Level {
                px: 347.16,
                sz: 85.304,
                n: 1
            }
        );
        assert_eq!(
            b.asks[0],
            L2Level {
                px: 347.23,
                sz: 20.661,
                n: 2
            }
        );
        assert_eq!((b.best_bid(), b.best_ask()), (Some(347.16), Some(347.23)));
        close(b.mid().unwrap(), MID, "mid");
        // 0.07 / 347.195 × 1e4 (bc: 2.016158066792436…)
        close(b.spread_bps().unwrap(), 2.016_158_066_792_436_5, "spread");
        assert_eq!(b.age_ms(1_790_775_353_605), 1_000);
        assert_eq!(b.age_ms(0), 0, "saturating");
    }

    /// Hand-computed with awk and bc outside the repo (meta.json `walks`).
    #[test]
    fn fixture_walks_match_hand_computed_vwaps() {
        let b = tsla_book();
        struct V {
            side: Side,
            target: WalkTarget,
            limit: Option<f64>,
            filled_qty: f64,
            filled_notional: f64,
            vwap: f64,
            worst: f64,
            levels: usize,
            slip_mid: f64,
            slip_touch: f64,
            unfilled: f64,
            end: WalkEnd,
        }
        let vectors = [
            V {
                side: Side::Buy,
                target: WalkTarget::Notional(1_000.0),
                limit: None,
                filled_qty: 2.879_935_489_445_036,
                filled_notional: 1_000.0,
                vwap: 347.23,
                worst: 347.23,
                levels: 1,
                slip_mid: 1.008_079_033_396_218,
                slip_touch: 0.0,
                unfilled: 0.0,
                end: WalkEnd::Filled,
            },
            V {
                side: Side::Buy,
                target: WalkTarget::Notional(10_000.0),
                limit: None,
                filled_qty: 28.799_120_521_829_282,
                filled_notional: 10_000.0,
                vwap: 347.232_825_822_585_68,
                worst: 347.24,
                levels: 2,
                slip_mid: 1.089_469_104_845_391,
                slip_touch: 0.081_381_867_513_739,
                unfilled: 0.0,
                end: WalkEnd::Filled,
            },
            V {
                side: Side::Buy,
                target: WalkTarget::Qty(100.0),
                limit: None,
                filled_qty: 100.0,
                filled_notional: 34_726.732_71,
                vwap: 347.267_327_1,
                worst: 347.3,
                levels: 6,
                slip_mid: 2.083_183_801_610_046,
                slip_touch: 1.074_996_400_081,
                unfilled: 0.0,
                end: WalkEnd::Filled,
            },
            V {
                side: Side::Sell,
                target: WalkTarget::Qty(100.0),
                limit: None,
                filled_qty: 100.0,
                filled_notional: 34_715.638_28,
                vwap: 347.156_382_8,
                worst: 347.11,
                levels: 4,
                slip_mid: 1.112_262_561_384,
                slip_touch: 0.104_194_031_571,
                unfilled: 0.0,
                end: WalkEnd::Filled,
            },
            V {
                side: Side::Sell,
                target: WalkTarget::Notional(50_000.0),
                limit: None,
                filled_qty: 144.033_193_281_669,
                filled_notional: 50_000.0,
                vwap: 347.142_202_854_733,
                worst: 347.11,
                levels: 4,
                slip_mid: 1.520_677_004_763,
                slip_touch: 0.512_649_650_504,
                unfilled: 0.0,
                end: WalkEnd::Filled,
            },
            // More than the visible asks (530.752): partial, never hidden liquidity.
            V {
                side: Side::Buy,
                target: WalkTarget::Qty(1_000.0),
                limit: None,
                filled_qty: 530.752,
                filled_notional: 184_357.911_6,
                vwap: 347.352_269_233_088,
                worst: 347.44,
                levels: 20,
                slip_mid: 4.529_709_042_129,
                slip_touch: 3.521_275_036_378,
                unfilled: 469.248,
                end: WalkEnd::Depth,
            },
            V {
                side: Side::Buy,
                target: WalkTarget::Qty(100.0),
                limit: Some(347.27),
                filled_qty: 61.594,
                filled_notional: 21_388.573_25,
                vwap: 347.250_921_355_976,
                worst: 347.27,
                levels: 4,
                slip_mid: 1.610_661_327_964,
                slip_touch: 0.602_521_555_632,
                unfilled: 38.406,
                end: WalkEnd::Limit,
            },
            V {
                side: Side::Sell,
                target: WalkTarget::Qty(100.0),
                limit: Some(347.12),
                filled_qty: 98.15,
                filled_notional: 34_073.484_78,
                vwap: 347.157_257_055_527,
                worst: 347.12,
                levels: 3,
                slip_mid: 1.087_082_028_045,
                slip_touch: 0.079_010_959_579,
                unfilled: 1.85,
                end: WalkEnd::Limit,
            },
        ];
        for v in vectors {
            let w = b.walk(v.side, v.target, v.limit).unwrap();
            let what = format!("{:?} {:?} limit {:?}", v.side, v.target, v.limit);
            assert_eq!(w.end, v.end, "{what}");
            assert_eq!(w.levels_used, v.levels, "{what}");
            assert_eq!(w.fills.len(), v.levels, "{what}");
            assert_eq!(w.worst_px, Some(v.worst), "{what}");
            close(w.filled_qty, v.filled_qty, &format!("{what} filled_qty"));
            close(
                w.filled_notional,
                v.filled_notional,
                &format!("{what} notional"),
            );
            close(w.vwap.unwrap(), v.vwap, &format!("{what} vwap"));
            // bps values are hand-computed to 12 decimals.
            let bps_close = |a: f64, b: f64, n: &str| {
                assert!((a - b).abs() < 1e-9, "{what} {n}: got {a}, want {b}")
            };
            bps_close(w.slippage_bps_vs_mid.unwrap(), v.slip_mid, "slip vs mid");
            bps_close(
                w.slippage_bps_vs_touch.unwrap(),
                v.slip_touch,
                "slip vs touch",
            );
            close(w.unfilled, v.unfilled, &format!("{what} unfilled"));
            assert_eq!(w.is_complete(), v.end == WalkEnd::Filled);
        }
    }

    #[test]
    fn walk_fills_level_by_level_and_notional_is_exact() {
        let b = tsla_book();
        let w = b.walk(Side::Sell, WalkTarget::Qty(100.0), None).unwrap();
        let got: Vec<(usize, f64, f64)> = w.fills.iter().map(|f| (f.level, f.px, f.qty)).collect();
        let want = [
            (0, 347.16, 85.304),
            (1, 347.14, 12.231),
            (2, 347.12, 0.615),
            (3, 347.11, 1.85),
        ];
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert_eq!((g.0, g.1), (w.0, w.1));
            close(g.2, w.2, "level qty");
        }
        // A notional walk takes exactly the notional asked for.
        let w = b
            .walk(Side::Buy, WalkTarget::Notional(100.0), None)
            .unwrap();
        close(w.filled_notional, 100.0, "notional");
        assert_eq!(w.slippage_bps_vs_touch, Some(0.0));
    }

    #[test]
    fn empty_and_zero_walks_carry_no_numbers() {
        let empty = L2Book::new(vec![], vec![], 0).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.mid(), None);
        let w = empty.walk(Side::Buy, WalkTarget::Qty(1.0), None).unwrap();
        assert_eq!(w.end, WalkEnd::Depth);
        assert_eq!((w.vwap, w.worst_px), (None, None));
        assert_eq!(w.slippage_bps_vs_mid, None, "missing is never 0");
        assert_eq!(w.slippage_bps_vs_touch, None);
        assert_eq!(w.unfilled, 1.0);

        let b = tsla_book();
        let w = b.walk(Side::Buy, WalkTarget::Qty(0.0), None).unwrap();
        assert_eq!(w.end, WalkEnd::Filled);
        assert_eq!((w.filled_qty, w.vwap, w.levels_used), (0.0, None, 0));

        // A one-sided book walks but has no mid-relative slippage.
        let asks_only = L2Book::new(vec![], vec![lvl(10.0, 1.0)], 0).unwrap();
        let w = asks_only
            .walk(Side::Buy, WalkTarget::Qty(1.0), None)
            .unwrap();
        assert_eq!((w.vwap, w.slippage_bps_vs_mid), (Some(10.0), None));
    }

    #[test]
    fn walk_rejects_bad_arguments() {
        let b = tsla_book();
        for t in [WalkTarget::Qty(-1.0), WalkTarget::Notional(f64::NAN)] {
            assert!(matches!(b.walk(Side::Buy, t, None), Err(BookError::Arg(_))));
        }
        assert!(matches!(
            b.walk(Side::Buy, WalkTarget::Qty(1.0), Some(0.0)),
            Err(BookError::Arg(_))
        ));
        assert!(matches!(
            walk(&b.asks, Side::Buy, WalkTarget::Qty(1.0), None, Some(-1.0)),
            Err(BookError::Arg(_))
        ));
    }

    #[test]
    fn residue_of_decimal_sizes_counts_as_filled() {
        // 0.1 + 0.2 != 0.3 in f64; the walk must still end `filled`.
        let asks = vec![lvl(10.0, 0.1), lvl(11.0, 0.2), lvl(12.0, 5.0)];
        let b = L2Book::new(vec![lvl(9.0, 1.0)], asks, 0).unwrap();
        let w = b.walk(Side::Buy, WalkTarget::Qty(0.3), None).unwrap();
        assert_eq!(
            (w.end, w.levels_used, w.unfilled),
            (WalkEnd::Filled, 2, 0.0)
        );
        close(w.vwap.unwrap(), (1.0 + 2.2) / 0.3, "vwap");
    }

    #[test]
    fn depth_within_matches_hand_computed_sums() {
        let b = tsla_book();
        // 2 bps: bids >= 347.125561, asks <= 347.264439.
        let d = depth_within(&b, 2.0).unwrap();
        assert_eq!((d.bid_levels, d.ask_levels), (2, 3));
        close(d.bid_usd, 33_860.005_98, "bid 2 bps");
        close(d.ask_usd, 11_310.103_31, "ask 2 bps");
        close(
            d.imbalance().unwrap(),
            (33_860.005_98 - 11_310.103_31) / (33_860.005_98 + 11_310.103_31),
            "imbalance",
        );
        assert_eq!(d.for_taker(Side::Buy), d.ask_usd);
        // 5 bps: bids >= 347.0214025 (10 levels), asks <= 347.3685975 (12).
        let d = depth_within(&b, 5.0).unwrap();
        assert_eq!((d.bid_levels, d.ask_levels), (10, 12));
        close(d.bid_usd, 151_769.130_46, "bid 5 bps");
        close(d.ask_usd, 97_621.231_58, "ask 5 bps");
        // 100 bps covers the whole visible book.
        let d = depth_within(&b, 100.0).unwrap();
        assert_eq!((d.bid_levels, d.ask_levels), (20, 20));
        close(d.bid_usd, 259_662.541_83, "all bids");
        close(d.ask_usd, 184_357.911_6, "all asks");
        // 0 bps: nothing rests exactly at mid.
        let d = depth_within(&b, 0.0).unwrap();
        assert_eq!((d.bid_usd, d.ask_usd, d.imbalance()), (0.0, 0.0, None));

        assert!(matches!(depth_within(&b, -1.0), Err(BookError::Arg(_))));
        let one_sided = L2Book::new(vec![lvl(9.0, 1.0)], vec![], 0).unwrap();
        assert_eq!(depth_within(&one_sided, 5.0), Err(BookError::NoMid));
    }

    #[test]
    fn new_rejects_invalid_books() {
        let unordered = L2Book::new(vec![lvl(9.0, 1.0), lvl(9.5, 1.0)], vec![], 0);
        assert!(matches!(
            unordered,
            Err(BookError::Level {
                side: "bid",
                index: 1,
                ..
            })
        ));
        let dup = L2Book::new(vec![], vec![lvl(10.0, 1.0), lvl(10.0, 2.0)], 0);
        assert!(matches!(
            dup,
            Err(BookError::Level {
                side: "ask",
                index: 1,
                ..
            })
        ));
        let zero = L2Book::new(vec![lvl(9.0, 0.0)], vec![], 0);
        assert!(matches!(zero, Err(BookError::Level { index: 0, .. })));
        let crossed = L2Book::new(vec![lvl(10.0, 1.0)], vec![lvl(10.0, 1.0)], 0);
        assert!(matches!(crossed, Err(BookError::Crossed { .. })));
    }

    #[test]
    fn level_reads_decimal_strings_and_numbers() {
        let l: L2Level = serde_json::from_str(r#"{"px":"347.16","sz":"85.304","n":1}"#).unwrap();
        assert_eq!(
            l,
            L2Level {
                px: 347.16,
                sz: 85.304,
                n: 1
            }
        );
        let l: L2Level = serde_json::from_str(r#"{"px":347.16,"sz":2}"#).unwrap();
        assert_eq!(
            l,
            L2Level {
                px: 347.16,
                sz: 2.0,
                n: 0
            }
        );
        for bad in [r#"{"px":"x","sz":"1"}"#, r#"{"px":"inf","sz":"1"}"#] {
            assert!(serde_json::from_str::<L2Level>(bad).is_err(), "{bad}");
        }
        // Round trip through the observation payload keeps the values.
        let b = tsla_book();
        let back: L2Book = serde_json::from_value(serde_json::to_value(&b).unwrap()).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn side_helpers() {
        assert_eq!(Side::parse("buy"), Some(Side::Buy));
        assert_eq!(Side::parse("long"), None);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
        assert_eq!((Side::Buy.sign(), Side::Sell.sign()), (1.0, -1.0));
        assert_eq!(serde_json::to_value(Side::Sell).unwrap(), "sell");
    }
}
