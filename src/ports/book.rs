//! `BookSource` — the L2 book the paper fill engine fills against, read
//! AFTER the simulated latency (`application/paper.rs`, tracker convention
//! 16). One engine, three sources:
//!
//! | Impl | Serves | Lands in |
//! |---|---|---|
//! | live | `hl_book` read with `max_age_secs = 0`, recorded as `hl_book/1:<id>` | `risk-gate-enforcement` (with `hl-book-tool`) |
//! | replay | `HistoryStore::asof(["hl_book/1:<id>"], clock now, …)` — the book as of decision time + latency | `ops-replay-harness` (M7) |
//! | tests | [`ScriptedBooks`]: the scripted book in force at the clock's now | here |

// Consumers land next wave (`risk-gate-enforcement`, `risk-paper-tools`).
#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::book::L2Book;
use crate::domain::market::InstrumentId;
use crate::domain::observation::ReadError;

/// One book read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BookRead {
    pub book: L2Book,
    /// When this process read it (live) or the recorder observed it
    /// (replay), ms since the epoch.
    pub observed_at_ms: i64,
}

impl BookRead {
    /// Venue timestamp of the snapshot (`book.venue_ts_ms`, HL `time`).
    pub(crate) fn venue_ts_ms(&self) -> i64 {
        self.book.venue_ts_ms
    }

    /// Age at `now_ms` from the older of the venue and read stamps
    /// (saturating at 0): a lagging venue snapshot and a late read both count.
    pub(crate) fn age_ms(&self, now_ms: i64) -> u64 {
        let stamp = self.venue_ts_ms().min(self.observed_at_ms);
        now_ms.saturating_sub(stamp).max(0) as u64
    }
}

#[async_trait]
pub(crate) trait BookSource: Send + Sync {
    /// The book of `instrument` now. `Err` = no book (the order is not
    /// filled): `not_applicable` for an id the venue does not know (HL
    /// `200 null`), `transient` / `timeout` / `rate_limited` for outages.
    async fn fresh_book(&self, instrument: &InstrumentId) -> Result<BookRead, ReadError>;
}

/// Test double: serves, for one instrument, the last scripted book whose
/// start is ≤ the clock's now (`observed_at_ms` = now), and records the
/// clock time of every read — so a test sees which book a fill used and
/// when it was read.
#[cfg(test)]
pub(crate) struct ScriptedBooks {
    clock: std::sync::Arc<dyn crate::ports::clock::Clock>,
    instrument: InstrumentId,
    /// `(from_ms, book)`, ascending by `from_ms`.
    script: Vec<(i64, L2Book)>,
    fail: Option<ReadError>,
    reads: std::sync::Mutex<Vec<i64>>,
}

#[cfg(test)]
impl ScriptedBooks {
    pub(crate) fn new(
        clock: std::sync::Arc<dyn crate::ports::clock::Clock>,
        instrument: InstrumentId,
        mut script: Vec<(i64, L2Book)>,
    ) -> Self {
        script.sort_by_key(|(from_ms, _)| *from_ms);
        Self {
            clock,
            instrument,
            script,
            fail: None,
            reads: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Every read fails with `error`.
    pub(crate) fn failing(mut self, error: ReadError) -> Self {
        self.fail = Some(error);
        self
    }

    /// Clock time of each read, in order.
    pub(crate) fn reads(&self) -> Vec<i64> {
        self.reads.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl BookSource for ScriptedBooks {
    async fn fresh_book(&self, instrument: &InstrumentId) -> Result<BookRead, ReadError> {
        use crate::domain::observation::ErrorClass;
        let now = self.clock.now_ms();
        self.reads.lock().unwrap().push(now);
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        if *instrument != self.instrument {
            return Err(ReadError::new(
                "hl_book",
                ErrorClass::NotApplicable,
                format!("no book for {instrument}"),
            ));
        }
        let book = self
            .script
            .iter()
            .rev()
            .find(|(from_ms, _)| *from_ms <= now)
            .map(|(_, book)| book.clone())
            .ok_or_else(|| {
                ReadError::new(
                    "hl_book",
                    ErrorClass::Transient,
                    format!("no book for {instrument} yet at {now}"),
                )
            })?;
        Ok(BookRead {
            book,
            observed_at_ms: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::book::L2Level;
    use crate::domain::observation::ErrorClass;
    use crate::ports::clock::ManualClock;

    fn book(bid: f64, ask: f64, venue_ts_ms: i64) -> L2Book {
        let lvl = |px| L2Level { px, sz: 1.0, n: 1 };
        L2Book::new(vec![lvl(bid)], vec![lvl(ask)], venue_ts_ms).unwrap()
    }

    #[test]
    fn age_counts_the_older_stamp() {
        let read = |venue, observed| BookRead {
            book: book(99.0, 101.0, venue),
            observed_at_ms: observed,
        };
        assert_eq!(read(1_000, 1_200).age_ms(1_500), 500, "venue stamp older");
        assert_eq!(read(1_300, 1_200).age_ms(1_500), 300, "venue clock ahead");
        assert_eq!(read(2_000, 2_000).age_ms(1_500), 0, "saturating");
        assert_eq!(read(1_000, 1_200).venue_ts_ms(), 1_000);
    }

    #[tokio::test]
    async fn scripted_books_follow_the_clock() {
        let tsla = InstrumentId::parse("hyperliquid:xyz:TSLA").unwrap();
        let clock = Arc::new(ManualClock::at(1_000));
        let books = ScriptedBooks::new(
            clock.clone(),
            tsla.clone(),
            vec![
                (1_500, book(100.5, 101.5, 1_490)),
                (0, book(99.0, 101.0, 990)),
            ],
        );
        let a = books.fresh_book(&tsla).await.unwrap();
        assert_eq!((a.book.best_ask(), a.observed_at_ms), (Some(101.0), 1_000));
        clock.set(1_500);
        let b = books.fresh_book(&tsla).await.unwrap();
        assert_eq!((b.book.best_ask(), b.venue_ts_ms()), (Some(101.5), 1_490));
        assert_eq!(books.reads(), [1_000, 1_500]);
        let nvda = InstrumentId::parse("hyperliquid:xyz:NVDA").unwrap();
        let e = books.fresh_book(&nvda).await.unwrap_err();
        assert_eq!(e.class, ErrorClass::NotApplicable);
        assert!(e.message.contains("hyperliquid:xyz:NVDA"), "{}", e.message);
        let down = ScriptedBooks::new(clock, tsla.clone(), vec![]).failing(ReadError::new(
            "hl_book",
            ErrorClass::Timeout,
            "slow",
        ));
        assert_eq!(
            down.fresh_book(&tsla).await.unwrap_err().class,
            ErrorClass::Timeout
        );
        assert_eq!(down.reads().len(), 1);
    }
}
