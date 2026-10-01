//! Market-data decoders (xlab, `docs/xlab-2026-10-01.md` § 1, § 4): venue
//! replies and files → the rows of `domain/marketdata.rs`. Pure — the
//! fetchers and importers (`adapters/outbound/backfill/`) hand in the reply
//! or the file text and the fetch time.
//!
//! | Input | Decoder | Rows |
//! |---|---|---|
//! | HL `candleSnapshot`: `[{t, T, s, i, o, c, h, l, v, n}]`, decimals as strings | [`hl_candles`] | `Bar`s, ascending |
//! | HL `fundingHistory`: `[{coin, fundingRate, premium, time}]` | [`hl_funding`] | `FundingPoint`s, ascending |
//! | GeckoTerminal OHLCV: `data.attributes.ohlcv_list` = `[[t_open_s, o, h, l, c, v], …]`, newest first | [`gecko_ohlcv`] | `Bar`s, ascending (`v` in USD, no `n`) |
//! | HL archive `asset_ctxs/<YYYYMMDD>.csv`: `time,coin,funding,open_interest,prev_day_px,day_ntl_vlm,premium,oracle_px,mark_px,mid_px,impact_bid_px,impact_ask_px` | [`hl_asset_ctxs_csv`] | `(coin, CtxPoint)` per row (`prev_day_px` unused) |
//! | JSON dataset file: `[{instrument, interval?, source?, bars?, funding?}]` | [`json_datasets`] | [`Dataset`]s |
//!
//! | Rule | Value |
//! |---|---|
//! | A bad row | an error naming its row (or line) and time — never a silent 0, never skipped |
//! | Bars | `Bar::validate` and `t_open_ms` on the interval's UTC grid (`BarSeries::close_at` finds a bar at `instant − interval`) |
//! | A bar still open at fetch time | dropped by [`closed_bars`] — never stored |
//! | Empty CSV cell | `None` (HL leaves `mid_px` / impact prices empty for a market without a book) |
//! | HL coin | [`hl_coin`]: the native id of a `hyperliquid:` instrument, verbatim (`xyz:TSLA`, `SOL`) |

use std::fmt::Display;

use serde::Deserialize;
use serde_json::Value;

use crate::domain::market::{decimal_value, parse_decimal, InstrumentId, HYPERLIQUID};
use crate::domain::marketdata::{fmt_time, Bar, CtxPoint, FundingPoint, Interval};

/// The HL coin of a `hyperliquid:` instrument id (`hyperliquid:xyz:TSLA` →
/// `xyz:TSLA`).
pub(crate) fn hl_coin(instrument: &str) -> Result<String, String> {
    let id = InstrumentId::parse(instrument)?;
    if id.venue() != HYPERLIQUID {
        return Err(format!(
            "`{instrument}` is not a Hyperliquid instrument (hyperliquid:<coin>)"
        ));
    }
    Ok(id.native().to_string())
}

/// The bars closed at `now_ms` (`t_open + interval ≤ now`), order kept — a
/// bar still open at fetch time is never stored.
pub(crate) fn closed_bars(bars: Vec<Bar>, interval: Interval, now_ms: i64) -> Vec<Bar> {
    bars.into_iter()
        .filter(|b| b.t_close_ms(interval) <= now_ms)
        .collect()
}

/// `Bar::validate` + the interval grid.
pub(crate) fn check_bar(bar: &Bar, interval: Interval) -> Result<(), String> {
    bar.validate()?;
    if bar.t_open_ms.rem_euclid(interval.ms()) != 0 {
        return Err(format!(
            "bar {}: t_open is not on the {interval} grid",
            bar.t_open_ms
        ));
    }
    Ok(())
}

/// `<what> row <i> (<time>): <e>`.
fn row_error(what: &str, i: usize, t_ms: Option<i64>, e: impl Display) -> String {
    match t_ms {
        Some(t) => format!("{what} row {i} ({}): {e}", fmt_time(t)),
        None => format!("{what} row {i}: {e}"),
    }
}

/// `obj[key]` as a decimal (string or number); missing / null / anything
/// else is an error.
fn decimal(obj: &Value, key: &str) -> Result<f64, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Err(format!("`{key}` is missing")),
        Some(v) => decimal_value(v).ok_or_else(|| format!("`{key}` is {v}, not a decimal")),
    }
}

/// `obj[key]` as a decimal when present; null / missing ⇒ `None`.
fn opt_decimal(obj: &Value, key: &str) -> Result<Option<f64>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => decimal(obj, key).map(Some),
    }
}

/// A JSON integer (`1790366400000`, or `1790366400000.0`).
fn integer(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|x| x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15)
            .map(|x| x as i64)
    })
}

/// HL `candleSnapshot` reply for `coin` at `interval` → bars, ascending. A
/// row naming another coin or interval, or failing [`check_bar`], is an
/// error naming its row and open time.
pub(crate) fn hl_candles(
    reply: &Value,
    coin: &str,
    interval: Interval,
) -> Result<Vec<Bar>, String> {
    let what = format!("candleSnapshot {coin} {interval}");
    let rows = reply
        .as_array()
        .ok_or_else(|| format!("{what}: the reply is not an array"))?;
    let mut bars = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let t = integer(row.get("t"));
        let bar = (|| {
            if let Some(s) = row.get("s").filter(|s| !s.is_null()) {
                if s.as_str() != Some(coin) {
                    return Err(format!("coin is {s}, not {coin}"));
                }
            }
            if let Some(iv) = row.get("i").filter(|iv| !iv.is_null()) {
                if iv.as_str() != Some(interval.as_str()) {
                    return Err(format!("interval is {iv}, not {interval}"));
                }
            }
            let n = match row.get("n") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .ok_or_else(|| format!("`n` is {v}, not a trade count"))?,
                ),
            };
            let bar = Bar {
                t_open_ms: t.ok_or_else(|| "`t` is not an integer".to_string())?,
                o: decimal(row, "o")?,
                h: decimal(row, "h")?,
                l: decimal(row, "l")?,
                c: decimal(row, "c")?,
                v: decimal(row, "v")?,
                n,
            };
            check_bar(&bar, interval)?;
            Ok(bar)
        })()
        .map_err(|e| row_error(&what, i, t, e))?;
        bars.push(bar);
    }
    bars.sort_by_key(|b| b.t_open_ms);
    Ok(bars)
}

/// HL `fundingHistory` reply for `coin` → settlements, ascending. A row
/// naming another coin, without a time or with a rate that is no decimal is
/// an error naming its row; a missing `premium` is `None`.
pub(crate) fn hl_funding(reply: &Value, coin: &str) -> Result<Vec<FundingPoint>, String> {
    let what = format!("fundingHistory {coin}");
    let rows = reply
        .as_array()
        .ok_or_else(|| format!("{what}: the reply is not an array"))?;
    let mut points = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let t = integer(row.get("time"));
        let point = (|| {
            if let Some(c) = row.get("coin").filter(|c| !c.is_null()) {
                if c.as_str() != Some(coin) {
                    return Err(format!("coin is {c}, not {coin}"));
                }
            }
            Ok(FundingPoint {
                t_ms: t.ok_or_else(|| "`time` is not an integer".to_string())?,
                rate_1h: decimal(row, "fundingRate")?,
                premium: opt_decimal(row, "premium")?,
            })
        })()
        .map_err(|e: String| row_error(&what, i, t, e))?;
        points.push(point);
    }
    points.sort_by_key(|p| p.t_ms);
    Ok(points)
}

/// GeckoTerminal `…/ohlcv/<timeframe>` reply → bars at `interval`,
/// ascending (the reply is newest first). Rows are `[t_open_s, o, h, l, c,
/// v]`; `v` is USD volume with `currency=usd`.
pub(crate) fn gecko_ohlcv(reply: &Value, interval: Interval) -> Result<Vec<Bar>, String> {
    let what = format!("GeckoTerminal OHLCV {interval}");
    let rows = reply
        .pointer("/data/attributes/ohlcv_list")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{what}: the reply has no data.attributes.ohlcv_list array"))?;
    let mut bars = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let cells = row.as_array();
        let t = cells
            .and_then(|c| integer(c.first()))
            .and_then(|s| s.checked_mul(1000));
        let bar = (|| {
            let cells = cells
                .filter(|c| c.len() >= 6)
                .ok_or_else(|| format!("{row} is not [t, o, h, l, c, v]"))?;
            let px = |k: usize, name: &str| {
                decimal_value(&cells[k])
                    .ok_or_else(|| format!("{name} is {}, not a decimal", cells[k]))
            };
            let bar = Bar {
                t_open_ms: t.ok_or_else(|| "the time is not integer seconds".to_string())?,
                o: px(1, "open")?,
                h: px(2, "high")?,
                l: px(3, "low")?,
                c: px(4, "close")?,
                v: px(5, "volume")?,
                n: None,
            };
            check_bar(&bar, interval)?;
            Ok(bar)
        })()
        .map_err(|e: String| row_error(&what, i, t, e))?;
        bars.push(bar);
    }
    bars.sort_by_key(|b| b.t_open_ms);
    Ok(bars)
}

/// The `asset_ctxs` columns a row needs (`prev_day_px` is not stored).
const CTX_COLUMNS: [&str; 11] = [
    "time",
    "coin",
    "funding",
    "open_interest",
    "day_ntl_vlm",
    "premium",
    "oracle_px",
    "mark_px",
    "mid_px",
    "impact_bid_px",
    "impact_ask_px",
];

/// An archive time: RFC 3339 (`2026-09-30T00:01:00Z`), else a naive
/// `YYYY-MM-DD[T ]HH:MM:SS[.f]` read as UTC (the archive is UTC).
fn archive_time(s: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp_millis());
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok())
        .map(|t| t.and_utc().timestamp_millis())
}

/// HL archive `asset_ctxs` CSV (one row per coin per sample) → `(coin,
/// CtxPoint)` rows in file order. Columns are found by header name (extra
/// columns ignored); an empty cell is `None`; a missing column, a short /
/// long line, a bad time or a cell that is no decimal is an error naming
/// the line (1 = the header) and, once read, its time and coin.
pub(crate) fn hl_asset_ctxs_csv(text: &str) -> Result<Vec<(String, CtxPoint)>, String> {
    let mut lines = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim_end_matches('\r')))
        .filter(|(_, l)| !l.trim().is_empty());
    let (_, header) = lines
        .next()
        .ok_or_else(|| "asset_ctxs: the file is empty".to_string())?;
    let names: Vec<&str> = header.split(',').map(str::trim).collect();
    let mut at = [0usize; CTX_COLUMNS.len()];
    for (slot, col) in at.iter_mut().zip(CTX_COLUMNS) {
        *slot = names
            .iter()
            .position(|n| *n == col)
            .ok_or_else(|| format!("asset_ctxs header has no `{col}` column: {header}"))?;
    }
    let [time, coin, funding, oi, vlm, premium, oracle, mark, mid, bid, ask] = at;
    let mut out = Vec::new();
    for (line_no, line) in lines {
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        if cells.len() != names.len() {
            return Err(format!(
                "asset_ctxs line {line_no}: {} fields, the header has {}",
                cells.len(),
                names.len()
            ));
        }
        let t = archive_time(cells[time]).ok_or_else(|| {
            format!(
                "asset_ctxs line {line_no}: time `{}` is not RFC 3339",
                cells[time]
            )
        })?;
        let name = cells[coin];
        if name.is_empty() {
            return Err(format!(
                "asset_ctxs line {line_no} ({}): the coin is empty",
                fmt_time(t)
            ));
        }
        let cell = |k: usize| -> Result<Option<f64>, String> {
            let c = cells[k];
            if c.is_empty() {
                return Ok(None);
            }
            parse_decimal(c).map(Some).ok_or_else(|| {
                format!(
                    "asset_ctxs line {line_no} ({}, {name}): {} `{c}` is not a decimal",
                    fmt_time(t),
                    names[k]
                )
            })
        };
        out.push((
            name.to_string(),
            CtxPoint {
                t_ms: t,
                mark: cell(mark)?,
                oracle: cell(oracle)?,
                mid: cell(mid)?,
                impact_bid: cell(bid)?,
                impact_ask: cell(ask)?,
                oi: cell(oi)?,
                day_ntl_vlm: cell(vlm)?,
                funding_1h: cell(funding)?,
                premium: cell(premium)?,
            },
        ));
    }
    Ok(out)
}

/// One dataset of a JSON import file (the generic path for any history and
/// for offline fixtures). `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dataset {
    /// Full instrument id (`hyperliquid:xyz:TSLA`, `solana:<mint>`).
    pub instrument: String,
    /// Required when `bars` is not empty.
    #[serde(default)]
    pub interval: Option<Interval>,
    /// The rows' `source`; default `json:<file name>`.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub bars: Vec<Bar>,
    #[serde(default)]
    pub funding: Vec<FundingPoint>,
}

/// A JSON dataset file (an array of [`Dataset`]) → its datasets, every row
/// checked before anything is written: the instrument id parses, bars have
/// an interval and pass [`check_bar`], funding is finite.
pub(crate) fn json_datasets(text: &str) -> Result<Vec<Dataset>, String> {
    let sets: Vec<Dataset> =
        serde_json::from_str(text).map_err(|e| format!("JSON dataset file: {e}"))?;
    for (k, d) in sets.iter().enumerate() {
        let what = format!("dataset {k} ({})", d.instrument);
        InstrumentId::parse(&d.instrument).map_err(|e| format!("{what}: {e}"))?;
        if let Some(s) = &d.source {
            if s.trim().is_empty() || s.chars().any(char::is_control) {
                return Err(format!("{what}: source must be non-empty text"));
            }
        }
        if !d.bars.is_empty() {
            let interval = d
                .interval
                .ok_or_else(|| format!("{what}: bars need an interval"))?;
            for (i, b) in d.bars.iter().enumerate() {
                check_bar(b, interval)
                    .map_err(|e| row_error(&format!("{what} bars"), i, Some(b.t_open_ms), e))?;
            }
        }
        for (i, p) in d.funding.iter().enumerate() {
            if !p.rate_1h.is_finite() || p.premium.is_some_and(|x| !x.is_finite()) {
                return Err(row_error(
                    &format!("{what} funding"),
                    i,
                    Some(p.t_ms),
                    "rate_1h / premium must be finite",
                ));
            }
        }
    }
    Ok(sets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const H: i64 = 3_600_000;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/hyperliquid/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn hl_candles_decode_the_captured_reply() {
        let reply = fixture("candleSnapshot_xyz_TSLA_1h.json");
        let bars = hl_candles(&reply, "xyz:TSLA", Interval::H1).unwrap();
        assert_eq!(bars.len(), 67);
        assert_eq!(
            bars[0],
            Bar {
                t_open_ms: 1_790_366_400_000,
                o: 372.3,
                h: 372.48,
                l: 372.0,
                c: 372.33,
                v: 1120.694,
                n: Some(363),
            }
        );
        assert!(bars
            .windows(2)
            .all(|w| w[1].t_open_ms - w[0].t_open_ms == H));
        // A bar is kept once closed: at its close, not a ms before.
        let last = bars.last().unwrap();
        let closed = closed_bars(bars.clone(), Interval::H1, last.t_open_ms + H - 1);
        assert_eq!(closed.len(), 66, "the open bar is dropped");
        assert_eq!(
            closed_bars(bars.clone(), Interval::H1, last.t_open_ms + H).len(),
            67
        );
        // Another coin or interval is refused, never relabelled.
        let e = hl_candles(&reply, "xyz:NVDA", Interval::H1).unwrap_err();
        assert!(
            e.starts_with("candleSnapshot xyz:NVDA 1h row 0 (2026-09-25T20:00:00Z): coin is"),
            "{e}"
        );
        let e = hl_candles(&reply, "xyz:TSLA", Interval::M5).unwrap_err();
        assert!(e.contains("interval is \"1h\", not 5m"), "{e}");
    }

    #[test]
    fn a_bad_candle_is_an_error_naming_its_row_and_time() {
        let row = |o: Value, t: i64| json!({"t": t, "s": "SOL", "i": "1h", "o": o, "h": "2", "l": "1", "c": "1.5", "v": "10", "n": 3});
        let ok = json!([row(json!("1.2"), 0), row(json!(1.3), H)]);
        let bars = hl_candles(&ok, "SOL", Interval::H1).unwrap();
        assert_eq!(bars[1].o, 1.3, "a JSON number is a decimal too");
        for (bad, needle) in [
            (row(json!("x"), H), "`o` is \"x\", not a decimal"),
            (row(json!("0"), H), "prices must be finite and > 0"),
            (row(json!("1.2"), H + 60_000), "not on the 1h grid"),
            (
                json!({"s": "SOL", "o": "1"}),
                "row 1: `t` is not an integer",
            ),
        ] {
            let e =
                hl_candles(&json!([row(json!("1.2"), 0), bad]), "SOL", Interval::H1).unwrap_err();
            assert!(e.contains(needle), "{e}");
            assert!(e.starts_with("candleSnapshot SOL 1h row 1"), "{e}");
        }
        let mut no_v = row(json!("1.2"), 0);
        no_v.as_object_mut().unwrap().remove("v");
        let e = hl_candles(&json!([no_v]), "SOL", Interval::H1).unwrap_err();
        assert!(
            e.contains("row 0 (1970-01-01T00:00:00Z): `v` is missing"),
            "{e}"
        );
        assert!(hl_candles(&json!({"err": 1}), "SOL", Interval::H1).is_err());
    }

    #[test]
    fn hl_funding_decodes_and_names_bad_rows() {
        let reply = json!([
            {"coin": "xyz:TSLA", "fundingRate": "0.0000125", "premium": "-0.0003", "time": 2 * H + 31},
            {"coin": "xyz:TSLA", "fundingRate": "-0.00002", "time": H + 17},
        ]);
        let pts = hl_funding(&reply, "xyz:TSLA").unwrap();
        assert_eq!(
            pts,
            vec![
                FundingPoint {
                    t_ms: H + 17,
                    rate_1h: -0.00002,
                    premium: None
                },
                FundingPoint {
                    t_ms: 2 * H + 31,
                    rate_1h: 0.0000125,
                    premium: Some(-0.0003)
                },
            ],
            "ascending, times verbatim"
        );
        let e = hl_funding(&reply, "xyz:NVDA").unwrap_err();
        assert!(
            e.starts_with("fundingHistory xyz:NVDA row 0 (1970-01-01T02:00:00Z)"),
            "{e}"
        );
        let bad = json!([{"coin": "SOL", "fundingRate": "abc", "time": H}]);
        let e = hl_funding(&bad, "SOL").unwrap_err();
        assert!(e.contains("`fundingRate` is \"abc\""), "{e}");
        let bad = json!([{"coin": "SOL", "fundingRate": "0.1", "premium": [], "time": H}]);
        assert!(hl_funding(&bad, "SOL").unwrap_err().contains("`premium`"));
        assert!(hl_funding(&json!([]), "SOL").unwrap().is_empty());
    }

    #[test]
    fn gecko_ohlcv_is_newest_first_in_seconds() {
        let t0 = 1_790_366_400; // seconds, on the hour
        let reply = json!({"data": {"id": "x", "type": "ohlcv_request_response", "attributes": {
            "ohlcv_list": [
                [t0 + 3600, 151.2, 152.0, 150.9, 151.5, 125000.5],
                [t0, 150.0, 151.3, 149.8, 151.2, 98000.0],
            ]}}, "meta": {}});
        let bars = gecko_ohlcv(&reply, Interval::H1).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].t_open_ms, t0 * 1000, "ascending, ms");
        assert_eq!((bars[1].c, bars[1].v, bars[1].n), (151.5, 125000.5, None));
        let bad = json!({"data": {"attributes": {"ohlcv_list": [[t0, 150.0, 149.0, 149.8, 151.2, 1.0]]}}});
        let e = gecko_ohlcv(&bad, Interval::H1).unwrap_err();
        assert!(
            e.contains("row 0 (2026-09-25T20:00:00Z)") && e.contains("low / high"),
            "{e}"
        );
        let short = json!({"data": {"attributes": {"ohlcv_list": [[t0, 1.0]]}}});
        assert!(gecko_ohlcv(&short, Interval::H1)
            .unwrap_err()
            .contains("not [t, o, h, l, c, v]"));
        let e = gecko_ohlcv(&json!({"errors": [{"status": "404"}]}), Interval::H1).unwrap_err();
        assert!(e.contains("no data.attributes.ohlcv_list"), "{e}");
        let empty = json!({"data": {"attributes": {"ohlcv_list": []}}});
        assert!(gecko_ohlcv(&empty, Interval::D1).unwrap().is_empty());
    }

    const CSV: &str = "time,coin,funding,open_interest,prev_day_px,day_ntl_vlm,premium,oracle_px,mark_px,mid_px,impact_bid_px,impact_ask_px\r\n\
2026-09-30T00:00:00Z,BTC,0.0000125,23456.7,64000.0,1234567890.5,-0.0001,64010.0,64012.0,64011.5,64005.0,64018.0\r\n\
2026-09-30T00:00:00Z,kPEPE,-0.00001,1000000,0.0101,5000.0,0.0002,0.0102,0.0102,,,\r\n\
\r\n";

    #[test]
    fn archive_csv_rows_by_header_name() {
        let rows = hl_asset_ctxs_csv(CSV).unwrap();
        assert_eq!(rows.len(), 2);
        let (coin, btc) = &rows[0];
        assert_eq!(coin, "BTC");
        assert_eq!(
            btc.t_ms,
            crate::domain::marketdata::parse_time("2026-09-30").unwrap()
        );
        assert_eq!(
            (
                btc.mark,
                btc.oracle,
                btc.mid,
                btc.oi,
                btc.funding_1h,
                btc.premium
            ),
            (
                Some(64012.0),
                Some(64010.0),
                Some(64011.5),
                Some(23456.7),
                Some(0.0000125),
                Some(-0.0001)
            )
        );
        assert_eq!(
            (btc.impact_bid, btc.impact_ask, btc.day_ntl_vlm),
            (Some(64005.0), Some(64018.0), Some(1234567890.5))
        );
        let (coin, pepe) = &rows[1];
        assert_eq!(coin, "kPEPE", "coins verbatim");
        assert_eq!(
            (pepe.mid, pepe.impact_bid, pepe.impact_ask),
            (None, None, None),
            "empty = None"
        );

        // Columns in another order, an extra one, a naive UTC time.
        let shuffled = "coin,time,extra,mark_px,oracle_px,mid_px,impact_bid_px,impact_ask_px,funding,open_interest,day_ntl_vlm,premium\nSOL,2026-09-30 00:01:00,z,150,150,150,149.9,150.1,0,1,2,0\n";
        let rows = hl_asset_ctxs_csv(shuffled).unwrap();
        assert_eq!(
            rows[0].1.t_ms,
            crate::domain::marketdata::parse_time("2026-09-30T00:01:00Z").unwrap()
        );
        assert_eq!(rows[0].1.impact_ask, Some(150.1));
    }

    #[test]
    fn a_bad_archive_line_is_named() {
        let header = CSV.lines().next().unwrap();
        for (line, needle) in [
            (
                "2026-09-30T00:00:00Z,BTC,x,1,1,1,1,1,1,1,1,1",
                "line 2 (2026-09-30T00:00:00Z, BTC): funding `x` is not a decimal",
            ),
            (
                "2026-09-30T00:00:00Z,BTC,1,1",
                "line 2: 4 fields, the header has 12",
            ),
            (
                "yesterday,BTC,1,1,1,1,1,1,1,1,1,1",
                "line 2: time `yesterday` is not RFC 3339",
            ),
            (
                "2026-09-30T00:00:00Z,,1,1,1,1,1,1,1,1,1,1",
                "the coin is empty",
            ),
        ] {
            let e = hl_asset_ctxs_csv(&format!("{header}\n{line}\n")).unwrap_err();
            assert!(e.contains(needle), "{e}");
        }
        let e = hl_asset_ctxs_csv("time,coin,funding\n").unwrap_err();
        assert!(e.contains("no `open_interest` column"), "{e}");
        assert!(hl_asset_ctxs_csv("").unwrap_err().contains("empty"));
        assert!(hl_asset_ctxs_csv(header).unwrap().is_empty(), "header only");
    }

    #[test]
    fn json_datasets_are_checked_before_any_write() {
        let text = r#"[
          {"instrument": "solana:So11111111111111111111111111111111111111112", "interval": "1h",
           "source": "gecko:solana:fixture",
           "bars": [{"t_open_ms": 0, "o": 1, "h": 2, "l": 0.5, "c": 1.5, "v": 10},
                    {"t_open_ms": 3600000, "o": 1.5, "h": 2, "l": 1, "c": 1.8, "v": 7, "n": 4}]},
          {"instrument": "hyperliquid:xyz:TSLA",
           "funding": [{"t_ms": 3600000, "rate_1h": 0.0000125}, {"t_ms": 7200000, "rate_1h": -0.00001, "premium": 0.0002}]}
        ]"#;
        let sets = json_datasets(text).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].bars[1].n, Some(4));
        assert_eq!(sets[0].source.as_deref(), Some("gecko:solana:fixture"));
        assert_eq!(
            (sets[1].interval, sets[1].bars.len(), sets[1].funding.len()),
            (None, 0, 2)
        );

        for (bad, needle) in [
            (
                r#"[{"instrument": "nowhere:X"}]"#,
                "unknown venue `nowhere`",
            ),
            (
                r#"[{"instrument": "hyperliquid:SOL", "bars": [{"t_open_ms": 0, "o": 1, "h": 1, "l": 1, "c": 1, "v": 1}]}]"#,
                "dataset 0 (hyperliquid:SOL): bars need an interval",
            ),
            (
                r#"[{"instrument": "hyperliquid:SOL", "interval": "1h", "bars": [{"t_open_ms": 60000, "o": 1, "h": 1, "l": 1, "c": 1, "v": 1}]}]"#,
                "dataset 0 (hyperliquid:SOL) bars row 0 (1970-01-01T00:01:00Z)",
            ),
            (
                r#"[{"instrument": "hyperliquid:SOL", "candles": []}]"#,
                "unknown field `candles`",
            ),
            (
                r#"[{"instrument": "hyperliquid:SOL", "interval": "2h"}]"#,
                "not one of",
            ),
            (
                r#"[{"instrument": "hyperliquid:SOL", "source": " "}]"#,
                "source must be non-empty",
            ),
            (r#"{"instrument": "hyperliquid:SOL"}"#, "JSON dataset file"),
        ] {
            let e = json_datasets(bad).unwrap_err();
            assert!(e.contains(needle), "{bad}: {e}");
        }
    }

    #[test]
    fn hl_coins_of_ids() {
        assert_eq!(hl_coin("hyperliquid:xyz:TSLA").unwrap(), "xyz:TSLA");
        assert_eq!(hl_coin("hyperliquid:SOL").unwrap(), "SOL");
        assert_eq!(hl_coin("hyperliquid:@151").unwrap(), "@151");
        let e = hl_coin("solana:So11111111111111111111111111111111111111112").unwrap_err();
        assert!(
            e.contains("solana:So11111111111111111111111111111111111111112"),
            "{e}"
        );
        assert!(hl_coin("TSLA").is_err());
    }
}
