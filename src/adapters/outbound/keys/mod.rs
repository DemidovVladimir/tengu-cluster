//! Sealed keys — the tengu side of the `tengu-seal` Cloudflare Worker
//! (`cloudflare/seal-worker`, `docs/sealed-keys-2026-10-09.md`).
//!
//! No provider key is stored here. `tengu keys seal` HPKE-seals each one to
//! the Worker's public key (`<TENGU_HOME>/sealed/<name>.sealed`, safe in
//! git). At startup [`install`] gets a session: the SSH agent (Secretive →
//! Secure Enclave, Touch ID) signs a challenge, the Worker returns a token
//! kept in RAM only. It then exports `[keys.env]`, so the existing clients
//! talk to the Worker without code changes:
//!
//! | Export | Value | Who reads it |
//! |---|---|---|
//! | `[keys.env]` blob entry (`OPENROUTER_BASE_URL = "openrouter"`) | `<proxy>/s/<blob>` | OpenRouter engine, embeddings, Jev, wiki compiler, Solana / EVM RPC, Telegram (`TELEGRAM_API_URL`) |
//! | `[keys.env]` `@session` entry (`OPENROUTER_API_KEY`) | session token, sent as `Authorization: Bearer` | the same clients |
//! | `TENGU_KEYS_PROXY`, `TENGU_KEYS_SESSION_TOKEN`, `TENGU_KEYS_SESSION_EXP_MS` | proxy origin, token, expiry | children reuse the session; [`auth_header_for`] adds it to clients that send no `Authorization` |
//!
//! The token's env name ends in `_TOKEN`, so every redaction registry built
//! after [`install`] hides it (`domain::secrets::is_env_secret`).

pub(crate) mod agent;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tengu_seal::blob::Blob;
use tengu_seal::ssh::{session_message, SessionRequest};
use tracing::{info, warn};

use crate::config::keys::{KeysConfig, SESSION_VALUE};
use crate::config::paths::{expand_tilde, resolve_tengu_home};

pub(crate) const PROXY_ENV: &str = "TENGU_KEYS_PROXY";
pub(crate) const SESSION_ENV: &str = "TENGU_KEYS_SESSION_TOKEN";
pub(crate) const SESSION_EXP_ENV: &str = "TENGU_KEYS_SESSION_EXP_MS";
/// Telegram `bot<token>` placeholder the Worker swaps for the real token.
pub(crate) const PLACEHOLDER: &str = "TENGU_SECRET";
/// An inherited session closer than this to expiry is replaced.
const REUSE_MARGIN_MS: u64 = 5 * 60 * 1000;

/// A session the Worker issued.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Session {
    #[serde(rename = "session")]
    pub token: String,
    pub exp_ms: u64,
    pub fp: String,
    pub label: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The proxy origin without a trailing slash.
pub(crate) fn proxy(cfg: &KeysConfig) -> Result<String> {
    cfg.proxy
        .as_deref()
        .map(|p| p.trim_end_matches('/').to_string())
        .ok_or_else(|| anyhow!("[keys] proxy is not set"))
}

pub(crate) fn sealed_dir(cfg: &KeysConfig) -> PathBuf {
    match &cfg.sealed_dir {
        Some(d) => expand_tilde(Path::new(d)),
        None => resolve_tengu_home().join("sealed"),
    }
}

pub(crate) fn blob_path(cfg: &KeysConfig, name: &str) -> PathBuf {
    sealed_dir(cfg).join(format!("{name}.sealed"))
}

/// The sealed blob `name`, checked to parse.
pub(crate) fn read_blob(cfg: &KeysConfig, name: &str) -> Result<Blob> {
    let path = blob_path(cfg, name);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "sealed blob '{name}' not found at {} — run `tengu keys seal {name} …`",
            path.display()
        )
    })?;
    Blob::parse(&text).map_err(|e| anyhow!("sealed blob '{name}' ({}): {e}", path.display()))
}

pub(crate) fn agent_socket(cfg: &KeysConfig) -> Result<PathBuf> {
    match &cfg.agent_socket {
        Some(s) => Ok(expand_tilde(Path::new(s))),
        None => std::env::var_os("SSH_AUTH_SOCK")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("no ssh-agent: set SSH_AUTH_SOCK or [keys] agent_socket")),
    }
}

/// The agent key named by `[keys] client` (comment or full fingerprint),
/// else the agent's only key.
pub(crate) fn select_identity(cfg: &KeysConfig) -> Result<agent::Identity> {
    let socket = agent_socket(cfg)?;
    let ids = agent::identities(&socket)?;
    match cfg.client.as_deref() {
        Some(want) => ids
            .into_iter()
            .find(|i| i.comment == want || i.key.fingerprint() == want)
            .ok_or_else(|| anyhow!("ssh-agent has no key '{want}' ([keys] client)")),
        None => match ids.len() {
            1 => Ok(ids.into_iter().next().expect("one key")),
            0 => bail!("ssh-agent holds no ecdsa-sha2-nistp256 / ssh-ed25519 key"),
            n => bail!("ssh-agent holds {n} keys — name one with [keys] client"),
        },
    }
}

/// Ask the Worker for a session, signed by the agent key.
pub(crate) async fn mint(cfg: &KeysConfig, client: &reqwest::Client) -> Result<Session> {
    let origin = proxy(cfg)?;
    let id = select_identity(cfg)?;
    let fp = id.key.fingerprint();
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow!("OS random source failed: {e}"))?;
    let nonce = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce)
    };
    let ts_ms = now_ms();
    let msg = session_message(&fp, ts_ms, &nonce, &origin);
    let socket = agent_socket(cfg)?;
    let key = id.key.clone();
    let sig = tokio::task::spawn_blocking(move || agent::sign(&socket, &key, &msg))
        .await
        .context("ssh-agent signer task")??;
    let body = SessionRequest {
        pubkey: id.key.to_openssh(),
        ts_ms,
        nonce,
        sig: {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
        },
    };
    let resp = client
        .post(format!("{origin}/session"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {origin}/session"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "seal proxy refused the session ({status}): {} — key {fp}",
            worker_error(&text)
        );
    }
    serde_json::from_str(&text).context("seal proxy: malformed session response")
}

/// The `error.message` of a Worker error body, else the raw text (short).
pub(crate) fn worker_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// The session in the env (a parent's), if it is for `origin` and fresh.
fn inherited_session(origin: &str) -> Option<(String, u64)> {
    let token = std::env::var(SESSION_ENV).ok()?;
    let exp: u64 = std::env::var(SESSION_EXP_ENV).ok()?.parse().ok()?;
    let same = std::env::var(PROXY_ENV).ok()? == origin;
    (same && exp > now_ms() + REUSE_MARGIN_MS && !token.is_empty()).then_some((token, exp))
}

/// What `[keys.env]` exports, given the session token. Blob files must
/// exist; a missing one is an error naming the `tengu keys seal` command.
pub(crate) fn env_exports(cfg: &KeysConfig, token: &str) -> Result<Vec<(String, String)>> {
    let origin = proxy(cfg)?;
    let mut out = Vec::new();
    for (name, value) in &cfg.env {
        let v = if value == SESSION_VALUE {
            token.to_string()
        } else {
            let blob = read_blob(cfg, value)?;
            format!("{origin}/s/{}", blob.encode())
        };
        out.push((name.clone(), v));
    }
    Ok(out)
}

/// Get a session (reuse a parent's, else sign a new one) and export
/// `[keys.env]`. No-op without `[keys] proxy`. Fail-soft: on error the env is
/// left alone and a warning names the cause, so commands that need no keys
/// still run.
pub(crate) fn install(cfg: &KeysConfig) {
    if !cfg.enabled() {
        return;
    }
    if let Err(e) = try_install(cfg) {
        warn!(
            error = %format!("{e:#}"),
            "[keys] sealed-key session not set up — calls through the seal proxy will fail"
        );
    }
}

fn try_install(cfg: &KeysConfig) -> Result<()> {
    let origin = proxy(cfg)?;
    let (token, exp_ms, who) = match inherited_session(&origin) {
        Some((t, exp)) => (t, exp, None),
        None => {
            let s = mint_blocking(cfg)?;
            (s.token, s.exp_ms, Some((s.label, s.fp)))
        }
    };
    let exports = env_exports(cfg, &token)?;
    // Same startup point as the vault's `load_secrets_into_env` and the
    // `TENGU_CONFIG` pin: before agents, tools or children run.
    std::env::set_var(PROXY_ENV, &origin);
    std::env::set_var(SESSION_ENV, &token);
    std::env::set_var(SESSION_EXP_ENV, exp_ms.to_string());
    for (k, v) in &exports {
        std::env::set_var(k, v);
    }
    let names: Vec<&str> = exports.iter().map(|(k, _)| k.as_str()).collect();
    match who {
        Some((label, fp)) => info!(
            proxy = %origin, label = %label, fp = %fp, exp_ms, exported = ?names,
            "[keys] seal-proxy session started"
        ),
        None => {
            info!(proxy = %origin, exp_ms, exported = ?names, "[keys] reusing the parent's session")
        }
    }
    Ok(())
}

/// [`mint`] from sync code: its own thread and current-thread runtime, so it
/// works inside or outside the main runtime.
pub(crate) fn mint_blocking(cfg: &KeysConfig) -> Result<Session> {
    let cfg = cfg.clone();
    std::thread::spawn(move || -> Result<Session> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("keys: session runtime")?;
        rt.block_on(async {
            let client = http_client()?;
            mint(&cfg, &client).await
        })
    })
    .join()
    .map_err(|_| anyhow!("keys: session thread panicked"))?
}

/// Client for the seal proxy: LLM-API routing (`[egress] route_llm_api`),
/// since most of its traffic is provider calls.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    crate::adapters::outbound::egress::policy().llm_api_client(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60)),
    )
}

/// `Authorization: Bearer <session>` for a request to the seal proxy, for
/// clients that put their key in the URL (RPC, Telegram) and so send no
/// header. `None` for any other URL — the token goes to the proxy only.
pub(crate) fn auth_header_for(url: &str) -> Option<String> {
    bearer_for(
        std::env::var(PROXY_ENV).ok().as_deref(),
        std::env::var(SESSION_ENV).ok().as_deref(),
        url,
    )
}

/// For a client library that builds absolute request paths (teloxide's
/// `/bot<token>/<method>`), turn an exported `<proxy>/s/<blob>` URL into
/// header mode: (`<proxy>/` as the base URL, `Authorization` + `Tengu-Sealed`
/// headers). `None` when `url` is not a seal-proxy blob URL.
pub(crate) fn header_mode(url: &str) -> Option<(String, Vec<(&'static str, String)>)> {
    header_mode_for(
        std::env::var(PROXY_ENV).ok().as_deref(),
        std::env::var(SESSION_ENV).ok().as_deref(),
        url,
    )
}

fn header_mode_for(
    origin: Option<&str>,
    token: Option<&str>,
    url: &str,
) -> Option<(String, Vec<(&'static str, String)>)> {
    let (origin, token) = (origin?, token?);
    let blob = url.strip_prefix(origin)?.strip_prefix("/s/")?;
    let blob = blob.trim_end_matches('/');
    if blob.is_empty() || blob.contains('/') {
        return None;
    }
    Some((
        format!("{origin}/"),
        vec![
            ("authorization", format!("Bearer {token}")),
            ("tengu-sealed", blob.to_string()),
        ],
    ))
}

fn bearer_for(origin: Option<&str>, token: Option<&str>, url: &str) -> Option<String> {
    let (origin, token) = (origin?, token?);
    let rest = url.strip_prefix(origin)?;
    (rest.is_empty() || rest.starts_with('/')).then(|| format!("Bearer {token}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tengu_seal::blob::{seal, Inject, SealedMeta};
    use tengu_seal::vault::Vault;

    fn cfg(dir: &Path) -> KeysConfig {
        KeysConfig {
            proxy: Some("https://seal.example.workers.dev/".into()),
            sealed_dir: Some(dir.display().to_string()),
            env: BTreeMap::from([
                ("OPENROUTER_BASE_URL".into(), "openrouter".into()),
                ("OPENROUTER_API_KEY".into(), "@session".into()),
            ]),
            ..Default::default()
        }
    }

    fn write_blob(dir: &Path, name: &str) -> String {
        let v = Vault::from_seed(&[5u8; 32]);
        let meta = SealedMeta {
            v: 1,
            name: name.into(),
            upstream: "https://openrouter.ai/api".into(),
            inject: Inject::Bearer,
            clients: vec![],
            kid: String::new(),
            created_ms: 0,
        };
        let wire = seal(&v.public_key(), meta, b"sk-x", &mut tengu_seal::OsRng)
            .unwrap()
            .encode();
        std::fs::write(dir.join(format!("{name}.sealed")), format!("{wire}\n")).unwrap();
        wire
    }

    #[test]
    fn exports_blob_urls_and_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let wire = write_blob(dir.path(), "openrouter");
        let out = env_exports(&cfg(dir.path()), "tss1.tok").unwrap();
        assert_eq!(
            out,
            vec![
                ("OPENROUTER_API_KEY".to_string(), "tss1.tok".to_string()),
                (
                    "OPENROUTER_BASE_URL".to_string(),
                    format!("https://seal.example.workers.dev/s/{wire}")
                ),
            ]
        );
    }

    #[test]
    fn missing_blob_names_the_seal_command() {
        let dir = tempfile::tempdir().unwrap();
        let err = env_exports(&cfg(dir.path()), "t").unwrap_err();
        assert!(format!("{err:#}").contains("tengu keys seal openrouter"));
    }

    #[test]
    fn header_mode_splits_a_blob_url() {
        let o = Some("https://seal.example.workers.dev");
        let t = Some("tss1.x");
        let (base, headers) =
            header_mode_for(o, t, "https://seal.example.workers.dev/s/tsb1.a.b.c").unwrap();
        assert_eq!(base, "https://seal.example.workers.dev/");
        assert_eq!(
            headers,
            vec![
                ("authorization", "Bearer tss1.x".to_string()),
                ("tengu-sealed", "tsb1.a.b.c".to_string()),
            ]
        );
        assert!(header_mode_for(o, t, "https://api.telegram.org/").is_none());
        assert!(header_mode_for(o, t, "https://seal.example.workers.dev/s/a/b").is_none());
        assert!(header_mode_for(o, None, "https://seal.example.workers.dev/s/a").is_none());
    }

    #[test]
    fn bearer_only_for_the_proxy_origin() {
        let o = Some("https://seal.example.workers.dev");
        let t = Some("tss1.x");
        assert_eq!(
            bearer_for(o, t, "https://seal.example.workers.dev/s/b").as_deref(),
            Some("Bearer tss1.x")
        );
        assert_eq!(
            bearer_for(o, t, "https://seal.example.workers.dev.evil.com/s"),
            None
        );
        assert_eq!(
            bearer_for(o, t, "https://api.telegram.org/bot1/getMe"),
            None
        );
        assert_eq!(
            bearer_for(None, t, "https://seal.example.workers.dev/s"),
            None
        );
    }

    #[test]
    fn worker_error_reads_the_message() {
        assert_eq!(
            worker_error(r#"{"error":{"code":"unauthorized","message":"nope"}}"#),
            "nope"
        );
        assert_eq!(worker_error("plain"), "plain");
    }
}
