//! `tengu-seal` Worker: adds a route's key to tengu's request, forwards it
//! and masks the key out of the reply. Provider keys are Worker secrets the
//! operator sets in Cloudflare; they never reach the harness.
//!
//! | Endpoint | Auth | Does |
//! |---|---|---|
//! | `GET /healthz` | none | liveness + route names |
//! | `POST /session` | SSH signature of a key listed in `CLIENTS` | session token (HMAC under `SESSION_KEY`) |
//! | `GET /whoami` | session | fingerprint, label, expiry, allowed routes |
//! | `ANY /<route>/<rest>` | session | route's key in → fetch (`redirect: manual`) → key masked out → back |
//! | `ANY <path>` + `Tengu-Route: <route>` | session | same, for clients that build absolute paths (teloxide) |
//!
//! Config (`wrangler.toml`, no secrets): `ROUTES`, `CLIENTS`. Secrets (set
//! in Cloudflare by the operator): `SESSION_KEY` + one per route. Pure logic
//! lives in `crates/tengu-seal`; this file is the runtime glue. Logs carry
//! route, client label, upstream host, status, ms — never headers, bodies
//! or secrets.

use futures_util::StreamExt;
use tengu_seal::route::{Clients, Routes};
use tengu_seal::session::{self, Claims};
use tengu_seal::ssh::SessionRequest;
use tengu_seal::{target, Code};
use worker::*;

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

/// Who called and where it went, for the one log line per request.
#[derive(Default)]
struct Trace {
    label: String,
    route: String,
    upstream: String,
    masked: i64,
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let started = now_ms();
    let mut trace = Trace::default();
    let outcome = match (req.method(), req.path().as_str()) {
        (Method::Get, "/healthz") => healthz(&env),
        (Method::Post, "/session") => new_session(req, &env, &mut trace).await,
        (Method::Get, "/whoami") => whoami(&req, &env, &mut trace),
        _ => forward(req, &env, &mut trace).await,
    };
    let resp = match outcome {
        Ok(r) => r,
        Err(f) => error_response(&f)?,
    };
    let line = serde_json::json!({
        "client": trace.label,
        "route": trace.route,
        "upstream": trace.upstream,
        "status": resp.status_code(),
        "masked": trace.masked,
        "ms": now_ms().saturating_sub(started),
    });
    console_log!("{line}");
    Ok(resp)
}

fn routes(env: &Env) -> Res<Routes> {
    let raw = env
        .var("ROUTES")
        .map_err(|_| fail(Code::Misconfigured, "ROUTES var is not set"))?
        .to_string();
    Ok(Routes::parse(&raw)?)
}

fn clients(env: &Env, routes: &Routes) -> Res<Clients> {
    let raw = env
        .var("CLIENTS")
        .map_err(|_| fail(Code::Misconfigured, "CLIENTS var is not set"))?
        .to_string();
    Ok(Clients::parse(&raw, routes)?)
}

fn secret(env: &Env, name: &str) -> Res<String> {
    env.secret(name).map(|s| s.to_string()).map_err(|_| {
        fail(
            Code::Misconfigured,
            &format!("Worker secret {name} is not set — add it in Cloudflare"),
        )
    })
}

fn session_key(env: &Env) -> Res<Vec<u8>> {
    let key = secret(env, tengu_seal::route::SESSION_KEY)?.into_bytes();
    session::check_key(&key)?;
    Ok(key)
}

fn healthz(env: &Env) -> Res<Response> {
    let names: Vec<String> = routes(env)?.names().map(str::to_string).collect();
    Ok(Response::from_json(
        &serde_json::json!({ "ok": true, "routes": names }),
    )?)
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
    let table = routes(env)?;
    let all = clients(env, &table)?;
    let client = all.get(&fp).ok_or_else(|| {
        fail(
            Code::Unauthorized,
            "client key is not in CLIENTS (or was removed)",
        )
    })?;
    trace.label = client.label.clone();
    if body.routes.is_empty() {
        return Err(fail(Code::BadRequest, "ask for at least one route"));
    }
    let mut granted: Vec<String> = Vec::new();
    for r in &body.routes {
        if table.get(r).is_none() {
            return Err(fail(Code::BadRequest, &format!("unknown route '{r}'")));
        }
        if client.allows(r) && !granted.contains(r) {
            granted.push(r.clone());
        }
    }
    if granted.is_empty() {
        return Err(fail(
            Code::Forbidden,
            "this client key may use none of the requested routes",
        ));
    }
    let claims = Claims {
        fp: fp.clone(),
        label: client.label.clone(),
        exp_ms: now + client.session_ttl_ms(),
        routes: granted,
    };
    let token = session::issue(&session_key(env)?, &claims);
    Ok(Response::from_json(&serde_json::json!({
        "session": token,
        "exp_ms": claims.exp_ms,
        "fp": fp,
        "label": claims.label,
        "routes": claims.routes,
    }))?)
}

/// Session from `Authorization: Bearer`, its key still in `CLIENTS`, and —
/// for a proxied call — `route` both granted to the session and still
/// allowed for the key.
fn authorize(
    req: &Request,
    env: &Env,
    table: &Routes,
    route: Option<&str>,
    trace: &mut Trace,
) -> Res<Claims> {
    let header = req.headers().get("authorization")?.unwrap_or_default();
    let token = header.strip_prefix("Bearer ").ok_or_else(|| {
        fail(
            Code::Unauthorized,
            "missing session (Authorization: Bearer tss1.…)",
        )
    })?;
    let claims = session::verify(&session_key(env)?, token, now_ms())?;
    trace.label = claims.label.clone();
    let all = clients(env, table)?;
    let client = all.get(&claims.fp).ok_or_else(|| {
        fail(
            Code::Unauthorized,
            "client key is not in CLIENTS (or was removed)",
        )
    })?;
    if let Some(r) = route {
        if !claims.routes.iter().any(|g| g == r) || !client.allows(r) {
            return Err(fail(
                Code::Forbidden,
                "this session or client key may not use this route",
            ));
        }
    }
    Ok(claims)
}

fn whoami(req: &Request, env: &Env, trace: &mut Trace) -> Res<Response> {
    let table = routes(env)?;
    let c = authorize(req, env, &table, None, trace)?;
    Ok(Response::from_json(&serde_json::json!({
        "fp": c.fp, "label": c.label, "exp_ms": c.exp_ms, "routes": c.routes,
    }))?)
}

async fn forward(req: Request, env: &Env, trace: &mut Trace) -> Res<Response> {
    let header_route = req.headers().get("tengu-route")?;
    let url = req.url()?;
    let (route_name, rest) = target::split_path(url.path(), header_route.as_deref())
        .ok_or_else(|| fail(Code::BadRequest, "unknown route"))?;
    let (route_name, rest) = (route_name.to_string(), rest.to_string());
    trace.route = route_name.clone();
    let table = routes(env)?;
    authorize(&req, env, &table, Some(&route_name), trace)?;
    let route = table
        .get(&route_name)
        .ok_or_else(|| fail(Code::BadRequest, "unknown route"))?;
    let key = secret(env, &route.secret)?;
    let t = target::build(route, &key, &rest, url.query())?;
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
    // From here on the URL or a header holds the key: errors are mapped
    // without their text, so nothing secret reaches the log.
    if let Some((k, val)) = &t.header {
        headers.set(k, val).map_err(|_| {
            fail(
                Code::Misconfigured,
                "the route's key is not a valid header value",
            )
        })?;
    }
    let method = req.method();
    let mut init = RequestInit::new();
    // Never follow a redirect with the key attached: the 3xx goes back to
    // tengu, which decides (and re-checks egress) itself.
    init.with_method(method.clone())
        .with_headers(headers)
        .with_redirect(RequestRedirect::Manual);
    if !matches!(method, Method::Get | Method::Head) {
        init.with_body(req.inner().body().map(Into::into));
    }
    let upstream = Request::new_with_init(&t.url, &init)
        .map_err(|_| fail(Code::BadRequest, "upstream request could not be built"))?;
    let resp = Fetch::Request(upstream)
        .send()
        .await
        .map_err(|_| fail(Code::BadGateway, "upstream unreachable"))?;
    mask_reply(resp, &key, &method, trace).await
}

/// The reply with every copy of `key` replaced in all header values and in
/// a text / JSON / XML body of up to `MASK_MAX_BYTES` (read in chunks). A
/// larger body streams on from there unmasked (`masked` = -1 in the log); a
/// binary body or an event stream passes through untouched; a body-less
/// reply (HEAD, 101/204/205/304) stays body-less.
async fn mask_reply(
    mut resp: Response,
    key: &str,
    method: &Method,
    trace: &mut Trace,
) -> Res<Response> {
    let status = resp.status_code();
    let ct = resp.headers().get("content-type")?;
    let headers = Headers::new();
    let mut hits: i64 = 0;
    for (k, v) in resp.headers().entries() {
        let lower = k.to_ascii_lowercase();
        // The body is re-emitted decoded and re-measured by the runtime.
        if lower == "content-length" || lower == "content-encoding" {
            continue;
        }
        let (v, n) = target::mask(&v, key);
        hits += n as i64;
        headers.set(&k, &v)?;
    }
    let body_less = target::null_body(status)
        || matches!(method, Method::Head)
        || matches!(resp.body(), ResponseBody::Empty);
    let out = if body_less {
        Response::empty()?
    } else if !target::maskable(ct.as_deref()) {
        Response::from_stream(resp.stream()?)?
    } else {
        let mut stream = resp.stream()?;
        let mut buf: Vec<u8> = Vec::new();
        let mut over = false;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| fail(Code::BadGateway, "upstream reply could not be read"))?;
            buf.extend_from_slice(&chunk);
            if buf.len() > target::MASK_MAX_BYTES {
                over = true;
                break;
            }
        }
        let (masked, n) = target::mask_bytes(&buf, key);
        if over {
            hits = -1;
            let head = futures_util::stream::once(async move { Ok::<Vec<u8>, Error>(masked) });
            Response::from_stream(head.chain(stream))?
        } else {
            hits += n as i64;
            Response::from_bytes(masked)?
        }
    };
    trace.masked = hits;
    Ok(out.with_headers(headers).with_status(status))
}
