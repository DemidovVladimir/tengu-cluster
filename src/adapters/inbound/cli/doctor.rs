//! `tengu status` / `tengu doctor` (incl. `--tor` exit check and `--live`,
//! the running `tengu run`: heartbeat file + `loop/1` / `feed/1` rows →
//! `domain::runtime::live_verdict`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::adapters::outbound::engines::build_engine;
use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::runtime_store::read_heartbeat;
use crate::bootstrap::runtime::{agent_workspace, runner_name, runtime_state_dir};
use crate::config::{Config, RuntimeProfile};
use crate::domain::observation::{now_ms, Observation, Observed};
use crate::domain::runtime::{live_verdict, FeedHealth, HeartbeatRead, LiveKnobs, LoopHealth};
use crate::ports::observation::ObservationStore;

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
/// build, or with `--live` when the sandbox's `tengu run` is not live; the
/// Docker `HEALTHCHECK` relies on that exit code.
pub(super) async fn run_doctor(config: &Config, tor_check: bool, live: bool) -> Result<()> {
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
    }

    doctor_egress(tor_check, &mut failures).await;
    if live {
        doctor_live(config, &mut failures).await;
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
