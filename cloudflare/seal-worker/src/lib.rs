//! `tengu-seal` Worker: adds a route's key to tengu's request, forwards it
//! and masks the key out of the reply. Provider keys are Worker secrets the
//! operator sets in Cloudflare; they never reach the harness.
//!
//! | Endpoint | Auth | Does |
//! |---|---|---|
//! | `GET /healthz` | none | liveness + route names |
//! | `POST /session` | SSH signature of a key listed in `CLIENTS` | session token (HMAC under `SESSION_KEY`) |
//! | `GET /whoami` | session | fingerprint, label, expiry, allowed routes |
//! | `ANY /<route>/<rest>` | session | route's key in → fetch (`redirect: manual`) → key masked out of every header and body byte (streamed, any type / size) → back |
//! | `ANY <path>` + `Tengu-Route: <route>` | session | same, for clients that build absolute paths (teloxide) |
//!
//! Config (`wrangler.toml`, no secrets): `ROUTES`, `CLIENTS`. Secrets (set
//! in Cloudflare by the operator): `SESSION_KEY` + one per route. Pure logic
//! (incl. the masker, `tengu_seal::mask`) lives in `crates/tengu-seal` and is
//! tested there; this file is the runtime glue, shipped for wasm32 (its
//! reply-body glue `masked_stream` is tested on the host: `cargo test`).
//! Logs carry route, client label, upstream host, status, masked count, ms —
//! never headers, bodies or secrets.

use futures_util::StreamExt;
use tengu_seal::mask::Masker;
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
#[derive(Default, Clone)]
struct Trace {
    started: u64,
    label: String,
    route: String,
    upstream: String,
    /// Copies of the route's key masked, headers + body.
    masked: usize,
    /// The reply body streams: [`MaskedBody`] writes the line when it ends.
    deferred: bool,
}

impl Trace {
    /// `body`: how a streamed reply body ended — `done`, or `cut` (upstream
    /// read error / caller gone; the held tail is never sent).
    fn log(&self, status: u16, body: Option<&str>) {
        let mut line = serde_json::json!({
            "client": self.label,
            "route": self.route,
            "upstream": self.upstream,
            "status": status,
            "masked": self.masked,
            "ms": now_ms().saturating_sub(self.started),
        });
        if let Some(b) = body {
            line["body"] = b.into();
        }
        console_log!("{line}");
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let mut trace = Trace {
        started: now_ms(),
        ..Trace::default()
    };
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
    if !trace.deferred {
        trace.log(resp.status_code(), None);
    }
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

/// A Worker secret, trimmed: a pasted or piped value often ends in a newline,
/// which no header, path or query may carry.
fn secret(env: &Env, name: &str) -> Res<String> {
    env.secret(name)
        .map(|s| s.to_string().trim().to_string())
        .map_err(|_| {
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
    // Fail closed before anything goes upstream: a key the reply masking
    // cannot handle (shorter than MASK_MIN_SECRET) is a 500.
    let masker = Masker::new(&key)?;
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
    mask_reply(resp, masker, &method, trace)
}

/// The reply with every copy of the route's key masked
/// ([`tengu_seal::mask`]): every header value (a header whose name holds it
/// is dropped), and the whole body as it streams — any content type, any
/// size, no pass-through. A body-less reply (HEAD, 101/204/205/304) stays
/// body-less. A body that cannot be opened is a 502; one cut mid-stream ends
/// short — never with unmasked bytes.
fn mask_reply(
    mut resp: Response,
    masker: Masker,
    method: &Method,
    trace: &mut Trace,
) -> Res<Response> {
    let status = resp.status_code();
    let (pairs, hits) = masker.reply_headers(resp.headers().entries());
    trace.masked = hits;
    let headers = Headers::new();
    for (k, v) in &pairs {
        headers.set(k, v)?;
    }
    let body_less = target::null_body(status)
        || matches!(method, Method::Head)
        || matches!(resp.body(), ResponseBody::Empty);
    if body_less {
        return Ok(Response::empty()?.with_headers(headers).with_status(status));
    }
    let upstream = resp
        .stream()
        .map_err(|_| fail(Code::BadGateway, "upstream reply could not be read"))?;
    let mut line = trace.clone();
    let body = masked_stream(upstream, masker, move |body_hits, end| {
        line.masked += body_hits;
        line.log(status, Some(end));
    });
    let out = Response::from_stream(body)?;
    trace.deferred = true;
    Ok(out.with_headers(headers).with_status(status))
}

/// A reply body on its way back: each upstream chunk through the
/// [`Masker`], which holds back only a tail that could still start a key
/// (so a key split across chunks is masked too). Dropping it — body done,
/// upstream read error, caller gone — calls `on_end(body hits, "done" |
/// "cut")` once: the request's log line.
struct MaskedBody<S, F: FnOnce(usize, &'static str)> {
    upstream: S,
    masker: Masker,
    on_end: Option<F>,
    end: &'static str,
}

impl<S, F: FnOnce(usize, &'static str)> Drop for MaskedBody<S, F> {
    fn drop(&mut self) {
        if let Some(f) = self.on_end.take() {
            f(self.masker.hits(), self.end);
        }
    }
}

/// `upstream` masked chunk by chunk; no byte reaches the caller unmasked.
fn masked_stream<S, F>(
    upstream: S,
    masker: Masker,
    on_end: F,
) -> impl futures_util::Stream<Item = Result<Vec<u8>>>
where
    S: futures_util::Stream<Item = Result<Vec<u8>>> + Unpin + 'static,
    F: FnOnce(usize, &'static str) + 'static,
{
    let body = MaskedBody {
        upstream,
        masker,
        on_end: Some(on_end),
        end: "cut",
    };
    futures_util::stream::unfold(Some(body), |state| async move {
        let mut b = state?;
        match b.upstream.next().await {
            Some(Ok(chunk)) => {
                let out = b.masker.push(&chunk);
                Some((Ok(out), Some(b)))
            }
            // The held tail is dropped with `b`: a cut body ends short,
            // never with unmasked bytes.
            Some(Err(_)) => Some((
                Err(Error::RustError("upstream reply could not be read".into())),
                None,
            )),
            None => {
                let out = b.masker.finish();
                b.end = "done";
                Some((Ok(out), None))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    //! Host-side: the body glue with a plain stream in place of the
    //! runtime's `ByteStream` (the masker itself is tested in
    //! `crates/tengu-seal`).
    use super::*;
    use futures_util::FutureExt;
    use std::cell::RefCell;
    use std::rc::Rc;

    const KEY: &str = "sk-or-v1-0123456789abcdef";

    type Chunks = Vec<Result<Vec<u8>>>;
    /// `on_end` calls: (body hits, `done` / `cut`).
    type Ends = Vec<(usize, &'static str)>;

    /// Run the masked stream over `chunks`; (items, `on_end` calls).
    fn run(chunks: Chunks) -> (Chunks, Ends) {
        let ended: Rc<RefCell<Ends>> = Rc::default();
        let seen = ended.clone();
        let body = masked_stream(
            futures_util::stream::iter(chunks),
            Masker::new(KEY).unwrap(),
            move |n, end| seen.borrow_mut().push((n, end)),
        );
        let items = body
            .collect::<Vec<_>>()
            .now_or_never()
            .expect("ready stream");
        let calls = ended.borrow().clone();
        (items, calls)
    }

    #[test]
    fn every_chunk_goes_through_the_masker() {
        let (a, b) = KEY.as_bytes().split_at(10);
        let (items, ended) = run(vec![
            Ok(b"event: x\ndata: \"".to_vec()),
            Ok(a.to_vec()),
            Ok([b, b"\"\n\n\x00\xff".as_slice()].concat()),
        ]);
        let body: Vec<u8> = items.into_iter().flat_map(|r| r.unwrap()).collect();
        assert_eq!(body, b"event: x\ndata: \"[REDACTED]\"\n\n\x00\xff".to_vec());
        assert_eq!(ended, vec![(1, "done")]);
    }

    #[test]
    fn a_cut_body_never_sends_its_held_tail() {
        let (a, _) = KEY.as_bytes().split_at(KEY.len() - 1);
        let (items, ended) = run(vec![
            Ok([b"data: ".as_slice(), a].concat()),
            Err(Error::RustError("reset".into())),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_ref().unwrap(), b"data: ");
        assert!(items[1].is_err());
        assert_eq!(ended, vec![(0, "cut")]);
    }
}
