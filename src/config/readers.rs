//! Readers of outside text vs. side effects (audit S10). The fence
//! (`domain::untrusted`) is a label for the model; what keeps a hostile page,
//! A2A reply or MCP result from steering a write, a shell, a signature or an
//! env read is that the agent reading it holds none. Pure over the loaded
//! config (before `fold_default_scopes`: a tool's scope is its agent entry,
//! else `[default_scopes]`, else none = the permissive fallback).
//!
//! | Rule | Kind |
//! |---|---|
//! | An agent with a reader signal holds no side effect | load warning ([`warnings`]) |
//! | The `[orchestrator]` agent on `claude_code` runs `builtin_tools_profile = "none"` | load error ([`errors`]) |
//!
//! | Reader signal (any) | Side effect (any) |
//! |---|---|
//! | `claude_code` profile `research` (WebSearch) | `write_file`, `run_command`, a Privy signing, xmarket exec or Solana write tool — held, on a scope that is not deny-all |
//! | `codex` `web_search` other than `disabled` | `claude_code` profile `editor` / `editor_shell`; `codex` `sandbox = "workspace-write"` |
//! | `http_request` on no scope, or `net_hosts` with `"*"` | an `env_reads` entry on a held tool's scope whose `net_hosts` holds `"*"` (a `$VAR` can go to any host; a credential bound to named hosts, like `a2a`'s, cannot) |
//! | `a2a` | |
//! | an `[[mcp_servers]]` tool (an empty `tools` holds them all) | |
//!
//! Held = the agent's `tools` (empty = every default tool) + its
//! `workspace_tools`, as `bootstrap::tools::agent_base_tools`.

use crate::domain::scope::ToolScope;
use crate::domain::tools::{
    A2A, DEFAULT_TOOLS, MCP_NAME_SEPARATOR, PRIVY_SIGNING_TOOLS, SOLANA_WRITE_TOOLS, XM_EXEC_TOOLS,
};

use super::{AgentConfig, Config};

/// One warning per agent that reads outside text and can act on it
/// (module table), naming both sides and the fix.
pub(crate) fn warnings(cfg: &Config) -> Vec<String> {
    let mut out: Vec<String> = cfg
        .agents
        .iter()
        .filter_map(|(id, agent)| {
            let readers = reader_signals(cfg, agent);
            let effects = side_effects(cfg, agent);
            (!readers.is_empty() && !effects.is_empty()).then(|| {
                format!(
                    "agents.{id} reads text tengu did not write ({}) and holds side effects ({}): a \
                     hostile page can steer them — split the reader from the actor, or narrow \
                     `tools` / scopes (docs/tools.md § Untrusted input)",
                    readers.join(", "),
                    effects.join(", ")
                )
            })
        })
        .collect();
    out.sort();
    out
}

/// The `[orchestrator]` agent on `claude_code` with built-ins on: the planner
/// reads every user message, recall row and step error.
pub(crate) fn errors(cfg: &Config) -> Vec<String> {
    let Some(orch) = &cfg.orchestrator else {
        return Vec::new();
    };
    let Some(agent) = cfg.agents.get(&orch.agent) else {
        return Vec::new();
    };
    let profile = profile(agent);
    if agent.engine != "claude_code" || profile == "none" {
        return Vec::new();
    }
    vec![format!(
        "agents.{}.claude_code.builtin_tools_profile = \"{profile}\": the [orchestrator] agent on \
         claude_code must run \"none\" — the planner reads every message, recall row and step \
         error, and a built-in tool would act on them",
        orch.agent
    )]
}

/// The tools `agent` holds (module footer).
fn held(agent: &AgentConfig) -> Vec<&str> {
    let mut names: Vec<&str> = if agent.tools.is_empty() {
        DEFAULT_TOOLS.to_vec()
    } else {
        agent.tools.iter().map(String::as_str).collect()
    };
    names.extend(agent.workspace_tools.iter().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    names
}

/// `tool`'s scope for `agent`, `None` = the permissive fallback.
fn scope<'a>(cfg: &'a Config, agent: &'a AgentConfig, tool: &str) -> Option<&'a ToolScope> {
    agent
        .scopes
        .get(tool)
        .or_else(|| cfg.default_scopes.get(tool))
}

fn deny_all(s: &ToolScope) -> bool {
    s.fs_roots.is_empty()
        && s.net_hosts.is_empty()
        && s.env_reads.is_empty()
        && s.shell_bins.is_empty()
        && s.wallets.is_empty()
}

fn profile(agent: &AgentConfig) -> &str {
    agent
        .claude_code
        .as_ref()
        .map_or("none", |c| c.builtin_tools_profile.trim())
}

fn codex_value<'a>(
    agent: &'a AgentConfig,
    pick: fn(&'a super::AgentCodexConfig) -> &'a str,
) -> &'a str {
    agent.codex.as_ref().map_or("", |c| pick(c).trim())
}

fn reader_signals(cfg: &Config, agent: &AgentConfig) -> Vec<String> {
    let held = held(agent);
    let mut out = Vec::new();
    if agent.engine == "claude_code" && profile(agent) == "research" {
        out.push("claude_code profile `research`".to_string());
    }
    let web_search = codex_value(agent, |c| c.web_search.as_str());
    if agent.engine == "codex" && !web_search.is_empty() && web_search != "disabled" {
        out.push(format!("codex web_search `{web_search}`"));
    }
    if held.contains(&"http_request") {
        match scope(cfg, agent, "http_request") {
            None => out.push("`http_request` on no scope".to_string()),
            Some(s) if s.net_hosts.iter().any(|h| h == "*") => {
                out.push("`http_request` to `*`".to_string())
            }
            Some(_) => {}
        }
    }
    if held.contains(&A2A) {
        out.push("`a2a`".to_string());
    }
    if !cfg.mcp_servers.is_empty() && agent.tools.is_empty() {
        out.push("every [[mcp_servers]] tool".to_string());
    } else if let Some(t) = held.iter().find(|t| t.contains(MCP_NAME_SEPARATOR)) {
        out.push(format!("`{t}`"));
    }
    out
}

fn side_effects(cfg: &Config, agent: &AgentConfig) -> Vec<String> {
    let held = held(agent);
    let acts = |t: &str| {
        ["write_file", "run_command"].contains(&t)
            || PRIVY_SIGNING_TOOLS.contains(&t)
            || XM_EXEC_TOOLS.contains(&t)
            || SOLANA_WRITE_TOOLS.contains(&t)
    };
    let mut out: Vec<String> = held
        .iter()
        .filter(|t| acts(t) && !scope(cfg, agent, t).is_some_and(deny_all))
        .map(|t| format!("`{t}`"))
        .collect();
    let mut env: Vec<&str> = held
        .iter()
        .filter_map(|t| scope(cfg, agent, t))
        .filter(|s| s.net_hosts.iter().any(|h| h == "*"))
        .flat_map(|s| s.env_reads.iter().map(String::as_str))
        .collect();
    env.sort_unstable();
    env.dedup();
    if !env.is_empty() {
        out.push(format!("env reads {env:?}"));
    }
    let p = profile(agent);
    if agent.engine == "claude_code" && (p == "editor" || p == "editor_shell") {
        out.push(format!("claude_code profile `{p}`"));
    }
    if agent.engine == "codex" && codex_value(agent, |c| c.sandbox.as_str()) == "workspace-write" {
        out.push("codex sandbox `workspace-write`".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Config {
        toml::from_str(toml).expect("config parses")
    }

    const AGENT: &str = "[agents.r]\nengine = \"openrouter\"\nmodel = \"m\"\n";

    #[test]
    fn a_reader_with_a_writer_warns_naming_both() {
        let cfg = load(&format!(
            "{AGENT}tools = [\"http_request\", \"write_file\"]\n"
        ));
        let w = warnings(&cfg);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`http_request` on no scope") && w[0].contains("`write_file`"));
        // An empty `tools` = every default tool: both sides.
        let w = warnings(&load(AGENT));
        assert!(
            w[0].contains("`run_command`") && w[0].contains("sign_message"),
            "{w:?}"
        );
    }

    #[test]
    fn narrow_hosts_or_deny_all_writers_are_quiet() {
        let cfg = load(&format!(
            "{AGENT}tools = [\"http_request\", \"write_file\"]\n\
             [agents.r.scopes.http_request]\nnet_hosts = [\"api.example.com\"]\n"
        ));
        assert!(warnings(&cfg).is_empty());
        let cfg = load(&format!(
            "{AGENT}tools = [\"http_request\", \"write_file\"]\n\
             [default_scopes.write_file]\n"
        ));
        assert!(warnings(&cfg).is_empty());
        // A reader alone, or an actor alone, is fine.
        assert!(warnings(&load(&format!("{AGENT}tools = [\"a2a\"]\n"))).is_empty());
        assert!(warnings(&load(&format!("{AGENT}tools = [\"write_file\"]\n"))).is_empty());
        // A credential bound to named hosts is no leak.
        let cfg = load(&format!(
            "{AGENT}tools = [\"a2a\", \"write_file\"]\n[default_scopes.write_file]\n\
             [default_scopes.a2a]\nnet_hosts = [\"127.0.0.1\"]\nenv_reads = [\"TOKEN\"]\n"
        ));
        assert!(warnings(&cfg).is_empty(), "{:?}", warnings(&cfg));
    }

    #[test]
    fn env_reads_and_builtins_count_as_side_effects() {
        let cfg = load(&format!(
            "{AGENT}tools = [\"http_request\"]\n\
             [agents.r.scopes.http_request]\nnet_hosts = [\"*\"]\nenv_reads = [\"GITHUB_TOKEN\"]\n"
        ));
        let w = warnings(&cfg);
        assert!(
            w[0].contains("`http_request` to `*`") && w[0].contains("GITHUB_TOKEN"),
            "{w:?}"
        );
        let cfg = load(
            "[agents.r]\nengine = \"codex\"\nmodel = \"m\"\ntools = [\"read_file\"]\n\
             [agents.r.codex]\nweb_search = \"live\"\nsandbox = \"workspace-write\"\n",
        );
        let w = warnings(&cfg);
        assert!(
            w[0].contains("web_search `live`") && w[0].contains("workspace-write"),
            "{w:?}"
        );
        let cfg = load(
            "[agents.r]\nengine = \"claude_code\"\nmodel = \"m\"\ntools = [\"run_command\"]\n\
             [agents.r.claude_code]\nbuiltin_tools_profile = \"research\"\n",
        );
        assert!(warnings(&cfg)[0].contains("profile `research`"));
    }

    #[test]
    fn mcp_tools_are_readers() {
        let mcp = "[[mcp_servers]]\nname = \"srv\"\ntransport = \"stdio\"\ncommand = [\"x\"]\n";
        let w = warnings(&load(&format!(
            "{mcp}{AGENT}tools = [\"srv__q\", \"write_file\"]\n"
        )));
        assert!(w[0].contains("`srv__q`"), "{w:?}");
        let w = warnings(&load(&format!("{mcp}{AGENT}")));
        assert!(w[0].contains("every [[mcp_servers]] tool"), "{w:?}");
    }

    #[test]
    fn a_claude_code_orchestrator_runs_profile_none() {
        let base =
            "[orchestrator]\nagent = \"p\"\n[agents.p]\nengine = \"claude_code\"\nmodel = \"m\"\n";
        assert!(errors(&load(base)).is_empty(), "no block = none");
        let cfg = load(&format!(
            "{base}[agents.p.claude_code]\nbuiltin_tools_profile = \"read_only\"\n"
        ));
        let e = errors(&cfg);
        assert!(e.len() == 1 && e[0].contains("\"read_only\""), "{e:?}");
        let cfg = load(
            "[orchestrator]\nagent = \"p\"\n[agents.p]\nengine = \"openrouter\"\nmodel = \"m\"\n",
        );
        assert!(errors(&cfg).is_empty());
    }

    /// The shipped configs this rule flags today — a new pair is a decision
    /// (split the agent, narrow it, or list it here with the reason).
    #[test]
    fn shipped_configs_flag_only_the_known_agents() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("sandboxes"))
            .unwrap()
            .map(|e| e.unwrap().path().join("config.toml"))
            .filter(|p| p.exists())
            .collect();
        files.push(root.join("config.example.toml"));
        let mut flagged = Vec::new();
        for path in files {
            let raw = std::fs::read_to_string(&path).unwrap();
            let cfg: Config =
                toml::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let name = match path.file_name().unwrap().to_string_lossy().as_ref() {
                "config.toml" => path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                other => other.trim_end_matches(".toml").to_string(),
            };
            for w in warnings(&cfg) {
                let agent = w
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .trim_start_matches("agents.");
                flagged.push(format!("{name}/{agent}"));
            }
            assert!(
                errors(&cfg).is_empty(),
                "{}: {:?}",
                path.display(),
                errors(&cfg)
            );
        }
        flagged.sort();
        // S14 gives `main`, `probe` and `planner` explicit `tools`.
        assert_eq!(
            flagged,
            [
                "a2a-lab/planner",
                "config.example/main",
                "lping/crypto_researcher",
                "lping/lping",
                "storage-test/storage",
                "tor-check/probe",
            ]
        );
    }
}
