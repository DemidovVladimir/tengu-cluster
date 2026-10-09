//! `tengu keys` — the operator side of the sealed-secret proxy
//! (`docs/sealed-keys-2026-10-09.md`).
//!
//! | Action | Does | Network | Touch ID |
//! |---|---|---|---|
//! | `setup --proxy <url>` | fetch the Worker public key, list agent keys, print the `[keys]` block + allow-list commands | `GET /pubkey` | no |
//! | `seal <name> …` | read one secret (hidden prompt / `--stdin`), HPKE-seal it to the pinned key, write `<sealed_dir>/<name>.sealed` | none | no |
//! | `status` | config, blobs, session (`/whoami`) | `/session`, `/whoami` | yes (new session) |
//! | `check <name>` | one GET through the proxy; prints status + size only | `/s/<blob>/…` | yes (new session) |
//!
//! Nothing here prints a secret or the session token.

use std::io::BufRead;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use tengu_seal::blob::{seal, Inject, SealedMeta};
use tengu_seal::vault::PublicKey;
use zeroize::Zeroizing;

use crate::adapters::outbound::keys as k;
use crate::config::keys::{is_blob_name, KeysConfig};
use crate::config::Config;

#[derive(Subcommand, Debug)]
pub(crate) enum KeysAction {
    /// Fetch the Worker's public key, list this machine's agent keys, and
    /// print the `[keys]` block plus the allow-list commands.
    Setup {
        /// Worker origin, e.g. https://tengu-seal.<subdomain>.workers.dev
        #[arg(long)]
        proxy: String,
        /// SSH-agent socket (default $SSH_AUTH_SOCK)
        #[arg(long)]
        agent_socket: Option<String>,
    },
    /// Seal one secret to the Worker's public key; writes <name>.sealed.
    Seal {
        /// Blob name: 1-64 chars of [A-Za-z0-9_-] (e.g. openrouter)
        name: String,
        /// https://host[/base] the secret may be sent to
        #[arg(long)]
        upstream: String,
        /// bearer | basic | url | header:<Name> | placeholder[:<TOKEN>]
        #[arg(long, default_value = "bearer")]
        inject: String,
        /// Allowed client key fingerprint (SHA256:…); repeat; none = any allow-listed key
        #[arg(long = "client")]
        clients: Vec<String>,
        /// Read the secret from stdin (one line) instead of a hidden prompt
        #[arg(long)]
        stdin: bool,
        /// Use [keys] from sandboxes/<name>/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Show the [keys] config, the sealed blobs and the session.
    Status {
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// One GET through the proxy with a sealed blob; prints status and size.
    Check {
        /// Sealed blob name
        name: String,
        /// Path under the blob's upstream, e.g. v1/models
        #[arg(long, default_value = "")]
        path: String,
        #[arg(long)]
        sandbox: Option<String>,
    },
}

/// `[keys]` of the sandbox file, else of the base config — without
/// starting a session (no Touch ID for `seal`).
fn keys_config(base: &Config, sandbox: Option<&str>) -> Result<KeysConfig> {
    match sandbox {
        None => Ok(base.keys.clone()),
        Some(name) => {
            let path = PathBuf::from("sandboxes").join(name).join("config.toml");
            Ok(Config::load(&path)
                .with_context(|| format!("load sandbox {}", path.display()))?
                .keys)
        }
    }
}

pub(crate) async fn run_keys(base: &Config, action: KeysAction) -> Result<()> {
    match action {
        KeysAction::Setup {
            proxy,
            agent_socket,
        } => setup(&proxy, agent_socket).await,
        KeysAction::Seal {
            name,
            upstream,
            inject,
            clients,
            stdin,
            sandbox,
        } => {
            let cfg = keys_config(base, sandbox.as_deref())?;
            seal_one(&cfg, &name, &upstream, &inject, clients, stdin)
        }
        KeysAction::Status { sandbox } => status(&keys_config(base, sandbox.as_deref())?).await,
        KeysAction::Check {
            name,
            path,
            sandbox,
        } => check(&keys_config(base, sandbox.as_deref())?, &name, &path).await,
    }
}

#[derive(Deserialize)]
struct PubkeyResponse {
    kid: String,
    pubkey: String,
    suite: String,
}

async fn fetch_pubkey(client: &reqwest::Client, origin: &str) -> Result<PubkeyResponse> {
    let resp = client
        .get(format!("{origin}/pubkey"))
        .send()
        .await
        .with_context(|| format!("GET {origin}/pubkey"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{origin}/pubkey answered {status}: {}",
            k::worker_error(&body)
        );
    }
    let p: PubkeyResponse = serde_json::from_str(&body).context("malformed /pubkey answer")?;
    let kid = PublicKey::from_b64(&p.pubkey)?.kid();
    if kid != p.kid {
        bail!("/pubkey kid does not match its key — refusing to pin it");
    }
    Ok(p)
}

async fn setup(proxy: &str, agent_socket: Option<String>) -> Result<()> {
    let origin = proxy.trim_end_matches('/').to_string();
    let cfg = KeysConfig {
        proxy: Some(origin.clone()),
        agent_socket: agent_socket.clone(),
        ..Default::default()
    };
    if let Some(e) = cfg.validation_errors(&[]).first() {
        bail!("{e}");
    }
    let p = fetch_pubkey(&k::http_client()?, &origin).await?;
    println!("Worker public key ({})", p.suite);
    println!("  kid: {}", p.kid);
    println!();
    let socket = k::agent_socket(&cfg)?;
    let ids = k::agent::identities(&socket)?;
    if ids.is_empty() {
        println!(
            "No usable key in the ssh-agent at {} (need ecdsa-sha2-nistp256 or ssh-ed25519).",
            socket.display()
        );
        println!("Secretive: create a key (Touch ID optional), then rerun.");
    }
    println!("Add to your config (base or sandbox):");
    println!();
    println!("[keys]");
    println!("proxy = \"{origin}\"");
    println!("pubkey = \"{}\"", p.pubkey);
    if let Some(first) = ids.first() {
        println!("client = \"{}\"", first.comment);
    }
    if let Some(s) = &agent_socket {
        println!("agent_socket = \"{s}\"");
    }
    println!("[keys.env]");
    println!("OPENROUTER_BASE_URL = \"openrouter\"");
    println!("OPENROUTER_API_KEY = \"@session\"");
    println!();
    println!("Allow-list each client key (run in cloudflare/seal-worker; session_hours 1-168):");
    for id in &ids {
        let label = if id.comment.is_empty() {
            "tengu".to_string()
        } else {
            id.comment.clone()
        };
        println!(
            "  npx wrangler kv key put --binding CLIENTS --remote \"clients:{}\" '{{\"label\":\"{}\",\"session_hours\":24}}'",
            id.key.fingerprint(),
            label.replace(['"', '\''], "")
        );
    }
    Ok(())
}

fn parse_inject(s: &str) -> Result<Inject> {
    Ok(match s {
        "bearer" => Inject::Bearer,
        "basic" => Inject::Basic,
        "url" => Inject::Url,
        "placeholder" => Inject::Placeholder {
            token: k::PLACEHOLDER.into(),
        },
        other => match other.split_once(':') {
            Some(("header", name)) => Inject::Header { name: name.into() },
            Some(("placeholder", token)) => Inject::Placeholder {
                token: token.into(),
            },
            _ => bail!(
                "--inject must be bearer | basic | url | header:<Name> | placeholder[:<TOKEN>], got '{other}'"
            ),
        },
    })
}

fn read_secret(name: &str, stdin: bool) -> Result<Zeroizing<String>> {
    let raw = if stdin {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read secret from stdin")?;
        line
    } else {
        rpassword::prompt_password(format!("Secret for '{name}' (input hidden): "))
            .context("read secret from the terminal")?
    };
    let secret = Zeroizing::new(raw.trim().to_string());
    drop(Zeroizing::new(raw));
    if secret.is_empty() {
        bail!("empty secret");
    }
    Ok(secret)
}

fn seal_one(
    cfg: &KeysConfig,
    name: &str,
    upstream: &str,
    inject: &str,
    clients: Vec<String>,
    stdin: bool,
) -> Result<()> {
    if !is_blob_name(name) {
        bail!("blob name must be 1-64 chars of [A-Za-z0-9_-]");
    }
    let pk_b64 = cfg.pubkey.as_deref().ok_or_else(|| {
        anyhow!("[keys] pubkey is not set — run `tengu keys setup --proxy <url>`")
    })?;
    let pk = PublicKey::from_b64(pk_b64)?;
    let inject = parse_inject(inject)?;
    for c in &clients {
        if !c.starts_with("SHA256:") {
            bail!("--client takes a full SHA256:… fingerprint (see `tengu keys setup`)");
        }
    }
    let meta = SealedMeta {
        v: 1,
        name: name.to_string(),
        upstream: upstream.trim_end_matches('/').to_string(),
        inject,
        clients,
        kid: String::new(),
        created_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
    };
    // Shape errors before the prompt, so a typo never costs a paste.
    let mut probe = meta.clone();
    probe.kid = pk.kid();
    probe.validate()?;
    let secret = read_secret(name, stdin)?;
    let blob = seal(&pk, meta, secret.as_bytes(), &mut tengu_seal::OsRng)?;
    drop(secret);
    let dir = k::sealed_dir(cfg);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = k::blob_path(cfg, name);
    let existed = path.exists();
    std::fs::write(&path, format!("{}\n", blob.encode()))
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "{} {} (upstream {}, inject {:?}, kid {})",
        if existed { "replaced" } else { "wrote" },
        path.display(),
        blob.meta.upstream,
        blob.meta.inject,
        blob.meta.kid
    );
    println!("The file holds ciphertext only; it is safe to commit.");
    Ok(())
}

async fn session(cfg: &KeysConfig, client: &reqwest::Client) -> Result<k::Session> {
    k::mint(cfg, client).await
}

async fn status(cfg: &KeysConfig) -> Result<()> {
    let origin = k::proxy(cfg)?;
    println!("proxy:      {origin}");
    println!("sealed_dir: {}", k::sealed_dir(cfg).display());
    let pinned_kid = match cfg.pubkey.as_deref().map(PublicKey::from_b64) {
        Some(Ok(pk)) => Some(pk.kid()),
        Some(Err(e)) => bail!("[keys] pubkey: {e}"),
        None => None,
    };
    let client = k::http_client()?;
    let live = fetch_pubkey(&client, &origin).await?;
    println!("worker kid: {}", live.kid);
    match &pinned_kid {
        Some(kid) if *kid == live.kid => println!("pinned kid: matches"),
        Some(kid) => println!("pinned kid: {kid} — DIFFERS from the Worker; reseal every blob"),
        None => println!("pinned kid: none ([keys] pubkey unset)"),
    }
    println!();
    println!("sealed blobs:");
    let mut names: Vec<String> = std::fs::read_dir(k::sealed_dir(cfg))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".sealed"))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        println!("  (none)");
    }
    for n in &names {
        match k::read_blob(cfg, n) {
            Ok(b) => println!(
                "  {n}: upstream {} · inject {:?} · clients {} · kid {}",
                b.meta.upstream,
                b.meta.inject,
                if b.meta.clients.is_empty() {
                    "any".to_string()
                } else {
                    b.meta.clients.join(", ")
                },
                if b.meta.kid == live.kid {
                    "ok"
                } else {
                    "STALE — reseal"
                }
            ),
            Err(e) => println!("  {n}: {e:#}"),
        }
    }
    println!();
    println!("[keys.env]:");
    for (var, value) in &cfg.env {
        println!("  {var} ← {value}");
    }
    println!();
    let s = session(cfg, &client).await?;
    let who: serde_json::Value = client
        .get(format!("{origin}/whoami"))
        .bearer_auth(&s.token)
        .send()
        .await?
        .json()
        .await
        .context("malformed /whoami answer")?;
    println!(
        "session:    ok · label {} · key {} · expires {}",
        s.label,
        s.fp,
        chrono::DateTime::from_timestamp_millis(s.exp_ms as i64)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    );
    if who["fp"].as_str() != Some(s.fp.as_str()) {
        bail!("/whoami disagrees with the session ({})", who);
    }
    Ok(())
}

async fn check(cfg: &KeysConfig, name: &str, path: &str) -> Result<()> {
    let origin = k::proxy(cfg)?;
    let blob = k::read_blob(cfg, name)?;
    let client = k::http_client()?;
    let s = session(cfg, &client).await?;
    let url = format!(
        "{origin}/s/{}/{}",
        blob.encode(),
        path.trim_start_matches('/')
    );
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
        "{name} → {}/{}: {status} · {ctype} · {} bytes · {} ms",
        blob.meta.upstream,
        path.trim_start_matches('/'),
        body.len(),
        started.elapsed().as_millis()
    );
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        if text.contains("\"error\"") && text.contains("\"code\"") {
            println!("proxy says: {}", k::worker_error(&text));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_modes_parse() {
        assert_eq!(parse_inject("bearer").unwrap(), Inject::Bearer);
        assert_eq!(parse_inject("basic").unwrap(), Inject::Basic);
        assert_eq!(parse_inject("url").unwrap(), Inject::Url);
        assert_eq!(
            parse_inject("header:X-Api-Key").unwrap(),
            Inject::Header {
                name: "X-Api-Key".into()
            }
        );
        assert_eq!(
            parse_inject("placeholder").unwrap(),
            Inject::Placeholder {
                token: "TENGU_SECRET".into()
            }
        );
        assert!(parse_inject("cookie").is_err());
    }

    #[test]
    fn seal_without_pinned_key_names_setup() {
        let err = seal_one(
            &KeysConfig {
                proxy: Some("https://p.example".into()),
                ..Default::default()
            },
            "openrouter",
            "https://openrouter.ai/api",
            "bearer",
            vec![],
            true,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("tengu keys setup"));
    }
}
