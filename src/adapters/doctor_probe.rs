use anyhow::Result;

pub(crate) fn format_engine_diagnostics_compact(
    diagnostics: &tengu_core::EngineDiagnostics,
) -> String {
    let caps = &diagnostics.capabilities;
    format!(
        "model={} endpoint={} transport={} context={} output_cap={} streaming={}",
        diagnostics.configured_model.as_deref().unwrap_or("n/a"),
        diagnostics.endpoint.as_deref().unwrap_or("n/a"),
        diagnostics.transport.as_deref().unwrap_or("n/a"),
        caps.context_window,
        caps.max_output_tokens_per_turn,
        caps.supports_streaming,
    )
}

pub(crate) fn resolve_models_probe_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/models")
    } else {
        format!("{trimmed}/v1/models")
    }
}

pub(crate) fn first_output_line(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let stderr_line = String::from_utf8_lossy(stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string);
    if stderr_line.is_some() {
        return stderr_line;
    }
    String::from_utf8_lossy(stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
}

fn format_http_probe_result(
    label: &str,
    result: Result<reqwest::Response, reqwest::Error>,
) -> String {
    let status = match result {
        Ok(resp) if resp.status().is_success() => "OK".to_string(),
        Ok(resp) => format!("Error: HTTP {}", resp.status()),
        Err(err) => format!("Unreachable: {}", err),
    };
    format!("{label}... {status}")
}

pub(crate) async fn run_engine_probe(
    diagnostics: &tengu_core::EngineDiagnostics,
) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    match diagnostics.engine_id.as_str() {
        "ollama" => {
            let base_url = diagnostics
                .endpoint
                .as_deref()
                .unwrap_or("http://localhost:11434")
                .trim_end_matches('/')
                .to_string();
            let probe = client.get(format!("{base_url}/api/tags")).send().await;
            Some(format_http_probe_result("probe /api/tags", probe))
        }
        "openai" => {
            let api_key = match std::env::var("OPENAI_API_KEY") {
                Ok(value) => value,
                Err(_) => {
                    return Some("probe /v1/models... skipped (OPENAI_API_KEY missing)".to_string())
                }
            };
            let base_url = diagnostics
                .endpoint
                .as_deref()
                .unwrap_or("https://api.openai.com");
            let probe_url = resolve_models_probe_url(base_url);
            let probe = client.get(probe_url).bearer_auth(api_key).send().await;
            Some(format_http_probe_result("probe /v1/models", probe))
        }
        "anthropic" => {
            let api_key = match std::env::var("ANTHROPIC_API_KEY") {
                Ok(value) => value,
                Err(_) => {
                    return Some(
                        "probe /v1/models... skipped (ANTHROPIC_API_KEY missing)".to_string(),
                    )
                }
            };
            let base_url = diagnostics
                .endpoint
                .as_deref()
                .unwrap_or("https://api.anthropic.com");
            let probe_url = resolve_models_probe_url(base_url);
            let probe = client
                .get(probe_url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await;
            Some(format_http_probe_result("probe /v1/models", probe))
        }
        "huggingface" => {
            let api_token = match std::env::var("HF_TOKEN") {
                Ok(value) => value,
                Err(_) => {
                    return Some("probe /v1/models... skipped (HF_TOKEN missing)".to_string())
                }
            };
            let base_url = diagnostics
                .endpoint
                .as_deref()
                .unwrap_or("https://router.huggingface.co/v1");
            let probe_url = resolve_models_probe_url(base_url);
            let probe = client.get(probe_url).bearer_auth(api_token).send().await;
            Some(format_http_probe_result("probe /v1/models", probe))
        }
        "claude-code" => {
            let binary = std::env::var("CLAUDE_CODE_BIN").unwrap_or_else(|_| "claude".to_string());
            let output = tokio::process::Command::new(&binary)
                .arg("--version")
                .output()
                .await;
            let status = match output {
                Ok(output) if output.status.success() => {
                    let version = first_output_line(&output.stdout, &output.stderr)
                        .unwrap_or_else(|| "version unavailable".to_string());
                    format!("OK ({version})")
                }
                Ok(output) => {
                    let details = first_output_line(&output.stdout, &output.stderr)
                        .unwrap_or_else(|| "no output".to_string());
                    format!("Error: exit status {:?} ({details})", output.status.code())
                }
                Err(err) => format!("Unavailable: {}", err),
            };
            Some(format!("probe claude --version... {status}"))
        }
        _ => None,
    }
}
