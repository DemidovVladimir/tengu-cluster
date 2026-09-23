//! Sandbox resolution — picks `sandboxes/<name>/config.toml` over the base
//! config and installs its `[egress]` policy.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::config::Config;

/// Load a sandbox config if `--sandbox <name>` was given, otherwise use the default config.
///
/// Sandbox configs are loaded from `sandboxes/<name>/config.toml` relative to the
/// current working directory. Phase 7.2 — when a sandbox config is loaded, the
/// `Config.sandbox_name` runtime-only field is set to the resolved name so
/// downstream code (notably `SubprocessRunner` → `tengu run-agent` IPC) can
/// re-resolve the same config in the child process. Without this, child
/// subagents fall through to the default user config and lose sandbox-specific
/// scopes/secrets/MCP servers — concretely, http_request scope-denies in the
/// child even when the parent's sandbox allows it.
pub(crate) fn load_sandbox_or(sandbox: Option<String>, default: Config) -> Result<Config> {
    let cfg = match sandbox {
        None => default,
        Some(name) => {
            let path = PathBuf::from("sandboxes").join(&name).join("config.toml");
            let mut cfg = Config::load(&path).with_context(|| {
                format!("Failed to load sandbox '{}' from {}", name, path.display())
            })?;
            cfg.sandbox_name = Some(name);
            crate::adapters::outbound::egress::install(&cfg.egress)?;
            cfg
        }
    };
    // The effective policy is known only now (sandbox wins over the base
    // config). Children inherit it via `TENGU_EGRESS` and stay quiet — the
    // parent already printed the warning.
    if std::env::var_os("TENGU_AGENT_IPC").is_none() {
        crate::adapters::outbound::egress::policy().warn_if_proxy_unreachable();
    }
    Ok(cfg)
}
