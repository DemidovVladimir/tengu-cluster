//! Studio composition (`TENGU_STUDIO_PLAN.md` § 5): the inputs the Studio
//! use cases (`application/studio/`) need from adapters, and their redaction
//! before anything leaves the process.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`graph_inputs`] | each agent's catalog tools — `agent_base_tools` (`tools` allow-list + workspace opt-ins, `[generation]` filter), as every surface advertises them |
//! | [`workflow_graph`] | `build_graph` over [`graph_inputs`], every attr redacted with the process `SecretRegistry` (a `${VAR}` value substituted into an arg never leaves); a refused map lists every reason |

use anyhow::{anyhow, Result};

use crate::application::studio::graph::{build_graph, AgentTools};
use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
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
    graph.map_attrs(|v| secrets.redact_value(v));
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A secret substituted into an action arg is redacted in the graph.
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
            args = { method = "GET", url = "https://x.example/?key=sk-studio-secret-123" }
            "#,
        )
        .unwrap();
        cfg.fold_default_scopes();
        let mut secrets = SecretRegistry::new();
        secrets.register("sk-studio-secret-123".into());
        let g = workflow_graph(&cfg, None, &secrets).unwrap();
        let text = serde_json::to_string(&g).unwrap();
        assert!(!text.contains("sk-studio-secret-123"), "{text}");
        assert_eq!(
            g.node("action:l/fetch").unwrap().attrs["args"]["url"],
            json!("https://x.example/?key=[REDACTED]")
        );
        assert_eq!(g.sandbox, "default");
        assert_eq!(g.config_hash, None);
    }
}
