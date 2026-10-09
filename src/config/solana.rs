//! `[solana]` — who signs the Solana write tools' `mode = "send"` (phase 6b)
//! and the sandbox rules that keep that signer private. Doc:
//! `docs/typed-observations-2026-09-24.md` § Write tools.
//!
//! ```toml
//! [solana]
//! privy_wallet_id = "<Privy Solana wallet id>"   # not a secret
//!
//! [keys.env]
//! PRIVY_API_URL = "privy"                         # signs through the seal proxy
//! ```
//!
//! The wallet is a Privy server wallet; Privy signs through the seal
//! proxy's `privy` route (`docs/sealed-keys-2026-10-09.md`), so no key and
//! no app secret is on this machine (`outbound/solana/privy.rs`).
//!
//! | Rule (load time — any violation fails `Config::load`) | Why |
//! |---|---|
//! | Signer: a hardened sandbox (`config/hardening.rs`, shared with `[risk]`) — `claude_code` agents only with `builtin_tools_profile = "none"`, no `[[mcp_servers]]`, no scope granting `shell_bins` (tools without a scope get a no-shell fallback), `<TENGU_HOME>/state` and the config file outside every `fs_roots` and agent `workspace` | built-in tools, foreign processes and a shell ignore tengu scopes |
//! | Signer: `privy_wallet_id` 1-64 chars `[A-Za-z0-9_-]`; `[keys.env] PRIVY_API_URL` set to a route | the id goes into a URL path; Privy is only reached through the proxy |
//! | Signer: no scope's `env_reads` names the session (`TENGU_KEYS_SESSION_TOKEN`, a `[keys.env]` `@session` var) or `"*"` | the session can sign with the wallet through the proxy — an agent must never hold it |
//! | A write tool's scope with `wallets`: never in `[default_scopes]`; the agent has no `description`, is not `default`, is no webhook endpoint's `agent` | only a non-routable agent (decision loops, direct runs) may send |
//! | A loop action running a write tool with `read_only = true` sets `args.mode = "simulate"` | `read_only` bypasses `dry_run` |

use serde::{Deserialize, Serialize};

use super::keys::{is_route_target, SESSION_VALUE};
use super::Config;
use crate::domain::solana_write::valid_privy_wallet_id;
use crate::domain::tools::SOLANA_WRITE_TOOLS;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolanaConfig {
    /// Privy Solana wallet that signs the write tools' `mode = "send"`
    /// (Privy's wallet id, not a secret). Absent = every write tool is
    /// simulate-only.
    #[serde(default)]
    pub privy_wallet_id: Option<String>,
}

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    signer_rules(cfg, &mut errors);
    wallet_grant_rules(cfg, &mut errors);
    loop_rules(cfg, &mut errors);
    errors
}

/// The wallet id, the proxy route it signs through, and no agent holding
/// the session that can use that route.
fn signer_rules(cfg: &Config, errors: &mut Vec<String>) {
    let Some(id) = cfg.solana.privy_wallet_id.as_deref() else {
        return;
    };
    if !valid_privy_wallet_id(id) {
        errors.push(format!(
            "solana.privy_wallet_id `{id}` must be 1-64 chars of A-Z a-z 0-9 _ -"
        ));
    }
    if !cfg
        .keys
        .env
        .get("PRIVY_API_URL")
        .is_some_and(|v| v != SESSION_VALUE && is_route_target(v))
    {
        errors.push(
            "solana.privy_wallet_id needs [keys.env] PRIVY_API_URL = \"privy\" — Privy signs \
             through the seal proxy only"
                .into(),
        );
    }
    let session = cfg.keys.session_vars();
    let mut scopes: Vec<(String, &crate::domain::scope::ToolScope)> = cfg
        .default_scopes
        .iter()
        .map(|(t, s)| (format!("default_scopes.{t}"), s))
        .collect();
    for (id, agent) in &cfg.agents {
        scopes.extend(
            agent
                .scopes
                .iter()
                .map(|(t, s)| (format!("agents.{id}.scopes.{t}"), s)),
        );
    }
    scopes.sort_by(|a, b| a.0.cmp(&b.0));
    for (at, scope) in scopes {
        for name in &scope.env_reads {
            if name == "*" || session.contains(&name.as_str()) {
                errors.push(format!(
                    "{at}.env_reads: '{name}' hands an agent the seal-proxy session, which can \
                     sign with the Privy wallet ([solana] privy_wallet_id)"
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

    fn with_signer(id: &str) -> Config {
        let mut cfg = base();
        cfg.solana.privy_wallet_id = Some(id.into());
        cfg.keys.proxy = Some("https://seal.example.workers.dev".into());
        cfg.keys.env.insert("PRIVY_API_URL".into(), "privy".into());
        cfg.keys.strip = vec!["PRIVY_APP_SECRET".into()];
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
        cfg.default_scopes.insert(
            "http_request".into(),
            scope(|s| s.env_reads = vec!["*".into()]),
        );
        assert_eq!(errs(&cfg), "");
    }

    /// The id and its proxy route; the no-MCP / no-shell rules are
    /// hardened-sandbox rules (`config/hardening.rs` tests).
    #[test]
    fn signer_needs_a_plain_id_and_the_proxy_route() {
        let mut cfg = with_signer("cmw1abc-2_x");
        cfg.agents.get_mut("main").unwrap().engine = "claude_code".into();
        assert_eq!(errs(&cfg), "", "hardening.rs owns the claude_code rule");
        assert!(errs(&with_signer("../x")).contains("must be 1-64 chars"));
        assert!(errs(&with_signer("")).contains("must be 1-64 chars"));

        let mut cfg = with_signer("w1");
        cfg.keys.env.remove("PRIVY_API_URL");
        assert!(errs(&cfg).contains("needs [keys.env] PRIVY_API_URL"));
        let mut cfg = with_signer("w1");
        cfg.keys
            .env
            .insert("PRIVY_API_URL".into(), "@session".into());
        assert!(errs(&cfg).contains("needs [keys.env] PRIVY_API_URL"));
    }

    #[test]
    fn no_agent_may_read_the_session_beside_a_signer() {
        let mut cfg = with_signer("w1");
        cfg.keys
            .env
            .insert("OPENROUTER_API_KEY".into(), "@session".into());
        cfg.keys
            .env
            .insert("OPENROUTER_BASE_URL".into(), "openrouter".into());
        cfg.default_scopes.insert(
            "http_request".into(),
            scope(|s| s.env_reads = vec!["*".into()]),
        );
        let main = cfg.agents.get_mut("main").unwrap();
        main.scopes.insert(
            "jupiter_swap".into(),
            scope(|s| {
                s.env_reads = vec![
                    "PRIVY_API_URL".into(),
                    "PRIVY_APP_ID".into(),
                    "TENGU_KEYS_SESSION_TOKEN".into(),
                    "OPENROUTER_API_KEY".into(),
                ]
            }),
        );
        let e = errs(&cfg);
        assert!(
            e.contains("default_scopes.http_request.env_reads: '*'"),
            "{e}"
        );
        assert!(
            e.contains("agents.main.scopes.jupiter_swap.env_reads: 'TENGU_KEYS_SESSION_TOKEN'"),
            "{e}"
        );
        assert!(
            e.contains("agents.main.scopes.jupiter_swap.env_reads: 'OPENROUTER_API_KEY'"),
            "{e}"
        );
        assert!(
            !e.contains("'PRIVY_API_URL'") && !e.contains("'PRIVY_APP_ID'"),
            "{e}"
        );
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
