//! `[a2a]` — A2A (Agent2Agent, `domain/a2a/`): the remote agents this
//! sandbox's agents may call (`[a2a.remotes.<name>]`, the opt-in `a2a` tool)
//! and the server that exposes its agents to other harnesses
//! (`[a2a.server]`, `tengu a2a serve`). Absent = no A2A. `deny_unknown_fields`
//! throughout. Doc: `docs/a2a-2026-10-10.md`.
//!
//! `[a2a.remotes.<name>]` (name: `[a-z0-9][a-z0-9_-]*`)
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `url` | required | the remote's base URL — its card is `<url>/.well-known/agent-card.json` (then the 0.3 path `agent.json`) — or the card's URL itself (ends in `.json`) |
//! | `endpoint_url` | — | call this URL instead of the card's interface URL (same binding and version): a card that names another host, a container address |
//! | `description` | — | what the remote is for, shown by `a2a` `list` |
//! | `bearer_env` | — | `Authorization: Bearer $<bearer_env>` |
//! | `header` + `header_env` | — | `<header>: $<header_env>` (an API-key header); not with `bearer_env` |
//! | `timeout_secs` | 120 | longest wait for a task to settle (`a2a` `wait_secs` may only lower it) |
//! | `max_result_chars` | 12000 | content text a result shows (ids always whole) |
//!
//! Credentials go only to the host of `url` or `endpoint_url`: the client
//! refuses a card that points elsewhere (`outbound/a2a/`). Each call also
//! passes `[egress]` and the caller's `[default_scopes.a2a]` / agent scope
//! (`net_hosts`; `env_reads` for the credential variable).
//!
//! `[a2a.server]`
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `bind` / `port` | `127.0.0.1` / 8710 | listen address |
//! | `public_url` | `http://<bind>:<port>` | base URL the cards advertise (behind a proxy / tunnel) |
//! | `token_env` | — | clients send `Authorization: Bearer $<token_env>`; required unless `allow_unauthenticated` |
//! | `allow_unauthenticated` | false | no token — only on a loopback `bind`, never in a hardened sandbox |
//! | `name` / `description` | `tengu <sandbox>` / the planner's or agent's | the front door card's name and description |
//! | `orchestrator` | false | serve the planner (needs `[orchestrator]`) at `/a2a` — the root card |
//! | `agents` | `[]` | serve each agent (one with a `description`) at `/a2a/agents/<name>` |
//! | `max_tasks` | 500 | tasks kept in memory (the oldest settled ones go first) |
//! | `max_running` | 4 | tasks run at once; the rest wait `SUBMITTED` |
//! | `run_timeout_secs` | 900 | a run longer than this fails (`timed out`) |
//! | `context_turns` | 8 | earlier turns of a context handed to the agent with a follow-up message |

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use super::Config;

/// `[a2a]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A2aConfig {
    #[serde(default)]
    pub remotes: BTreeMap<String, A2aRemoteConfig>,
    #[serde(default)]
    pub server: Option<A2aServerConfig>,
}

/// `[a2a.remotes.<name>]` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A2aRemoteConfig {
    pub url: String,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub bearer_env: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub header_env: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_result_chars")]
    pub max_result_chars: usize,
}

/// `[a2a.server]` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A2aServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub allow_unauthenticated: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub orchestrator: bool,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,
    #[serde(default = "default_max_running")]
    pub max_running: usize,
    #[serde(default = "default_run_timeout_secs")]
    pub run_timeout_secs: u64,
    #[serde(default = "default_context_turns")]
    pub context_turns: usize,
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_max_result_chars() -> usize {
    12_000
}

fn default_bind() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8710
}

fn default_max_tasks() -> usize {
    500
}

fn default_context_turns() -> usize {
    8
}

fn default_max_running() -> usize {
    4
}

fn default_run_timeout_secs() -> u64 {
    900
}

/// Longest `run_timeout_secs` (a day).
pub const MAX_RUN_TIMEOUT_SECS: u64 = 86_400;

/// Longest `timeout_secs` (an hour).
pub const MAX_TIMEOUT_SECS: u64 = 3_600;

impl A2aRemoteConfig {
    /// Where the card is: `url` itself when it ends in `.json`, else
    /// `<url>/.well-known/agent-card.json`; then the 0.3 path.
    pub fn card_urls(&self) -> Vec<String> {
        let url = self.url.trim();
        if url.ends_with(".json") {
            return vec![url.to_string()];
        }
        let base = url.trim_end_matches('/');
        vec![
            format!("{base}/.well-known/agent-card.json"),
            format!("{base}/.well-known/agent.json"),
        ]
    }

    /// The credential's header and variable, if any.
    pub fn credential(&self) -> Option<(String, &str)> {
        if let Some(env) = &self.bearer_env {
            return Some(("Authorization".into(), env.as_str()));
        }
        match (&self.header, &self.header_env) {
            (Some(h), Some(env)) => Some((h.clone(), env.as_str())),
            _ => None,
        }
    }
}

impl A2aServerConfig {
    /// Base URL the cards advertise, no trailing `/`.
    pub fn base_url(&self) -> String {
        match &self.public_url {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => match self.bind.parse::<IpAddr>() {
                Ok(IpAddr::V6(ip)) => format!("http://[{ip}]:{}", self.port),
                _ => format!("http://{}:{}", self.bind, self.port),
            },
        }
    }

    /// `bind` is a loopback address.
    pub fn is_loopback(&self) -> bool {
        self.bind.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

/// A remote's name: `[a-z0-9][a-z0-9_-]*` (tool args, audit lines).
pub fn valid_remote_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn http_url_error(at: &str, url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(r) if !r.is_empty() && !r.starts_with('/') => None,
        _ => Some(format!(
            "{at} must be an http:// or https:// URL with a host (got '{url}')"
        )),
    }
}

fn env_name_error(at: &str, name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    (!ok).then(|| format!("{at} must name an environment variable ([A-Z0-9_]+), got '{name}'"))
}

/// Every `[a2a]` load error.
pub fn validation_errors(cfg: &Config) -> Vec<String> {
    let Some(a2a) = &cfg.a2a else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, r) in &a2a.remotes {
        let at = format!("a2a.remotes.{name}");
        if !valid_remote_name(name) {
            out.push(format!(
                "{at}: the name must match [a-z0-9][a-z0-9_-]* (it is the `remote` the a2a tool takes)"
            ));
        }
        out.extend(http_url_error(&format!("{at}.url"), &r.url));
        if let Some(e) = &r.endpoint_url {
            out.extend(http_url_error(&format!("{at}.endpoint_url"), e));
        }
        if let Some(env) = &r.bearer_env {
            out.extend(env_name_error(&format!("{at}.bearer_env"), env));
            if r.header.is_some() || r.header_env.is_some() {
                out.push(format!(
                    "{at}: `bearer_env` and `header` / `header_env` — use one credential"
                ));
            }
        }
        match (&r.header, &r.header_env) {
            (Some(h), Some(env)) => {
                if h.trim().is_empty() || h.contains(|c: char| c.is_whitespace() || c == ':') {
                    out.push(format!(
                        "{at}.header must be an HTTP header name (got '{h}')"
                    ));
                }
                out.extend(env_name_error(&format!("{at}.header_env"), env));
            }
            (None, None) => {}
            _ => out.push(format!("{at}: `header` and `header_env` go together")),
        }
        if r.timeout_secs == 0 || r.timeout_secs > MAX_TIMEOUT_SECS {
            out.push(format!(
                "{at}.timeout_secs must be 1..={MAX_TIMEOUT_SECS} (got {})",
                r.timeout_secs
            ));
        }
        if r.max_result_chars < 200 {
            out.push(format!(
                "{at}.max_result_chars must be at least 200 (got {})",
                r.max_result_chars
            ));
        }
    }
    if let Some(s) = &a2a.server {
        out.extend(server_errors(cfg, s));
    }
    out
}

fn server_errors(cfg: &Config, s: &A2aServerConfig) -> Vec<String> {
    let mut out = Vec::new();
    if s.bind.parse::<IpAddr>().is_err() {
        out.push(format!(
            "a2a.server.bind must be an IP address (got '{}')",
            s.bind
        ));
    }
    if s.port == 0 {
        out.push("a2a.server.port must be greater than 0".into());
    }
    if let Some(u) = &s.public_url {
        out.extend(http_url_error("a2a.server.public_url", u));
    }
    match (&s.token_env, s.allow_unauthenticated) {
        (Some(_), true) => {
            out.push("a2a.server: `token_env` and `allow_unauthenticated = true` — pick one".into())
        }
        (Some(env), false) => out.extend(env_name_error("a2a.server.token_env", env)),
        (None, false) => out.push(
            "a2a.server: set `token_env` (clients send `Authorization: Bearer <token>`), or \
             `allow_unauthenticated = true` on a loopback bind"
                .into(),
        ),
        (None, true) => {
            if !s.is_loopback() {
                out.push(format!(
                    "a2a.server.allow_unauthenticated needs a loopback bind (got '{}') — \
                     anyone who reaches the port would run your agents",
                    s.bind
                ));
            }
            if super::hardening::requires_hardened_claude_code(cfg) {
                out.push(
                    "a2a.server.allow_unauthenticated is refused in a hardened sandbox ([risk], \
                     [soe] or a [solana] signer) — set `token_env`"
                        .into(),
                );
            }
        }
    }
    if !s.orchestrator && s.agents.is_empty() {
        out.push(
            "a2a.server serves nothing: set `orchestrator = true` and / or list `agents`".into(),
        );
    }
    if s.orchestrator && cfg.orchestrator.is_none() {
        out.push("a2a.server.orchestrator = true needs an [orchestrator] section".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in &s.agents {
        if !seen.insert(name) {
            out.push(format!("a2a.server.agents lists '{name}' twice"));
        }
        match cfg.agents.get(name) {
            None => out.push(format!("a2a.server.agents: no [agents.{name}] block")),
            Some(a) if a.description.is_none() => out.push(format!(
                "a2a.server.agents: '{name}' has no `description` — a private agent is never served"
            )),
            Some(_) => {}
        }
    }
    if s.max_tasks == 0 {
        out.push("a2a.server.max_tasks must be greater than 0".into());
    }
    if s.max_running == 0 {
        out.push("a2a.server.max_running must be greater than 0".into());
    }
    if s.run_timeout_secs == 0 || s.run_timeout_secs > MAX_RUN_TIMEOUT_SECS {
        out.push(format!(
            "a2a.server.run_timeout_secs must be 1..={MAX_RUN_TIMEOUT_SECS} (got {})",
            s.run_timeout_secs
        ));
    }
    out
}

/// `[a2a]` smells logged at load: an agent holds the `a2a` tool but there
/// is no remote to call.
pub fn validation_warnings(cfg: &Config) -> Vec<String> {
    let remotes = cfg.a2a.as_ref().map_or(0, |a| a.remotes.len());
    if remotes > 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = cfg
        .agents
        .iter()
        .filter(|(_, a)| {
            a.tools.iter().chain(&a.workspace_tools).any(|t| t == crate::domain::tools::A2A)
        })
        .map(|(id, _)| {
            format!(
                "agents.{id} holds the `a2a` tool but there is no [a2a.remotes.<name>] — every call is refused"
            )
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        Config::load(&path).map_err(|e| format!("{e:#}"))
    }

    const AGENTS: &str = r#"
[agents.main]
engine = "openrouter"
model = "x/y"
default = true

[agents.researcher]
engine = "openrouter"
model = "x/y"
description = "Finds things."
tools = ["a2a"]

[agents.signer]
engine = "openrouter"
model = "x/y"
"#;

    #[test]
    fn remotes_and_server_load() {
        let cfg = load(&format!(
            r#"{AGENTS}
[a2a.remotes.research-desk]
url = "https://agents.example.com/research"
bearer_env = "RESEARCH_A2A_TOKEN"
description = "Another harness."

[a2a.remotes.local_box]
url = "http://127.0.0.1:9000/.well-known/agent-card.json"
header = "X-API-Key"
header_env = "LOCAL_KEY"
timeout_secs = 30

[a2a.server]
token_env = "TENGU_A2A_TOKEN"
agents = ["researcher"]
"#
        ))
        .unwrap();
        let a2a = cfg.a2a.as_ref().unwrap();
        let r = &a2a.remotes["research-desk"];
        assert_eq!(
            r.card_urls(),
            vec![
                "https://agents.example.com/research/.well-known/agent-card.json".to_string(),
                "https://agents.example.com/research/.well-known/agent.json".to_string(),
            ]
        );
        assert_eq!(
            r.credential(),
            Some(("Authorization".to_string(), "RESEARCH_A2A_TOKEN"))
        );
        assert_eq!(r.timeout_secs, 120);
        let l = &a2a.remotes["local_box"];
        assert_eq!(l.card_urls().len(), 1);
        assert_eq!(l.credential(), Some(("X-API-Key".to_string(), "LOCAL_KEY")));
        let s = a2a.server.as_ref().unwrap();
        assert_eq!(s.base_url(), "http://127.0.0.1:8710");
        // Every agent's tools see the remotes.
        let seen = cfg.agents["researcher"].sandbox.a2a.as_ref().unwrap();
        assert!(seen.remotes.contains_key("research-desk"));
    }

    #[test]
    fn bad_remotes_and_servers_fail_the_load() {
        let e = load(&format!(
            r#"{AGENTS}
[a2a.remotes.Bad]
url = "ftp://x"
bearer_env = "lower"
header = "X-Key"
timeout_secs = 0

[a2a.server]
bind = "0.0.0.0"
allow_unauthenticated = true
orchestrator = true
agents = ["signer", "ghost"]
"#
        ))
        .unwrap_err();
        for want in [
            "a2a.remotes.Bad: the name must match",
            "a2a.remotes.Bad.url must be an http",
            "a2a.remotes.Bad.bearer_env must name an environment variable",
            "use one credential",
            "`header` and `header_env` go together",
            "a2a.remotes.Bad.timeout_secs must be 1..=3600",
            "allow_unauthenticated needs a loopback bind",
            "orchestrator = true needs an [orchestrator] section",
            "'signer' has no `description`",
            "no [agents.ghost] block",
        ] {
            assert!(e.contains(want), "missing `{want}` in:\n{e}");
        }
    }

    #[test]
    fn a_server_needs_auth_or_an_open_loopback() {
        let e = load(&format!(
            "{AGENTS}\n[a2a.server]\nagents = [\"researcher\"]\n"
        ))
        .unwrap_err();
        assert!(e.contains("set `token_env`"), "{e}");
        assert!(load(&format!(
            "{AGENTS}\n[a2a.server]\nallow_unauthenticated = true\nagents = [\"researcher\"]\n"
        ))
        .is_ok());
        let e = load(&format!("{AGENTS}\n[a2a.server]\ntoken_env = \"T\"\n")).unwrap_err();
        assert!(e.contains("serves nothing"), "{e}");
    }

    #[test]
    fn an_unknown_key_fails() {
        let e = load(&format!(
            "{AGENTS}\n[a2a.remotes.x]\nurl = \"https://x\"\nbearer = \"T\"\n"
        ))
        .unwrap_err();
        assert!(e.contains("unknown field `bearer`"), "{e}");
    }

    #[test]
    fn the_tool_without_remotes_warns() {
        let cfg = load(AGENTS).unwrap();
        let w = validation_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].starts_with("agents.researcher holds the `a2a` tool"),
            "{w:?}"
        );
    }

    /// The commented `[a2a]` block of `config.example.toml`, uncommented
    /// (with its agents and planner), loads.
    #[test]
    fn example_block_uncommented_loads() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [a2a.remotes.research]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 10, "{block:?}");
        let cfg = load(&format!(
            "[orchestrator]\nagent = \"main\"\n{}\n{}",
            AGENTS,
            block.join("\n")
        ))
        .unwrap();
        let a2a = cfg.a2a.unwrap();
        assert_eq!(
            a2a.remotes["research"].bearer_env.as_deref(),
            Some("RESEARCH_A2A_TOKEN")
        );
        let s = a2a.server.unwrap();
        assert!(s.orchestrator);
        assert_eq!(s.agents, ["researcher"]);
    }

    #[test]
    fn names_and_urls() {
        assert!(valid_remote_name("research-desk_2"));
        assert!(!valid_remote_name("-x"));
        assert!(!valid_remote_name("Big"));
        assert!(!valid_remote_name(""));
        assert!(http_url_error("u", "https://h").is_none());
        assert!(http_url_error("u", "https:///path").is_some());
        let v6 = A2aServerConfig {
            bind: "::1".into(),
            ..serde_json::from_value(serde_json::json!({"agents": ["a"]})).unwrap()
        };
        assert_eq!(v6.base_url(), "http://[::1]:8710");
        assert!(v6.is_loopback());
    }
}
