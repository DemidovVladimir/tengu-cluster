//! `tengu status` / `tengu doctor` (incl. `--tor` exit check).

use anyhow::Result;

use crate::adapters::outbound::engines::build_engine;
use crate::config::{Config, RuntimeProfile};

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
/// build; the Docker `HEALTHCHECK` relies on that exit code.
pub(super) async fn run_doctor(config: &Config, tor_check: bool) -> Result<()> {
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
