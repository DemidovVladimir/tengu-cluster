//! Shared retry + request-budget policy. Pure: time (`now_ms`, any monotonic
//! origin) and randomness (`rand01`) are inputs; nothing here reads a clock.
//!
//! | `ErrorClass` | [`next_delay`] |
//! |---|---|
//! | `rate_limited` | `Retry(max(Retry-After, jittered exp))`; a wait above `cap_ms` ⇒ `Park` |
//! | `quota_exhausted` | `Park(Retry-After, else quota_park_ms)` |
//! | `transient`, `timeout` | `Retry(jittered exp)` — uniform in `[min_ms, min(cap_ms, base_ms · 2^(attempt−1))]` |
//! | `auth_required`, `fatal`, `decode`, `not_applicable` | `Stop` — the caller reports the source down |
//! | `attempt > max_attempts` (retry classes) | `Stop` |
//!
//! | Type | Role |
//! |---|---|
//! | [`TokenBucket`] | one `[rate_limits.<name>]` budget: refill `per_minute`, capacity `burst`, request weights, an execution reserve reads may not use, post-hoc charges (debt) |
//! | [`CircuitBreaker`] | `threshold` consecutive failures open it for `cooldown_ms` (calls fail fast); then calls pass again — the next failure re-opens at once, a success closes |

// Users: Jev (`outbound/decisions.rs`) and the limiters
// (`outbound/rate_limit.rs`) now; `rt-scheduler` / `rt-health` next.
#![allow(dead_code)]

use crate::domain::observation::ErrorClass;

/// Knobs of [`next_delay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackoffPolicy {
    /// Exponential base: attempt n waits up to `base_ms · 2^(n−1)`.
    pub base_ms: u64,
    /// Longest retry wait; a rate-limit wait above it parks instead.
    pub cap_ms: u64,
    /// Floor of every retry wait (HL WS reconnects: ≥ 2 s).
    pub min_ms: u64,
    /// Retries allowed for the retry classes (`u32::MAX` = unbounded).
    pub max_attempts: u32,
    /// Park when a quota is exhausted and the server gave no `Retry-After`.
    pub quota_park_ms: u64,
}

impl BackoffPolicy {
    /// Feeds and streams: 1 s … 60 s full jitter, unbounded, quota park 1 h.
    pub(crate) const FEED: BackoffPolicy = BackoffPolicy {
        base_ms: 1_000,
        cap_ms: 60_000,
        min_ms: 1_000,
        max_attempts: u32::MAX,
        quota_park_ms: 3_600_000,
    };
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self::FEED
    }
}

/// What to do after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delay {
    /// Sleep `ms`, then try the same call again.
    Retry(u64),
    /// Leave the source alone for `ms` (quota, long rate limit).
    Park(u64),
    /// Do not retry; report the failure.
    Stop,
}

impl Delay {
    pub(crate) fn ms(self) -> Option<u64> {
        match self {
            Delay::Retry(ms) | Delay::Park(ms) => Some(ms),
            Delay::Stop => None,
        }
    }
}

/// Decision after failed attempt number `attempt` (1 = the first call
/// failed). `rand01` ∈ [0, 1] picks the jitter; out-of-range or NaN counts
/// as 0.5.
pub(crate) fn next_delay(
    class: ErrorClass,
    attempt: u32,
    retry_after_ms: Option<u64>,
    policy: &BackoffPolicy,
    rand01: f64,
) -> Delay {
    use ErrorClass::*;
    match class {
        AuthRequired | Fatal | Decode | NotApplicable => return Delay::Stop,
        QuotaExhausted => return Delay::Park(retry_after_ms.unwrap_or(policy.quota_park_ms)),
        RateLimited | Transient | Timeout => {}
    }
    let attempt = attempt.max(1);
    if attempt > policy.max_attempts {
        return Delay::Stop;
    }
    let wait = jittered_exp(attempt, policy, rand01);
    if class != RateLimited {
        return Delay::Retry(wait);
    }
    let wait = wait.max(retry_after_ms.unwrap_or(0));
    if wait > policy.cap_ms {
        Delay::Park(wait)
    } else {
        Delay::Retry(wait)
    }
}

fn jittered_exp(attempt: u32, policy: &BackoffPolicy, rand01: f64) -> u64 {
    let factor = 1u64 << (attempt - 1).min(40);
    let exp = policy
        .base_ms
        .saturating_mul(factor)
        .min(policy.cap_ms)
        .max(policy.min_ms);
    let r = if (0.0..=1.0).contains(&rand01) {
        rand01
    } else {
        0.5
    };
    let lo = policy.min_ms.min(exp);
    lo + ((exp - lo) as f64 * r).round() as u64
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

/// Outcome of [`TokenBucket::try_take`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Take {
    Granted,
    /// Enough tokens accrue in this many ms (assuming no other taker).
    Wait(u64),
    /// `weight + reserve` exceeds the bucket — never grantable.
    TooLarge,
}

/// Weight budget refilled continuously at `per_minute`, holding at most
/// `burst`. Starts full. A post-hoc [`charge`](Self::charge) may push it
/// into debt (down to `−burst`), which later takers wait out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenBucket {
    per_minute: u32,
    burst: u32,
    tokens: f64,
    last_ms: i64,
}

impl TokenBucket {
    /// Full bucket at `now_ms`. `per_minute` / `burst` of 0 count as 1.
    pub(crate) fn new(per_minute: u32, burst: u32, now_ms: i64) -> Self {
        let burst = burst.max(1);
        Self {
            per_minute: per_minute.max(1),
            burst,
            tokens: burst as f64,
            last_ms: now_ms,
        }
    }

    pub(crate) fn per_minute(&self) -> u32 {
        self.per_minute
    }

    pub(crate) fn burst(&self) -> u32 {
        self.burst
    }

    /// A clock that steps back refills nothing.
    fn refill(&mut self, now_ms: i64) {
        if now_ms > self.last_ms {
            let add = (now_ms - self.last_ms) as f64 * self.per_minute as f64 / 60_000.0;
            self.tokens = (self.tokens + add).min(self.burst as f64);
            self.last_ms = now_ms;
        }
    }

    /// Tokens after refilling to `now_ms` (negative = debt).
    pub(crate) fn available(&mut self, now_ms: i64) -> f64 {
        self.refill(now_ms);
        self.tokens
    }

    /// Take `weight`, leaving at least `reserve` tokens behind (reads pass
    /// the execution reserve, execution calls 0).
    pub(crate) fn try_take(&mut self, weight: u32, reserve: u32, now_ms: i64) -> Take {
        self.refill(now_ms);
        let need = weight as f64 + reserve as f64;
        if need > self.burst as f64 {
            return Take::TooLarge;
        }
        if self.tokens >= need {
            self.tokens -= weight as f64;
            return Take::Granted;
        }
        let missing = need - self.tokens;
        Take::Wait((missing * 60_000.0 / self.per_minute as f64).ceil() as u64)
    }

    /// Deduct a cost known only after the reply (HL candles: +1 per 60).
    pub(crate) fn charge(&mut self, weight: u32, now_ms: i64) {
        self.refill(now_ms);
        self.tokens = (self.tokens - weight as f64).max(-(self.burst as f64));
    }

    /// Empty the bucket (the server answered 429); debt is kept.
    pub(crate) fn drain(&mut self, now_ms: i64) {
        self.refill(now_ms);
        self.tokens = self.tokens.min(0.0);
    }

    /// New limits; the current level is kept, clamped to the new `burst`.
    pub(crate) fn reconfigure(&mut self, per_minute: u32, burst: u32, now_ms: i64) {
        self.refill(now_ms);
        self.per_minute = per_minute.max(1);
        self.burst = burst.max(1);
        self.tokens = self.tokens.min(self.burst as f64);
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Opens after `threshold` consecutive failures for `cooldown_ms`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CircuitBreaker {
    threshold: u32,
    cooldown_ms: u64,
    failures: u32,
    open_until_ms: Option<i64>,
}

impl CircuitBreaker {
    pub(crate) fn new(threshold: u32, cooldown_ms: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown_ms,
            failures: 0,
            open_until_ms: None,
        }
    }

    /// `Ok` = call; `Err(ms)` = open, fail fast (ms until it half-opens).
    pub(crate) fn allow(&self, now_ms: i64) -> Result<(), u64> {
        match self.open_until_ms {
            Some(until) if now_ms < until => Err((until - now_ms) as u64),
            _ => Ok(()),
        }
    }

    pub(crate) fn record_success(&mut self) {
        self.failures = 0;
        self.open_until_ms = None;
    }

    pub(crate) fn record_failure(&mut self, now_ms: i64) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.threshold {
            self.open_until_ms = Some(now_ms.saturating_add(self.cooldown_ms as i64));
        }
    }

    /// Consecutive failures so far.
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ErrorClass::*;

    const P: BackoffPolicy = BackoffPolicy {
        base_ms: 1_000,
        cap_ms: 60_000,
        min_ms: 1_000,
        max_attempts: 5,
        quota_park_ms: 3_600_000,
    };

    #[test]
    fn next_delay_by_class() {
        for (class, attempt, ra, r, want) in [
            // Transient / timeout: jitter in [1 s, min(60 s, 2^(n−1) s)].
            (Transient, 1, None, 0.0, Delay::Retry(1_000)),
            (Transient, 1, None, 1.0, Delay::Retry(1_000)),
            (Transient, 2, None, 1.0, Delay::Retry(2_000)),
            (Timeout, 3, None, 0.5, Delay::Retry(2_500)),
            (Transient, 5, None, 1.0, Delay::Retry(16_000)),
            (Transient, 6, None, 0.0, Delay::Stop),
            // Rate limited: Retry-After wins when longer; above the cap ⇒ park.
            (RateLimited, 1, Some(3_000), 0.0, Delay::Retry(3_000)),
            (RateLimited, 3, Some(500), 1.0, Delay::Retry(4_000)),
            (RateLimited, 1, Some(90_000), 0.0, Delay::Park(90_000)),
            (RateLimited, 6, Some(1_000), 0.0, Delay::Stop),
            // Quota: park for Retry-After, else the policy's reset.
            (QuotaExhausted, 1, None, 0.0, Delay::Park(3_600_000)),
            (QuotaExhausted, 9, Some(120_000), 0.0, Delay::Park(120_000)),
            // Never retried.
            (AuthRequired, 1, None, 0.0, Delay::Stop),
            (Fatal, 1, Some(1_000), 0.0, Delay::Stop),
            (Decode, 1, None, 0.0, Delay::Stop),
            (NotApplicable, 1, None, 0.0, Delay::Stop),
        ] {
            assert_eq!(
                next_delay(class, attempt, ra, &P, r),
                want,
                "{class:?} attempt {attempt} ra {ra:?} r {r}"
            );
        }
    }

    #[test]
    fn jitter_is_bounded_and_capped() {
        let unbounded = BackoffPolicy::FEED;
        for attempt in [1, 7, 40, 1_000, u32::MAX] {
            for r in [0.0, 0.3, 1.0, f64::NAN, -1.0, 2.0] {
                let d = next_delay(Transient, attempt, None, &unbounded, r)
                    .ms()
                    .unwrap();
                assert!(
                    (1_000..=60_000).contains(&d),
                    "attempt {attempt} r {r}: {d}"
                );
            }
        }
        assert_eq!(
            next_delay(Transient, 0, None, &P, 1.0),
            Delay::Retry(1_000),
            "attempt 0 counts as the first"
        );
        // A 2 s floor (WS reconnect) holds even at attempt 1.
        let ws = BackoffPolicy { min_ms: 2_000, ..P };
        assert_eq!(
            next_delay(Transient, 1, None, &ws, 0.0),
            Delay::Retry(2_000)
        );
    }

    #[test]
    fn bucket_refills_waits_and_respects_the_reserve() {
        // HL: 1200 / min = 20 per second.
        let mut b = TokenBucket::new(1_200, 1_200, 0);
        assert_eq!(b.try_take(1_000, 0, 0), Take::Granted);
        assert_eq!(b.available(0), 200.0);
        // A read must leave the 200-token execution reserve.
        assert_eq!(b.try_take(20, 200, 0), Take::Wait(1_000));
        assert_eq!(b.try_take(20, 0, 0), Take::Granted, "execution may use it");
        assert_eq!(b.try_take(200, 0, 0), Take::Wait(1_000));
        // After 1 s: +20.
        assert_eq!(b.try_take(200, 0, 1_000), Take::Granted);
        assert_eq!(b.available(1_000), 0.0);
        // Refill caps at the burst; a clock stepping back refills nothing.
        assert_eq!(b.available(3_600_000), 1_200.0);
        assert_eq!(b.available(0), 1_200.0);
        assert_eq!(b.try_take(1_201, 0, 0), Take::TooLarge);
        assert_eq!(b.try_take(1_100, 200, 0), Take::TooLarge);
    }

    #[test]
    fn bucket_charge_drain_and_reconfigure() {
        let mut b = TokenBucket::new(60, 10, 0); // 1 per second
        b.charge(4, 0);
        assert_eq!(b.available(0), 6.0);
        b.charge(100, 0);
        assert_eq!(b.available(0), -10.0, "debt floors at −burst");
        assert_eq!(b.try_take(1, 0, 0), Take::Wait(11_000));
        let mut b = TokenBucket::new(60, 10, 0);
        b.drain(0);
        assert_eq!(b.available(0), 0.0);
        assert_eq!(b.try_take(2, 0, 500), Take::Wait(1_500));
        b.reconfigure(120, 4, 2_000);
        assert_eq!(b.available(2_000), 2.0);
        assert_eq!((b.per_minute(), b.burst()), (120, 4));
        assert_eq!(b.available(10_000), 4.0);
        let zero = TokenBucket::new(0, 0, 0);
        assert_eq!((zero.per_minute(), zero.burst()), (1, 1));
    }

    #[test]
    fn breaker_opens_after_threshold_and_recovers() {
        let mut b = CircuitBreaker::new(3, 30_000);
        assert!(b.allow(0).is_ok());
        b.record_failure(0);
        b.record_failure(1);
        assert!(b.allow(2).is_ok(), "two failures: still closed");
        b.record_failure(10);
        assert_eq!(b.allow(10), Err(30_000));
        assert_eq!(b.allow(20_010), Err(10_000));
        assert!(b.allow(30_010).is_ok(), "half-open after the cooldown");
        b.record_failure(30_010);
        assert_eq!(b.allow(30_010), Err(30_000), "a failed trial re-opens");
        b.record_success();
        assert!(b.allow(30_011).is_ok());
        assert_eq!(b.failures(), 0);
    }
}
