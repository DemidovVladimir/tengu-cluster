//! HL S3 archive import — local `asset_ctxs` files the operator downloaded
//! (`aws s3 cp --request-payer requester
//! s3://hyperliquid-archive/asset_ctxs/<YYYYMMDD>.csv.lz4 <dir>/`) →
//! `market.db` `ctx` rows, `source = hl-archive:asset_ctxs`, instrument
//! `hyperliquid:<coin>` (main-dex perps only: the archive has no `xyz:*`).
//!
//! | Step | Rule |
//! |---|---|
//! | Files | every `*.csv.lz4` (LZ4 frame, `lz4_flex`) and `*.csv` under the dir, recursively, by path; symlinked dirs are not followed |
//! | Rows | `domain::marketdata_decode::hl_asset_ctxs_csv` (header names, empty cell = `None`) |
//! | A file that does not decode | a run error naming the file and line; nothing of that file is written; the next file runs |
//! | Write | per file, one `put_ctx` per coin (upsert: a re-import replaces) |

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{BackfillReport, ReportRow};
use crate::domain::market::InstrumentId;
use crate::domain::marketdata::CtxPoint;
use crate::domain::marketdata_decode::hl_asset_ctxs_csv;
use crate::ports::market_data::MarketDataStore;

/// The `source` of archive rows.
pub(crate) const ARCHIVE_SOURCE: &str = "hl-archive:asset_ctxs";

/// `*.csv.lz4` / `*.csv` under `dir`, recursively, sorted.
pub(crate) fn archive_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if kind.is_dir() {
                stack.push(path);
            } else if name.ends_with(".csv.lz4") || name.ends_with(".csv") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A file's text: LZ4-frame decoded for `*.lz4`, else as is.
fn read_text(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut text = String::new();
    if path.extension().is_some_and(|e| e == "lz4") {
        lz4_flex::frame::FrameDecoder::new(std::io::BufReader::new(file))
            .read_to_string(&mut text)
            .with_context(|| format!("{} is not an LZ4 frame of UTF-8 text", path.display()))?;
    } else {
        std::io::BufReader::new(file)
            .read_to_string(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
    }
    Ok(text)
}

/// One file → rows grouped by full instrument id.
fn read_file(path: &Path) -> Result<BTreeMap<String, Vec<CtxPoint>>> {
    let rows = hl_asset_ctxs_csv(&read_text(path)?).map_err(|e| anyhow::anyhow!(e))?;
    let mut by: BTreeMap<String, Vec<CtxPoint>> = BTreeMap::new();
    for (coin, point) in rows {
        let id = InstrumentId::hyperliquid(&coin).map_err(|e| anyhow::anyhow!(e))?;
        by.entry(id.to_string()).or_default().push(point);
    }
    Ok(by)
}

/// Import every archive file under `dir` (module table). Fails only when
/// `dir` cannot be listed, holds no archive file, or the store fails.
pub(crate) async fn import_hl_archive(
    dir: &Path,
    store: &dyn MarketDataStore,
) -> Result<BackfillReport> {
    let files = archive_files(dir)?;
    if files.is_empty() {
        bail!("no *.csv.lz4 or *.csv file under {}", dir.display());
    }
    let mut report = BackfillReport::default();
    let mut rows: BTreeMap<String, ReportRow> = BTreeMap::new();
    let mut imported = 0usize;
    for path in &files {
        let p = path.clone();
        let parsed = tokio::task::spawn_blocking(move || read_file(&p))
            .await
            .context("archive read task")?;
        let by_instrument = match parsed {
            Ok(b) => b,
            Err(e) => {
                report.errors.push(format!("{}: {e:#}", path.display()));
                continue;
            }
        };
        imported += 1;
        for (instrument, points) in by_instrument {
            let n = store.put_ctx(&instrument, ARCHIVE_SOURCE, &points).await?;
            let row = rows
                .entry(instrument.clone())
                .or_insert_with(|| ReportRow::new(&instrument, "ctx", None, ARCHIVE_SOURCE));
            row.reads += 1;
            row.wrote(n, points.iter().map(|p| p.t_ms));
        }
    }
    report.rows = rows.into_values().collect();
    report.notes.push(format!(
        "{imported} of {} archive file(s) under {} imported",
        files.len(),
        dir.display()
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::domain::marketdata::parse_time;
    use std::io::Write;

    const HEADER: &str = "time,coin,funding,open_interest,prev_day_px,day_ntl_vlm,premium,oracle_px,mark_px,mid_px,impact_bid_px,impact_ask_px";

    fn lz4(text: &str) -> Vec<u8> {
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    #[tokio::test]
    async fn lz4_and_plain_files_import_into_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let day1 = format!(
            "{HEADER}\n2026-09-29T00:00:00Z,BTC,0.0000125,23456.7,64000,1e9,-0.0001,64010,64012,64011.5,64005,64018\n\
             2026-09-29T00:00:00Z,SOL,0.00001,1000,150,5e7,0.0001,150.1,150.2,150.15,150.0,150.3\n\
             2026-09-29T00:01:00Z,BTC,0.0000125,23457.0,64000,1e9,-0.0001,64011,64013,64012.5,64006,64019\n"
        );
        let nested = dir.path().join("asset_ctxs");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("20260929.csv.lz4"), lz4(&day1)).unwrap();
        std::fs::write(
            dir.path().join("20260930.csv"),
            format!(
                "{HEADER}\n2026-09-30T00:00:00Z,BTC,0.00001,23500,64012,1e9,0,64100,64102,,,\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not an archive").unwrap();
        std::fs::write(dir.path().join("broken.csv.lz4"), b"not lz4").unwrap();
        std::fs::write(
            dir.path().join("bad.csv"),
            format!("{HEADER}\n2026-09-30T00:00:00Z,BTC,x,1,1,1,1,1,1,1,1,1\n"),
        )
        .unwrap();

        let store = SqliteMarketData::open(&dir.path().join("state")).unwrap();
        let report = import_hl_archive(dir.path(), &store).await.unwrap();
        let ids: Vec<&str> = report.rows.iter().map(|r| r.instrument.as_str()).collect();
        assert_eq!(ids, vec!["hyperliquid:BTC", "hyperliquid:SOL"]);
        let btc = &report.rows[0];
        assert_eq!((btc.rows, btc.reads, btc.kind), (3, 2, "ctx"));
        assert_eq!(btc.first_ms, Some(parse_time("2026-09-29").unwrap()));
        assert_eq!(btc.last_ms, Some(parse_time("2026-09-30").unwrap()));
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("broken.csv.lz4") && e.contains("not an LZ4 frame")),
            "{:?}",
            report.errors
        );
        assert!(
            report.errors.iter().any(|e| e.contains("bad.csv")
                && e.contains("line 2 (2026-09-30T00:00:00Z, BTC): funding `x`")),
            "{:?}",
            report.errors
        );
        assert_eq!(
            report.notes,
            vec![format!(
                "2 of 4 archive file(s) under {} imported",
                dir.path().display()
            )]
        );

        let ctx = store.ctx("hyperliquid:BTC", 0, i64::MAX).await.unwrap();
        assert_eq!(ctx.points.len(), 3);
        assert_eq!(ctx.points[0].impact_ask, Some(64018.0));
        assert_eq!(ctx.points[2].mid, None, "empty cell");
        let cov = store.coverage(Some("hyperliquid:SOL")).await.unwrap();
        assert_eq!(cov[0].sources, vec![ARCHIVE_SOURCE]);

        // A re-import replaces: same rows, same count.
        let again = import_hl_archive(dir.path(), &store).await.unwrap();
        assert_eq!(again.rows_written(), report.rows_written());
        assert_eq!(
            store
                .ctx("hyperliquid:BTC", 0, i64::MAX)
                .await
                .unwrap()
                .points
                .len(),
            3
        );

        let empty = tempfile::tempdir().unwrap();
        let e = import_hl_archive(empty.path(), &store).await.unwrap_err();
        assert!(e.to_string().contains("no *.csv.lz4 or *.csv file"), "{e}");
        assert!(import_hl_archive(&empty.path().join("nope"), &store)
            .await
            .is_err());
    }
}
