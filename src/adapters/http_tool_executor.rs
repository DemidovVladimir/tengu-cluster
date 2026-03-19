//! HTTP request tool executor — generic HTTP client for skill-driven API calls.

use crate::adapters::tool_builder::validate_path;
use crate::adapters::ports::ToolExecutionPort;
use crate::adapters::types::{ToolResultEnvelope, ToolResultStatus};
use anyhow::{bail, Context, Result};
use reqwest::Method;
use std::path::PathBuf;
use crate::adapters::types::ToolCall;

const MAX_RESPONSE_BYTES: usize = 200_000;

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
        let url = call
            .arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http_request: missing 'url' argument"))?;
        let method_str = call
            .arguments
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http_request: missing 'method' argument"))?;
        let body = call.arguments.get("body").and_then(|v| v.as_str());
        let headers_json = call.arguments.get("headers").and_then(|v| v.as_str());
        let file_path = call.arguments.get("file_path").and_then(|v| v.as_str());
        let file_field_name = call
            .arguments
            .get("file_field_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let auth_bearer_env = call
            .arguments
            .get("auth_bearer_env")
            .and_then(|v| v.as_str());
        let auth_basic_user_env = call
            .arguments
            .get("auth_basic_user_env")
            .and_then(|v| v.as_str());
        let auth_basic_pass_env = call
            .arguments
            .get("auth_basic_pass_env")
            .and_then(|v| v.as_str());

        // Expand $ENV_VAR in URL so skills can reference configurable endpoints.
        let url = expand_env_refs(url)?;

        if !url.starts_with("https://") && !url.starts_with("http://") {
            bail!("http_request: url must start with http:// or https://");
        }

        let method = Method::from_bytes(method_str.trim().to_uppercase().as_bytes())
            .context("http_request: invalid HTTP method")?;

        let client = self.client.clone();
        let body = body.map(|s| s.to_string());
        let file_field_name = file_field_name.to_string();

        // Read file bytes before entering async context.
        let file_data = match file_path {
            Some(fp) => {
                let validated = validate_path(&self.workspace, fp)?;
                let bytes = std::fs::read(&validated)
                    .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", fp, e))?;
                let filename = validated
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                Some((bytes, filename))
            }
            None => None,
        };

        let headers = parse_headers(headers_json.unwrap_or("{}"))?;
        let auth = resolve_auth(auth_bearer_env, auth_basic_user_env, auth_basic_pass_env)?;

        self.run_async(async move {
            let response = if let Some((bytes, filename)) = file_data {
                if file_field_name.is_empty() {
                    // Raw body upload (e.g. S3 presigned PUT).
                    let mut request = client.request(method, &url);
                    request = apply_auth(request, &auth);
                    for (key, value) in &headers {
                        request = request.header(key, value);
                    }
                    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                        let mime = mime_from_filename(&filename);
                        request = request.header("Content-Type", mime);
                    }
                    request
                        .header("Content-Length", bytes.len().to_string())
                        .body(bytes)
                        .send()
                        .await?
                } else {
                    // Multipart upload.
                    let mime = mime_from_filename(&filename);
                    let part = reqwest::multipart::Part::bytes(bytes)
                        .file_name(filename)
                        .mime_str(&mime)?;
                    let mut form = reqwest::multipart::Form::new().part(file_field_name, part);

                    if let Some(ref body_str) = body {
                        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(body_str) {
                            if let Some(map) = obj.as_object() {
                                for (key, value) in map {
                                    let v = match value.as_str() {
                                        Some(s) => s.to_string(),
                                        None => value.to_string(),
                                    };
                                    form = form.text(key.clone(), v);
                                }
                            }
                        }
                    }

                    let mut request = client.request(method, &url);
                    request = apply_auth(request, &auth);
                    for (key, value) in &headers {
                        if !key.eq_ignore_ascii_case("content-type") {
                            request = request.header(key, value);
                        }
                    }
                    request.multipart(form).send().await?
                }
            } else {
                // Standard request.
                let mut request = client.request(method.clone(), &url);
                request = apply_auth(request, &auth);
                for (key, value) in &headers {
                    request = request.header(key, value);
                }
                if let Some(ref body_str) = body {
                    if !headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    {
                        request = request.header("Content-Type", "application/json");
                    }
                    request = request.body(body_str.clone());
                }
                request.send().await?
            };

            let status = response.status();
            let resp_headers = format_response_headers(&response);
            let text = response.text().await.unwrap_or_default();

            let body_output = if text.len() > MAX_RESPONSE_BYTES {
                let mut end = MAX_RESPONSE_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...(truncated, {} total bytes)", &text[..end], text.len())
            } else {
                text
            };

            let envelope_status = if status.is_success() {
                ToolResultStatus::Ok
            } else {
                ToolResultStatus::Error
            };
            let mut envelope = ToolResultEnvelope {
                tool_name: "http_request".to_string(),
                status: envelope_status,
                summary: format!("HTTP {} {}", status.as_u16(), url),
                ..Default::default()
            };
            if status.is_success() {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&body_output) {
                    envelope.raw_response = Some(json_val);
                } else {
                    envelope.raw_response = Some(serde_json::Value::String(body_output));
                }
            } else {
                let body_json: serde_json::Value =
                    serde_json::from_str(&body_output).unwrap_or_else(|_| {
                        serde_json::Value::String(body_output)
                    });
                envelope.raw_response = Some(serde_json::json!({
                    "status": status.as_u16(),
                    "url": url,
                    "headers": resp_headers,
                    "body": body_json,
                }));
            }
            envelope.to_json_string().map_err(|e| e.into())
        })
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
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("headers must be a JSON object string: {}", e))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("headers must decode to a JSON object"))?;
    let mut headers = Vec::new();
    for (key, value) in object {
        let v = match value.as_str() {
            Some(s) => s.to_string(),
            None => value.to_string(),
        };
        let expanded = expand_env_refs(&v)?;
        headers.push((key.clone(), expanded));
    }
    Ok(headers)
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

/// Guess MIME type from filename extension.
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

fn format_response_headers(response: &reqwest::Response) -> String {
    let mut lines = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            let k = key.as_str().to_lowercase();
            if matches!(
                k.as_str(),
                "content-type" | "location" | "x-request-id" | "retry-after" | "www-authenticate"
            ) {
                lines.push(format!("{}: {}", key, v));
            }
        }
    }
    lines.join("\n")
}
