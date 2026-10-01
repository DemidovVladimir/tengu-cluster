//! `[rate_limits.<name>]` — request budgets shared process-wide by every feed
//! and tool that names them (`outbound/rate_limit.rs`, tracker convention
//! 14). Weights are the venue's units: Hyperliquid allows 1200 weight / min
//! per IP (`l2Book` 2, most info requests 20). `deny_unknown_fields`.
//!
//! ```toml
//! [rate_limits.hyperliquid]
//! per_minute = 1200
//! # burst = 1200        # bucket size, default = per_minute
//! # exec_reserve = 200  # weight reads leave for order calls, default 0
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One named budget (a token bucket).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Refill rate, weight units per minute.
    pub per_minute: u32,
    /// Bucket size — the largest burst. Default = `per_minute`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    /// Weight kept for execution calls; reads leave it untouched. Default 0.
    #[serde(default)]
    pub exec_reserve: u32,
}

impl RateLimitConfig {
    pub fn burst(&self) -> u32 {
        self.burst.unwrap_or(self.per_minute)
    }
}

/// Problems in the `[rate_limits]` table (names sorted for stable output).
pub fn validation_errors(limits: &HashMap<String, RateLimitConfig>) -> Vec<String> {
    let mut names: Vec<&String> = limits.keys().collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let l = &limits[name];
        let at = format!("rate_limits.{name}");
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        {
            out.push(format!("{at}: name must be [a-z0-9_-]+"));
        }
        if l.per_minute == 0 {
            out.push(format!("{at}.per_minute must be at least 1"));
        }
        if l.burst == Some(0) {
            out.push(format!("{at}.burst must be at least 1"));
        }
        if l.exec_reserve >= l.burst().max(1) {
            out.push(format!(
                "{at}.exec_reserve ({}) must be below the burst ({})",
                l.exec_reserve,
                l.burst()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(toml_str: &str) -> HashMap<String, RateLimitConfig> {
        #[derive(Deserialize)]
        struct T {
            rate_limits: HashMap<String, RateLimitConfig>,
        }
        toml::from_str::<T>(toml_str).unwrap().rate_limits
    }

    #[test]
    fn parses_defaults_and_validates() {
        let t = table("[rate_limits.hyperliquid]\nper_minute = 1200\n");
        let hl = &t["hyperliquid"];
        assert_eq!(
            (hl.per_minute, hl.burst(), hl.exec_reserve),
            (1_200, 1_200, 0)
        );
        assert!(validation_errors(&t).is_empty());

        let t = table(
            "[rate_limits.sec-gov]\nper_minute = 480\nburst = 8\nexec_reserve = 8\n\
             [rate_limits.Bad]\nper_minute = 0\n",
        );
        let errs = validation_errors(&t);
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs[0].starts_with("rate_limits.Bad: name"), "{errs:?}");
        assert!(errs[1].contains("per_minute must be at least 1"));
        assert!(
            errs[2].contains("rate_limits.sec-gov.exec_reserve (8) must be below the burst (8)")
        );
    }

    #[test]
    fn budgets_reach_every_agent_through_the_sandbox_sections() {
        let mut c: crate::config::Config = toml::from_str(
            r#"
            [rate_limits.hyperliquid]
            per_minute = 1200

            [agents.main]
            default = true
            engine = "openrouter"
            model = "m"
        "#,
        )
        .unwrap();
        c.validate().unwrap();
        c.fold_default_scopes();
        assert_eq!(
            c.agents["main"].sandbox.rate_limits["hyperliquid"].per_minute,
            1_200
        );
        let bad: crate::config::Config = toml::from_str(
            "[rate_limits.hl]\nper_minute = 0\n[agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n",
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    /// HL's 1200 weight / min per IP is shared by the sandboxes one host runs
    /// (review finding: xlab's budget was the whole 1200 — its 1h backfill
    /// held ≈ 1 243 / min for 27 min, so the weekend run's book reads would
    /// get 429s). The worst 60 s window of xlab's bucket (burst +
    /// per_minute) leaves room for xmarket-weekend's measured peak and
    /// xmarket's steady use (`docs/runtime-2026-09-30.md`).
    #[test]
    fn the_xlab_hl_budget_leaves_room_for_the_xmarket_runs() {
        const HL_PER_IP: u32 = 1_200;
        const WEEKEND_PEAK: u32 = 190;
        const XMARKET_STEADY: u32 = 110;
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/xlab/config.toml");
        let c = crate::config::Config::load(&path).unwrap();
        let hl = &c.rate_limits["hyperliquid"];
        let window = hl.burst() + hl.per_minute;
        assert!(
            window + WEEKEND_PEAK + XMARKET_STEADY <= HL_PER_IP,
            "xlab's worst minute {window} + weekend {WEEKEND_PEAK} + xmarket {XMARKET_STEADY} > \
             {HL_PER_IP}"
        );
        // A page's base weight still fits the bucket (candles, funding: 20).
        assert!(hl.burst() >= 20);
    }

    #[test]
    fn rejects_unknown_keys() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct T {
            rate_limits: HashMap<String, RateLimitConfig>,
        }
        let e = toml::from_str::<T>("[rate_limits.hyperliquid]\nper_minut = 1200\n").unwrap_err();
        assert!(e.to_string().contains("per_minut"), "{e}");
    }
}
