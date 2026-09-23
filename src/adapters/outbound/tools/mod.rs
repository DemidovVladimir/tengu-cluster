//! Tools — one directory per tool (or tool group). Each implements
//! `ports::tool::Tool` and is grouped by a `ToolPlugin`.
//!
//! ## Adding a tool
//!
//! 1. `tools/<name>/mod.rs`: the `Tool` impls, a `ToolPlugin`, and
//!    `tool_defs()`. First line of every `Tool::execute` is a
//!    `ctx.scope.check_*` call (or `// scope: pure-compute`) — enforced by
//!    `tests/scope_lint.rs`.
//! 2. One `ToolEntry` in [`catalog`] below. That single row drives
//!    registration in-process, the MCP bridge (Claude Code subagents), and the
//!    tool list advertised to the model.
//! 3. Opt-in only (`opt_in: Some(..)`): also add the name to
//!    `domain::tools::WORKSPACE_TOOLS` so config validation accepts it.
//!    `catalog_tests` fail if you forget.
//!
//! Then list the tool name in `[agents.<name>] tools = [...]` in the sandbox
//! config. See `docs/tools.md`.
//!
//! Not in the catalog: skill shell tools (`skill/`, built from SKILL.md) and
//! external MCP server tools (`outbound/mcp_client/`, from `[[mcp_servers]]`).

#[cfg(feature = "postgres_memory")]
pub(crate) mod agentic_memory;
pub(crate) mod args;
pub(crate) mod cache;
pub(crate) mod crypto;
pub(crate) mod http;
pub(crate) mod manage_skill;
pub(crate) mod memory;
pub(crate) mod skill;
pub(crate) mod skill_lifecycle;
pub(crate) mod skill_resource;
pub(crate) mod view_skill;
pub(crate) mod workspace;

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::application::tools::registry::ToolRegistry;
use crate::domain::message::ToolDef;
use crate::domain::tools as names;
use crate::ports::tool::{PluginCtx, ToolPlugin};

/// Inputs some plugins need at construction time.
pub(crate) struct CatalogOpts<'a> {
    pub cancel: Option<Arc<AtomicBool>>,
    pub memory_config: Option<&'a crate::config::MemoryConfig>,
}

/// One row of the tool catalog.
pub(crate) struct ToolEntry {
    /// `None`: always available. `Some(name)`: only when `name` is in the
    /// agent's `workspace_tools` (a `domain::tools::WORKSPACE_TOOLS` entry).
    pub opt_in: Option<&'static str>,
    /// Advertise only when the agent has memory enabled.
    pub needs_memory: bool,
    /// Definitions advertised to the model.
    pub defs: fn() -> Vec<ToolDef>,
    /// The plugin that executes them. Several rows may share one plugin
    /// (same `ToolPlugin::name`); it is registered once.
    pub plugin: fn(&CatalogOpts<'_>) -> Box<dyn ToolPlugin>,
}

fn memory_plugin(opts: &CatalogOpts<'_>) -> Box<dyn ToolPlugin> {
    let (size, overlap) = opts.memory_config.map_or((1000, 200), |mc| {
        (
            mc.persistent_store_chunk_size,
            mc.persistent_store_chunk_overlap,
        )
    });
    Box::new(memory::MemoryPlugin::new(size, overlap))
}

/// Every built-in tool, in the order it is advertised to the model.
pub(crate) fn catalog() -> Vec<ToolEntry> {
    let mut rows = vec![
        ToolEntry {
            opt_in: None,
            needs_memory: false,
            defs: workspace::tool_defs,
            plugin: |_| Box::new(workspace::WorkspacePlugin),
        },
        ToolEntry {
            opt_in: None,
            needs_memory: true,
            defs: memory::tool_defs,
            plugin: memory_plugin,
        },
        ToolEntry {
            opt_in: Some(names::SHARED_CACHE),
            needs_memory: false,
            defs: cache::tool_defs,
            plugin: |_| Box::new(cache::CachePlugin),
        },
    ];
    #[cfg(feature = "postgres_memory")]
    rows.push(ToolEntry {
        opt_in: Some(names::AGENTIC_MEMORY),
        needs_memory: false,
        defs: agentic_memory::tool_defs,
        plugin: |_| Box::new(agentic_memory::AgenticMemoryPlugin),
    });
    rows.extend([
        ToolEntry {
            opt_in: Some(names::PERSISTENT_STORE),
            needs_memory: false,
            defs: memory::persistent_store_tool_defs,
            plugin: memory_plugin,
        },
        ToolEntry {
            opt_in: Some(names::SKILL_DISTILL),
            needs_memory: false,
            defs: skill_lifecycle::distill_tool_defs,
            plugin: |_| Box::new(skill_lifecycle::SkillLifecyclePlugin),
        },
        ToolEntry {
            opt_in: Some(names::APPLY_IMPROVER_PROPOSAL),
            needs_memory: false,
            defs: skill_lifecycle::apply_improver_tool_defs,
            plugin: |_| Box::new(skill_lifecycle::SkillLifecyclePlugin),
        },
        ToolEntry {
            opt_in: Some(names::MANAGE_SKILL),
            needs_memory: false,
            defs: manage_skill::tool_defs,
            plugin: |_| Box::new(manage_skill::ManageSkillPlugin),
        },
        ToolEntry {
            opt_in: None,
            needs_memory: false,
            defs: http::tool_defs,
            plugin: |_| Box::new(http::HttpPlugin),
        },
        ToolEntry {
            opt_in: None,
            needs_memory: false,
            defs: crypto::tool_defs,
            plugin: |opts| Box::new(crypto::CryptoPlugin::new(opts.cancel.clone())),
        },
        ToolEntry {
            opt_in: None,
            needs_memory: false,
            defs: skill_resource::tool_defs,
            plugin: |_| Box::new(skill_resource::SkillResourcePlugin),
        },
        ToolEntry {
            opt_in: None,
            needs_memory: false,
            defs: view_skill::tool_defs,
            plugin: |_| Box::new(view_skill::ViewSkillPlugin),
        },
    ]);
    rows
}

/// Tool definitions an agent sees: always-on rows, memory rows when
/// `has_memory`, and opt-in rows named in `workspace_tools`.
pub(crate) fn advertised_defs(has_memory: bool, workspace_tools: &[String]) -> Vec<ToolDef> {
    catalog()
        .into_iter()
        .filter(|row| match row.opt_in {
            Some(name) => workspace_tools.iter().any(|t| t == name),
            None => has_memory || !row.needs_memory,
        })
        .flat_map(|row| (row.defs)())
        .collect()
}

/// Register every catalog plugin whose row is always-on or opted in via
/// `allowed_names`. `register_plugin` then keeps only the tools in
/// `allowed_list`. Each plugin is registered once. `compress_and_store` has
/// no plugin — the runner intercepts it; `SkillPlugin` / `McpPlugin` are
/// registered by callers that have their inputs.
pub(crate) async fn register_catalog(
    registry: &mut ToolRegistry,
    ctx: &PluginCtx<'_>,
    allowed_names: &HashSet<String>,
    allowed_list: &[String],
    opts: CatalogOpts<'_>,
) {
    let mut registered: HashSet<&'static str> = HashSet::new();
    for row in catalog() {
        if row.opt_in.is_some_and(|name| !allowed_names.contains(name)) {
            continue;
        }
        let plugin = (row.plugin)(&opts);
        if !registered.insert(plugin.name()) {
            continue;
        }
        if let Err(e) = registry
            .register_plugin(plugin.as_ref(), ctx, allowed_list)
            .await
        {
            tracing::warn!(plugin = plugin.name(), error = %e, "tool catalog: plugin failed");
        }
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    #[test]
    fn opt_in_rows_match_workspace_tools() {
        let rows = catalog();
        let opt_ins: Vec<&str> = rows.iter().filter_map(|r| r.opt_in).collect();
        for name in &opt_ins {
            assert!(
                names::WORKSPACE_TOOLS.contains(name),
                "catalog opt-in '{name}' missing from domain::tools::WORKSPACE_TOOLS"
            );
        }
        for name in names::WORKSPACE_TOOLS {
            if !cfg!(feature = "postgres_memory") && *name == names::AGENTIC_MEMORY {
                continue;
            }
            assert!(
                opt_ins.contains(name),
                "WORKSPACE_TOOLS '{name}' has no catalog row"
            );
        }
    }

    #[test]
    fn opt_in_name_is_a_tool_the_row_defines() {
        for row in catalog() {
            if let Some(name) = row.opt_in {
                let defs = (row.defs)();
                assert!(
                    defs.iter().any(|d| d.name == name),
                    "opt-in '{name}' is not among its row's tool definitions"
                );
            }
        }
    }

    #[test]
    fn advertised_defs_gates_memory_and_opt_ins() {
        let names_of =
            |defs: Vec<ToolDef>| -> Vec<String> { defs.into_iter().map(|d| d.name).collect() };
        let base = names_of(advertised_defs(false, &[]));
        assert!(base.contains(&"http_request".to_string()));
        assert!(!base.contains(&"memory_search".to_string()));
        assert!(!base.contains(&names::SHARED_CACHE.to_string()));

        let full = names_of(advertised_defs(
            true,
            &[
                names::SHARED_CACHE.to_string(),
                names::MANAGE_SKILL.to_string(),
            ],
        ));
        assert!(full.contains(&"memory_search".to_string()));
        assert!(full.contains(&names::SHARED_CACHE.to_string()));
        assert!(full.contains(&names::MANAGE_SKILL.to_string()));
        assert!(!full.contains(&names::SKILL_DISTILL.to_string()));
    }
}
