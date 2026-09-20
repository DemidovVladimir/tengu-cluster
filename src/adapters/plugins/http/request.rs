// src/adapters/plugins/http/request.rs
//! `http_request` tool — generic HTTP client for skill-driven API calls.
//!
//! Migrated from `http_tool_executor.rs` during Phase A / task A2.
//! The old executor used `tokio::task::block_in_place` to bridge sync→async;
//! this implementation is natively async and uses `ctx.http` directly.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Method, Url};
use serde_json::{json, Value};

use std::time::Instant;

use crate::adapters::egress;
use crate::adapters::tool_builder::validate_path;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::tool_utils::require_str;
use crate::adapters::types::ToolDef;

pub(crate) struct HttpRequestTool {
    def: ToolDef,
}

impl HttpRequestTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "http_request",
                "Make an HTTP request.",
                json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Full URL. Supports $ENV_VAR (e.g. $MOLECULE_LABS_URL or https://api.example.com/v1/resource)"
                        },
                        "method": {
                            "type": "string",
                            "description": "HTTP method",
                            "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"]
                        },
                        "headers": {
                            "type": "string",
                            "description": "JSON object of request headers. Use $ENV_VAR for secrets, e.g. {\"Authorization\": \"Bearer $BEACH_API_KEY\"}"
                        },
                        "body": {
                            "type": "string",
                            "description": "Request body — JSON string for application/json, or raw text"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Workspace-relative path for multipart/form-data file upload"
                        },
                        "file_field_name": {
                            "type": "string",
                            "description": "Form field name for the uploaded file (default: \"file\")"
                        },
                        "auth_bearer_env": {
                            "type": "string",
                            "description": "Env var name for Bearer token auth (e.g. \"BEACH_API_KEY\")"
                        },
                        "auth_basic_user_env": {
                            "type": "string",
                            "description": "Env var name for Basic auth username (e.g. \"PRIVY_APP_ID\")"
                        },
                        "auth_basic_pass_env": {
                            "type": "string",
                            "description": "Env var name for Basic auth password (e.g. \"PRIVY_APP_SECRET\")"
                        },
                        "return_body": {
                            "type": "boolean",
                            "description": "If true, include the response body in the result. Default false — only status is returned on success. Set to true when you need data from the response (e.g. upload URLs, created resource IDs). Errors always include the body."
                        }
                    },
                    "required": ["url", "method"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // Scope gate first (also enforced inside `expand_env_refs` for env vars).
        let url_raw = require_str(args, "http_request", "url")?;
        let method_str = require_str(args, "http_request", "method")?;
        let policy = egress::policy();
        let audit = HopAudit {
            policy: &policy,
            ctx,
            method: method_str,
            url_raw,
            args,
        };
        let request = match PreparedRequest::from_args(args, url_raw, method_str, ctx) {
            Ok(r) => r,
            Err(e) => {
                audit.record(0, url_raw, None, Err(&e), None);
                return Err(e);
            }
        };
        // Hop-0 gate, before anything is sent: `[egress]` (scheme + host
        // ceiling), then this tool's `net_hosts` scope. Redirect hops repeat
        // the same pair in `check_hop`.
        let host = request.url.host_str().unwrap_or("");
        let gate = policy
            .check_url(&request.url)
            .and_then(|_| ctx.scope.check_net_host(host));
        if let Err(e) = gate {
            audit.record(0, url_raw, None, Err(&e), None);
            return Err(e);
        }
        send_following_redirects(&request, ctx, &policy, &audit)
            .await
            .map(ToolOutput::from)
    }
}

/// Redirect hops `http_request` follows itself (the tool client has
/// redirects off) so each hop passes the same egress + scope gate.
const MAX_REDIRECTS: u32 = 10;

/// Everything needed to (re)send the request on each hop.
struct PreparedRequest {
    url: Url,
    method: Method,
    body: Option<String>,
    file: Option<(Vec<u8>, String)>,
    file_field_name: String,
    /// `(name, value, carried_env_ref)` — env-expanded headers are treated
    /// as credentials and dropped on a cross-origin redirect.
    headers: Vec<(String, String, bool)>,
    auth: ResolvedAuth,
    return_body: bool,
}

impl PreparedRequest {
    fn from_args(args: &Value, url_raw: &str, method_str: &str, ctx: &ToolCtx<'_>) -> Result<Self> {
        let expanded = expand_env_refs(url_raw, ctx)?;
        if !expanded.starts_with("https://") && !expanded.starts_with("http://") {
            bail!("http_request: url must start with http:// or https://");
        }
        let url = Url::parse(&expanded)
            .map_err(|e| anyhow!("http_request: invalid url '{}': {}", url_raw, e))?;
        // The hop-0 egress + scope gate runs in `execute` (see there).

        let method = Method::from_bytes(method_str.trim().to_uppercase().as_bytes())
            .context("http_request: invalid HTTP method")?;

        let file = match args.get("file_path").and_then(|v| v.as_str()) {
            Some(fp) => {
                let validated = validate_path(ctx.workspace, fp)?;
                ctx.scope.check_fs_read(&validated)?;
                let bytes = std::fs::read(&validated)
                    .map_err(|e| anyhow!("Cannot read file '{}': {}", fp, e))?;
                let filename = validated
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                Some((bytes, filename))
            }
            None => None,
        };

        let headers_json = arg_to_string(args, "headers");
        let headers = parse_headers(headers_json.as_deref().unwrap_or("{}"), ctx)?;
        let auth = resolve_auth(
            args.get("auth_bearer_env").and_then(|v| v.as_str()),
            args.get("auth_basic_user_env").and_then(|v| v.as_str()),
            args.get("auth_basic_pass_env").and_then(|v| v.as_str()),
            ctx,
        )?;

        Ok(Self {
            url,
            method,
            body: arg_to_string(args, "body"),
            file,
            file_field_name: args
                .get("file_field_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            headers,
            auth,
            return_body: args
                .get("return_body")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    fn is_multipart(&self) -> bool {
        self.file.is_some() && !self.file_field_name.is_empty()
    }

    /// Build one hop. `with_body = false` after a 301/302/303 (method became
    /// GET); `with_credentials = false` once the hop leaves the origin.
    fn build(
        &self,
        http: &reqwest::Client,
        method: &Method,
        url: &Url,
        with_body: bool,
        with_credentials: bool,
    ) -> Result<reqwest::RequestBuilder> {
        let mut req = http.request(method.clone(), url.clone());
        if with_credentials {
            req = apply_auth(req, &self.auth);
        }
        let multipart = with_body && self.is_multipart();
        for (key, value, carried_env) in &self.headers {
            if multipart && key.eq_ignore_ascii_case("content-type") {
                continue;
            }
            if !with_body && is_body_header(key) {
                continue;
            }
            if !with_credentials && (*carried_env || is_sensitive_header(key)) {
                continue;
            }
            req = req.header(key, value);
        }
        if !with_body {
            return Ok(req);
        }

        Ok(match &self.file {
            Some((bytes, filename)) if multipart => {
                let mime = mime_from_filename(filename);
                let part = reqwest::multipart::Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(&mime)?;
                let mut form =
                    reqwest::multipart::Form::new().part(self.file_field_name.clone(), part);
                if let Some(ref b) = self.body {
                    if let Ok(map) =
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(b)
                    {
                        for (k, v) in map {
                            form = form.text(
                                k,
                                v.as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| v.to_string()),
                            );
                        }
                    }
                }
                req.multipart(form)
            }
            Some((bytes, filename)) => {
                // Raw body upload (e.g. S3 presigned PUT).
                let content_type = match self.header("content-type") {
                    Some(ct) => ct.to_string(),
                    None => {
                        let mime = mime_from_filename(filename);
                        req = req.header("Content-Type", &mime);
                        mime
                    }
                };
                tracing::debug!(
                    url = %url,
                    file = %filename,
                    size = bytes.len(),
                    content_type = %content_type,
                    "S3 raw body upload"
                );
                req.header("Content-Length", bytes.len().to_string())
                    .body(bytes.clone())
            }
            None => {
                // Standard request.
                if let Some(ref b) = self.body {
                    if self.header("content-type").is_none() {
                        req = req.header("Content-Type", "application/json");
                    }
                    req = req.body(b.clone());
                }
                req
            }
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v, _)| v.as_str())
    }
}

/// The per-hop gate: `[egress]` (scheme + sandbox-wide host ceiling), then
/// the tool's own `net_hosts` scope.
fn check_hop(url: &Url, ctx: &ToolCtx<'_>, policy: &egress::EgressPolicy) -> Result<()> {
    policy.check_url(url)?;
    ctx.scope.check_net_host(url.host_str().unwrap_or(""))
}

async fn send_following_redirects(
    request: &PreparedRequest,
    ctx: &ToolCtx<'_>,
    policy: &egress::EgressPolicy,
    audit: &HopAudit<'_>,
) -> Result<String> {
    let mut url = request.url.clone();
    let mut method = request.method.clone();
    let mut with_body = true;
    let mut chain: Vec<String> = Vec::new();

    for hop in 0..=MAX_REDIRECTS {
        let shown = if hop == 0 {
            audit.url_raw.to_string()
        } else {
            url.to_string()
        };
        let started = Instant::now();
        let with_credentials = same_origin(&request.url, &url);
        let sent = match request.build(ctx.http, &method, &url, with_body, with_credentials) {
            Ok(builder) => builder.send().await.map_err(anyhow::Error::from),
            Err(e) => Err(e),
        };
        let elapsed = started.elapsed().as_millis() as u64;
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                // Full cause chain in the message itself — the MCP bridge
                // and the engine only render the top-level Display.
                let e = anyhow!(
                    "http_request {} {} failed (egress {}): {:#}",
                    method,
                    url,
                    policy.proxy().unwrap_or("direct"),
                    e
                );
                audit.record(hop, &shown, Some(&method), Err(&e), Some(elapsed));
                return Err(e);
            }
        };
        let status = response.status();
        audit.record(
            hop,
            &shown,
            Some(&method),
            Ok(status.as_u16()),
            Some(elapsed),
        );

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let follow = match (status.as_u16(), &location) {
            (301..=303, Some(_)) => {
                if method != Method::GET && method != Method::HEAD {
                    method = Method::GET;
                }
                with_body = false;
                true
            }
            (307 | 308, Some(_)) => true,
            _ => false,
        };
        if !follow {
            let mut text = format_response(url.as_str(), response, request.return_body).await?;
            if !chain.is_empty() {
                text = format!("{text}\n(redirected from {})", chain.join(" -> "));
            }
            return Ok(text);
        }
        if hop == MAX_REDIRECTS {
            bail!("http_request: more than {MAX_REDIRECTS} redirects, last hop {url}");
        }

        let location = location.unwrap_or_default();
        let next = url
            .join(&location)
            .map_err(|e| anyhow!("http_request: bad redirect Location '{location}': {e}"))?;
        if let Err(e) = check_hop(&next, ctx, policy) {
            let e = e.context(format!("http_request: redirect {url} -> {next} blocked"));
            audit.record(hop + 1, next.as_str(), Some(&method), Err(&e), None);
            return Err(e);
        }
        chain.push(url.to_string());
        url = next;
    }
    unreachable!("loop returns on the last hop")
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn is_sensitive_header(name: &str) -> bool {
    [
        "authorization",
        "proxy-authorization",
        "cookie",
        "cookie2",
        "www-authenticate",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

fn is_body_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-type") || name.eq_ignore_ascii_case("content-length")
}

/// One audit record per hop. Hop 0 logs the URL exactly as the LLM wrote it
/// (`$VAR` refs unexpanded, so no secrets); header values are never logged.
struct HopAudit<'a> {
    policy: &'a egress::EgressPolicy,
    ctx: &'a ToolCtx<'a>,
    method: &'a str,
    url_raw: &'a str,
    args: &'a Value,
}

impl HopAudit<'_> {
    fn record(
        &self,
        hop: u32,
        url: &str,
        method: Option<&Method>,
        outcome: std::result::Result<u16, &anyhow::Error>,
        ms: Option<u64>,
    ) {
        let redact = |s: &str| self.ctx.secret_registry.redact(s);
        let header_names: Vec<String> = arg_to_string(self.args, "headers")
            .and_then(|h| serde_json::from_str::<serde_json::Map<String, Value>>(&h).ok())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let mut event = json!({
            "tool": "http_request",
            "hop": hop,
            "method": method.map(|m| m.to_string()).unwrap_or_else(|| self.method.to_string()),
            "url": redact(url),
            "host": Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)),
            "headers": header_names,
            "auth_env": self.args.get("auth_bearer_env").or_else(|| self.args.get("auth_basic_user_env")),
            "body": arg_to_string(self.args, "body").map(|b| redact(&b)),
            "file_path": self.args.get("file_path"),
            "ms": ms,
        });
        if let Some(role) = self.ctx.agent_config.and_then(|a| a.role.clone()) {
            event["agent"] = role.into();
        }
        match outcome {
            Ok(status) => {
                event["verdict"] = "allowed".into();
                event["status"] = status.into();
            }
            Err(e) => {
                let sent = ms.is_some();
                event["verdict"] = if sent { "error" } else { "denied" }.into();
                event["reason"] = redact(&format!("{e:#}")).into();
            }
        }
        self.policy.audit(event);
    }
}

// ---------------------------------------------------------------------------
// Helpers (lift-and-shift from http_tool_executor.rs)
// ---------------------------------------------------------------------------

fn arg_to_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

async fn format_response(
    url: &str,
    response: reqwest::Response,
    return_body: bool,
) -> Result<String> {
    let status = response.status();

    // On success without return_body, skip reading the response entirely.
    if status.is_success() && !return_body {
        return Ok(format!("HTTP {} {}", status.as_u16(), url));
    }

    let body_text = response.text().await.unwrap_or_default();

    if status.is_success() {
        Ok(format!("HTTP {} {}\n{}", status.as_u16(), url, body_text))
    } else {
        Ok(format!("HTTP {} {}\n{}", status.as_u16(), url, body_text))
    }
}

enum ResolvedAuth {
    None,
    Bearer(String),
    Basic(String, String),
}

fn resolve_auth(
    bearer_env: Option<&str>,
    basic_user_env: Option<&str>,
    basic_pass_env: Option<&str>,
    ctx: &ToolCtx<'_>,
) -> Result<ResolvedAuth> {
    if let Some(env_name) = bearer_env {
        let token = read_env(env_name, ctx)?;
        return Ok(ResolvedAuth::Bearer(token));
    }
    if let (Some(user_env), Some(pass_env)) = (basic_user_env, basic_pass_env) {
        let user = read_env(user_env, ctx)?;
        let pass = read_env(pass_env, ctx)?;
        return Ok(ResolvedAuth::Basic(user, pass));
    }
    Ok(ResolvedAuth::None)
}

fn apply_auth(request: reqwest::RequestBuilder, auth: &ResolvedAuth) -> reqwest::RequestBuilder {
    match auth {
        ResolvedAuth::None => request,
        ResolvedAuth::Bearer(token) => request.bearer_auth(token),
        ResolvedAuth::Basic(user, pass) => request.basic_auth(user, Some(pass)),
    }
}

fn parse_headers(raw: &str, ctx: &ToolCtx<'_>) -> Result<Vec<(String, String, bool)>> {
    if raw.trim().is_empty() || raw.trim() == "{}" {
        return Ok(Vec::new());
    }
    let object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(raw).map_err(|e| anyhow!("headers must be a JSON object: {}", e))?;
    object
        .into_iter()
        .map(|(key, value)| {
            let v = value
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| value.to_string());
            let carried_env = v.contains('$');
            Ok((key, expand_env_refs(&v, ctx)?, carried_env))
        })
        .collect()
}

/// Expand `$UPPERCASE_VAR` tokens in a string from the process environment.
///
/// Each env read is gated by `ctx.scope.check_env_read(name)`; scopes whose
/// `env_reads` list is empty will reject the expansion (default-deny).
/// `permissive_scope` (the fallback for tools with no configured scope) sets
/// `env_reads = ["*"]`, which the check honours as allow-any; a configured
/// `[agents.*.scopes.http_request].env_reads` list narrows it.
fn expand_env_refs(input: &str, ctx: &ToolCtx<'_>) -> Result<String> {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'$' {
            idx += 1;
            let start = idx;
            while idx < bytes.len()
                && ((bytes[idx] as char).is_ascii_uppercase()
                    || (bytes[idx] as char).is_ascii_digit()
                    || bytes[idx] == b'_')
            {
                idx += 1;
            }
            let name = &input[start..idx];
            if name.is_empty() {
                out.push('$');
                continue;
            }
            let value = read_env(name, ctx)?;
            out.push_str(&value);
        } else {
            out.push(bytes[idx] as char);
            idx += 1;
        }
    }
    Ok(out)
}

/// Read an env var, gated by scope (`"*"` wildcard handled inside
/// `ToolScope::check_env_read`). Default-deny otherwise.
fn read_env(name: &str, ctx: &ToolCtx<'_>) -> Result<String> {
    ctx.scope.check_env_read(name)?;
    std::env::var(name).map_err(|_| anyhow!("Missing environment variable {}", name))
}

fn mime_from_filename(filename: &str) -> String {
    match filename.rsplit('.').next().map(|e| e.to_lowercase()) {
        Some(ref ext) if ext == "pdf" => "application/pdf".to_string(),
        Some(ref ext) if ext == "png" => "image/png".to_string(),
        Some(ref ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg".to_string(),
        Some(ref ext) if ext == "json" => "application/json".to_string(),
        Some(ref ext) if ext == "txt" || ext == "md" => "text/plain".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolScope};
    use crate::adapters::secret_builder::SecretRegistry;
    use crate::adapters::shell_executor::LocalShellExecutor;
    use crate::adapters::types::ToolCall;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;

    struct NoopActivity;
    impl ToolActivityPort for NoopActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    struct HttpHarness {
        workspace: PathBuf,
        scope: ToolScope,
        shell: Arc<dyn ShellExecutionPort>,
        http: reqwest::Client,
        secrets: SecretRegistry,
        activity: Arc<dyn ToolActivityPort>,
    }

    impl HttpHarness {
        fn with_scope(workspace: &Path, scope: ToolScope) -> Self {
            Self {
                workspace: workspace.to_path_buf(),
                scope,
                shell: Arc::new(LocalShellExecutor::new()),
                // Same shape as the production tool client: redirects off,
                // `http_request` follows them itself.
                http: egress::policy()
                    .tool_client(std::time::Duration::from_secs(5))
                    .unwrap(),
                secrets: SecretRegistry::new(),
                activity: Arc::new(NoopActivity),
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                workspace: &self.workspace,
                scope: &self.scope,
                shell: self.shell.as_ref(),
                http: &self.http,
                memory_manager: None,
                secret_registry: &self.secrets,
                activity: self.activity.as_ref(),
                conversation: crate::adapters::tool_plugin::ConversationView::empty(),
                agent_config: None,
            }
        }
    }

    #[tokio::test]
    async fn http_request_scope_denies_disallowed_host() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            net_hosts: vec!["api.example.com".to_string()],
            ..Default::default()
        };
        let harness = HttpHarness::with_scope(tmp.path(), scope);
        let tool = HttpRequestTool::new();
        let result = tool
            .execute(
                &json!({
                    "url": "https://api.other.com/v1/ping",
                    "method": "GET",
                }),
                &harness.ctx(),
            )
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("api.other.com") || msg.contains("not in allowed net_hosts"),
            "expected net_hosts denial message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn http_request_missing_url_errors() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            net_hosts: vec!["*".to_string()],
            ..Default::default()
        };
        let harness = HttpHarness::with_scope(tmp.path(), scope);
        let tool = HttpRequestTool::new();
        let result = tool.execute(&json!({}), &harness.ctx()).await;
        assert!(result.is_err(), "expected error for missing url");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("'url' is required"),
            "expected missing url message, got: {}",
            msg
        );
    }

    /// One-shot HTTP server on 127.0.0.1 answering every request with
    /// `response`; returns the base URL and a counter of requests served.
    async fn serve(response: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn http_request_redirect_to_unlisted_host_is_blocked_before_sending() {
        let (base, hits) = serve(
            "HTTP/1.1 302 Found\r\nLocation: http://blocked.invalid/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            net_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        };
        let harness = HttpHarness::with_scope(tmp.path(), scope);
        let err = HttpRequestTool::new()
            .execute(
                &json!({"url": format!("{base}/start"), "method": "GET"}),
                &harness.ctx(),
            )
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("redirect"), "{msg}");
        assert!(msg.contains("blocked.invalid"), "{msg}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http_request_follows_allowed_redirect_and_reports_chain() {
        let (target, _) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await;
        let redirect: &'static str = Box::leak(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (base, _) = serve(redirect).await;
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            net_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        };
        let harness = HttpHarness::with_scope(tmp.path(), scope);
        let out = HttpRequestTool::new()
            .execute(
                &json!({"url": format!("{base}/start"), "method": "POST", "body": "{}", "return_body": true}),
                &harness.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("HTTP 200"), "{}", out.text);
        assert!(out.text.contains("hello"), "{}", out.text);
        assert!(out.text.contains("redirected from"), "{}", out.text);
    }

    #[test]
    fn cross_origin_redirect_drops_credentials() {
        let a = Url::parse("https://api.example.com/x").unwrap();
        assert!(same_origin(
            &a,
            &Url::parse("https://api.example.com/y").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("https://other.example.com/y").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("http://api.example.com/y").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("https://api.example.com:8443/y").unwrap()
        ));
    }
}
