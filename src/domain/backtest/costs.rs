//! Cost model of a backtest (`docs/xlab-2026-10-01.md` § 6): what one side of
//! a fill costs in bps of its notional, and whether funding is booked. Set
//! per instrument-id prefix in `[backtest.costs."<prefix>"]` (longest prefix
//! wins) and overridable per strategy spec (`costs = { … }`).
//!
//! | Field | Meaning |
//! |---|---|
//! | `taker_fee_bps` | fee per side |
//! | `half_spread` | `{ model = "fixed", bps }` · `{ model = "abdi_ranaldo", window_bars, floor_bps }` (from bars closed before the decision) · `{ model = "ctx", fallback_bps }` (archive impact prices, else the fallback) |
//! | `slippage_bps` | extra per side |
//! | `funding` | book funding over the hold (default true) |

// Consumers land with the xlab wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};

/// What one side of a fill costs (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSpec {
    pub taker_fee_bps: f64,
    #[serde(default)]
    pub half_spread: HalfSpread,
    #[serde(default)]
    pub slippage_bps: f64,
    #[serde(default = "default_true")]
    pub funding: bool,
}

/// How the half-spread paid per side is set (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum HalfSpread {
    Fixed {
        bps: f64,
    },
    /// Abdi & Ranaldo (2017) close-high-low estimator over the last
    /// `window_bars` bars closed before the decision, floored.
    AbdiRanaldo {
        window_bars: u32,
        floor_bps: f64,
    },
    /// `(impact_ask − impact_bid) / 2 / mid` of the latest archive context
    /// at or before the decision; `fallback_bps` without one.
    Ctx {
        fallback_bps: f64,
    },
}

impl Default for HalfSpread {
    fn default() -> Self {
        HalfSpread::Fixed { bps: 0.0 }
    }
}

fn default_true() -> bool {
    true
}

/// Bounds for every bps knob: finite, 0 ≤ x ≤ 10 000.
fn bps_ok(x: f64) -> bool {
    x.is_finite() && (0.0..=10_000.0).contains(&x)
}

impl CostSpec {
    /// Every problem, each naming `what` (the config path or spec field).
    pub fn validation_errors(&self, what: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if !bps_ok(self.taker_fee_bps) {
            errors.push(format!("{what}.taker_fee_bps must be within 0..=10000"));
        }
        if !bps_ok(self.slippage_bps) {
            errors.push(format!("{what}.slippage_bps must be within 0..=10000"));
        }
        match &self.half_spread {
            HalfSpread::Fixed { bps } if !bps_ok(*bps) => {
                errors.push(format!("{what}.half_spread.bps must be within 0..=10000"));
            }
            HalfSpread::AbdiRanaldo {
                window_bars,
                floor_bps,
            } => {
                if !(2..=10_000).contains(window_bars) {
                    errors.push(format!(
                        "{what}.half_spread.window_bars must be within 2..=10000"
                    ));
                }
                if !bps_ok(*floor_bps) {
                    errors.push(format!(
                        "{what}.half_spread.floor_bps must be within 0..=10000"
                    ));
                }
            }
            HalfSpread::Ctx { fallback_bps } if !bps_ok(*fallback_bps) => {
                errors.push(format!(
                    "{what}.half_spread.fallback_bps must be within 0..=10000"
                ));
            }
            _ => {}
        }
        errors
    }
}

/// The cost spec for `instrument` from `[backtest.costs]`: the entry whose
/// key is the longest prefix of the id; `None` when no key matches.
pub fn cost_for<'a>(
    costs: &'a std::collections::BTreeMap<String, CostSpec>,
    instrument: &str,
) -> Option<&'a CostSpec> {
    costs
        .iter()
        .filter(|(prefix, _)| instrument.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn half_spread_models_parse_from_toml() {
        let c: CostSpec = toml::from_str(
            r#"
            taker_fee_bps = 0.9
            half_spread = { model = "abdi_ranaldo", window_bars = 48, floor_bps = 0.5 }
            "#,
        )
        .unwrap();
        assert_eq!(
            c.half_spread,
            HalfSpread::AbdiRanaldo {
                window_bars: 48,
                floor_bps: 0.5
            }
        );
        assert!(c.funding, "funding defaults on");
        assert_eq!(c.slippage_bps, 0.0);
        let fixed: CostSpec = serde_json::from_str(
            r#"{"taker_fee_bps": 4.5, "half_spread": {"model": "fixed", "bps": 1.0}}"#,
        )
        .unwrap();
        assert_eq!(fixed.half_spread, HalfSpread::Fixed { bps: 1.0 });
        assert!(serde_json::from_str::<CostSpec>(r#"{"taker_fee_bps": 1, "fee": 2}"#).is_err());
        assert!(serde_json::from_str::<CostSpec>(
            r#"{"taker_fee_bps": 1, "half_spread": {"model": "magic"}}"#
        )
        .is_err());
    }

    #[test]
    fn bad_knobs_are_named() {
        let c = CostSpec {
            taker_fee_bps: -1.0,
            half_spread: HalfSpread::AbdiRanaldo {
                window_bars: 1,
                floor_bps: f64::NAN,
            },
            slippage_bps: 0.0,
            funding: true,
        };
        let e = c.validation_errors("backtest.costs.\"hyperliquid:\"");
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e[0].starts_with("backtest.costs.\"hyperliquid:\".taker_fee_bps"));
    }

    #[test]
    fn the_longest_prefix_wins() {
        let spec = |fee| CostSpec {
            taker_fee_bps: fee,
            half_spread: HalfSpread::default(),
            slippage_bps: 0.0,
            funding: true,
        };
        let costs = BTreeMap::from([
            ("hyperliquid:".to_string(), spec(4.5)),
            ("hyperliquid:xyz:".to_string(), spec(0.9)),
        ]);
        assert_eq!(
            cost_for(&costs, "hyperliquid:xyz:TSLA")
                .unwrap()
                .taker_fee_bps,
            0.9
        );
        assert_eq!(
            cost_for(&costs, "hyperliquid:SOL").unwrap().taker_fee_bps,
            4.5
        );
        assert!(cost_for(&costs, "solana:So11111111111111111111111111111111111111112").is_none());
    }
}
