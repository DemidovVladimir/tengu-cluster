//! `Clock` — wall time and sleeping behind a port, so feeds, the paper fill
//! engine's latency and replay (tracker convention 16) run on a fake clock
//! in tests and on simulated time in replay.
//!
//! | Impl | Time |
//! |---|---|
//! | `SystemClock` (`adapters/outbound/clock.rs`) | the OS wall clock, tokio sleep |
//! | [`SimClock`] | settable: moves only on `set` / `advance` or when a sleeper asks for a later instant — the decision loop's replay clock (`tengu backtest --gate`, `docs/xlab-2026-10-01.md` § 7) and the test double (`ManualClock`) |

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

/// Simulated time: moves only when `set` / `advance` is called, or when a
/// sleeper asks for a future instant (the clock jumps there) — a replay
/// sets it to each decision instant; a feed schedule or a fill latency runs
/// instantly and deterministically. Never moves on its own.
#[derive(Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))] // the backtest gate arm drives it (xlab)
pub(crate) struct SimClock {
    now: std::sync::atomic::AtomicI64,
}

/// The tests' name of [`SimClock`].
#[cfg(test)]
pub(crate) type ManualClock = SimClock;

#[cfg_attr(not(test), allow(dead_code))] // the backtest gate arm drives it (xlab)
impl SimClock {
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

#[async_trait]
impl Clock for SimClock {
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

    /// Behind `Arc<dyn Clock>` (how the decision loop holds it), a `set` on
    /// the owner's handle is what every reader sees.
    #[test]
    fn sim_clock_is_shared_through_dyn_clock() {
        let sim = std::sync::Arc::new(SimClock::at(1_790_000_000_000));
        let seen: std::sync::Arc<dyn Clock> = sim.clone();
        assert_eq!(seen.now_ms(), 1_790_000_000_000);
        sim.set(1_790_000_020_000);
        assert_eq!(seen.now_ms(), 1_790_000_020_000);
    }
}
