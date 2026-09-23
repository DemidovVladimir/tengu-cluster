//! `[egress]` — network policy schema and validation. The runtime policy
//! (clients, proxy env, audit log) lives in `adapters/egress.rs`; see
//! `docs/egress-2026-09-16.md`.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use serde::{Deserialize, Serialize};

pub(crate) const SHELL_PROXY_ENV: &str = "proxy_env";
pub(crate) const SHELL_ISOLATED: &str = "isolated";

/// `[egress] network` values.
pub(crate) const NETWORK_TOR: &str = "tor";
pub(crate) const NETWORK_OPEN: &str = "open";
/// Where the `deploy/tor/` stack listens (SOCKS5 + HTTP CONNECT).
pub(crate) const DEFAULT_TOR_PROXY: &str = "socks5h://127.0.0.1:9050";
/// Overrides the Tor proxy address when `[egress].proxy` is unset — set by
/// `docker-compose.tor.yml` to `socks5h://tor:9050`.
pub(crate) const TOR_PROXY_ENV: &str = "TENGU_TOR_PROXY";

/// `[egress]` — see the module doc for what each knob controls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    /// `"tor"` (default): everything goes through the Tor proxy, fail-closed.
    /// `"open"`: plain internet. Sets the defaults of `proxy` and
    /// `route_llm_api`; explicit values win.
    #[serde(default = "default_network")]
    pub network: String,
    /// Proxy for all tool traffic, e.g. `socks5h://127.0.0.1:9050` (Tor).
    /// `socks5h` resolves DNS through the proxy; plain `socks5://` is
    /// refused because it leaks DNS. Unset = `TENGU_TOR_PROXY` or
    /// `socks5h://127.0.0.1:9050` under `network = "tor"`, direct under `"open"`.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Also send LLM provider traffic (OpenRouter, embeddings, the Claude
    /// Code CLI) through `proxy`. Unset = `true` under `network = "tor"`,
    /// `false` under `"open"`.
    #[serde(default)]
    pub route_llm_api: Option<bool>,
    /// Sandbox-wide host ceiling (same patterns as `net_hosts`: exact,
    /// `*.suffix`, `*`). Non-empty = only these hosts; per-agent scopes
    /// cannot widen it.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// Hosts always refused, checked before `allow_hosts`.
    #[serde(default)]
    pub deny_hosts: Vec<String>,
    /// Refuse plain `http://`.
    #[serde(default)]
    pub https_only: bool,
    /// `"proxy_env"` (default: export proxy env vars — advisory) or
    /// `"isolated"` (macOS `sandbox-exec`: no network except the proxy port).
    #[serde(default = "default_shell_network")]
    pub shell_network: String,
    /// Write the JSONL audit log. Default true.
    #[serde(default = "default_true")]
    pub audit: bool,
    /// Audit log path. Default `<TENGU_HOME>/logs/egress.jsonl`.
    #[serde(default)]
    pub audit_log: Option<PathBuf>,
}

fn default_shell_network() -> String {
    SHELL_PROXY_ENV.to_string()
}

fn default_network() -> String {
    NETWORK_TOR.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            network: default_network(),
            proxy: None,
            route_llm_api: None,
            allow_hosts: Vec::new(),
            deny_hosts: Vec::new(),
            https_only: false,
            shell_network: default_shell_network(),
            audit: true,
            audit_log: None,
        }
    }
}

impl EgressConfig {
    /// `[egress] network = "open"` — plain internet, nothing proxied.
    pub(crate) fn open() -> Self {
        Self {
            network: NETWORK_OPEN.to_string(),
            ..Self::default()
        }
    }

    pub(crate) fn is_tor(&self) -> bool {
        self.network == NETWORK_TOR
    }

    /// Fill `proxy` / `route_llm_api` from `network` (explicit values win).
    /// Under `tor`, an unset `proxy` reads `TENGU_TOR_PROXY` and falls back
    /// to `socks5h://127.0.0.1:9050`. The result round-trips through
    /// `TENGU_EGRESS` unchanged, so children never re-resolve the env.
    pub(crate) fn resolved(&self) -> Self {
        let mut cfg = self.clone();
        if cfg.is_tor() {
            if cfg.proxy.is_none() {
                cfg.proxy = Some(
                    std::env::var(TOR_PROXY_ENV)
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_string()),
                );
            }
            cfg.route_llm_api.get_or_insert(true);
        } else {
            cfg.route_llm_api.get_or_insert(false);
        }
        cfg
    }

    pub(crate) fn validation_errors(&self) -> Vec<String> {
        let this = self.resolved();
        let mut errors = Vec::new();
        if !matches!(this.network.as_str(), NETWORK_TOR | NETWORK_OPEN) {
            errors.push(format!(
                "egress.network must be one of tor|open (got '{}')",
                this.network
            ));
        }
        let proxy = match this.proxy.as_deref().map(parse_proxy) {
            Some(Ok(url)) => Some(url),
            Some(Err(e)) => {
                errors.push(format!("egress.proxy: {e}"));
                None
            }
            None => None,
        };
        if this.route_llm_api == Some(true) && this.proxy.is_none() {
            errors.push("egress.route_llm_api needs egress.proxy".to_string());
        }
        match this.shell_network.as_str() {
            SHELL_PROXY_ENV => {}
            SHELL_ISOLATED => {
                if !cfg!(target_os = "macos") {
                    errors.push(
                        "egress.shell_network = \"isolated\" needs macOS sandbox-exec; on Linux \
                         run tengu in the Docker tor profile (docker-compose.tor.yml) instead"
                            .to_string(),
                    );
                }
                if let Some(url) = &proxy {
                    if !is_loopback_host(url.host_str().unwrap_or("")) {
                        errors.push(
                            "egress.shell_network = \"isolated\" needs a loopback proxy \
                             (127.0.0.1 / localhost / ::1)"
                                .to_string(),
                        );
                    }
                }
            }
            other => errors.push(format!(
                "egress.shell_network must be one of proxy_env|isolated (got '{other}')"
            )),
        }
        for (field, patterns) in [
            ("allow_hosts", &this.allow_hosts),
            ("deny_hosts", &this.deny_hosts),
        ] {
            if patterns.iter().any(|p| p.trim().is_empty()) {
                errors.push(format!("egress.{field} contains an empty pattern"));
            }
        }
        errors
    }
}

pub(crate) fn parse_proxy(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow!("invalid url '{raw}': {e}"))?;
    match url.scheme() {
        "socks5h" | "http" | "https" => {}
        "socks5" => bail!("'{raw}' resolves DNS locally (DNS leak) — use socks5h://"),
        other => bail!("unsupported scheme '{other}' (socks5h|http|https)"),
    }
    if url.host_str().map_or(true, str::is_empty) {
        bail!("'{raw}' has no host");
    }
    if url.port().is_none() {
        bail!("'{raw}' needs an explicit port (Tor: 9050)");
    }
    Ok(url)
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}
