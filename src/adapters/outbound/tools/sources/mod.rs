//! Source tool family (O2) — read-only evidence over the sandbox's source
//! store (`<TENGU_HOME>/state/<sources.state>/sources.db`,
//! `outbound/sources/store.rs`). Agents never fetch: the operator fills the
//! store with `tengu sources fetch` / `import` (`cli/sources.rs`). One
//! opt-in catalog row per tool; interfaces live in [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `evidence.rs` | `source_evidence` — `source_asof/1:<source\|all>:<event\|entity\|all>:<at ms>`: the as-of evidence packet (`application::sources::evidence_as_of`, `domain/source/packet.rs`) paged to its newest records, its text = the packet's fenced text |
//!
//! The plugin opens the workspace observation store (`open_observation_store`:
//! rows are recorded when `[recorder]` takes `source_asof/1`; ttl 0, never
//! cached). Each call opens `sources.db` only when it exists (a read creates
//! nothing). No `[sources]` ⇒ `sources_state_missing`. Scope: `fs_roots` = the
//! workspace (the observation store); no network, no env. `sources.db` sits in
//! the state dir, outside every fs root by design (the load refuses it
//! inside one): the tool's own state, never an agent path. Arguments parse
//! strictly with the xlab family's helpers (an unknown key is an error).

pub(crate) mod defs;
pub(crate) mod evidence;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::sources::sources_state_dir;
use crate::config::sections::SandboxSections;
use crate::config::sources::SourcesConfig;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Handles every family tool shares.
#[derive(Clone)]
pub(crate) struct SourcesShared {
    /// Observation store (records rows); `None` = nothing recorded.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The sandbox's sections (`[sources]` and its state dir).
    pub sandbox: Arc<SandboxSections>,
}

impl SourcesShared {
    /// The `[sources]` registry, or `sources_state_missing`.
    pub(crate) fn registry(&self) -> Result<&SourcesConfig> {
        sources_state_dir(&self.sandbox)?;
        self.sandbox.sources.as_deref().ok_or_else(|| {
            anyhow!("sources_state_missing: no [sources] section — no source registry to read")
        })
    }
}

/// Plugin grouping the source tools; each catalog row shares it.
pub(crate) struct SourcesPlugin;

#[async_trait]
impl ToolPlugin for SourcesPlugin {
    fn name(&self) -> &'static str {
        "sources"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let sections = &ctx.config.sandbox;
        let store = match open_observation_store(ctx.workspace, sections) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%error, "observation store unavailable; source rows are not recorded");
                None
            }
        };
        let shared = SourcesShared {
            store,
            sandbox: Arc::clone(sections),
        };
        Ok(evidence::tools(&shared))
    }
}
