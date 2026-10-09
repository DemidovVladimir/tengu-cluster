//! Cycle operations (roadmap O4 "source failures, latency, cost, tokens and
//! manual correction time measured"): what one weekly cycle cost to run.
//! Pure; the stage runner reports each stage, the cycle writes `ops.json`.
//! The operator's correction minutes come from the grade (`review.rs`).
//!
//! | `CycleOps` | Value |
//! |---|---|
//! | `stages[]` | [`StageRun`]: [`Stage`] (`OBSERVE` `ARCHITECT` `CHALLENGE` `ALLOCATE` `REPORT`), `agent?`, `latency_ms`, `prompt_tokens`, `completion_tokens`, `ok`, `tokens_unknown` (the stage ended with no reply — a crash, a timeout: its tokens are not 0, they are unknown) |
//! | `source_failures[]` | the sources whose fetch failed for this cycle (ids) |
//! | totals | latency, prompt / completion tokens (of the stages that reported them), failed stages, `tokens_unknown` (the stages whose tokens are unknown) |
//! | `cost` | [`Cost`]: `KNOWN` = Σ tokens × the per-million price, each part ceiled (an outflow) · `UNKNOWN` with why — no price configured, or a stage's tokens unknown, is never a cost of 0 |

// Consumers land with the cycle and the review packet.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::value::{Currency, Flow, Minor, ValueError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Stage {
    Observe,
    Architect,
    Challenge,
    Allocate,
    Report,
}

/// One stage of one cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageRun {
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub ok: bool,
    /// No reply came back (the runner failed: a crash, a timeout): the
    /// stage's tokens are unknown — never read as 0 (module table).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tokens_unknown: bool,
}

/// Token prices (from config; none set ⇒ the cost is unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPrices {
    pub currency: Currency,
    pub prompt_per_million: Minor,
    pub completion_per_million: Minor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Cost {
    Known { amount: Minor, currency: Currency },
    Unknown { reason: String },
}

/// Module table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CycleOps {
    pub cycle_id: String,
    pub stages: Vec<StageRun>,
    pub source_failures: Vec<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub failed_stages: usize,
    /// The stages whose tokens are unknown ([`StageRun::tokens_unknown`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens_unknown: Vec<Stage>,
    pub cost: Cost,
}

const MILLION: i64 = 1_000_000;

/// `tokens` × a per-million price, ceiled.
fn priced(price: Minor, tokens: u64) -> Result<Minor, ValueError> {
    let n = i64::try_from(tokens)
        .map_err(|_| ValueError::new(super::value::codes::OVERFLOW, format!("{tokens} tokens")))?;
    price.mul_div(n, MILLION, Flow::Outflow)
}

/// Module table: the ops record of one cycle (source failures sorted, once each).
pub fn cycle_ops(
    cycle_id: &str,
    stages: Vec<StageRun>,
    mut source_failures: Vec<String>,
    prices: Option<&TokenPrices>,
) -> Result<CycleOps, ValueError> {
    source_failures.sort();
    source_failures.dedup();
    let sum = |f: fn(&StageRun) -> u64| stages.iter().map(f).fold(0u64, u64::saturating_add);
    let (prompt, completion) = (sum(|s| s.prompt_tokens), sum(|s| s.completion_tokens));
    let unknown: Vec<Stage> = stages
        .iter()
        .filter(|s| s.tokens_unknown)
        .map(|s| s.stage)
        .collect();
    let cost = match prices {
        _ if !unknown.is_empty() => Cost::Unknown {
            reason: format!(
                "tokens unknown: {} ended with no reply",
                unknown
                    .iter()
                    .map(|s| format!("{s:?}").to_uppercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        Some(p) => Cost::Known {
            amount: priced(p.prompt_per_million, prompt)?
                .checked_add(priced(p.completion_per_million, completion)?)?,
            currency: p.currency,
        },
        None => Cost::Unknown {
            reason: "no token prices configured".into(),
        },
    };
    Ok(CycleOps {
        cycle_id: cycle_id.to_string(),
        latency_ms: sum(|s| s.latency_ms),
        prompt_tokens: prompt,
        completion_tokens: completion,
        failed_stages: stages.iter().filter(|s| !s.ok).count(),
        tokens_unknown: unknown,
        stages,
        source_failures,
        cost,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn run(
        stage: Stage,
        latency: u64,
        prompt: u64,
        completion: u64,
        ok: bool,
    ) -> StageRun {
        StageRun {
            stage,
            agent: Some(format!("{stage:?}").to_lowercase()),
            latency_ms: latency,
            prompt_tokens: prompt,
            completion_tokens: completion,
            ok,
            tokens_unknown: false,
        }
    }

    pub(crate) fn stages() -> Vec<StageRun> {
        vec![
            run(Stage::Observe, 900, 0, 0, true),
            run(Stage::Architect, 41_000, 18_000, 3_100, true),
            run(Stage::Challenge, 23_000, 12_500, 1_400, false),
            run(Stage::Allocate, 40, 0, 0, true),
        ]
    }

    #[test]
    fn cost_unknown_without_prices() {
        let o = cycle_ops(
            "2026-W41",
            stages(),
            vec!["ted_search".into(), "sec_edgar".into(), "ted_search".into()],
            None,
        )
        .unwrap();
        assert_eq!(
            (
                o.latency_ms,
                o.prompt_tokens,
                o.completion_tokens,
                o.failed_stages
            ),
            (64_940, 30_500, 4_500, 1)
        );
        assert_eq!(o.source_failures, ["sec_edgar", "ted_search"]);
        assert_eq!(
            o.cost,
            Cost::Unknown {
                reason: "no token prices configured".into()
            }
        );
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["cost"]["kind"], "UNKNOWN");
    }

    #[test]
    fn cost_sums_tokens_and_rounds_up() {
        let prices = TokenPrices {
            currency: Currency::Usd,
            prompt_per_million: "3.00".parse().unwrap(),
            completion_per_million: "15.00".parse().unwrap(),
        };
        let o = cycle_ops("2026-W41", stages(), vec![], Some(&prices)).unwrap();
        // 30 500 × 3.00 / 10⁶ = 0.0915 → 0.10; 4 500 × 15.00 / 10⁶ = 0.0675 → 0.07.
        assert_eq!(
            o.cost,
            Cost::Known {
                amount: "0.17".parse().unwrap(),
                currency: Currency::Usd
            }
        );
        let back: CycleOps = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
        assert_eq!(back, o);
        assert!(serde_json::to_value(&o)
            .unwrap()
            .get("tokens_unknown")
            .is_none());
    }

    /// A stage that ended with no reply (crash, timeout) reported no
    /// tokens: they are unknown, not 0 — the cost is `UNKNOWN` even with
    /// prices, and the totals name the stage (roadmap § 13 no silent unknown).
    #[test]
    fn stage_without_reply_makes_cost_unknown() {
        let prices = TokenPrices {
            currency: Currency::Usd,
            prompt_per_million: "3.00".parse().unwrap(),
            completion_per_million: "15.00".parse().unwrap(),
        };
        let mut s = stages();
        s[2] = StageRun {
            prompt_tokens: 0,
            completion_tokens: 0,
            tokens_unknown: true,
            ..s[2].clone()
        };
        let o = cycle_ops("2026-W41", s, vec![], Some(&prices)).unwrap();
        assert_eq!(o.tokens_unknown, [Stage::Challenge]);
        assert_eq!(
            o.cost,
            Cost::Unknown {
                reason: "tokens unknown: CHALLENGE ended with no reply".into()
            }
        );
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["stages"][2]["tokens_unknown"], true);
        assert_eq!(j["tokens_unknown"], serde_json::json!(["CHALLENGE"]));
        let back: CycleOps = serde_json::from_value(j).unwrap();
        assert_eq!(back, o);
    }
}
