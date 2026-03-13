use crate::application::ports::ToolExecutionPort;
use crate::domain::skill::{ApiAuth, SkillDefinition, SkillExecution};
use anyhow::{bail, Result};
use reqwest::{Client, Method};
use std::collections::HashMap;

pub(crate) struct ApiSkillExecutionAdapter {
    client: Client,
    skills: HashMap<String, SkillDefinition>,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl ApiSkillExecutionAdapter {
    pub(crate) fn new(skill_defs: Vec<SkillDefinition>) -> Result<Self> {
        let skills = skill_defs
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect::<HashMap<_, _>>();
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        Ok(Self {
            client: Client::builder().build()?,
            skills,
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

impl ToolExecutionPort for ApiSkillExecutionAdapter {
    fn execute_tool(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let skill = self
            .skills
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown API skill: {}", call.name))?;
        let api = match &skill.execution {
            SkillExecution::Api(api) => api,
            SkillExecution::Shell { .. } => bail!("Skill '{}' is not an API tool", call.name),
        };

        let method = call
            .arguments
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("{}: missing 'method' argument", call.name))?;
        let path = call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("{}: missing 'path' argument", call.name))?;
        let body = call
            .arguments
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let dynamic_headers = call
            .arguments
            .get("headers")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");

        let method = Method::from_bytes(method.trim().as_bytes())?;
        let url = if path.trim().is_empty() {
            api.base_url.clone()
        } else if path.starts_with('/') {
            format!("{}{}", api.base_url.trim_end_matches('/'), path)
        } else {
            format!("{}/{}", api.base_url.trim_end_matches('/'), path)
        };

        let client = self.client.clone();
        let auth = api.auth.clone();
        let headers = api.headers.clone();
        let body = body.to_string();
        let extra_headers = parse_extra_headers(dynamic_headers)?;

        self.run_async(async move {
            let mut request = client.request(method.clone(), &url);
            request = request.header("Content-Type", "application/json");

            match auth {
                ApiAuth::None => {}
                ApiAuth::BearerEnv { env } => {
                    let token = std::env::var(&env)
                        .map_err(|_| anyhow::anyhow!("Missing environment variable {}", env))?;
                    request = request.bearer_auth(token);
                }
                ApiAuth::BasicEnv {
                    username_env,
                    password_env,
                } => {
                    let username = std::env::var(&username_env).map_err(|_| {
                        anyhow::anyhow!("Missing environment variable {}", username_env)
                    })?;
                    let password = std::env::var(&password_env).map_err(|_| {
                        anyhow::anyhow!("Missing environment variable {}", password_env)
                    })?;
                    request = request.basic_auth(username, Some(password));
                }
            }

            for (key, value) in headers {
                request = request.header(&key, expand_env_refs(&value)?);
            }
            for (key, value) in extra_headers {
                request = request.header(key, value);
            }

            if !matches!(method, Method::GET | Method::DELETE) || body.trim() != "{}" {
                request = request.body(body.clone());
            }

            let response = request.send().await?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("HTTP {} from {}: {}", status, url, text);
            }
            Ok(format!("HTTP {} {}\n{}", status, url, text))
        })
    }
}

fn parse_extra_headers(raw: &str) -> Result<Vec<(String, String)>> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("headers must be a JSON object string: {}", e))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("headers must decode to a JSON object"))?;
    let mut headers = Vec::new();
    for (key, value) in object {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("header '{}' must have a string value", key))?;
        headers.push((key.clone(), value.to_string()));
    }
    Ok(headers)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_refs_substitutes_tokens() {
        std::env::set_var("API_TEST_VALUE", "secret");
        let expanded = expand_env_refs("Bearer $API_TEST_VALUE").unwrap();
        assert_eq!(expanded, "Bearer secret");
    }

    #[test]
    fn parse_extra_headers_accepts_json_object() {
        let headers = parse_extra_headers(r#"{"x-service-token":"abc"}"#).unwrap();
        assert_eq!(
            headers,
            vec![("x-service-token".to_string(), "abc".to_string())]
        );
    }
}
