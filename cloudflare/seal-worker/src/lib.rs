//! `tengu-seal` Worker: opens tengu's sealed blobs, injects the secret and
//! forwards the request. Provider keys never reach the harness.
//!
//! | Route | Auth | Does |
//! |---|---|---|
//! | `GET /healthz` | none | liveness |
//! | `GET /pubkey` | none | X25519 public key + `kid` (made on first call by the `KeyVault` Durable Object) |
//! | `POST /session` | SSH signature of an allow-listed key (`CLIENTS` KV `clients:<fp>`) | 24 h session token |
//! | `GET /whoami` | session | the caller's fingerprint, label, expiry |
//! | `ANY /fwd/<rest>` | session + `Tengu-Sealed: <blob>` | open blob → check client + path → inject → fetch → stream back |
//!
//! Pure logic (blob, session, SSH, target) lives in `crates/tengu-seal` and is
//! tested natively; this file is the thin runtime glue. Logs carry route,
//! client label, upstream host, status and latency — never headers, bodies
//! or secret values.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde::Deserialize;
use tengu_seal::blob::{self, Blob};
use tengu_seal::session::{self, Claims};
use tengu_seal::ssh::SessionRequest;
use tengu_seal::vault::{Vault, SUITE};
use tengu_seal::{target, Code};
use worker::*;

mod vault_do;
pub use vault_do::KeyVault;

/// Allow-list entries are re-read at most this often per isolate, so a
/// revoked client is cut within a minute.
const CLIENT_CACHE_MS: u64 = 60_000;
/// A missing entry is re-read sooner, so a fresh registration works at once.
const CLIENT_MISS_CACHE_MS: u64 = 5_000;

thread_local! {
    static VAULT: RefCell<Option<Rc<Vault>>> = const { RefCell::new(None) };
    static CLIENTS: RefCell<HashMap<String, (Option<ClientEntry>, u64)>> = RefCell::new(HashMap::new());
}

/// KV value under `clients:<fp>`: `{"label": "mac-attended", "session_hours": 24}`.
#[derive(Clone, Deserialize)]
struct ClientEntry {
    label: String,
    /// Session lifetime for this key (default 24, capped at 168 = 7 days).
    #[serde(default)]
    session_hours: Option<u64>,
}

impl ClientEntry {
    fn session_ttl_ms(&self) -> u64 {
        self.session_hours
            .map(|h| h.clamp(1, 168) * 3_600_000)
            .unwrap_or(session::TTL_MS)
    }
}

struct Fail {
    code: Code,
    message: String,
}

impl From<tengu_seal::Error> for Fail {
    fn from(e: tengu_seal::Error) -> Self {
        Fail {
            code: e.code,
            message: e.message,
        }
    }
}

impl From<worker::Error> for Fail {
    fn from(e: worker::Error) -> Self {
        console_error!("worker error: {e}");
        Fail {
            code: Code::BadRequest,
            message: "request could not be processed".into(),
        }
    }
}

type Res<T> = std::result::Result<T, Fail>;

fn fail(code: Code, message: &str) -> Fail {
    Fail {
        code,
        message: message.into(),
    }
}

fn error_response(f: &Fail) -> Result<Response> {
    let body = serde_json::json!({ "error": { "code": f.code.as_str(), "message": f.message } });
    Ok(Response::from_json(&body)?.with_status(f.code.http_status()))
}

fn now_ms() -> u64 {
    Date::now().as_millis()
}

fn random<const N: usize>() -> Res<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf)
        .map_err(|_| fail(Code::BadRequest, "no randomness available"))?;
    Ok(buf)
}

/// Who called and where it went, for the one log line per request.
#[derive(Default)]
struct Trace {
    label: String,
    upstream: String,
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let started = now_ms();
    let header_blob = req.headers().has("tengu-sealed").unwrap_or(false);
    let route = route_name(&req.method(), &req.path(), header_blob);
    let mut trace = Trace::default();
    let outcome = match route {
        "healthz" => Response::from_json(&serde_json::json!({ "ok": true })).map_err(Fail::from),
        "pubkey" => pubkey(&env).await,
        "session" => new_session(req, &env, &mut trace).await,
        "whoami" => whoami(&req, &env, &mut trace).await,
        "fwd" => forward(req, &env, &mut trace).await,
        _ => Err(fail(Code::BadRequest, "unknown route")),
    };
    let resp = match outcome {
        Ok(r) => r,
        Err(f) => error_response(&f)?,
    };
    let line = serde_json::json!({
        "route": route,
        "client": trace.label,
        "upstream": trace.upstream,
        "status": resp.status_code(),
        "ms": now_ms().saturating_sub(started),
    });
    console_log!("{line}");
    Ok(resp)
}

fn route_name(method: &Method, path: &str, header_blob: bool) -> &'static str {
    match (method, path) {
        (Method::Get, "/healthz") => "healthz",
        (Method::Get, "/pubkey") => "pubkey",
        (Method::Post, "/session") => "session",
        (Method::Get, "/whoami") => "whoami",
        (_, p) if target::split_forward_path(p, header_blob).is_some() => "fwd",
        _ => "unknown",
    }
}

/// The vault for this isolate, fetched once from the `KeyVault` object.
async fn vault(env: &Env) -> Res<Rc<Vault>> {
    if let Some(v) = VAULT.with(|c| c.borrow().clone()) {
        return Ok(v);
    }
    let ns = env.durable_object("VAULT")?;
    let stub = ns.id_from_name("vault")?.get_stub()?;
    let mut resp = stub.fetch_with_str("https://vault/seed").await?;
    let seed_b64 = resp.text().await?;
    let seed = vault_do::decode_seed(&seed_b64)
        .ok_or_else(|| fail(Code::BadRequest, "vault seed is malformed"))?;
    let v = Rc::new(Vault::from_seed(&seed));
    VAULT.with(|c| *c.borrow_mut() = Some(v.clone()));
    Ok(v)
}

async fn pubkey(env: &Env) -> Res<Response> {
    let v = vault(env).await?;
    let pk = v.public_key();
    Ok(Response::from_json(&serde_json::json!({
        "suite": SUITE,
        "kid": pk.kid(),
        "pubkey": pk.to_b64(),
    }))?)
}

/// Allow-list lookup with a short per-isolate cache.
async fn client(env: &Env, fp: &str) -> Res<ClientEntry> {
    let now = now_ms();
    let cached = CLIENTS.with(|c| c.borrow().get(fp).cloned());
    let entry = match cached {
        Some((entry, until)) if now < until => entry,
        _ => {
            let kv = env.kv("CLIENTS")?;
            let raw = kv.get(&format!("clients:{fp}")).text().await.map_err(|e| {
                console_error!("kv error: {e}");
                fail(Code::BadRequest, "allow-list unavailable")
            })?;
            let entry = raw.and_then(|s| serde_json::from_str::<ClientEntry>(&s).ok());
            let ttl = if entry.is_some() {
                CLIENT_CACHE_MS
            } else {
                CLIENT_MISS_CACHE_MS
            };
            CLIENTS.with(|c| {
                c.borrow_mut()
                    .insert(fp.to_string(), (entry.clone(), now + ttl))
            });
            entry
        }
    };
    entry.ok_or_else(|| {
        fail(
            Code::Unauthorized,
            "client key is not allow-listed (or was revoked)",
        )
    })
}

async fn new_session(mut req: Request, env: &Env, trace: &mut Trace) -> Res<Response> {
    let body: SessionRequest = req
        .json()
        .await
        .map_err(|_| fail(Code::BadRequest, "body must be a session request"))?;
    let aud = req.url()?.origin().ascii_serialization();
    let now = now_ms();
    let key = body.verify(&aud, now)?;
    let fp = key.fingerprint();
    let entry = client(env, &fp).await?;
    trace.label = entry.label.clone();
    let v = vault(env).await?;
    let claims = Claims {
        fp: fp.clone(),
        label: entry.label.clone(),
        exp_ms: now + entry.session_ttl_ms(),
        kid: v.public_key().kid(),
    };
    let token = session::issue(&v, &claims, random::<12>()?);
    Ok(Response::from_json(&serde_json::json!({
        "session": token,
        "exp_ms": claims.exp_ms,
        "fp": fp,
        "label": entry.label,
        "kid": claims.kid,
    }))?)
}

/// Session from `Authorization: Bearer`, still allow-listed.
async fn authorize(req: &Request, env: &Env, trace: &mut Trace) -> Res<Claims> {
    let header = req.headers().get("authorization")?.unwrap_or_default();
    let token = header.strip_prefix("Bearer ").ok_or_else(|| {
        fail(
            Code::Unauthorized,
            "missing session (Authorization: Bearer tss1.…)",
        )
    })?;
    let v = vault(env).await?;
    let claims = session::verify(&v, token, now_ms())?;
    trace.label = claims.label.clone();
    client(env, &claims.fp).await?;
    Ok(claims)
}

async fn whoami(req: &Request, env: &Env, trace: &mut Trace) -> Res<Response> {
    let c = authorize(req, env, trace).await?;
    Ok(Response::from_json(&serde_json::json!({
        "fp": c.fp, "label": c.label, "exp_ms": c.exp_ms, "kid": c.kid,
    }))?)
}

async fn forward(req: Request, env: &Env, trace: &mut Trace) -> Res<Response> {
    let claims = authorize(&req, env, trace).await?;
    let url = req.url()?;
    let header_blob = req.headers().has("tengu-sealed")?;
    let (path_blob, rest) = target::split_forward_path(url.path(), header_blob)
        .ok_or_else(|| fail(Code::BadRequest, "unknown forward path"))?;
    let rest = rest.to_string();
    let sealed = match path_blob {
        Some(b) => b.to_string(),
        None => req
            .headers()
            .get("tengu-sealed")?
            .ok_or_else(|| fail(Code::BadRequest, "missing Tengu-Sealed header"))?,
    };
    let parsed = Blob::parse(&sealed)?;
    let v = vault(env).await?;
    let secret = blob::open(&v, &parsed)?;
    if !parsed.meta.allows_client(&claims.fp) {
        return Err(fail(
            Code::Forbidden,
            "this client key may not use this blob",
        ));
    }
    let secret = std::str::from_utf8(&secret)
        .map_err(|_| fail(Code::BadRequest, "sealed secret is not UTF-8"))?;
    let t = target::build(&parsed.meta, secret, &rest, url.query())?;
    trace.upstream = Url::parse(&t.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();

    let headers = Headers::new();
    for (k, val) in req.headers().entries() {
        if !target::drop_request_header(&k) {
            headers.set(&k, &val)?;
        }
    }
    // From here on the URL or a header holds the secret: errors are mapped
    // without their text, so nothing secret reaches the log.
    if let Some((k, val)) = &t.header {
        headers.set(k, val).map_err(|_| {
            fail(
                Code::BadRequest,
                "sealed secret is not a valid header value",
            )
        })?;
    }
    let method = req.method();
    let mut init = RequestInit::new();
    // Never follow a redirect with the secret attached: the 3xx goes back
    // to tengu, which decides (and re-checks egress) itself.
    init.with_method(method.clone())
        .with_headers(headers)
        .with_redirect(RequestRedirect::Manual);
    if !matches!(method, Method::Get | Method::Head) {
        init.with_body(req.inner().body().map(Into::into));
    }
    let upstream = Request::new_with_init(&t.url, &init)
        .map_err(|_| fail(Code::BadRequest, "upstream request could not be built"))?;
    Fetch::Request(upstream)
        .send()
        .await
        .map_err(|_| fail(Code::BadGateway, "upstream unreachable"))
}
