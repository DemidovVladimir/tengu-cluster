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

use crate::adapters::tool_builder::validate_path;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
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
        let url_raw = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("http_request: missing 'url'"))?;
        let url = expand_env_refs(url_raw, ctx)?;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            bail!("http_request: url must start with http:// or https://");
        }
        let host = host_from_url(&url)?;
        ctx.scope.check_net_host(&host)?;

        let method_str = args
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("http_request: missing 'method'"))?;
        let body = arg_to_string(args, "body");
        let headers_json = arg_to_string(args, "headers");
        let file_path = args.get("file_path").and_then(|v| v.as_str());
        let file_field_name = args
            .get("file_field_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let auth_bearer_env = args.get("auth_bearer_env").and_then(|v| v.as_str());
        let auth_basic_user_env = args.get("auth_basic_user_env").and_then(|v| v.as_str());
        let auth_basic_pass_env = args.get("auth_basic_pass_env").and_then(|v| v.as_str());
        let return_body = args
            .get("return_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let method = Method::from_bytes(method_str.trim().to_uppercase().as_bytes())
            .context("http_request: invalid HTTP method")?;

        let file_data = match file_path {
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

        let headers = parse_headers(headers_json.as_deref().unwrap_or("{}"), ctx)?;
        let auth = resolve_auth(
            auth_bearer_env,
            auth_basic_user_env,
            auth_basic_pass_env,
            ctx,
        )?;

        let is_multipart = file_data.is_some() && !file_field_name.is_empty();

        // Common setup: method, url, auth, headers (once instead of per-branch).
        let mut req = ctx.http.request(method, &url);
        req = apply_auth(req, &auth);
        for (key, value) in &headers {
            if is_multipart && key.eq_ignore_ascii_case("content-type") {
                continue;
            }
            req = req.header(key, value);
        }

        let response = match file_data {
            Some((bytes, filename)) if !file_field_name.is_empty() => {
                // Multipart upload.
                let mime = mime_from_filename(&filename);
                let part = reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(&mime)?;
                let mut form = reqwest::multipart::Form::new().part(file_field_name, part);
                if let Some(ref b) = body {
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
                req.multipart(form).send().await?
            }
            Some((bytes, filename)) => {
                // Raw body upload (e.g. S3 presigned PUT).
                let content_type = if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    let mime = mime_from_filename(&filename);
                    req = req.header("Content-Type", &mime);
                    mime
                } else {
                    headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                tracing::debug!(
                    url = %url,
                    file = %filename,
                    size = bytes.len(),
                    content_type = %content_type,
                    "S3 raw body upload"
                );
                req.header("Content-Length", bytes.len().to_string())
                    .body(bytes)
                    .send()
                    .await?
            }
            None => {
                // Standard request.
                if let Some(ref b) = body {
                    if !headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    {
                        req = req.header("Content-Type", "application/json");
                    }
                    req = req.body(b.clone());
                }
                req.send().await?
            }
        };

        let text = format_response(&url, response, return_body).await?;
        Ok(ToolOutput::from(text))
    }
}

// ---------------------------------------------------------------------------
// Helpers (lift-and-shift from http_tool_executor.rs)
// ---------------------------------------------------------------------------

/// Extract the host component from an already-expanded URL.
fn host_from_url(url: &str) -> Result<String> {
    Ok(Url::parse(url)
        .map_err(|e| anyhow!("http_request: invalid url '{}': {}", url, e))?
        .host_str()
        .unwrap_or("")
        .to_string())
}

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

fn parse_headers(raw: &str, ctx: &ToolCtx<'_>) -> Result<Vec<(String, String)>> {
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
            Ok((key, expand_env_refs(&v, ctx)?))
        })
        .collect()
}

/// Expand `$UPPERCASE_VAR` tokens in a string from the process environment.
///
/// Each env read is gated by `ctx.scope.check_env_read(name)`; scopes whose
/// `env_reads` list is empty will reject the expansion (default-deny).
/// The A1 migration-window `permissive_scope` leaves `env_reads` empty, so
/// tools that expand `$VAR` must opt in explicitly via agent config.
// TODO(Phase B): revisit once per-agent env_reads are wired into permissive_scope.
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

/// Read an env var, gated by scope. Scope-less callers (permissive_scope)
/// bypass the gate only when `env_reads` contains a `"*"` wildcard — mirroring
/// the shell_bins / net_hosts wildcard convention. Default-deny otherwise.
fn read_env(name: &str, ctx: &ToolCtx<'_>) -> Result<String> {
    let allowed_any = ctx.scope.env_reads.iter().any(|v| v == "*");
    if !allowed_any {
        ctx.scope.check_env_read(name)?;
    }
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
                http: reqwest::Client::new(),
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
            msg.contains("missing 'url'"),
            "expected missing url message, got: {}",
            msg
        );
    }
}
