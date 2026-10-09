//! `[keys]` — sealed-secret proxy (`docs/sealed-keys-2026-10-09.md`).
//!
//! Provider keys never sit on this machine: each one is HPKE-sealed to the
//! `tengu-seal` Cloudflare Worker (`tengu keys seal`), and the Worker opens
//! it, injects it and forwards the request. At startup tengu signs a session
//! request with an SSH-agent key (Secure Enclave via Secretive, Touch ID) and
//! exports env vars that point the existing clients at the Worker
//! (`adapters/outbound/keys`).
//!
//! ```toml
//! [keys]
//! proxy = "https://tengu-seal.<subdomain>.workers.dev"
//! pubkey = "<printed by tengu keys setup>"
//! client = "tengu-attended"            # agent key comment or SHA256:… fingerprint
//! [keys.env]
//! OPENROUTER_BASE_URL = "openrouter"   # → <proxy>/s/<sealed openrouter blob>
//! OPENROUTER_API_KEY = "@session"      # → the session token
//! ```

use std::collections::BTreeMap;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::domain::scope::host_matches;

/// `[keys.env]` value that exports the session token itself.
pub(crate) const SESSION_VALUE: &str = "@session";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysConfig {
    /// Worker origin, `https://tengu-seal.<subdomain>.workers.dev` (no
    /// path). Unset = sealed keys off; every other field needs it.
    #[serde(default)]
    pub proxy: Option<String>,
    /// The Worker's X25519 public key (base64url), pinned from `tengu keys
    /// setup`; `tengu keys seal` encrypts to it.
    #[serde(default)]
    pub pubkey: Option<String>,
    /// SSH-agent key that signs the session request: its comment or its
    /// full `SHA256:` fingerprint. Unset = the agent's only usable key.
    #[serde(default)]
    pub client: Option<String>,
    /// SSH-agent socket; unset = `$SSH_AUTH_SOCK`. `~` is expanded.
    /// Secretive: `~/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh`.
    #[serde(default)]
    pub agent_socket: Option<String>,
    /// Directory of `<name>.sealed` files; unset = `<TENGU_HOME>/sealed`.
    #[serde(default)]
    pub sealed_dir: Option<String>,
    /// Env var → sealed blob name (exported as `<proxy>/s/<blob>`) or
    /// `@session` (the session token). Overrides any inherited value.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl KeysConfig {
    pub fn enabled(&self) -> bool {
        self.proxy.is_some()
    }

    /// Problems with `[keys]`; `allow_hosts` is the sandbox's `[egress]`
    /// ceiling, which must admit the proxy host.
    pub fn validation_errors(&self, allow_hosts: &[String]) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(proxy) = &self.proxy else {
            let set = self.pubkey.is_some()
                || self.client.is_some()
                || self.agent_socket.is_some()
                || self.sealed_dir.is_some()
                || !self.env.is_empty();
            if set {
                errors.push("keys: every [keys] field needs keys.proxy".to_string());
            }
            return errors;
        };
        match Url::parse(proxy) {
            Ok(u) if u.host_str().is_some() && (u.scheme() == "https" || is_local_dev(&u)) => {
                if u.path() != "/" || u.query().is_some() || !u.username().is_empty() {
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
            _ => errors.push(format!("keys.proxy must be an https URL (or http://127.0.0.1 for wrangler dev), got '{proxy}'")),
        }
        for (name, value) in &self.env {
            let name_ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !name_ok {
                errors.push(format!("keys.env: '{name}' is not an env var name"));
            }
            if value != SESSION_VALUE && !is_blob_name(value) {
                errors.push(format!(
                    "keys.env.{name}: '{value}' must be '@session' or a sealed blob name"
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

/// Sealed blob names: 1–64 chars of `[A-Za-z0-9_-]` (the Worker checks the
/// same rule on the name sealed inside the blob).
pub(crate) fn is_blob_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    #[test]
    fn env_values_are_blob_names_or_session() {
        let mut c = cfg("https://p.example");
        c.env.insert("SOLANA_RPC_URL".into(), "helius rpc".into());
        c.env.insert("bad-name".into(), "x".into());
        assert_eq!(c.validation_errors(&[]).len(), 2);
    }
}
