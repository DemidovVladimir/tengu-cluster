//! `SqliteMarketData` — `ports::market_data::MarketDataStore` over ONE
//! database per xmarket state dir, `<state dir>/market.db` (xlab,
//! `docs/xlab-2026-10-01.md` § 4): outside every workspace and fs root, like
//! `ledger.db`. WAL (`synchronous = NORMAL`) + busy_timeout, so a backfill and
//! a backtest of the install share it; every call runs on `spawn_blocking`.
//! No `[xmarket]` ⇒ [`open_market_data`] refuses. Fed by
//! `outbound/backfill/`.
//!
//! | Table (`WITHOUT ROWID`) | Key | Columns |
//! |---|---|---|
//! | `bars` | (instrument, interval, t_open_ms) | o, h, l, c, v, n (trades, nullable), source, fetched_at_ms |
//! | `funding` | (instrument, t_ms) | rate_1h (HL per-hour rate; > 0 = longs pay), premium, source, fetched_at_ms |
//! | `ctx` | (instrument, t_ms) | mark, oracle, mid, impact_bid, impact_ask, oi, day_ntl_vlm, funding_1h, premium, source |
//! | `events` (Phase 7) | (instrument, source, id) | published_ms (indexed with the instrument), kind, form, title, fetched_at_ms — an SEC filing: its accession number and acceptance time |
//! | `event_coverage` | (instrument, source) | from_ms, to_ms, covered, note, fetched_at_ms — the span the latest fetch read; `covered = 0`: the source has nothing for the instrument |
//!
//! | Call | Rule |
//! |---|---|
//! | `put_*` | `INSERT OR REPLACE` in one transaction: a re-fetch replaces the row (same key); returns the rows written. Bars are `Bar::validate`d again and funding must be finite — one bad row fails the call, nothing written |
//! | `bars` / `funding` / `ctx` | half-open `[from, to)`, ascending, as the domain series |
//! | `coverage` | `GROUP BY` instrument (+ interval) and source per table, folded per (instrument, kind, interval): first / last time, rows, sources |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};

use crate::config::sections::SandboxSections;
use crate::config::xmarket::{market_db, MARKET_DB};
use crate::domain::marketdata::{
    Bar, BarSeries, CtxPoint, CtxSeries, EventCoverage, FundingPoint, FundingSeries, Interval,
    MarketEvent,
};
use crate::domain::observation::now_ms;
use crate::ports::market_data::{CoverageRow, MarketDataStore};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS bars (
  instrument TEXT NOT NULL, interval TEXT NOT NULL, t_open_ms INTEGER NOT NULL,
  o REAL NOT NULL, h REAL NOT NULL, l REAL NOT NULL, c REAL NOT NULL, v REAL NOT NULL,
  n INTEGER, source TEXT NOT NULL, fetched_at_ms INTEGER NOT NULL,
  PRIMARY KEY (instrument, interval, t_open_ms)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS funding (
  instrument TEXT NOT NULL, t_ms INTEGER NOT NULL, rate_1h REAL NOT NULL, premium REAL,
  source TEXT NOT NULL, fetched_at_ms INTEGER NOT NULL,
  PRIMARY KEY (instrument, t_ms)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ctx (
  instrument TEXT NOT NULL, t_ms INTEGER NOT NULL, mark REAL, oracle REAL, mid REAL,
  impact_bid REAL, impact_ask REAL, oi REAL, day_ntl_vlm REAL, funding_1h REAL, premium REAL,
  source TEXT NOT NULL, PRIMARY KEY (instrument, t_ms)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS events (
  instrument TEXT NOT NULL, source TEXT NOT NULL, id TEXT NOT NULL, published_ms INTEGER NOT NULL,
  kind TEXT NOT NULL, form TEXT NOT NULL, title TEXT, fetched_at_ms INTEGER NOT NULL,
  PRIMARY KEY (instrument, source, id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS events_by_time ON events(instrument, published_ms);
CREATE TABLE IF NOT EXISTS event_coverage (
  instrument TEXT NOT NULL, source TEXT NOT NULL, from_ms INTEGER NOT NULL, to_ms INTEGER NOT NULL,
  covered INTEGER NOT NULL, note TEXT, fetched_at_ms INTEGER NOT NULL,
  PRIMARY KEY (instrument, source)) WITHOUT ROWID;";

const PUT_EVENT_SQL: &str = "
INSERT OR REPLACE INTO events(instrument, source, id, published_ms, kind, form, title,
  fetched_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

const PUT_BAR_SQL: &str = "
INSERT OR REPLACE INTO bars(instrument, interval, t_open_ms, o, h, l, c, v, n, source,
  fetched_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const PUT_FUNDING_SQL: &str = "
INSERT OR REPLACE INTO funding(instrument, t_ms, rate_1h, premium, source, fetched_at_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

const PUT_CTX_SQL: &str = "
INSERT OR REPLACE INTO ctx(instrument, t_ms, mark, oracle, mid, impact_bid, impact_ask, oi,
  day_ntl_vlm, funding_1h, premium, source)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

/// Per table: (kind, `SELECT instrument, interval, source, min, max, count
/// FROM <table>`, `GROUP BY` columns). The instrument filter goes between
/// (an indexed `WHERE instrument = ?1`, [`coverage_sql`]).
const COVERAGE_SQL: [(&str, &str, &str); 3] = [
    (
        "bars",
        "SELECT instrument, interval, source, MIN(t_open_ms), MAX(t_open_ms), COUNT(*) FROM bars",
        "instrument, interval, source",
    ),
    (
        "funding",
        "SELECT instrument, NULL, source, MIN(t_ms), MAX(t_ms), COUNT(*) FROM funding",
        "instrument, source",
    ),
    (
        "ctx",
        "SELECT instrument, NULL, source, MIN(t_ms), MAX(t_ms), COUNT(*) FROM ctx",
        "instrument, source",
    ),
];

fn coverage_sql(select: &str, group: &str, one_instrument: bool) -> String {
    let filter = if one_instrument {
        " WHERE instrument = ?1"
    } else {
        ""
    };
    format!("{select}{filter} GROUP BY {group}")
}

/// The sandbox's market-data warehouse (`[xmarket]` state dir); refused
/// without `[xmarket]`.
pub(crate) fn open_market_data(sections: &SandboxSections) -> Result<Arc<dyn MarketDataStore>> {
    let dir = market_state_dir(sections)?;
    Ok(Arc::new(SqliteMarketData::open(dir)?))
}

/// `[xmarket]`'s state dir, where `market.db` lives; refused without it.
pub(crate) fn market_state_dir(sections: &SandboxSections) -> Result<&Path> {
    sections.xm_state_dir.as_deref().ok_or_else(|| {
        anyhow!(
            "no [xmarket] section: market data lives in \
             <TENGU_HOME>/state/<xmarket.state>/{MARKET_DB} — add [xmarket] state = \"<name>\""
        )
    })
}

pub(crate) struct SqliteMarketData {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMarketData {
    /// Open (creating) `<state_dir>/market.db`.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let path: PathBuf = market_db(state_dir);
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        // NORMAL: WAL stays consistent; a power cut may lose the last writes,
        // which a re-run of the backfill fetches again.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA_SQL)
            .with_context(|| format!("schema of {}", path.display()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|e| anyhow!("market data lock: {e}"))?;
            f(&mut guard)
        })
        .await
        .context("market data task")?
    }
}

fn opt_i64(n: Option<u64>) -> Result<Option<i64>> {
    n.map(|n| i64::try_from(n).map_err(|_| anyhow!("trade count {n} is out of range")))
        .transpose()
}

/// (instrument, kind, interval, source, first, last, rows) of one group.
type Part = (
    String,
    &'static str,
    Option<Interval>,
    String,
    i64,
    i64,
    u64,
);

/// Fold per-source groups into one row per (instrument, kind, interval),
/// sorted by those.
fn fold_coverage(parts: Vec<Part>) -> Vec<CoverageRow> {
    let mut rows: BTreeMap<(String, &str, Option<Interval>), CoverageRow> = BTreeMap::new();
    for (instrument, kind, interval, source, first, last, n) in parts {
        let row = rows
            .entry((instrument.clone(), kind, interval))
            .or_insert_with(|| CoverageRow {
                instrument,
                kind: kind.to_string(),
                interval,
                first_ms: first,
                last_ms: last,
                rows: 0,
                sources: Vec::new(),
            });
        row.first_ms = row.first_ms.min(first);
        row.last_ms = row.last_ms.max(last);
        row.rows += n;
        row.sources.push(source);
    }
    rows.into_values()
        .map(|mut r| {
            r.sources.sort();
            r.sources.dedup();
            r
        })
        .collect()
}

#[async_trait]
impl MarketDataStore for SqliteMarketData {
    async fn put_bars(
        &self,
        instrument: &str,
        interval: Interval,
        source: &str,
        bars: &[Bar],
    ) -> Result<usize> {
        if bars.is_empty() {
            return Ok(0);
        }
        for b in bars {
            b.validate()
                .map_err(|e| anyhow!("{instrument} {interval}: {e}; nothing written"))?;
        }
        let (instrument, source, bars) =
            (instrument.to_string(), source.to_string(), bars.to_vec());
        self.with_conn(move |conn| {
            let at = now_ms();
            let tx = conn.transaction()?;
            let mut written = 0;
            {
                let mut stmt = tx.prepare(PUT_BAR_SQL)?;
                for b in &bars {
                    written += stmt.execute(params![
                        instrument,
                        interval.as_str(),
                        b.t_open_ms,
                        b.o,
                        b.h,
                        b.l,
                        b.c,
                        b.v,
                        opt_i64(b.n)?,
                        source,
                        at
                    ])?;
                }
            }
            tx.commit()?;
            Ok(written)
        })
        .await
    }

    async fn bars(
        &self,
        instrument: &str,
        interval: Interval,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<BarSeries> {
        let id = instrument.to_string();
        let bars = self
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT t_open_ms, o, h, l, c, v, n FROM bars WHERE instrument = ?1 \
                     AND interval = ?2 AND t_open_ms >= ?3 AND t_open_ms < ?4 ORDER BY t_open_ms",
                )?;
                let rows = stmt.query_map(params![id, interval.as_str(), from_ms, to_ms], |r| {
                    Ok(Bar {
                        t_open_ms: r.get(0)?,
                        o: r.get(1)?,
                        h: r.get(2)?,
                        l: r.get(3)?,
                        c: r.get(4)?,
                        v: r.get(5)?,
                        n: r.get::<_, Option<i64>>(6)?.map(|n| n.max(0) as u64),
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<Bar>>>()?)
            })
            .await?;
        Ok(BarSeries::new(instrument, interval, bars))
    }

    async fn put_funding(
        &self,
        instrument: &str,
        source: &str,
        points: &[FundingPoint],
    ) -> Result<usize> {
        if points.is_empty() {
            return Ok(0);
        }
        if let Some(p) = points
            .iter()
            .find(|p| !p.rate_1h.is_finite() || p.premium.is_some_and(|x| !x.is_finite()))
        {
            bail!(
                "{instrument} funding at {}: rate_1h / premium must be finite; nothing written",
                p.t_ms
            );
        }
        let (instrument, source, points) =
            (instrument.to_string(), source.to_string(), points.to_vec());
        self.with_conn(move |conn| {
            let at = now_ms();
            let tx = conn.transaction()?;
            let mut written = 0;
            {
                let mut stmt = tx.prepare(PUT_FUNDING_SQL)?;
                for p in &points {
                    written += stmt.execute(params![
                        instrument, p.t_ms, p.rate_1h, p.premium, source, at
                    ])?;
                }
            }
            tx.commit()?;
            Ok(written)
        })
        .await
    }

    async fn funding(&self, instrument: &str, from_ms: i64, to_ms: i64) -> Result<FundingSeries> {
        let id = instrument.to_string();
        let points = self
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT t_ms, rate_1h, premium FROM funding WHERE instrument = ?1 \
                     AND t_ms >= ?2 AND t_ms < ?3 ORDER BY t_ms",
                )?;
                let rows = stmt.query_map(params![id, from_ms, to_ms], |r| {
                    Ok(FundingPoint {
                        t_ms: r.get(0)?,
                        rate_1h: r.get(1)?,
                        premium: r.get(2)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<FundingPoint>>>()?)
            })
            .await?;
        Ok(FundingSeries::new(instrument, points))
    }

    async fn put_ctx(&self, instrument: &str, source: &str, points: &[CtxPoint]) -> Result<usize> {
        if points.is_empty() {
            return Ok(0);
        }
        let (instrument, source, points) =
            (instrument.to_string(), source.to_string(), points.to_vec());
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            let mut written = 0;
            {
                let mut stmt = tx.prepare(PUT_CTX_SQL)?;
                for p in &points {
                    written += stmt.execute(params![
                        instrument,
                        p.t_ms,
                        p.mark,
                        p.oracle,
                        p.mid,
                        p.impact_bid,
                        p.impact_ask,
                        p.oi,
                        p.day_ntl_vlm,
                        p.funding_1h,
                        p.premium,
                        source
                    ])?;
                }
            }
            tx.commit()?;
            Ok(written)
        })
        .await
    }

    async fn ctx(&self, instrument: &str, from_ms: i64, to_ms: i64) -> Result<CtxSeries> {
        let id = instrument.to_string();
        let points = self
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT t_ms, mark, oracle, mid, impact_bid, impact_ask, oi, day_ntl_vlm, \
                     funding_1h, premium FROM ctx WHERE instrument = ?1 AND t_ms >= ?2 \
                     AND t_ms < ?3 ORDER BY t_ms",
                )?;
                let rows = stmt.query_map(params![id, from_ms, to_ms], |r| {
                    Ok(CtxPoint {
                        t_ms: r.get(0)?,
                        mark: r.get(1)?,
                        oracle: r.get(2)?,
                        mid: r.get(3)?,
                        impact_bid: r.get(4)?,
                        impact_ask: r.get(5)?,
                        oi: r.get(6)?,
                        day_ntl_vlm: r.get(7)?,
                        funding_1h: r.get(8)?,
                        premium: r.get(9)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<CtxPoint>>>()?)
            })
            .await?;
        Ok(CtxSeries::new(instrument, points))
    }

    async fn coverage(&self, instrument: Option<&str>) -> Result<Vec<CoverageRow>> {
        let id = instrument.map(str::to_string);
        self.with_conn(move |conn| {
            let mut parts: Vec<Part> = Vec::new();
            for (kind, select, group) in COVERAGE_SQL {
                let mut stmt = conn.prepare(&coverage_sql(select, group, id.is_some()))?;
                let mut rows = match &id {
                    Some(id) => stmt.query(params![id])?,
                    None => stmt.query([])?,
                };
                while let Some(r) = rows.next()? {
                    let interval = r
                        .get::<_, Option<String>>(1)?
                        .map(|s| {
                            Interval::parse(&s).map_err(|e| anyhow!("{MARKET_DB} {kind}: {e}"))
                        })
                        .transpose()?;
                    parts.push((
                        r.get(0)?,
                        kind,
                        interval,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get::<_, i64>(5)?.max(0) as u64,
                    ));
                }
            }
            Ok(fold_coverage(parts))
        })
        .await
    }

    async fn put_events(&self, source: &str, events: &[MarketEvent]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        for e in events {
            e.validate()
                .map_err(|why| anyhow!("{source}: {why}; nothing written"))?;
        }
        let (source, events) = (source.to_string(), events.to_vec());
        self.with_conn(move |conn| {
            let at = now_ms();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(PUT_EVENT_SQL)?;
                for e in &events {
                    stmt.execute(params![
                        e.instrument,
                        source,
                        e.id,
                        e.published_ms,
                        e.kind,
                        e.form,
                        e.title,
                        at
                    ])?;
                }
            }
            tx.commit()?;
            Ok(events.len())
        })
        .await
    }

    async fn events(&self, instrument: &str, from_ms: i64, to_ms: i64) -> Result<Vec<MarketEvent>> {
        let id = instrument.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT instrument, published_ms, kind, id, form, title FROM events \
                 WHERE instrument = ?1 AND published_ms >= ?2 AND published_ms < ?3 \
                 ORDER BY published_ms, id",
            )?;
            let rows = stmt.query_map(params![id, from_ms, to_ms], |r| {
                Ok(MarketEvent {
                    instrument: r.get(0)?,
                    published_ms: r.get(1)?,
                    kind: r.get(2)?,
                    id: r.get(3)?,
                    form: r.get(4)?,
                    title: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn put_event_coverage(&self, c: &EventCoverage) -> Result<()> {
        if c.from_ms > c.to_ms {
            bail!(
                "{} {}: coverage from {} is after to {}",
                c.instrument,
                c.source,
                c.from_ms,
                c.to_ms
            );
        }
        let c = c.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO event_coverage(instrument, source, from_ms, to_ms, \
                 covered, note, fetched_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    c.instrument,
                    c.source,
                    c.from_ms,
                    c.to_ms,
                    c.covered,
                    c.note,
                    c.fetched_at_ms
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn event_coverage(&self, instrument: &str) -> Result<Vec<EventCoverage>> {
        let id = instrument.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT instrument, source, from_ms, to_ms, covered, note, fetched_at_ms \
                 FROM event_coverage WHERE instrument = ?1 ORDER BY source",
            )?;
            let rows = stmt.query_map(params![id], |r| {
                Ok(EventCoverage {
                    instrument: r.get(0)?,
                    source: r.get(1)?,
                    from_ms: r.get(2)?,
                    to_ms: r.get(3)?,
                    covered: r.get(4)?,
                    note: r.get(5)?,
                    fetched_at_ms: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const TSLA: &str = "hyperliquid:xyz:TSLA";

    fn filing(id: &str, t: i64, form: &str) -> MarketEvent {
        MarketEvent {
            instrument: TSLA.into(),
            published_ms: t,
            kind: "filing".into(),
            id: id.into(),
            form: form.into(),
            title: Some("Item 2.02 Results of Operations".into()),
        }
    }

    /// Phase 7: events round-trip by publication time; a re-fetch replaces
    /// by `(instrument, source, id)`; one bad event writes nothing;
    /// coverage keeps the latest span per source.
    #[tokio::test]
    async fn events_and_their_coverage_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteMarketData::open(dir.path()).unwrap();
        let a = filing("0001318605-26-000052", 10 * H, "8-K");
        let b = filing("0001318605-26-000041", 2 * H, "10-Q");
        assert_eq!(
            s.put_events("sec", &[a.clone(), b.clone()]).await.unwrap(),
            2
        );
        let mut a2 = a.clone();
        a2.form = "8-K/A".into();
        assert_eq!(s.put_events("sec", &[a2.clone()]).await.unwrap(), 1);
        assert_eq!(
            s.events(TSLA, 0, 24 * H).await.unwrap(),
            vec![b.clone(), a2]
        );
        assert_eq!(
            s.events(TSLA, 3 * H, 10 * H).await.unwrap(),
            Vec::<MarketEvent>::new()
        );
        let mut bad = filing("x", H, "8-K");
        bad.kind = "rumour".into();
        let e = s
            .put_events("sec", &[filing("y", H, "8-K"), bad])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("kind `rumour`"), "{e}");
        assert_eq!(s.events(TSLA, 0, 2 * H).await.unwrap().len(), 0);
        let c = |to: i64| EventCoverage {
            instrument: TSLA.into(),
            source: "sec".into(),
            from_ms: 0,
            to_ms: to,
            covered: true,
            note: Some("cik 0001318605 TSLA".into()),
            fetched_at_ms: to,
        };
        s.put_event_coverage(&c(5 * H)).await.unwrap();
        s.put_event_coverage(&c(9 * H)).await.unwrap();
        assert_eq!(s.event_coverage(TSLA).await.unwrap(), vec![c(9 * H)]);
        assert!(s
            .event_coverage("hyperliquid:xyz:SMSN")
            .await
            .unwrap()
            .is_empty());
    }
    const SOL_MINT: &str = "solana:So11111111111111111111111111111111111111112";

    fn bar(t: i64, c: f64) -> Bar {
        Bar {
            t_open_ms: t,
            o: c,
            h: c * 1.01,
            l: c * 0.99,
            c,
            v: 10.0,
            n: Some(3),
        }
    }

    fn fp(t: i64, rate: f64) -> FundingPoint {
        FundingPoint {
            t_ms: t,
            rate_1h: rate,
            premium: Some(0.0001),
        }
    }

    #[tokio::test]
    async fn bars_funding_and_ctx_round_trip_and_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        assert!(dir.path().join("market.db").exists());

        let bars = [bar(0, 100.0), bar(H, 101.0), bar(2 * H, 102.0)];
        assert_eq!(
            store
                .put_bars(TSLA, Interval::H1, "hl", &bars)
                .await
                .unwrap(),
            3
        );
        // A re-fetch replaces (same key): count returned, no duplicate.
        let mut again = bar(2 * H, 103.0);
        again.n = None;
        assert_eq!(
            store
                .put_bars(TSLA, Interval::H1, "hl", &[again])
                .await
                .unwrap(),
            1
        );
        let s = store.bars(TSLA, Interval::H1, 0, 3 * H).await.unwrap();
        assert_eq!(s.bars, vec![bars[0], bars[1], again]);
        assert_eq!((s.instrument.as_str(), s.interval), (TSLA, Interval::H1));
        // Half-open [from, to); another interval is another series.
        assert_eq!(
            store.bars(TSLA, Interval::H1, H, 2 * H).await.unwrap().bars,
            vec![bars[1]]
        );
        assert!(store
            .bars(TSLA, Interval::M5, 0, 3 * H)
            .await
            .unwrap()
            .bars
            .is_empty());

        let pts = [fp(H + 17, 0.0000125), fp(2 * H + 31, -0.00001)];
        assert_eq!(store.put_funding(TSLA, "hl", &pts).await.unwrap(), 2);
        let f = store.funding(TSLA, 0, 3 * H).await.unwrap();
        assert_eq!(f.points, pts.to_vec());

        let c = CtxPoint {
            t_ms: 60_000,
            mark: Some(64_012.0),
            mid: Some(64_011.5),
            impact_bid: Some(64_005.0),
            impact_ask: Some(64_018.0),
            oi: Some(23_456.7),
            ..Default::default()
        };
        assert_eq!(
            store
                .put_ctx("hyperliquid:BTC", "hl-archive:asset_ctxs", &[c])
                .await
                .unwrap(),
            1
        );
        let got = store.ctx("hyperliquid:BTC", 0, 120_000).await.unwrap();
        assert_eq!(got.points, vec![c], "None columns stay None");

        // A second handle (another process) sees the same rows.
        let other = SqliteMarketData::open(dir.path()).unwrap();
        assert_eq!(
            other
                .bars(TSLA, Interval::H1, 0, i64::MAX)
                .await
                .unwrap()
                .bars
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn a_bad_row_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let mut bad = bar(H, 1.0);
        bad.l = 0.0;
        let e = store
            .put_bars(TSLA, Interval::H1, "hl", &[bar(0, 1.0), bad])
            .await
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("hyperliquid:xyz:TSLA 1h: bar 3600000"),
            "{e}"
        );
        assert!(store
            .bars(TSLA, Interval::H1, 0, i64::MAX)
            .await
            .unwrap()
            .bars
            .is_empty());
        let e = store
            .put_funding(TSLA, "hl", &[fp(H, f64::NAN)])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("must be finite"), "{e}");
        assert_eq!(
            store.put_bars(TSLA, Interval::H1, "hl", &[]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn coverage_groups_per_instrument_kind_and_interval() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        assert!(store.coverage(None).await.unwrap().is_empty());
        store
            .put_bars(TSLA, Interval::H1, "hl", &[bar(H, 1.0), bar(2 * H, 1.0)])
            .await
            .unwrap();
        store
            .put_bars(TSLA, Interval::H1, "json:fixture.json", &[bar(5 * H, 1.0)])
            .await
            .unwrap();
        store
            .put_bars(TSLA, Interval::D1, "hl", &[bar(0, 1.0)])
            .await
            .unwrap();
        store
            .put_funding(TSLA, "hl", &[fp(H, 0.1), fp(3 * H, 0.2)])
            .await
            .unwrap();
        let pool = format!(
            "gecko:solana:{}",
            "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj"
        );
        store
            .put_bars(SOL_MINT, Interval::H1, &pool, &[bar(0, 150.0)])
            .await
            .unwrap();

        let all = store.coverage(None).await.unwrap();
        let keys: Vec<(&str, &str, Option<Interval>)> = all
            .iter()
            .map(|r| (r.instrument.as_str(), r.kind.as_str(), r.interval))
            .collect();
        assert_eq!(
            keys,
            vec![
                (TSLA, "bars", Some(Interval::H1)),
                (TSLA, "bars", Some(Interval::D1)),
                (TSLA, "funding", None),
                (SOL_MINT, "bars", Some(Interval::H1)),
            ]
        );
        let h1 = &all[0];
        assert_eq!((h1.first_ms, h1.last_ms, h1.rows), (H, 5 * H, 3));
        assert_eq!(h1.sources, vec!["hl", "json:fixture.json"]);
        assert_eq!(
            (all[2].first_ms, all[2].last_ms, all[2].rows),
            (H, 3 * H, 2)
        );
        assert_eq!(all[3].sources, vec![pool.clone()], "full pool address");

        let one = store.coverage(Some(SOL_MINT)).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].instrument, SOL_MINT);
        assert!(store
            .coverage(Some("hyperliquid:NOPE"))
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn no_xmarket_means_no_market_db() {
        let e = open_market_data(&SandboxSections::default()).err().unwrap();
        assert!(e.to_string().contains("no [xmarket] section"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(dir.path().join("xlab")),
            ..Default::default()
        };
        assert!(open_market_data(&sections).is_ok());
        assert!(market_db(&dir.path().join("xlab")).exists());
    }
}
