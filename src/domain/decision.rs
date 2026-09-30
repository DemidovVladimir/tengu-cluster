//! Decision-model data: the questions a System One model (TypeSafe Jev via
//! OpenRouter `/api/alpha/decisions`) answers, its typed answers, and the
//! action history a decision loop feeds back into the next call's state.
//!
//! Wire shapes were probed live on 2026-09-24 (`docs/lping-2026-09-24.md`):
//! `choice` takes `criteria` as a `{label: description}` map, `score` takes
//! `criteria` as `[{score, description}]`, `noul` takes instructions only.
//! Answers carry `probabilities` + `confidence` for choice/score.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::observation::ObsMeta;

/// One question in a decisions request. Serialises to the exact wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Question {
    /// Pick one label from `criteria` (label → description).
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// Place the state on a scale anchored by `criteria`.
    Score {
        instructions: String,
        criteria: Vec<ScoreAnchor>,
    },
    /// Probability (0..1) that the answer is "yes".
    Noul { instructions: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ScoreAnchor {
    pub score: f64,
    pub description: String,
}

/// One typed answer. Fields are optional because each question type fills a
/// different subset; `confidence` is absent for `noul`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Answer {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

impl Answer {
    /// Confidence used for gating: explicit `confidence`, else the
    /// probability of the chosen label, else `noul`.
    pub(crate) fn gate_confidence(&self) -> f64 {
        if let Some(c) = self.confidence {
            return c;
        }
        if let Some(label) = &self.choice {
            if let Some(p) = self.probabilities.get(label) {
                return *p;
            }
        }
        self.noul.unwrap_or(0.0)
    }
}

/// Token usage + cost reported by the decisions endpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cost: Option<f64>,
}

/// Full response of one decisions call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Decision {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: DecisionUsage,
}

/// One executed (or skipped) action, fed back as `state.history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    /// Monotonic step counter within the loop's lifetime.
    pub t: u64,
    pub action: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub args: Value,
    /// `true` = succeeded, `false` = failed, `None` = not executed (dry run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// Reduced result (see `application::decision_loop::reduce`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    /// Key / status / source / age / slot of a typed tool result; `None`
    /// for text-only tools, dry runs and terminal actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs: Option<ObsMeta>,
}

/// What the loop did with one decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum StepOutcome {
    /// Tool ran (successfully or not — see the history entry's `ok`).
    Executed { action: String },
    /// Write action chosen while `dry_run = true`; logged, not run.
    DryRun { action: String },
    /// Terminal action (no tool): loop stops for this event.
    Stopped { action: String },
    /// Confidence below `act_at`; handed to the escalation agent.
    Escalated { action: String, confidence: f64 },
    /// A cap or slot check rejected the decision.
    Rejected { action: String, reason: String },
    /// The decisions call itself failed (timeout, 402, 5xx): no action was
    /// chosen. Audited, then the error propagates.
    Error { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn question_serialises_to_wire_shape() {
        let q = Question::Choice {
            instructions: "pick".into(),
            criteria: BTreeMap::from([("hold".into(), "do nothing".into())]),
        };
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            json!({"type": "choice", "instructions": "pick", "criteria": {"hold": "do nothing"}})
        );
        let s = Question::Score {
            instructions: "urgency".into(),
            criteria: vec![ScoreAnchor {
                score: 0.0,
                description: "calm".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"type": "score", "instructions": "urgency", "criteria": [{"score": 0.0, "description": "calm"}]})
        );
    }

    #[test]
    fn decision_parses_probed_response() {
        // Verbatim response captured from the live probe on 2026-09-24.
        let raw = json!({"model":"typesafe/jev-1.13-20260917","answers":{"next_action":{"type":"choice","choice":"rebalance","probabilities":{"close":0.15,"rebalance":0.83,"hold":0.02},"confidence":0.74}},"usage":{"input_tokens":406,"output_tokens":43,"cost":0.000017052},"id":"gen-dec-1790246789-fCdZuJJniKWBOrhedqAW","provider":"TypeSafe"});
        let d: Decision = serde_json::from_value(raw).unwrap();
        let a = &d.answers["next_action"];
        assert_eq!(a.choice.as_deref(), Some("rebalance"));
        assert_eq!(a.gate_confidence(), 0.74);
        assert_eq!(d.usage.input_tokens, 406);
        assert_eq!(d.id, "gen-dec-1790246789-fCdZuJJniKWBOrhedqAW");
    }

    #[test]
    fn gate_confidence_falls_back_to_choice_probability() {
        let a = Answer {
            choice: Some("x".into()),
            probabilities: BTreeMap::from([("x".into(), 0.6)]),
            ..Default::default()
        };
        assert_eq!(a.gate_confidence(), 0.6);
    }
}
