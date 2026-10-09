//! `tengu keys` — the operator side of the seal proxy
//! (`docs/sealed-keys-2026-10-09.md`).
//!
//! | Action | Does | Network | Touch ID |
//! |---|---|---|---|
//! | `setup [--proxy <url>]` | list the ssh-agent keys; print the `CLIENTS` entries for `wrangler.toml` and the `[keys]` block | `GET /healthz` when a proxy is known | no |
//! | `status` | routes, session, `/whoami` | `/healthz`, `/session`, `/whoami` | yes (new session) |
//! | `check <route> [--path p]` | one GET through the proxy; prints status + size only | `/<route>/…` | yes (new session) |
//!
//! Nothing here prints a provider key (there is none on this machine) or
//! the session token.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::adapters::outbound::keys as k;
use crate::config::keys::KeysConfig;
use crate::config::Config;

#[derive(Subcommand, Debug)]
pub(crate) enum KeysAction {
    /// List this machine's ssh-agent keys and print the CLIENTS entries for
    /// cloudflare/seal-worker/wrangler.toml plus the [keys] config block.
    Setup {
        /// Worker origin, e.g. https://tengu-seal.<subdomain>.workers.dev
        /// (default: [keys] proxy)
        #[arg(long)]
        proxy: Option<String>,
        /// SSH-agent socket (default: [keys] agent_socket, else $SSH_AUTH_SOCK)
        #[arg(long)]
        agent_socket: Option<String>,
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Show the Worker's routes, start a session and check it with /whoami.
    Status {
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// One GET through the proxy on a route; prints status and size only.
    Check {
        /// Worker route name (e.g. openrouter)
        route: String,
        /// Path (and ?query) under the route's upstream, e.g. v1/models
        #[arg(long, default_value = "")]
        path: String,
        #[arg(long)]
        sandbox: Option<String>,
    },
}

/// `[keys]` of the sandbox file, else of the base config — without
/// starting a session. A sandbox's `[egress]` policy is installed too, so
/// the proxy is reached the way that sandbox reaches it (startup installed
/// the base policy only).
fn keys_config(base: &Config, sandbox: Option<&str>) -> Result<KeysConfig> {
    match sandbox {
        None => Ok(base.keys.clone()),
        Some(name) => {
            let path = std::path::PathBuf::from("sandboxes")
                .join(name)
                .join("config.toml");
            let cfg =
                Config::load(&path).with_context(|| format!("load sandbox {}", path.display()))?;
            crate::adapters::outbound::egress::install(&cfg.egress)?;
            Ok(cfg.keys)
        }
    }
}

pub(crate) async fn run_keys(base: &Config, action: KeysAction) -> Result<()> {
    match action {
        KeysAction::Setup {
            proxy,
            agent_socket,
            sandbox,
        } => {
            let mut cfg = keys_config(base, sandbox.as_deref())?;
            if proxy.is_some() {
                cfg.proxy = proxy;
            }
            if agent_socket.is_some() {
                cfg.agent_socket = agent_socket;
            }
            setup(&cfg).await
        }
        KeysAction::Status { sandbox } => status(&keys_config(base, sandbox.as_deref())?).await,
        KeysAction::Check {
            route,
            path,
            sandbox,
        } => check(&keys_config(base, sandbox.as_deref())?, &route, &path).await,
    }
}

#[derive(Deserialize)]
struct Healthz {
    routes: Vec<String>,
}

async fn healthz(client: &reqwest::Client, origin: &str) -> Result<Vec<String>> {
    let resp = client
        .get(format!("{origin}/healthz"))
        .send()
        .await
        .with_context(|| format!("GET {origin}/healthz"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{origin}/healthz answered {status}: {}",
            k::worker_error(&body)
        );
    }
    Ok(serde_json::from_str::<Healthz>(&body)
        .context("malformed /healthz answer")?
        .routes)
}

async fn setup(cfg: &KeysConfig) -> Result<()> {
    if let Some(proxy) = &cfg.proxy {
        if let Some(e) = cfg.validation_errors(&[]).first() {
            bail!("{e}");
        }
        let origin = k::proxy(cfg)?;
        match healthz(&k::http_client()?, &origin).await {
            Ok(routes) => println!("Worker {proxy}: up · routes {}", routes.join(", ")),
            Err(e) => println!("Worker {proxy}: not reachable yet ({e:#})"),
        }
        println!();
    }
    let socket = k::agent_socket(cfg)?;
    let ids = k::agent::identities(&socket)?;
    if ids.is_empty() {
        println!(
            "No usable key in the ssh-agent at {} (need ecdsa-sha2-nistp256 or ssh-ed25519).",
            socket.display()
        );
        println!("Secretive: create a key (Touch ID optional), then rerun.");
        return Ok(());
    }
    // The routes this config uses — the least each key needs.
    let routes = {
        let r = cfg.routes();
        let r = if r.is_empty() {
            vec!["openrouter".to_string()]
        } else {
            r
        };
        r.iter()
            .map(|x| format!("\"{x}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("1. In cloudflare/seal-worker/wrangler.toml, CLIENTS (then `npx wrangler deploy`):");
    println!();
    let entries: Vec<String> = ids
        .iter()
        .map(|id| {
            let label = if id.comment.is_empty() {
                "tengu".to_string()
            } else {
                id.comment.replace(['"', '\\'], "")
            };
            format!(
                "  \"{}\": {{ \"label\": \"{label}\", \"session_hours\": 24, \"routes\": [{routes}] }}",
                id.key.fingerprint()
            )
        })
        .collect();
    println!("{}", entries.join(",\n"));
    println!();
    println!("2. In your config (base or sandbox):");
    println!();
    println!("[keys]");
    println!(
        "proxy = \"{}\"",
        cfg.proxy
            .as_deref()
            .unwrap_or("https://tengu-seal.<subdomain>.workers.dev")
    );
    println!("client = \"{}\"", ids[0].comment.replace(['"', '\\'], ""));
    if let Some(s) = &cfg.agent_socket {
        println!("agent_socket = \"{s}\"");
    }
    println!("[keys.env]");
    println!("OPENROUTER_BASE_URL = \"openrouter\"");
    println!("OPENROUTER_API_KEY = \"@session\"");
    Ok(())
}

async fn status(cfg: &KeysConfig) -> Result<()> {
    let origin = k::proxy(cfg)?;
    let client = k::http_client()?;
    println!("proxy:   {origin}");
    let routes = healthz(&client, &origin).await?;
    println!("routes:  {}", routes.join(", "));
    println!("[keys.env]:");
    for (var, value) in &cfg.env {
        let note = match crate::config::keys::route_of(value) {
            _ if value == crate::config::keys::SESSION_VALUE => String::new(),
            r if routes.iter().any(|x| x == r) => String::new(),
            r => format!("  — NO route '{r}' on the Worker"),
        };
        println!("  {var} ← {value}{note}");
    }
    let s = k::mint(cfg, &client, &cfg.routes()).await?;
    let who: serde_json::Value = client
        .get(format!("{origin}/whoami"))
        .bearer_auth(&s.token)
        .send()
        .await?
        .json()
        .await
        .context("malformed /whoami answer")?;
    if who["fp"].as_str() != Some(s.fp.as_str()) {
        bail!("/whoami disagrees with the session ({who})");
    }
    let allowed: Vec<&str> = who["routes"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    println!(
        "session: ok · label {} · key {} · expires {} · granted {}",
        s.label,
        s.fp,
        chrono::DateTime::from_timestamp_millis(s.exp_ms as i64)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default(),
        allowed.join(", ")
    );
    Ok(())
}

async fn check(cfg: &KeysConfig, route: &str, path: &str) -> Result<()> {
    if !tengu_seal::route::is_route_name(route) {
        bail!("'{route}' is not a route name");
    }
    let origin = k::proxy(cfg)?;
    let client = k::http_client()?;
    let s = k::mint(cfg, &client, &[route.to_string()]).await?;
    let path = path.trim_start_matches('/');
    let url = if path.is_empty() {
        format!("{origin}/{route}")
    } else {
        format!("{origin}/{route}/{path}")
    };
    let started = std::time::Instant::now();
    let resp = client
        .get(&url)
        .bearer_auth(&s.token)
        .send()
        .await
        .map_err(|e| anyhow!("GET through the proxy failed: {}", e.without_url()))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let body = resp.bytes().await.unwrap_or_default();
    println!(
        "{route}/{path}: {status} · {ctype} · {} bytes · {} ms",
        body.len(),
        started.elapsed().as_millis()
    );
    let text = String::from_utf8_lossy(&body);
    if !status.is_success() && text.contains("\"error\"") && text.contains("\"code\"") {
        println!("proxy says: {}", k::worker_error(&text));
    }
    if text.contains(tengu_seal::target::MASK) {
        println!("note: the reply contained the route's key; the Worker masked it");
    }
    Ok(())
}
