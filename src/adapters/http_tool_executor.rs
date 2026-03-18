//! HTTP request tool executor — generic HTTP client for skill-driven API calls.
//!
//! **Host allowlist**: When active API skills are present, the executor collects
//! every literal hostname from skill documentation and the `base_url` frontmatter
//! field. Requests whose URL was NOT derived from an `$ENV_VAR` reference must
//! target one of these hosts (or a host previously seen in a successful response).
//! This prevents the LLM from hallucinating URLs.

use crate::adapters::workspace_tools::validate_path;
use crate::application::ports::ToolExecutionPort;
use crate::domain::tool_result::{ToolResultEnvelope, ToolResultStatus};
use anyhow::{bail, Context, Result};
use reqwest::Method;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::RwLock;
use tengu_core::types::ToolCall;

const MAX_RESPONSE_BYTES: usize = 200_000;

pub(crate) struct HttpToolExecutionAdapter {
    client: reqwest::Client,
    workspace: PathBuf,
    fallback_runtime: Option<tokio::runtime::Runtime>,
    /// Allowed URL hosts extracted from skill documentation.
    /// Empty = no restriction. Populated = hard enforcement.
    allowed_hosts: RwLock<HashSet<String>>,
    /// Allowed `$ENV_VAR` names extracted from skill documentation.
    /// Empty = no restriction. Populated = reject unknown var names.
    allowed_env_vars: HashSet<String>,
}

impl HttpToolExecutionAdapter {
    #[allow(dead_code)]
    pub(crate) fn new(
        workspace: PathBuf,
        initial_allowed_hosts: HashSet<String>,
        allowed_env_vars: HashSet<String>,
    ) -> Result<Self> {
        Self::with_client(None, workspace, initial_allowed_hosts, allowed_env_vars)
    }

    /// Create with an optional shared `reqwest::Client`. When `None`, a new
    /// client is built internally. Sharing a client across agents eliminates
    /// redundant connection pools.
    pub(crate) fn with_client(
        shared_client: Option<reqwest::Client>,
        workspace: PathBuf,
        initial_allowed_hosts: HashSet<String>,
        allowed_env_vars: HashSet<String>,
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
        if !initial_allowed_hosts.is_empty() {
            tracing::info!("http_request: allowed hosts = {:?}", initial_allowed_hosts);
        }
        if !allowed_env_vars.is_empty() {
            tracing::info!("http_request: allowed env vars = {:?}", allowed_env_vars);
        }
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
            allowed_hosts: RwLock::new(initial_allowed_hosts),
            allowed_env_vars,
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

        // Validate env var references BEFORE expansion — reject hallucinated names
        // with a helpful error listing the valid ones from skill documentation.
        let url_has_env_ref = url.contains('$');
        if url_has_env_ref && !self.allowed_env_vars.is_empty() {
            validate_env_refs(url, &self.allowed_env_vars)?;
        }

        // Expand $ENV_VAR in URL so skills can reference configurable endpoints
        // (e.g., $MOLECULE_LABS_URL for staging vs production).
        let url = expand_env_refs(url)?;

        if !url.starts_with("https://") && !url.starts_with("http://") {
            bail!("http_request: url must start with http:// or https://");
        }

        // Host allowlist enforcement: if the URL was NOT derived from an env var,
        // verify its host is in the allowed set (from skill documentation or
        // previous successful responses).
        if !url_has_env_ref {
            let allowed = self.allowed_hosts.read().unwrap();
            if !allowed.is_empty() {
                let host = extract_url_host(&url).unwrap_or_default();
                if !host.is_empty() && !allowed.contains(&host) {
                    bail!(
                        "http_request: host '{}' is not in the allowed hosts list. \
                         Only use URLs documented in the skill reference or $ENV_VAR references. \
                         Allowed: {:?}",
                        host,
                        allowed.iter().collect::<Vec<_>>()
                    );
                }
            }
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

        // Validate env var names in headers before expansion.
        if let Some(raw_headers) = headers_json {
            if !self.allowed_env_vars.is_empty() && raw_headers.contains('$') {
                validate_env_refs(raw_headers, &self.allowed_env_vars)?;
            }
        }
        let headers = parse_headers(headers_json.unwrap_or("{}"))?;

        // Validate auth env var names.
        if !self.allowed_env_vars.is_empty() {
            for env_name in [auth_bearer_env, auth_basic_user_env, auth_basic_pass_env]
                .iter()
                .flatten()
            {
                if !self.allowed_env_vars.contains(*env_name) {
                    bail!(
                        "http_request: env var '{}' is not documented in skill reference. \
                         Valid env vars: {:?}",
                        env_name,
                        self.allowed_env_vars.iter().collect::<Vec<_>>()
                    );
                }
            }
        }

        // Resolve auth from env vars (secrets never appear in LLM output).
        let auth = resolve_auth(auth_bearer_env, auth_basic_user_env, auth_basic_pass_env)?;

        let allowed_hosts_ref = &self.allowed_hosts;
        self.run_async(async move {
            let response = if let Some((bytes, filename)) = file_data {
                if file_field_name.is_empty() {
                    // Raw body upload (e.g. S3 presigned PUT) — send file bytes
                    // directly without multipart wrapping.
                    let mut request = client.request(method, &url);
                    request = apply_auth(request, &auth);
                    for (key, value) in &headers {
                        request = request.header(key, value);
                    }
                    // Set Content-Type from filename if not already in headers.
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
                    // Multipart upload (e.g. POI registration) — wrap file in a
                    // form part with the specified field name.
                    let mime = mime_from_filename(&filename);
                    let part = reqwest::multipart::Part::bytes(bytes)
                        .file_name(filename)
                        .mime_str(&mime)?;
                    let mut form = reqwest::multipart::Form::new().part(file_field_name, part);

                    // Add body fields as form text parts if present.
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
                    // Skip Content-Type for multipart — reqwest sets it with the
                    // correct boundary automatically. Manually overriding it breaks
                    // the multipart boundary and causes "No files selected" errors.
                    for (key, value) in &headers {
                        if !key.eq_ignore_ascii_case("content-type") {
                            request = request.header(key, value);
                        }
                    }
                    request.multipart(form).send().await?
                }
            } else {
                // Standard request
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

            // Learn hosts from successful response bodies so that presigned URLs,
            // redirect targets, and other dynamically-returned endpoints are
            // allowed in subsequent requests.
            if status.is_success() {
                let new_hosts = extract_hosts_from_text(&text);
                if !new_hosts.is_empty() {
                    if let Ok(mut hosts) = allowed_hosts_ref.write() {
                        for h in new_hosts {
                            if hosts.insert(h.clone()) {
                                tracing::debug!("http_request: learned host '{}' from response", h);
                            }
                        }
                    }
                }
            }

            let body_output = if text.len() > MAX_RESPONSE_BYTES {
                let mut end = MAX_RESPONSE_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...(truncated, {} total bytes)", &text[..end], text.len())
            } else {
                text
            };

            // Wrap in ToolResultEnvelope so structured data flows through RunState.
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
            // For successful JSON responses, extract key fields into `ids`
            // for RunState, and include the parsed body directly (no raw_response
            // duplication — keeping the envelope compact for the LLM).
            // For errors or non-JSON, include the raw body in raw_response.
            if status.is_success() {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&body_output) {
                    extract_ids_from_json(&json_val, "", &mut envelope);
                    // Include parsed body as raw_response without the wrapper
                    // so the LLM sees clean data without status/url/headers noise.
                    envelope.raw_response = Some(json_val);
                } else {
                    envelope.raw_response = Some(serde_json::Value::String(body_output));
                }
            } else {
                // For errors, include full context for debugging.
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
        // Expand $ENV_VAR references so skills can reference secrets safely.
        let expanded = expand_env_refs(&v)?;
        headers.push((key.clone(), expanded));
    }
    Ok(headers)
}

/// Validate that every `$UPPERCASE_VAR` reference in `input` is in `allowed`.
/// Returns a helpful error listing valid env var names on mismatch.
fn validate_env_refs(input: &str, allowed: &HashSet<String>) -> Result<()> {
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
            if !name.is_empty() && !allowed.contains(name) {
                bail!(
                    "http_request: env var '${}' is not documented in skill reference. \
                     Valid env vars: {:?}",
                    name,
                    allowed.iter().collect::<Vec<_>>()
                );
            }
        } else {
            idx += 1;
        }
    }
    Ok(())
}

/// Extract env var names from text using two patterns:
/// 1. `$UPPERCASE_VAR` — dollar-prefixed references
/// 2. `` `UPPERCASE_VAR` `` — backtick-wrapped names with at least one underscore
///    (catches env var names in markdown tables and yaml-like `auth_bearer_env: VAR`)
pub(crate) fn extract_env_refs_from_text(text: &str) -> HashSet<String> {
    let mut vars = HashSet::new();
    let bytes = text.as_bytes();
    let mut idx = 0;

    // Pattern 1: $UPPERCASE_VAR
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
            let name = &text[start..idx];
            if !name.is_empty() {
                vars.insert(name.to_string());
            }
        } else {
            idx += 1;
        }
    }

    // Pattern 2: `UPPERCASE_VAR` (backtick-wrapped, must contain underscore)
    for (pos, _) in text.match_indices('`') {
        let rest = &text[pos + 1..];
        if let Some(end) = rest.find('`') {
            let candidate = &rest[..end];
            if candidate.len() >= 3
                && candidate.contains('_')
                && candidate
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                vars.insert(candidate.to_string());
            }
        }
    }

    vars
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

/// Extract the hostname from a URL (lowercase, without port).
fn extract_url_host(url: &str) -> Option<String> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = without_scheme.split('/').next()?;
    let host = host.split(':').next()?; // strip port
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// Extract hostnames from all `http(s)://` URLs found in text.
///
/// Skips env-var references (`$VAR`) and template placeholders (`<...>`).
/// Used to learn hosts from skill documentation and from response bodies.
pub(crate) fn extract_hosts_from_text(text: &str) -> HashSet<String> {
    let mut hosts = HashSet::new();
    for scheme in ["https://", "http://"] {
        for (idx, _) in text.match_indices(scheme) {
            let after = &text[idx + scheme.len()..];
            let end = after
                .find(|c: char| {
                    matches!(
                        c,
                        '/' | ' ' | '"' | '\'' | '\n' | '\r' | '>' | ')' | ']' | ',' | '`'
                    )
                })
                .unwrap_or(after.len());
            let host_port = &after[..end];
            let host = host_port.split(':').next().unwrap_or("");
            if !host.is_empty()
                && !host.starts_with('$')
                && !host.starts_with('<')
                && host.contains('.')
            {
                hosts.insert(host.to_lowercase());
            }
        }
    }
    hosts
}

/// Extract interesting scalar values from a JSON response into the envelope's `ids` map.
///
/// Looks for keys that typically carry identifiers, hashes, or URLs:
/// `*_id`, `*_hash`, `*_root`, `*_url`, `*_address`, `token_id`, `tx_hash`, etc.
/// Nested objects are flattened with dot-separated keys (max depth 3).
fn extract_ids_from_json(
    value: &serde_json::Value,
    prefix: &str,
    envelope: &mut ToolResultEnvelope,
) {
    const MAX_DEPTH: usize = 3;
    let depth = prefix.matches('.').count();

    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let full_key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", prefix, key)
                };
                match val {
                    serde_json::Value::String(s) => {
                        // Extract identifiers and short URLs into ids.
                        // Skip very long values (presigned S3 URLs, tokens >500 chars)
                        // — the LLM reads those from raw_response instead.
                        if is_interesting_key(key) && !s.is_empty() && s.len() < 500 {
                            envelope.ids.insert(full_key, s.clone());
                        }
                    }
                    serde_json::Value::Number(n) => {
                        if is_interesting_key(key) {
                            envelope.ids.insert(full_key, n.to_string());
                        }
                    }
                    serde_json::Value::Object(_) if depth < MAX_DEPTH => {
                        extract_ids_from_json(val, &full_key, envelope);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn is_interesting_key(key: &str) -> bool {
    let k = key.to_lowercase();
    k.ends_with("_id")
        || k.ends_with("id")
        || k.ends_with("_hash")
        || k.ends_with("hash")
        || k.ends_with("_root")
        || k.ends_with("root")
        || k.ends_with("_url")
        || k.ends_with("url")
        || k.ends_with("_address")
        || k.ends_with("address")
        || k.ends_with("_token")
        || k.ends_with("token")
        || k.ends_with("_symbol")
        || k.ends_with("symbol")
        || k == "merkle_root"
        || k == "merkleroot"
        || k == "transaction_to"
        || k == "transaction_data"
        || k == "tx_hash"
        || k == "signature"
        || k == "signer"
        || k == "reservation_id"
        || k == "reservationid"
        || k == "ipnft_id"
        || k == "ipnftid"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_headers_empty() {
        assert!(parse_headers("{}").unwrap().is_empty());
        assert!(parse_headers("").unwrap().is_empty());
    }

    #[test]
    fn parse_headers_valid() {
        let headers =
            parse_headers(r#"{"Authorization": "Bearer tok", "X-Custom": "val"}"#).unwrap();
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn parse_headers_invalid_json() {
        assert!(parse_headers("not json").is_err());
    }

    #[test]
    fn expand_env_refs_substitutes_token() {
        std::env::set_var("HTTP_TEST_TOKEN", "secret123");
        let result = expand_env_refs("Bearer $HTTP_TEST_TOKEN").unwrap();
        assert_eq!(result, "Bearer secret123");
    }

    #[test]
    fn expand_env_refs_passthrough_no_dollar() {
        let result = expand_env_refs("plain text").unwrap();
        assert_eq!(result, "plain text");
    }

    #[test]
    fn expand_env_refs_bare_dollar_stays() {
        let result = expand_env_refs("just a $ sign").unwrap();
        assert_eq!(result, "just a $ sign");
    }

    #[test]
    fn expand_env_refs_in_url() {
        std::env::set_var("HTTP_TEST_BASE_URL", "https://api.example.com/graphql");
        let result = expand_env_refs("$HTTP_TEST_BASE_URL").unwrap();
        assert_eq!(result, "https://api.example.com/graphql");
    }

    #[test]
    fn expand_env_refs_mixed_literal_and_var() {
        std::env::set_var("HTTP_TEST_HOST", "staging.example.com");
        let result = expand_env_refs("https://$HTTP_TEST_HOST/api/v1").unwrap();
        assert_eq!(result, "https://staging.example.com/api/v1");
    }

    #[test]
    fn extract_url_host_basic() {
        assert_eq!(
            extract_url_host("https://api.example.com/v1/foo"),
            Some("api.example.com".into())
        );
        assert_eq!(
            extract_url_host("http://localhost:8080/test"),
            Some("localhost".into())
        );
        assert_eq!(extract_url_host("not a url"), None);
    }

    #[test]
    fn extract_hosts_from_text_finds_urls() {
        let text = r#"
            Use https://testnet.molecule.xyz/api/v1/inventions for POI.
            GraphQL at https://staging.graphql.api.molecule.xyz/graphql
            Env var: $MOLECULE_LABS_URL (skipped)
            Placeholder: https://<host>/path (skipped)
        "#;
        let hosts = extract_hosts_from_text(text);
        assert!(hosts.contains("testnet.molecule.xyz"));
        assert!(hosts.contains("staging.graphql.api.molecule.xyz"));
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn extract_hosts_skips_env_and_placeholders() {
        let text = "url: $MY_URL and https://<dynamic>/path";
        let hosts = extract_hosts_from_text(text);
        assert!(hosts.is_empty());
    }

    #[test]
    fn extract_hosts_from_response_json() {
        let text = r#"{"presignedUrl": "https://bucket.s3.amazonaws.com/upload?sig=abc"}"#;
        let hosts = extract_hosts_from_text(text);
        assert!(hosts.contains("bucket.s3.amazonaws.com"));
    }

    #[test]
    fn extract_env_refs_from_skill_body() {
        let text = r#"
            | `MOLECULE_API_KEY` | All GraphQL calls | Sent as x-api-key header |
            | `POI_API_KEY` | POI registration | Bearer token |
            headers: {"x-api-key": "$MOLECULE_API_KEY"}
            url: $MOLECULE_LABS_URL
            auth_bearer_env: POI_API_KEY
        "#;
        let vars = extract_env_refs_from_text(text);
        assert!(vars.contains("MOLECULE_API_KEY")); // from both $ref and `backtick`
        assert!(vars.contains("MOLECULE_LABS_URL")); // from $ref
        assert!(vars.contains("POI_API_KEY")); // from `backtick` in table
    }

    #[test]
    fn validate_env_refs_rejects_hallucinated() {
        let allowed: HashSet<String> = ["MOLECULE_LABS_URL", "MOLECULE_API_KEY"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Valid ref passes
        assert!(validate_env_refs("$MOLECULE_LABS_URL", &allowed).is_ok());
        // Hallucinated ref fails
        let err = validate_env_refs("$AURA_API_URL", &allowed).unwrap_err();
        assert!(err.to_string().contains("AURA_API_URL"));
        assert!(err.to_string().contains("not documented"));
    }

    #[test]
    fn validate_env_refs_allows_bare_dollar() {
        let allowed: HashSet<String> = ["FOO"].iter().map(|s| s.to_string()).collect();
        assert!(validate_env_refs("just a $ sign", &allowed).is_ok());
    }
}
