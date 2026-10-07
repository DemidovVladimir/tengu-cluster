//! Decision-model data: the questions a System One model (TypeSafe Jev via
//! OpenRouter `/api/alpha/decisions`) answers, its typed answers, the
//! action history a decision loop feeds back into the next call's state, and
//! the `Verdict` of a terminal-only loop (the backtest gate arm).
//!
//! Wire shapes were probed live on 2026-09-24 (`docs/lping-2026-09-24.md`):
//! `choice` takes `criteria` as a `{label: description}` map, `score` takes
//! `criteria` as `[{score, description}]`, `noul` takes instructions only.
//! Answers carry `probabilities` + `confidence` for choice/score. A
//! `Decision` round-trips through JSON (the replay cache stores it as text).

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
    /// Tool ran and the `[risk]` gate refused the order: its typed result
    /// carries `features.risk = "deny"`; `rule` = `features.risk_rule`.
    /// Counted apart from `Executed` (no output parsing); the loop goes on
    /// as after a failed tool (the history entry has `ok = false`).
    Refused { action: String, rule: String },
    /// Write action chosen while `dry_run = true`; logged, not run. The
    /// loop goes on (a chain is walked without its writes).
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

/// One decision of a terminal-only loop as the backtest gate arm reads it
/// (`DecisionLoop::decide_terminal`, `docs/xlab-2026-10-01.md` § 7).
///
/// | `outcome` | Means | Gate arm |
/// |---|---|---|
/// | `Stopped { action }` | confident pick (`confidence ≥ act_at`) of a terminal action | `take` = trade, else no trade |
/// | `Escalated { action, confidence }` | `below_act_at` — the loop did not act | `unsure` (no trade, counted) |
/// | `Rejected { action, reason }` | no `next_action` answer, or a label that was not offered | no trade, counted |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Verdict {
    /// The chosen action (`next_action.choice`); empty when the answer has none.
    pub action: String,
    /// `Answer::gate_confidence` of `next_action` — what the loop gates on
    /// (explicit `confidence`, else p(choice), else `noul`; 0 when missing).
    pub confidence: f64,
    /// `next_action.probabilities` — p of each offered action as the model
    /// returned it; empty when it sent none.
    pub probabilities: BTreeMap<String, f64>,
    /// `confidence < act_at`.
    pub below_act_at: bool,
    /// What the loop did with the decision (table above).
    pub outcome: StepOutcome,
}

impl Verdict {
    /// The verdict of one step: `next` = the decision's `next_action`
    /// answer (`None` when missing), `outcome` = what the loop did.
    pub(crate) fn of(outcome: StepOutcome, next: Option<&Answer>, act_at: f64) -> Self {
        let confidence = next.map_or(0.0, Answer::gate_confidence);
        Self {
            action: next.and_then(|a| a.choice.clone()).unwrap_or_default(),
            confidence,
            probabilities: next.map(|a| a.probabilities.clone()).unwrap_or_default(),
            below_act_at: confidence < act_at,
            outcome,
        }
    }
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

    /// The replay cache stores decisions as JSON text: every field survives.
    #[test]
    fn decision_round_trips_through_json() {
        let raw = json!({"model":"typesafe/jev-1.13-20260917","answers":{"next_action":{"type":"choice","choice":"rebalance","probabilities":{"close":0.15,"rebalance":0.83,"hold":0.02},"confidence":0.74},"open__size":{"type":"noul","noul":0.31}},"usage":{"input_tokens":406,"output_tokens":43,"cost":0.000017052},"id":"gen-dec-1790246789-fCdZuJJniKWBOrhedqAW"});
        let d: Decision = serde_json::from_value(raw).unwrap();
        let text = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Decision>(&text).unwrap(), d);
        let bare = Decision::default();
        let text = serde_json::to_string(&bare).unwrap();
        assert_eq!(serde_json::from_str::<Decision>(&text).unwrap(), bare);
    }

    #[test]
    fn verdict_reads_the_next_action_answer() {
        let next = Answer {
            kind: "choice".into(),
            choice: Some("take".into()),
            probabilities: BTreeMap::from([
                ("take".into(), 0.62),
                ("skip".into(), 0.30),
                ("ask_architect".into(), 0.08),
            ]),
            confidence: Some(0.55),
            ..Default::default()
        };
        let unsure = StepOutcome::Escalated {
            action: "take".into(),
            confidence: 0.55,
        };
        let v = Verdict::of(unsure.clone(), Some(&next), 0.7);
        assert_eq!(v.action, "take");
        assert_eq!(v.confidence, 0.55);
        assert_eq!(v.probabilities["take"], 0.62);
        assert!(v.below_act_at);
        assert_eq!(v.outcome, unsure);
        assert!(!Verdict::of(unsure, Some(&next), 0.5).below_act_at);

        let none = Verdict::of(
            StepOutcome::Rejected {
                action: "?".into(),
                reason: "decision has no next_action answer".into(),
            },
            None,
            0.7,
        );
        assert_eq!((none.action.as_str(), none.confidence), ("", 0.0));
        assert!(none.probabilities.is_empty() && none.below_act_at);
    }
}
