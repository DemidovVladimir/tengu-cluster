//! `hl_book` — `l2Book {coin}` (+ `recentTrades {coin}`) → the
//! `hl_book/1:hyperliquid:<coin>` row. Walks and depth come from
//! `domain/book.rs` (never re-implemented here); `data` keeps the levels
//! (≤ 20 per side, full precision, HL's `time`) so the paper fill engine and
//! replay re-walk the same book.
//!
//! | Status | When |
//! |---|---|
//! | `absent` | HL does not know the coin (`200 null`, `500 null`), or both sides are empty (`book_empty`: delisted / halted) |
//! | `error` | the book read failed or did not decode (crossed, unordered, bad decimals) |
//! | `partial` | one side empty, or `include_trades` and `recentTrades` failed |
//! | `ok` | otherwise |
//!
//! | Feature | Rule — a missing input ⇒ omitted, never 0 |
//! |---|---|
//! | `bid`, `ask`, `mid`, `spread_bps` | touch prices; `(ask − bid) / mid · 1e4` |
//! | `depth_usd_10bps_bid` / `_ask`, `depth_usd_50bps_bid` / `_ask` | Σ px·sz resting within 10 / 50 bps of mid (`book::depth_within`) — visible levels only: equal to `depth_usd_<side>` ⇒ the band reaches past the 20 levels (a lower bound) |
//! | `imbalance_10bps` | `(bid − ask) / (bid + ask)` of the 10 bps depth |
//! | `depth_usd_bid` / `_ask`, `bid_levels` / `ask_levels` | every visible level |
//! | `notional_usd_k`, `buy_slip_bps_k`, `sell_slip_bps_k` (k = 1..3) | the k-th requested notional; taker VWAP slippage vs mid of a walk that fills it — a notional beyond the visible depth has no slippage |
//! | `book_empty` | both sides empty |
//! | `venue_ts_ms`, `book_age_ms` | HL's `time`; receive time − `time` (≥ 0) |
//! | `last`, `last_age_s` | newest `recentTrades` price and its age at receive time |

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::book::{depth_within, L2Book, L2Level, Side, Walk, WalkTarget};
use crate::domain::lp::market::fmt_sig;
use crate::domain::market::{decimal_value, InstrumentId, Listing};
use crate::domain::observation::{
    set_bool, set_int, set_num, ErrorClass, Features, ObsStatus, Observed, ReadError,
};

/// `hl_book/1` TTL.
pub(crate) const BOOK_TTL_MS: u64 = 2_000;
/// Levels kept per side (HL's `l2Book` sends ≤ 20).
pub(crate) const MAX_LEVELS: usize = 20;
/// Most notionals one call walks.
pub(crate) const MAX_NOTIONALS: usize = 3;
/// Notionals walked when the caller names none.
pub(crate) const DEFAULT_NOTIONALS_USD: [f64; 3] = [100.0, 1_000.0, 10_000.0];
/// `ReadError.field` of a failed `recentTrades` read or decode.
pub(crate) const LAST_FIELD: &str = "last";

/// The newest trade of `recentTrades`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct LastTrade {
    pub px: f64,
    pub sz: f64,
    /// HL trade `time` (ms).
    pub ts_ms: i64,
}

fn decode(field: &str, message: impl Into<String>) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, message)
}

/// `l2Book {coin}` reply → a validated book (`domain::book` invariants), ≤
/// [`MAX_LEVELS`] per side. `[[], []]` (delisted / halted) is an empty book.
pub(crate) fn decode_l2_book(coin: &str, v: &Value) -> Result<L2Book, ReadError> {
    if let Some(c) = v.get("coin").and_then(Value::as_str) {
        if c != coin {
            return Err(decode(
                "book",
                format!("l2Book reply is for {c}, asked {coin}"),
            ));
        }
    }
    let time = v["time"]
        .as_i64()
        .ok_or_else(|| decode("book", format!("l2Book for {coin} has no time")))?;
    let sides = v["levels"]
        .as_array()
        .filter(|s| s.len() == 2)
        .ok_or_else(|| {
            decode(
                "book",
                format!("l2Book for {coin}: levels is not [bids, asks]"),
            )
        })?;
    let side = |i: usize, name: &str| -> Result<Vec<L2Level>, ReadError> {
        let levels = sides[i]
            .as_array()
            .ok_or_else(|| decode("book", format!("l2Book for {coin}: {name} is not a list")))?;
        levels
            .iter()
            .take(MAX_LEVELS)
            .map(|l| {
                serde_json::from_value::<L2Level>(l.clone()).map_err(|e| {
                    decode("book", format!("l2Book for {coin}: {name} level {l}: {e}"))
                })
            })
            .collect()
    };
    let (bids, asks) = (side(0, "bids")?, side(1, "asks")?);
    L2Book::new(bids, asks, time).map_err(|e| decode("book", format!("l2Book for {coin}: {e}")))
}

/// `recentTrades {coin}` reply → the newest trade (largest `time`); `None`
/// for an empty list.
pub(crate) fn decode_last_trade(v: &Value) -> Result<Option<LastTrade>, ReadError> {
    let trades = v
        .as_array()
        .ok_or_else(|| decode(LAST_FIELD, "recentTrades reply is not a list"))?;
    let mut newest: Option<LastTrade> = None;
    for t in trades {
        let (Some(px), Some(sz), Some(ts_ms)) = (
            t.get("px").and_then(decimal_value),
            t.get("sz").and_then(decimal_value),
            t["time"].as_i64(),
        ) else {
            return Err(decode(
                LAST_FIELD,
                format!("recentTrades entry {t} has no px / sz / time"),
            ));
        };
        if newest.is_none_or(|n| ts_ms > n.ts_ms) {
            newest = Some(LastTrade { px, sz, ts_ms });
        }
    }
    Ok(newest)
}

/// `hl_book/1:hyperliquid:<coin>` — one L2 snapshot plus what the caller
/// asked to derive from it (module tables).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HlBook {
    pub id: InstrumentId,
    /// `listed`, or `not_found` (HL `200 null`); an empty book stays listed.
    pub listing: Listing,
    /// Receive time of the reply (ms): `book_age_ms`, `last_age_s` use it.
    pub read_ms: i64,
    /// `None`: not found, or the read failed (`errors`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<L2Book>,
    /// USD notionals the slippage features walk (≤ 3, request order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notional_usd: Vec<f64>,
    /// Newest trade (`include_trades`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastTrade>,
    /// `book` (read / decode) and `last` (`recentTrades`) failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

fn pos(x: f64) -> Option<f64> {
    (x.is_finite() && x > 0.0).then_some(x)
}

impl HlBook {
    pub(crate) fn of(id: InstrumentId, read_ms: i64, book: L2Book, notional_usd: Vec<f64>) -> Self {
        Self {
            id,
            listing: Listing::Listed,
            read_ms,
            book: Some(book),
            notional_usd,
            last: None,
            errors: Vec::new(),
        }
    }

    /// HL does not know the coin.
    pub(crate) fn not_found(id: InstrumentId, read_ms: i64, notional_usd: Vec<f64>) -> Self {
        Self {
            id,
            listing: Listing::NotFound,
            read_ms,
            book: None,
            notional_usd,
            last: None,
            errors: Vec::new(),
        }
    }

    /// The book read failed.
    pub(crate) fn failed(
        id: InstrumentId,
        read_ms: i64,
        notional_usd: Vec<f64>,
        error: ReadError,
    ) -> Self {
        Self {
            listing: Listing::Listed,
            errors: vec![error],
            ..Self::not_found(id, read_ms, notional_usd)
        }
    }

    /// The taker walk for `notional_usd` on `side` when the visible book
    /// fills it completely (slippage vs mid needs both sides).
    pub(crate) fn filled_walk(&self, side: Side, notional_usd: f64) -> Option<Walk> {
        let w = self
            .book
            .as_ref()?
            .walk(side, WalkTarget::Notional(pos(notional_usd)?), None)
            .ok()?;
        w.is_complete().then_some(w)
    }
}

impl Observed for HlBook {
    const SCHEMA: &'static str = "hl_book/1";

    fn subject(&self) -> String {
        self.id.to_string()
    }

    fn headline(&self) -> String {
        let id = &self.id;
        if self.listing == Listing::NotFound {
            return format!("book {id} not listed by the venue");
        }
        let Some(b) = &self.book else {
            return format!("book {id} unavailable");
        };
        if b.is_empty() {
            return format!("book {id} empty (no levels: delisted or halted)");
        }
        let px = |x: Option<f64>| x.map_or_else(|| "none".to_string(), |v| v.to_string());
        let mut h = format!(
            "book {id} bid={} ask={}",
            px(b.best_bid()),
            px(b.best_ask())
        );
        if let Some(s) = b.spread_bps() {
            h.push_str(&format!(" spread_bps={s:.2}"));
        }
        if let Ok(d) = depth_within(b, 10.0) {
            h.push_str(&format!(
                " depth_10bps_usd={}/{}",
                fmt_sig(d.bid_usd, 6),
                fmt_sig(d.ask_usd, 6)
            ));
        }
        h.push_str(&format!(" levels={}/{}", b.bids.len(), b.asks.len()));
        if let Some(t) = self.last {
            h.push_str(&format!(" last={}", t.px));
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let Some(b) = &self.book else {
            return f;
        };
        set_num(&mut f, "bid", b.best_bid());
        set_num(&mut f, "ask", b.best_ask());
        set_num(&mut f, "mid", b.mid());
        set_num(&mut f, "spread_bps", b.spread_bps());
        for (bps, name) in [(10.0, "10bps"), (50.0, "50bps")] {
            if let Ok(d) = depth_within(b, bps) {
                set_num(&mut f, &format!("depth_usd_{name}_bid"), Some(d.bid_usd));
                set_num(&mut f, &format!("depth_usd_{name}_ask"), Some(d.ask_usd));
                if bps == 10.0 {
                    set_num(&mut f, "imbalance_10bps", d.imbalance());
                }
            }
        }
        let visible = |levels: &[L2Level]| levels.iter().map(L2Level::notional).sum::<f64>();
        set_num(&mut f, "depth_usd_bid", Some(visible(&b.bids)));
        set_num(&mut f, "depth_usd_ask", Some(visible(&b.asks)));
        set_int(&mut f, "bid_levels", Some(b.bids.len() as i64));
        set_int(&mut f, "ask_levels", Some(b.asks.len() as i64));
        for (k, n) in self.notional_usd.iter().take(MAX_NOTIONALS).enumerate() {
            let k = k + 1;
            set_num(&mut f, &format!("notional_usd_{k}"), Some(*n));
            for (side, name) in [(Side::Buy, "buy"), (Side::Sell, "sell")] {
                let slip = self
                    .filled_walk(side, *n)
                    .and_then(|w| w.slippage_bps_vs_mid);
                set_num(&mut f, &format!("{name}_slip_bps_{k}"), slip);
            }
        }
        set_bool(&mut f, "book_empty", Some(b.is_empty()));
        set_int(&mut f, "venue_ts_ms", Some(b.venue_ts_ms));
        set_int(&mut f, "book_age_ms", Some(b.age_ms(self.read_ms) as i64));
        if let Some(t) = self.last {
            set_num(&mut f, "last", Some(t.px));
            let age_ms = self.read_ms.saturating_sub(t.ts_ms).max(0);
            set_num(&mut f, "last_age_s", Some(age_ms as f64 / 1000.0));
        }
        f
    }

    fn status(&self) -> ObsStatus {
        if self.listing == Listing::NotFound {
            return ObsStatus::Absent;
        }
        let Some(b) = &self.book else {
            return ObsStatus::Error;
        };
        if b.is_empty() {
            ObsStatus::Absent
        } else if b.bids.is_empty() || b.asks.is_empty() || !self.errors.is_empty() {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::observation::{
        assert_features_ok, ObsSource, Observation, MAX_FEATURES, MAX_LINE1_CHARS,
    };
    use serde_json::json;

    /// Live `l2Book xyz:TSLA` with hand-computed goldens (`tests/fixtures/xm/`).
    pub(crate) const L2_TSLA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xm/l2_xyz_TSLA.json"
    ));
    pub(crate) const GOLDENS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xm/meta.json"
    ));
    pub(crate) const L2_FLX_TSLA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/l2Book_flx_TSLA.json"
    ));
    pub(crate) const TRADES_TSLA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/recentTrades_xyz_TSLA.json"
    ));
    /// The fixture's venue time + 1 s.
    pub(crate) const READ_MS: i64 = 1_790_775_353_605;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn tsla() -> InstrumentId {
        InstrumentId::hyperliquid("xyz:TSLA").unwrap()
    }

    fn book(notionals: &[f64]) -> HlBook {
        let b = decode_l2_book("xyz:TSLA", &j(L2_TSLA)).unwrap();
        HlBook::of(tsla(), READ_MS, b, notionals.to_vec())
    }

    fn num(f: &Features, k: &str) -> f64 {
        f[k].as_f64()
            .unwrap_or_else(|| panic!("{k} missing in {f:?}"))
    }

    /// `meta.json` golden: the walk with `side` and target `{kind: amount}`.
    fn golden_slip(side: &str, kind: &str, amount: f64) -> f64 {
        let g = j(GOLDENS);
        g["walks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["side"] == side && w["target"][kind].as_f64() == Some(amount))
            .unwrap_or_else(|| panic!("no golden {side} {kind} {amount}"))["slippage_bps_vs_mid"]
            .as_f64()
            .unwrap()
    }

    fn bps_close(a: f64, b: f64, what: &str) {
        assert!((a - b).abs() < 1e-9, "{what}: got {a}, want {b}");
    }

    #[test]
    fn features_match_the_hand_computed_goldens() {
        let g = j(GOLDENS);
        let o = Observation::of(
            "hl_book",
            &book(&[1_000.0, 10_000.0, 50_000.0]),
            READ_MS,
            BOOK_TTL_MS,
            ObsSource::Live,
        );
        assert_eq!(o.key, "hl_book/1:hyperliquid:xyz:TSLA");
        assert_eq!(o.status, ObsStatus::Ok);
        let f = &o.features;
        assert_features_ok(f);
        assert!(f.len() <= MAX_FEATURES);
        assert_eq!((num(f, "bid"), num(f, "ask")), (347.16, 347.23));
        assert!((num(f, "mid") - 347.195).abs() < 1e-9);
        bps_close(
            num(f, "spread_bps"),
            g["spread_bps"].as_f64().unwrap(),
            "spread",
        );
        // Buy $1 000 / $10 000 and sell $50 000: meta.json `walks`.
        bps_close(
            num(f, "buy_slip_bps_1"),
            golden_slip("buy", "notional", 1_000.0),
            "buy 1k",
        );
        bps_close(
            num(f, "buy_slip_bps_2"),
            golden_slip("buy", "notional", 10_000.0),
            "buy 10k",
        );
        bps_close(
            num(f, "sell_slip_bps_3"),
            golden_slip("sell", "notional", 50_000.0),
            "sell 50k",
        );
        // $1 000 / $10 000 sells stay at the touch: the half spread.
        bps_close(
            num(f, "sell_slip_bps_1"),
            golden_slip("buy", "notional", 1_000.0),
            "sell 1k",
        );
        bps_close(
            num(f, "sell_slip_bps_2"),
            golden_slip("buy", "notional", 1_000.0),
            "sell 10k",
        );
        assert_eq!(num(f, "notional_usd_3"), 50_000.0);
        // 10 and 50 bps both reach past the 20 visible levels (lowest bid
        // 346.93 ≥ 346.847805, highest ask 347.44 ≤ 347.542195): the
        // `depth_within` 100 bps golden.
        let all = &g["depth_within"][2];
        let (bid, ask) = (
            all["bid_usd"].as_f64().unwrap(),
            all["ask_usd"].as_f64().unwrap(),
        );
        for k in [
            "depth_usd_10bps_bid",
            "depth_usd_50bps_bid",
            "depth_usd_bid",
        ] {
            assert!((num(f, k) - bid).abs() < 1e-6, "{k}");
        }
        for k in [
            "depth_usd_10bps_ask",
            "depth_usd_50bps_ask",
            "depth_usd_ask",
        ] {
            assert!((num(f, k) - ask).abs() < 1e-6, "{k}");
        }
        bps_close(
            num(f, "imbalance_10bps"),
            (bid - ask) / (bid + ask),
            "imbalance",
        );
        assert_eq!(
            (f["bid_levels"].clone(), f["ask_levels"].clone()),
            (json!(20), json!(20))
        );
        assert_eq!(f["venue_ts_ms"], 1_790_775_352_605i64);
        assert_eq!(f["book_age_ms"], 1_000);
        assert_eq!(f["book_empty"], false);
        assert!(!f.contains_key("last"), "no trades asked");
        assert_eq!(
            o.headline,
            "book hyperliquid:xyz:TSLA bid=347.16 ask=347.23 spread_bps=2.02 depth_10bps_usd=259663/184358 levels=20/20"
        );
        let text = o.render_text(READ_MS);
        assert!(text.lines().next().unwrap().chars().count() <= MAX_LINE1_CHARS);
        // `data` keeps the levels: the typed row re-walks to the same numbers.
        let back: HlBook = o.typed().unwrap();
        assert_eq!(back, book(&[1_000.0, 10_000.0, 50_000.0]));
        assert_eq!(
            o.data["book"]["bids"][0],
            json!({"px": 347.16, "sz": 85.304, "n": 1})
        );
    }

    #[test]
    fn notionals_beyond_visible_depth_have_no_slippage() {
        // Visible asks $184 358, bids $259 663.
        let f = book(&[200_000.0, 300_000.0]).features();
        assert!(
            !f.contains_key("buy_slip_bps_1"),
            "never a partial-fill number"
        );
        assert!(f.contains_key("sell_slip_bps_1"));
        assert!(!f.contains_key("buy_slip_bps_2") && !f.contains_key("sell_slip_bps_2"));
        assert!(!f.contains_key("notional_usd_3"));
        let none = book(&[]).features();
        assert!(!none
            .keys()
            .any(|k| k.contains("slip") || k.starts_with("notional")));
    }

    #[test]
    fn empty_one_sided_missing_and_failed_books() {
        let flx = InstrumentId::hyperliquid("flx:TSLA").unwrap();
        let empty = decode_l2_book("flx:TSLA", &j(L2_FLX_TSLA)).unwrap();
        let e = HlBook::of(flx, READ_MS, empty, DEFAULT_NOTIONALS_USD.to_vec());
        assert_eq!(e.status(), ObsStatus::Absent);
        assert_eq!(
            e.headline(),
            "book hyperliquid:flx:TSLA empty (no levels: delisted or halted)"
        );
        let f = e.features();
        assert_eq!(f["book_empty"], true);
        for k in [
            "bid",
            "ask",
            "mid",
            "spread_bps",
            "buy_slip_bps_1",
            "imbalance_10bps",
        ] {
            assert!(!f.contains_key(k), "{k}");
        }

        let mut one_sided = book(&[100.0]);
        one_sided.book.as_mut().unwrap().asks.clear();
        assert_eq!(one_sided.status(), ObsStatus::Partial);
        let f = one_sided.features();
        assert!(f.contains_key("bid") && !f.contains_key("mid"));
        assert!(!f.contains_key("sell_slip_bps_1"), "no mid, no slippage");

        let nope = InstrumentId::hyperliquid("xyz:NOPE").unwrap();
        let nf = HlBook::not_found(nope.clone(), READ_MS, vec![]);
        assert_eq!(nf.status(), ObsStatus::Absent);
        assert_eq!(
            nf.headline(),
            "book hyperliquid:xyz:NOPE not listed by the venue"
        );
        assert!(nf.features().is_empty());

        let err = ReadError::new("book", ErrorClass::Timeout, "slow");
        let failed = HlBook::failed(nope, READ_MS, vec![], err);
        assert_eq!(failed.status(), ObsStatus::Error);
        assert_eq!(failed.headline(), "book hyperliquid:xyz:NOPE unavailable");
        let o = Observation::of("hl_book", &failed, READ_MS, BOOK_TTL_MS, ObsSource::Live);
        assert_eq!(o.typed::<HlBook>().unwrap(), failed);
        assert!(o.data.get("book").is_none());

        let mut trades_failed = book(&[]);
        trades_failed.errors.push(ReadError::new(
            "last",
            ErrorClass::RateLimited,
            "recentTrades: HTTP 429",
        ));
        assert_eq!(trades_failed.status(), ObsStatus::Partial);
    }

    #[test]
    fn replies_decode_strictly() {
        let ok = decode_l2_book("xyz:TSLA", &j(L2_TSLA)).unwrap();
        assert_eq!((ok.bids.len(), ok.asks.len()), (20, 20));
        let wrong_coin = decode_l2_book("xyz:NVDA", &j(L2_TSLA)).unwrap_err();
        assert!(
            wrong_coin.message.contains("is for xyz:TSLA"),
            "{wrong_coin:?}"
        );
        let crossed = json!({"coin": "x", "time": 1, "levels": [
            [{"px": "10.0", "sz": "1", "n": 1}], [{"px": "9.0", "sz": "1", "n": 1}]]});
        let e = decode_l2_book("x", &crossed).unwrap_err();
        assert_eq!(e.class, ErrorClass::Decode);
        assert!(e.message.contains("crossed book"), "{e:?}");
        for bad in [
            json!({"coin": "x", "levels": [[], []]}),
            json!({"coin": "x", "time": 1, "levels": [[]]}),
            json!({"coin": "x", "time": 1, "levels": [[{"px": "abc", "sz": "1"}], []]}),
        ] {
            assert_eq!(
                decode_l2_book("x", &bad).unwrap_err().class,
                ErrorClass::Decode,
                "{bad}"
            );
        }
        // More than 20 levels: the first 20 are kept.
        let many: Vec<Value> = (0..25)
            .map(|i| json!({"px": format!("{}", 100 - i), "sz": "1", "n": 1}))
            .collect();
        let deep = decode_l2_book("x", &json!({"time": 5, "levels": [many, []]})).unwrap();
        assert_eq!(deep.bids.len(), MAX_LEVELS);

        let last = decode_last_trade(&j(TRADES_TSLA)).unwrap().unwrap();
        assert_eq!(
            last,
            LastTrade {
                px: 349.77,
                sz: 0.029,
                ts_ms: 1_790_779_608_796
            }
        );
        assert_eq!(decode_last_trade(&json!([])).unwrap(), None);
        assert!(decode_last_trade(&json!({})).is_err());
        assert!(decode_last_trade(&json!([{"px": "1"}])).is_err());
    }

    #[test]
    fn last_trade_features_and_headline() {
        let mut b = book(&[]);
        let t = decode_last_trade(&j(TRADES_TSLA)).unwrap().unwrap();
        b.read_ms = t.ts_ms + 2_500;
        b.last = Some(t);
        let f = b.features();
        assert_eq!((num(&f, "last"), num(&f, "last_age_s")), (349.77, 2.5));
        assert!(b.headline().ends_with(" last=349.77"), "{}", b.headline());
        assert_features_ok(&f);
        assert!(f.len() <= MAX_FEATURES);
        // Every feature at once still fits the contract.
        b.notional_usd = DEFAULT_NOTIONALS_USD.to_vec();
        let f = b.features();
        assert_eq!(f.len(), 27);
    }
}
