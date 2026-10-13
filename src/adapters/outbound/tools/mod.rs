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

pub(crate) mod a2a;
#[cfg(feature = "postgres_memory")]
pub(crate) mod agentic_memory;
pub(crate) mod args;
pub(crate) mod cache;
pub(crate) mod crypto;
pub(crate) mod http;
pub(crate) mod hyperliquid;
pub(crate) mod manage_skill;
pub(crate) mod memory;
pub(crate) mod schema_lint;
pub(crate) mod skill;
pub(crate) mod skill_lifecycle;
pub(crate) mod skill_resource;
pub(crate) mod soe;
pub(crate) mod solana;
pub(crate) mod sources;
pub(crate) mod view_skill;
pub(crate) mod workspace;
pub(crate) mod xlab;
pub(crate) mod xm;

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
            needs_memory: true, // built by the memory plugin only with a vector backend
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
        // Solana LP family: one opt-in row per tool, all sharing the
        // `solana` plugin (interfaces in `solana/defs.rs`).
        ToolEntry {
            opt_in: Some(names::SOL_PRICE),
            needs_memory: false,
            defs: || solana::defs_named(names::SOL_PRICE),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::DLMM_POOLS),
            needs_memory: false,
            defs: || solana::defs_named(names::DLMM_POOLS),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::DLMM_POOL),
            needs_memory: false,
            defs: || solana::defs_named(names::DLMM_POOL),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::DLMM_POSITIONS),
            needs_memory: false,
            defs: || solana::defs_named(names::DLMM_POSITIONS),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::JUP_PERPS),
            needs_memory: false,
            defs: || solana::defs_named(names::JUP_PERPS),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::SOLANA_WALLET),
            needs_memory: false,
            defs: || solana::defs_named(names::SOLANA_WALLET),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::SOLANA_TX),
            needs_memory: false,
            defs: || solana::defs_named(names::SOLANA_TX),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::LP_SNAPSHOT),
            needs_memory: false,
            defs: || solana::defs_named(names::LP_SNAPSHOT),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::LP_SWAP_PLAN),
            needs_memory: false,
            defs: || solana::defs_named(names::LP_SWAP_PLAN),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::HEDGE_DECIDE),
            needs_memory: false,
            defs: || solana::defs_named(names::HEDGE_DECIDE),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::LP_DECIDE),
            needs_memory: false,
            defs: || solana::defs_named(names::LP_DECIDE),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        // Solana write tools (phase 6b): same plugin; `mode = "send"` also
        // needs a wallet grant in the agent's scope (`config/solana.rs`).
        ToolEntry {
            opt_in: Some(names::SOLANA_CLOSE_TOKEN_ACCOUNTS),
            needs_memory: false,
            defs: || solana::defs_named(names::SOLANA_CLOSE_TOKEN_ACCOUNTS),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::JUPITER_SWAP),
            needs_memory: false,
            defs: || solana::defs_named(names::JUPITER_SWAP),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::DLMM_CLOSE_POSITION),
            needs_memory: false,
            defs: || solana::defs_named(names::DLMM_CLOSE_POSITION),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::DLMM_OPEN_POSITION),
            needs_memory: false,
            defs: || solana::defs_named(names::DLMM_OPEN_POSITION),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        ToolEntry {
            opt_in: Some(names::JUP_PERPS_ORDER),
            needs_memory: false,
            defs: || solana::defs_named(names::JUP_PERPS_ORDER),
            plugin: |_| Box::new(solana::SolanaPlugin),
        },
        // Hyperliquid family: one opt-in row per tool, all sharing the
        // `hyperliquid` plugin (interfaces in `hyperliquid/defs.rs`).
        ToolEntry {
            opt_in: Some(names::HL_CTX),
            needs_memory: false,
            defs: || hyperliquid::defs_named(names::HL_CTX),
            plugin: |_| Box::new(hyperliquid::HyperliquidPlugin),
        },
        ToolEntry {
            opt_in: Some(names::HL_BOOK),
            needs_memory: false,
            defs: || hyperliquid::defs_named(names::HL_BOOK),
            plugin: |_| Box::new(hyperliquid::HyperliquidPlugin),
        },
        // xmarket risk / paper family: one opt-in row per tool, all sharing
        // the `xm` plugin (interfaces in `xm/defs.rs`).
        ToolEntry {
            opt_in: Some(names::RISK_STATUS),
            needs_memory: false,
            defs: || xm::defs_named(names::RISK_STATUS),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        // Exec tools: the `[risk]` gate inside the tool (`xm/exec_common.rs`);
        // only a private agent may hold them (`config/risk.rs`).
        ToolEntry {
            opt_in: Some(names::PAPER_ORDER),
            needs_memory: false,
            defs: || xm::defs_named(names::PAPER_ORDER),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        ToolEntry {
            opt_in: Some(names::PAPER_CLOSE),
            needs_memory: false,
            defs: || xm::defs_named(names::PAPER_CLOSE),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        ToolEntry {
            opt_in: Some(names::XM_EXITS),
            needs_memory: false,
            defs: || xm::defs_named(names::XM_EXITS),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        ToolEntry {
            opt_in: Some(names::XM_WEEKEND_FADE),
            needs_memory: false,
            defs: || xm::defs_named(names::XM_WEEKEND_FADE),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        ToolEntry {
            opt_in: Some(names::PAPER_POSITIONS),
            needs_memory: false,
            defs: || xm::defs_named(names::PAPER_POSITIONS),
            plugin: |_| Box::new(xm::XmPlugin),
        },
        // xlab research family: one opt-in row per tool, all sharing the
        // `xlab` plugin (interfaces in `xlab/defs.rs`); read-only.
        ToolEntry {
            opt_in: Some(names::MARKET_HISTORY),
            needs_memory: false,
            defs: || xlab::defs_named(names::MARKET_HISTORY),
            plugin: |_| Box::new(xlab::XlabPlugin),
        },
        ToolEntry {
            opt_in: Some(names::BACKTEST),
            needs_memory: false,
            defs: || xlab::defs_named(names::BACKTEST),
            plugin: |_| Box::new(xlab::XlabPlugin),
        },
        ToolEntry {
            opt_in: Some(names::STRATEGY_RANKING),
            needs_memory: false,
            defs: || xlab::defs_named(names::STRATEGY_RANKING),
            plugin: |_| Box::new(xlab::XlabPlugin),
        },
        // Source family (O2): read-only evidence over `sources.db`
        // (interfaces in `sources/defs.rs`); agents never fetch.
        ToolEntry {
            opt_in: Some(names::SOURCE_EVIDENCE),
            needs_memory: false,
            defs: || sources::defs_named(names::SOURCE_EVIDENCE),
            plugin: |_| Box::new(sources::SourcesPlugin),
        },
        // SOE family (O3): one opt-in row per tool, all sharing the `soe`
        // plugin (interfaces in `soe/defs.rs`); one read, two stage writes
        // into the open run dir of the private SOE state.
        ToolEntry {
            opt_in: Some(names::SOE_VIEW),
            needs_memory: false,
            defs: || soe::defs_named(names::SOE_VIEW),
            plugin: |_| Box::new(soe::SoePlugin),
        },
        ToolEntry {
            opt_in: Some(names::SOE_PROPOSE),
            needs_memory: false,
            defs: || soe::defs_named(names::SOE_PROPOSE),
            plugin: |_| Box::new(soe::SoePlugin),
        },
        ToolEntry {
            opt_in: Some(names::SOE_CHALLENGE),
            needs_memory: false,
            defs: || soe::defs_named(names::SOE_CHALLENGE),
            plugin: |_| Box::new(soe::SoePlugin),
        },
        // A2A client (`a2a/`): another agent harness, one of the sandbox's
        // `[a2a.remotes.<name>]` only.
        ToolEntry {
            opt_in: Some(names::A2A),
            needs_memory: false,
            defs: a2a::tool_defs,
            plugin: |_| Box::new(a2a::A2aPlugin),
        },
    ]);
    rows
}

/// Tool definitions an agent sees: always-on rows, memory rows when
/// `has_memory`, and opt-in rows named in `workspace_tools`.
pub(crate) fn advertised_defs(has_memory: bool, workspace_tools: &[String]) -> Vec<ToolDef> {
    catalog()
        .into_iter()
        .filter(|row| {
            let wanted = match row.opt_in {
                Some(name) => workspace_tools.iter().any(|t| t == name),
                None => true,
            };
            // A memory row (opt-in too: `persistent_store`) is only built with
            // a vector backend — advertised without one, its calls failed.
            wanted && (has_memory || !row.needs_memory)
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

/// `ports::tool::ToolDirectory`: every catalog tool (all opt-ins included)
/// plus a live `tools/list` of each `[[mcp_servers]]` entry (fail-soft per
/// server).
pub(crate) struct CatalogDirectory {
    pub mcp_servers: Vec<crate::config::McpServerConfig>,
}

#[async_trait::async_trait]
impl crate::ports::tool::ToolDirectory for CatalogDirectory {
    async fn all_tool_defs(&self) -> Vec<ToolDef> {
        let opt_ins: Vec<String> = names::WORKSPACE_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut defs = advertised_defs(true, &opt_ins);
        defs.extend(
            crate::adapters::outbound::mcp_client::enumerate_tools(&self.mcp_servers).await,
        );
        defs
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

    /// `persistent_store` is advertised only with a vector backend: its
    /// plugin builds no tool without one.
    #[test]
    fn persistent_store_needs_a_memory_backend() {
        let opted = vec![names::PERSISTENT_STORE.to_string()];
        let names_of = |has_memory| -> Vec<String> {
            advertised_defs(has_memory, &opted)
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert!(!names_of(false).iter().any(|n| n == names::PERSISTENT_STORE));
        assert!(names_of(true).iter().any(|n| n == names::PERSISTENT_STORE));
    }

    /// `DEFAULT_TOOLS` (what an agent's `tools` may name, with the opt-ins) is
    /// exactly the default rows' tools + `compress_and_store`.
    #[test]
    fn default_rows_match_default_tools() {
        let mut defaults: Vec<String> = catalog()
            .iter()
            .filter(|r| r.opt_in.is_none())
            .flat_map(|r| (r.defs)())
            .map(|d| d.name)
            .collect();
        defaults.push("compress_and_store".into());
        defaults.sort();
        let mut listed: Vec<String> = names::DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect();
        listed.sort();
        assert_eq!(defaults, listed);
    }

    /// `XM_TOOLS` (the shared-workspace load rule, `config/xmarket.rs`) is
    /// exactly the opt-in rows of the `hyperliquid` and `xm` plugins.
    #[test]
    fn xm_tools_are_the_hyperliquid_and_xm_rows() {
        let opts = CatalogOpts {
            cancel: None,
            memory_config: None,
        };
        let mut rows: Vec<&str> = catalog()
            .iter()
            .filter(|r| matches!((r.plugin)(&opts).name(), "hyperliquid" | "xm"))
            .filter_map(|r| r.opt_in)
            .collect();
        rows.sort_unstable();
        let mut want = names::XM_TOOLS.to_vec();
        want.sort_unstable();
        assert_eq!(rows, want);
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

    #[tokio::test]
    async fn directory_lists_catalog_and_mcp_tools() {
        use crate::ports::tool::ToolDirectory;
        let dir = CatalogDirectory {
            mcp_servers: vec![crate::adapters::outbound::mcp_client::tests::fake_server(
                "fake",
            )],
        };
        let names: Vec<String> = dir
            .all_tool_defs()
            .await
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"http_request".to_string()));
        assert!(names.contains(&names::MANAGE_SKILL.to_string()));
        assert!(names.contains(&"fake__echo".to_string()), "{names:?}");
    }
}
