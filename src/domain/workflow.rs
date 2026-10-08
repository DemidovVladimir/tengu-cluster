//! Workflow graph — the read model Tengu Studio draws (`TENGU_STUDIO_PLAN.md`
//! § 5): one sandbox's validated config + tool catalog (+ an optional
//! execution map) as nodes and edges. Pure data: built by
//! `application/studio/graph.rs`, printed by `tengu studio graph`. Layout
//! columns (`layer`) and the order inside a column (`order`) are computed
//! here in Rust, so a browser only draws.
//!
//! | Node id ([`node_id`]) | Kind | Layer | What |
//! |---|---|---|---|
//! | `runtime:<sandbox>` | `runtime` | 0 | `tengu run` of the sandbox (`[runtime]` knobs) |
//! | `trigger:decide` · `trigger:webhook/<endpoint>` · `trigger:map/<sha256>` | `trigger` | 1 | what can hand a loop an event |
//! | `feed:<name>` | `feed` | 1 | a `[feeds.<name>]` schedule |
//! | `agent:<name>` · `loop:<name>` | `agent` · `loop` | 2 | a loop and the agent whose tools, scopes and workspace it uses |
//! | `world:<loop>/<alias>` | `world` | 3 | an observation the loop reads every step (never fetched) |
//! | `jev:<loop>` · `gate:<loop>/act_at` | `jev` · `gate` | 4 | the decisions call and its confidence gate |
//! | `action:<loop>/<action>` · `escalation:<loop>` | `action` · `escalation` | 5 | a choice Jev may pick; where a low-confidence step goes |
//! | `gate:<loop>/<action>/caps` · `scope:<agent>/<tool>` | `gate` · `scope` | 6 | numeric caps of an action; the agent's configured scope of a tool |
//! | `tool:<agent>/<tool>` | `tool` | 7 | a tool as that agent runs it |
//!
//! | Edge | From → to |
//! |---|---|
//! | `fires` | trigger / tick feed → loop |
//! | `reads` | loop → world |
//! | `asks` | loop → jev |
//! | `chooses` | jev → action (kept actions only) |
//! | `calls` | action / tool feed → tool |
//! | `binds` | world / earlier action → action whose slot it fills |
//! | `next` | `sequence` step → the step after it |
//! | `guards` | gate → jev / action · world (`requires`) → action · scope → tool |
//! | `escalates` | act_at gate → escalation |
//! | `owns` | agent → loop / tool feed · runtime → feed |
//!
//! Ids keep every name in full — tool names, `{server}__{tool}`, the 64-hex
//! map hash — never shortened. `narrowed_out` = configured in the sandbox but
//! dropped by the execution map (drawn grey).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the [`WorkflowGraph`] JSON shape.
pub(crate) const WORKFLOW_SCHEMA_VERSION: u32 = 1;

/// Layout columns, left → right (`Node::layer`, module table).
pub(crate) mod layer {
    pub(crate) const RUNTIME: u8 = 0;
    pub(crate) const TRIGGER: u8 = 1;
    pub(crate) const LOOP: u8 = 2;
    pub(crate) const WORLD: u8 = 3;
    pub(crate) const JEV: u8 = 4;
    pub(crate) const ACTION: u8 = 5;
    pub(crate) const GUARD: u8 = 6;
    pub(crate) const TOOL: u8 = 7;
}

/// One sandbox as a graph (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkflowGraph {
    pub schema_version: u32,
    /// The runner name: the `--sandbox` name, else `default`.
    pub sandbox: String,
    /// sha256 of the loaded config file (`Config::source_sha256`); `None`
    /// for a config built in code.
    pub config_hash: Option<String>,
    /// The execution map the graph was narrowed by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapRef>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// An execution map by identity (`ExecutionMap::sha256`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MapRef {
    pub sha256: String,
    #[serde(rename = "loop")]
    pub loop_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    /// Column ([`layer`]).
    pub layer: u8,
    /// Position inside the column.
    pub order: u16,
    /// Validated config facts the inspector shows (redacted by the caller).
    pub attrs: BTreeMap<String, Value>,
    /// Configured, but dropped by the execution map.
    pub narrowed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeKind {
    Runtime,
    Trigger,
    Feed,
    Agent,
    Loop,
    World,
    Jev,
    Gate,
    Action,
    Escalation,
    Scope,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EdgeKind {
    Fires,
    Reads,
    Asks,
    Chooses,
    Calls,
    Binds,
    Next,
    Guards,
    Escalates,
    Owns,
}

/// Node ids (module table). Every name goes in whole.
pub(crate) mod node_id {
    pub(crate) fn runtime(sandbox: &str) -> String {
        format!("runtime:{sandbox}")
    }
    pub(crate) fn trigger_decide() -> String {
        "trigger:decide".to_string()
    }
    pub(crate) fn trigger_webhook(endpoint: &str) -> String {
        format!("trigger:webhook/{endpoint}")
    }
    pub(crate) fn trigger_map(sha256: &str) -> String {
        format!("trigger:map/{sha256}")
    }
    pub(crate) fn feed(name: &str) -> String {
        format!("feed:{name}")
    }
    pub(crate) fn agent(name: &str) -> String {
        format!("agent:{name}")
    }
    pub(crate) fn loop_(name: &str) -> String {
        format!("loop:{name}")
    }
    pub(crate) fn world(loop_name: &str, alias: &str) -> String {
        format!("world:{loop_name}/{alias}")
    }
    pub(crate) fn jev(loop_name: &str) -> String {
        format!("jev:{loop_name}")
    }
    pub(crate) fn gate_act_at(loop_name: &str) -> String {
        format!("gate:{loop_name}/act_at")
    }
    pub(crate) fn action(loop_name: &str, action: &str) -> String {
        format!("action:{loop_name}/{action}")
    }
    pub(crate) fn escalation(loop_name: &str) -> String {
        format!("escalation:{loop_name}")
    }
    pub(crate) fn gate_caps(loop_name: &str, action: &str) -> String {
        format!("gate:{loop_name}/{action}/caps")
    }
    pub(crate) fn scope(agent: &str, tool: &str) -> String {
        format!("scope:{agent}/{tool}")
    }
    pub(crate) fn tool(agent: &str, tool: &str) -> String {
        format!("tool:{agent}/{tool}")
    }
}

impl WorkflowGraph {
    /// Canonical order: nodes by (layer, order, id), edges by (from, to,
    /// kind); duplicate ids / edges dropped (first kept). The same graph
    /// serializes to the same bytes whatever order it was built in.
    pub(crate) fn normalize(&mut self) {
        self.nodes
            .sort_by(|a, b| (a.layer, a.order, &a.id).cmp(&(b.layer, b.order, &b.id)));
        let mut seen = std::collections::BTreeSet::new();
        self.nodes.retain(|n| seen.insert(n.id.clone()));
        self.edges.sort();
        self.edges.dedup();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Edges whose `from` or `to` is not a node (a builder bug).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn dangling_edges(&self) -> Vec<&Edge> {
        let ids: std::collections::BTreeSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        self.edges
            .iter()
            .filter(|e| !ids.contains(e.from.as_str()) || !ids.contains(e.to.as_str()))
            .collect()
    }

    /// Apply `f` to every attr value — the caller's redaction
    /// (`SecretRegistry::redact_value`) before the graph leaves the process.
    pub(crate) fn map_attrs(&mut self, mut f: impl FnMut(&mut Value)) {
        for n in &mut self.nodes {
            n.attrs.values_mut().for_each(&mut f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, layer: u8, order: u16) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Action,
            label: id.to_string(),
            layer,
            order,
            attrs: BTreeMap::from([("k".to_string(), json!(id))]),
            narrowed_out: false,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            kind,
        }
    }

    /// Any build order normalizes to the same nodes, edges and bytes.
    #[test]
    fn normalize_is_order_independent() {
        let nodes = vec![
            node("loop:b", layer::LOOP, 1),
            node("loop:a", layer::LOOP, 0),
            node("tool:x/read_file", layer::TOOL, 0),
            node("trigger:decide", layer::TRIGGER, 0),
            node("action:a/hold", layer::ACTION, 0),
            node("action:a/act", layer::ACTION, 0),
        ];
        let edges = vec![
            edge("trigger:decide", "loop:a", EdgeKind::Fires),
            edge("trigger:decide", "loop:b", EdgeKind::Fires),
            edge("action:a/act", "tool:x/read_file", EdgeKind::Calls),
            edge("loop:a", "action:a/act", EdgeKind::Asks),
        ];
        let mk = |nodes: Vec<Node>, edges: Vec<Edge>| WorkflowGraph {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            sandbox: "s".into(),
            config_hash: None,
            map: None,
            nodes,
            edges,
        };
        let mut a = mk(nodes.clone(), edges.clone());
        let (mut rn, mut re) = (nodes.clone(), edges.clone());
        rn.reverse();
        re.reverse();
        // A duplicate node and edge collapse to one each.
        rn.push(node("loop:a", layer::LOOP, 0));
        re.push(edge("trigger:decide", "loop:a", EdgeKind::Fires));
        let mut b = mk(rn, re);
        a.normalize();
        b.normalize();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        let ids: Vec<&str> = a.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "trigger:decide",
                "loop:a",
                "loop:b",
                "action:a/act",
                "action:a/hold",
                "tool:x/read_file"
            ]
        );
        assert_eq!(a.edges.len(), 4);
        assert!(a.dangling_edges().is_empty());
    }

    /// Ids carry names whole: an MCP tool, a long action name, a 64-hex map
    /// hash and a Solana mint are never shortened.
    #[test]
    fn node_ids_keep_full_names() {
        let sha = "1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef";
        assert_eq!(node_id::trigger_map(sha), format!("trigger:map/{sha}"));
        assert_eq!(
            node_id::tool("lp_executor", "helius__get_asset_by_owner_with_metadata"),
            "tool:lp_executor/helius__get_asset_by_owner_with_metadata"
        );
        let action = "open_position_in_the_highest_fee_pool_after_refresh";
        assert_eq!(
            node_id::action("lp_watch", action),
            format!("action:lp_watch/{action}")
        );
        let mint = "So11111111111111111111111111111111111111112";
        assert_eq!(
            node_id::world("lp_watch", mint),
            format!("world:lp_watch/{mint}")
        );
        assert_eq!(node_id::gate_caps("l", "open"), "gate:l/open/caps");
        assert_eq!(node_id::scope("a", "http_request"), "scope:a/http_request");
        assert_eq!(
            node_id::runtime("control-loop-lab"),
            "runtime:control-loop-lab"
        );
        assert_eq!(node_id::trigger_webhook("helius"), "trigger:webhook/helius");
    }
}
