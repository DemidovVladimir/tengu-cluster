//! `[keys]` — the seal proxy (`docs/sealed-keys-2026-10-09.md`).
//!
//! Provider keys never sit on this machine: they are secrets of the
//! `tengu-seal` Cloudflare Worker (set by the operator in Cloudflare). At
//! startup tengu signs a session request with an SSH-agent key (Secure
//! Enclave via Secretive, Touch ID) and exports env vars that point the
//! existing clients at the Worker's routes (`adapters/outbound/keys`).
//!
//! ```toml
//! [keys]
//! proxy = "https://tengu-seal.<subdomain>.workers.dev"
//! client = "tengu-attended"            # agent key comment or SHA256:… fingerprint
//! [keys.env]
//! OPENROUTER_BASE_URL = "openrouter"   # → <proxy>/openrouter
//! OPENROUTER_API_KEY = "@session"      # → the session token
//! ```

use std::collections::BTreeMap;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::domain::scope::host_matches;

/// `[keys.env]` value that exports the session token itself.
pub(crate) const SESSION_VALUE: &str = "@session";

/// Env var holding the session token in every tengu process
/// (`outbound/keys::install`).
pub(crate) const SESSION_TOKEN_ENV: &str = "TENGU_KEYS_SESSION_TOKEN";

impl KeysConfig {
    /// Env var names that carry the session: [`SESSION_TOKEN_ENV`] and every
    /// `[keys.env]` var set to `@session`.
    pub(crate) fn session_vars(&self) -> Vec<&str> {
        let mut out = vec![SESSION_TOKEN_ENV];
        out.extend(
            self.env
                .iter()
                .filter(|(_, v)| *v == SESSION_VALUE)
                .map(|(k, _)| k.as_str()),
        );
        out
    }
}

/// The only vars that may carry `@session`, each with the var that must
/// point its client at the proxy — so the token is never sent to the
/// provider itself (`auth_header_for` covers every other client).
pub(crate) const SESSION_VARS: &[(&str, &str)] = &[("OPENROUTER_API_KEY", "OPENROUTER_BASE_URL")];

/// A `[keys.env]` var that sends a provider through the proxy → the local
/// key its client falls back to without it. That key must be in `strip`, so
/// a failed session never falls back to a local key.
pub(crate) const FALLBACK_KEYS: &[(&str, &str)] = &[("PRIVY_API_URL", "PRIVY_APP_SECRET")];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysConfig {
    /// Worker origin, `https://tengu-seal.<subdomain>.workers.dev` (no
    /// path). Unset = seal proxy off; every other field needs it.
    #[serde(default)]
    pub proxy: Option<String>,
    /// SSH-agent key that signs the session request: its comment or its
    /// full `SHA256:` fingerprint. Unset = the agent's only usable key.
    #[serde(default)]
    pub client: Option<String>,
    /// SSH-agent socket; unset = `$SSH_AUTH_SOCK`. `~` is expanded.
    /// Secretive: `~/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh`.
    #[serde(default)]
    pub agent_socket: Option<String>,
    /// Env var → a Worker route, optionally with a path / query after it
    /// (`"openrouter"`, `"solana-rpc/?api-key=TENGU_SECRET"`; exported as
    /// `<proxy>/<value>`), or `@session` (the session token; only
    /// `OPENROUTER_API_KEY`, next to an `OPENROUTER_BASE_URL` route).
    /// Overrides any inherited value. The session is asked for exactly these
    /// routes.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Env vars removed at load — local copies of keys that now live in the
    /// Worker (`TELEGRAM_BOT_TOKEN`, `HELIUS_API_KEY`, …), so no tool, shell
    /// or child sees them even if `.env` / the vault still holds them.
    #[serde(default)]
    pub strip: Vec<String>,
}

impl KeysConfig {
    pub fn enabled(&self) -> bool {
        self.proxy.is_some()
    }

    /// The Worker routes `[keys.env]` uses, in order, once each — what the
    /// session asks for.
    pub fn routes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for v in self.env.values().filter(|v| *v != SESSION_VALUE) {
            let r = route_of(v).to_string();
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }

    /// Problems with `[keys]`; `allow_hosts` is the sandbox's `[egress]`
    /// ceiling, which must admit the proxy host.
    pub fn validation_errors(&self, allow_hosts: &[String]) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(proxy) = &self.proxy else {
            let set = self.client.is_some()
                || self.agent_socket.is_some()
                || !self.env.is_empty()
                || !self.strip.is_empty();
            if set {
                errors.push("keys: every [keys] field needs keys.proxy".to_string());
            }
            return errors;
        };
        match Url::parse(proxy) {
            Ok(u) if u.host_str().is_some() && (u.scheme() == "https" || is_local_dev(&u)) => {
                if u.path() != "/"
                    || u.query().is_some()
                    || u.fragment().is_some()
                    || !u.username().is_empty()
                    || u.password().is_some()
                {
                    errors.push(format!(
                        "keys.proxy must be an origin (https://host), got '{proxy}'"
                    ));
                }
                let host = u.host_str().unwrap_or_default();
                if !allow_hosts.is_empty() && !allow_hosts.iter().any(|p| host_matches(p, host)) {
                    errors.push(format!(
                        "keys.proxy host '{host}' is not in [egress] allow_hosts"
                    ));
                }
            }
            _ => errors.push(format!(
                "keys.proxy must be an https URL (or http://127.0.0.1 for wrangler dev), got '{proxy}'"
            )),
        }
        let var_name = |n: &str| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        };
        for (name, value) in &self.env {
            if !var_name(name) {
                errors.push(format!("keys.env: '{name}' is not an env var name"));
            }
            if value == SESSION_VALUE {
                match SESSION_VARS.iter().find(|(v, _)| v == name) {
                    None => errors.push(format!(
                        "keys.env.{name}: '@session' is only for {}",
                        SESSION_VARS
                            .iter()
                            .map(|(v, _)| *v)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    Some((_, base)) => {
                        if !self.env.get(*base).is_some_and(|b| is_route_target(b)) {
                            errors.push(format!(
                                "keys.env.{name} = '@session' needs keys.env.{base} = '<route>' (else the token goes to the provider)"
                            ));
                        }
                    }
                }
            } else if !is_route_target(value) {
                errors.push(format!(
                    "keys.env.{name}: '{value}' must be '@session' or '<route>[/path][?query]'"
                ));
            }
        }
        for name in &self.strip {
            if !var_name(name) || self.env.contains_key(name) {
                errors.push(format!(
                    "keys.strip: '{name}' must be an env var name not set by [keys.env]"
                ));
            }
        }
        for (var, key) in FALLBACK_KEYS {
            if self.env.contains_key(*var) && !self.strip.iter().any(|s| s == key) {
                errors.push(format!(
                    "keys.env.{var} needs '{key}' in keys.strip (else a failed session falls back to the local key)"
                ));
            }
        }
        errors
    }
}

/// `http://127.0.0.1:<port>` / `http://localhost:<port>` — `wrangler dev`
/// only; a deployed Worker is always https.
fn is_local_dev(u: &Url) -> bool {
    u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

/// The route name at the start of a `[keys.env]` value.
pub(crate) fn route_of(value: &str) -> &str {
    let end = value.find(['/', '?']).unwrap_or(value.len());
    &value[..end]
}

/// `<route>[/path][?query]` where the route is a Worker route name.
pub(crate) fn is_route_target(value: &str) -> bool {
    tengu_seal::route::is_route_name(route_of(value))
        && !value.contains(char::is_whitespace)
        && !value.contains("..")
        && !value.contains('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(proxy: &str) -> KeysConfig {
        KeysConfig {
            proxy: Some(proxy.into()),
            env: BTreeMap::from([
                ("OPENROUTER_BASE_URL".into(), "openrouter".into()),
                ("OPENROUTER_API_KEY".into(), "@session".into()),
                (
                    "SOLANA_RPC_URL".into(),
                    "solana-rpc/?api-key=TENGU_SECRET".into(),
                ),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn off_by_default_and_fields_need_proxy() {
        assert!(KeysConfig::default().validation_errors(&[]).is_empty());
        let stray = KeysConfig {
            client: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(stray.validation_errors(&[]).len(), 1);
    }

    #[test]
    fn proxy_must_be_an_https_origin_inside_allow_hosts() {
        assert!(cfg("https://tengu-seal.a.workers.dev")
            .validation_errors(&[])
            .is_empty());
        assert!(!cfg("http://tengu-seal.a.workers.dev")
            .validation_errors(&[])
            .is_empty());
        assert!(cfg("http://127.0.0.1:8787")
            .validation_errors(&[])
            .is_empty());
        assert!(!cfg("https://tengu-seal.a.workers.dev/x")
            .validation_errors(&[])
            .is_empty());
        let allow = vec!["api.hyperliquid.xyz".to_string()];
        assert_eq!(
            cfg("https://tengu-seal.a.workers.dev")
                .validation_errors(&allow)
                .len(),
            1
        );
        let allow = vec!["*.workers.dev".to_string()];
        assert!(cfg("https://tengu-seal.a.workers.dev")
            .validation_errors(&allow)
            .is_empty());
    }

    /// `sandboxes/sealed-check` proves the seal proxy end to end; it must stay
    /// harmless: one pure-compute tool with a deny-all scope, OpenRouter only
    /// through the proxy, no money / signer / memory / MCP / planner / pins.
    #[test]
    fn sealed_check_sandbox_is_harmless() {
        use crate::config::execution_map::ExecutionMap;
        use crate::config::Config;
        use std::path::Path;

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/sealed-check");
        let cfg = Config::load(&dir.join("config.toml")).unwrap_or_else(|e| panic!("{e:#}"));

        assert!(cfg.risk.is_none() && cfg.paper.is_none() && cfg.xmarket.is_none());
        assert!(cfg.solana.privy_wallet_id.is_none() && cfg.generation.is_none());
        assert!(cfg.mcp_servers.is_empty() && cfg.orchestrator.is_none());
        assert!(cfg.default_scopes.is_empty() && !cfg.memory.enabled);
        assert!(!cfg.studio.control, "control via --allow-control only");

        let keys: Vec<&str> = cfg.keys.env.keys().map(String::as_str).collect();
        assert_eq!(keys, ["OPENROUTER_API_KEY", "OPENROUTER_BASE_URL"]);
        assert_eq!(cfg.keys.env["OPENROUTER_API_KEY"], SESSION_VALUE);
        assert_eq!(cfg.keys.env["OPENROUTER_BASE_URL"], "openrouter");

        assert_eq!(cfg.agents.keys().collect::<Vec<_>>(), ["checker"]);
        let a = &cfg.agents["checker"];
        assert_eq!(a.tools, ["hex_to_uint256"]);
        assert!(a.workspace_tools.is_empty() && a.skill_packages.is_empty());
        assert!(a.description.is_none() && !a.default, "private");
        let s = &a.scopes["hex_to_uint256"];
        assert!(
            s.fs_roots.is_empty()
                && s.net_hosts.is_empty()
                && s.env_reads.is_empty()
                && s.shell_bins.is_empty()
                && s.wallets.is_empty(),
            "deny-all scope"
        );

        let check = &cfg.decision_loops["check"];
        assert!(check.dry_run);
        for (an, act) in &check.actions {
            let tool = act.tool.as_deref().unwrap_or("hex_to_uint256");
            assert_eq!(tool, "hex_to_uint256", "action {an}");
        }
        for entry in std::fs::read_dir(dir.join("scenarios")).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            if file.ends_with(".map.json") {
                let map = ExecutionMap::parse(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
                assert_eq!(map.loop_name, "check", "{file}");
                map.apply(check).unwrap_or_else(|e| panic!("{file}: {e:?}"));
            } else {
                let event: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
                assert!(event["scenario"].is_string(), "{file}: no `scenario`");
            }
        }
    }

    #[test]
    fn session_only_next_to_a_proxy_base_and_strip_names() {
        let mut c = cfg("https://p.example");
        assert_eq!(c.routes(), ["openrouter", "solana-rpc"]);
        c.env.insert("TELEGRAM_BOT_TOKEN".into(), "@session".into());
        assert_eq!(
            c.validation_errors(&[]).len(),
            1,
            "@session only for OPENROUTER_API_KEY"
        );
        c.env.remove("TELEGRAM_BOT_TOKEN");
        c.env.remove("OPENROUTER_BASE_URL");
        assert_eq!(
            c.validation_errors(&[]).len(),
            1,
            "@session needs its base route"
        );
        let mut c = cfg("https://p.example");
        c.strip = vec![
            "TELEGRAM_BOT_TOKEN".into(),
            "OPENROUTER_API_KEY".into(),
            "bad-name".into(),
        ];
        assert_eq!(c.validation_errors(&[]).len(), 2);
        assert!(!cfg("https://u:pw@p.example")
            .validation_errors(&[])
            .is_empty());
        assert!(!cfg("https://:pw@p.example")
            .validation_errors(&[])
            .is_empty());
    }

    #[test]
    fn a_routed_provider_strips_its_local_key() {
        let mut c = cfg("https://p.example");
        c.env.insert("PRIVY_API_URL".into(), "privy".into());
        let errors = c.validation_errors(&[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("PRIVY_APP_SECRET"), "{errors:?}");
        c.strip = vec!["PRIVY_APP_SECRET".into()];
        assert!(c.validation_errors(&[]).is_empty());
    }

    #[test]
    fn env_values_are_routes_or_session() {
        let mut c = cfg("https://p.example");
        c.env.insert("A".into(), "Bad Route".into());
        c.env.insert("B".into(), "session".into());
        c.env.insert("C".into(), "openrouter/../x".into());
        c.env.insert("bad-name".into(), "openrouter".into());
        assert_eq!(c.validation_errors(&[]).len(), 4);
        assert_eq!(route_of("solana-rpc/?api-key=x"), "solana-rpc");
        assert_eq!(route_of("openrouter"), "openrouter");
    }
}
