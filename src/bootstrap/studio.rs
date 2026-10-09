//! Studio composition (`TENGU_STUDIO_PLAN.md` § 5): the inputs the Studio
//! use cases (`application/studio/`) need from adapters, and their redaction
//! before anything leaves the process.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`graph_inputs`] | each agent's catalog tools — `agent_base_tools` (`tools` allow-list + workspace opt-ins, `[generation]` filter), as every surface advertises them |
//! | [`workflow_graph`] | `build_graph` over [`graph_inputs`], every attr scrubbed as a trace payload is (`domain::trace::scrub_value`: the process `SecretRegistry`, then every URL → `<url>` — a `${VAR}` substituted into an arg never leaves, registered or not); a refused map lists every reason |
//! | `StudioContext` (`--features studio`) | what `tengu studio` serves, built once: the validated config, its redacted graph, the sandbox's trace reader (`<home>/logs/trace/<sandbox>/`), the runtime state dir, the scenarios Studio's control may send (`read_scenarios`: `<sandbox dir>/scenarios/*.json`, not the `*.map.json` maps, redacted); per request: a kept map's graph (`<home>/logs/maps/<sha256>.json`, re-hashed and re-applied to this config), the live verdict (`runtime::read_live`, as `tengu doctor --live`), the heartbeat holder (the live run) and, for the control, the heartbeat seen now (`seen_fn`: holder, pid, state, fresh), the evidence paths |

use anyhow::{anyhow, Result};

use crate::application::studio::graph::{build_graph, AgentTools};
use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::scrub_value;
use crate::domain::workflow::WorkflowGraph;

/// Each agent's catalog tools, by agent name.
pub(crate) fn graph_inputs(cfg: &Config) -> AgentTools {
    cfg.agents
        .iter()
        .map(|(name, agent)| {
            let tools = crate::bootstrap::tools::agent_base_tools(agent, true, cfg.memory.enabled);
            (name.clone(), tools)
        })
        .collect()
}

/// The sandbox's workflow graph, narrowed by `map`, attrs redacted.
pub(crate) fn workflow_graph(
    cfg: &Config,
    map: Option<&ExecutionMap>,
    secrets: &SecretRegistry,
) -> Result<WorkflowGraph> {
    redacted_graph(cfg, map, secrets)
        .map_err(|errs| anyhow!("execution map refused:\n- {}", errs.join("\n- ")))
}

/// [`workflow_graph`], a refused map's reasons as a list.
fn redacted_graph(
    cfg: &Config,
    map: Option<&ExecutionMap>,
    secrets: &SecretRegistry,
) -> Result<WorkflowGraph, Vec<String>> {
    let mut graph = build_graph(cfg, &graph_inputs(cfg), map)?;
    graph.map_attrs(|v| scrub_value(v, secrets));
    Ok(graph)
}

#[cfg(feature = "studio")]
pub(crate) use context::{MapGraphError, NodeError, StudioContext};

#[cfg(feature = "studio")]
mod context {
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::Result;
    use serde::Serialize;
    use serde_json::{json, Value};

    use super::{graph_inputs, redacted_graph};
    use crate::adapters::outbound::runtime_store::read_heartbeat;
    use crate::adapters::outbound::trace_store::{run_file, trace_root, JsonlTraceReader};
    use crate::application::studio::board::run_map;
    use crate::application::studio::control::{
        scenario_name, Scenario, Seen, SeenFn, MAX_SCENARIOS, MAX_SCENARIO_BYTES,
    };
    use crate::application::studio::graph::AgentTools;
    use crate::application::studio::inspect::{config_slice, node_evidence, EvidenceRef};
    use crate::application::studio::stream::HolderFn;
    use crate::bootstrap::decision::{audit_file, maps_dir};
    use crate::bootstrap::runtime::{read_live, runner_name, LiveHealth};
    use crate::config::execution_map::ExecutionMap;
    use crate::config::paths::contract_tilde;
    use crate::config::Config;
    use crate::domain::observation::now_ms;
    use crate::domain::runtime::heartbeat_file;
    use crate::domain::secrets::SecretRegistry;
    use crate::domain::trace::{scrub_value, ExecutionEvent};
    use crate::domain::workflow::{NodeKind, WorkflowGraph};
    use crate::ports::trace::TraceReader;

    /// What `tengu studio` serves (module table).
    pub(crate) struct StudioContext {
        /// Runner name: the trace dir, the heartbeat file, the lease.
        pub sandbox: String,
        pub config: Config,
        pub secrets: Arc<SecretRegistry>,
        /// The base graph (no map), attrs redacted.
        pub graph: WorkflowGraph,
        /// Each agent's catalog tools (the graph's input; the inspector's
        /// tool definitions).
        pub tools: AgentTools,
        pub reader: Arc<dyn TraceReader>,
        /// `TENGU_HOME`: `logs/trace`, `logs/maps`, `logs/decisions.jsonl`.
        pub home: PathBuf,
        /// Where the runtime's `run-<sandbox>.json` lives.
        pub state_dir: PathBuf,
        /// The events the page may send (`<sandbox dir>/scenarios/*.json`,
        /// redacted), read once: [`read_scenarios`].
        pub scenarios: Vec<Scenario>,
    }

    /// Why a kept map has no graph.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum MapGraphError {
        /// Not a 64-hex sha256.
        BadId,
        /// No `<sha256>.json` under `logs/maps/`.
        Missing,
        /// It does not parse, or does not hash to its name.
        Unreadable(String),
        /// `ExecutionMap::apply` refused it against this config.
        Refused(Vec<String>),
    }

    impl std::fmt::Display for MapGraphError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                MapGraphError::BadId => write!(f, "not a sha256"),
                MapGraphError::Missing => write!(f, "not kept under logs/maps/"),
                MapGraphError::Unreadable(why) => write!(f, "unreadable: {why}"),
                MapGraphError::Refused(why) => {
                    write!(f, "refused by this config: {}", why.join("; "))
                }
            }
        }
    }

    /// Why a node has no detail.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum NodeError {
        /// The graph (with the map, if one was named) has no such node.
        Missing,
        Map(MapGraphError),
    }

    /// The graph a recorded run is drawn on: the kept execution map's for a
    /// `tengu decide --map` run (re-applied to this config), else the base
    /// graph; `note` says why a map run fell back to the base graph.
    pub(crate) struct RunGraph<'a> {
        pub graph: Cow<'a, WorkflowGraph>,
        pub map: Option<String>,
        pub note: Option<String>,
    }

    /// Files a Studio user may open to check what the page shows (`~` for
    /// the home dir, as the graph's scopes print it).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub(crate) struct Evidence {
        pub trace_dir: String,
        pub heartbeat: String,
        pub decisions: String,
        pub maps_dir: String,
    }

    impl StudioContext {
        /// For this process's `TENGU_HOME` and the runtime state dir
        /// `tengu run` of this config uses.
        pub(crate) fn open(config: Config, secrets: Arc<SecretRegistry>) -> Result<Self> {
            let home = crate::config::paths::resolve_tengu_home();
            let state_dir = crate::bootstrap::runtime::runtime_state_dir(&config);
            Self::at(config, secrets, &home, &state_dir)
        }

        /// Under `home`, the runtime's heartbeat in `state_dir`.
        pub(crate) fn at(
            config: Config,
            secrets: Arc<SecretRegistry>,
            home: &Path,
            state_dir: &Path,
        ) -> Result<Self> {
            let sandbox = runner_name(&config);
            let reader = Arc::new(JsonlTraceReader::new(&trace_root(home), &sandbox)?);
            let graph = redacted_graph(&config, None, &secrets)
                .map_err(|e| anyhow::anyhow!("workflow graph: {}", e.join("; ")))?;
            let tools = graph_inputs(&config);
            let scenarios = read_scenarios(&config, &secrets);
            Ok(Self {
                sandbox,
                config,
                secrets,
                graph,
                tools,
                reader,
                home: home.to_path_buf(),
                state_dir: state_dir.to_path_buf(),
                scenarios,
            })
        }

        /// The map `tengu decide --map` kept as `<sha256>.json`, re-read and
        /// re-hashed.
        fn kept_map(&self, sha256: &str) -> Result<ExecutionMap, MapGraphError> {
            let hex = sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                return Err(MapGraphError::BadId);
            }
            let path = maps_dir(&self.home).join(format!("{sha256}.json"));
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(MapGraphError::Missing)
                }
                Err(e) => return Err(MapGraphError::Unreadable(e.to_string())),
            };
            let map = ExecutionMap::parse(&text).map_err(MapGraphError::Unreadable)?;
            if map.sha256() != sha256 {
                return Err(MapGraphError::Unreadable(format!(
                    "{sha256}.json hashes to {}",
                    map.sha256()
                )));
            }
            Ok(map)
        }

        /// The graph narrowed by the kept map `sha256` — re-read, re-hashed
        /// and re-applied to this config (a map kept for an older config
        /// may be refused now).
        pub(crate) fn map_graph(&self, sha256: &str) -> Result<WorkflowGraph, MapGraphError> {
            let map = self.kept_map(sha256)?;
            redacted_graph(&self.config, Some(&map), &self.secrets).map_err(MapGraphError::Refused)
        }

        /// The graph `events` (one run) are drawn on (`RunGraph`).
        pub(crate) fn run_graph(&self, events: &[ExecutionEvent]) -> RunGraph<'_> {
            let Some(sha) = run_map(events) else {
                return RunGraph {
                    graph: Cow::Borrowed(&self.graph),
                    map: None,
                    note: None,
                };
            };
            match self.map_graph(&sha) {
                Ok(g) => RunGraph {
                    graph: Cow::Owned(g),
                    map: Some(sha),
                    note: None,
                },
                Err(e) => RunGraph {
                    graph: Cow::Borrowed(&self.graph),
                    note: Some(format!(
                        "execution map {sha}: {e} — drawn on the base graph"
                    )),
                    map: Some(sha),
                },
            }
        }

        /// The inspector's view of one node (`GET /api/v1/nodes/<id>`): the
        /// graph node, the validated config behind it
        /// (`application::studio::inspect`), its edges, the files its facts
        /// live in — redacted like the graph (secrets, then URLs).
        pub(crate) fn node_detail(
            &self,
            node_id: &str,
            map: Option<&str>,
        ) -> Result<Value, NodeError> {
            let kept = map
                .map(|sha| self.kept_map(sha))
                .transpose()
                .map_err(NodeError::Map)?;
            let narrowed;
            let graph = match &kept {
                Some(m) => {
                    narrowed = redacted_graph(&self.config, Some(m), &self.secrets)
                        .map_err(|e| NodeError::Map(MapGraphError::Refused(e)))?;
                    &narrowed
                }
                None => &self.graph,
            };
            let node = graph.node(node_id).ok_or(NodeError::Missing)?;
            let slice = config_slice(&self.config, &self.tools, node, kept.as_ref());
            let edges_in: Vec<Value> = graph
                .edges
                .iter()
                .filter(|e| e.to == node_id)
                .map(|e| json!({"from": e.from, "kind": e.kind}))
                .collect();
            let edges_out: Vec<Value> = graph
                .edges
                .iter()
                .filter(|e| e.from == node_id)
                .map(|e| json!({"to": e.to, "kind": e.kind}))
                .collect();
            let mut v = json!({
                "node": node,
                "map": map,
                "source": if kept.is_some() {
                    "validated config (Config::load), narrowed by ExecutionMap::apply"
                } else {
                    "validated config (Config::load)"
                },
                "config": slice,
                "edges": {"in": edges_in, "out": edges_out},
                "evidence": self.node_files(node.kind, node_id, map, node_evidence(&self.config, node)),
            });
            scrub_value(&mut v, &self.secrets);
            Ok(v)
        }

        /// The files a node of `kind` is evidenced in (`~/…`).
        fn node_files(
            &self,
            kind: NodeKind,
            node_id: &str,
            map: Option<&str>,
            mut rows: Vec<EvidenceRef>,
        ) -> Vec<EvidenceRef> {
            let ev = self.evidence();
            let file = |label: &'static str, path: &str| EvidenceRef {
                label,
                path: path.to_string(),
                key: None,
            };
            let mut out = Vec::new();
            if matches!(kind, NodeKind::Runtime | NodeKind::Feed | NodeKind::Loop) {
                out.push(file("heartbeat (tengu doctor --live)", &ev.heartbeat));
            }
            if matches!(
                kind,
                NodeKind::Loop
                    | NodeKind::Jev
                    | NodeKind::Gate
                    | NodeKind::Action
                    | NodeKind::Escalation
                    | NodeKind::Tool
            ) {
                out.push(file("decision audit (decisions.jsonl)", &ev.decisions));
            }
            out.append(&mut rows);
            let sha = node_id.strip_prefix("trigger:map/").or(map);
            if let Some(sha) = sha {
                let path = contract_tilde(&maps_dir(&self.home).join(format!("{sha}.json")));
                out.push(file("kept execution map", &path));
            }
            out.push(file("trace recordings", &ev.trace_dir));
            out
        }

        /// The live verdict `tengu doctor --live` prints.
        pub(crate) async fn health(&self) -> LiveHealth {
            read_live(&self.config, &self.state_dir).await
        }

        /// The heartbeat now as Studio's control reads it
        /// (`application::studio::control::Seen`): holder, pid, state, fresh
        /// within `[runtime] heartbeat_stale_secs`.
        pub(crate) fn seen_fn(&self) -> SeenFn {
            let (dir, sandbox) = (self.state_dir.clone(), self.sandbox.clone());
            let stale_ms = (self.config.runtime.heartbeat_stale_secs as i64).saturating_mul(1000);
            Arc::new(move || {
                let hb = read_heartbeat(&dir, &sandbox).ok().flatten()?;
                Some(Seen {
                    fresh: now_ms().saturating_sub(hb.ts_ms) <= stale_ms,
                    holder: hb.holder,
                    pid: hb.pid,
                    state: hb.state,
                })
            })
        }

        /// The heartbeat's lease holder now (the live run's `runtime_id`).
        pub(crate) fn holder_fn(&self) -> HolderFn {
            let (dir, sandbox) = (self.state_dir.clone(), self.sandbox.clone());
            Arc::new(move || {
                read_heartbeat(&dir, &sandbox)
                    .ok()
                    .flatten()
                    .map(|hb| hb.holder)
            })
        }

        /// One run's trace file (`~/…`).
        pub(crate) fn run_file(&self, run_id: &str) -> String {
            contract_tilde(&run_file(&trace_root(&self.home), &self.sandbox, run_id))
        }

        pub(crate) fn evidence(&self) -> Evidence {
            let t = |p: PathBuf| contract_tilde(&p);
            Evidence {
                trace_dir: t(trace_root(&self.home).join(&self.sandbox)),
                heartbeat: t(self.state_dir.join(heartbeat_file(&self.sandbox))),
                decisions: t(audit_file(&self.home)),
                maps_dir: t(maps_dir(&self.home)),
            }
        }
    }

    /// The scenarios of the sandbox `config` was loaded from:
    /// `<its dir>/scenarios/<name>.json` (`control::scenario_name`; not the
    /// `*.map.json` maps), each a JSON object ≤ `MAX_SCENARIO_BYTES`, by
    /// name, at most `MAX_SCENARIOS`; shown redacted (`scrub_value`), sent
    /// as the file says (`Scenario::raw`, as `tengu decide --event`). A file
    /// that is none of that is skipped with a warn. No config file = none.
    pub(crate) fn read_scenarios(config: &Config, secrets: &SecretRegistry) -> Vec<Scenario> {
        let Some(dir) = config
            .loaded_from
            .as_deref()
            .and_then(Path::parent)
            .map(|d| d.join("scenarios"))
        else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                let name = scenario_name(&file)?.to_string();
                Some((name, e.path()))
            })
            .collect();
        found.sort();
        let mut out = Vec::new();
        for (name, path) in found {
            if out.len() == MAX_SCENARIOS {
                tracing::warn!(dir = %dir.display(), max = MAX_SCENARIOS, "more scenarios than Studio lists; the rest skipped");
                break;
            }
            let read = std::fs::metadata(&path)
                .map_err(|e| e.to_string())
                .and_then(|m| {
                    if m.len() > MAX_SCENARIO_BYTES {
                        Err(format!("larger than {MAX_SCENARIO_BYTES} bytes"))
                    } else {
                        std::fs::read_to_string(&path).map_err(|e| e.to_string())
                    }
                })
                .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
                .and_then(|v| {
                    v.is_object()
                        .then_some(v)
                        .ok_or_else(|| "not a JSON object".to_string())
                });
            match read {
                Ok(raw) => {
                    let mut event = raw.clone();
                    scrub_value(&mut event, secrets);
                    out.push(Scenario {
                        name,
                        file: contract_tilde(&path),
                        event,
                        raw,
                    });
                }
                Err(why) => {
                    tracing::warn!(file = %path.display(), %why, "scenario skipped");
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A registered secret anywhere in an attr is redacted, and every URL
    /// is hidden: one carrying an unregistered `${VAR}` value (an RPC key)
    /// never leaves either — the rule a trace payload gets.
    #[test]
    fn graph_attrs_are_redacted() {
        let mut cfg: Config = toml::from_str(
            r#"
            [agents.a]
            engine = "openrouter"
            model = "m"
            tools = ["http_request"]
            [decision_loops.l]
            goal = "g"
            agent = "a"
            [decision_loops.l.actions.hold]
            description = "stop"
            [decision_loops.l.actions.fetch]
            description = "fetch"
            tool = "http_request"
            read_only = true
            args = { method = "GET", url = "https://rpc.example/?api-key=unregistered-k1", headers = { auth = "Bearer sk-studio-secret-123" } }
            "#,
        )
        .unwrap();
        cfg.fold_default_scopes();
        let mut secrets = SecretRegistry::new();
        secrets.register("sk-studio-secret-123".into());
        let g = workflow_graph(&cfg, None, &secrets).unwrap();
        let text = serde_json::to_string(&g).unwrap();
        for leaked in ["sk-studio-secret-123", "unregistered-k1", "rpc.example"] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        let args = &g.node("action:l/fetch").unwrap().attrs["args"];
        assert_eq!(args["url"], json!("<url>"));
        assert_eq!(args["headers"]["auth"], json!("Bearer [REDACTED]"));
        assert_eq!(args["method"], json!("GET"));
        assert_eq!(g.sandbox, "default");
        assert_eq!(g.config_hash, None);
    }

    /// The inspector's detail is scrubbed as the graph is: a registered
    /// secret and an unregistered key in a URL never leave.
    #[cfg(feature = "studio")]
    #[test]
    fn node_detail_is_redacted() {
        let mut cfg: Config = toml::from_str(
            r#"
            [agents.a]
            engine = "openrouter"
            model = "m"
            tools = ["http_request"]
            [decision_loops.l]
            goal = "g"
            agent = "a"
            [decision_loops.l.actions.hold]
            description = "stop"
            [decision_loops.l.actions.fetch]
            description = "fetch"
            tool = "http_request"
            read_only = true
            args = { method = "GET", url = "https://rpc.example/?api-key=unregistered-k1", headers = { auth = "Bearer sk-studio-secret-123" } }
            "#,
        )
        .unwrap();
        cfg.fold_default_scopes();
        let mut secrets = SecretRegistry::new();
        secrets.register("sk-studio-secret-123".into());
        let tmp = tempfile::tempdir().unwrap();
        let ctx =
            StudioContext::at(cfg, std::sync::Arc::new(secrets), tmp.path(), tmp.path()).unwrap();
        let d = ctx.node_detail("action:l/fetch", None).unwrap();
        let text = d.to_string();
        for leaked in ["sk-studio-secret-123", "unregistered-k1", "rpc.example"] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        assert_eq!(d["config"]["value"]["args"]["url"], json!("<url>"));
        assert_eq!(
            d["config"]["value"]["args"]["headers"]["auth"],
            json!("Bearer [REDACTED]")
        );
        assert_eq!(
            ctx.node_detail("action:l/nope", None),
            Err(NodeError::Missing)
        );
    }
}
