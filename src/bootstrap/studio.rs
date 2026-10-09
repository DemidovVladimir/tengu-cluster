//! Studio composition (`TENGU_STUDIO_PLAN.md` § 5): the inputs the Studio
//! use cases (`application/studio/`) need from adapters, and their redaction
//! before anything leaves the process.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`graph_inputs`] | each agent's catalog tools — `agent_base_tools` (`tools` allow-list + workspace opt-ins, `[generation]` filter), as every surface advertises them |
//! | [`workflow_graph`] | `build_graph` over [`graph_inputs`], every attr scrubbed as a trace payload is (`domain::trace::scrub_value`: the process `SecretRegistry`, then every URL → `<url>` — a `${VAR}` substituted into an arg never leaves, registered or not); a refused map lists every reason |
//! | `StudioContext` (`--features studio`) | what `tengu studio` serves, built once: the validated config, its redacted graph, the sandbox's trace reader (`<home>/logs/trace/<sandbox>/`), the runtime state dir; per request: a kept map's graph (`<home>/logs/maps/<sha256>.json`, re-hashed and re-applied to this config), the live verdict (`runtime::read_live`, as `tengu doctor --live`), the heartbeat holder (the live run), the evidence paths |

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
pub(crate) use context::{MapGraphError, StudioContext};

#[cfg(feature = "studio")]
mod context {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::Result;
    use serde::Serialize;

    use super::redacted_graph;
    use crate::adapters::outbound::runtime_store::read_heartbeat;
    use crate::adapters::outbound::trace_store::{trace_root, JsonlTraceReader};
    use crate::application::studio::stream::HolderFn;
    use crate::bootstrap::decision::{audit_file, maps_dir};
    use crate::bootstrap::runtime::{read_live, runner_name, LiveHealth};
    use crate::config::execution_map::ExecutionMap;
    use crate::config::paths::contract_tilde;
    use crate::config::Config;
    use crate::domain::runtime::heartbeat_file;
    use crate::domain::secrets::SecretRegistry;
    use crate::domain::workflow::WorkflowGraph;
    use crate::ports::trace::TraceReader;

    /// What `tengu studio` serves (module table).
    pub(crate) struct StudioContext {
        /// Runner name: the trace dir, the heartbeat file, the lease.
        pub sandbox: String,
        pub config: Config,
        pub secrets: Arc<SecretRegistry>,
        /// The base graph (no map), attrs redacted.
        pub graph: WorkflowGraph,
        pub reader: Arc<dyn TraceReader>,
        /// `TENGU_HOME`: `logs/trace`, `logs/maps`, `logs/decisions.jsonl`.
        pub home: PathBuf,
        /// Where the runtime's `run-<sandbox>.json` lives.
        pub state_dir: PathBuf,
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
            Ok(Self {
                sandbox,
                config,
                secrets,
                graph,
                reader,
                home: home.to_path_buf(),
                state_dir: state_dir.to_path_buf(),
            })
        }

        /// The graph narrowed by the kept map `sha256` — re-read, re-hashed
        /// and re-applied to this config (a map kept for an older config
        /// may be refused now).
        pub(crate) fn map_graph(&self, sha256: &str) -> Result<WorkflowGraph, MapGraphError> {
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
            redacted_graph(&self.config, Some(&map), &self.secrets).map_err(MapGraphError::Refused)
        }

        /// The live verdict `tengu doctor --live` prints.
        pub(crate) async fn health(&self) -> LiveHealth {
            read_live(&self.config, &self.state_dir).await
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
}
