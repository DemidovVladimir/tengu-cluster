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
//!
//! Typed tools (results carrying an `Observation`) add `world` (alias →
//! observation-store key, read every step, never fetched), a per-action
//! `requires` freshness gate and `FromObservation` slots:
//!
//! ```toml
//! world = { price = "price_oracle/1:So11111111111111111111111111111111111111112" }
//! world_max_age_secs = 30
//!
//! [decision_loops.lp_watch.actions.open_position]
//! requires = { price = 30 }   # legal only while `price` is usable and <= 30 s old
//! slots    = { pool = { observation = "pools", items = "/data/pools/*", value = "address" } }
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
    /// `state.world`: alias → observation key (`<schema>:<subject>`, e.g.
    /// `price_oracle/1:<mint>`). Read from the loop agent's observation store
    /// (`<workspace>/.tengu/observations.db`) every step — never fetched.
    /// Stale / missing / failed entries carry no numbers. Empty (default) =
    /// no `world` key in the state.
    #[serde(default)]
    pub world: BTreeMap<String, String>,
    /// Max age (seconds) of a usable `world` entry for it to be rendered with
    /// its features and to feed `FromObservation` slots. Default 30.
    #[serde(default = "default_world_max_age_secs")]
    pub world_max_age_secs: u64,
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
    /// Freshness gate: `world` alias → max age in seconds. The action is
    /// offered only while every listed entry exists, is usable (status not
    /// `error`) and is at most that old.
    #[serde(default)]
    pub requires: BTreeMap<String, u64>,
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
    /// Items from a fresh `world` entry, read from its
    /// `Observation::decision_root` (`{status, age_s, source, features,
    /// data}`). Stale / missing / failed entries yield no candidates, so the
    /// action is not legal.
    FromObservation {
        /// `world` alias to read (distinct from `FromHistory`'s `from`).
        observation: String,
        /// Path to the item list, e.g. `/data/pools/*`.
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
        for (alias, key) in &self.world {
            if !key.contains(':') {
                errs.push(format!(
                    "{p}.world.{alias}: `{key}` is not an observation key (`<schema>:<subject>`)"
                ));
            }
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
                    SlotConfig::FromObservation { observation, .. }
                        if !self.world.contains_key(observation) =>
                    {
                        errs.push(format!(
                            "{p}.actions.{an}.slots.{sn}: `observation = \"{observation}\"` is not a `world` alias"
                        ))
                    }
                    _ => {}
                }
            }
            for alias in a.requires.keys() {
                if !self.world.contains_key(alias) {
                    errs.push(format!(
                        "{p}.actions.{an}.requires.{alias}: not a `world` alias"
                    ));
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
fn default_world_max_age_secs() -> u64 {
    30
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
    fn old_toml_has_no_world() {
        let c = parse(MIN);
        assert!(c.world.is_empty());
        assert_eq!(c.world_max_age_secs, 30);
        assert!(c.actions.values().all(|a| a.requires.is_empty()));
    }

    const TYPED: &str = r#"
goal = "g"
agent = "a"
world = { price = "price_oracle/1:So11111111111111111111111111111111111111112", pools = "dlmm_pools/1:SOL-USDC|fee_tvl_24h|10|100000" }
world_max_age_secs = 45
[actions.hold]
description = "nothing"
[actions.open]
description = "open"
tool = "dlmm_open_position"
requires = { price = 30 }
slots = { pool = { observation = "pools", items = "/data/pools/*", value = "address", top = 3 }, size = [1] }
"#;

    #[test]
    fn parses_world_requires_and_observation_slot() {
        let c = parse(TYPED);
        assert_eq!(c.world_max_age_secs, 45);
        assert_eq!(
            c.world["price"],
            "price_oracle/1:So11111111111111111111111111111111111111112"
        );
        let open = &c.actions["open"];
        assert_eq!(open.requires["price"], 30);
        assert!(matches!(
            &open.slots["pool"],
            SlotConfig::FromObservation { observation, top: 3, .. } if observation == "pools"
        ));
        assert!(
            c.validation_errors("x").is_empty(),
            "{:?}",
            c.validation_errors("x")
        );
    }

    #[test]
    fn rejects_unknown_world_aliases_and_bad_keys() {
        let c = parse(
            r#"
goal = "g"
agent = "a"
world = { price = "no-colon" }
[actions.hold]
description = "nothing"
[actions.open]
description = "open"
tool = "t"
requires = { oracle = 30 }
slots = { pool = { observation = "pools", items = "/data/*", value = "a" } }
"#,
        );
        let e = c.validation_errors("x").join("\n");
        assert!(e.contains("world.price"), "{e}");
        assert!(e.contains("requires.oracle"), "{e}");
        assert!(e.contains("`observation = \"pools\"`"), "{e}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(toml::from_str::<DecisionLoopConfig>(
            "goal=\"g\"\nagent=\"a\"\nactions={}\ntypo=1"
        )
        .is_err());
    }
}
