//! `[solana]` — the signing key of the Solana write tools (phase 6b) and the
//! sandbox rules that keep it private. Doc:
//! `docs/typed-observations-2026-09-24.md` § Write tools.
//!
//! ```toml
//! [solana]
//! signer_key_file = "~/.tengu/keys/lping-signer.json"   # chmod 600
//! ```
//!
//! | Rule (load time — any violation fails `Config::load`) | Why |
//! |---|---|
//! | Signer: a hardened sandbox (`config/hardening.rs`, shared with `[risk]`) — `claude_code` agents only with `builtin_tools_profile = "none"`, no `[[mcp_servers]]`, no scope granting `shell_bins` (tools without a scope get a no-shell fallback), the key, `<TENGU_HOME>/state` and the config file outside every `fs_roots` and agent `workspace` | built-in tools, foreign processes and a shell ignore tengu scopes; `read_file` / `list_directory` must not reach the key |
//! | Signer: the key file is set, absolute (or `~/…`) | one file for the parent, `run-agent` children and the bridge |
//! | A write tool's scope with `wallets`: never in `[default_scopes]`; the agent has no `description`, is not `default`, is no webhook endpoint's `agent` | only a non-routable agent (decision loops, direct runs) may send |
//! | A loop action running a write tool with `read_only = true` sets `args.mode = "simulate"` | `read_only` bypasses `dry_run` |

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::paths::expand_tilde;
use super::Config;
use crate::domain::tools::SOLANA_WRITE_TOOLS;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolanaConfig {
    /// Signing key of the write tools' `mode = "send"`: a solana-keygen JSON
    /// array or a base58 64-byte key, mode 0600. Absent = every write tool
    /// is simulate-only.
    #[serde(default)]
    pub signer_key_file: Option<String>,
}

impl SolanaConfig {
    /// Expanded key path, when a signer is configured.
    pub(crate) fn signer_path(&self) -> Option<PathBuf> {
        self.signer_key_file
            .as_deref()
            .map(|p| expand_tilde(Path::new(p)))
    }
}

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    signer_rules(cfg, &mut errors);
    wallet_grant_rules(cfg, &mut errors);
    loop_rules(cfg, &mut errors);
    errors
}

/// The key path itself; where it may live (outside every fs root and
/// workspace) is a hardened-sandbox rule (`config/hardening.rs`).
fn signer_rules(cfg: &Config, errors: &mut Vec<String>) {
    let Some(raw) = cfg.solana.signer_key_file.as_deref() else {
        return;
    };
    if raw.trim().is_empty() {
        errors.push("solana.signer_key_file must not be empty when set".into());
        return;
    }
    if !expand_tilde(Path::new(raw)).is_absolute() {
        errors.push(format!(
            "solana.signer_key_file `{raw}` must be absolute or start with `~/`"
        ));
    }
}

fn wallet_grant_rules(cfg: &Config, errors: &mut Vec<String>) {
    for tool in SOLANA_WRITE_TOOLS {
        if cfg
            .default_scopes
            .get(*tool)
            .is_some_and(|s| !s.wallets.is_empty())
        {
            errors.push(format!(
                "default_scopes.{tool}.wallets: signing grants go on one agent \
                 ([agents.<name>.scopes.{tool}]), never in [default_scopes]"
            ));
        }
    }
    let webhook_agents: Vec<&str> = cfg
        .webhooks
        .endpoints
        .values()
        .map(|e| e.agent.as_str())
        .collect();
    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    for (id, agent) in agents {
        let grants: Vec<&str> = SOLANA_WRITE_TOOLS
            .iter()
            .copied()
            .filter(|t| agent.scopes.get(*t).is_some_and(|s| !s.wallets.is_empty()))
            .collect();
        if grants.is_empty() {
            continue;
        }
        let tools = grants.join(", ");
        if agent.description.is_some() {
            errors.push(format!(
                "agents.{id} may sign ({tools}) but has a `description` — a signing agent must \
                 not be planner-routable"
            ));
        }
        if agent.default {
            errors.push(format!(
                "agents.{id} may sign ({tools}) but is the default chat agent"
            ));
        }
        if webhook_agents.contains(&id.as_str()) {
            errors.push(format!(
                "agents.{id} may sign ({tools}) but is a webhook endpoint's `agent`"
            ));
        }
    }
}

fn loop_rules(cfg: &Config, errors: &mut Vec<String>) {
    let mut loops: Vec<_> = cfg.decision_loops.iter().collect();
    loops.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dl) in loops {
        for (an, action) in &dl.actions {
            let Some(tool) = action.tool.as_deref() else {
                continue;
            };
            if action.read_only
                && SOLANA_WRITE_TOOLS.contains(&tool)
                && action.args.get("mode").and_then(|m| m.as_str()) != Some("simulate")
            {
                errors.push(format!(
                    "decision_loops.{name}.actions.{an}: `{tool}` with read_only = true runs even \
                     under dry_run — set args.mode = \"simulate\" or drop read_only"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::scope::ToolScope;

    fn base() -> Config {
        let mut cfg = Config::default();
        cfg.agents.get_mut("main").unwrap().default = false;
        cfg
    }

    fn with_signer(key: &str) -> Config {
        let mut cfg = base();
        cfg.solana.signer_key_file = Some(key.into());
        cfg
    }

    fn scope(f: impl FnOnce(&mut ToolScope)) -> ToolScope {
        let mut s = ToolScope::default();
        f(&mut s);
        s
    }

    fn errs(cfg: &Config) -> String {
        validation_errors(cfg).join("\n")
    }

    #[test]
    fn no_signer_no_rules() {
        let mut cfg = base();
        cfg.agents.get_mut("main").unwrap().engine = "claude_code".into();
        cfg.default_scopes.insert(
            "run_command".into(),
            scope(|s| s.shell_bins = vec!["*".into()]),
        );
        assert_eq!(errs(&cfg), "");
    }

    /// The key path must be set and absolute; where it may live, and the
    /// no-MCP / no-shell rules, are hardened-sandbox rules
    /// (`config/hardening.rs` tests).
    #[test]
    fn signer_key_path_must_be_absolute() {
        let mut cfg = with_signer("/keys/signer.json");
        cfg.agents.get_mut("main").unwrap().engine = "claude_code".into();
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/keys")]),
        );
        assert_eq!(errs(&cfg), "", "hardening.rs owns those");
        assert!(errs(&with_signer("keys/signer.json")).contains("must be absolute"));
        assert!(errs(&with_signer("  ")).contains("must not be empty"));
    }

    #[test]
    fn wallet_grants_only_on_a_private_agent() {
        let grant =
            || scope(|s| s.wallets = vec!["AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9".into()]);
        let mut cfg = base();
        cfg.default_scopes.insert("jupiter_swap".into(), grant());
        assert!(errs(&cfg).contains("default_scopes.jupiter_swap.wallets"));

        let mut cfg = base();
        let main = cfg.agents.get_mut("main").unwrap();
        main.scopes.insert("dlmm_open_position".into(), grant());
        main.description = Some("routable".into());
        main.default = true;
        cfg.webhooks.endpoints.insert(
            "helius".into(),
            serde_json::from_value(serde_json::json!({"agent": "main"})).unwrap(),
        );
        let e = errs(&cfg);
        assert!(e.contains("has a `description`"), "{e}");
        assert!(e.contains("is the default chat agent"), "{e}");
        assert!(e.contains("webhook endpoint's `agent`"), "{e}");

        let mut cfg = base();
        cfg.agents
            .get_mut("main")
            .unwrap()
            .scopes
            .insert("dlmm_open_position".into(), grant());
        assert_eq!(errs(&cfg), "", "private agent may hold the grant");
    }

    #[test]
    fn read_only_write_action_must_simulate() {
        let loop_toml = |args: &str| {
            let mut cfg = base();
            let dl: crate::config::decision_loop::DecisionLoopConfig = toml::from_str(&format!(
                r#"
                goal = "g"
                agent = "main"
                [actions.swap]
                description = "swap"
                tool = "jupiter_swap"
                read_only = true
                args = {args}
                "#
            ))
            .unwrap();
            cfg.decision_loops.insert("l".into(), dl);
            errs(&cfg)
        };
        assert!(
            loop_toml(r#"{ mode = "send" }"#).contains("read_only = true runs even under dry_run")
        );
        assert!(loop_toml("{ }").contains("read_only = true"));
        assert_eq!(loop_toml(r#"{ mode = "simulate" }"#), "");
    }
}
