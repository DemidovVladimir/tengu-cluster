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
//!
//! Bindings carry one value into an argument without a question to Jev —
//! from the event (what a higher-order agent hands the loop), an earlier
//! action of this event, or a `world` row. Unresolved ⇒ the action is not
//! legal, so a chain only advances on real values:
//!
//! ```toml
//! slots = { target_sol = { event = "/target_sol" },
//!           amount     = { from = "plan_swap", path = "/swaps/0/amount" },
//!           wallet_sol = { observation = "snap", path = "/data/wallet_balances/native_sol/value" } }
//! caps  = { target_sol = 2.0 }
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
    /// Explicit step order: `"action"`, or `"action?"` for an optional
    /// step. Each step offers Jev only the next action + the terminal
    /// actions; an optional step that is not legal (a binding unresolved,
    /// `requires` stale) is skipped; a required one that is not legal, or
    /// a step whose tool failed or was refused, halts the chain (terminal
    /// actions only). Empty (default) = Jev picks among every legal action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequence: Vec<SeqStep>,
}

/// One `sequence` step; written `"action"` or `"action?"` (optional).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct SeqStep {
    pub action: String,
    pub optional: bool,
}

impl TryFrom<String> for SeqStep {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let (action, optional) = match s.strip_suffix('?') {
            Some(a) => (a, true),
            None => (s.as_str(), false),
        };
        if action.trim().is_empty() || action.contains('?') {
            return Err(format!(
                "sequence step `{s}`: an action name, `?` at the end for an optional step"
            ));
        }
        Ok(SeqStep {
            action: action.to_string(),
            optional,
        })
    }
}

impl From<SeqStep> for String {
    fn from(s: SeqStep) -> String {
        if s.optional {
            format!("{}?", s.action)
        } else {
            s.action
        }
    }
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

/// Where a slot's candidates come from. A source either offers a list
/// (`items` + `value`: one candidate per item, Jev picks) or binds ONE value
/// (`path`: a single candidate, filled without a question). A path that
/// reads nothing (missing, `null`, a failed `Field`'s absent `value`) yields
/// no candidate, so the action is not legal — a binding never invents a
/// value. Typed results: bind exact amounts from `data` (features are
/// rounded to 6 significant digits).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum SlotConfig {
    /// Fixed candidate values.
    Static(Vec<Value>),
    /// The latest successful `from` action's reduced result in the CURRENT
    /// event. An earlier event's result is never offered (its data may be
    /// hours old): until `from` succeeds in this event the slot has no
    /// candidates and the action is not legal.
    FromHistory {
        /// Action name whose result to read.
        from: String,
        /// List mode: path to the item list inside that result, e.g. `/pools/*`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        items: Option<String>,
        /// List mode: field of each item used as the argument value, e.g. `address`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// Binding mode: path to the one value, e.g. `/swaps/0/amount`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// List mode: offer at most this many items. Default 5.
        #[serde(default = "default_top")]
        top: usize,
    },
    /// A fresh `world` entry, read from its `Observation::decision_root`
    /// (`{status, age_s, source, features, data}`). Stale / missing / failed
    /// entries yield no candidates, so the action is not legal.
    FromObservation {
        /// `world` alias to read (distinct from `FromHistory`'s `from`).
        observation: String,
        /// List mode: path to the item list, e.g. `/data/pools/*`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        items: Option<String>,
        /// List mode: field of each item used as the argument value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// Binding mode: path to the one value, e.g.
        /// `/data/wallet_balances/native_sol/value`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// List mode: offer at most this many items. Default 5.
        #[serde(default = "default_top")]
        top: usize,
    },
    /// The incoming event (after `event_reduce`) — what a trigger or a
    /// higher-order agent hands the loop (`tengu decide --event`, a webhook
    /// body). `event` is the path: without `value` it binds the one value
    /// there (`{ event = "/target_sol" }`); with `value` it is a list path
    /// and each item's `value` field is a candidate. Event values are
    /// untrusted input: `caps`, the tool's own checks and the agent's
    /// scopes (wallet grants) still apply.
    FromEvent {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// List mode: offer at most this many items. Default 5.
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
        for step in &self.sequence {
            match self.actions.get(&step.action) {
                None => errs.push(format!(
                    "{p}.sequence: `{}` is not an action",
                    step.action
                )),
                Some(a) if a.tool.is_none() => errs.push(format!(
                    "{p}.sequence: `{}` is terminal — terminal actions are always offered, never sequenced",
                    step.action
                )),
                Some(_) => {}
            }
        }
        if !self.sequence.is_empty() && (self.max_steps as usize) <= self.sequence.len() {
            errs.push(format!(
                "{p}.max_steps ({}) must exceed the sequence length ({}) so the chain can end on a terminal action",
                self.max_steps,
                self.sequence.len()
            ));
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
                let at = format!("{p}.actions.{an}.slots.{sn}");
                match slot {
                    SlotConfig::Static(v) if v.is_empty() => {
                        errs.push(format!("{at}: empty candidate list"))
                    }
                    SlotConfig::FromHistory { from, .. } if !self.actions.contains_key(from) => {
                        errs.push(format!("{at}: `from = \"{from}\"` is not an action"))
                    }
                    SlotConfig::FromObservation { observation, .. }
                        if !self.world.contains_key(observation) =>
                    {
                        errs.push(format!(
                            "{at}: `observation = \"{observation}\"` is not a `world` alias"
                        ))
                    }
                    _ => {}
                }
                if let Some(problem) = slot.mode_problem() {
                    errs.push(format!("{at}: {problem}"));
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

/// How a bound slot reads its source (`SlotConfig::mode`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SlotMode<'a> {
    /// Every item at `items`; each item's `value` field is a candidate.
    List {
        items: &'a str,
        value: &'a str,
        top: usize,
    },
    /// The one value at `path`.
    One { path: &'a str },
}

impl SlotConfig {
    /// List or binding mode of a sourced slot; `None` for `Static` and for
    /// a slot `mode_problem` refuses.
    pub(crate) fn mode(&self) -> Option<SlotMode<'_>> {
        fn sourced<'a>(
            items: &'a Option<String>,
            value: &'a Option<String>,
            path: &'a Option<String>,
            top: usize,
        ) -> Option<SlotMode<'a>> {
            match (items.as_deref(), value.as_deref(), path.as_deref()) {
                (Some(items), Some(value), None) => Some(SlotMode::List { items, value, top }),
                (None, None, Some(path)) => Some(SlotMode::One { path }),
                _ => None,
            }
        }
        match self {
            SlotConfig::Static(_) => None,
            SlotConfig::FromHistory {
                items,
                value,
                path,
                top,
                ..
            }
            | SlotConfig::FromObservation {
                items,
                value,
                path,
                top,
                ..
            } => sourced(items, value, path, *top),
            SlotConfig::FromEvent { event, value, top } => Some(match value.as_deref() {
                Some(value) => SlotMode::List {
                    items: event,
                    value,
                    top: *top,
                },
                None => SlotMode::One { path: event },
            }),
        }
    }

    /// Why a sourced slot is malformed: list mode needs `items` AND `value`,
    /// binding mode `path` alone.
    fn mode_problem(&self) -> Option<&'static str> {
        match self {
            SlotConfig::FromHistory { .. } | SlotConfig::FromObservation { .. }
                if self.mode().is_none() =>
            {
                Some("set either `items` + `value` (a list Jev picks from) or `path` (one bound value)")
            }
            SlotConfig::FromEvent { event, .. } if !event.starts_with('/') => {
                Some("`event` is a path into the event and starts with `/`")
            }
            _ => None,
        }
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

    const BOUND: &str = r#"
goal = "g"
agent = "a"
world = { snap = "lp_snapshot/1:W:P" }
[actions.hold]
description = "nothing"
[actions.plan]
description = "plan"
tool = "lp_swap_plan"
slots = { target = { event = "/target_sol" }, sol = { observation = "snap", path = "/data/sol" } }
caps = { target = 2.0 }
[actions.swap]
description = "swap"
tool = "jupiter_swap"
slots = { amount = { from = "plan", path = "/swaps/0/amount" }, pool = { event = "/pools/*", value = "address", top = 2 } }
"#;

    #[test]
    fn parses_bindings_and_event_slots() {
        let c = parse(BOUND);
        assert!(
            c.validation_errors("x").is_empty(),
            "{:?}",
            c.validation_errors("x")
        );
        let plan = &c.actions["plan"].slots;
        assert_eq!(
            plan["target"].mode(),
            Some(SlotMode::One {
                path: "/target_sol"
            })
        );
        assert_eq!(
            plan["sol"].mode(),
            Some(SlotMode::One { path: "/data/sol" })
        );
        let swap = &c.actions["swap"].slots;
        assert_eq!(
            swap["amount"].mode(),
            Some(SlotMode::One {
                path: "/swaps/0/amount"
            })
        );
        assert_eq!(
            swap["pool"].mode(),
            Some(SlotMode::List {
                items: "/pools/*",
                value: "address",
                top: 2
            })
        );
        // Old list slots keep their mode.
        assert_eq!(
            parse(MIN).actions["open"].slots["pool"].mode(),
            Some(SlotMode::List {
                items: "/pools/*",
                value: "address",
                top: 5
            })
        );
    }

    #[test]
    fn rejects_mixed_or_incomplete_slot_modes() {
        let c = parse(
            r#"
goal = "g"
agent = "a"
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch"
tool = "t"
[actions.open]
description = "open"
tool = "t"
slots = { both = { from = "fetch", items = "/a/*", value = "x", path = "/b" }, half = { from = "fetch", items = "/a/*" }, ev = { event = "target" } }
"#,
        );
        let e = c.validation_errors("x").join("\n");
        assert!(e.contains("slots.both: set either"), "{e}");
        assert!(e.contains("slots.half: set either"), "{e}");
        assert!(e.contains("slots.ev: `event` is a path"), "{e}");
    }

    /// `sandboxes/control-loop-lab` (TENGU_STUDIO_PLAN ST-02) stays the safe
    /// reference run: no money, signer or generation section; private agents
    /// holding only the three workspace tools, each with its own scope inside
    /// the lab workspace and no shell, network, env or wallet grant; the one
    /// write action's path a TOML constant; feeds read-only; every scenario
    /// event an object with a `scenario`, every scenario map a valid
    /// narrowing of `demo` (`ExecutionMap::apply`).
    #[test]
    fn control_loop_lab_has_no_dangerous_surface() {
        use crate::config::execution_map::ExecutionMap;
        use crate::config::paths::expand_tilde;
        use crate::config::Config;
        use std::path::{Component, Path};

        const TOOLS: [&str; 3] = ["read_file", "write_file", "list_directory"];
        const READ_ONLY: [&str; 2] = ["read_file", "list_directory"];
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/control-loop-lab");
        let cfg = Config::load(&dir.join("config.toml")).unwrap_or_else(|e| panic!("{e:#}"));

        assert!(cfg.risk.is_none() && cfg.paper.is_none() && cfg.xmarket.is_none());
        assert!(cfg.solana.signer_key_file.is_none(), "no signer");
        assert!(cfg.generation.is_none(), "unbound: no generation pins");
        assert!(cfg.mcp_servers.is_empty() && cfg.orchestrator.is_none());
        assert!(cfg.default_scopes.is_empty(), "scopes per agent only");

        for (name, a) in &cfg.agents {
            assert!(
                !a.tools.is_empty(),
                "{name}: an empty `tools` = every always-on tool, run_command included"
            );
            assert!(a.workspace_tools.is_empty(), "{name}: no opt-in tools");
            for t in &a.tools {
                assert!(TOOLS.contains(&t.as_str()), "{name}: tool `{t}`");
                assert!(a.scopes.contains_key(t), "{name}: `{t}` has no own scope");
            }
            assert_ne!(
                a.engine, "claude_code",
                "{name}: CLI built-ins ignore scopes"
            );
            assert!(a.description.is_none() && !a.default, "{name}: private");
            let ws = expand_tilde(a.workspace.as_deref().expect("a lab workspace"));
            for (tool, s) in &a.scopes {
                let grants = (&s.shell_bins, &s.net_hosts, &s.env_reads, &s.wallets);
                assert!(
                    s.shell_bins.is_empty()
                        && s.net_hosts.is_empty()
                        && s.env_reads.is_empty()
                        && s.wallets.is_empty(),
                    "{name}.scopes.{tool}: {grants:?}"
                );
                assert!(!s.fs_roots.is_empty(), "{name}.scopes.{tool}: no root");
                for r in &s.fs_roots {
                    let inside =
                        r.starts_with(&ws) && !r.components().any(|c| c == Component::ParentDir);
                    assert!(
                        inside,
                        "{name}.scopes.{tool}: {} outside {}",
                        r.display(),
                        ws.display()
                    );
                }
            }
        }

        for (ln, dl) in &cfg.decision_loops {
            for (an, a) in &dl.actions {
                let Some(tool) = a.tool.as_deref() else {
                    continue;
                };
                let at = format!("decision_loops.{ln}.actions.{an}");
                assert!(TOOLS.contains(&tool), "{at}: tool `{tool}`");
                if READ_ONLY.contains(&tool) {
                    continue;
                }
                assert!(
                    !a.read_only,
                    "{at}: a write flagged read_only runs in dry-run"
                );
                let path = a.args.get("path").and_then(Value::as_str).unwrap_or("");
                let constant = path.starts_with("out/")
                    && !path.contains('{')
                    && !Path::new(path)
                        .components()
                        .any(|c| c == Component::ParentDir);
                assert!(
                    constant,
                    "{at}: write path `{path}` is not a constant under out/"
                );
            }
        }
        for (fname, f) in &cfg.feeds {
            if let Some(t) = f.tool.as_deref() {
                assert!(READ_ONLY.contains(&t), "feeds.{fname}: scheduled `{t}`");
            }
        }

        let demo = &cfg.decision_loops["demo"];
        let mut maps = BTreeMap::new();
        for entry in std::fs::read_dir(dir.join("scenarios")).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            if let Some(stem) = file.strip_suffix(".map.json") {
                let map = ExecutionMap::parse(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
                assert_eq!(map.loop_name, "demo", "{file}");
                let narrowed = map.apply(demo).unwrap_or_else(|e| panic!("{file}: {e:?}"));
                maps.insert(stem.to_string(), narrowed);
            } else {
                let event: Value =
                    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
                assert!(event["scenario"].is_string(), "{file}: no `scenario`");
            }
        }
        let knobs = |m: &str| (maps[m].act_at, maps[m].dry_run, maps[m].actions.len());
        assert_eq!(knobs("uncertain"), (1.0, true, demo.actions.len()));
        assert_eq!(knobs("act-dry"), (demo.act_at, true, demo.actions.len()));
    }

    #[test]
    fn slot_with_two_sources_or_a_typo_is_rejected() {
        for slot in [
            r#"{ from = "fetch", observation = "snap", path = "/x" }"#,
            r#"{ event = "/x", pth = "/y" }"#,
            r#"{ from = "fetch", path = "/x", tpo = 3 }"#,
        ] {
            let toml = format!(
                "goal=\"g\"\nagent=\"a\"\n[actions.hold]\ndescription=\"h\"\n\
                 [actions.fetch]\ndescription=\"f\"\ntool=\"t\"\nslots = {{ s = {slot} }}\n"
            );
            assert!(
                toml::from_str::<DecisionLoopConfig>(&toml).is_err(),
                "accepted {slot}"
            );
        }
    }
}
