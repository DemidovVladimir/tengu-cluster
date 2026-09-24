//! `[decision_loops.<name>]` — a System One (Jev) control loop over existing
//! tools. Behaviour lives here in TOML; the loop itself is
//! `application::decision_loop`. Design: `docs/decision-loop-plan-2026-09-24.md`.
//!
//! ```toml
//! [decision_loops.lp_watch]
//! goal    = "Keep the SOL/USDC LP position in range; max 2 SOL"
//! agent   = "crypto_researcher"      # tools, scopes, workspace come from this agent
//! dry_run = true                     # write actions are logged, not run
//!
//! [decision_loops.lp_watch.actions.hold]
//! description = "Nothing to do"      # no `tool` = terminal action
//!
//! [decision_loops.lp_watch.actions.fetch_pools]
//! description = "Fetch SOL/USDC pools"
//! tool        = "http_request"
//! read_only   = true
//! args        = { method = "GET", url = "https://example/pools?pair={pair}", return_body = true }
//! slots       = { pair = ["SOL-USDC"] }
//! reduce      = { pools = "/data/*/{address,fees_24h}" }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionLoopConfig {
    /// Decisions-endpoint model slug. Default `~typesafe/jev-latest`.
    #[serde(default = "default_model")]
    pub model: String,
    /// Objective + hard limits, sent as `state.goal` on every call.
    pub goal: String,
    /// `[agents.<agent>]` whose tools, scopes and workspace the loop uses.
    /// Every action `tool` must be callable by this agent.
    pub agent: String,
    /// Ring-buffer size of `state.history`. Default 8.
    #[serde(default = "default_history")]
    pub history: usize,
    /// Minimum confidence (next action AND each of its slots) to act.
    /// Below it the step escalates (or stops, if `escalate = false`). Default 0.8.
    #[serde(default = "default_act_at")]
    pub act_at: f64,
    /// Max decisions per incoming event. Default 4.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// When true (default), non-`read_only` actions are logged, not run.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Hand low-confidence steps to the orchestrator (planner → subagents).
    /// Only surfaces with an orchestrator (the webhook listener) can escalate.
    #[serde(default = "default_true")]
    pub escalate: bool,
    /// Reducer for the incoming event: `{name = "<path>"}` (see
    /// `application::decision_loop::reduce`). Empty = raw event.
    #[serde(default)]
    pub event_reduce: BTreeMap<String, String>,
    /// Decisions HTTP timeout. Default 20.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// The action set Jev chooses from. Keys are the labels Jev sees.
    pub actions: BTreeMap<String, ActionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionConfig {
    /// Shown to Jev as the choice description.
    pub description: String,
    /// Tool to invoke. `None` = terminal action (hold / done): stop the event.
    /// Must be in the loop agent's `tools`, except for write actions of a
    /// `dry_run` loop (never invoked — lets a loop name tools not built yet).
    #[serde(default)]
    pub tool: Option<String>,
    /// Tool arguments. Strings may reference slots as `{slot}`; a string that
    /// is exactly `"{slot}"` is replaced by the slot value with its JSON type.
    #[serde(default)]
    pub args: Value,
    /// Argument slots Jev fills (one `choice` question each).
    #[serde(default)]
    pub slots: BTreeMap<String, SlotConfig>,
    /// Reducer for this action's result (see `event_reduce`). Empty = raw.
    #[serde(default)]
    pub reduce: BTreeMap<String, String>,
    /// Numeric upper bounds per slot; candidates above are never offered and
    /// answers are re-checked before running.
    #[serde(default)]
    pub caps: BTreeMap<String, f64>,
    /// Runs even when `dry_run = true` (fetches, reads).
    #[serde(default)]
    pub read_only: bool,
}

/// Where a slot's candidates come from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SlotConfig {
    /// Fixed candidate values.
    Static(Vec<Value>),
    /// Items from the latest successful `from` action's reduced result.
    FromHistory {
        /// Action name whose result to read.
        from: String,
        /// Path to the item list inside that result, e.g. `/pools/*`.
        items: String,
        /// Field of each item used as the argument value, e.g. `address`.
        value: String,
        /// Offer at most this many items. Default 5.
        #[serde(default = "default_top")]
        top: usize,
    },
}

impl DecisionLoopConfig {
    /// Structural checks that need no other config section.
    pub(crate) fn validation_errors(&self, name: &str) -> Vec<String> {
        let mut errs = Vec::new();
        let p = format!("decision_loops.{name}");
        if self.goal.trim().is_empty() {
            errs.push(format!("{p}.goal is required"));
        }
        if !(self.act_at > 0.0 && self.act_at <= 1.0) {
            errs.push(format!("{p}.act_at must be in (0, 1]"));
        }
        if self.history == 0 {
            errs.push(format!("{p}.history must be at least 1"));
        }
        if self.max_steps == 0 {
            errs.push(format!("{p}.max_steps must be at least 1"));
        }
        if self.actions.is_empty() {
            errs.push(format!("{p}.actions: at least one action is required"));
        }
        if !self.actions.values().any(|a| a.tool.is_none()) {
            errs.push(format!(
                "{p}.actions: add a terminal action without `tool` (e.g. `hold`) so the loop can stop"
            ));
        }
        for (an, a) in &self.actions {
            if a.tool.is_none() && !a.slots.is_empty() {
                errs.push(format!(
                    "{p}.actions.{an}: terminal action cannot have slots"
                ));
            }
            for (sn, slot) in &a.slots {
                match slot {
                    SlotConfig::Static(v) if v.is_empty() => {
                        errs.push(format!("{p}.actions.{an}.slots.{sn}: empty candidate list"))
                    }
                    SlotConfig::FromHistory { from, .. } if !self.actions.contains_key(from) => {
                        errs.push(format!(
                            "{p}.actions.{an}.slots.{sn}: `from = \"{from}\"` is not an action"
                        ))
                    }
                    _ => {}
                }
            }
            for cap in a.caps.keys() {
                if !a.slots.contains_key(cap) {
                    errs.push(format!(
                        "{p}.actions.{an}.caps.{cap}: no slot with that name"
                    ));
                }
            }
        }
        errs
    }
}

fn default_model() -> String {
    "~typesafe/jev-latest".to_string()
}
fn default_history() -> usize {
    8
}
fn default_act_at() -> f64 {
    0.8
}
fn default_max_steps() -> u32 {
    4
}
fn default_true() -> bool {
    true
}
fn default_timeout_secs() -> u64 {
    20
}
fn default_top() -> usize {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> DecisionLoopConfig {
        toml::from_str(s).expect("parse")
    }

    const MIN: &str = r#"
goal = "g"
agent = "a"
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch"
tool = "http_request"
read_only = true
args = { method = "GET", url = "https://x/{pair}" }
slots = { pair = ["SOL-USDC"] }
[actions.open]
description = "open"
tool = "open_position"
slots = { pool = { from = "fetch", items = "/pools/*", value = "address" }, size = [0.5, 1, 2] }
caps = { size = 2.0 }
"#;

    #[test]
    fn parses_with_defaults() {
        let c = parse(MIN);
        assert_eq!(c.model, "~typesafe/jev-latest");
        assert!(c.dry_run && c.escalate);
        assert_eq!(c.act_at, 0.8);
        assert!(matches!(
            c.actions["open"].slots["pool"],
            SlotConfig::FromHistory { top: 5, .. }
        ));
        assert!(
            c.validation_errors("x").is_empty(),
            "{:?}",
            c.validation_errors("x")
        );
    }

    #[test]
    fn rejects_missing_terminal_and_bad_refs() {
        let c = parse(
            r#"
goal = "g"
agent = "a"
act_at = 1.5
[actions.open]
description = "open"
tool = "t"
slots = { pool = { from = "nope", items = "/*", value = "a" } }
caps = { size = 1.0 }
"#,
        );
        let e = c.validation_errors("x").join("\n");
        assert!(e.contains("act_at"), "{e}");
        assert!(e.contains("terminal action"), "{e}");
        assert!(e.contains("`from = \"nope\"`"), "{e}");
        assert!(e.contains("caps.size"), "{e}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(toml::from_str::<DecisionLoopConfig>(
            "goal=\"g\"\nagent=\"a\"\nactions={}\ntypo=1"
        )
        .is_err());
    }
}
