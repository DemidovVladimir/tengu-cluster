//! Studio inspector — the validated config behind one graph node
//! (`TENGU_STUDIO_PLAN.md` § 4 "Inspector": "selected node's validated
//! config"). Read from the parsed `Config` structs after `Config::load`
//! (defaults filled in, `[default_scopes]` folded, an execution map applied
//! by `ExecutionMap::apply`) — never from the TOML text, so a comment or a
//! key the schema dropped cannot reach the page. The caller redacts the
//! result (`bootstrap::studio`, as the graph's attrs) (feature `studio`).
//!
//! | Node kind | `section` · value |
//! |---|---|
//! | `runtime` | `runtime` · `RuntimeConfig` |
//! | `trigger` | webhook: `webhooks.endpoints.<e>` (agent, loop, auth env names, goal template; an inline `secret` only as `"<set>"`) · decide / map: none (the CLI; the map is the graph's `map`) |
//! | `feed` | `feeds.<f>` · `FeedConfig` |
//! | `agent` | `agents.<a>` · engine, model, description, workspace, tools, workspace_tools, skill_packages, limits, the scoped tool names |
//! | `loop` | `decision_loops.<l>` · `DecisionLoopConfig` (the map's when it narrows this loop), `actions` as names |
//! | `world` · `jev` · `gate` (`act_at`) · `escalation` | the loop's fields they read |
//! | `action` · `gate` (caps) | `decision_loops.<l>.actions.<a>` · `ActionConfig` (the map's when kept, else the base one: `narrowed_out`) · its `caps` |
//! | `scope` | `agents.<a>.scopes.<t>` · `ToolScope`, roots `~/…` |
//! | `tool` | `catalog.<t>` · the catalog `ToolDef` the agent gets (name, description, parameters), `in_catalog`, `allowed` (its `tools` allow-list) |
//!
//! Evidence the config names ([`node_evidence`]): the observation store a
//! node's rows live in (`<workspace>/.tengu/observations.db`) and the key —
//! `feed/1:<f>`, `loop/1:<l>`, a `world` key.

use std::borrow::Cow;

use serde::Serialize;
use serde_json::{json, Value};

use super::graph::AgentTools;
use crate::config::decision_loop::DecisionLoopConfig;
use crate::config::execution_map::ExecutionMap;
use crate::config::feeds::FeedKind;
use crate::config::paths::contract_tilde;
use crate::config::Config;
use crate::domain::scope::ToolScope;
use crate::domain::workflow::{Node, NodeKind};

/// One node's config (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Slice {
    /// The TOML table the value was validated from.
    pub section: String,
    pub value: Value,
}

/// A file a node's facts live in, and the record's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvidenceRef {
    pub label: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn to_json<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Loop `l` as the graph shows it: narrowed by `map` when it names `l`.
fn loop_cfg<'c>(
    cfg: &'c Config,
    map: Option<&ExecutionMap>,
    l: &str,
) -> Option<Cow<'c, DecisionLoopConfig>> {
    let base = cfg.decision_loops.get(l)?;
    match map.filter(|m| m.loop_name == l).map(|m| m.apply(base)) {
        Some(Ok(narrowed)) => Some(Cow::Owned(narrowed)),
        _ => Some(Cow::Borrowed(base)),
    }
}

fn slice(section: String, value: Value) -> Option<Slice> {
    Some(Slice { section, value })
}

/// The validated config behind `node` (module table); `None` for a node no
/// config section stands behind (`tengu decide`, a map trigger).
pub(crate) fn config_slice(
    cfg: &Config,
    tools: &AgentTools,
    node: &Node,
    map: Option<&ExecutionMap>,
) -> Option<Slice> {
    let f = &node.facets;
    let l = f.loop_name.as_deref();
    match node.kind {
        NodeKind::Runtime => slice("runtime".into(), to_json(&cfg.runtime)),
        NodeKind::Trigger => {
            let ep = node.id.strip_prefix("trigger:webhook/")?;
            let c = cfg.webhooks.endpoints.get(ep)?;
            slice(
                format!("webhooks.endpoints.{ep}"),
                json!({
                    "agent": c.agent,
                    "loop": c.decision_loop,
                    "auth_header_env": c.auth_header_env,
                    "secret_env": c.secret_env,
                    "secret": c.secret.as_ref().map(|_| "<set>"),
                    "goal_template": c.goal_template,
                    "listener_enabled": cfg.webhooks.enabled,
                }),
            )
        }
        NodeKind::Feed => {
            let name = f.feed.as_deref()?;
            slice(format!("feeds.{name}"), to_json(cfg.feeds.get(name)?))
        }
        NodeKind::Planner => {
            let o = cfg.orchestrator.as_ref()?;
            let routable: Vec<String> =
                crate::application::orchestrator::shared_files::routable_agents(&cfg.agents)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
            slice(
                "orchestrator".into(),
                json!({
                    "agent": o.agent,
                    "engine": o.engine,
                    "max_attempts_per_step": o.max_attempts_per_step,
                    "max_replans": o.max_replans,
                    "routable_agents": routable,
                }),
            )
        }
        NodeKind::Agent => {
            let name = f.agent.as_deref()?;
            let a = cfg.agents.get(name)?;
            let mut scoped: Vec<&String> = a.scopes.keys().collect();
            scoped.sort();
            slice(
                format!("agents.{name}"),
                json!({
                    "engine": a.engine,
                    "model": a.model,
                    "description": a.description,
                    "workspace": a.workspace.as_ref().map(|w| contract_tilde(w)),
                    "tools": a.tools,
                    "workspace_tools": a.workspace_tools,
                    "skill_packages": a.skill_packages,
                    "limits": to_json(&a.limits),
                    "scoped_tools": scoped,
                }),
            )
        }
        NodeKind::Loop => {
            let dl = loop_cfg(cfg, map, l?)?;
            let mut v = to_json(dl.as_ref());
            if let Some(o) = v.as_object_mut() {
                let mut names: Vec<&String> = dl.actions.keys().collect();
                names.sort();
                o.insert("actions".into(), json!(names));
            }
            slice(format!("decision_loops.{}", l?), v)
        }
        NodeKind::World => {
            let dl = loop_cfg(cfg, map, l?)?;
            let alias = node.label.as_str();
            slice(
                format!("decision_loops.{}.world.{alias}", l?),
                json!({
                    "key": dl.world.get(alias),
                    "world_max_age_secs": dl.world_max_age_secs,
                }),
            )
        }
        NodeKind::Jev => {
            let dl = loop_cfg(cfg, map, l?)?;
            slice(
                format!("decision_loops.{}", l?),
                json!({
                    "model": dl.model,
                    "timeout_secs": dl.timeout_secs,
                    "history": dl.history,
                    "max_steps": dl.max_steps,
                    "goal": dl.goal,
                }),
            )
        }
        NodeKind::Gate | NodeKind::Action => {
            let dl = loop_cfg(cfg, map, l?)?;
            let Some(a) = f.action.as_deref() else {
                return slice(
                    format!("decision_loops.{}", l?),
                    json!({"act_at": dl.act_at, "escalate": dl.escalate, "dry_run": dl.dry_run}),
                );
            };
            // A map drops actions: the base one stands behind a grey node.
            let base = cfg.decision_loops.get(l?)?;
            let ac = dl.actions.get(a).or_else(|| base.actions.get(a))?;
            if node.kind == NodeKind::Gate {
                return slice(
                    format!("decision_loops.{}.actions.{a}.caps", l?),
                    to_json(&ac.caps),
                );
            }
            let mut v = to_json(ac);
            if let Some(o) = v.as_object_mut() {
                o.insert("logs_only".into(), json!(dl.logs_only(ac)));
            }
            slice(format!("decision_loops.{}.actions.{a}", l?), v)
        }
        NodeKind::Escalation => {
            let dl = loop_cfg(cfg, map, l?)?;
            slice(
                format!("decision_loops.{}", l?),
                json!({"escalate": dl.escalate, "act_at": dl.act_at}),
            )
        }
        NodeKind::Scope => {
            let (agent, tool) = (f.agent.as_deref()?, f.tool.as_deref()?);
            let s = cfg.agents.get(agent)?.scopes.get(tool)?;
            slice(format!("agents.{agent}.scopes.{tool}"), scope_json(s))
        }
        NodeKind::Tool => {
            let (agent, tool) = (f.agent.as_deref()?, f.tool.as_deref()?);
            let def = tools
                .get(agent)
                .and_then(|ts| ts.iter().find(|t| t.name == tool));
            let allowed = cfg.agents.get(agent).map(|a| {
                a.tools.is_empty()
                    || a.tools.iter().any(|t| t == tool)
                    || a.workspace_tools.iter().any(|t| t == tool)
            });
            slice(
                format!("catalog.{tool}"),
                json!({
                    "name": tool,
                    "agent": agent,
                    "in_catalog": def.is_some(),
                    "allowed": allowed,
                    "description": def.map(|d| &d.description),
                    "parameters": def.map(|d| &d.parameters),
                }),
            )
        }
    }
}

/// A scope as configured, roots `~/…` (as the graph prints them).
fn scope_json(s: &ToolScope) -> Value {
    let mut v = to_json(s);
    if let Some(roots) = v.get_mut("fs_roots") {
        *roots = json!(s
            .fs_roots
            .iter()
            .map(|r| contract_tilde(r))
            .collect::<Vec<_>>());
    }
    v
}

/// `<workspace>/.tengu/observations.db` of agent `a`, `~/…`.
fn store_of(cfg: &Config, agent: &str) -> Option<String> {
    let ws = cfg.agents.get(agent)?.workspace.as_ref()?;
    Some(contract_tilde(&ws.join(".tengu").join("observations.db")))
}

/// The observation-store rows a node's facts live in (module doc).
pub(crate) fn node_evidence(cfg: &Config, node: &Node) -> Vec<EvidenceRef> {
    let f = &node.facets;
    let row = |agent: Option<&str>, key: String, label: &'static str| {
        agent
            .and_then(|a| store_of(cfg, a))
            .map(|path| EvidenceRef {
                label,
                path,
                key: Some(key),
            })
    };
    let item = match node.kind {
        NodeKind::Feed => f.feed.as_deref().and_then(|name| {
            // A tick feed's row is in its loop agent's store, a job feed's in
            // its job's agent's (its facets).
            let agent = match cfg.feeds.get(name).map(|c| c.kind()) {
                Some(Ok(FeedKind::Tick | FeedKind::Tool | FeedKind::Job)) => f.agent.as_deref(),
                _ => None,
            };
            row(agent, format!("feed/1:{name}"), "feed health row")
        }),
        NodeKind::Loop => f
            .loop_name
            .as_deref()
            .and_then(|l| row(f.agent.as_deref(), format!("loop/1:{l}"), "loop health row")),
        NodeKind::World => {
            let key = node.attrs.get("key").and_then(Value::as_str);
            key.and_then(|k| row(f.agent.as_deref(), k.to_string(), "observation row"))
        }
        _ => None,
    };
    item.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::studio::graph::build_graph;
    use std::path::Path;

    fn lab() -> Config {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/control-loop-lab/config.toml");
        let mut cfg = Config::load(&path).unwrap();
        cfg.sandbox_name = Some("control-loop-lab".into());
        cfg
    }

    /// Every lab node: a section from the validated structs (defaults the
    /// TOML never wrote are there), the map's narrowed loop for a map.
    #[test]
    fn slices_come_from_validated_structs() {
        let cfg = lab();
        let tools = crate::bootstrap::studio::graph_inputs(&cfg);
        let g = build_graph(&cfg, &tools, None).unwrap();
        let get = |id: &str| config_slice(&cfg, &tools, g.node(id).unwrap(), None);
        let lp = get("loop:demo").unwrap();
        assert_eq!(lp.section, "decision_loops.demo");
        assert_eq!(
            lp.value["timeout_secs"],
            json!(20),
            "a default, not in the TOML"
        );
        assert_eq!(
            lp.value["actions"],
            json!(["hold", "read_probe", "write_marker"])
        );
        let wm = get("action:demo/write_marker").unwrap();
        assert_eq!(wm.section, "decision_loops.demo.actions.write_marker");
        assert_eq!(wm.value["args"]["path"], json!("out/marker.txt"));
        assert_eq!(wm.value["logs_only"], json!(false));
        let sc = get("scope:lab/write_file").unwrap();
        assert_eq!(
            sc.value["fs_roots"],
            json!(["~/tengu-lab/control-loop-lab/out"])
        );
        let tool = get("tool:lab/write_file").unwrap();
        assert_eq!(tool.section, "catalog.write_file");
        assert_eq!(
            (
                tool.value["in_catalog"].clone(),
                tool.value["allowed"].clone()
            ),
            (json!(true), json!(true))
        );
        assert!(tool.value["parameters"].is_object());
        assert_eq!(get("feed:tick").unwrap().value["target"], json!("demo"));
        assert_eq!(
            get("runtime:control-loop-lab").unwrap().value["heartbeat_secs"],
            json!(2)
        );
        assert_eq!(
            get("world:demo/tick").unwrap().value["key"],
            json!("feed/1:tick")
        );
        assert!(get("trigger:decide").is_none());
        let agent = get("agent:lab").unwrap();
        assert_eq!(
            agent.value["workspace"],
            json!("~/tengu-lab/control-loop-lab")
        );
        assert_eq!(
            agent.value["scoped_tools"],
            json!(["list_directory", "read_file", "write_file"])
        );

        let m = ExecutionMap::parse(
            r#"{"loop":"demo","actions":["write_marker"],"act_at":1.0,"dry_run":true}"#,
        )
        .unwrap();
        let mg = build_graph(&cfg, &tools, Some(&m)).unwrap();
        let lp = config_slice(&cfg, &tools, mg.node("loop:demo").unwrap(), Some(&m)).unwrap();
        assert_eq!(
            (lp.value["act_at"].clone(), lp.value["dry_run"].clone()),
            (json!(1.0), json!(true))
        );
        assert_eq!(lp.value["actions"], json!(["hold", "write_marker"]));
        let probe = config_slice(
            &cfg,
            &tools,
            mg.node("action:demo/read_probe").unwrap(),
            Some(&m),
        )
        .unwrap();
        assert_eq!(
            probe.value["tool"],
            json!("read_file"),
            "a dropped action: the base one"
        );
        let wm = config_slice(
            &cfg,
            &tools,
            mg.node("action:demo/write_marker").unwrap(),
            Some(&m),
        )
        .unwrap();
        assert_eq!(wm.value["logs_only"], json!(true));

        let ev = node_evidence(&cfg, g.node("feed:tick").unwrap());
        assert_eq!(
            ev[0].path,
            "~/tengu-lab/control-loop-lab/.tengu/observations.db"
        );
        assert_eq!(ev[0].key.as_deref(), Some("feed/1:tick"));
        assert_eq!(
            node_evidence(&cfg, g.node("loop:demo").unwrap())[0]
                .key
                .as_deref(),
            Some("loop/1:demo")
        );
        assert!(node_evidence(&cfg, g.node("jev:demo").unwrap()).is_empty());
    }
}
