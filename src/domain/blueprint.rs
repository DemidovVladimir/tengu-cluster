//! Blueprint — the Studio builder's canvas document (`docs/studio-builder-2026-10-10.md`):
//! cards (`nodes`) and the wires between them (`edges`), saved as
//! `sandboxes/<name>/builder.json`. Pure data: the palette, the compiler to
//! `config.toml` and the load check live in `config/builder/`; the page only
//! draws what they answer ([`Status`]).
//!
//! | Type | What |
//! |---|---|
//! | [`Blueprint`] | the document: sandbox name, nodes, edges, the canvas view |
//! | [`Node`] | one card: `id` (the page's, opaque), `kind` (a palette kind), position, `fields` (values keyed by the kind's field keys) |
//! | [`Edge`] | one wire `from` → `to` (node ids); its kind is derived from the two node kinds, never sent |
//! | [`Status`] | per node and per edge: `ok` / `warn` / `error` (`live` for a valid edge — the page draws it as flowing current) + the issues behind it |
//!
//! [`Blueprint::shape_errors`] are the checks that need no palette: ids,
//! duplicate ids, edges to missing nodes, self-loops, duplicate wires.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Version of the [`Blueprint`] JSON shape.
pub(crate) const BLUEPRINT_SCHEMA_VERSION: u32 = 1;
/// Longest node / edge id.
pub(crate) const MAX_ID_CHARS: usize = 64;
/// Most nodes in one blueprint (a sandbox is a team, not a city).
pub(crate) const MAX_NODES: usize = 400;
/// Most edges in one blueprint.
pub(crate) const MAX_EDGES: usize = 1600;

/// The canvas document (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Blueprint {
    pub schema_version: u32,
    /// The sandbox it composes: `sandboxes/<sandbox>/config.toml`.
    pub sandbox: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Pan / zoom the page restores.
    #[serde(default)]
    pub view: View,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Node {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edge {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct View {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default = "one")]
    pub zoom: f64,
}

fn one() -> f64 {
    1.0
}

impl Default for View {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

/// Whether `id` is a node / edge id the page may use: 1–64 of
/// `[A-Za-z0-9_-]`.
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Blueprint {
    pub(crate) fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Problems that need no palette (module doc), each with the node /
    /// edge it is about. Empty = the document is well-formed.
    pub(crate) fn shape_errors(&self) -> Vec<Issue> {
        let mut out = Vec::new();
        if self.schema_version != BLUEPRINT_SCHEMA_VERSION {
            out.push(Issue::error(format!(
                "blueprint schema_version {} (this build reads {BLUEPRINT_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if self.nodes.len() > MAX_NODES || self.edges.len() > MAX_EDGES {
            out.push(Issue::error(format!(
                "too large: {} nodes, {} edges (at most {MAX_NODES} / {MAX_EDGES})",
                self.nodes.len(),
                self.edges.len()
            )));
        }
        let mut ids = HashSet::new();
        for n in &self.nodes {
            if !valid_id(&n.id) {
                out.push(Issue::error(format!(
                    "node id '{}' must be 1–{MAX_ID_CHARS} of A-Z a-z 0-9 _ -",
                    n.id
                )));
            } else if !ids.insert(n.id.as_str()) {
                out.push(Issue::error(format!("node id '{}' is used twice", n.id)).node(&n.id));
            }
            if !n.x.is_finite() || !n.y.is_finite() {
                out.push(Issue::error("position is not a number").node(&n.id));
            }
        }
        let mut edge_ids = HashSet::new();
        let mut wires = HashSet::new();
        for e in &self.edges {
            if !valid_id(&e.id) {
                out.push(Issue::error(format!(
                    "edge id '{}' must be 1–{MAX_ID_CHARS} of A-Z a-z 0-9 _ -",
                    e.id
                )));
                continue;
            }
            if !edge_ids.insert(e.id.as_str()) {
                out.push(Issue::error(format!("edge id '{}' is used twice", e.id)).edge(&e.id));
            }
            for end in [&e.from, &e.to] {
                if self.node(end).is_none() {
                    out.push(
                        Issue::error(format!("wire ends at a missing card '{end}'")).edge(&e.id),
                    );
                }
            }
            if e.from == e.to {
                out.push(Issue::error("a card cannot wire to itself").edge(&e.id));
            } else if !wires.insert((e.from.as_str(), e.to.as_str())) {
                out.push(Issue::error("these two cards are already wired").edge(&e.id));
            }
        }
        out
    }
}

/// How bad an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Level {
    Warn,
    Error,
}

/// One finding, pinned to a node (and field) or an edge when it is about one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Issue {
    pub level: Level,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl Issue {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            node: None,
            edge: None,
            field: None,
        }
    }

    pub(crate) fn warn(message: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            ..Self::error(message)
        }
    }

    pub(crate) fn node(mut self, id: &str) -> Self {
        self.node = Some(id.to_string());
        self
    }

    pub(crate) fn edge(mut self, id: &str) -> Self {
        self.edge = Some(id.to_string());
        self
    }

    pub(crate) fn field(mut self, key: &str) -> Self {
        self.field = Some(key.to_string());
        self
    }
}

/// A card's or a wire's colour: the page maps the name to a look
/// (`live` = flowing current).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Ok,
    Live,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeStatus {
    pub state: State,
    /// What the card shows as its name (the TOML name it writes).
    pub title: String,
    pub subtitle: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EdgeStatus {
    pub state: State,
    /// Connection kind (`uses`, `loads`, …); empty for a wire no kind allows.
    pub edge: String,
    pub label: String,
    /// The TOML key this wire writes (`agents.<a>.tools`).
    pub writes: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Counts {
    pub errors: usize,
    pub warnings: usize,
}

/// The verdict on a blueprint (module table): every node and edge has an
/// entry; `issues` is the flat list (sandbox-wide ones included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Status {
    pub ok: bool,
    pub nodes: BTreeMap<String, NodeStatus>,
    pub edges: BTreeMap<String, EdgeStatus>,
    pub issues: Vec<Issue>,
    pub counts: Counts,
}

impl Status {
    /// Fold `issues` into per-node / per-edge states; `titles` = each node's
    /// (title, subtitle), `kinds` = each edge's (kind, label, writes).
    pub(crate) fn fold(
        bp: &Blueprint,
        issues: Vec<Issue>,
        titles: &BTreeMap<String, (String, String)>,
        kinds: &BTreeMap<String, (String, String, String)>,
    ) -> Self {
        let mut nodes = BTreeMap::new();
        for n in &bp.nodes {
            let (title, subtitle) = titles
                .get(&n.id)
                .cloned()
                .unwrap_or_else(|| (n.kind.clone(), String::new()));
            let mine: Vec<Issue> = issues
                .iter()
                .filter(|i| i.node.as_deref() == Some(n.id.as_str()))
                .cloned()
                .collect();
            nodes.insert(
                n.id.clone(),
                NodeStatus {
                    state: worst(&mine, State::Ok),
                    title,
                    subtitle,
                    issues: mine,
                },
            );
        }
        let mut edges = BTreeMap::new();
        for e in &bp.edges {
            let (edge, label, writes) = kinds.get(&e.id).cloned().unwrap_or_default();
            let mine: Vec<Issue> = issues
                .iter()
                .filter(|i| i.edge.as_deref() == Some(e.id.as_str()))
                .cloned()
                .collect();
            edges.insert(
                e.id.clone(),
                EdgeStatus {
                    state: worst(&mine, State::Live),
                    edge,
                    label,
                    writes,
                    issues: mine,
                },
            );
        }
        let counts = Counts {
            errors: issues.iter().filter(|i| i.level == Level::Error).count(),
            warnings: issues.iter().filter(|i| i.level == Level::Warn).count(),
        };
        Self {
            ok: counts.errors == 0,
            nodes,
            edges,
            issues,
            counts,
        }
    }
}

fn worst(issues: &[Issue], clean: State) -> State {
    match issues.iter().map(|i| i.level).max() {
        Some(Level::Error) => State::Error,
        Some(Level::Warn) => State::Warn,
        None => clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bp(nodes: Value, edges: Value) -> Blueprint {
        serde_json::from_value(json!({
            "schema_version": 1, "sandbox": "t", "nodes": nodes, "edges": edges
        }))
        .unwrap()
    }

    #[test]
    fn well_formed_blueprint_has_no_shape_errors() {
        let b = bp(
            json!([{"id":"a","kind":"agent"},{"id":"b","kind":"tool"}]),
            json!([{"id":"e1","from":"a","to":"b"}]),
        );
        assert!(b.shape_errors().is_empty(), "{:?}", b.shape_errors());
        assert_eq!(b.view, View::default());
    }

    #[test]
    fn shape_errors_name_the_node_or_edge() {
        let b = bp(
            json!([{"id":"a","kind":"agent"},{"id":"a","kind":"tool"},{"id":"bad id","kind":"x"}]),
            json!([
                {"id":"e1","from":"a","to":"zz"},
                {"id":"e2","from":"a","to":"a"},
                {"id":"e3","from":"a","to":"bad id"},
                {"id":"e4","from":"a","to":"bad id"}
            ]),
        );
        let errs = b.shape_errors();
        let text: Vec<&str> = errs.iter().map(|i| i.message.as_str()).collect();
        assert!(text.iter().any(|m| m.contains("used twice")), "{text:?}");
        assert!(text.iter().any(|m| m.contains("'bad id'")), "{text:?}");
        assert!(errs
            .iter()
            .any(|i| i.edge.as_deref() == Some("e1") && i.message.contains("missing card 'zz'")));
        assert!(errs
            .iter()
            .any(|i| i.edge.as_deref() == Some("e2") && i.message.contains("itself")));
        assert!(errs
            .iter()
            .any(|i| i.edge.as_deref() == Some("e4") && i.message.contains("already wired")));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let r: Result<Blueprint, _> = serde_json::from_value(json!({
            "schema_version": 1, "sandbox": "t", "nodes": [], "edges": [], "extra": 1
        }));
        assert!(r.is_err());
    }

    #[test]
    fn fold_colours_nodes_and_edges_by_their_worst_issue() {
        let b = bp(
            json!([{"id":"a","kind":"agent"},{"id":"b","kind":"tool"},{"id":"c","kind":"tool"}]),
            json!([{"id":"e1","from":"a","to":"b"},{"id":"e2","from":"a","to":"c"}]),
        );
        let issues = vec![
            Issue::warn("w").node("a"),
            Issue::error("bad wire").edge("e2"),
            Issue::error("sandbox-wide"),
        ];
        let s = Status::fold(&b, issues, &BTreeMap::new(), &BTreeMap::new());
        assert!(!s.ok);
        assert_eq!(
            s.counts,
            Counts {
                errors: 2,
                warnings: 1
            }
        );
        assert_eq!(s.nodes["a"].state, State::Warn);
        assert_eq!(s.nodes["b"].state, State::Ok);
        assert_eq!(s.edges["e1"].state, State::Live);
        assert_eq!(s.edges["e2"].state, State::Error);
        assert_eq!(s.issues.len(), 3);
    }
}
