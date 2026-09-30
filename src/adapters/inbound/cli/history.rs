//! `tengu history range | asof` — read the recorder's day files
//! (`[recorder]`, `<TENGU_HOME>/state/<xmarket.state>/history/`) as JSON
//! lines with full keys. Thin: a `SqliteHistoryStore::reader`, which creates
//! and deletes nothing.

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use crate::adapters::outbound::history_sqlite::SqliteHistoryStore;
use crate::config::paths::resolve_tengu_home;
use crate::config::Config;
use crate::ports::history::HistoryStore;

#[derive(Subcommand)]
pub(super) enum HistoryAction {
    /// Rows of one key with from <= observed_at < to, oldest first.
    Range {
        /// Observation key, e.g. mkt_ctx/1:hyperliquid:xyz:TSLA
        key: String,
        /// Start, inclusive: epoch ms or RFC 3339 (2026-10-02T20:00:00-04:00).
        #[arg(long)]
        from: String,
        /// End, exclusive: epoch ms or RFC 3339.
        #[arg(long)]
        to: String,
    },
    /// Per key, the latest row at or before --at (`"row": null` when none).
    Asof {
        /// One or more observation keys.
        #[arg(required = true)]
        keys: Vec<String>,
        /// Epoch ms or RFC 3339.
        #[arg(long)]
        at: String,
        /// Ignore rows older than this; default: any age.
        #[arg(long)]
        max_age_secs: Option<u64>,
    },
}

pub(super) async fn run_history(config: &Config, action: HistoryAction) -> Result<()> {
    let xmarket = config.xmarket.as_ref().ok_or_else(|| {
        anyhow!("no [xmarket] section: history lives in <TENGU_HOME>/state/<xmarket.state>/history")
    })?;
    let dir = xmarket.history_dir(&resolve_tengu_home());
    let store = SqliteHistoryStore::reader(&dir);
    match action {
        HistoryAction::Range { key, from, to } => {
            let rows = store
                .range(&key, parse_time(&from)?, parse_time(&to)?)
                .await?;
            for row in &rows {
                println!("{}", serde_json::to_string(row)?);
            }
            eprintln!("{} rows from {}", rows.len(), dir.display());
        }
        HistoryAction::Asof {
            keys,
            at,
            max_age_secs,
        } => {
            let max_age_ms = max_age_secs.map_or(u64::MAX, |s| s.saturating_mul(1000));
            let rows = store.asof(&keys, parse_time(&at)?, max_age_ms).await?;
            for (key, row) in keys.iter().zip(rows) {
                println!("{}", serde_json::json!({ "key": key, "row": row }));
            }
        }
    }
    Ok(())
}

/// Epoch ms, or RFC 3339 (`2026-10-02T20:00:00-04:00`).
fn parse_time(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis())
        .with_context(|| {
            format!("`{s}` is neither epoch ms nor RFC 3339 (2026-10-02T20:00:00-04:00)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_as_ms_or_rfc3339() {
        assert_eq!(parse_time("1759449600000").unwrap(), 1_759_449_600_000);
        // Fri 2026-10-02 20:00 EDT = Sat 00:00 UTC.
        assert_eq!(
            parse_time("2026-10-02T20:00:00-04:00").unwrap(),
            parse_time("2026-10-03T00:00:00Z").unwrap()
        );
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 10, 3)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(parse_time("2026-10-03T00:00:00Z").unwrap(), midnight);
        let e = parse_time("friday").unwrap_err().to_string();
        assert!(e.contains("neither epoch ms nor RFC 3339"), "{e}");
    }
}
