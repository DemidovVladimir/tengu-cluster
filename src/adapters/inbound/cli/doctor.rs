//! `tengu status` / `tengu doctor` (incl. `--tor` exit check, `--live`, the
//! running `tengu run`: heartbeat file + `loop/1` / `feed/1` rows →
//! `domain::runtime::live_verdict`, and `--engines`, a tool-using smoke turn
//! per agent → `domain::engine_smoke`).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::adapters::outbound::engines::{build_engine, build_step_engine, StepOpts};
use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::runtime_store::read_heartbeat;
use crate::adapters::outbound::secrets::{process_secret_registry, SanitizedToolExecutor};
use crate::application::chat::tool_loop::collect_engine_response;
use crate::bootstrap::runtime::{agent_workspace, runner_name, runtime_state_dir};
use crate::bootstrap::tools::{
    build_tool_executor, compute_base_tools, grant_workspace_root, subagent_config,
};
use crate::config::{AgentClaudeCodeConfig, AgentConfig, Config, RuntimeProfile};
use crate::domain::engine_smoke::{
    runs_summary, smoke_prompt, smoke_verdict, SMOKE_MAX_ROUNDS, SMOKE_SYSTEM, SMOKE_TOOLS,
    TOKEN_FILE_PREFIX,
};
use crate::domain::message::{Message, Role, ToolCall, ToolDef, ToolRun};
use crate::domain::observation::{now_ms, Observation, Observed};
use crate::domain::runtime::{live_verdict, FeedHealth, HeartbeatRead, LiveKnobs, LoopHealth};
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::EngineContext;
use crate::ports::observation::ObservationStore;
use crate::ports::tool_activity::ToolActivityPort;

fn format_diagnostics_compact(d: &crate::ports::engine::EngineDiagnostics) -> String {
    let caps = &d.capabilities;
    format!(
        "model={} endpoint={} transport={} context={} output_cap={} streaming={}",
        d.configured_model.as_deref().unwrap_or("n/a"),
        d.endpoint.as_deref().unwrap_or("n/a"),
        d.transport.as_deref().unwrap_or("n/a"),
        caps.context_window,
        caps.max_output_tokens_per_turn,
        caps.supports_streaming,
    )
}

pub(super) fn print_status(config: &Config, profile: RuntimeProfile) {
    println!();
    println!("  TENGU CLUSTER — Status");
    println!("  ─────────────────────────────────────");
    println!("  Profile:  {:?}", profile);
    println!("  Agents:   {}", config.agents.len());
    for (id, ac) in &config.agents {
        println!(
            "    - {} ({}/{}){}",
            id,
            ac.engine,
            ac.model,
            if ac.default { " [default]" } else { "" }
        );
        match build_engine(id, ac, config.claude_code.as_ref()) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "      diagnostics: {}",
                    format_diagnostics_compact(&diagnostics)
                );
            }
            Err(err) => {
                println!("      diagnostics: unavailable ({})", err);
            }
        }
    }
    println!("  Hub:      {}:{}", config.hub.bind, config.hub.port);
    println!("  ─────────────────────────────────────");
    println!();
}

/// `tengu doctor` — build every configured agent's engine and print its
/// diagnostics. Returns `Err` (→ non-zero exit) when any engine fails to
/// build, with `--live` when the sandbox's `tengu run` is not live, or with
/// `--engines` when an agent's smoke turn fails; the Docker `HEALTHCHECK`
/// relies on that exit code.
pub(super) async fn run_doctor(
    config: &Config,
    tor_check: bool,
    live: bool,
    engines: bool,
) -> Result<()> {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    println!("  Backend diagnostics:");
    let mut failures: Vec<String> = Vec::new();
    for (id, ac) in &config.agents {
        match build_engine(id, ac, config.claude_code.as_ref()) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "    {}: engine={} {}",
                    id,
                    diagnostics.engine_id,
                    format_diagnostics_compact(&diagnostics)
                );
            }
            Err(err) => {
                println!("    {}: backend init error: {}", id, err);
                failures.push(format!("{id}: {err}"));
            }
        }
        // A `claude_code` engine builds without its CLI; its turns then fail.
        // The shipped image has none, so a container passed this healthcheck
        // with agents that could not run.
        if ac.engine == "claude_code" {
            let cli = config
                .claude_code
                .as_ref()
                .map_or("claude", |c| c.cli_path.as_str());
            if find_executable(cli, std::env::var_os("PATH")).is_none() {
                println!("    {id}: Claude CLI `{cli}` not found");
                failures.push(format!(
                    "{id}: Claude CLI `{cli}` not found — install it or set [claude_code] cli_path"
                ));
            }
        }
    }

    doctor_egress(tor_check, &mut failures).await;
    if live {
        doctor_live(config, &mut failures).await;
    }
    if engines {
        doctor_engines(config, &mut failures).await;
    }

    println!("  ─────────────────────────────────────");
    println!();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "doctor: {} check(s) failed:\n{}",
            failures.len(),
            failures
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// `[egress]` block of `tengu doctor`: prints the installed policy, checks
/// the proxy port accepts TCP, and with `--tor` asks check.torproject.org
/// (through the tool client) whether traffic exits via Tor.
async fn doctor_egress(tor_check: bool, failures: &mut Vec<String>) {
    let policy = crate::adapters::outbound::egress::policy();
    let cfg = policy.config();
    println!("  Egress:");
    println!("    network: {}", policy.network());
    println!(
        "    proxy: {}",
        cfg.proxy.as_deref().unwrap_or("none (direct)")
    );
    println!(
        "    llm api: {}",
        if policy.route_llm_api() {
            "via proxy"
        } else {
            "direct"
        }
    );
    println!(
        "    hosts: allow={:?} deny={:?} https_only={}",
        cfg.allow_hosts, cfg.deny_hosts, cfg.https_only
    );
    let shell = match (policy.is_isolated_shell(), cfg.proxy.is_some()) {
        (true, _) => "isolated (sandbox-exec: only the proxy port)",
        (false, true) => "proxy_env (advisory — programs may ignore it)",
        (false, false) => "direct",
    };
    println!("    shell: {shell}");
    println!(
        "    audit: {}",
        policy
            .audit_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "off".to_string())
    );

    if let Some(url) = cfg
        .proxy
        .as_deref()
        .and_then(|p| reqwest::Url::parse(p).ok())
    {
        let host = url
            .host_str()
            .unwrap_or("")
            .trim_matches(|c| c == '[' || c == ']');
        let addr = format!("{}:{}", host, url.port().unwrap_or(0));
        let connect = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::TcpStream::connect(addr.as_str()),
        )
        .await;
        match connect {
            Ok(Ok(_)) => println!("    proxy port: {addr} reachable"),
            Ok(Err(e)) => {
                println!("    proxy port: {addr} UNREACHABLE ({e})");
                failures.push(format!(
                    "egress: proxy {addr} unreachable ({e}) — is tor running?"
                ));
            }
            Err(_) => {
                println!("    proxy port: {addr} UNREACHABLE (timeout)");
                failures.push(format!("egress: proxy {addr} connect timed out"));
            }
        }
    }

    if tor_check {
        match tor_exit_check(&policy).await {
            Ok((true, ip)) => println!("    tor: IsTor=true exit={ip}"),
            Ok((false, ip)) => {
                println!("    tor: IsTor=false exit={ip}");
                failures.push(format!("egress: traffic exits at {ip}, NOT via Tor"));
            }
            Err(e) => {
                println!("    tor: check failed ({e:#})");
                failures.push(format!("egress: tor check failed: {e:#}"));
            }
        }
    }
}

/// `--live` block: one line per check of `live_verdict` (heartbeat, loops,
/// feeds); failing checks fail the doctor.
async fn doctor_live(config: &Config, failures: &mut Vec<String>) {
    let (sandbox, dir) = (runner_name(config), runtime_state_dir(config));
    println!("  Runtime (live): sandbox {sandbox} · {}", dir.display());
    let heartbeat = match read_heartbeat(&dir, &sandbox) {
        Ok(Some(hb)) => HeartbeatRead::Found(hb),
        Ok(None) => HeartbeatRead::Missing,
        Err(e) => HeartbeatRead::Unreadable(format!("{e:#}")),
    };
    let rows = health_rows(config, &heartbeat).await;
    let knobs = LiveKnobs {
        heartbeat_stale_secs: config.runtime.heartbeat_stale_secs,
    };
    let report = live_verdict(&sandbox, &heartbeat, &rows, now_ms(), knobs);
    for c in &report.checks {
        println!(
            "    {} {:<24} {}",
            if c.ok { "ok  " } else { "FAIL" },
            c.subject,
            c.detail
        );
        if !c.ok {
            failures.push(format!("live: {} — {}", c.subject, c.detail));
        }
    }
    println!("    => {}", if report.ok() { "live" } else { "NOT live" });
}

/// Row of a `local` agent `--engines` never contacts (operator rule
/// 2026-09-30: no local model on the Mac).
const LOCAL_SKIPPED: &str = "skipped (local models run on the operator's PC)";

/// `--engines` block (`x-engine-matrix-smoke`): the fixed smoke turn
/// (`domain::engine_smoke`: `list_directory` → `read_file` → answer with the
/// token) on every agent's own engine + model, one row per agent:
/// agent | engine | model | ok | tools called | secs. A failed row fails the
/// doctor.
///
/// | Per agent | How |
/// |---|---|
/// | workspace | a fresh temp dir holding a random token file + a decoy |
/// | tools | `list_directory` + `read_file` only, in-process executor (secrets redacted); Claude Code: the same two through `tengu mcp-bridge`, built-ins off |
/// | scopes | the agent's own; the temp workspace granted on each, like a `run-agent` step — Claude Code's bridge through the explicit engine option (`build_step_engine`, `TENGU_BRIDGE_GRANT_WORKSPACE`), never a process-wide env var |
/// | bound | `min(limits.max_tool_rounds, SMOKE_MAX_ROUNDS)` rounds, `limits.step_timeout_secs` |
/// | `local` with a loopback `base_url`, on macOS | [`LOCAL_SKIPPED`], never contacted |
/// | its own scopes deny a smoke tool outright (`read_file = {}`) | skipped, naming the tools — a deny-all scope stays one in a `run-agent` step, so the smoke cannot run (`sandboxes/jev-exec`'s architect) |
async fn doctor_engines(config: &Config, failures: &mut Vec<String>) {
    let mut secrets: Option<Arc<SecretRegistry>> = None;
    let mut agents: Vec<(&String, &AgentConfig)> = config.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    let width = |f: &dyn Fn(&(&String, &AgentConfig)) -> usize, min: usize| {
        agents.iter().map(f).max().unwrap_or(min).max(min)
    };
    let wa = width(&|(id, _)| id.len(), 5);
    let we = width(&|(_, a)| a.engine.len(), 6);
    let wm = width(&|(_, a)| a.model.len(), 5);
    let wt = SMOKE_TOOLS.join(", ").len().max("tools called".len());
    println!(
        "  Engines (smoke turn: {} in a temp workspace):",
        SMOKE_TOOLS.join(" + ")
    );
    println!(
        "    {:<wa$}  {:<we$}  {:<wm$}  {:<4}  {:<wt$}  {:>6}",
        "agent", "engine", "model", "ok", "tools called", "secs"
    );
    for (id, agent) in agents {
        if agent.engine == "local" && cfg!(target_os = "macos") && local_on_loopback(agent) {
            println!(
                "    {id:<wa$}  {:<we$}  {:<wm$}  {LOCAL_SKIPPED}",
                agent.engine, agent.model
            );
            continue;
        }
        let denied = smoke_tools_denied(agent);
        if !denied.is_empty() {
            println!(
                "    {id:<wa$}  {:<we$}  {:<wm$}  skipped ({} denied by its own scopes)",
                agent.engine,
                agent.model,
                denied.join(", ")
            );
            continue;
        }
        let secrets = secrets.get_or_insert_with(|| Arc::new(process_secret_registry(None)));
        let started = std::time::Instant::now();
        let (ok, called, problem) = match smoke_agent(config, id, agent, secrets).await {
            Ok((runs, text, token)) => {
                let v = smoke_verdict(&SMOKE_TOOLS, &runs, &text, &token);
                (v.ok(), runs_summary(&runs), v.problems())
            }
            Err(e) => (false, "-".to_string(), format!("{e:#}")),
        };
        println!(
            "    {id:<wa$}  {:<we$}  {:<wm$}  {:<4}  {called:<wt$}  {:>6.1}",
            agent.engine,
            agent.model,
            if ok { "ok" } else { "FAIL" },
            started.elapsed().as_secs_f64(),
        );
        if !ok {
            println!("      {problem}");
            failures.push(format!(
                "engines: {id} ({} {}): {problem}",
                agent.engine, agent.model
            ));
        }
    }
}

/// The smoke tools the agent's own scopes deny outright (every field empty):
/// the workspace grant leaves those a deny.
fn smoke_tools_denied(agent: &AgentConfig) -> Vec<&'static str> {
    SMOKE_TOOLS
        .iter()
        .copied()
        .filter(|t| agent.scopes.get(*t).is_some_and(|s| s.is_deny_all()))
        .collect()
}

/// The agent's `[agents.<n>.local] base_url` (default when absent) names this
/// machine: `localhost` or a loopback / unspecified address.
fn local_on_loopback(agent: &AgentConfig) -> bool {
    let base_url = agent.local.clone().unwrap_or_default().base_url;
    let Ok(url) = reqwest::Url::parse(&base_url) else {
        return false;
    };
    let host = url
        .host_str()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// Tool activity is read from the turn's `tool_runs`, not published.
struct QuietActivity;
impl ToolActivityPort for QuietActivity {
    fn publish_tool_activity(&self, _call: &ToolCall) {}
}

/// One agent's smoke turn → `(tool runs, final text, token)`.
async fn smoke_agent(
    config: &Config,
    id: &str,
    agent: &AgentConfig,
    secrets: &Arc<SecretRegistry>,
) -> Result<(Vec<ToolRun>, String, String)> {
    let dir = tempfile::tempdir().context("temp workspace")?;
    // Canonical: scope checks compare resolved paths (macOS /var → /private/var).
    let ws = std::fs::canonicalize(dir.path()).context("temp workspace")?;
    let token = format!("smoke-{}", uuid::Uuid::new_v4().simple());
    let token_file = format!("{TOKEN_FILE_PREFIX}{}.txt", uuid::Uuid::new_v4().simple());
    std::fs::write(ws.join(token_file), &token).context("write the token file")?;
    std::fs::write(ws.join("notes.txt"), "not the token\n").context("write the decoy")?;

    let mut cfg = subagent_config(agent);
    cfg.workspace = Some(ws.clone());
    cfg.tools = SMOKE_TOOLS.iter().map(|t| t.to_string()).collect();
    // Claude's built-ins (Read, Bash) would bypass the tengu tools under test.
    cfg.claude_code = Some(AgentClaudeCodeConfig {
        builtin_tools_profile: "none".to_string(),
    });
    grant_workspace_root(&mut cfg.scopes, &ws);
    // Run like a `run-agent` step: a Claude Code bridge grants the temp
    // workspace on every configured scope too — an explicit engine option,
    // never this process's env.
    let engine = build_step_engine(
        id,
        &cfg,
        config.claude_code.as_ref(),
        StepOpts {
            grant_workspace: true,
            summary_file: None,
            config_file: None,
        },
    )
    .context("build engine")?;

    let tools: Vec<ToolDef> = compute_base_tools(true, false, &[])
        .into_iter()
        .filter(|t| SMOKE_TOOLS.contains(&t.name.as_str()))
        .collect();
    let executor = build_tool_executor(
        &ws,
        &tools,
        &crate::application::skills::registry::SkillRegistry::new(Vec::new()),
        &None,
        secrets,
        Arc::new(QuietActivity),
        None,
        None,
        &cfg,
        &[],
    )
    .context("tool executor unavailable (egress tool client)")?;
    let executor = SanitizedToolExecutor::new(Arc::new(executor), Arc::clone(secrets));

    let message = |role, content: String| Message {
        role,
        content,
        tool_call_id: None,
        tool_calls: None,
    };
    let messages = [
        message(Role::System, SMOKE_SYSTEM.to_string()),
        message(Role::User, smoke_prompt()),
    ];
    let limits = &cfg.limits;
    let rounds = limits.max_tool_rounds.clamp(1, SMOKE_MAX_ROUNDS);
    let context = EngineContext {
        workspace: Some(ws.clone()),
        system_prompt: Some(SMOKE_SYSTEM.to_string()),
        bridge_tools: engine.manages_own_workspace().then(|| tools.clone()),
        max_tool_rounds: Some(rounds),
        max_mcp_result_chars: Some(limits.max_mcp_result_chars),
        mcp_servers: Vec::new(),
    };
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(limits.step_timeout_secs),
        collect_engine_response(
            engine.as_ref(),
            &messages,
            &tools,
            &context,
            Some(&executor),
            None,
            None,
            None,
            rounds,
            limits.max_tool_result_chars,
            limits.stream_event_timeout_secs,
            limits.compact_result_limit,
        ),
    )
    .await
    .with_context(|| format!("no answer within {}s", limits.step_timeout_secs))??;
    Ok((resp.tool_runs, resp.text, token))
}

/// `loop/1` rows of every configured loop and `feed/1` rows of every feed
/// the heartbeat lists, from each agent workspace whose observation store
/// exists (never created here). Unreadable stores are skipped.
async fn health_rows(config: &Config, heartbeat: &HeartbeatRead) -> Vec<Observation> {
    let mut keys: BTreeSet<String> = config
        .decision_loops
        .keys()
        .map(|n| Observation::key_for(LoopHealth::SCHEMA, n))
        .collect();
    if let HeartbeatRead::Found(hb) = heartbeat {
        keys.extend(
            hb.loops
                .keys()
                .map(|n| Observation::key_for(LoopHealth::SCHEMA, n)),
        );
        keys.extend(
            hb.feeds
                .keys()
                .map(|n| Observation::key_for(FeedHealth::SCHEMA, n)),
        );
    }
    let keys: Vec<String> = keys.into_iter().collect();
    let workspaces: BTreeSet<PathBuf> = config
        .agents
        .keys()
        .map(|a| agent_workspace(config, a))
        .filter(|ws| ws.join(".tengu").join("observations.db").exists())
        .collect();
    let mut rows = Vec::new();
    for ws in workspaces {
        let Ok(store) = SqliteObservationStore::open(&ws) else {
            continue;
        };
        if let Ok(found) = store.get_many(&keys).await {
            rows.extend(found.into_iter().flatten());
        }
    }
    rows
}

async fn tor_exit_check(
    policy: &crate::adapters::outbound::egress::EgressPolicy,
) -> Result<(bool, String)> {
    let body: serde_json::Value = policy
        .tool_client(std::time::Duration::from_secs(60))?
        .get("https://check.torproject.org/api/ip")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok((
        body["IsTor"].as_bool().unwrap_or(false),
        body["IP"].as_str().unwrap_or("unknown").to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_agent(base_url: Option<&str>) -> AgentConfig {
        let mut agent = Config::default().agents.remove("main").unwrap();
        agent.engine = "local".to_string();
        agent.model = "gemma4:latest".to_string();
        agent.local = base_url.map(|u| crate::config::AgentLocalConfig {
            base_url: u.to_string(),
            api_key_env: String::new(),
        });
        agent
    }

    /// `base_url` → names this machine. Not a URL (an unset
    /// `${TENGU_MATRIX_LOCAL_BASE_URL}`) is not loopback: the engine then
    /// fails to build the request, before any connection.
    #[test]
    fn loopback_base_urls() {
        let cases = [
            (Some("http://127.0.0.1:11434/v1"), true),
            (Some("http://localhost:8888"), true),
            (Some("http://LOCALHOST:8888"), true),
            (Some("http://api.localhost:1234"), true),
            (Some("http://[::1]:11434"), true),
            (Some("http://0.0.0.0:11434"), true),
            (Some("http://127.1:11434"), true),
            (None, true), // default: http://127.0.0.1:8888
            (Some("http://192.168.1.20:11434/v1"), false),
            (Some("http://gaming-pc.lan:11434/v1"), false),
            (Some("${TENGU_MATRIX_LOCAL_BASE_URL}"), false),
        ];
        for (url, want) in cases {
            assert_eq!(local_on_loopback(&local_agent(url)), want, "{url:?}");
        }
    }

    /// sandboxes/jev-exec's architect denies `read_file` / `list_directory`
    /// outright (its only hand is `run_command` → `tengu`): its smoke row is
    /// skipped, not failed; the executor's runs.
    #[test]
    fn smoke_skips_an_agent_whose_scopes_deny_the_smoke_tools() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/jev-exec/config.toml");
        let cfg = Config::load(&path).expect("jev-exec config");
        assert_eq!(
            smoke_tools_denied(&cfg.agents["architect"]),
            ["list_directory", "read_file"]
        );
        assert!(smoke_tools_denied(&cfg.agents["executor"]).is_empty());
    }

    /// On macOS a loopback `local` agent is reported skipped and never
    /// contacted: nothing connects to the listener its `base_url` names.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn engines_never_contact_a_loopback_local_agent_on_macos() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let mut config = Config::default();
        config.agents.clear();
        config
            .agents
            .insert("gemma".to_string(), local_agent(Some(&url)));
        let mut failures = Vec::new();
        doctor_engines(&config, &mut failures).await;
        assert!(failures.is_empty(), "{failures:?}");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "the doctor connected to the local server"
        );
    }
}

/// `cli` as an executable file: a path (with a `/`) as given, else the first
/// match in `path_var` (`PATH`).
fn find_executable(cli: &str, path_var: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    fn runnable(p: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    }
    let expanded = crate::config::paths::expand_tilde(std::path::Path::new(cli));
    if cli.contains('/') {
        return runnable(&expanded).then_some(expanded);
    }
    std::env::split_paths(&path_var?)
        .map(|dir| dir.join(cli))
        .find(|p| runnable(p))
}

#[cfg(test)]
mod find_executable_tests {
    use super::find_executable;

    #[cfg(unix)]
    #[test]
    fn finds_the_cli_on_path_or_by_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("claude");
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = Some(dir.path().as_os_str().to_owned());
        assert_eq!(find_executable("claude", path.clone()), Some(cli.clone()));
        assert_eq!(find_executable("nope", path.clone()), None);
        assert_eq!(
            find_executable(cli.to_str().unwrap(), None),
            Some(cli.clone())
        );
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_executable("claude", path), None, "not executable");
    }
}
