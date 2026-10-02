//! Process-wide named request limiters — one `TokenBucket`
//! (`domain/backoff.rs`) per `[rate_limits.<name>]` (`config/rate_limits.rs`),
//! shared by feeds, `hl-info-client` and `info-fetch` (tracker convention 14).
//!
//! | Call | Rule |
//! |---|---|
//! | [`Limiters::acquire`] | waits (tokio sleep) until the bucket grants `weight`; `Priority::Read` leaves `exec_reserve`, `Priority::Exec` may use it; a wait past `max_wait` ⇒ `RateLimited` (`retry_after_ms` = the wait still needed), a weight above the bucket ⇒ `Fatal` — nothing is sent either way |
//! | `cfg = None` | no `[rate_limits.<name>]`: unlimited, one debug line per name |
//! | [`Limiters::charge`] | a cost known after the reply (HL candles): deducted without waiting (may go into debt) |
//! | [`Limiters::penalize`] | the server answered 429: bucket drained, blocked for `Retry-After` |
//! | Scope | per process: `tengu run` holds every feed and loop; `run-agent` children and the MCP bridge hold their own buckets |
//!
//! Also the IO side of `domain::backoff`: [`mono_ms`] (monotonic clock) and
//! [`jitter01`] (OS randomness).

// Consumers: `outbound/hyperliquid/info.rs` and the feed scheduler
// (`jitter01`, `bootstrap/runtime.rs`) now; `info-fetch` next.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::adapters::outbound::http_class::HttpError;
use crate::config::rate_limits::RateLimitConfig;
use crate::domain::backoff::{Take, TokenBucket};
use crate::domain::observation::ErrorClass;

static START: Lazy<Instant> = Lazy::new(Instant::now);

/// Monotonic ms since the first call in this process.
pub(crate) fn mono_ms() -> i64 {
    START.elapsed().as_millis() as i64
}

/// Uniform in [0, 1) from the OS RNG (0.5 if it fails) — `rand01` of
/// `domain::backoff::next_delay`.
pub(crate) fn jitter01() -> f64 {
    let mut b = [0u8; 8];
    match getrandom::getrandom(&mut b) {
        Ok(()) => (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64,
        Err(_) => 0.5,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    /// Reads and polls: leave `exec_reserve` untouched.
    Read,
    /// Order placement / cancels: may use the whole bucket.
    Exec,
}

struct Entry {
    cfg: RateLimitConfig,
    bucket: TokenBucket,
    blocked_until_ms: i64,
}

impl Entry {
    fn new(cfg: &RateLimitConfig, now: i64) -> Self {
        Self {
            cfg: cfg.clone(),
            bucket: TokenBucket::new(cfg.per_minute, cfg.burst(), now),
            blocked_until_ms: i64::MIN,
        }
    }

    /// A changed section (another sandbox, a reload) re-limits the bucket
    /// and keeps its level.
    fn sync(&mut self, cfg: &RateLimitConfig, now: i64) {
        if &self.cfg != cfg {
            self.bucket.reconfigure(cfg.per_minute, cfg.burst(), now);
            self.cfg = cfg.clone();
        }
    }
}

enum Step {
    Go,
    Wait(u64),
    Refuse(HttpError),
}

/// Named buckets. [`limiters`] is the process-wide instance.
#[derive(Default)]
pub(crate) struct Limiters {
    entries: Mutex<HashMap<String, Entry>>,
    unlimited_logged: Mutex<HashSet<String>>,
}

static LIMITERS: Lazy<Limiters> = Lazy::new(Limiters::default);

/// The process-wide registry every feed and tool shares.
pub(crate) fn limiters() -> &'static Limiters {
    &LIMITERS
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Limiters {
    /// Wait until `weight` is granted from `[rate_limits.<name>]` (see the
    /// module table). `max_wait = 0` never sleeps.
    pub(crate) async fn acquire(
        &self,
        name: &str,
        cfg: Option<&RateLimitConfig>,
        weight: u32,
        priority: Priority,
        max_wait: Duration,
    ) -> Result<(), HttpError> {
        let Some(cfg) = cfg else {
            self.note_unlimited(name);
            return Ok(());
        };
        let started = Instant::now();
        loop {
            match self.step(name, cfg, weight, priority, mono_ms()) {
                Step::Go => return Ok(()),
                Step::Refuse(e) => return Err(e),
                Step::Wait(ms) => {
                    let wait = Duration::from_millis(ms);
                    if started.elapsed() + wait > max_wait {
                        let mut e = HttpError::new(
                            ErrorClass::RateLimited,
                            format!(
                                "[rate_limits.{name}]: weight {weight} needs {ms} ms more \
                                 (max wait {} ms); not sent",
                                max_wait.as_millis()
                            ),
                        );
                        e.retry_after_ms = Some(ms);
                        return Err(e);
                    }
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    fn step(
        &self,
        name: &str,
        cfg: &RateLimitConfig,
        weight: u32,
        priority: Priority,
        now: i64,
    ) -> Step {
        let mut entries = lock(&self.entries);
        let e = entries
            .entry(name.to_string())
            .or_insert_with(|| Entry::new(cfg, now));
        e.sync(cfg, now);
        if now < e.blocked_until_ms {
            return Step::Wait((e.blocked_until_ms - now) as u64);
        }
        let reserve = match priority {
            Priority::Read => cfg.exec_reserve,
            Priority::Exec => 0,
        };
        match e.bucket.try_take(weight, reserve, now) {
            Take::Granted => Step::Go,
            Take::Wait(ms) => Step::Wait(ms.max(1)),
            Take::TooLarge => Step::Refuse(HttpError::new(
                ErrorClass::Fatal,
                format!(
                    "[rate_limits.{name}]: weight {weight} (+ reserve {reserve}) exceeds the bucket ({}); not sent",
                    e.bucket.burst()
                ),
            )),
        }
    }

    /// Deduct `weight` learnt from the reply; no wait, may go into debt.
    pub(crate) fn charge(&self, name: &str, cfg: Option<&RateLimitConfig>, weight: u32) {
        let Some(cfg) = cfg.filter(|_| weight > 0) else {
            return;
        };
        let now = mono_ms();
        let mut entries = lock(&self.entries);
        let e = entries
            .entry(name.to_string())
            .or_insert_with(|| Entry::new(cfg, now));
        e.sync(cfg, now);
        e.bucket.charge(weight, now);
    }

    /// The server answered 429: drain the bucket and hold every caller of
    /// `name` for `retry_after_ms` (when given).
    pub(crate) fn penalize(
        &self,
        name: &str,
        cfg: Option<&RateLimitConfig>,
        retry_after_ms: Option<u64>,
    ) {
        let Some(cfg) = cfg else {
            return;
        };
        let now = mono_ms();
        let mut entries = lock(&self.entries);
        let e = entries
            .entry(name.to_string())
            .or_insert_with(|| Entry::new(cfg, now));
        e.sync(cfg, now);
        e.bucket.drain(now);
        if let Some(ms) = retry_after_ms {
            e.blocked_until_ms = e
                .blocked_until_ms
                .max(now.saturating_add(ms.min(i64::MAX as u64) as i64));
        }
        tracing::warn!(
            limit = name,
            retry_after_ms,
            "rate limited by the server; [rate_limits.{name}] drained"
        );
    }

    fn note_unlimited(&self, name: &str) {
        if lock(&self.unlimited_logged).insert(name.to_string()) {
            tracing::debug!(
                limit = name,
                "no [rate_limits.{name}] section: requests to it are not budgeted"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn tokens(&self, name: &str) -> Option<f64> {
        lock(&self.entries)
            .get_mut(name)
            .map(|e| e.bucket.available(mono_ms()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(per_minute: u32, burst: u32, exec_reserve: u32) -> RateLimitConfig {
        RateLimitConfig {
            per_minute,
            burst: Some(burst),
            exec_reserve,
        }
    }

    const NOW: Duration = Duration::ZERO;

    #[tokio::test]
    async fn unconfigured_names_are_unlimited() {
        let l = Limiters::default();
        for _ in 0..100 {
            l.acquire("hl-free", None, 1_000_000, Priority::Read, NOW)
                .await
                .unwrap();
        }
        assert!(l.tokens("hl-free").is_none(), "no bucket is created");
        l.charge("hl-free", None, 50);
        l.penalize("hl-free", None, Some(1_000));
        assert!(l.tokens("hl-free").is_none());
    }

    #[tokio::test]
    async fn grants_then_fails_fast_past_max_wait() {
        let l = Limiters::default();
        let c = cfg(1, 2, 0); // 1 per minute: timing-proof on a busy host
        for _ in 0..2 {
            l.acquire("t1", Some(&c), 1, Priority::Read, NOW)
                .await
                .unwrap();
        }
        let e = l
            .acquire("t1", Some(&c), 1, Priority::Read, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::RateLimited);
        let ra = e.retry_after_ms.unwrap();
        assert!((50_000..=60_000).contains(&ra), "{ra}");
        assert!(e.message.contains("[rate_limits.t1]"), "{}", e.message);
    }

    #[tokio::test]
    async fn waits_for_the_refill() {
        let l = Limiters::default();
        let c = cfg(6_000, 5, 0); // 1 per 10 ms
        let t = Instant::now();
        // 10 weight from a 5-weight bucket needs ≥ 50 ms of refill, however
        // the scheduler interleaves the calls (−10 ms for ms truncation).
        for _ in 0..2 {
            l.acquire("t2", Some(&c), 5, Priority::Read, Duration::from_secs(5))
                .await
                .unwrap();
        }
        assert!(
            t.elapsed() >= Duration::from_millis(40),
            "{:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn reads_leave_the_exec_reserve_and_big_weights_are_fatal() {
        let l = Limiters::default();
        let c = cfg(1, 10, 5);
        l.acquire("t3", Some(&c), 5, Priority::Read, NOW)
            .await
            .unwrap();
        let e = l
            .acquire("t3", Some(&c), 1, Priority::Read, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::RateLimited);
        l.acquire("t3", Some(&c), 5, Priority::Exec, NOW)
            .await
            .unwrap();
        let e = l
            .acquire("t3", Some(&c), 11, Priority::Exec, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Fatal);
        let e = l
            .acquire("t3", Some(&c), 6, Priority::Read, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Fatal, "6 + reserve 5 > 10");
    }

    #[tokio::test]
    async fn charge_penalize_and_reconfigure() {
        let l = Limiters::default();
        let c = cfg(6, 100, 0); // 1 per 10 s
        l.charge("t4", Some(&c), 30);
        let left = l.tokens("t4").unwrap();
        assert!((70.0..=71.0).contains(&left), "{left}");
        l.penalize("t4", Some(&c), Some(60_000));
        let e = l
            .acquire("t4", Some(&c), 1, Priority::Exec, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::RateLimited);
        assert!(e.retry_after_ms.unwrap() > 59_000, "{e:?}");
        // A new section for the same name re-limits the bucket in place.
        let c2 = cfg(1, 3, 0);
        l.charge("t5", Some(&c), 1);
        l.acquire("t5", Some(&c2), 3, Priority::Read, NOW)
            .await
            .unwrap();
        assert!(l.tokens("t5").unwrap() < 1.0);
    }

    #[test]
    fn process_registry_clock_and_jitter() {
        assert!(std::ptr::eq(limiters(), limiters()));
        let a = mono_ms();
        assert!(mono_ms() >= a);
        for _ in 0..100 {
            let r = jitter01();
            assert!((0.0..1.0).contains(&r), "{r}");
        }
    }
}
