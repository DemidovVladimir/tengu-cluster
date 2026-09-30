//! `SystemClock` — the real `ports::clock::Clock`: wall time from the OS,
//! sleeping on the tokio timer. Used by the `[feeds]` scheduler
//! (`bootstrap/runtime.rs`), which naps ≤ 60 s at a time so a wall-clock
//! jump (NTP step, system sleep) is noticed within a minute.

use async_trait::async_trait;

use crate::domain::observation::now_ms;
use crate::ports::clock::Clock;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        now_ms()
    }

    async fn sleep_until_ms(&self, t_ms: i64) {
        let wait = t_ms.saturating_sub(now_ms());
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleeps_until_the_instant() {
        let c = SystemClock;
        let start = c.now_ms();
        c.sleep_ms(20).await;
        assert!(c.now_ms() >= start + 20);
        c.sleep_until_ms(start).await; // past: returns at once
    }
}
