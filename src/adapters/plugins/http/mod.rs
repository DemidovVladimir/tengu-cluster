// src/adapters/plugins/http/mod.rs
//! HTTP plugin — generic outbound HTTP client for skill-driven API calls.
//!
//! Provides: `http_request`. Gated by `ctx.scope.check_net_host()` per call.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod request;

pub(crate) use request::HttpRequestTool;

/// Tool definitions advertised by the HTTP plugin.
///
/// Used by `channel_runtime::compute_base_tools` and `compute_bridge_tools` to
/// advertise HTTP tools to the engine before the plugin is instantiated.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![HttpRequestTool::new().definition().clone()]
}

/// Plugin grouping the HTTP primitive tools.
pub(crate) struct HttpPlugin;

#[async_trait]
impl ToolPlugin for HttpPlugin {
    fn name(&self) -> &'static str {
        "http"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(HttpRequestTool::new())])
    }
}
