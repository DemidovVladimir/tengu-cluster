//! Execution map — a higher-order agent's (the Architect's) run of one
//! `[decision_loops.<name>]` loop, as JSON data, never code. A map can only
//! NARROW the loop the sandbox TOML defines: it never reaches a tool,
//! argument, wallet or `mode` the TOML does not already name, so the
//! sandbox's scopes, signer rules and caps stay the ceiling. Run one with
//! `tengu decide --map <file|->`; skill `skills/execution-map/SKILL.md`
//! teaches the format.
//!
//! ```json
//! {"loop": "lp_exec",
//!  "goal": "Open 0.5 SOL + 40 USDC; skip if the plan is blocked",
//!  "sequence": ["snapshot", "plan_swap", "swap?", "refresh", "open"],
//!  "event": {"target_sol": 0.5, "target_usdc": 40},
//!  "caps": {"plan_swap": {"target_sol": 1.0}},
//!  "max_steps": 6}
//! ```
//!
//! | Field | Rule |
//! |---|---|
//! | `loop` | the base loop (required) |
//! | `actions` | a subset of the base actions; terminal actions are always kept |
//! | `sequence` | step order (`"a"`, `"a?"` optional) over the kept actions — replaces the base `sequence` |
//! | `event` | the event the loop runs on — event-bound slots read it; a JSON object, ≤ 16 KiB canonical |
//! | `goal` | appended to the base goal as `Task (architect): …` (≤ 2 000 chars); the base goal and its limits stay |
//! | `caps` | `{action: {slot: max}}` — a slot of that action; never above the base cap |
//! | `max_steps` | ≤ the base |
//! | `act_at` | ≥ the base, ≤ 1 |
//! | `dry_run` | may turn dry-run on, never off |
//!
//! Not settable: tools, args, slot sources, `world`, `requires`, `reduce`,
//! the agent, the model. The narrowed loop must pass the same
//! `DecisionLoopConfig::validation_errors` as the TOML one; a map is refused
//! with every problem named. Its identity is the sha256 of its canonical
//! JSON (`domain::canonical`), carried as `trigger = "map:<sha256>"` on
//! every audit line of the run.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::decision_loop::{DecisionLoopConfig, SeqStep};
use crate::domain::canonical::{canonical_json, canonical_sha256};

/// Max canonical size of `event`.
pub(crate) const MAX_EVENT_BYTES: usize = 16 * 1024;
/// Max chars of `goal`.
pub(crate) const MAX_GOAL_CHARS: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionMap {
    #[serde(rename = "loop")]
    pub loop_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<SeqStep>>,
    #[serde(default = "empty_object")]
    pub event: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub caps: BTreeMap<String, BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

impl ExecutionMap {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("execution map: {e}"))
    }

    /// sha256 of the map's canonical JSON — 64 hex chars, never shortened.
    pub(crate) fn sha256(&self) -> String {
        canonical_sha256(&serde_json::to_value(self).unwrap_or(Value::Null))
    }

    /// The canonical JSON text the hash is over (what `tengu decide` keeps).
    pub(crate) fn canonical(&self) -> String {
        canonical_json(&serde_json::to_value(self).unwrap_or(Value::Null))
    }

    /// The base loop narrowed by this map, or every reason it is refused.
    pub(crate) fn apply(
        &self,
        base: &DecisionLoopConfig,
    ) -> Result<DecisionLoopConfig, Vec<String>> {
        let mut errs = Vec::new();
        let mut cfg = base.clone();
        let p = "execution map";

        if let Some(keep) = &self.actions {
            for a in keep {
                if !base.actions.contains_key(a) {
                    errs.push(format!(
                        "{p}.actions: `{a}` is not an action of loop `{}`",
                        self.loop_name
                    ));
                }
            }
            cfg.actions
                .retain(|name, a| a.tool.is_none() || keep.contains(name));
        }
        if let Some(seq) = &self.sequence {
            cfg.sequence = seq.clone();
        }
        if !self.event.is_object() {
            errs.push(format!("{p}.event must be a JSON object"));
        } else if canonical_json(&self.event).len() > MAX_EVENT_BYTES {
            errs.push(format!("{p}.event is larger than {MAX_EVENT_BYTES} bytes"));
        }
        if let Some(goal) = &self.goal {
            if goal.trim().is_empty() {
                errs.push(format!(
                    "{p}.goal is empty — omit it to keep the loop's goal"
                ));
            } else if goal.chars().count() > MAX_GOAL_CHARS {
                errs.push(format!("{p}.goal is longer than {MAX_GOAL_CHARS} chars"));
            } else {
                cfg.goal = format!(
                    "{}\n\nTask (architect): {}",
                    base.goal.trim_end(),
                    goal.trim()
                );
            }
        }
        for (an, slots) in &self.caps {
            let Some(action) = cfg.actions.get_mut(an) else {
                errs.push(format!("{p}.caps.{an}: not an action of this run"));
                continue;
            };
            for (sn, &cap) in slots {
                if !action.slots.contains_key(sn) {
                    errs.push(format!("{p}.caps.{an}.{sn}: `{an}` has no slot `{sn}`"));
                } else if !cap.is_finite() {
                    errs.push(format!("{p}.caps.{an}.{sn} must be finite"));
                } else if let Some(&b) = action.caps.get(sn).filter(|&&b| cap > b) {
                    errs.push(format!(
                        "{p}.caps.{an}.{sn} = {cap} is above the loop's cap {b} — a map may only tighten"
                    ));
                } else {
                    action.caps.insert(sn.clone(), cap);
                }
            }
        }
        if let Some(m) = self.max_steps {
            if m > base.max_steps {
                errs.push(format!(
                    "{p}.max_steps = {m} is above the loop's {} — a map may only lower it",
                    base.max_steps
                ));
            } else {
                cfg.max_steps = m;
            }
        }
        if let Some(x) = self.act_at {
            if !(x >= base.act_at && x <= 1.0) {
                errs.push(format!(
                    "{p}.act_at = {x} must be in [{}, 1] — a map may only raise it",
                    base.act_at
                ));
            } else {
                cfg.act_at = x;
            }
        }
        match self.dry_run {
            Some(false) if base.dry_run => errs.push(format!(
                "{p}.dry_run = false: the loop is dry-run in the sandbox config — a map may only turn dry-run on"
            )),
            Some(d) => cfg.dry_run = d,
            None => {}
        }
        errs.extend(cfg.validation_errors(&self.loop_name));
        if errs.is_empty() {
            Ok(cfg)
        } else {
            Err(errs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = r#"
goal = "Open what the event asks for; max 2 SOL"
agent = "a"
max_steps = 6
[actions.hold]
description = "nothing"
[actions.snapshot]
description = "read"
tool = "lp_snapshot"
read_only = true
[actions.plan]
description = "plan"
tool = "lp_swap_plan"
read_only = true
slots = { target = { event = "/target_sol" } }
caps = { target = 2.0 }
[actions.swap]
description = "swap"
tool = "jupiter_swap"
slots = { amount = { from = "plan", path = "/swaps/0/amount" } }
[actions.open]
description = "open"
tool = "dlmm_open_position"
slots = { x = { event = "/target_sol" } }
"#;

    fn base() -> DecisionLoopConfig {
        let c: DecisionLoopConfig = toml::from_str(BASE).unwrap();
        assert!(c.validation_errors("lp").is_empty());
        c
    }

    fn map(v: Value) -> ExecutionMap {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn narrows_order_event_caps_and_goal() {
        let m = map(json!({
            "loop": "lp",
            "goal": "half size only",
            "sequence": ["snapshot", "plan", "swap?", "open"],
            "event": {"target_sol": 0.5},
            "caps": {"plan": {"target": 1.0}},
            "max_steps": 5,
            "act_at": 0.9,
            "dry_run": true
        }));
        let c = m.apply(&base()).unwrap();
        assert_eq!(
            c.sequence
                .iter()
                .map(|s| String::from(s.clone()))
                .collect::<Vec<_>>(),
            ["snapshot", "plan", "swap?", "open"]
        );
        assert!(c
            .goal
            .starts_with("Open what the event asks for; max 2 SOL"));
        assert!(c.goal.ends_with("Task (architect): half size only"));
        assert_eq!(c.actions["plan"].caps["target"], 1.0);
        assert_eq!((c.max_steps, c.act_at, c.dry_run), (5, 0.9, true));
        // Everything else is the TOML's.
        assert_eq!(c.actions["swap"].tool.as_deref(), Some("jupiter_swap"));
    }

    #[test]
    fn a_subset_keeps_terminal_actions() {
        let c = map(json!({"loop": "lp", "actions": ["snapshot"]}))
            .apply(&base())
            .unwrap();
        let names: Vec<_> = c.actions.keys().map(String::as_str).collect();
        assert_eq!(names, ["hold", "snapshot"]);
    }

    #[test]
    fn widening_or_unknown_names_are_refused_with_every_reason() {
        let mut b = base();
        b.dry_run = true;
        let e = map(json!({
            "loop": "lp",
            "actions": ["snapshot", "swap", "send_everything"],
            "sequence": ["snapshot", "hold"],
            "caps": {"plan": {"target": 5.0}, "open": {"nope": 1.0}},
            "max_steps": 9,
            "act_at": 0.5,
            "dry_run": false,
            "event": [1, 2],
            "goal": " "
        }))
        .apply(&b)
        .unwrap_err()
        .join("\n");
        for want in [
            "`send_everything` is not an action",
            "caps.plan: not an action of this run", // dropped by `actions`
            "caps.open: not an action of this run",
            "max_steps = 9 is above",
            "act_at = 0.5 must be in [0.8, 1]",
            "may only turn dry-run on",
            "event must be a JSON object",
            "goal is empty",
            "`hold` is terminal",
            // `swap` reads `plan`, which the subset dropped.
            "`from = \"plan\"` is not an action",
        ] {
            assert!(e.contains(want), "missing `{want}` in:\n{e}");
        }
        let e = map(json!({"loop": "lp", "caps": {"plan": {"target": 2.5}}}))
            .apply(&base())
            .unwrap_err()
            .join("\n");
        assert!(e.contains("above the loop's cap 2"), "{e}");
    }

    #[test]
    fn unknown_keys_and_bad_steps_fail_to_parse() {
        assert!(ExecutionMap::parse(r#"{"loop": "lp", "tools": ["shell"]}"#).is_err());
        assert!(ExecutionMap::parse(r#"{"loop": "lp", "sequence": ["a??"]}"#).is_err());
        assert!(ExecutionMap::parse(r#"{"loop": "lp", "sequence": ["?"]}"#).is_err());
        assert!(
            ExecutionMap::parse(r#"{"sequence": []}"#).is_err(),
            "loop is required"
        );
    }

    #[test]
    fn identity_is_the_canonical_sha256() {
        let a = ExecutionMap::parse(r#"{"loop":"lp","event":{"b":1,"a":2}}"#).unwrap();
        let b = ExecutionMap::parse(r#"{"event":{"a":2,"b":1},"loop":"lp"}"#).unwrap();
        assert_eq!(a.sha256(), b.sha256());
        assert_eq!(a.sha256().len(), 64);
        assert_eq!(a.canonical(), r#"{"event":{"a":2,"b":1},"loop":"lp"}"#);
    }

    #[test]
    fn a_short_max_steps_for_the_sequence_is_refused() {
        let e =
            map(json!({"loop": "lp", "sequence": ["snapshot", "plan", "open"], "max_steps": 3}))
                .apply(&base())
                .unwrap_err()
                .join("\n");
        assert!(e.contains("must exceed the sequence length (3)"), "{e}");
    }
}
