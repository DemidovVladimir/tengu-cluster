//! HTTP tool executor — legacy migration-window code.
//!
//! TODO(A9): delete once `mcp_bridge` is rewritten to dispatch through `ToolRegistry`.
//! The new `plugins::http::HttpRequestTool` is used by `channel_runtime::build_tool_executor`;
//! this file exists only to back the MCP bridge path until A9.
//!
//! HTTP request tool executor — generic HTTP client for skill-driven API calls.

use crate::adapters::tool_builder::validate_path;
use crate::adapters::ports::ToolExecutionPort;
use anyhow::{bail, Context, Result};
use reqwest::Method;
use std::path::PathBuf;
use crate::adapters::types::ToolCall;

pub(crate) struct HttpToolExecutionAdapter {
    client: reqwest::Client,
    workspace: PathBuf,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl HttpToolExecutionAdapter {
    pub(crate) fn with_client(
        shared_client: Option<reqwest::Client>,
        workspace: PathBuf,
    ) -> Result<Self> {
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        let client = match shared_client {
            Some(c) => c,
            None => reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()?,
        };
        Ok(Self {
            client,
            workspace,
            fallback_runtime,
        })
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            self.fallback_runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(future)
        }
    }
}

impl ToolExecutionPort for HttpToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        let url = call.arguments.get("url").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http_request: missing 'url'"))?;
        let method_str = call.arguments.get("method").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http_request: missing 'method'"))?;
        let body = arg_to_string(call, "body");
        let headers_json = arg_to_string(call, "headers");
        let file_path = call.arguments.get("file_path").and_then(|v| v.as_str());
        let file_field_name = call.arguments.get("file_field_name")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let auth_bearer_env = call.arguments.get("auth_bearer_env").and_then(|v| v.as_str());
        let auth_basic_user_env = call.arguments.get("auth_basic_user_env").and_then(|v| v.as_str());
        let auth_basic_pass_env = call.arguments.get("auth_basic_pass_env").and_then(|v| v.as_str());
        let return_body = call.arguments.get("return_body").and_then(|v| v.as_bool()).unwrap_or(false);

        let url = expand_env_refs(url)?;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            bail!("http_request: url must start with http:// or https://");
        }
        let method = Method::from_bytes(method_str.trim().to_uppercase().as_bytes())
            .context("http_request: invalid HTTP method")?;

        let file_data = match file_path {
            Some(fp) => {
                let validated = validate_path(&self.workspace, fp)?;
                let bytes = std::fs::read(&validated)
                    .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", fp, e))?;
                let filename = validated.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                Some((bytes, filename))
            }
            None => None,
        };

        let headers = parse_headers(headers_json.as_deref().unwrap_or("{}"))?;
        let auth = resolve_auth(auth_bearer_env, auth_basic_user_env, auth_basic_pass_env)?;
        let client = self.client.clone();

        self.run_async(async move {
            let is_multipart = file_data.is_some() && !file_field_name.is_empty();

            // Common setup: method, url, auth, headers (once instead of per-branch).
            let mut req = client.request(method, &url);
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
                        .file_name(filename).mime_str(&mime)?;
                    let mut form = reqwest::multipart::Form::new().part(file_field_name, part);
                    if let Some(ref b) = body {
                        if let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(b) {
                            for (k, v) in map {
                                form = form.text(k, v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
                            }
                        }
                    }
                    req.multipart(form).send().await?
                }
                Some((bytes, filename)) => {
                    // Raw body upload (e.g. S3 presigned PUT).
                    let content_type = if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                        let mime = mime_from_filename(&filename);
                        req = req.header("Content-Type", &mime);
                        mime
                    } else {
                        headers.iter()
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
                        .body(bytes).send().await?
                }
                None => {
                    // Standard request.
                    if let Some(ref b) = body {
                        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                            req = req.header("Content-Type", "application/json");
                        }
                        req = req.body(b.clone());
                    }
                    req.send().await?
                }
            };

            format_response(&url, response, return_body).await
        })
    }
}

fn arg_to_string(call: &ToolCall, key: &str) -> Option<String> {
    call.arguments.get(key).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    })
}

async fn format_response(url: &str, response: reqwest::Response, return_body: bool) -> Result<String> {
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
) -> Result<ResolvedAuth> {
    if let Some(env_name) = bearer_env {
        let token = std::env::var(env_name)
            .map_err(|_| anyhow::anyhow!("Missing environment variable {}", env_name))?;
        return Ok(ResolvedAuth::Bearer(token));
    }
    if let (Some(user_env), Some(pass_env)) = (basic_user_env, basic_pass_env) {
        let user = std::env::var(user_env)
            .map_err(|_| anyhow::anyhow!("Missing environment variable {}", user_env))?;
        let pass = std::env::var(pass_env)
            .map_err(|_| anyhow::anyhow!("Missing environment variable {}", pass_env))?;
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

fn parse_headers(raw: &str) -> Result<Vec<(String, String)>> {
    if raw.trim().is_empty() || raw.trim() == "{}" {
        return Ok(Vec::new());
    }
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("headers must be a JSON object: {}", e))?;
    object.into_iter().map(|(key, value)| {
        let v = value.as_str().map(String::from).unwrap_or_else(|| value.to_string());
        Ok((key, expand_env_refs(&v)?))
    }).collect()
}

/// Expand `$UPPERCASE_VAR` tokens in a string from the process environment.
fn expand_env_refs(input: &str) -> Result<String> {
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
            let value = std::env::var(name)
                .map_err(|_| anyhow::anyhow!("Missing environment variable {}", name))?;
            out.push_str(&value);
        } else {
            out.push(bytes[idx] as char);
            idx += 1;
        }
    }
    Ok(out)
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

