//! Backfill — fill `market.db` (`MarketDataStore`, `outbound/market_data.rs`)
//! from public history and local files (xlab, `docs/xlab-2026-10-01.md` § 1,
//! § 4, § 10). Network fetchers send every request through the egress policy
//! (tool client, `check_url` = `[egress] allow_hosts`, audit line) and a
//! `[rate_limits.<name>]` budget; a 429 / 5xx / timeout is retried per
//! [`Retry`] (`domain::backoff::next_delay`, `Retry-After` respected). Rows go
//! through the pure decoders of `domain/marketdata_decode.rs` (SEC:
//! `domain/sec.rs`). Each run returns a [`BackfillReport`] (full ids).
//!
//! | File | Reads | Writes (`source`) | Resume |
//! |---|---|---|---|
//! | `sec.rs` | SEC EDGAR `company_tickers.json`, `submissions/CIK##########.json` (+ older pages), one filing index page per filing (`Accepted`, New York → UTC) | `events` (`sec`, kind `filing`), `event_coverage` (one per instrument; no CIK ⇒ not covered) | a stored accession keeps its time (no index read); coverage joins a touching stored span |
//! | `hl.rs` | HL `candleSnapshot` (newest 5 000 bars / interval: an older start is clamped, noted), `fundingHistory` (≤ 500 rows / reply) via `HlInfo` | `bars`, `funding` (`hl`; `hl:<host>` off mainnet) | [`missing_ranges`]: from the bar after the last stored one / after the last stored `t_ms`; a `from` ≥ one bar (funding: 1 h) before the first stored row also fetches that head |
//! | `gecko.rs` | GeckoTerminal `/networks/<network>/pools/<pool>/ohlcv/<timeframe>` (≤ 1 000 bars / reply, newest first, paged backwards) | `bars` (`gecko:<network>:<pool>`) | as HL bars |
//! | `hl_archive.rs` | local `asset_ctxs` files (`*.csv.lz4` LZ4 frame, `*.csv`) under a dir | `ctx` (`hl-archive:asset_ctxs`) | a re-import replaces |
//! | `json.rs` | a JSON dataset file (`domain::marketdata_decode::Dataset`) | `bars`, `funding` (the dataset's, else `json:<file name>`) | a re-import replaces |
//!
//! | Rule | Value |
//! |---|---|
//! | Partial bars | a bar still open at fetch time is never stored |
//! | A failed request | after the retries: the error names the instrument; that instrument / kind stops, rows already written stay, the next instrument runs |
//! | A bad row | the decoder's error (row + time) — the page is not written |

pub(crate) mod gecko;
pub(crate) mod hl;
pub(crate) mod hl_archive;
pub(crate) mod json;
pub(crate) mod sec;

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::adapters::outbound::http_class::read_error;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::domain::backoff::{next_delay, BackoffPolicy, Delay};
use crate::domain::marketdata::{fmt_time, Interval};
use crate::domain::observation::ErrorClass;
use crate::ports::market_data::MarketDataStore;

/// Retry of one backfill request (module doc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Retry {
    pub policy: BackoffPolicy,
    /// A `Park` (quota, a long `Retry-After`) up to this long is waited out;
    /// longer fails the request.
    pub max_park_ms: u64,
}

impl Retry {
    /// Operator backfills: 5 retries, 1 s … 60 s full jitter, parks ≤ 5 min.
    pub(crate) const BACKFILL: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 2_000,
            cap_ms: 60_000,
            min_ms: 1_000,
            max_attempts: 5,
            quota_park_ms: 3_600_000,
        },
        max_park_ms: 300_000,
    };

    /// A tool call's backfill (`market_history` `fetch`): a model waits for
    /// it — 2 retries, 250 ms … 5 s, parks ≤ 10 s.
    pub(crate) const TOOL: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 500,
            cap_ms: 5_000,
            min_ms: 250,
            max_attempts: 2,
            quota_park_ms: 3_600_000,
        },
        max_park_ms: 10_000,
    };

    /// `call` until it succeeds or `next_delay` stops (the error is then
    /// returned). `what` labels the log line.
    pub(crate) async fn run<T, F, Fut>(&self, what: &str, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 0u32;
        loop {
            let err = match call().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            attempt += 1;
            let r = read_error(what, &err);
            let wait =
                match next_delay(r.class, attempt, r.retry_after_ms, &self.policy, jitter01()) {
                    Delay::Retry(ms) => ms,
                    Delay::Park(ms) if ms <= self.max_park_ms => ms,
                    Delay::Park(_) | Delay::Stop => return Err(err),
                };
            tracing::warn!(
                what,
                class = r.class.as_str(),
                attempt,
                wait_ms = wait,
                error = %r.message,
                "backfill request failed; retrying"
            );
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }
}

/// What one run wrote for one instrument and kind.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ReportRow {
    /// Full instrument id.
    pub instrument: String,
    /// `bars` | `funding` | `ctx` | `filings` (SEC events).
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    pub source: String,
    /// Rows upserted.
    pub rows: usize,
    /// Requests attempted (HL, Gecko, SEC: retries and an egress denial
    /// count once) or files read (archive, JSON).
    pub reads: u32,
    /// Earliest / latest written row time (bars: `t_open_ms`; filings:
    /// `published_ms`).
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// Clamped start, resume, up to date.
    pub notes: Vec<String>,
    pub errors: Vec<String>,
    /// The class of each of `errors` recorded by [`ReportRow::fail`]
    /// (`http_class::read_error`; a tool reports it per field).
    #[serde(skip)]
    pub classes: Vec<ErrorClass>,
}

impl ReportRow {
    pub(crate) fn new(
        instrument: &str,
        kind: &'static str,
        interval: Option<Interval>,
        source: &str,
    ) -> Self {
        Self {
            instrument: instrument.to_string(),
            kind,
            interval,
            source: source.to_string(),
            rows: 0,
            reads: 0,
            first_ms: None,
            last_ms: None,
            notes: Vec::new(),
            errors: Vec::new(),
            classes: Vec::new(),
        }
    }

    /// Record a failed fetch: its text (`{e:#}`) and its class.
    pub(crate) fn fail(&mut self, e: &anyhow::Error) {
        self.errors.push(format!("{e:#}"));
        self.classes.push(read_error(self.kind, e).class);
    }

    /// Count `n` rows written spanning `times` (any order).
    pub(crate) fn wrote(&mut self, n: usize, times: impl IntoIterator<Item = i64>) {
        self.rows += n;
        for t in times {
            self.first_ms = Some(self.first_ms.map_or(t, |f| f.min(t)));
            self.last_ms = Some(self.last_ms.map_or(t, |l| l.max(t)));
        }
    }

    /// `instrument kind [interval]` — how notes and errors name the row.
    pub(crate) fn label(&self) -> String {
        match self.interval {
            Some(i) => format!("{} {} {i}", self.instrument, self.kind),
            None => format!("{} {}", self.instrument, self.kind),
        }
    }
}

/// One backfill / import run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct BackfillReport {
    pub rows: Vec<ReportRow>,
    /// Problems not tied to one row (a file that does not decode).
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

impl BackfillReport {
    /// Row errors + run errors.
    pub(crate) fn error_count(&self) -> usize {
        self.errors.len() + self.rows.iter().map(|r| r.errors.len()).sum::<usize>()
    }

    pub(crate) fn rows_written(&self) -> usize {
        self.rows.iter().map(|r| r.rows).sum()
    }

    /// The table (full ids, RFC 3339 UTC times), then notes and errors.
    pub(crate) fn render(&self) -> String {
        let opt_time = |t: Option<i64>| t.map(fmt_time).unwrap_or_else(|| "-".to_string());
        let rows: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|r| {
                vec![
                    r.instrument.clone(),
                    r.kind.to_string(),
                    r.interval.map_or("-".to_string(), |i| i.to_string()),
                    r.source.clone(),
                    r.rows.to_string(),
                    r.reads.to_string(),
                    opt_time(r.first_ms),
                    opt_time(r.last_ms),
                    if r.errors.is_empty() { "ok" } else { "error" }.to_string(),
                ]
            })
            .collect();
        let mut out = text_table(
            &[
                "instrument",
                "kind",
                "interval",
                "source",
                "rows",
                "reads",
                "first",
                "last",
                "status",
            ],
            &rows,
        );
        let notes: Vec<String> = self
            .notes
            .iter()
            .cloned()
            .chain(
                self.rows
                    .iter()
                    .flat_map(|r| r.notes.iter().map(move |n| format!("{}: {n}", r.label()))),
            )
            .collect();
        let errors: Vec<String> = self
            .errors
            .iter()
            .cloned()
            .chain(
                self.rows
                    .iter()
                    .flat_map(|r| r.errors.iter().map(move |e| format!("{}: {e}", r.label()))),
            )
            .collect();
        for (title, lines) in [("notes", notes), ("errors", errors)] {
            if !lines.is_empty() {
                out.push_str(&format!("\n{title}:\n"));
                for l in lines {
                    out.push_str(&format!("  {l}\n"));
                }
            }
        }
        out.push_str(&format!(
            "\n{} rows written, {} error(s)\n",
            self.rows_written(),
            self.error_count()
        ));
        out
    }
}

/// Left-aligned columns, two spaces apart, header first; cells never cut.
pub(crate) fn text_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut width: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in width.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&width)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut out = line(header.to_vec());
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

/// The stored span `(first, last)` of `instrument`'s `kind` (bars: at
/// `interval`), any source.
pub(crate) async fn stored_span(
    store: &dyn MarketDataStore,
    instrument: &str,
    kind: &str,
    interval: Option<Interval>,
) -> Result<Option<(i64, i64)>> {
    Ok(store
        .coverage(Some(instrument))
        .await?
        .into_iter()
        .find(|r| r.kind == kind && r.interval == interval)
        .map(|r| (r.first_ms, r.last_ms)))
}

/// The parts of `[from, to)` a fetch still needs given the stored span
/// `(first, last)`: the head `[from, first)` when `from` is at least
/// `head_min` before the first row (HL funding times run a few ms past the
/// hour: `head_min` = 1 h there, one bar for bars), and the tail from
/// `last + step` (the next row) — ascending, never empty.
pub(crate) fn missing_ranges(
    from: i64,
    to: i64,
    stored: Option<(i64, i64)>,
    step: i64,
    head_min: i64,
) -> Vec<(i64, i64)> {
    if from >= to {
        return Vec::new();
    }
    let Some((first, last)) = stored else {
        return vec![(from, to)];
    };
    let mut out = Vec::new();
    if first.saturating_sub(from) >= head_min.max(1) {
        out.push((from, first.min(to)));
    }
    let tail = from.max(last.saturating_add(step));
    if tail < to {
        out.push((tail, to));
    }
    out
}

/// Note for a bar range `[from, to)` (after clamping to what the venue
/// serves and to the closed bars) that holds no bar; `None` otherwise.
pub(crate) fn no_bars_note(from: i64, to: i64, interval: Interval) -> Option<String> {
    (from >= to).then(|| {
        format!(
            "nothing to fetch: no closed {interval} bar the source serves in {} … {}",
            fmt_time(from),
            fmt_time(to)
        )
    })
}

/// Note for a resumed fetch: what is stored and what is fetched.
pub(crate) fn resume_note(stored: Option<(i64, i64)>, ranges: &[(i64, i64)]) -> Option<String> {
    let (first, last) = stored?;
    let span = format!("stored {} … {}", fmt_time(first), fmt_time(last));
    Some(if ranges.is_empty() {
        format!("{span}: up to date, nothing fetched")
    } else {
        let parts: Vec<String> = ranges
            .iter()
            .map(|(a, b)| format!("{} … {}", fmt_time(*a), fmt_time(*b)))
            .collect();
        format!("{span}: fetching only {}", parts.join(" and "))
    })
}

/// `t` rounded down / up to the interval grid.
pub(crate) fn grid_floor(t: i64, step: i64) -> i64 {
    t.div_euclid(step) * step
}

pub(crate) fn grid_ceil(t: i64, step: i64) -> i64 {
    let f = grid_floor(t, step);
    if f == t {
        t
    } else {
        f.saturating_add(step)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// No real waits: 1 – 10 ms between attempts.
    pub(crate) const FAST: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 1,
            cap_ms: 10,
            min_ms: 1,
            max_attempts: 3,
            quota_park_ms: 10,
        },
        max_park_ms: 10,
    };
}

#[cfg(test)]
mod tests {
    use super::test_support::FAST;
    use super::*;
    use crate::adapters::outbound::http_class::HttpError;
    use crate::domain::observation::ErrorClass;

    const H: i64 = 3_600_000;

    #[test]
    fn missing_ranges_fetch_only_head_and_tail() {
        assert_eq!(missing_ranges(0, 10 * H, None, H, H), vec![(0, 10 * H)]);
        assert!(missing_ranges(5, 5, None, H, H).is_empty());
        // Stored [2H, 5H]: head [0, 2H), tail from the next bar 6H.
        assert_eq!(
            missing_ranges(0, 10 * H, Some((2 * H, 5 * H)), H, H),
            vec![(0, 2 * H), (6 * H, 10 * H)]
        );
        // Starting inside the stored span: only the tail.
        assert_eq!(
            missing_ranges(3 * H, 10 * H, Some((2 * H, 5 * H)), H, H),
            vec![(6 * H, 10 * H)]
        );
        // Up to date.
        assert!(missing_ranges(3 * H, 6 * H, Some((2 * H, 5 * H)), H, H).is_empty());
        // Entirely before the stored span: the head only, cut at `to`.
        assert_eq!(
            missing_ranges(0, H, Some((2 * H, 5 * H)), H, H),
            vec![(0, H)]
        );
        // Funding: the tail starts 1 ms after the last stored row; a first
        // row a few ms past `from` (HL settles just after the hour) is no head.
        assert_eq!(
            missing_ranges(H, 10 * H, Some((H + 40, 4 * H + 40)), 1, H),
            vec![(4 * H + 41, 10 * H)]
        );
        assert_eq!(
            missing_ranges(0, 10 * H, Some((H + 40, 4 * H + 40)), 1, H),
            vec![(0, H + 40), (4 * H + 41, 10 * H)]
        );
        let note = resume_note(Some((2 * H, 5 * H)), &[(6 * H, 10 * H)]).unwrap();
        assert_eq!(
            note,
            "stored 1970-01-01T02:00:00Z … 1970-01-01T05:00:00Z: fetching only \
             1970-01-01T06:00:00Z … 1970-01-01T10:00:00Z"
        );
        assert!(resume_note(Some((0, H)), &[])
            .unwrap()
            .contains("up to date"));
        assert!(resume_note(None, &[(0, H)]).is_none());
        assert_eq!(
            (grid_floor(H + 5, H), grid_ceil(H + 5, H), grid_ceil(H, H)),
            (H, 2 * H, H)
        );
        assert_eq!(grid_floor(-1, H), -H);
    }

    #[tokio::test]
    async fn retry_retries_transient_and_stops_on_fatal() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let got = FAST
            .run("t", || {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(HttpError::new(ErrorClass::Transient, "502").into())
                    } else {
                        Ok(n)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(got, 2);

        let calls = std::sync::atomic::AtomicU32::new(0);
        let e = FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err::<(), _>(HttpError::new(ErrorClass::Fatal, "404").into()) }
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("404"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry"
        );

        // Retries run out: the last error is returned.
        let calls = std::sync::atomic::AtomicU32::new(0);
        assert!(FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err::<(), _>(HttpError::new(ErrorClass::Timeout, "slow").into()) }
            })
            .await
            .is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "1 + 3 retries"
        );

        // A Retry-After beyond the park limit is not waited out.
        let mut slow = HttpError::new(ErrorClass::RateLimited, "429");
        slow.retry_after_ms = Some(60_000);
        let calls = std::sync::atomic::AtomicU32::new(0);
        assert!(FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let e = slow.clone();
                async move { Err::<(), _>(e.into()) }
            })
            .await
            .is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failure_keeps_its_text_and_class() {
        let mut row = ReportRow::new("hyperliquid:xyz:TSLA", "bars", Some(Interval::H1), "hl");
        row.fail(
            &HttpError::new(ErrorClass::RateLimited, "HTTP 429 from api.hyperliquid.xyz").into(),
        );
        row.fail(&anyhow::anyhow!("candleSnapshot xyz:TSLA 1h row 0: bad"));
        assert_eq!(
            row.classes,
            vec![ErrorClass::RateLimited, ErrorClass::Fatal]
        );
        assert!(row.errors[0].contains("HTTP 429"), "{:?}", row.errors);
        assert_eq!(row.errors[1], "candleSnapshot xyz:TSLA 1h row 0: bad");
    }

    #[test]
    fn the_report_names_every_row_in_full() {
        let pool = "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj";
        let mut a = ReportRow::new("hyperliquid:xyz:TSLA", "bars", Some(Interval::H1), "hl");
        a.wrote(2, [2 * H, H]);
        a.reads = 1;
        a.notes.push("start clamped".into());
        let mut b = ReportRow::new(
            "solana:So11111111111111111111111111111111111111112",
            "bars",
            Some(Interval::H1),
            &format!("gecko:solana:{pool}"),
        );
        b.errors.push("HTTP 404".into());
        let report = BackfillReport {
            rows: vec![a, b],
            ..Default::default()
        };
        assert_eq!((report.rows_written(), report.error_count()), (2, 1));
        let text = report.render();
        assert!(
            text.contains("solana:So11111111111111111111111111111111111111112"),
            "{text}"
        );
        assert!(text.contains(&format!("gecko:solana:{pool}")), "{text}");
        assert!(
            text.contains("1970-01-01T01:00:00Z  1970-01-01T02:00:00Z  ok"),
            "{text}"
        );
        assert!(
            text.contains("hyperliquid:xyz:TSLA bars 1h: start clamped"),
            "{text}"
        );
        assert!(
            text.contains("solana:So11111111111111111111111111111111111111112 bars 1h: HTTP 404"),
            "{text}"
        );
        assert!(text.ends_with("2 rows written, 1 error(s)\n"), "{text}");
        let t = text_table(&["a", "bb"], &[vec!["xyz".into(), "1".into()]]);
        assert_eq!(t, "a    bb\nxyz  1\n");
    }
}
