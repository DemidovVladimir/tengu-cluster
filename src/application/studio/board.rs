//! Studio board — one recorded run folded over the workflow graph: the one
//! place an event becomes a node colour, a highlighted edge, a grey node or
//! a header fact (`TENGU_STUDIO_PLAN.md` § 4). Pure: events in `seq` order +
//! the graph in, a per-event view and the board at a `seq` out; the page
//! draws what this returns and decides nothing (feature `studio`).
//!
//! | Rule | Value |
//! |---|---|
//! | node colour | the latest event naming the node (`node_id`) → `Status::tone`; no event yet = no tone |
//! | grey | a map's `narrowed_out` node; or an action node of a loop absent from that loop's latest `jev.*` `payload.legal_actions` (the step's legal set; a set the payload bound dropped greys nothing). Grey wins over the node's latest tone |
//! | highlighted edge | only `action.*` / `tool.*` / `step.*` events, only edges the graph has: `jev:<l>` → the action (`chooses`; `action.escalated` names the picked action in `payload.action`), `gate:<l>/act_at` → `escalation:<l>` (`escalates`, on `action.escalated`), the caller → the tool (`calls`: the nearest action, feed or agent up the parent chain — a plan step's `run-agent` tool calls hang under its `step.started`), `planner` → the step's agent (`delegates`, on `step.*`); colour = that event's tone |
//! | facets | the node's (`Node::facets`), each missing field from the parent event's (`parent_event_id`); `model` = a `jev.*` `payload.model`, else the parent's — so a tool call carries its loop, action, feed and the model that chose it |
//! | header | first event: run ids, `config_hash`; `run.opened` `payload.kind`; runtime = the latest `runtime.*` (state = the kind after `runtime.`); model = the latest `jev.completed` `payload.model`; counters = each loop's latest `loop.*` `payload.stats`; map = a `trigger.map` root's sha256; `closed` = the last event folded closes the run (`domain::trace::closes_run`) |
//! | not in this graph | a `node_id` the graph lacks (the run's config differs, a map trigger on the base graph) is listed, never drawn |
//!
//! Parents are remembered for the last [`PARENT_WINDOW`] events (a parent is
//! a few lines back), so a fold of any run is bounded; the same file always
//! folds to the same board.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::Serialize;
use serde_json::Value;

use crate::domain::trace::{closes_run, ExecutionEvent, Status, Tone, RUN_OPENED};
use crate::domain::workflow::{node_id, EdgeKind, Facets, Node, NodeKind, WorkflowGraph};

/// Events whose facets a child may inherit (by `event_id`).
pub(crate) const PARENT_WINDOW: usize = 4096;

/// What the page needs of one event besides the event itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EventView {
    pub tone: Tone,
    #[serde(flatten)]
    pub facets: Facets,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Its `node_id` is a node of the graph the run is drawn on.
    pub in_graph: bool,
    /// The graph edges it highlights.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct EdgeRef {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// The latest event that named a node or highlighted an edge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Mark {
    pub tone: Tone,
    pub status: Status,
    pub event_kind: String,
    pub seq: u64,
    pub event_id: String,
    pub ts_ms: i64,
}

/// How one graph node is drawn at this `seq`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct NodeDraw {
    pub node_id: String,
    /// `None`: no event named it yet and it is not grey.
    pub tone: Option<Tone>,
    /// `event` · `not_legal` (the step's legal set) · `narrowed_out` (the map).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark: Option<Mark>,
    /// The last event folded named it.
    pub latest: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EdgeDraw {
    #[serde(flatten)]
    pub edge: EdgeRef,
    #[serde(flatten)]
    pub mark: Mark,
    pub latest: bool,
}

/// One loop's legal set: the latest `jev.*` of that loop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegalSet {
    #[serde(rename = "loop")]
    pub loop_name: String,
    pub event_id: String,
    pub seq: u64,
    pub step: Option<u64>,
    /// `None`: the payload bound dropped it (nothing is greyed).
    pub legal_actions: Option<Vec<String>>,
    /// The loop's action nodes not in it.
    pub grey: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RuntimeSeen {
    /// `starting` · `running` · `start_failed` · `stopping` · `stopped`.
    pub state: String,
    pub status: Status,
    pub tone: Tone,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Seen {
    pub value: Value,
    pub event_id: String,
}

/// The header facts (module table).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct Header {
    pub run_id: Option<String>,
    pub runtime_id: Option<String>,
    pub sandbox: Option<String>,
    pub config_hash: Option<String>,
    /// `run` · `decide` (the `run.opened` payload).
    pub kind: Option<String>,
    pub runtime: Option<RuntimeSeen>,
    pub model: Option<Seen>,
    /// Loop name → its latest `payload.stats`.
    pub loops: BTreeMap<String, Seen>,
    /// The sha256 of the execution map a `trigger.map` root ran.
    pub map: Option<String>,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Latest {
    pub event_id: String,
    pub seq: u64,
    pub node_id: Option<String>,
}

/// A run at one `seq` (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Board {
    /// `seq` of the last event folded (0: none).
    pub upto: u64,
    pub events: u64,
    pub header: Header,
    /// Every graph node, in graph order.
    pub nodes: Vec<NodeDraw>,
    /// Highlighted edges, by (from, to, kind).
    pub edges: Vec<EdgeDraw>,
    /// By loop name.
    pub legal: Vec<LegalSet>,
    /// Node ids events named that the graph lacks, sorted.
    pub not_in_graph: Vec<String>,
    pub latest: Option<Latest>,
}

/// What a child event takes from its parent.
#[derive(Debug, Clone, Default)]
struct Inherit {
    facets: Facets,
    model: Option<String>,
    /// The nearest action or feed node up the chain (who calls a tool).
    caller: Option<String>,
}

/// The fold (module table).
pub(crate) struct Fold<'g> {
    graph: &'g WorkflowGraph,
    by_id: HashMap<&'g str, &'g Node>,
    edge_set: HashSet<(&'g str, &'g str, EdgeKind)>,
    window: VecDeque<String>,
    parents: HashMap<String, Inherit>,
    nodes: BTreeMap<String, Mark>,
    edges: BTreeMap<EdgeRef, Mark>,
    legal: BTreeMap<String, LegalSet>,
    header: Header,
    not_in_graph: BTreeSet<String>,
    latest: Option<Latest>,
    latest_edges: Vec<EdgeRef>,
    upto: u64,
    events: u64,
}

impl<'g> Fold<'g> {
    pub(crate) fn new(graph: &'g WorkflowGraph) -> Self {
        Self {
            graph,
            by_id: graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect(),
            edge_set: graph
                .edges
                .iter()
                .map(|e| (e.from.as_str(), e.to.as_str(), e.kind))
                .collect(),
            window: VecDeque::new(),
            parents: HashMap::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            legal: BTreeMap::new(),
            header: Header::default(),
            not_in_graph: BTreeSet::new(),
            latest: None,
            latest_edges: Vec::new(),
            upto: 0,
            events: 0,
        }
    }

    /// The edge `from` → `to` of `kind`, if the graph has it.
    fn edge(&self, from: &str, to: &str, kind: EdgeKind) -> Option<EdgeRef> {
        self.edge_set.contains(&(from, to, kind)).then(|| EdgeRef {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        })
    }

    /// Fold the next event (`seq` order) and return its view.
    pub(crate) fn apply(&mut self, ev: &ExecutionEvent) -> EventView {
        let own = ev
            .node_id
            .as_deref()
            .and_then(|id| self.by_id.get(id).copied());
        let parent = ev
            .parent_event_id
            .as_deref()
            .and_then(|p| self.parents.get(p))
            .cloned()
            .unwrap_or_default();
        let facets = match own {
            Some(n) => n.facets.clone().or(&parent.facets),
            None => parent.facets.clone(),
        };
        let model = if ev.kind.starts_with("jev.") {
            ev.payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        }
        .or(parent.model.clone());
        let caller = match own {
            Some(n) if matches!(n.kind, NodeKind::Action | NodeKind::Feed | NodeKind::Agent) => {
                Some(n.id.clone())
            }
            _ => parent.caller.clone(),
        };
        let tone = ev.status.tone();
        let mark = Mark {
            tone,
            status: ev.status,
            event_kind: ev.kind.clone(),
            seq: ev.seq,
            event_id: ev.event_id.clone(),
            ts_ms: ev.ts_ms,
        };

        let edges = self.edges_of(ev, own, &facets, caller.as_deref());
        match (&ev.node_id, own) {
            (Some(id), Some(_)) => {
                self.nodes.insert(id.clone(), mark.clone());
            }
            (Some(id), None) => {
                self.not_in_graph.insert(id.clone());
            }
            (None, _) => {}
        }
        for e in &edges {
            self.edges.insert(e.clone(), mark.clone());
        }
        if ev.kind.starts_with("jev.") {
            if let Some(l) = &facets.loop_name {
                let set = self.legal_set(ev, l);
                self.legal.insert(l.clone(), set);
            }
        }
        self.header_of(ev, &facets);

        self.latest = Some(Latest {
            event_id: ev.event_id.clone(),
            seq: ev.seq,
            node_id: ev.node_id.clone(),
        });
        self.latest_edges.clone_from(&edges);
        self.upto = ev.seq;
        self.events += 1;
        self.remember(
            &ev.event_id,
            Inherit {
                facets: facets.clone(),
                model: model.clone(),
                caller,
            },
        );
        EventView {
            tone,
            facets,
            model,
            in_graph: own.is_some(),
            edges,
        }
    }

    /// Edges an `action.*` / `tool.*` event highlights (module table).
    fn edges_of(
        &self,
        ev: &ExecutionEvent,
        own: Option<&Node>,
        facets: &Facets,
        caller: Option<&str>,
    ) -> Vec<EdgeRef> {
        let mut out = Vec::new();
        if ev.kind.starts_with("action.") {
            let Some(l) = facets.loop_name.as_deref() else {
                return out;
            };
            let action = match own {
                Some(n) if n.kind == NodeKind::Action => Some(n.id.clone()),
                _ if ev.kind == "action.escalated" => ev
                    .payload
                    .get("action")
                    .and_then(Value::as_str)
                    .map(|a| node_id::action(l, a)),
                _ => None,
            };
            if let Some(a) = action {
                out.extend(self.edge(&node_id::jev(l), &a, EdgeKind::Chooses));
            }
            if ev.kind == "action.escalated" {
                if let Some(gate) = ev.node_id.as_deref() {
                    out.extend(self.edge(gate, &node_id::escalation(l), EdgeKind::Escalates));
                }
            }
        } else if ev.kind.starts_with("tool.") {
            if let (Some(tool), Some(from)) = (own.filter(|n| n.kind == NodeKind::Tool), caller) {
                out.extend(self.edge(from, &tool.id, EdgeKind::Calls));
            }
        } else if ev.kind.starts_with("step.") {
            if let Some(agent) = own.filter(|n| n.kind == NodeKind::Agent) {
                out.extend(self.edge(&node_id::planner(), &agent.id, EdgeKind::Delegates));
            }
        }
        out
    }

    /// Loop `l`'s legal set from its `jev.*` event.
    fn legal_set(&self, ev: &ExecutionEvent, l: &str) -> LegalSet {
        let legal: Option<Vec<String>> = ev
            .payload
            .get("legal_actions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
        let grey = match &legal {
            None => Vec::new(),
            Some(names) => self
                .graph
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Action)
                .filter(|n| n.facets.loop_name.as_deref() == Some(l))
                .filter(|n| {
                    n.facets
                        .action
                        .as_ref()
                        .is_some_and(|a| !names.iter().any(|x| x == a))
                })
                .map(|n| n.id.clone())
                .collect(),
        };
        LegalSet {
            loop_name: l.to_string(),
            event_id: ev.event_id.clone(),
            seq: ev.seq,
            step: ev.payload.get("step").and_then(Value::as_u64),
            legal_actions: legal,
            grey,
        }
    }

    fn header_of(&mut self, ev: &ExecutionEvent, facets: &Facets) {
        let h = &mut self.header;
        if h.run_id.is_none() {
            h.run_id = Some(ev.run_id.clone());
            h.runtime_id.clone_from(&ev.runtime_id);
            h.sandbox = Some(ev.sandbox.clone());
            h.config_hash.clone_from(&ev.config_hash);
        }
        if ev.kind == RUN_OPENED {
            h.kind = ev
                .payload
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some(state) = ev.kind.strip_prefix("runtime.") {
            h.runtime = Some(RuntimeSeen {
                state: state.to_string(),
                status: ev.status,
                tone: ev.status.tone(),
                event_id: ev.event_id.clone(),
            });
        }
        if ev.kind == "jev.completed" {
            if let Some(m) = ev.payload.get("model").filter(|m| m.is_string()) {
                h.model = Some(Seen {
                    value: m.clone(),
                    event_id: ev.event_id.clone(),
                });
            }
        }
        if ev.kind.starts_with("loop.") {
            if let (Some(l), Some(stats)) = (&facets.loop_name, ev.payload.get("stats")) {
                h.loops.insert(
                    l.clone(),
                    Seen {
                        value: stats.clone(),
                        event_id: ev.event_id.clone(),
                    },
                );
            }
        }
        if ev.kind == "trigger.map" {
            if let Some(sha) = ev
                .node_id
                .as_deref()
                .and_then(|n| n.strip_prefix("trigger:map/"))
            {
                h.map = Some(sha.to_string());
            }
        }
        h.closed = closes_run(&ev.kind, ev.status);
    }

    fn remember(&mut self, event_id: &str, inherit: Inherit) {
        self.parents.insert(event_id.to_string(), inherit);
        self.window.push_back(event_id.to_string());
        while self.window.len() > PARENT_WINDOW {
            if let Some(old) = self.window.pop_front() {
                self.parents.remove(&old);
            }
        }
    }

    /// The board after the events folded so far.
    pub(crate) fn board(&self) -> Board {
        let latest_id = self.latest.as_ref().map(|l| l.event_id.as_str());
        let grey: HashSet<&str> = self
            .legal
            .values()
            .flat_map(|s| s.grey.iter().map(String::as_str))
            .collect();
        let nodes = self
            .graph
            .nodes
            .iter()
            .map(|n| {
                let mark = self.nodes.get(&n.id).cloned();
                let latest =
                    mark.as_ref().map(|m| m.event_id.as_str()) == latest_id && latest_id.is_some();
                let (tone, why) = if n.narrowed_out {
                    (Some(Tone::Grey), Some("narrowed_out"))
                } else if grey.contains(n.id.as_str()) {
                    (Some(Tone::Grey), Some("not_legal"))
                } else if let Some(m) = &mark {
                    (Some(m.tone), Some("event"))
                } else {
                    (None, None)
                };
                NodeDraw {
                    node_id: n.id.clone(),
                    tone,
                    why,
                    mark,
                    latest,
                }
            })
            .collect();
        let edges = self
            .edges
            .iter()
            .map(|(e, m)| EdgeDraw {
                edge: e.clone(),
                mark: m.clone(),
                latest: self.latest_edges.contains(e),
            })
            .collect();
        Board {
            upto: self.upto,
            events: self.events,
            header: self.header.clone(),
            nodes,
            edges,
            legal: self.legal.values().cloned().collect(),
            not_in_graph: self.not_in_graph.iter().cloned().collect(),
            latest: self.latest.clone(),
        }
    }
}

/// Every event's view (seq ≤ `upto`, all when `None`) and the board after
/// the last one folded. `events` in `seq` order (`TraceReader::events`).
pub(crate) fn fold_run(
    graph: &WorkflowGraph,
    events: &[ExecutionEvent],
    upto: Option<u64>,
) -> (Vec<EventView>, Board) {
    let mut fold = Fold::new(graph);
    let views = events
        .iter()
        .take_while(|e| upto.map_or(true, |u| e.seq <= u))
        .map(|e| fold.apply(e))
        .collect();
    (views, fold.board())
}

/// The execution map a decide run ran (`trigger.map` root → its sha256).
pub(crate) fn run_map(events: &[ExecutionEvent]) -> Option<String> {
    events
        .iter()
        .filter(|e| e.kind == "trigger.map")
        .find_map(|e| e.node_id.as_deref()?.strip_prefix("trigger:map/"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::trace::{Component, EventDraft, RunContext};
    use serde_json::json;
    use std::path::Path;

    const RUN: &str = "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e";

    fn lab() -> WorkflowGraph {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/studio/graph-control-loop-lab.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Events of one run, `seq` 1…n; `parent` = an earlier index (1-based).
    struct Run {
        ctx: RunContext,
        evs: Vec<ExecutionEvent>,
    }

    impl Run {
        fn new() -> Self {
            Self {
                ctx: RunContext {
                    sandbox: "control-loop-lab".into(),
                    config_hash: Some("a".repeat(64)),
                    runtime_id: Some("host:1:0f0e0d0c-0b0a-4908-8706-050403020100".into()),
                    run_id: RUN.into(),
                },
                evs: Vec::new(),
            }
        }

        fn ev(
            &mut self,
            kind: &str,
            status: Status,
            node: Option<&str>,
            parent: Option<u64>,
            payload: Value,
        ) -> u64 {
            let seq = self.evs.len() as u64 + 1;
            let mut d = EventDraft::new(Component::Loop, kind, status).payload(payload);
            if let Some(n) = node {
                d = d.node(n);
            }
            if let Some(p) = parent {
                d = d.parent(format!("{RUN}:{p}"));
            }
            self.evs.push(self.ctx.stamp(seq, 1_000 + seq as i64, d));
            seq
        }
    }

    fn draw<'b>(b: &'b Board, id: &str) -> &'b NodeDraw {
        b.nodes.iter().find(|n| n.node_id == id).unwrap()
    }

    /// The act scenario of a tick: colours come from each node's latest
    /// status only, edges from action / tool events only, tool calls get
    /// their loop / action / model from the parent chain.
    #[test]
    fn colours_and_edges_from_events_only() {
        let g = lab();
        let mut r = Run::new();
        r.ev(
            RUN_OPENED,
            Status::Ok,
            Some("runtime:control-loop-lab"),
            None,
            json!({"kind": "run"}),
        );
        let fired = r.ev(
            "feed.fired",
            Status::Running,
            Some("feed:tick"),
            None,
            json!({}),
        );
        let started = r.ev(
            "loop.started",
            Status::Running,
            Some("loop:demo"),
            Some(fired),
            json!({"stats": {"in_flight": 1}}),
        );
        let jev = r.ev(
            "jev.completed",
            Status::Ok,
            Some("jev:demo"),
            Some(started),
            json!({"model": "typesafe/jev-1.13-20260917", "legal_actions": ["hold", "read_probe", "write_marker"]}),
        );
        let sel = r.ev(
            "action.selected",
            Status::Running,
            Some("action:demo/write_marker"),
            Some(jev),
            json!({}),
        );
        let ts = r.ev(
            "tool.started",
            Status::Running,
            Some("tool:lab/write_file"),
            Some(sel),
            json!({}),
        );
        r.ev(
            "tool.completed",
            Status::Ok,
            Some("tool:lab/write_file"),
            Some(ts),
            json!({}),
        );
        let (views, b) = fold_run(&g, &r.evs, None);

        // The tool call: the loop, the action, the feed and the model come down the chain.
        let tool = &views[6];
        assert_eq!(tool.facets.loop_name.as_deref(), Some("demo"));
        assert_eq!(tool.facets.action.as_deref(), Some("write_marker"));
        assert_eq!(tool.facets.feed.as_deref(), Some("tick"));
        assert_eq!(tool.facets.tool.as_deref(), Some("write_file"));
        assert_eq!(tool.model.as_deref(), Some("typesafe/jev-1.13-20260917"));
        assert_eq!(
            tool.edges,
            [EdgeRef {
                from: "action:demo/write_marker".into(),
                to: "tool:lab/write_file".into(),
                kind: EdgeKind::Calls
            }]
        );
        assert_eq!(
            views[4].edges,
            [EdgeRef {
                from: "jev:demo".into(),
                to: "action:demo/write_marker".into(),
                kind: EdgeKind::Chooses
            }]
        );
        // Neither a feed, a loop nor a Jev event highlights an edge.
        assert!(views[..4].iter().all(|v| v.edges.is_empty()));

        assert_eq!(
            draw(&b, "tool:lab/write_file").tone,
            Some(Tone::Green),
            "latest = completed"
        );
        assert!(draw(&b, "tool:lab/write_file").latest);
        assert_eq!(
            draw(&b, "action:demo/write_marker").tone,
            Some(Tone::Amber),
            "selected, running"
        );
        assert_eq!(draw(&b, "feed:tick").tone, Some(Tone::Amber));
        assert_eq!(draw(&b, "jev:demo").tone, Some(Tone::Green));
        assert_eq!(draw(&b, "action:demo/hold").tone, None, "no event, legal");
        assert_eq!(draw(&b, "scope:lab/write_file").tone, None);
        let calls = b
            .edges
            .iter()
            .find(|e| e.edge.kind == EdgeKind::Calls)
            .unwrap();
        assert_eq!((calls.mark.tone, calls.latest), (Tone::Green, true));
        assert_eq!(b.edges.len(), 2);
        assert_eq!(
            b.header.model.as_ref().unwrap().value,
            json!("typesafe/jev-1.13-20260917")
        );
        assert_eq!(b.header.loops["demo"].value, json!({"in_flight": 1}));
        assert_eq!(b.header.kind.as_deref(), Some("run"));
        assert_eq!(b.upto, 7);
        assert!(!b.header.closed);

        // A feed's tool call: the caller is the feed.
        let mut r = Run::new();
        let f = r.ev(
            "feed.fired",
            Status::Running,
            Some("feed:probe"),
            None,
            json!({}),
        );
        let t = r.ev(
            "tool.started",
            Status::Running,
            Some("tool:lab/read_file"),
            Some(f),
            json!({}),
        );
        r.ev(
            "tool.failed",
            Status::Failed,
            Some("tool:lab/read_file"),
            Some(t),
            json!({}),
        );
        let (views, b) = fold_run(&g, &r.evs, None);
        assert_eq!(views[2].edges[0].from, "feed:probe");
        assert_eq!(views[2].facets.loop_name, None);
        assert_eq!(draw(&b, "tool:lab/read_file").tone, Some(Tone::Red));
    }

    /// Grey = the loop's actions absent from its latest legal set; a
    /// below-confidence step highlights the picked action's edge amber;
    /// a node the graph lacks is listed, not drawn; `upto` cuts the fold.
    #[test]
    fn grey_is_the_latest_legal_set() {
        let g = lab();
        let mut r = Run::new();
        let root = r.ev(
            "trigger.map",
            Status::Running,
            Some(&format!("trigger:map/{}", "b".repeat(64))),
            None,
            json!({}),
        );
        let jev = r.ev(
            "jev.completed",
            Status::Ok,
            Some("jev:demo"),
            Some(root),
            json!({"legal_actions": ["hold", "write_marker"], "step": 1}),
        );
        r.ev(
            "action.escalated",
            Status::Escalated,
            Some("gate:demo/act_at"),
            Some(jev),
            json!({"action": "write_marker", "confidence": 0.5, "act_at": 0.8}),
        );
        r.ev(
            "trigger.completed",
            Status::Ok,
            Some(&format!("trigger:map/{}", "b".repeat(64))),
            Some(root),
            json!({}),
        );
        let (views, b) = fold_run(&g, &r.evs, None);
        assert_eq!(draw(&b, "action:demo/read_probe").tone, Some(Tone::Grey));
        assert_eq!(draw(&b, "action:demo/read_probe").why, Some("not_legal"));
        assert_eq!(draw(&b, "action:demo/hold").tone, None);
        assert_eq!(draw(&b, "gate:demo/act_at").tone, Some(Tone::Amber));
        assert_eq!(views[2].edges[0].to, "action:demo/write_marker");
        assert_eq!(b.edges[0].mark.tone, Tone::Amber);
        assert_eq!(
            b.legal[0].legal_actions.as_deref().unwrap(),
            ["hold", "write_marker"]
        );
        assert_eq!(b.legal[0].step, Some(1));
        assert_eq!(b.not_in_graph, [format!("trigger:map/{}", "b".repeat(64))]);
        assert!(!views[0].in_graph);
        assert_eq!(b.header.map, Some("b".repeat(64)));
        assert!(b.header.closed);
        assert_eq!(run_map(&r.evs), Some("b".repeat(64)));

        // A newer step offering everything un-greys it; a set the bound
        // dropped greys nothing.
        r.ev(
            "jev.completed",
            Status::Ok,
            Some("jev:demo"),
            Some(root),
            json!({"legal_actions": ["hold", "read_probe", "write_marker"]}),
        );
        assert_eq!(
            draw(&fold_run(&g, &r.evs, None).1, "action:demo/read_probe").tone,
            None
        );
        r.ev(
            "jev.completed",
            Status::Ok,
            Some("jev:demo"),
            Some(root),
            json!({"_dropped": ["legal", "legal_actions"]}),
        );
        let b = fold_run(&g, &r.evs, None).1;
        assert_eq!(b.legal[0].legal_actions, None);
        assert!(b.nodes.iter().all(|n| n.tone != Some(Tone::Grey)));

        // `upto`: the board as it was after seq 2.
        let (views, b) = fold_run(&g, &r.evs, Some(2));
        assert_eq!((views.len(), b.upto, b.events), (2, 2, 2));
        assert!(!b.header.closed);
        assert_eq!(draw(&b, "action:demo/read_probe").tone, Some(Tone::Grey));
    }

    /// The same events fold to the same board, whatever the parent window
    /// held; a skipped (dry-run) step is plain, a dropped tick amber.
    #[test]
    fn fold_is_deterministic() {
        let g = lab();
        let mut r = Run::new();
        let jev = r.ev(
            "jev.completed",
            Status::Ok,
            Some("jev:demo"),
            None,
            json!({"legal_actions": ["write_marker"]}),
        );
        r.ev(
            "action.selected",
            Status::Skipped,
            Some("action:demo/write_marker"),
            Some(jev),
            json!({}),
        );
        r.ev(
            "feed.dropped",
            Status::Dropped,
            Some("feed:tick"),
            None,
            json!({}),
        );
        r.ev(
            "runtime.stopped",
            Status::Ok,
            Some("runtime:control-loop-lab"),
            None,
            json!({}),
        );
        let a = fold_run(&g, &r.evs, None);
        let b = fold_run(&g, &r.evs, None);
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a.1).unwrap(),
            serde_json::to_string(&b.1).unwrap()
        );
        assert_eq!(
            draw(&a.1, "action:demo/write_marker").tone,
            Some(Tone::Plain)
        );
        assert_eq!(draw(&a.1, "feed:tick").tone, Some(Tone::Amber));
        assert_eq!(draw(&a.1, "action:demo/hold").tone, Some(Tone::Grey));
        let rt = a.1.header.runtime.as_ref().unwrap();
        assert_eq!((rt.state.as_str(), rt.tone), ("stopped", Tone::Green));
        assert!(a.1.header.closed);
        let order: Vec<&str> = a.1.nodes.iter().map(|n| n.node_id.as_str()).collect();
        let graph_order: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, graph_order, "graph order");
    }

    /// lping's orchestration on its golden graph: a webhook root, a plan
    /// that delegates one step whose `run-agent` tool call hangs under it.
    /// The planner, the agent and the tool take their latest event's tone;
    /// `planner → agent` (`delegates`) and `agent → tool` (`calls`) light;
    /// the run stays open (`webhook.responded` / `plan.completed` close
    /// nothing).
    #[test]
    fn orchestration_lights_planner_agent_and_tool() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/studio/graph-lping.json");
        let g: WorkflowGraph =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut r = Run::new();
        let root = r.ev(
            "trigger.webhook",
            Status::Running,
            Some("trigger:webhook/solana_events"),
            None,
            json!({"kind": "agent"}),
        );
        r.ev(
            "webhook.responded",
            Status::Ok,
            Some("trigger:webhook/solana_events"),
            Some(root),
            json!({"status_code": 202}),
        );
        let plan = r.ev(
            "plan.created",
            Status::Ok,
            Some("planner"),
            Some(root),
            json!({}),
        );
        let step = r.ev(
            "step.started",
            Status::Running,
            Some("agent:crypto_researcher"),
            Some(plan),
            json!({}),
        );
        let ts = r.ev(
            "tool.started",
            Status::Running,
            Some("tool:crypto_researcher/sol_price"),
            Some(step),
            json!({}),
        );
        r.ev(
            "tool.failed",
            Status::Failed,
            Some("tool:crypto_researcher/sol_price"),
            Some(ts),
            json!({}),
        );
        r.ev(
            "step.completed",
            Status::Ok,
            Some("agent:crypto_researcher"),
            Some(step),
            json!({}),
        );
        r.ev(
            "plan.completed",
            Status::Ok,
            Some("planner"),
            Some(plan),
            json!({}),
        );
        let (views, b) = fold_run(&g, &r.evs, None);
        assert!(b.not_in_graph.is_empty(), "{:?}", b.not_in_graph);
        for (id, tone) in [
            ("trigger:webhook/solana_events", Tone::Green),
            ("planner", Tone::Green),
            ("agent:crypto_researcher", Tone::Green),
            ("tool:crypto_researcher/sol_price", Tone::Red),
        ] {
            assert_eq!(draw(&b, id).tone, Some(tone), "{id}");
        }
        let lit: Vec<(&str, &str, EdgeKind, Tone)> = b
            .edges
            .iter()
            .map(|e| {
                (
                    e.edge.from.as_str(),
                    e.edge.to.as_str(),
                    e.edge.kind,
                    e.mark.tone,
                )
            })
            .collect();
        assert_eq!(
            lit,
            [
                (
                    "agent:crypto_researcher",
                    "tool:crypto_researcher/sol_price",
                    EdgeKind::Calls,
                    Tone::Red
                ),
                (
                    "planner",
                    "agent:crypto_researcher",
                    EdgeKind::Delegates,
                    Tone::Green
                ),
            ]
        );
        assert_eq!(views[5].facets.agent.as_deref(), Some("crypto_researcher"));
        assert!(!b.header.closed);
    }
}
