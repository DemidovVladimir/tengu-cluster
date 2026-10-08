//! Studio composition (`TENGU_STUDIO_PLAN.md` § 5): the inputs the Studio
//! use cases (`application/studio/`) need from adapters, and their redaction
//! before anything leaves the process.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`graph_inputs`] | each agent's catalog tools — `agent_base_tools` (`tools` allow-list + workspace opt-ins, `[generation]` filter), as every surface advertises them |
//! | [`workflow_graph`] | `build_graph` over [`graph_inputs`], every attr scrubbed as a trace payload is (`domain::trace::scrub_value`: the process `SecretRegistry`, then every URL → `<url>` — a `${VAR}` substituted into an arg never leaves, registered or not); a refused map lists every reason |

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
    let mut graph = build_graph(cfg, &graph_inputs(cfg), map)
        .map_err(|errs| anyhow!("execution map refused:\n- {}", errs.join("\n- ")))?;
    graph.map_attrs(|v| scrub_value(v, secrets));
    Ok(graph)
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
