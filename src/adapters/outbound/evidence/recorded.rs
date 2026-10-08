//! Read-only recorded and backfilled rows for `tengu evidence`:
//! [`DayFiles`] — `ports::evidence::RecordedHistory` over recorder day files
//! (`<dir>/<YYYYMMDD>.db`, table `obs_history`, the schema of
//! `outbound/history_sqlite.rs`) of one or more dirs; [`MarketDb`] —
//! `ports::evidence::BackfillSource` over an xlab `market.db` (`bars`,
//! `funding`). Both open every file through [`super::open_read_only`]
//! (never `SqliteHistoryStore` / `SqliteMarketData`, which create, migrate
//! or sweep). A missing day file is no rows; a row lives in the file of its
//! UTC day.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use chrono::DateTime;
use rusqlite::{params, Connection};
use serde_json::Value;

use super::open_read_only;
use crate::domain::observation::{Features, ObsStatus};
use crate::ports::evidence::{BackfillSource, BackfilledBar, RecordedHistory, RecordedRow};

const DAY_MS: i64 = 86_400_000;

pub(crate) struct DayFiles {
    dirs: Vec<PathBuf>,
    /// Open read-only connections by file (one open per file per process).
    conns: RefCell<BTreeMap<PathBuf, Rc<Connection>>>,
}

impl DayFiles {
    pub(crate) fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            conns: RefCell::default(),
        }
    }

    fn conn(&self, path: &Path) -> Result<Rc<Connection>> {
        if let Some(c) = self.conns.borrow().get(path) {
            return Ok(Rc::clone(c));
        }
        let c = Rc::new(open_read_only(path)?);
        self.conns
            .borrow_mut()
            .insert(path.to_path_buf(), Rc::clone(&c));
        Ok(c)
    }

    /// The existing day files of `[from, to)` in every dir.
    fn files(&self, from_ms: i64, to_ms: i64) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if to_ms <= from_ms {
            return out;
        }
        let (first, last) = (from_ms.div_euclid(DAY_MS), (to_ms - 1).div_euclid(DAY_MS));
        for dir in &self.dirs {
            for day in first..=last {
                let Some(name) = DateTime::from_timestamp_millis(day * DAY_MS)
                    .map(|d| d.format("%Y%m%d.db").to_string())
                else {
                    continue;
                };
                let path = dir.join(name);
                if path.is_file() {
                    out.push(path);
                }
            }
        }
        out
    }
}

fn status_of(s: &str) -> ObsStatus {
    match s {
        "ok" => ObsStatus::Ok,
        "partial" => ObsStatus::Partial,
        "absent" => ObsStatus::Absent,
        _ => ObsStatus::Error,
    }
}

impl RecordedHistory for DayFiles {
    fn instants(
        &self,
        schema: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(String, i64, ObsStatus)>> {
        let mut out = Vec::new();
        for path in self.files(from_ms, to_ms) {
            let conn = self.conn(&path)?;
            let mut stmt = conn.prepare_cached(
                "SELECT key, observed_at_ms, status FROM obs_history \
                 WHERE schema = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3",
            )?;
            let rows = stmt.query_map(params![schema, from_ms, to_ms], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (key, at, status) = row.with_context(|| format!("read {}", path.display()))?;
                out.push((key, at, status_of(&status)));
            }
        }
        out.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
        Ok(out)
    }

    fn keys(&self, schema: &str, from_ms: i64, to_ms: i64) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for path in self.files(from_ms, to_ms) {
            let conn = self.conn(&path)?;
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT key FROM obs_history \
                 WHERE schema = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3",
            )?;
            for key in stmt.query_map(params![schema, from_ms, to_ms], |r| r.get::<_, String>(0))? {
                out.insert(key?);
            }
        }
        Ok(out)
    }

    fn rows(
        &self,
        key: &str,
        from_ms: i64,
        to_ms: i64,
        with_data: bool,
    ) -> Result<Vec<RecordedRow>> {
        let mut out = Vec::new();
        let data_col = if with_data { "data" } else { "NULL" };
        for path in self.files(from_ms, to_ms) {
            let conn = self.conn(&path)?;
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT observed_at_ms, status, errors, features, {data_col} FROM obs_history \
                 WHERE key = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3 ORDER BY observed_at_ms"
            ))?;
            let rows = stmt.query_map(params![key, from_ms, to_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            for row in rows {
                let (at, status, errors, features, data) = row?;
                let features: Features = serde_json::from_str(&features)
                    .with_context(|| format!("{}: features of {key} at {at}", path.display()))?;
                let data: Option<Value> =
                    match data {
                        Some(d) => Some(serde_json::from_str(&d).with_context(|| {
                            format!("{}: data of {key} at {at}", path.display())
                        })?),
                        None => None,
                    };
                out.push(RecordedRow {
                    key: key.to_string(),
                    observed_at_ms: at,
                    status: status_of(&status),
                    features,
                    data,
                    errors,
                });
            }
        }
        out.sort_by_key(|r| r.observed_at_ms);
        Ok(out)
    }

    fn describe(&self) -> String {
        self.dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

pub(crate) struct MarketDb {
    path: PathBuf,
    conn: rusqlite::Connection,
}

impl MarketDb {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        let conn = open_read_only(&path)?;
        Ok(Self { path, conn })
    }
}

impl BackfillSource for MarketDb {
    fn bars(
        &self,
        instrument: &str,
        interval: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<BackfilledBar>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t_open_ms, c, n, fetched_at_ms FROM bars \
             WHERE instrument = ?1 AND interval = ?2 AND t_open_ms >= ?3 AND t_open_ms < ?4 ORDER BY t_open_ms",
        )?;
        let rows = stmt
            .query_map(params![instrument, interval, from_ms, to_ms], |r| {
                Ok(BackfilledBar {
                    t_open_ms: r.get(0)?,
                    close: r.get(1)?,
                    trades: r.get(2)?,
                    fetched_at_ms: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn funding(&self, instrument: &str, from_ms: i64, to_ms: i64) -> Result<Vec<(i64, f64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t_ms, rate_1h FROM funding WHERE instrument = ?1 AND t_ms >= ?2 AND t_ms < ?3 ORDER BY t_ms",
        )?;
        let rows = stmt
            .query_map(params![instrument, from_ms, to_ms], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_files_across_dirs_and_days() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let schema = "CREATE TABLE obs_history (key TEXT NOT NULL, schema TEXT NOT NULL, observed_at_ms INTEGER NOT NULL, \
            venue_ts_ms INTEGER, slot INTEGER, source TEXT NOT NULL, status TEXT NOT NULL, errors TEXT, features TEXT NOT NULL, data TEXT);";
        // 2026-10-04 = day 20730; 2026-10-05 = 20731.
        let d4 = 20_730 * DAY_MS;
        for (dir, name, at, status) in [
            (&a, "20261004.db", d4 + 1_000, "ok"),
            (&a, "20261005.db", d4 + DAY_MS + 5, "error"),
            (&b, "20261004.db", d4 + 2_000, "ok"),
        ] {
            let c = Connection::open(dir.join(name)).unwrap();
            c.execute_batch(schema).unwrap();
            c.execute(
                "INSERT INTO obs_history VALUES ('mkt_ctx/1:x:A', 'mkt_ctx/1', ?1, NULL, NULL, 'live', ?2, NULL, '{\"mid\":1.5}', '{\"book\":1}')",
                params![at, status],
            )
            .unwrap();
        }
        let h = DayFiles::new(vec![a, b]);
        let inst = h.instants("mkt_ctx/1", d4, d4 + 2 * DAY_MS).unwrap();
        assert_eq!(inst.len(), 3);
        assert_eq!(inst[2].2, ObsStatus::Error);
        assert_eq!(h.keys("mkt_ctx/1", d4, d4 + 1_500).unwrap().len(), 1);
        let rows = h.rows("mkt_ctx/1:x:A", d4, d4 + DAY_MS, false).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].features["mid"], 1.5);
        assert!(rows[0].data.is_none());
        let rows = h.rows("mkt_ctx/1:x:A", d4, d4 + DAY_MS, true).unwrap();
        assert_eq!(rows[1].data.as_ref().unwrap()["book"], 1);
        assert!(h
            .rows("mkt_ctx/1:x:A", d4 - DAY_MS, d4, false)
            .unwrap()
            .is_empty());
    }
}
