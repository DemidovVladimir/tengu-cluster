//! JSON import — the generic path for any history (another venue, a vendor
//! file, offline test fixtures) into `market.db`: a file holding an array of
//! `{"instrument", "interval", "source", "bars": [{t_open_ms, o, h, l, c, v,
//! n?}], "funding": [{t_ms, rate_1h, premium?}]}` (`bars` / `funding`
//! optional; `interval` required with bars; `source` default
//! `json:<file name>`).
//!
//! | Rule | Value |
//! |---|---|
//! | Check | every dataset decoded and checked (`domain::marketdata_decode::json_datasets`) before anything is written: one bad row ⇒ nothing imported |
//! | Write | `put_bars` / `put_funding` per dataset (upsert: a re-import replaces) |
//! | Partial bars | the file's bars are history: a bar not closed at import time is dropped (noted) |

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use super::{BackfillReport, ReportRow};
use crate::domain::marketdata_decode::{closed_bars, json_datasets};
use crate::ports::market_data::MarketDataStore;

/// Import the datasets of `path` (module table).
pub(crate) async fn import_json(
    path: &Path,
    store: &dyn MarketDataStore,
    now_ms: i64,
) -> Result<BackfillReport> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let sets = json_datasets(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let default_source = format!("json:{name}");
    let mut report = BackfillReport::default();
    for d in sets {
        let source = d.source.clone().unwrap_or_else(|| default_source.clone());
        if let (Some(interval), false) = (d.interval, d.bars.is_empty()) {
            let mut row = ReportRow::new(&d.instrument, "bars", Some(interval), &source);
            row.reads = 1;
            let total = d.bars.len();
            let bars = closed_bars(d.bars, interval, now_ms);
            if bars.len() < total {
                row.notes.push(format!(
                    "{} bar(s) not closed at import time dropped",
                    total - bars.len()
                ));
            }
            let n = store
                .put_bars(&d.instrument, interval, &source, &bars)
                .await?;
            row.wrote(n, bars.iter().map(|b| b.t_open_ms));
            report.rows.push(row);
        }
        if !d.funding.is_empty() {
            let mut row = ReportRow::new(&d.instrument, "funding", None, &source);
            row.reads = 1;
            let n = store
                .put_funding(&d.instrument, &source, &d.funding)
                .await?;
            row.wrote(n, d.funding.iter().map(|p| p.t_ms));
            report.rows.push(row);
        }
    }
    report.notes.push(format!("from {}", path.display()));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::domain::marketdata::Interval;

    const H: i64 = 3_600_000;
    const MINT: &str = "solana:So11111111111111111111111111111111111111112";

    #[tokio::test]
    async fn datasets_land_with_their_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rh_fixture.json");
        std::fs::write(
            &file,
            format!(
                r#"[
              {{"instrument": "{MINT}", "interval": "1h",
               "bars": [{{"t_open_ms": 0, "o": 1, "h": 2, "l": 0.5, "c": 1.5, "v": 10}},
                        {{"t_open_ms": {H}, "o": 1.5, "h": 2, "l": 1, "c": 1.8, "v": 7, "n": 4}},
                        {{"t_open_ms": {h2}, "o": 1.8, "h": 2, "l": 1, "c": 1.9, "v": 1}}]}},
              {{"instrument": "hyperliquid:xyz:TSLA", "source": "vendor:x",
               "funding": [{{"t_ms": {H}, "rate_1h": 0.0000125}}]}}
            ]"#,
                h2 = 2 * H
            ),
        )
        .unwrap();
        let store = SqliteMarketData::open(&dir.path().join("state")).unwrap();
        // At 02:30 the 02:00 bar is still open.
        let report = import_json(&file, &store, 2 * H + H / 2).await.unwrap();
        assert_eq!(report.rows.len(), 2);
        let bars = &report.rows[0];
        assert_eq!(
            (
                bars.instrument.as_str(),
                bars.kind,
                bars.source.as_str(),
                bars.rows
            ),
            (MINT, "bars", "json:rh_fixture.json", 2)
        );
        assert_eq!(
            bars.notes,
            vec!["1 bar(s) not closed at import time dropped"]
        );
        let funding = &report.rows[1];
        assert_eq!(
            (funding.kind, funding.source.as_str(), funding.rows),
            ("funding", "vendor:x", 1)
        );
        let s = store.bars(MINT, Interval::H1, 0, i64::MAX).await.unwrap();
        assert_eq!(s.bars.len(), 2);
        assert_eq!(s.bars[1].n, Some(4));
        assert_eq!(
            store
                .funding("hyperliquid:xyz:TSLA", 0, i64::MAX)
                .await
                .unwrap()
                .points[0]
                .rate_1h,
            0.0000125
        );

        // One bad row anywhere: nothing written.
        let bad = dir.path().join("bad.json");
        std::fs::write(
            &bad,
            r#"[{"instrument": "hyperliquid:SOL", "funding": [{"t_ms": 0, "rate_1h": 0.1}]},
                {"instrument": "hyperliquid:BTC", "interval": "1h",
                 "bars": [{"t_open_ms": 0, "o": 1, "h": 0.5, "l": 0.4, "c": 1, "v": 1}]}]"#,
        )
        .unwrap();
        let e = import_json(&bad, &store, 10 * H).await.unwrap_err();
        assert!(
            e.to_string()
                .contains("dataset 1 (hyperliquid:BTC) bars row 0"),
            "{e}"
        );
        assert!(store
            .coverage(Some("hyperliquid:SOL"))
            .await
            .unwrap()
            .is_empty());
        assert!(import_json(&dir.path().join("nope.json"), &store, 0)
            .await
            .is_err());
    }
}
