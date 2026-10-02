//! `SqliteHistoryStore` — `ports::history::HistoryStore` over one SQLite file
//! per UTC day, `<dir>/<YYYYMMDD>.db` (the day of `observed_at_ms`; `dir` =
//! `<TENGU_HOME>/state/<xmarket.state>/history`). Table `obs_history`, unique
//! `(key, observed_at_ms)` (a re-append is a no-op), WAL (`synchronous =
//! NORMAL`) + busy_timeout so every process of the install can append. Day
//! files older than `retention_days` are deleted on open and whenever a new
//! day file is created, and rows that old are not appended. A `reader`
//! creates no day file and deletes nothing (`tengu history`). Every call runs
//! on `spawn_blocking`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tracing::warn;

use crate::domain::observation::now_ms;
use crate::ports::history::{HistoryRow, HistoryStore};

const DAY_MS: i64 = 86_400_000;
/// Day files kept open for appends (today + one straggler).
const OPEN_DAYS: usize = 2;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS obs_history (
  key TEXT NOT NULL, schema TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
  venue_ts_ms INTEGER, slot INTEGER, source TEXT NOT NULL, status TEXT NOT NULL,
  errors TEXT, features TEXT NOT NULL, data TEXT);
CREATE UNIQUE INDEX IF NOT EXISTS obs_history_key_at ON obs_history(key, observed_at_ms);";

const INSERT_SQL: &str = "
INSERT OR IGNORE INTO obs_history(key, schema, observed_at_ms, venue_ts_ms, slot, source,
  status, errors, features, data)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

const RANGE_SQL: &str = "
SELECT key, schema, observed_at_ms, venue_ts_ms, slot, source, status, errors, features, data
FROM obs_history WHERE key = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3
ORDER BY observed_at_ms";

const ASOF_SQL: &str = "
SELECT key, schema, observed_at_ms, venue_ts_ms, slot, source, status, errors, features, data
FROM obs_history WHERE key = ?1 AND observed_at_ms <= ?2 AND observed_at_ms >= ?3
ORDER BY observed_at_ms DESC LIMIT 1";

pub(crate) struct SqliteHistoryStore {
    dir: PathBuf,
    /// `None` = never delete (a reader).
    retention_days: Option<u32>,
    /// Day files open for appends, by day number (days since the epoch).
    writers: Arc<Mutex<BTreeMap<i64, Connection>>>,
}

impl SqliteHistoryStore {
    /// Create `dir` and delete day files older than `retention_days`
    /// (`0` = keep all).
    pub(crate) fn open(dir: &Path, retention_days: u32) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let store = Self {
            dir: dir.to_path_buf(),
            retention_days: Some(retention_days),
            writers: Arc::default(),
        };
        store.sweep(now_ms())?;
        Ok(store)
    }

    /// Reads existing day files only: creates nothing, deletes nothing.
    pub(crate) fn reader(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            retention_days: None,
            writers: Arc::default(),
        }
    }

    /// Delete the day files (with `-wal` / `-shm`) older than
    /// `retention_days` before the UTC day of `now_ms`; returns how many.
    pub(crate) fn sweep(&self, now_ms: i64) -> Result<usize> {
        let mut writers = self
            .writers
            .lock()
            .map_err(|e| anyhow!("history store lock: {e}"))?;
        sweep_files(&self.dir, self.retention_days, day_of(now_ms), &mut writers)
    }

    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Path, Option<u32>, &mut BTreeMap<i64, Connection>) -> Result<T> + Send + 'static,
    {
        let (dir, retention, writers) = (
            self.dir.clone(),
            self.retention_days,
            Arc::clone(&self.writers),
        );
        tokio::task::spawn_blocking(move || {
            let mut writers = writers
                .lock()
                .map_err(|e| anyhow!("history store lock: {e}"))?;
            f(&dir, retention, &mut writers)
        })
        .await
        .context("history store task")?
    }
}

fn day_of(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// `YYYYMMDD.db` of a day number.
fn day_name(day: i64) -> Result<String> {
    DateTime::from_timestamp_millis(day.saturating_mul(DAY_MS))
        .map(|d| d.format("%Y%m%d.db").to_string())
        .ok_or_else(|| anyhow!("day {day} is out of range"))
}

/// Day number of a `YYYYMMDD.db` file name.
fn day_of_name(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".db").filter(|s| s.len() == 8)?;
    let date = NaiveDate::parse_from_str(stem, "%Y%m%d").ok()?;
    Some(day_of(
        date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis(),
    ))
}

/// Existing day files, oldest first.
fn day_files(dir: &Path) -> Result<Vec<(i64, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| Some((day_of_name(e.file_name().to_str()?)?, e.path())))
        .collect();
    out.sort();
    Ok(out)
}

fn open_day(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    // NORMAL: WAL stays consistent; a power cut may lose the last appends.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
    )?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

/// First day number kept, when a retention applies.
fn oldest_kept(retention_days: Option<u32>, today: i64) -> Option<i64> {
    retention_days
        .filter(|d| *d > 0)
        .map(|d| today - i64::from(d))
}

fn sweep_files(
    dir: &Path,
    retention_days: Option<u32>,
    today: i64,
    writers: &mut BTreeMap<i64, Connection>,
) -> Result<usize> {
    let Some(oldest) = oldest_kept(retention_days, today) else {
        return Ok(0);
    };
    writers.retain(|day, _| *day >= oldest);
    let mut removed = 0;
    for (day, path) in day_files(dir)? {
        if day >= oldest {
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut side = path.clone().into_os_string();
            side.push(suffix);
            let _ = std::fs::remove_file(side);
        }
        removed += 1;
    }
    Ok(removed)
}

struct RawRow {
    key: String,
    schema: String,
    at: i64,
    venue_ts_ms: Option<i64>,
    slot: Option<i64>,
    source: String,
    status: String,
    errors: Option<String>,
    features: String,
    data: Option<String>,
}

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        key: r.get(0)?,
        schema: r.get(1)?,
        at: r.get(2)?,
        venue_ts_ms: r.get(3)?,
        slot: r.get(4)?,
        source: r.get(5)?,
        status: r.get(6)?,
        errors: r.get(7)?,
        features: r.get(8)?,
        data: r.get(9)?,
    })
}

fn decode(raw: RawRow) -> Result<HistoryRow> {
    let what = format!("history row {} at {}", raw.key, raw.at);
    let parse = |s: &str| serde_json::from_str::<Value>(s).with_context(|| what.clone());
    Ok(HistoryRow {
        source: serde_json::from_value(Value::String(raw.source)).with_context(|| what.clone())?,
        status: serde_json::from_value(Value::String(raw.status)).with_context(|| what.clone())?,
        errors: match raw.errors.as_deref() {
            Some(e) => serde_json::from_value(parse(e)?).with_context(|| what.clone())?,
            None => Vec::new(),
        },
        features: serde_json::from_value(parse(&raw.features)?).with_context(|| what.clone())?,
        data: raw.data.as_deref().map(parse).transpose()?,
        key: raw.key,
        schema: raw.schema,
        observed_at_ms: raw.at,
        venue_ts_ms: raw.venue_ts_ms,
        slot: raw.slot.map(|s| s as u64),
    })
}

fn insert(conn: &mut Connection, rows: &[&HistoryRow]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut added = 0;
    for r in rows {
        let errors = if r.errors.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&r.errors)?)
        };
        let data = r.data.as_ref().map(serde_json::to_string).transpose()?;
        added += tx.execute(
            INSERT_SQL,
            params![
                r.key,
                r.schema,
                r.observed_at_ms,
                r.venue_ts_ms,
                r.slot.map(|s| s as i64),
                r.source.as_str(),
                r.status.as_str(),
                errors,
                serde_json::to_string(&r.features)?,
                data
            ],
        )?;
    }
    tx.commit()?;
    Ok(added)
}

#[async_trait]
impl HistoryStore for SqliteHistoryStore {
    async fn append(&self, rows: &[HistoryRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let rows = rows.to_vec();
        self.blocking(move |dir, retention, writers| {
            let today = day_of(now_ms());
            let oldest = oldest_kept(retention, today);
            let mut by_day: BTreeMap<i64, Vec<&HistoryRow>> = BTreeMap::new();
            for r in &rows {
                let day = day_of(r.observed_at_ms);
                if oldest.is_none_or(|o| day >= o) {
                    by_day.entry(day).or_default().push(r);
                }
            }
            let mut added = 0;
            for (day, rows) in by_day {
                if !writers.contains_key(&day) {
                    let path = dir.join(day_name(day)?);
                    let new_file = !path.exists();
                    let conn = open_day(&path)?;
                    while writers.len() >= OPEN_DAYS {
                        let Some(first) = writers.keys().next().copied() else {
                            break;
                        };
                        writers.remove(&first);
                    }
                    writers.insert(day, conn);
                    if new_file {
                        // A new day file: drop the ones past retention.
                        if let Err(e) = sweep_files(dir, retention, today, writers) {
                            let error = format!("{e:#}");
                            warn!(dir = %dir.display(), %error, "history retention sweep failed");
                        }
                    }
                }
                if let Some(conn) = writers.get_mut(&day) {
                    added += insert(conn, &rows)?;
                }
            }
            Ok(added)
        })
        .await
    }

    async fn range(&self, key: &str, from_ms: i64, to_ms: i64) -> Result<Vec<HistoryRow>> {
        if from_ms >= to_ms {
            return Ok(Vec::new());
        }
        let key = key.to_string();
        self.blocking(move |dir, _, _| {
            let (first, last) = (day_of(from_ms), day_of(to_ms - 1));
            let mut out = Vec::new();
            for (day, path) in day_files(dir)? {
                if day < first || day > last {
                    continue;
                }
                let conn = open_day(&path)?;
                let mut stmt = conn.prepare(RANGE_SQL)?;
                let rows = stmt.query_map(params![key, from_ms, to_ms], raw_row)?;
                for raw in rows {
                    out.push(decode(raw?)?);
                }
            }
            Ok(out)
        })
        .await
    }

    async fn asof(
        &self,
        keys: &[String],
        t_ms: i64,
        max_age_ms: u64,
    ) -> Result<Vec<Option<HistoryRow>>> {
        let keys = keys.to_vec();
        self.blocking(move |dir, _, _| {
            let floor = t_ms.saturating_sub(i64::try_from(max_age_ms).unwrap_or(i64::MAX));
            let (first, last) = (day_of(floor), day_of(t_ms));
            let mut out: Vec<Option<HistoryRow>> = vec![None; keys.len()];
            let mut files = day_files(dir)?;
            files.retain(|(day, _)| (first..=last).contains(day));
            for (_, path) in files.iter().rev() {
                if out.iter().all(Option::is_some) {
                    break;
                }
                let conn = open_day(path)?;
                let mut stmt = conn.prepare(ASOF_SQL)?;
                for (slot, key) in out.iter_mut().zip(&keys) {
                    if slot.is_none() {
                        if let Some(raw) = stmt
                            .query_row(params![key, t_ms, floor], raw_row)
                            .optional()?
                        {
                            *slot = Some(decode(raw)?);
                        }
                    }
                }
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, ReadError};
    use chrono::NaiveDateTime;
    use serde_json::json;

    const TSLA: &str = "mkt_ctx/1:hyperliquid:xyz:TSLA";
    const ETH: &str = "mkt_ctx/1:hyperliquid:ETH";

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    fn row(key: &str, at: i64, mark: f64) -> HistoryRow {
        HistoryRow {
            key: key.into(),
            schema: "mkt_ctx/1".into(),
            observed_at_ms: at,
            venue_ts_ms: None,
            slot: None,
            source: ObsSource::Live,
            status: ObsStatus::Ok,
            errors: vec![],
            features: [("mark".to_string(), json!(mark))].into(),
            data: None,
        }
    }

    /// Fri 2026-10-02 20:00 EDT = Sat 00:00 UTC: the weekend anchor sits on a
    /// day-file boundary.
    #[tokio::test]
    async fn append_range_and_asof_across_a_utc_day_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteHistoryStore::open(dir.path(), 0).unwrap();
        let midnight = utc("2026-10-03 00:00");
        let mut err = row(ETH, midnight - 30_000, 0.0);
        err.status = ObsStatus::Error;
        err.features.clear();
        err.errors = vec![ReadError::new("ctx", ErrorClass::Timeout, "no answer")];
        let mut book = row(TSLA, midnight + 60_000, 3.0);
        book.data = Some(json!({"levels": [[1.0, 2.0]]}));
        book.venue_ts_ms = Some(midnight + 59_000);
        book.slot = Some(7);
        let rows = [
            row(TSLA, midnight - 60_000, 1.0),
            row(TSLA, midnight, 2.0),
            book.clone(),
            err.clone(),
        ];
        assert_eq!(store.append(&rows).await.unwrap(), 4);
        assert_eq!(store.append(&rows[..2]).await.unwrap(), 0, "re-append");
        assert!(dir.path().join("20261002.db").exists());
        assert!(dir.path().join("20261003.db").exists());

        // Half-open range across both files, oldest first; rows round-trip.
        let got = store
            .range(TSLA, midnight - 120_000, midnight + 60_000)
            .await
            .unwrap();
        assert_eq!(got, rows[..2].to_vec());
        let all = store.range(TSLA, 0, i64::MAX).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], book);
        assert!(store
            .range(TSLA, midnight, midnight)
            .await
            .unwrap()
            .is_empty());

        let keys = [
            TSLA.to_string(),
            ETH.to_string(),
            "mkt_ctx/1:hyperliquid:xyz:NVDA".to_string(),
        ];
        let at = store.asof(&keys, midnight + 30_000, 120_000).await.unwrap();
        assert_eq!(at[0].as_ref().unwrap().observed_at_ms, midnight);
        assert_eq!(at[1].as_ref(), Some(&err), "found in the previous day file");
        assert!(at[2].is_none());
        let stale = store.asof(&keys[1..2], midnight + 30_000, 59_999).await;
        assert!(stale.unwrap()[0].is_none(), "older than max_age");
        let any_age = store.asof(&keys[..1], i64::MAX, u64::MAX).await.unwrap();
        assert_eq!(any_age[0].as_ref(), Some(&book));

        // A reader sees the same rows and creates nothing.
        let reader = SqliteHistoryStore::reader(dir.path());
        assert_eq!(reader.range(TSLA, 0, i64::MAX).await.unwrap(), all);
        let missing = SqliteHistoryStore::reader(&dir.path().join("nope"));
        assert!(missing.range(TSLA, 0, i64::MAX).await.unwrap().is_empty());
        assert!(!dir.path().join("nope").exists());
    }

    #[tokio::test]
    async fn retention_sweeps_old_day_files() {
        let dir = tempfile::tempdir().unwrap();
        let now = now_ms();
        let today = day_of(now);
        for day in [today - 40, today - 31, today - 30] {
            let path = dir.path().join(day_name(day).unwrap());
            drop(open_day(&path).unwrap());
            std::fs::write(format!("{}-wal", path.display()), b"").unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), b"kept").unwrap();

        let store = SqliteHistoryStore::open(dir.path(), 30).unwrap();
        let days =
            |d: &Path| -> Vec<i64> { day_files(d).unwrap().into_iter().map(|(d, _)| d).collect() };
        assert_eq!(days(dir.path()), [today - 30]);
        let gone = dir
            .path()
            .join(format!("{}-wal", day_name(today - 40).unwrap()));
        assert!(!gone.exists());
        assert!(dir.path().join("notes.txt").exists());

        // Rows past retention are not appended; a new day file sweeps again.
        assert_eq!(
            store
                .append(&[row(TSLA, now - 40 * DAY_MS, 1.0)])
                .await
                .unwrap(),
            0
        );
        drop(open_day(&dir.path().join(day_name(today - 35).unwrap())).unwrap());
        assert_eq!(store.append(&[row(TSLA, now, 1.0)]).await.unwrap(), 1);
        assert_eq!(days(dir.path()), [today - 30, today]);

        // A later clock: day today-30 falls out.
        assert_eq!(store.sweep(now + DAY_MS).unwrap(), 1);
        assert_eq!(days(dir.path()), [today]);

        // retention 0 and a reader never delete.
        let keep = SqliteHistoryStore::open(dir.path(), 0).unwrap();
        assert_eq!(keep.sweep(now + 400 * DAY_MS).unwrap(), 0);
        assert_eq!(
            SqliteHistoryStore::reader(dir.path())
                .sweep(now + 400 * DAY_MS)
                .unwrap(),
            0
        );
        assert_eq!(days(dir.path()), [today]);
    }

    #[test]
    fn day_names_round_trip() {
        let day = day_of(utc("2026-10-03 00:00"));
        assert_eq!(day_name(day).unwrap(), "20261003.db");
        assert_eq!(day_of_name("20261003.db"), Some(day));
        assert_eq!(day_of(utc("2026-10-02 23:59")), day - 1);
        for bad in ["20261003.db-wal", "2026103.db", "20261332.db", "notes.txt"] {
            assert_eq!(day_of_name(bad), None, "{bad}");
        }
    }
}
