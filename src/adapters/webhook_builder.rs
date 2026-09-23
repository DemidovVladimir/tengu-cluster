//! Inbound webhook listener — `tengu webhooks --sandbox <name>`.
//!
//! Each `[webhooks.endpoints.<name>]` block in the sandbox config binds
//! one URL path to one agent + one HMAC shared secret. Incoming POSTs
//! are verified with HMAC-SHA256 against `secret_env`/`secret`, the body
//! is wrapped into a synthesized user message, and the orchestrator runs
//! a one-shot turn with a per-request `session_id` of the form
//! `webhook-<endpoint>-<uuid>`.
//!
//! Response: `202 Accepted` with `{"session_id": "..."}` JSON body.
//! Agents can take minutes — webhook senders typically time out at 10s,
//! so we never block waiting for the agent. Final output goes to Open
//! Brain (Postgres `agentic_memory`, with `postgres_memory`) and the
//! tracing log. Recall later by querying Postgres for the `session_id`.
//!
//! ## Files / responsibilities
//!
//! - `run_webhooks(...)` — entry point called by `Commands::Webhooks` in
//!   `main.rs`. Owns the long-running tokio task that hosts the axum
//!   server.
//! - `WebhookAppState` — per-process shared state (config, memory
//!   manager). Cloned into each request handler.
//! - `dispatch_webhook(...)` — per-request handler. HMAC verify → mint
//!   session_id → spawn orchestrator turn → 202 reply.
//! - `verify_hmac(...)` — constant-time HMAC-SHA256 verify via the
//!   `hmac` crate's `Mac::verify_slice`.
//!
//! ## Doctrine
//!
//! - Behaviour changes via TOML, not code: every endpoint binding is in
//!   `sandboxes/<name>/config.toml`. Adding a new webhook is one TOML
//!   block, no recompile.
//! - Session_id is the recall key: per-request, namespaced as
//!   `webhook-<endpoint>-<uuid>`. Pairs with Fix B (parent / child
//!   share one id) and Fix A (within-session output recall).

#![cfg(feature = "webhooks")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::post,
    Router,
};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use tracing::{error, info, warn};

use crate::adapters::channel_runtime;
use crate::adapters::outbound::engines::build_engine;
use crate::adapters::outbound::noop::{NoopActivity, NoopRuntimeToolExecutor};
use crate::application::chat::tool_loop::collect_engine_response;
use crate::application::memory::manager::MemoryManager;
use crate::application::skills::registry::{FileSystemSkillSource, SkillRegistry};
use crate::config::{Config, WebhookEndpointConfig};
use crate::domain::message::{Message, Role};
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::ToolExecutor;
use crate::ports::engine::{Engine, EngineContext};
use crate::ports::orchestration::ChatServiceFactory;
use crate::ports::tool_activity::ToolActivityPort;

type HmacSha256 = Hmac<Sha256>;

/// Header carrying the HMAC-SHA256 signature of the request body.
/// Format: `sha256=<lowercase-hex>`. Mirrors GitHub / Stripe convention
/// (different prefix, identical shape).
const SIG_HEADER: &str = "x-tengu-signature";

/// Long-running entry point. Boots the axum server and never returns
/// (until ctrl-c).
pub async fn run_webhooks(config: Config, secret_registry: Arc<SecretRegistry>) -> Result<()> {
    if !config.webhooks.enabled {
        return Err(anyhow!(
            "[webhooks] not enabled in this sandbox config — set `[webhooks] enabled = true` to start the listener"
        ));
    }
    if config.webhooks.endpoints.is_empty() {
        return Err(anyhow!(
            "[webhooks] no endpoints defined — add at least one `[webhooks.endpoints.<name>]` block"
        ));
    }
    if config.orchestrator.is_none() {
        return Err(anyhow!(
            "[webhooks] requires `[orchestrator]` to be configured — webhooks dispatch through the orchestrator"
        ));
    }
    validate_endpoints(&config.webhooks.endpoints)?;

    let bind = config.webhooks.bind.clone();
    let port = config.webhooks.port;
    let addr: SocketAddr = format!("{}:{}", bind, port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", bind, port))?;

    // Memory manager is shared across all requests — opening it per
    // request would be wasteful (store open + embedder per webhook).
    let memory_manager = Arc::new(MemoryManager::new());
    let state = Arc::new(WebhookAppState {
        config,
        memory_manager,
        _secret_registry: secret_registry,
    });

    let endpoints_summary: Vec<&str> = state
        .config
        .webhooks
        .endpoints
        .keys()
        .map(|s| s.as_str())
        .collect();
    info!(
        bind = %bind,
        port = port,
        endpoints = ?endpoints_summary,
        sandbox = ?state.config.sandbox_name,
        "tengu webhooks listening"
    );

    // axum router: one route handles all endpoints via the {name} path
    // capture. Per-request lookup against config keeps the route shape
    // declarative — adding a new endpoint is a TOML edit, no router
    // rebuild.
    let app = Router::new()
        .route("/webhooks/:name", post(dispatch_webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind webhook listener to {}", addr))?;

    axum::serve(listener, app)
        .await
        .context("webhook server exited unexpectedly")?;
    Ok(())
}

/// Per-process shared state. Cloned cheaply into each request handler
/// via the axum `State` extractor wrapping an `Arc`.
struct WebhookAppState {
    config: Config,
    memory_manager: Arc<MemoryManager>,
    /// Held for redaction parity with telegram (unused in v1 — webhook
    /// responses are 202s with no agent text). Kept for the inevitable
    /// future "sync mode" that mirrors telegram's `secret_registry.redact`.
    _secret_registry: Arc<SecretRegistry>,
}

/// `POST /webhooks/{name}`. Validates HMAC, mints per-request session_id,
/// fires-and-forgets the orchestrator turn, returns 202 immediately.
async fn dispatch_webhook(
    Path(name): Path<String>,
    State(state): State<Arc<WebhookAppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Look up endpoint binding. 404 if unknown — surface clearly so
    //    senders see a typo'd URL rather than a vague auth failure.
    let Some(endpoint) = state.config.webhooks.endpoints.get(&name) else {
        warn!(name = %name, "unknown webhook endpoint");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown webhook endpoint '{}'", name)})),
        )
            .into_response();
    };

    // 2. Resolve the shared secret. `secret_env` (preferred) reads at
    //    request time so a key rotation doesn't require restart.
    let secret = match resolve_endpoint_secret(endpoint) {
        Ok(s) => s,
        Err(e) => {
            error!(name = %name, error = %e, "secret resolve failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("server misconfiguration: {}", e)})),
            )
                .into_response();
        }
    };

    // 3. Verify HMAC. 401 on missing-header / bad-format / mismatch.
    if let Err(e) = verify_signature(&headers, &body, secret.as_bytes()) {
        warn!(name = %name, error = %e, "HMAC verify failed");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // 4. Mint per-request session_id — uuid keeps it unique even when
    //    the same endpoint fires twice in the same second.
    let session_id = format!("webhook-{}-{}", name, uuid::Uuid::new_v4());

    // 5. Synthesize the user message: goal_template + body. Body bytes
    //    aren't required to be UTF-8 (binary webhooks exist) — fall
    //    through to lossy stringification rather than rejecting.
    let body_str = String::from_utf8_lossy(&body);
    let user_message = format!(
        "{}\n\nPayload (raw body, may be JSON):\n{}",
        endpoint.goal_template, body_str
    );

    // 6. Spawn the orchestrator turn. Fire-and-forget — caller gets
    //    202 immediately; agent runs in the background.
    let agent_name = endpoint.agent.clone();
    let session_id_for_handle = session_id.clone();
    let state_for_handle = Arc::clone(&state);
    tokio::spawn(async move {
        run_one_shot(state_for_handle, &session_id_for_handle, user_message).await;
    });

    info!(
        endpoint = %name,
        agent = %agent_name,
        session_id = %session_id,
        body_bytes = body.len(),
        "webhook accepted; dispatching one-shot orchestrator turn"
    );

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "session_id": session_id,
            "endpoint": name,
            "agent": agent_name,
        })),
    )
        .into_response()
}

/// Run one orchestrator turn for the synthesized webhook user message.
/// Final agent text is written to the tracing log; durable side-effects
/// (`agentic_memory` step summaries, metrics records) flow through the standard
/// pipeline. Errors are logged and discarded — the listener stays up.
async fn run_one_shot(state: Arc<WebhookAppState>, session_id: &str, user_message: String) {
    // Per-request orchestrator construction. Uses `WebhookChatServiceFactory`
    // — a per-turn rebuild-from-config pattern modelled on `EvalChatServiceFactory`
    // in `adapters/inbound/eval.rs`. Avoids the `OrchestratorSnapshots` map that
    // chat / telegram use (those surfaces pre-build per-agent state at
    // startup and refresh it before each turn; webhooks are stateless
    // one-shots, so each call just looks up the agent in `Config.agents`
    // and constructs the engine + tools fresh).
    let factory: Arc<dyn ChatServiceFactory> = Arc::new(WebhookChatServiceFactory {
        cfg: Arc::new(state.config.clone()),
    });

    let Some(orchestrator) = channel_runtime::build_orchestrator(
        &state.config,
        factory,
        Arc::clone(&state.memory_manager),
        session_id.to_string(),
    ) else {
        warn!(
            session_id = %session_id,
            "build_orchestrator returned None — webhook turn aborted (engine != \"rag\"). This is a sandbox-config issue, not a runtime crisis."
        );
        return;
    };

    let final_text = orchestrator.handle(user_message).await;
    if final_text.trim().is_empty() {
        warn!(
            session_id = %session_id,
            "webhook orchestrator turn returned empty output"
        );
        return;
    }

    let preview: String = final_text.chars().take(400).collect();
    let truncated = final_text.chars().count() > 400;
    info!(
        session_id = %session_id,
        output_chars = final_text.chars().count(),
        preview = %preview,
        truncated = truncated,
        "webhook orchestrator turn completed"
    );

    // Persist the orchestrator's final output to Open Brain (`agentic_memory`,
    // Postgres) with a synthetic step_id `"webhook-handler"`. Without this,
    // webhook turns where the planner emitted `Direct { response }` (no
    // subagent dispatched, so no `compress_and_store` and no backstop) leave
    // nothing recallable. Coexists with subagent step summaries on the same
    // session_id; different step_ids keep them distinct.
    persist_webhook_output(&state, session_id, &final_text).await;
}

/// Fire-and-forget persist of the webhook turn's final text into Open Brain
/// (`agentic_memory`, Postgres). Logs success at info, errors at warn — never
/// propagates. Embedding is best-effort: no `OPENROUTER_API_KEY` (or an embed
/// error) stores a text-only memory.
#[cfg(feature = "postgres_memory")]
async fn persist_webhook_output(state: &Arc<WebhookAppState>, session_id: &str, final_text: &str) {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                state.config.memory.embedding_model.clone(),
            );
            match embedder.embed(final_text).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        error = %e,
                        "webhook persist: embedding failed; writing text-only memory"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };
    match crate::adapters::outbound::tools::agentic_memory::write_step_summary_with_embedding(
        session_id,
        "webhook-handler",
        final_text,
        embedding.as_deref(),
    )
    .await
    {
        Ok(id) => info!(
            entry_id = %id,
            session_id = %session_id,
            step_id = "webhook-handler",
            "webhook output persisted to agentic_memory"
        ),
        Err(e) => warn!(
            session_id = %session_id,
            error = %e,
            "webhook output persist FAILED — agentic_memory unavailable (is TENGU_MEMORY_DATABASE_URL set?)"
        ),
    }
}

/// Without `postgres_memory` there is no durable memory backend — the webhook
/// turn still completes and its output is in the tracing log, just not
/// recallable.
#[cfg(not(feature = "postgres_memory"))]
async fn persist_webhook_output(
    _state: &Arc<WebhookAppState>,
    session_id: &str,
    _final_text: &str,
) {
    warn!(
        session_id = %session_id,
        "webhook output not persisted — built without the `postgres_memory` feature"
    );
}

/// Resolve the shared secret from `secret_env` (preferred) or `secret`
/// (literal in TOML). Returns the secret as a `String`. Fails when
/// neither is set, both are set, or `secret_env` points at an unset var.
fn resolve_endpoint_secret(ep: &WebhookEndpointConfig) -> Result<String> {
    match (&ep.secret_env, &ep.secret) {
        (Some(_), Some(_)) => Err(anyhow!(
            "endpoint config has both `secret_env` and `secret` — use exactly one"
        )),
        (None, None) => Err(anyhow!(
            "endpoint config has neither `secret_env` nor `secret` — one is required for HMAC verification"
        )),
        (Some(env_name), None) => std::env::var(env_name).map_err(|_| {
            anyhow!(
                "secret_env points at `{}` but that env var is not set in the listener's process",
                env_name
            )
        }),
        (None, Some(s)) if s.is_empty() => {
            Err(anyhow!("inline `secret` is empty — refuse to verify against empty key"))
        }
        (None, Some(s)) => Ok(s.clone()),
    }
}

/// Validate every endpoint's secret config at startup so misconfiguration
/// fails loudly *before* the listener accepts traffic. Catches the
/// "neither set / both set / empty inline" cases that
/// `resolve_endpoint_secret` would catch per-request.
fn validate_endpoints(endpoints: &HashMap<String, WebhookEndpointConfig>) -> Result<()> {
    for (name, ep) in endpoints {
        match (&ep.secret_env, &ep.secret) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] has both `secret_env` and `secret` — use exactly one",
                    name
                ));
            }
            (None, None) => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] has neither `secret_env` nor `secret` — one is required",
                    name
                ));
            }
            (None, Some(s)) if s.is_empty() => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] inline `secret` is empty — refuse to verify against empty key",
                    name
                ));
            }
            _ => {}
        }
        if ep.agent.is_empty() {
            return Err(anyhow!("[webhooks.endpoints.{}] `agent` is required", name));
        }
    }
    Ok(())
}

/// Verify the `X-Tengu-Signature` header against `body` using
/// HMAC-SHA256 with the shared secret. Returns `Ok` on match,
/// `Err(reason)` on missing/malformed/mismatched.
///
/// Constant-time compare via `Mac::verify_slice`.
fn verify_signature(headers: &HeaderMap, body: &[u8], secret: &[u8]) -> Result<()> {
    let header_value = headers
        .get(SIG_HEADER)
        .ok_or_else(|| anyhow!("missing `X-Tengu-Signature` header"))?
        .to_str()
        .map_err(|_| anyhow!("`X-Tengu-Signature` header is not valid UTF-8"))?;
    verify_hmac(header_value, body, secret)
}

/// Pure HMAC verify — extracted so unit tests don't need an `HeaderMap`.
fn verify_hmac(header_value: &str, body: &[u8], secret: &[u8]) -> Result<()> {
    let hex_sig = header_value
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow!("`X-Tengu-Signature` must start with `sha256=`"))?
        .trim();
    let expected_bytes =
        hex_decode(hex_sig).map_err(|e| anyhow!("`X-Tengu-Signature` hex decode failed: {}", e))?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| {
        anyhow!("HMAC key length invalid (this is a bug — Hmac<Sha256> accepts any length)")
    })?;
    mac.update(body);
    mac.verify_slice(&expected_bytes)
        .map_err(|_| anyhow!("HMAC mismatch"))?;
    Ok(())
}

/// Tiny hex decoder — avoids pulling `hex` as a dep just for this. Lower-
/// case and upper-case both accepted; whitespace is rejected.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd hex length: {}", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = decode_nibble(chunk[0])?;
        let lo = decode_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn decode_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(format!("non-hex byte 0x{:02x}", other)),
    }
}

/// Per-turn `ChatServiceFactory` impl that rebuilds the engine + tools +
/// system prompt from `Config.agents` for each `agent_name` it's asked to
/// run. Mirrors `EvalChatServiceFactory` in `adapters/inbound/eval.rs`. Used by the
/// webhook listener because webhook turns are stateless one-shots — there's
/// no pre-built per-agent state to read from (unlike chat/telegram, which
/// keep `OrchestratorSnapshots`).
///
/// The orchestrator agent (`Config.orchestrator.agent`, typically `aura`)
/// gets NO tools so the planner LLM emits plan JSON, not direct tool calls.
/// Every other dispatched agent gets its full tool surface — the same one
/// it has in chat/telegram.
struct WebhookChatServiceFactory {
    cfg: Arc<Config>,
}

#[async_trait]
impl ChatServiceFactory for WebhookChatServiceFactory {
    async fn run_turn(&self, agent_name: &str, text: &str) -> Result<String> {
        let agent = self
            .cfg
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow!("unknown agent in webhook turn: {}", agent_name))?;

        let engine_box = build_engine(agent_name, agent, self.cfg.claude_code.as_ref())?;
        let engine: Arc<dyn Engine> = Arc::from(engine_box);

        // Workspace fallback: when the agent has none of its own (rare in
        // production sandboxes), use cwd so file tools have a root.
        //
        // Tilde expansion is essential — sandbox configs ship with paths
        // like `workspace = "~/aura-workspace"`. Without `expand_tilde`
        // the literal `~` is joined into runtime paths (e.g. by the cache
        // plugin's `<workspace>/.tengu/cache.db`), creating a `~` directory
        // at CWD that pollutes the repo. Mirrors `telegram_builder`'s
        // pattern at the `let workspace: Option<PathBuf>` site.
        let workspace_path: PathBuf = agent
            .workspace
            .as_ref()
            .map(|p| crate::config::paths::expand_tilde(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let secret_registry = Arc::new(SecretRegistry::new());
        let log_activity: Arc<dyn ToolActivityPort> = Arc::new(NoopActivity);

        // Orchestrator agent gets NO tools — its job is to emit JSON only.
        // Doctrinal: LLM = heart, Open Brain + LLM Wiki = brain, tools =
        // hands. The planner call must not have hands. Mirrors
        // `EvalChatServiceFactory`.
        let is_orchestrator_agent = self
            .cfg
            .orchestrator
            .as_ref()
            .map(|o| o.agent == agent_name)
            .unwrap_or(false);

        let base_tools = if is_orchestrator_agent {
            Vec::new()
        } else {
            channel_runtime::compute_base_tools(
                true,
                false, // memory tools off for webhook one-shots
                &agent.workspace_tools,
            )
        };

        let skill_source = FileSystemSkillSource::new(workspace_path.clone());
        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
        let mut skill_registry =
            SkillRegistry::new(base_reserved).with_allowlist(Some(agent.skill_packages.clone()));
        skill_registry.reload(&skill_source);

        let current_tools = if is_orchestrator_agent {
            Vec::new()
        } else {
            channel_runtime::rebuild_tools(&base_tools, &skill_registry)
        };
        let system_prompt =
            channel_runtime::rebuild_system_prompt(agent, true, &skill_registry, &current_tools);

        let mut tool_defs = current_tools.clone();
        let inner_executor: Arc<dyn ToolExecutor> = match channel_runtime::build_tool_executor(
            &workspace_path,
            &current_tools,
            &skill_registry,
            &None,
            &secret_registry,
            log_activity,
            None,
            Some(&self.cfg.memory),
            agent,
            &self.cfg.mcp_servers,
        ) {
            Some(executor) => {
                let extra = executor.additional_tool_defs(&tool_defs);
                if !extra.is_empty() {
                    tool_defs.extend(extra);
                }
                Arc::new(executor) as Arc<dyn ToolExecutor>
            }
            None => Arc::new(NoopRuntimeToolExecutor) as Arc<dyn ToolExecutor>,
        };

        let messages = vec![
            Message {
                role: Role::System,
                content: system_prompt.clone(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::User,
                content: text.to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let engine_context = EngineContext {
            workspace: Some(workspace_path.clone()),
            system_prompt: Some(system_prompt.clone()),
            bridge_tools: None,
            max_tool_rounds: Some(agent.limits.max_tool_rounds),
            max_mcp_result_chars: Some(agent.limits.max_mcp_result_chars),
            mcp_servers: Vec::new(),
        };

        let response = collect_engine_response(
            &*engine,
            &messages,
            &tool_defs,
            &engine_context,
            Some(&*inner_executor),
            None,
            None,
            None,
            agent.limits.max_tool_rounds,
            agent.limits.max_tool_result_chars,
            agent.limits.stream_event_timeout_secs,
            agent.limits.compact_result_limit,
        )
        .await?;

        Ok(response.text)
    }
}

// `NoopActivity` + `NoopRuntimeToolExecutor` are shared with `inbound::eval`
// via `adapters::noop`. Webhooks and evals both rebuild per-turn services
// from config and need identical fallback impls.

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute the expected signature for a body+secret. Used by tests
    /// to build inputs for `verify_hmac`.
    fn sign(body: &[u8], secret: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            hex.push_str(&format!("{:02x}", b));
        }
        format!("sha256={}", hex)
    }

    #[test]
    fn verify_accepts_correct_signature() {
        let body = b"{\"action\":\"opened\",\"pr\":42}";
        let secret = b"abc123";
        let sig = sign(body, secret);
        assert!(verify_hmac(&sig, body, secret).is_ok());
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let body = b"{\"action\":\"opened\"}";
        let secret = b"abc123";
        let sig = sign(body, secret);
        let tampered = b"{\"action\":\"closed\"}";
        let err = verify_hmac(&sig, tampered, secret).unwrap_err();
        assert!(err.to_string().contains("HMAC mismatch"));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let body = b"hello";
        let sig = sign(body, b"correct");
        let err = verify_hmac(&sig, body, b"wrong").unwrap_err();
        assert!(err.to_string().contains("HMAC mismatch"));
    }

    #[test]
    fn verify_rejects_missing_prefix() {
        let body = b"hello";
        let sig_no_prefix = sign(body, b"k").trim_start_matches("sha256=").to_string();
        let err = verify_hmac(&sig_no_prefix, body, b"k").unwrap_err();
        assert!(err.to_string().contains("must start with `sha256=`"));
    }

    #[test]
    fn verify_rejects_malformed_hex() {
        let err = verify_hmac("sha256=zzzz", b"x", b"k").unwrap_err();
        assert!(err.to_string().contains("hex decode failed"));
    }

    #[test]
    fn validate_endpoints_requires_one_secret_source() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: "aura".to_string(),
                secret_env: None,
                secret: None,
                goal_template: "x".to_string(),
            },
        );
        let err = validate_endpoints(&endpoints).unwrap_err();
        assert!(err.to_string().contains("neither"));
    }

    #[test]
    fn validate_endpoints_rejects_both_secret_sources() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: "aura".to_string(),
                secret_env: Some("X".to_string()),
                secret: Some("y".to_string()),
                goal_template: "x".to_string(),
            },
        );
        let err = validate_endpoints(&endpoints).unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn validate_endpoints_rejects_empty_agent() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: String::new(),
                secret_env: Some("X".to_string()),
                secret: None,
                goal_template: "x".to_string(),
            },
        );
        let err = validate_endpoints(&endpoints).unwrap_err();
        assert!(err.to_string().contains("agent"));
    }

    #[test]
    fn hex_decode_round_trip() {
        let bytes = vec![0x00, 0x01, 0xab, 0xcd, 0xff];
        let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        let decoded = hex_decode(&hex).unwrap();
        assert_eq!(decoded, bytes);
        // Mixed case accepted.
        let mixed = "AbCd";
        assert_eq!(hex_decode(mixed).unwrap(), vec![0xab, 0xcd]);
    }
}
