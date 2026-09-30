//! `tengu run` — pure data of the long-running process: the single-runner
//! lease (one runner per sandbox). No IO; `now_ms` is always an input.
//! Composition `bootstrap/runtime.rs`; operator doc `docs/runtime-2026-09-30.md`.

use serde::{Deserialize, Serialize};

/// Lease TTL: a crashed runner's lease frees after this.
pub const LEASE_TTL_MS: i64 = 30_000;
/// Renewal period of a live runner (a third of the TTL).
pub const LEASE_RENEW_MS: u64 = 10_000;

/// `runtime:<sandbox>` — the lease one `tengu run` of a sandbox holds.
pub fn lease_resource(sandbox: &str) -> String {
    format!("runtime:{sandbox}")
}

/// One acquire / renew of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerLease {
    pub resource: String,
    pub holder: String,
    pub granted: bool,
    /// Who holds it now (`holder` when granted).
    pub current_holder: String,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
}

impl RunnerLease {
    /// Whole seconds until expiry at `now_ms` (0 once expired).
    pub fn remaining_secs(&self, now_ms: i64) -> u64 {
        (self.expires_at_ms.saturating_sub(now_ms).max(0) as u64).div_ceil(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_keeps_the_full_sandbox_name() {
        assert_eq!(lease_resource("xmarket-weekend"), "runtime:xmarket-weekend");
    }

    #[test]
    fn remaining_secs_rounds_up_and_saturates() {
        let l = RunnerLease {
            resource: lease_resource("s"),
            holder: "a".into(),
            granted: true,
            current_holder: "a".into(),
            acquired_at_ms: 0,
            expires_at_ms: 30_000,
        };
        assert_eq!(l.remaining_secs(0), 30);
        assert_eq!(l.remaining_secs(29_001), 1);
        assert_eq!(l.remaining_secs(30_000), 0);
        assert_eq!(l.remaining_secs(99_000), 0);
    }
}
