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
//! | Signer: no `engine = "claude_code"` agent | the CLI runs its own shell with the full environment |
//! | Signer: no `[[mcp_servers]]` | foreign processes with our filesystem |
//! | Signer: no configured scope grants `shell_bins`; tools without a scope get a no-shell fallback (`AgentConfig::no_shell_fallback`, runtime) | a shell reads any file |
//! | Signer: the key file is absolute (or `~/…`) and outside every `fs_roots` and agent `workspace` | `read_file` / `list_directory` must not reach it |
//! | A write tool's scope with `wallets`: never in `[default_scopes]`; the agent has no `description`, is not `default`, is no webhook endpoint's `agent` | only a non-routable agent (decision loops, direct runs) may send |
//! | A loop action running a write tool with `read_only = true` sets `args.mode = "simulate"` | `read_only` bypasses `dry_run` |

use std::path::{Component, Path, PathBuf};

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

/// Lexical normalisation (no symlink resolution) of an absolute path.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Absolute, normalised, and symlink-resolved when the path exists.
fn resolved(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    std::fs::canonicalize(&abs).unwrap_or_else(|_| normalize(&abs))
}

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    signer_rules(cfg, &mut errors);
    wallet_grant_rules(cfg, &mut errors);
    loop_rules(cfg, &mut errors);
    errors
}

fn signer_rules(cfg: &Config, errors: &mut Vec<String>) {
    let Some(raw) = cfg.solana.signer_key_file.as_deref() else {
        return;
    };
    if raw.trim().is_empty() {
        errors.push("solana.signer_key_file must not be empty when set".into());
        return;
    }
    let key = expand_tilde(Path::new(raw));
    if !key.is_absolute() {
        errors.push(format!(
            "solana.signer_key_file `{raw}` must be absolute or start with `~/`"
        ));
        return;
    }
    let key = resolved(&key);

    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    for (id, agent) in &agents {
        if agent.engine == "claude_code" {
            errors.push(format!(
                "solana.signer_key_file: agents.{id} uses engine = \"claude_code\" — its CLI \
                 runs a shell with the full environment and could read the key; keep signing \
                 sandboxes free of Claude Code agents"
            ));
        }
        if let Some(ws) = &agent.workspace {
            let ws = resolved(&expand_tilde(ws));
            if key.starts_with(&ws) {
                errors.push(format!(
                    "solana.signer_key_file is inside agents.{id}.workspace `{}` — move the key \
                     outside every workspace",
                    ws.display()
                ));
            }
        }
    }
    if !cfg.mcp_servers.is_empty() {
        errors.push(
            "solana.signer_key_file: [[mcp_servers]] are not allowed in a signing sandbox \
             (foreign processes with this filesystem)"
                .into(),
        );
    }
    let default = cfg
        .default_scopes
        .iter()
        .map(|(t, s)| (format!("default_scopes.{t}"), s));
    let per_agent = agents.iter().flat_map(|(id, a)| {
        a.scopes
            .iter()
            .map(move |(t, s)| (format!("agents.{id}.scopes.{t}"), s))
    });
    let mut scopes: Vec<_> = default.chain(per_agent).collect();
    scopes.sort_by(|a, b| a.0.cmp(&b.0));
    for (at, scope) in scopes {
        if !scope.shell_bins.is_empty() {
            errors.push(format!(
                "solana.signer_key_file: {at}.shell_bins grants a shell — a signing sandbox runs no shell"
            ));
        }
        for root in &scope.fs_roots {
            let root = resolved(&expand_tilde(root));
            if key.starts_with(&root) {
                errors.push(format!(
                    "solana.signer_key_file is inside {at}.fs_roots `{}` — move the key outside \
                     every fs root",
                    root.display()
                ));
            }
        }
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

    #[test]
    fn signer_refuses_claude_code_mcp_and_shell() {
        let mut cfg = with_signer("/keys/signer.json");
        cfg.agents.get_mut("main").unwrap().engine = "claude_code".into();
        cfg.mcp_servers.push(crate::config::McpServerConfig {
            name: "x".into(),
            transport: "stdio".into(),
            command: vec!["x".into()],
            url: None,
            env: Default::default(),
            auth: None,
        });
        cfg.default_scopes.insert(
            "run_command".into(),
            scope(|s| s.shell_bins = vec!["ls".into()]),
        );
        let e = errs(&cfg);
        assert!(
            e.contains("agents.main uses engine = \"claude_code\""),
            "{e}"
        );
        assert!(e.contains("[[mcp_servers]] are not allowed"), "{e}");
        assert!(e.contains("default_scopes.run_command.shell_bins"), "{e}");
    }

    #[test]
    fn signer_key_must_be_outside_fs_roots_and_workspaces() {
        let mut cfg = with_signer("/home/op/ws/keys/../signer.json");
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/home/op/ws")]),
        );
        let e = errs(&cfg);
        assert!(
            e.contains("inside default_scopes.read_file.fs_roots"),
            "{e}"
        );

        let mut cfg = with_signer("/home/op/keys/signer.json");
        cfg.agents.get_mut("main").unwrap().workspace = Some(PathBuf::from("/home/op"));
        assert!(errs(&cfg).contains("inside agents.main.workspace"));

        let mut cfg = with_signer("/home/op/keys/signer.json");
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/home/op/ws")]),
        );
        cfg.agents.get_mut("main").unwrap().workspace = Some(PathBuf::from("/home/op/ws"));
        assert_eq!(errs(&cfg), "", "sibling of the workspace is fine");

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
