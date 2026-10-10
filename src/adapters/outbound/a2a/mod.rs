//! A2A client (`domain/a2a/`): read a remote agent's card, send it a
//! message, poll / cancel its task. One `[a2a.remotes.<name>]` entry per
//! client (`config/a2a.rs`); used by the `a2a` tool (`tools/a2a/`) and
//! `tengu a2a card|send|get|cancel` (`cli/a2a.rs`).
//!
//! | Concern | Rule |
//! |---|---|
//! | Card | `GET <url>/.well-known/agent-card.json`, on 404 the 0.3 path `agent.json` (or `url` itself when it ends in `.json`) |
//! | Interface | `AgentCard::endpoint` — JSON-RPC 1.x, JSON-RPC 0.3 (`domain/a2a/v03.rs`) or HTTP+JSON 1.x, the card's order; `endpoint_url` replaces its URL |
//! | Hosts | every request goes to the host of `url` or `endpoint_url` only — a card that names another host is refused before anything is sent (no card-driven requests elsewhere, the credential never leaves) |
//! | Gate | per request: `[egress]` (`check_url`: scheme, host ceiling; through the proxy — Tor by default — but a loopback remote, `EgressPolicy::peer_client`) + the caller's scope `net_hosts` (the tool's; none for the operator's CLI) |
//! | Credential | `bearer_env` → `Authorization: Bearer …`, or `header` + `header_env`; the variable read through the scope's `env_reads`; never logged |
//! | Headers | `A2A-Version: 1.0` (`0.3` to a 0.3 interface), `Accept: application/json` |
//! | Ids | JSON-RPC `id` 1, 2, … per client; the caller picks `messageId` (the `a2a` tool: its call id — a retried call is the same message) |
//! | Redirects | refused (the peer client follows none) |
//! | Reply | ≤ [`MAX_BODY`] bytes; JSON-RPC `error` → the error's A2A name, code and message; HTTP+JSON non-2xx → status + body |
//! | Waiting | [`A2aClient::send_and_wait`]: `returnImmediately`, then `GetTask` every 0.5 s → 5 s until the task settles (terminal, input / auth required) or the wait ends |
//! | Audit | one egress line per request: `tool = "a2a"`, `remote`, `op` (`card`, the method), `url`, `host`, `verdict`, `status`, `ms` |

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Method as HttpMethod, Url};
use serde_json::{json, Value};

use crate::adapters::outbound::egress::{self, EgressPolicy};
use crate::config::a2a::A2aRemoteConfig;
use crate::domain::a2a::model::{
    AgentCard, Binding, Dialect, Endpoint, GetTaskRequest, SendMessageConfiguration,
    SendMessageRequest, SendResult, Task, TaskIdRequest,
};
use crate::domain::a2a::rpc::{self, Method};
use crate::domain::a2a::v03;
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;

/// Largest reply read (bytes).
pub(crate) const MAX_BODY: usize = 8 * 1024 * 1024;
/// Polling interval bounds while waiting for a task.
const POLL_FIRST: Duration = Duration::from_millis(500);
const POLL_MAX: Duration = Duration::from_secs(5);

/// A client of one configured remote (module table).
pub(crate) struct A2aClient<'a> {
    http: reqwest::Client,
    policy: std::sync::Arc<EgressPolicy>,
    name: String,
    cfg: A2aRemoteConfig,
    scope: Option<&'a ToolScope>,
    /// `(header, value)`, sent only to [`Self::hosts`].
    credential: Option<(String, String)>,
    /// Hosts of `url` and `endpoint_url`: the only hosts called.
    hosts: Vec<String>,
    secrets: &'a SecretRegistry,
    /// The calling agent's role (audit), if any.
    agent: Option<String>,
    /// JSON-RPC request ids: 1, 2, … per client (one client per tool call).
    next_id: std::sync::atomic::AtomicU64,
    /// An operator's `--url` remote (no `[a2a.remotes]` entry): hints say so.
    adhoc: bool,
    #[cfg(test)]
    audit_tap: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

fn host_of(url: &str) -> Result<String> {
    let u = Url::parse(url.trim()).with_context(|| format!("invalid URL '{url}'"))?;
    u.host_str()
        .filter(|h| !h.is_empty())
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| anyhow!("URL '{url}' has no host"))
}

impl<'a> A2aClient<'a> {
    /// The client of remote `name`. `scope` = the calling tool's (its
    /// `env_reads` gate the credential, its `net_hosts` every request);
    /// `None` = the operator's CLI. `wait` = the longest a call may take.
    pub(crate) fn new(
        name: &str,
        cfg: &A2aRemoteConfig,
        scope: Option<&'a ToolScope>,
        secrets: &'a SecretRegistry,
        agent: Option<String>,
        wait: Duration,
    ) -> Result<Self> {
        let credential = match cfg.credential() {
            Some((header, var)) => {
                if let Some(s) = scope {
                    s.check_env_read(var)?;
                }
                let value = std::env::var(var)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        anyhow!("a2a {name}: the credential variable {var} is not set")
                    })?;
                let value = if header.eq_ignore_ascii_case("authorization") {
                    format!("Bearer {value}")
                } else {
                    value
                };
                Some((header, value))
            }
            None => None,
        };
        let mut hosts = vec![host_of(&cfg.url)?];
        if let Some(e) = &cfg.endpoint_url {
            hosts.push(host_of(e)?);
        }
        let policy = egress::policy();
        let http = policy.peer_client(wait + Duration::from_secs(15))?;
        Ok(Self {
            http,
            policy,
            name: name.to_string(),
            cfg: cfg.clone(),
            scope,
            credential,
            hosts,
            secrets,
            agent,
            next_id: std::sync::atomic::AtomicU64::new(1),
            adhoc: false,
            #[cfg(test)]
            audit_tap: None,
        })
    }

    /// An operator's `--url` remote: no config entry to point hints at.
    pub(crate) fn adhoc(mut self) -> Self {
        self.adhoc = true;
        self
    }

    /// The configured remote's name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Read the remote's card (module table: card).
    pub(crate) async fn card(&self) -> Result<AgentCard> {
        let urls = self.cfg.card_urls();
        let last = urls.len() - 1;
        for (i, raw) in urls.iter().enumerate() {
            let url = Url::parse(raw).with_context(|| format!("a2a {}: card URL", self.name))?;
            let (status, body) = self
                .exchange("card", HttpMethod::GET, &url, None, Dialect::V1)
                .await?;
            if status == 404 && i < last {
                continue;
            }
            if !(200..300).contains(&status) {
                bail!(
                    "a2a {}: the card at {url} answered HTTP {status}: {}",
                    self.name,
                    self.snippet(&body)
                );
            }
            let card: AgentCard = serde_json::from_value(body)
                .with_context(|| format!("a2a {}: {url} is not an agent card", self.name))?;
            if card.name.is_empty() && card.supported_interfaces.is_empty() && card.url.is_none() {
                bail!(
                    "a2a {}: {url} is not an agent card (no name, no interface)",
                    self.name
                );
            }
            return Ok(card);
        }
        unreachable!("card_urls is never empty")
    }

    /// The interface to call (module table: interface, hosts).
    pub(crate) fn endpoint(&self, card: &AgentCard) -> Result<Endpoint> {
        let mut ep = card
            .endpoint()
            .map_err(|why| anyhow!("a2a {}: {why}", self.name))?;
        if let Some(u) = &self.cfg.endpoint_url {
            ep.url = u.trim().to_string();
        }
        let host = host_of(&ep.url)?;
        if !self.hosts.contains(&host) {
            let fix = if self.adhoc {
                format!("use --url {} (the card's URL)", ep.url)
            } else {
                format!(
                    "set [a2a.remotes.{}] endpoint_url to call it there",
                    self.name
                )
            };
            bail!(
                "a2a {}: the card's interface is at host `{host}` ({}), not `{}` — tengu calls only the \
                 configured hosts; {fix}",
                self.name,
                ep.url,
                self.hosts[0],
            );
        }
        Ok(ep)
    }

    /// Card, then interface.
    pub(crate) async fn connect(&self) -> Result<(AgentCard, Endpoint)> {
        let card = self.card().await?;
        let ep = self.endpoint(&card)?;
        Ok((card, ep))
    }

    /// `SendMessage` once.
    pub(crate) async fn send(&self, ep: &Endpoint, req: &SendMessageRequest) -> Result<SendResult> {
        let mut req = req.clone();
        req.tenant = ep.tenant.clone();
        let (params, op) = match ep.dialect {
            Dialect::V1 => (serde_json::to_value(&req)?, Method::SendMessage),
            Dialect::V03 => (v03::send_request_to_v03(&req), Method::SendMessage),
        };
        let v = self.call(ep, op, params).await?;
        let parsed = match ep.dialect {
            Dialect::V1 => serde_json::from_value::<SendResult>(v.clone())
                .map_err(|e| e.to_string())
                .or_else(|_| v03::send_result_from_v03(&v)),
            Dialect::V03 => v03::send_result_from_v03(&v),
        };
        parsed.map_err(|why| {
            anyhow!(
                "a2a {}: SendMessage answered no task or message ({why}): {}",
                self.name,
                self.snippet(&v)
            )
        })
    }

    /// `GetTask`.
    pub(crate) async fn get_task(
        &self,
        ep: &Endpoint,
        id: &str,
        history_length: Option<i64>,
    ) -> Result<Task> {
        let params = match ep.dialect {
            Dialect::V1 => serde_json::to_value(GetTaskRequest {
                tenant: ep.tenant.clone(),
                id: id.to_string(),
                history_length,
            })?,
            Dialect::V03 => {
                let mut p = json!({"id": id});
                if let Some(h) = history_length {
                    p["historyLength"] = json!(h);
                }
                p
            }
        };
        let v = self.call(ep, Method::GetTask, params).await?;
        Ok(self.task_of(ep, &v))
    }

    /// `CancelTask`.
    pub(crate) async fn cancel(&self, ep: &Endpoint, id: &str) -> Result<Task> {
        let params = match ep.dialect {
            Dialect::V1 => serde_json::to_value(TaskIdRequest {
                tenant: ep.tenant.clone(),
                id: id.to_string(),
                metadata: None,
            })?,
            Dialect::V03 => json!({"id": id}),
        };
        let v = self.call(ep, Method::CancelTask, params).await?;
        Ok(self.task_of(ep, &v))
    }

    /// Send with `returnImmediately`, then poll until the task settles or
    /// `wait` ends (module table: waiting). `wait` zero = the first answer.
    pub(crate) async fn send_and_wait(
        &self,
        ep: &Endpoint,
        req: &SendMessageRequest,
        wait: Duration,
    ) -> Result<SendResult> {
        let deadline = Instant::now() + wait;
        let mut req = req.clone();
        let mut c = req.configuration.take().unwrap_or_default();
        c.return_immediately = true;
        req.configuration = Some(SendMessageConfiguration { ..c });
        match self.send(ep, &req).await? {
            SendResult::Task(task) => Ok(SendResult::Task(self.settle(ep, task, deadline).await?)),
            message => Ok(message),
        }
    }

    /// `GetTask`, then poll it until it settles or `wait` ends.
    pub(crate) async fn get_and_wait(
        &self,
        ep: &Endpoint,
        id: &str,
        wait: Duration,
    ) -> Result<Task> {
        let deadline = Instant::now() + wait;
        let task = self.get_task(ep, id, Some(0)).await?;
        self.settle(ep, task, deadline).await
    }

    /// Poll `task` (0.5 s → 5 s) until it settles or `deadline`.
    async fn settle(&self, ep: &Endpoint, mut task: Task, deadline: Instant) -> Result<Task> {
        let mut every = POLL_FIRST;
        while !task.status.state.is_settled() {
            let now = Instant::now();
            if now >= deadline || task.id.is_empty() {
                break;
            }
            tokio::time::sleep(every.min(deadline - now)).await;
            every = (every * 3 / 2).min(POLL_MAX);
            task = self
                .get_task(ep, &task.id, Some(0))
                .await
                .with_context(|| {
                    format!(
                        "a2a {}: task {} is not settled and polling it failed",
                        self.name, task.id
                    )
                })?;
        }
        Ok(task)
    }

    fn task_of(&self, ep: &Endpoint, v: &Value) -> Task {
        match ep.dialect {
            Dialect::V1 => {
                serde_json::from_value(v.clone()).unwrap_or_else(|_| v03::task_from_v03(v))
            }
            Dialect::V03 => v03::task_from_v03(v),
        }
    }

    /// One operation on `ep` in its binding; the result object.
    async fn call(&self, ep: &Endpoint, m: Method, params: Value) -> Result<Value> {
        match ep.binding {
            Binding::JsonRpc => self.json_rpc(ep, m, params).await,
            Binding::HttpJson => self.rest(ep, m, params).await,
        }
    }

    async fn json_rpc(&self, ep: &Endpoint, m: Method, params: Value) -> Result<Value> {
        let name = m.name(ep.dialect).ok_or_else(|| {
            anyhow!(
                "a2a {}: {m:?} is not an A2A {} method",
                self.name,
                ep.dialect.version()
            )
        })?;
        let url = Url::parse(&ep.url).with_context(|| format!("a2a {}: endpoint", self.name))?;
        let id = Value::from(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        let body = rpc::request(&id, name, params);
        let (status, v) = self
            .exchange(name, HttpMethod::POST, &url, Some(&body), ep.dialect)
            .await?;
        if status == 401 || status == 403 {
            let why = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| self.snippet(&v));
            bail!(
                "a2a {}: {name} answered HTTP {status} (not authorized): {why} — check the credential of [a2a.remotes.{}] (bearer_env / header_env)",
                self.name,
                self.name
            );
        }
        if v.get("error").is_none() && !(200..300).contains(&status) {
            bail!(
                "a2a {}: {name} answered HTTP {status}: {}",
                self.name,
                self.snippet(&v)
            );
        }
        rpc::parse_response(&v).map_err(|e| anyhow!("a2a {}: {name} answered {e}", self.name))
    }

    /// The HTTP+JSON binding (v1.0 § 11): `POST /message:send`,
    /// `GET /tasks/{id}`, `POST /tasks/{id}:cancel` (a tenant prefixes the path).
    async fn rest(&self, ep: &Endpoint, m: Method, params: Value) -> Result<Value> {
        let base = ep.url.trim_end_matches('/');
        let base = match &ep.tenant {
            Some(t) => format!("{base}/{}", pct(t)),
            None => base.to_string(),
        };
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let (method, path, body) = match m {
            Method::SendMessage => (
                HttpMethod::POST,
                "/message:send".to_string(),
                Some(params.clone()),
            ),
            Method::GetTask => {
                let q = params
                    .get("historyLength")
                    .and_then(Value::as_i64)
                    .map(|h| format!("?historyLength={h}"))
                    .unwrap_or_default();
                (HttpMethod::GET, format!("/tasks/{}{q}", pct(id)), None)
            }
            Method::CancelTask => (
                HttpMethod::POST,
                format!("/tasks/{}:cancel", pct(id)),
                Some(json!({})),
            ),
            other => bail!("a2a {}: {other:?} is not called over HTTP+JSON", self.name),
        };
        let url = Url::parse(&format!("{base}{path}"))
            .with_context(|| format!("a2a {}: endpoint", self.name))?;
        let op = m.name(Dialect::V1).unwrap_or("?");
        let (status, v) = self
            .exchange(op, method, &url, body.as_ref(), Dialect::V1)
            .await?;
        if !(200..300).contains(&status) {
            let msg = v
                .pointer("/error/message")
                .or_else(|| v.get("detail"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| self.snippet(&v));
            // `google.rpc.ErrorInfo.reason` (`TASK_NOT_FOUND`, …), when given.
            let reason = v
                .pointer("/error/details")
                .and_then(Value::as_array)
                .and_then(|d| {
                    d.iter()
                        .find_map(|x| x.get("reason").and_then(Value::as_str))
                })
                .map(|r| format!(" {r}"))
                .unwrap_or_default();
            bail!(
                "a2a {}: {op} answered HTTP {status}{reason}: {msg}",
                self.name
            );
        }
        Ok(v)
    }

    /// One request: gate, send, read ≤ [`MAX_BODY`], audit.
    async fn exchange(
        &self,
        op: &str,
        method: HttpMethod,
        url: &Url,
        body: Option<&Value>,
        dialect: Dialect,
    ) -> Result<(u16, Value)> {
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let gate = self
            .policy
            .check_url(url)
            .and_then(|_| self.scope.map_or(Ok(()), |s| s.check_net_host(&host)))
            .and_then(|_| {
                if self.hosts.contains(&host) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "a2a {}: host `{host}` is not the configured remote's",
                        self.name
                    ))
                }
            });
        if let Err(e) = gate {
            self.audit(op, &method, url, Err(&e), None);
            return Err(e);
        }
        let mut req = self
            .http
            .request(method.clone(), url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header("A2A-Version", dialect.version());
        if let Some((h, v)) = &self.credential {
            req = req.header(h.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let started = Instant::now();
        let read = async {
            let mut resp = req.send().await?;
            let status = resp.status().as_u16();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if buf.len() + chunk.len() > MAX_BODY {
                    bail!("reply larger than {MAX_BODY} bytes");
                }
                buf.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>((status, buf))
        };
        let outcome = read.await;
        let ms = started.elapsed().as_millis() as u64;
        let (status, buf) = match outcome {
            Ok(r) => r,
            Err(e) => {
                let e = anyhow!(
                    "a2a {}: {op} {url} failed (egress {}): {e:#}",
                    self.name,
                    self.policy.proxy().unwrap_or("direct")
                );
                self.audit(op, &method, url, Err(&e), Some(ms));
                return Err(e);
            }
        };
        self.audit(op, &method, url, Ok(status), Some(ms));
        if (300..400).contains(&status) {
            bail!(
                "a2a {}: {op} {url} answered a redirect (HTTP {status}) — redirects are not followed; \
                 point [a2a.remotes.{}] at the final URL",
                self.name,
                self.name
            );
        }
        let value = serde_json::from_slice(&buf)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&buf).into_owned()));
        Ok((status, value))
    }

    /// The first 300 chars of a reply, redacted.
    fn snippet(&self, v: &Value) -> String {
        let text = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let cut: String = text.chars().take(300).collect();
        self.secrets.redact(&cut)
    }

    fn audit(
        &self,
        op: &str,
        method: &HttpMethod,
        url: &Url,
        outcome: std::result::Result<u16, &anyhow::Error>,
        ms: Option<u64>,
    ) {
        let mut event = json!({
            "tool": "a2a",
            "remote": self.name,
            "op": op,
            "method": method.as_str(),
            "url": self.secrets.redact(url.as_str()),
            "host": url.host_str(),
            "ms": ms,
        });
        if let Some(a) = &self.agent {
            event["agent"] = json!(a);
        }
        match outcome {
            Ok(status) => {
                event["verdict"] = json!("allowed");
                event["status"] = json!(status);
            }
            Err(e) => {
                event["verdict"] = json!(if ms.is_some() { "error" } else { "denied" });
                event["reason"] = json!(self.secrets.redact(&format!("{e:#}")));
            }
        }
        #[cfg(test)]
        if let Some(tap) = &self.audit_tap {
            tap.lock().unwrap().push(event.clone());
        }
        self.policy.audit(event);
    }
}

/// Percent-encode one path segment (everything but `A-Z a-z 0-9 - . _ ~`).
fn pct(seg: &str) -> String {
    seg.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::a2a::model::{Message, Part, Role, TaskState};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    /// A loopback A2A agent: answers `GET` with `card` and every `POST`
    /// through `reply` (the JSON body → the reply JSON); records each request.
    pub(crate) struct Mock {
        pub url: String,
        pub seen: Arc<Mutex<Vec<(String, String, Value)>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub(crate) fn mock(
        card: impl Fn(&str) -> Option<Value> + Send + 'static,
        reply: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> Mock {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, st2) = (Arc::clone(&seen), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !st2.load(std::sync::atomic::Ordering::SeqCst) {
                let Ok((mut sock, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                sock.set_nonblocking(false).ok();
                sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Head, then a body of Content-Length.
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (String::from_utf8_lossy(&buf).to_string(), String::new());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(at) = text.find("\r\n\r\n") {
                        let head = text[..at].to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let mut body = buf[at + 4..].to_vec();
                        while body.len() < len {
                            let n = sock.read(&mut chunk).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            body.extend_from_slice(&chunk[..n]);
                        }
                        break (head, String::from_utf8_lossy(&body).to_string());
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let mut parts = line.split(' ');
                let (method, path) = (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                );
                let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                s2.lock()
                    .unwrap()
                    .push((method.clone(), format!("{path}\n{head}"), json.clone()));
                let (code, out) = if method == "GET" {
                    match card(&path) {
                        Some(c) => (200, c.to_string()),
                        None => (404, "{}".to_string()),
                    }
                } else {
                    (200, reply(&path, &json).to_string())
                };
                let _ = write!(
                    sock,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                );
            }
        });
        Mock { url, seen, stop }
    }

    pub(crate) fn v1_card(endpoint: &str) -> Value {
        json!({
            "name": "Mock agent", "description": "Answers.", "version": "1.0.0",
            "supportedInterfaces": [{"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
            "capabilities": {}, "defaultInputModes": ["text/plain"], "defaultOutputModes": ["text/plain"],
            "skills": [{"id": "echo", "name": "Echo", "description": "Echoes.", "tags": []}]
        })
    }

    fn remote(url: &str) -> A2aRemoteConfig {
        serde_json::from_value(json!({"url": url, "timeout_secs": 10, "max_result_chars": 1000}))
            .unwrap()
    }

    fn request(text: &str) -> SendMessageRequest {
        SendMessageRequest {
            message: Message {
                message_id: "m-1".into(),
                role: Role::User,
                parts: vec![Part::text(text)],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn rpc_result(body: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": body["id"], "result": result})
    }

    #[tokio::test]
    async fn sends_v1_and_polls_until_the_task_settles() {
        let polls = Arc::new(Mutex::new(0usize));
        let p = Arc::clone(&polls);
        let m = mock(
            |path| {
                (path == "/.well-known/agent-card.json").then(|| v1_card("http://127.0.0.1:1/x"))
            },
            move |_, body| {
                let method = body["method"].as_str().unwrap_or("");
                match method {
                    "SendMessage" => rpc_result(
                        body,
                        json!({"task": {"id": "t-1", "contextId": "c-1",
                        "status": {"state": "TASK_STATE_WORKING"}}}),
                    ),
                    "GetTask" => {
                        let mut n = p.lock().unwrap();
                        *n += 1;
                        let state = if *n >= 2 {
                            "TASK_STATE_COMPLETED"
                        } else {
                            "TASK_STATE_WORKING"
                        };
                        rpc_result(
                            body,
                            json!({"id": "t-1", "contextId": "c-1", "status": {"state": state},
                            "artifacts": [{"artifactId": "a", "parts": [{"text": "done"}]}]}),
                        )
                    }
                    _ => {
                        json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "no"}})
                    }
                }
            },
        );
        // The card's interface names another port: same host, allowed.
        let mut cfg = remote(&m.url);
        cfg.endpoint_url = Some(m.url.clone());
        let secrets = SecretRegistry::new();
        let c =
            A2aClient::new("mock", &cfg, None, &secrets, None, Duration::from_secs(10)).unwrap();
        let (card, ep) = c.connect().await.unwrap();
        assert_eq!(card.name, "Mock agent");
        assert_eq!(ep.dialect, Dialect::V1);
        let r = c
            .send_and_wait(&ep, &request("hi"), Duration::from_secs(10))
            .await
            .unwrap();
        let SendResult::Task(t) = r else {
            panic!("{r:?}")
        };
        assert_eq!(t.status.state, TaskState::Completed);
        assert_eq!(*polls.lock().unwrap(), 2);
        let seen = m.seen.lock().unwrap();
        let send = seen
            .iter()
            .find(|(_, _, b)| b["method"] == "SendMessage")
            .unwrap();
        assert_eq!(send.2["params"]["configuration"]["returnImmediately"], true);
        assert!(
            send.1.to_ascii_lowercase().contains("a2a-version: 1.0"),
            "{}",
            send.1
        );
    }

    #[tokio::test]
    async fn speaks_v03_to_a_v03_card_and_falls_back_to_agent_json() {
        let m = mock(
            |path| {
                (path == "/.well-known/agent.json").then(|| {
                    json!({"name": "Old", "description": "d", "version": "1", "protocolVersion": "0.3.0",
                           "url": "http://127.0.0.1:9/rpc", "preferredTransport": "JSONRPC",
                           "capabilities": {}, "defaultInputModes": [], "defaultOutputModes": [], "skills": []})
                })
            },
            |_, body| {
                assert_eq!(body["method"], "message/send");
                assert_eq!(body["params"]["message"]["kind"], "message");
                assert_eq!(body["params"]["message"]["parts"][0]["kind"], "text");
                rpc_result(
                    body,
                    json!({"kind": "message", "messageId": "r-1", "role": "agent",
                    "parts": [{"kind": "text", "text": "old hello"}]}),
                )
            },
        );
        let mut cfg = remote(&m.url);
        cfg.endpoint_url = Some(format!("{}/rpc", m.url));
        let secrets = SecretRegistry::new();
        let c = A2aClient::new("old", &cfg, None, &secrets, None, Duration::from_secs(5)).unwrap();
        let (_, ep) = c.connect().await.unwrap();
        assert_eq!(ep.dialect, Dialect::V03);
        let r = c
            .send_and_wait(&ep, &request("hi"), Duration::ZERO)
            .await
            .unwrap();
        let SendResult::Message(msg) = r else {
            panic!("{r:?}")
        };
        assert_eq!(msg.parts[0].text.as_deref(), Some("old hello"));
    }

    #[tokio::test]
    async fn a_card_naming_another_host_is_refused_before_any_call() {
        let m = mock(
            |_| Some(v1_card("https://elsewhere.example.com/a2a")),
            |_, _| json!({}),
        );
        let secrets = SecretRegistry::new();
        let cfg = remote(&m.url);
        let c =
            A2aClient::new("pinned", &cfg, None, &secrets, None, Duration::from_secs(5)).unwrap();
        let e = c.connect().await.unwrap_err().to_string();
        assert!(e.contains("host `elsewhere.example.com`"), "{e}");
        assert!(e.contains("endpoint_url"), "{e}");
        assert_eq!(m.seen.lock().unwrap().len(), 1, "only the card was read");
    }

    #[tokio::test]
    async fn rpc_errors_and_scope_denials_read_clearly() {
        let m = mock(
            |_| Some(v1_card("http://127.0.0.1/")),
            |_, body| {
                json!({"jsonrpc": "2.0", "id": body["id"],
                             "error": {"code": -32001, "message": "Task not found"}})
            },
        );
        let mut cfg = remote(&m.url);
        cfg.endpoint_url = Some(m.url.clone());
        let secrets = SecretRegistry::new();
        let c = A2aClient::new("mock", &cfg, None, &secrets, None, Duration::from_secs(5)).unwrap();
        let (_, ep) = c.connect().await.unwrap();
        let e = c.get_task(&ep, "t-9", None).await.unwrap_err().to_string();
        assert!(
            e.contains("GetTask answered TaskNotFoundError (-32001): Task not found"),
            "{e}"
        );

        let scope = ToolScope {
            net_hosts: vec!["api.example.com".into()],
            ..Default::default()
        };
        let mut c = A2aClient::new(
            "mock",
            &cfg,
            Some(&scope),
            &secrets,
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        let tap = Arc::new(Mutex::new(Vec::new()));
        c.audit_tap = Some(Arc::clone(&tap));
        let e = c.card().await.unwrap_err().to_string();
        assert!(e.contains("127.0.0.1"), "{e}");
        let audit = tap.lock().unwrap();
        assert_eq!(audit[0]["verdict"], "denied");
        assert_eq!(audit[0]["tool"], "a2a");
        assert_eq!(audit[0]["op"], "card");
    }

    #[test]
    fn the_credential_needs_its_variable_and_the_scope() {
        let mut cfg = remote("https://agents.example.com");
        cfg.bearer_env = Some("TENGU_A2A_TEST_UNSET_TOKEN".into());
        let secrets = SecretRegistry::new();
        let e = A2aClient::new("r", &cfg, None, &secrets, None, Duration::from_secs(1))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("TENGU_A2A_TEST_UNSET_TOKEN is not set"), "{e}");
        let scope = ToolScope {
            net_hosts: vec!["*".into()],
            ..Default::default()
        };
        let e = A2aClient::new(
            "r",
            &cfg,
            Some(&scope),
            &secrets,
            None,
            Duration::from_secs(1),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(e.contains("TENGU_A2A_TEST_UNSET_TOKEN"), "{e}");
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(pct("t-1_a.b~"), "t-1_a.b~");
        assert_eq!(pct("a/b c"), "a%2Fb%20c");
    }
}
