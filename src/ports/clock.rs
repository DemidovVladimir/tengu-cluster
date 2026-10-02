//! `Clock` — wall time and sleeping behind a port, so feeds, the paper fill
//! engine's latency and replay (tracker convention 16) run on a fake clock
//! in tests and on recorded time in `tengu xm replay`. Impl: `SystemClock`
//! (`adapters/outbound/clock.rs`); test double `ManualClock` below.

use async_trait::async_trait;

#[async_trait]
pub(crate) trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
    /// Return once `now_ms() >= t_ms` (immediately when already past).
    async fn sleep_until_ms(&self, t_ms: i64);
    /// `sleep_until_ms(now_ms() + ms)`.
    #[cfg_attr(not(test), allow(dead_code))] // feeds and fills sleep until instants
    async fn sleep_ms(&self, ms: u64) {
        let t = self.now_ms().saturating_add(ms.min(i64::MAX as u64) as i64);
        self.sleep_until_ms(t).await;
    }
}

/// Test double: time moves only when a test calls `advance` / `set`, or when
/// a sleeper asks for a future instant (the clock jumps there) — so a feed
/// schedule or a fill latency runs instantly and deterministically.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct ManualClock {
    now: std::sync::atomic::AtomicI64,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn at(now_ms: i64) -> Self {
        Self {
            now: std::sync::atomic::AtomicI64::new(now_ms),
        }
    }
    pub(crate) fn set(&self, now_ms: i64) {
        self.now.store(now_ms, std::sync::atomic::Ordering::SeqCst);
    }
    pub(crate) fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
#[async_trait]
impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn sleep_until_ms(&self, t_ms: i64) {
        self.now
            .fetch_max(t_ms, std::sync::atomic::Ordering::SeqCst);
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manual_clock_jumps_to_the_requested_instant() {
        let c = ManualClock::at(1_000);
        c.sleep_ms(250).await;
        assert_eq!(c.now_ms(), 1_250);
        c.sleep_until_ms(900).await; // past: no change
        assert_eq!(c.now_ms(), 1_250);
        c.advance(50);
        assert_eq!(c.now_ms(), 1_300);
        c.set(5);
        assert_eq!(c.now_ms(), 5);
    }
}
