//! `build_graph` — a validated `Config` + each agent's catalog tools (+ an
//! optional execution map) → the `WorkflowGraph` read model
//! (`domain/workflow.rs`). Facts only from the config structs and the
//! catalog; no legality, confidence or cap rule is re-derived here — the
//! decision loop's own events (trace, ST-12) say what happened.
//!
//! | Source | Nodes / edges |
//! |---|---|
//! | `[runtime]` | `runtime:<s>` (knobs) —owns→ every feed |
//! | `tengu decide` | `trigger:decide` —fires→ every loop |
//! | `[webhooks.endpoints.<e>] loop = "<l>"` | `trigger:webhook/<e>` —fires→ `loop:<l>` (auth kind only, never a secret) |
//! | `[decision_loops.<l>]` | `agent:<agent>` —owns→ `loop:<l>` —reads→ `world:<l>/<alias>` · —asks→ `jev:<l>` ←guards— `gate:<l>/act_at` (—escalates→ `escalation:<l>` when `escalate`) · `jev:<l>` —chooses→ `action:<l>/<a>` |
//! | `actions.<a>` | —calls→ `tool:<agent>/<tool>` ←guards— `scope:<agent>/<tool>` (a configured scope) · `gate:<l>/<a>/caps` —guards→ · `world` —guards→ (`requires`) · —binds→ from `{from}` / `{observation}` slots · `sequence` —next→ |
//! | `[feeds.<f>]` | tick: `feed:<f>` —fires→ `loop:<target>` · tool: `agent:<a>` —owns→ `feed:<f>` —calls→ `tool:<a>/<tool>` |
//! | `--map` | `ExecutionMap::apply` of its loop (refused ⇒ every reason); `trigger:map/<sha256>` —fires→ `loop:<l>`; a base action the map drops stays as a node with `narrowed_out` (grey, no `chooses` / `next`), its tool / scope / caps too when nothing kept uses them; a changed knob shows `map_changes: {knob: {base, map}}` |
//!
//! Attrs carry config values as validated (scope roots shown `~/…` like the
//! TOML); the caller redacts them (`bootstrap::studio::workflow_graph`).
//! `effect` of an action: `terminal` (no tool), `logged` (a write under
//! `dry_run`: never run — `DecisionLoopConfig::logs_only`, the rule the loop
//! applies), else `runs`. `in_catalog`
//! of a tool: it is one of the agent's catalog tools (`agent_base_tools`) —
//! an `[[mcp_servers]]` or shell-skill tool shows `false`. `facets` of a
//! node: the loop / action / feed / agent / tool it belongs to, from the
//! config (a tick feed: its target loop + that loop's agent; a tool or
//! scope: agent + tool only — a tool event gets its loop or feed from its
//! parent event, `application/studio/board.rs`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::config::decision_loop::{ActionConfig, DecisionLoopConfig, SlotConfig};
use crate::config::execution_map::ExecutionMap;
use crate::config::feeds::FeedKind;
use crate::config::Config;
use crate::domain::message::ToolDef;
use crate::domain::scope::ToolScope;
use crate::domain::workflow::{
    layer, node_id, Edge, EdgeKind, Facets, MapRef, Node, NodeKind, WorkflowGraph,
    WORKFLOW_SCHEMA_VERSION,
};

/// Each agent's catalog tools (`bootstrap::studio::graph_inputs`).
pub(crate) type AgentTools = BTreeMap<String, Vec<ToolDef>>;

/// The graph of `cfg` (module table); `Err` = every reason `map` is refused.
pub(crate) fn build_graph(
    cfg: &Config,
    tools: &AgentTools,
    map: Option<&ExecutionMap>,
) -> Result<WorkflowGraph, Vec<String>> {
    let mapped = match map {
        None => None,
        Some(m) => {
            let base = cfg.decision_loops.get(&m.loop_name).ok_or_else(|| {
                vec![format!(
                    "execution map: no [decision_loops.{}] block in this config",
                    m.loop_name
                )]
            })?;
            Some((m, m.apply(base)?))
        }
    };
    let sandbox = cfg
        .sandbox_name
        .clone()
        .unwrap_or_else(|| crate::config::sections::DEFAULT_SANDBOX.to_string());
    let mut b = Builder::new(cfg, tools);

    let runtime = node_id::runtime(&sandbox);
    let rt = &cfg.runtime;
    b.node(
        &runtime,
        NodeKind::Runtime,
        "tengu run",
        layer::RUNTIME,
        attrs([
            ("heartbeat_secs", json!(rt.heartbeat_secs)),
            ("heartbeat_stale_secs", json!(rt.heartbeat_stale_secs)),
            ("shutdown_grace_secs", json!(rt.shutdown_grace_secs)),
            ("max_decisions_in_flight", json!(rt.max_decisions_in_flight)),
            ("max_queued_per_loop", json!(rt.max_queued_per_loop)),
        ]),
    );

    let mut loops: Vec<&String> = cfg.decision_loops.keys().collect();
    loops.sort();
    if !loops.is_empty() {
        b.node(
            &node_id::trigger_decide(),
            NodeKind::Trigger,
            "tengu decide",
            layer::TRIGGER,
            attrs([(
                "command",
                json!("tengu decide --loop <name> | --map <file>"),
            )]),
        );
    }
    for name in loops {
        let base = &cfg.decision_loops[name];
        let narrowed = mapped
            .as_ref()
            .filter(|(m, _)| &m.loop_name == name)
            .map(|(_, dl)| dl);
        b.add_loop(name, base, narrowed);
    }

    let mut feeds: Vec<&String> = cfg.feeds.keys().collect();
    feeds.sort();
    for name in feeds {
        b.add_feed(&runtime, name);
    }

    let mut endpoints: Vec<(&String, &String)> = cfg
        .webhooks
        .endpoints
        .iter()
        .filter_map(|(e, ep)| ep.decision_loop.as_ref().map(|l| (e, l)))
        .collect();
    endpoints.sort();
    for (ep, l) in endpoints {
        let c = &cfg.webhooks.endpoints[ep];
        let auth = if c.auth_header_env.is_some() {
            "header"
        } else if c.secret_env.is_some() || c.secret.is_some() {
            "hmac"
        } else {
            "none"
        };
        let id = node_id::trigger_webhook(ep);
        let n = b.node(
            &id,
            NodeKind::Trigger,
            &format!("/webhooks/{ep}"),
            layer::TRIGGER,
            attrs([
                ("loop", json!(l)),
                ("auth", json!(auth)),
                ("enabled", json!(cfg.webhooks.enabled)),
            ]),
        );
        belongs(n, facets(Some(l), None, None, None, None));
        b.edge(&id, &node_id::loop_(l), EdgeKind::Fires);
    }

    let map_ref = mapped.as_ref().map(|(m, _)| {
        let sha = m.sha256();
        let id = node_id::trigger_map(&sha);
        let n = b.node(
            &id,
            NodeKind::Trigger,
            &format!("map {sha}"),
            layer::TRIGGER,
            attrs([("sha256", json!(sha)), ("loop", json!(m.loop_name))]),
        );
        belongs(n, facets(Some(&m.loop_name), None, None, None, None));
        b.edge(&id, &node_id::loop_(&m.loop_name), EdgeKind::Fires);
        MapRef {
            sha256: sha,
            loop_name: m.loop_name.clone(),
        }
    });

    let mut graph = b.finish(sandbox, cfg.source_sha256.clone(), map_ref);
    graph.normalize();
    Ok(graph)
}

fn attrs<const N: usize>(kv: [(&str, Value); N]) -> BTreeMap<String, Value> {
    kv.into_iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// A serializable config value as JSON (`Null` when it cannot be).
fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// A node's [`Facets`]: what it belongs to (names whole).
fn facets(
    loop_name: Option<&str>,
    action: Option<&str>,
    feed: Option<&str>,
    agent: Option<&str>,
    tool: Option<&str>,
) -> Facets {
    let own = |v: Option<&str>| v.map(str::to_string);
    Facets {
        loop_name: own(loop_name),
        action: own(action),
        feed: own(feed),
        agent: own(agent),
        tool: own(tool),
    }
}

/// Set `n`'s facets unless an earlier insertion did (first insertion wins,
/// like its attrs).
fn belongs(n: &mut Node, f: Facets) -> &mut Node {
    if n.facets.is_empty() {
        n.facets = f;
    }
    n
}

struct Builder<'a> {
    cfg: &'a Config,
    tools: &'a AgentTools,
    nodes: BTreeMap<String, Node>,
    edges: BTreeSet<Edge>,
    /// Next `order` per layer: first insertion wins.
    next: BTreeMap<u8, u16>,
}

impl<'a> Builder<'a> {
    fn new(cfg: &'a Config, tools: &'a AgentTools) -> Self {
        Self {
            cfg,
            tools,
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
            next: BTreeMap::new(),
        }
    }

    /// Insert `id` unless it exists (the first insertion keeps its attrs).
    fn node(
        &mut self,
        id: &str,
        kind: NodeKind,
        label: &str,
        layer: u8,
        attrs: BTreeMap<String, Value>,
    ) -> &mut Node {
        let next = &mut self.next;
        self.nodes.entry(id.to_string()).or_insert_with(|| {
            let order = next.entry(layer).or_insert(0);
            let n = Node {
                id: id.to_string(),
                kind,
                label: label.to_string(),
                layer,
                order: *order,
                attrs,
                narrowed_out: false,
                facets: Facets::default(),
            };
            *order = order.saturating_add(1);
            n
        })
    }

    fn edge(&mut self, from: &str, to: &str, kind: EdgeKind) {
        self.edges.insert(Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        });
    }

    fn add_agent(&mut self, agent: &str) -> String {
        let id = node_id::agent(agent);
        let a = self.cfg.agents.get(agent);
        let n = self.node(
            &id,
            NodeKind::Agent,
            agent,
            layer::LOOP,
            attrs([
                ("engine", json!(a.map(|a| &a.engine))),
                ("model", json!(a.map(|a| &a.model))),
                (
                    "workspace",
                    json!(a
                        .and_then(|a| a.workspace.as_ref())
                        .map(|w| w.display().to_string())),
                ),
            ]),
        );
        belongs(n, facets(None, None, None, Some(agent), None));
        id
    }

    /// `tool:<agent>/<tool>` + its configured scope.
    fn add_tool(&mut self, agent: &str, tool: &str) -> String {
        let id = node_id::tool(agent, tool);
        let in_catalog = self
            .tools
            .get(agent)
            .is_some_and(|ts| ts.iter().any(|t| t.name == tool));
        let n = self.node(
            &id,
            NodeKind::Tool,
            tool,
            layer::TOOL,
            attrs([
                ("tool", json!(tool)),
                ("agent", json!(agent)),
                ("in_catalog", json!(in_catalog)),
            ]),
        );
        belongs(n, facets(None, None, None, Some(agent), Some(tool)));
        let scope = self
            .cfg
            .agents
            .get(agent)
            .and_then(|a| a.scopes.get(tool))
            .map(scope_attrs);
        if let Some(scope) = scope {
            let sid = node_id::scope(agent, tool);
            let n = self.node(&sid, NodeKind::Scope, "scope", layer::GUARD, scope);
            belongs(n, facets(None, None, None, Some(agent), Some(tool)));
            self.edge(&sid, &id, EdgeKind::Guards);
        }
        id
    }

    fn add_loop(
        &mut self,
        name: &str,
        base: &DecisionLoopConfig,
        map: Option<&DecisionLoopConfig>,
    ) {
        let dl = map.unwrap_or(base);
        let lid = node_id::loop_(name);
        let mut la = attrs([
            ("goal", json!(dl.goal)),
            ("agent", json!(dl.agent)),
            ("model", json!(dl.model)),
            ("act_at", json!(dl.act_at)),
            ("dry_run", json!(dl.dry_run)),
            ("escalate", json!(dl.escalate)),
            ("max_steps", json!(dl.max_steps)),
            ("history", json!(dl.history)),
            ("timeout_secs", json!(dl.timeout_secs)),
            ("world_max_age_secs", json!(dl.world_max_age_secs)),
        ]);
        if !dl.sequence.is_empty() {
            la.insert("sequence".into(), to_json(&dl.sequence));
        }
        if !dl.event_reduce.is_empty() {
            la.insert("event_reduce".into(), to_json(&dl.event_reduce));
        }
        if let Some(m) = map {
            let mut changes = Map::new();
            let mut changed = |k: &str, was: Value, now: Value| {
                if was != now {
                    changes.insert(k.into(), json!({"base": was, "map": now}));
                }
            };
            changed("act_at", json!(base.act_at), json!(m.act_at));
            changed("dry_run", json!(base.dry_run), json!(m.dry_run));
            changed("max_steps", json!(base.max_steps), json!(m.max_steps));
            changed("sequence", to_json(&base.sequence), to_json(&m.sequence));
            changed("goal", json!(base.goal), json!(m.goal));
            if !changes.is_empty() {
                la.insert("map_changes".into(), Value::Object(changes));
            }
        }
        let n = self.node(&lid, NodeKind::Loop, name, layer::LOOP, la);
        belongs(
            n,
            facets(Some(name), None, None, Some(dl.agent.as_str()), None),
        );
        let agent = self.add_agent(&dl.agent);
        self.edge(&agent, &lid, EdgeKind::Owns);
        self.edge(&node_id::trigger_decide(), &lid, EdgeKind::Fires);

        for (alias, key) in &dl.world {
            let wid = node_id::world(name, alias);
            let n = self.node(
                &wid,
                NodeKind::World,
                alias,
                layer::WORLD,
                attrs([
                    ("key", json!(key)),
                    ("max_age_secs", json!(dl.world_max_age_secs)),
                ]),
            );
            belongs(
                n,
                facets(Some(name), None, None, Some(dl.agent.as_str()), None),
            );
            self.edge(&lid, &wid, EdgeKind::Reads);
        }

        let jev = node_id::jev(name);
        let n = self.node(
            &jev,
            NodeKind::Jev,
            &dl.model,
            layer::JEV,
            attrs([
                ("model", json!(dl.model)),
                ("timeout_secs", json!(dl.timeout_secs)),
                ("history", json!(dl.history)),
            ]),
        );
        belongs(
            n,
            facets(Some(name), None, None, Some(dl.agent.as_str()), None),
        );
        self.edge(&lid, &jev, EdgeKind::Asks);
        let gate = node_id::gate_act_at(name);
        let n = self.node(
            &gate,
            NodeKind::Gate,
            &format!("act_at {}", dl.act_at),
            layer::JEV,
            attrs([
                ("act_at", json!(dl.act_at)),
                ("escalate", json!(dl.escalate)),
            ]),
        );
        belongs(
            n,
            facets(Some(name), None, None, Some(dl.agent.as_str()), None),
        );
        self.edge(&gate, &jev, EdgeKind::Guards);
        if dl.escalate {
            let esc = node_id::escalation(name);
            let n = self.node(
                &esc,
                NodeKind::Escalation,
                "escalation",
                layer::ACTION,
                BTreeMap::new(),
            );
            belongs(
                n,
                facets(Some(name), None, None, Some(dl.agent.as_str()), None),
            );
            self.edge(&gate, &esc, EdgeKind::Escalates);
        }

        // Sequence steps first, in order; then every other action by name.
        let mut order: Vec<&String> = Vec::new();
        for s in &dl.sequence {
            if let Some((k, _)) = base.actions.get_key_value(&s.action) {
                if !order.contains(&k) {
                    order.push(k);
                }
            }
        }
        let mut rest: Vec<&String> = base.actions.keys().filter(|k| !order.contains(k)).collect();
        rest.sort();
        order.extend(rest);

        let mut dropped_tools: BTreeSet<String> = BTreeSet::new();
        let mut kept_tools: BTreeSet<String> = BTreeSet::new();
        for an in order {
            let kept = dl.actions.get(an);
            let a = kept.unwrap_or(&base.actions[an]);
            let aid = node_id::action(name, an);
            let mut aa = action_attrs(dl, a);
            if let Some(pos) = dl.sequence.iter().position(|s| &s.action == an) {
                aa.insert("step".into(), json!(pos + 1));
                aa.insert("optional".into(), json!(dl.sequence[pos].optional));
            }
            if let (Some(_), Some(k)) = (map, kept) {
                let b = &base.actions[an];
                if b.caps != k.caps {
                    aa.insert("caps_base".into(), to_json(&b.caps));
                }
            }
            let n = self.node(&aid, NodeKind::Action, an, layer::ACTION, aa);
            n.narrowed_out = kept.is_none();
            let tool = a.tool.as_deref();
            belongs(
                n,
                facets(Some(name), Some(an), None, Some(dl.agent.as_str()), tool),
            );
            if kept.is_some() {
                self.edge(&jev, &aid, EdgeKind::Chooses);
            }
            if !a.caps.is_empty() {
                let cid = node_id::gate_caps(name, an);
                let n = self.node(
                    &cid,
                    NodeKind::Gate,
                    "caps",
                    layer::GUARD,
                    attrs([("caps", to_json(&a.caps))]),
                );
                n.narrowed_out = kept.is_none();
                belongs(
                    n,
                    facets(Some(name), Some(an), None, Some(dl.agent.as_str()), None),
                );
                self.edge(&cid, &aid, EdgeKind::Guards);
            }
            for alias in a.requires.keys() {
                self.edge(&node_id::world(name, alias), &aid, EdgeKind::Guards);
            }
            for slot in a.slots.values() {
                match slot {
                    SlotConfig::FromHistory { from, .. } => {
                        self.edge(&node_id::action(name, from), &aid, EdgeKind::Binds)
                    }
                    SlotConfig::FromObservation { observation, .. } => {
                        self.edge(&node_id::world(name, observation), &aid, EdgeKind::Binds)
                    }
                    SlotConfig::Static(_) | SlotConfig::FromEvent { .. } => {}
                }
            }
            if let Some(t) = &a.tool {
                let tid = self.add_tool(&dl.agent, t);
                self.edge(&aid, &tid, EdgeKind::Calls);
                if kept.is_some() {
                    kept_tools.insert(t.clone());
                } else {
                    dropped_tools.insert(t.clone());
                }
            }
        }
        for w in dl.sequence.windows(2) {
            self.edge(
                &node_id::action(name, &w[0].action),
                &node_id::action(name, &w[1].action),
                EdgeKind::Next,
            );
        }
        // A tool only a dropped action calls is greyed with it (a feed or a
        // kept action of another loop that calls it un-greys it in `finish`).
        for t in dropped_tools.difference(&kept_tools) {
            for id in [node_id::tool(&dl.agent, t), node_id::scope(&dl.agent, t)] {
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.narrowed_out = true;
                }
            }
        }
    }

    fn add_feed(&mut self, runtime: &str, name: &str) {
        let feed = &self.cfg.feeds[name];
        let fid = node_id::feed(name);
        let fa = match to_json(feed) {
            Value::Object(o) => o.into_iter().collect(),
            _ => BTreeMap::new(),
        };
        // A tick feed belongs to its target loop (and that loop's agent,
        // whose store holds its health row); a tool feed to its agent + tool.
        let f = match feed.kind() {
            Ok(FeedKind::Tick) => {
                let target = feed.target.as_deref();
                let agent = target
                    .and_then(|t| self.cfg.decision_loops.get(t))
                    .map(|dl| dl.agent.as_str());
                facets(target, None, Some(name), agent, None)
            }
            _ => facets(
                None,
                None,
                Some(name),
                feed.agent.as_deref(),
                feed.tool.as_deref(),
            ),
        };
        let n = self.node(&fid, NodeKind::Feed, name, layer::TRIGGER, fa);
        belongs(n, f);
        self.edge(runtime, &fid, EdgeKind::Owns);
        match feed.kind() {
            Ok(FeedKind::Tick) => {
                if let Some(t) = &feed.target {
                    self.edge(&fid, &node_id::loop_(t), EdgeKind::Fires);
                }
            }
            Ok(FeedKind::Tool) => {
                if let (Some(agent), Some(tool)) = (&feed.agent, &feed.tool) {
                    let aid = self.add_agent(agent);
                    self.edge(&aid, &fid, EdgeKind::Owns);
                    let tid = self.add_tool(agent, tool);
                    self.edge(&fid, &tid, EdgeKind::Calls);
                }
            }
            Err(_) => {}
        }
    }

    fn finish(
        self,
        sandbox: String,
        config_hash: Option<String>,
        map: Option<MapRef>,
    ) -> WorkflowGraph {
        let mut nodes: BTreeMap<String, Node> = self.nodes;
        // A tool any kept action of another loop calls is not grey either.
        let live_callers: BTreeSet<String> = self
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter(|e| nodes.get(&e.from).is_some_and(|n| !n.narrowed_out))
            .map(|e| e.to.clone())
            .collect();
        for id in &live_callers {
            if let Some(n) = nodes.get_mut(id) {
                n.narrowed_out = false;
            }
            if let Some(scope) = id.strip_prefix("tool:").map(|rest| format!("scope:{rest}")) {
                if let Some(n) = nodes.get_mut(&scope) {
                    n.narrowed_out = false;
                }
            }
        }
        WorkflowGraph {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            sandbox,
            config_hash,
            map,
            nodes: nodes.into_values().collect(),
            edges: self.edges.into_iter().collect(),
        }
    }
}

fn action_attrs(dl: &DecisionLoopConfig, a: &ActionConfig) -> BTreeMap<String, Value> {
    let effect = match &a.tool {
        None => "terminal",
        Some(_) if dl.logs_only(a) => "logged",
        Some(_) => "runs",
    };
    let mut out = attrs([
        ("description", json!(a.description)),
        ("tool", json!(a.tool)),
        ("read_only", json!(a.read_only)),
        ("effect", json!(effect)),
    ]);
    if !a.args.is_null() {
        out.insert("args".into(), a.args.clone());
    }
    for (k, v) in [
        ("slots", (!a.slots.is_empty()).then(|| to_json(&a.slots))),
        ("caps", (!a.caps.is_empty()).then(|| to_json(&a.caps))),
        (
            "requires",
            (!a.requires.is_empty()).then(|| to_json(&a.requires)),
        ),
        ("reduce", (!a.reduce.is_empty()).then(|| to_json(&a.reduce))),
    ] {
        if let Some(v) = v {
            out.insert(k.into(), v);
        }
    }
    out
}

/// A configured scope's non-empty fields; roots as `~/…`.
fn scope_attrs(s: &ToolScope) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let roots: Vec<String> = s
        .fs_roots
        .iter()
        .map(|r| crate::config::paths::contract_tilde(r))
        .collect();
    for (k, v) in [
        ("fs_roots", roots),
        ("net_hosts", s.net_hosts.clone()),
        ("env_reads", s.env_reads.clone()),
        ("shell_bins", s.shell_bins.clone()),
        ("wallets", s.wallets.clone()),
    ] {
        if !v.is_empty() {
            out.insert(k.to_string(), json!(v));
        }
    }
    if out.is_empty() {
        out.insert("deny_all".into(), json!(true));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn sandbox(name: &str) -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("sandboxes")
            .join(name)
            .join("config.toml");
        let mut cfg = Config::load(&path).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        cfg.sandbox_name = Some(name.to_string());
        cfg
    }

    fn graph(cfg: &Config, map: Option<&ExecutionMap>) -> Result<WorkflowGraph, Vec<String>> {
        build_graph(cfg, &crate::bootstrap::studio::graph_inputs(cfg), map)
    }

    fn golden_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/studio")
            .join(format!("graph-{name}.json"))
    }

    /// The graph of `sandboxes/<name>` equals its golden byte for byte;
    /// `TENGU_REGEN_GOLDEN=1` rewrites it.
    fn assert_golden(name: &str) {
        let g = graph(&sandbox(name), None).unwrap();
        assert!(g.dangling_edges().is_empty(), "{:?}", g.dangling_edges());
        let text = format!("{}\n", serde_json::to_string_pretty(&g).unwrap());
        let path = golden_path(name);
        if std::env::var_os("TENGU_REGEN_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            want == text,
            "{} is stale (sandbox config, catalog or graph builder changed) — \
             regenerate with `TENGU_REGEN_GOLDEN=1 cargo test --bin tengu studio::graph`\n{text}",
            path.display()
        );
    }

    /// The lab: one loop, a tick feed, a probe feed — every node the
    /// runbook names, the read_probe tool shared by an action and a feed.
    #[test]
    fn golden_control_loop_lab() {
        assert_golden("control-loop-lab");
        let g = graph(&sandbox("control-loop-lab"), None).unwrap();
        assert_eq!(g.sandbox, "control-loop-lab");
        assert_eq!(g.config_hash.as_deref().map(str::len), Some(64));
        for id in [
            "runtime:control-loop-lab",
            "trigger:decide",
            "feed:tick",
            "feed:probe",
            "agent:lab",
            "loop:demo",
            "world:demo/tick",
            "jev:demo",
            "gate:demo/act_at",
            "action:demo/hold",
            "action:demo/write_marker",
            "action:demo/read_probe",
            "tool:lab/write_file",
            "tool:lab/read_file",
            "scope:lab/write_file",
        ] {
            assert!(g.node(id).is_some(), "{id}");
        }
        assert!(g.node("escalation:demo").is_none(), "escalate = false");
        let wm = g.node("action:demo/write_marker").unwrap();
        assert_eq!(wm.attrs["effect"], json!("runs"));
        assert_eq!(
            g.node("action:demo/hold").unwrap().attrs["effect"],
            json!("terminal")
        );
        assert_eq!(
            g.node("scope:lab/write_file").unwrap().attrs["fs_roots"],
            json!(["~/tengu-lab/control-loop-lab/out"])
        );
        assert_eq!(
            g.node("tool:lab/read_file").unwrap().attrs["in_catalog"],
            json!(true)
        );
        let has = |from: &str, to: &str, kind: EdgeKind| {
            g.edges
                .iter()
                .any(|e| e.from == from && e.to == to && e.kind == kind)
        };
        assert!(has("feed:tick", "loop:demo", EdgeKind::Fires));
        assert!(has("feed:probe", "tool:lab/read_file", EdgeKind::Calls));
        assert!(has(
            "action:demo/read_probe",
            "tool:lab/read_file",
            EdgeKind::Calls
        ));
        assert!(has(
            "jev:demo",
            "action:demo/write_marker",
            EdgeKind::Chooses
        ));
        assert!(g.nodes.iter().all(|n| !n.narrowed_out));
    }

    /// lping: four loops with `world`, `requires`, `sequence`, history /
    /// observation / event bindings, caps and a webhook trigger.
    #[test]
    fn golden_lping() {
        assert_golden("lping");
        let g = graph(&sandbox("lping"), None).unwrap();
        let has = |from: &str, to: &str, kind: EdgeKind| {
            g.edges
                .iter()
                .any(|e| e.from == from && e.to == to && e.kind == kind)
        };
        assert!(has(
            "trigger:webhook/helius",
            "loop:lp_watch",
            EdgeKind::Fires
        ));
        assert!(
            g.node("trigger:webhook/solana_events").is_none(),
            "agent endpoint"
        );
        assert_eq!(
            g.node("trigger:webhook/helius").unwrap().attrs["auth"],
            json!("header")
        );
        assert!(has(
            "world:lp_watch/price",
            "action:lp_watch/open_position",
            EdgeKind::Guards
        ));
        assert!(has(
            "action:lp_watch/fetch_pools",
            "action:lp_watch/open_position",
            EdgeKind::Binds
        ));
        assert!(has(
            "gate:lp_watch/open_position/caps",
            "action:lp_watch/open_position",
            EdgeKind::Guards
        ));
        assert!(has(
            "gate:lp_watch/act_at",
            "escalation:lp_watch",
            EdgeKind::Escalates
        ));
        assert_eq!(
            g.node("action:lp_watch/open_position").unwrap().attrs["effect"],
            json!("logged")
        );
        assert!(g.edges.iter().any(|e| e.kind == EdgeKind::Next));
        assert!(g.node("world:lp_watch/price").unwrap().attrs["key"]
            .as_str()
            .unwrap()
            .ends_with("So11111111111111111111111111111111111111112"));
    }

    fn lab_map(json: &str) -> ExecutionMap {
        ExecutionMap::parse(json).unwrap()
    }

    /// A map keeping only write_marker, raising act_at and turning dry-run
    /// on: read_probe is grey (its tool stays live: the probe feed uses
    /// it), the knobs show base → map, the map trigger fires the loop.
    #[test]
    fn map_narrows_and_greys_dropped_actions() {
        let cfg = sandbox("control-loop-lab");
        let m = lab_map(
            r#"{"loop":"demo","actions":["write_marker"],"act_at":1.0,"dry_run":true,
                "event":{"scenario":"act"}}"#,
        );
        let g = graph(&cfg, Some(&m)).unwrap();
        let sha = m.sha256();
        assert_eq!(g.map.as_ref().unwrap().sha256, sha);
        let trig = format!("trigger:map/{sha}");
        assert!(g
            .edges
            .iter()
            .any(|e| e.from == trig && e.to == "loop:demo" && e.kind == EdgeKind::Fires));
        let probe = g.node("action:demo/read_probe").unwrap();
        assert!(probe.narrowed_out);
        assert!(!g
            .edges
            .iter()
            .any(|e| e.to == probe.id && e.kind == EdgeKind::Chooses));
        assert!(!g.node("action:demo/write_marker").unwrap().narrowed_out);
        assert!(
            !g.node("action:demo/hold").unwrap().narrowed_out,
            "terminal kept"
        );
        assert!(
            !g.node("tool:lab/read_file").unwrap().narrowed_out,
            "feed uses it"
        );
        assert_eq!(
            g.node("action:demo/write_marker").unwrap().attrs["effect"],
            json!("logged")
        );
        let lp = g.node("loop:demo").unwrap();
        assert_eq!(lp.attrs["act_at"], json!(1.0));
        assert_eq!(
            lp.attrs["map_changes"]["act_at"],
            json!({"base": 0.8, "map": 1.0})
        );
        assert_eq!(
            lp.attrs["map_changes"]["dry_run"],
            json!({"base": false, "map": true})
        );
        assert!(lp.attrs["map_changes"].get("max_steps").is_none());

        // Without the probe feed the dropped action's tool goes grey too.
        let mut no_feed = cfg.clone();
        no_feed.feeds.clear();
        let g = graph(&no_feed, Some(&m)).unwrap();
        assert!(g.node("tool:lab/read_file").unwrap().narrowed_out);
        assert!(!g.node("tool:lab/write_file").unwrap().narrowed_out);
    }

    /// A widening map is refused with its reasons; an accepted one adds no
    /// node or edge beyond the base graph's but the map trigger and its
    /// `fires` edge.
    #[test]
    fn map_never_widens() {
        let cfg = sandbox("control-loop-lab");
        for (json, why) in [
            (r#"{"loop":"demo","act_at":0.5}"#, "may only raise it"),
            (r#"{"loop":"demo","max_steps":9}"#, "may only lower it"),
            (r#"{"loop":"demo","actions":["rm_rf"]}"#, "is not an action"),
            (r#"{"loop":"nope"}"#, "no [decision_loops.nope]"),
        ] {
            let errs = graph(&cfg, Some(&lab_map(json))).unwrap_err();
            assert!(errs.iter().any(|e| e.contains(why)), "{json}: {errs:?}");
        }
        let base = graph(&cfg, None).unwrap();
        let m = lab_map(r#"{"loop":"demo","actions":[],"max_steps":2,"dry_run":true}"#);
        let mapped = graph(&cfg, Some(&m)).unwrap();
        let trig = format!("trigger:map/{}", m.sha256());
        for n in &mapped.nodes {
            assert!(
                n.id == trig || base.node(&n.id).is_some(),
                "new node {}",
                n.id
            );
        }
        for e in &mapped.edges {
            assert!(e.from == trig || base.edges.contains(e), "new edge {e:?}");
        }
    }
}
