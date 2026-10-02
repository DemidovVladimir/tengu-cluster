//! `SolanaRpc` — JSON-RPC 2.0 over the tool HTTP client, with error
//! classification into `ErrorClass`.
//!
//! | Concern | Rule |
//! |---|---|
//! | URL | `$SOLANA_RPC_URL` when `ctx.scope.check_env_read` allows it and it is set, else `DEFAULT_RPC_URL` |
//! | Egress | every request: `egress::policy().check_url` + `ctx.scope.check_net_host(host)` + one audit record (host only) on `ctx.http` |
//! | Commitment | `confirmed` (the bot's) |
//! | Errors | [`RpcError`]: class + message; the message never contains the URL (RPC URLs embed api keys) — [`SolanaRpc::host`] is the only rendering; transport errors, non-2xx bodies and JSON-RPC `error.message` (any HTTP status) all pass the endpoint [`Scrubber`] |
//! | Retry | at most one, only `Transient` / `RateLimited`, backoff ≤ 1 s (`Retry-After` honoured up to 1 s) |
//! | GMA | chunks of [`GMA_CHUNK`]; chunks after the first are pinned with `minContextSlot` = the first chunk's slot |
//!
//! | Failure | `ErrorClass` |
//! |---|---|
//! | JSON-RPC `-32429` / "max usage reached" (any HTTP status) | `QuotaExhausted` (never retried) |
//! | HTTP 429 / JSON-RPC 429 | `RateLimited` (`retry_after_ms` from `Retry-After`) |
//! | HTTP 401 / 403 | `AuthRequired` |
//! | request timeout | `Timeout` |
//! | JSON-RPC `-32016` (minContextSlot not reached), node-behind / block-not-available codes, `-32603`, HTTP 5xx, connect errors | `Transient` |
//! | malformed response | `Decode` |
//! | egress / scope denial, other codes / 4xx | `Fatal` |
//!
//! The transport is a trait ([`RpcTransport`]) so unit tests script replies
//! (`tests::FakeTransport`); `HttpTransport` is the reqwest implementation.
//! HTTP status / transport classification, [`RpcError`] (=
//! `http_class::HttpError`), [`Scrubber`] and [`display_url`] live in
//! `outbound/http_class.rs` and are re-exported here.

// Called by the Solana tool family (`tools/solana/*`), wired in the next stage.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use reqwest::Url;
use serde_json::{json, Value};

use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::is_quota_text;
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::domain::solana::{AccountRead, AccountState, Pubkey, Signature};
use crate::ports::tool::ToolCtx;

pub(crate) use crate::adapters::outbound::http_class::{
    display_url, http_status_error, read_error, reqwest_error, retry_after_ms,
    HttpError as RpcError, Scrubber,
};

/// Public mainnet endpoint used when `$SOLANA_RPC_URL` is absent / not allowed.
pub(crate) const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
/// Env var naming a custom RPC endpoint (may embed an api key).
pub(crate) const RPC_URL_ENV: &str = "SOLANA_RPC_URL";
/// Max keys per `getMultipleAccounts` call (the RPC's limit).
pub(crate) const GMA_CHUNK: usize = 100;
/// Max signatures per `getSignatureStatuses` call (the RPC's limit).
pub(crate) const SIG_STATUS_LIMIT: usize = 256;
/// Per-request timeout (RPC and `http_json`).
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const COMMITMENT: &str = "confirmed";
const DEFAULT_BACKOFF_MS: u64 = 400;
const MAX_BACKOFF_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// Errors (JSON-RPC codes; the HTTP model is `outbound/http_class.rs`)
// ---------------------------------------------------------------------------

/// Class of a JSON-RPC `error` object (see the module table).
pub(crate) fn classify_rpc_error(code: i64, message: &str) -> ErrorClass {
    if code == -32429 || is_quota_text(message) {
        return ErrorClass::QuotaExhausted;
    }
    match code {
        429 => ErrorClass::RateLimited,
        401 | 403 => ErrorClass::AuthRequired,
        // -32016 minContextSlot not reached · -32004 block not available ·
        // -32005 node unhealthy/behind · -32007 slot skipped · -32009 slot
        // missing in long-term storage · -32014 block status not yet
        // available · -32603 internal error.
        -32016 | -32004 | -32005 | -32007 | -32009 | -32014 | -32603 => ErrorClass::Transient,
        _ if message.to_ascii_lowercase().contains("too many requests") => ErrorClass::RateLimited,
        _ => ErrorClass::Fatal,
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One JSON-RPC POST. Returns the parsed response envelope
/// (`{"jsonrpc","id","result"|"error"}`); HTTP-level failures are
/// classified here, JSON-RPC `error` objects by `SolanaRpc`.
#[async_trait]
pub(crate) trait RpcTransport: Send + Sync {
    async fn call(&self, body: Value) -> std::result::Result<Value, RpcError>;
}

/// reqwest transport over the egress tool client (`ctx.http`).
pub(crate) struct HttpTransport {
    http: reqwest::Client,
    url: Url,
    host: String,
    scope: ToolScope,
    timeout: Duration,
    scrub: Scrubber,
}

impl HttpTransport {
    pub(crate) fn new(
        http: reqwest::Client,
        url: Url,
        scope: ToolScope,
        timeout: Duration,
    ) -> Self {
        let host = url.host_str().unwrap_or("").to_string();
        let scrub = Scrubber::for_rpc(&url);
        Self {
            http,
            url,
            host,
            scope,
            timeout,
            scrub,
        }
    }

    fn audit(&self, method: &str, outcome: std::result::Result<u16, &RpcError>, ms: Option<u64>) {
        let mut event = json!({
            "tool": "solana_rpc",
            "rpc_method": method,
            "host": self.host,
            "ms": ms,
        });
        match outcome {
            Ok(status) => {
                event["verdict"] = "allowed".into();
                event["status"] = status.into();
            }
            Err(e) => {
                event["verdict"] = if ms.is_some() { "error" } else { "denied" }.into();
                event["reason"] = e.message.clone().into();
                if let Some(s) = e.http_status {
                    event["status"] = s.into();
                }
            }
        }
        egress::policy().audit(event);
    }

    /// A 2xx envelope's JSON-RPC `error.message` can echo the request path
    /// or key like a non-2xx body does — scrub it before `parse_envelope`
    /// turns it into an [`RpcError`] message. `result` is never touched.
    fn scrub_rpc_error(&self, mut envelope: Value) -> Value {
        if let Some(m) = envelope.pointer_mut("/error/message") {
            if let Some(text) = m.as_str() {
                *m = Value::String(self.scrub.scrub(text));
            }
        }
        envelope
    }
}

#[async_trait]
impl RpcTransport for HttpTransport {
    async fn call(&self, body: Value) -> std::result::Result<Value, RpcError> {
        let method = body["method"].as_str().unwrap_or("").to_string();
        let gate = egress::policy()
            .check_url(&self.url)
            .and_then(|_| self.scope.check_net_host(&self.host));
        if let Err(e) = gate {
            let err = RpcError::new(ErrorClass::Fatal, self.scrub.scrub(&format!("{e:#}")));
            self.audit(&method, Err(&err), None);
            return Err(err);
        }
        let started = Instant::now();
        let sent = self
            .http
            .post(self.url.clone())
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await;
        let result = match sent {
            Err(e) => Err(reqwest_error(e, &self.host, &self.scrub)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = retry_after_ms(resp.headers());
                match resp.text().await {
                    Err(e) => Err(reqwest_error(e, &self.host, &self.scrub)),
                    Ok(text) if !(200..300).contains(&status) => Err(http_status_error(
                        status,
                        retry_after,
                        &text,
                        &self.host,
                        &self.scrub,
                    )),
                    Ok(text) => serde_json::from_str::<Value>(&text)
                        .map(|v| (status, self.scrub_rpc_error(v)))
                        .map_err(|e| {
                            RpcError::new(
                                ErrorClass::Decode,
                                format!("{} returned a non-JSON body: {e}", self.host),
                            )
                        }),
                }
            }
        };
        let ms = Some(started.elapsed().as_millis() as u64);
        match result {
            Ok((status, v)) => {
                self.audit(&method, Ok(status), ms);
                Ok(v)
            }
            Err(e) => {
                self.audit(&method, Err(&e), ms);
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// JSON-RPC client for one endpoint. Cheap to build per tool call.
pub(crate) struct SolanaRpc {
    transport: Arc<dyn RpcTransport>,
    host: String,
    backoff_ms: u64,
    next_id: AtomicU64,
}

/// The RPC URL a tool call uses: `$SOLANA_RPC_URL` when the scope allows
/// reading it and it is non-empty, else [`DEFAULT_RPC_URL`].
pub(crate) fn rpc_url(scope: &ToolScope) -> String {
    match scope.check_env_read(RPC_URL_ENV) {
        Ok(()) => std::env::var(RPC_URL_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_RPC_URL.to_string()),
        Err(_) => {
            tracing::debug!("{RPC_URL_ENV} not in env_reads; using the public mainnet RPC");
            DEFAULT_RPC_URL.to_string()
        }
    }
}

impl SolanaRpc {
    /// Client for a tool call: URL per [`rpc_url`], `ctx.http`, `ctx.scope`.
    pub(crate) fn from_ctx(ctx: &ToolCtx<'_>) -> Result<Self> {
        Self::from_parts(ctx.http.clone(), &rpc_url(ctx.scope), ctx.scope.clone())
    }

    /// Client over `http` for `raw_url` (errors never echo the URL).
    pub(crate) fn from_parts(
        http: reqwest::Client,
        raw_url: &str,
        scope: ToolScope,
    ) -> Result<Self> {
        let url = Url::parse(raw_url).map_err(|_| anyhow!("{RPC_URL_ENV} is not a valid URL"))?;
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow!("{RPC_URL_ENV} has no host"))?
            .to_string();
        let transport = HttpTransport::new(http, url, scope, REQUEST_TIMEOUT);
        Ok(Self::with_transport(Arc::new(transport), host))
    }

    pub(crate) fn with_transport(
        transport: Arc<dyn RpcTransport>,
        host: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            host: host.into(),
            backoff_ms: DEFAULT_BACKOFF_MS,
            next_id: AtomicU64::new(1),
        }
    }

    /// Backoff before the single retry (capped at 1 s). Tests use 0.
    #[cfg(test)]
    pub(crate) fn with_backoff_ms(mut self, ms: u64) -> Self {
        self.backoff_ms = ms.min(MAX_BACKOFF_MS);
        self
    }

    /// Host only — safe for headlines and audit (the URL may hold a key).
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// One JSON-RPC call; returns `result` (may be `null`). Retries once
    /// on `Transient` / `RateLimited`. Errors are [`RpcError`]s prefixed
    /// with `"<method> @ <host>"`.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut retried = false;
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let outcome = match self.transport.call(body).await {
                Ok(envelope) => parse_envelope(envelope),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(v) => return Ok(v),
                Err(e) if !retried && e.retryable() => {
                    let wait = e
                        .retry_after_ms
                        .unwrap_or(self.backoff_ms)
                        .min(MAX_BACKOFF_MS);
                    tracing::debug!(method, host = %self.host, class = e.class.as_str(), wait_ms = wait, "solana rpc retry");
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    retried = true;
                }
                Err(e) => return Err(e.prefixed(&format!("{method} @ {}", self.host)).into()),
            }
        }
    }

    /// `getMultipleAccounts` (base64). Chunks of 100; later chunks pinned
    /// with `minContextSlot` = the first chunk's slot. Returns that first
    /// slot and one read per key, in order (`null` → `Absent`). No keys →
    /// no request, slot `min_context_slot.unwrap_or(0)`.
    pub(crate) async fn get_multiple_accounts(
        &self,
        keys: &[Pubkey],
        min_context_slot: Option<u64>,
    ) -> Result<(u64, Vec<AccountRead>)> {
        let mut out = Vec::with_capacity(keys.len());
        let mut first_slot: Option<u64> = None;
        for chunk in keys.chunks(GMA_CHUNK) {
            let pin = first_slot.or(min_context_slot);
            let mut cfg = json!({"encoding": "base64", "commitment": COMMITMENT});
            if let Some(s) = pin {
                cfg["minContextSlot"] = json!(s);
            }
            let ids: Vec<String> = chunk.iter().map(Pubkey::to_string).collect();
            let result = self.call("getMultipleAccounts", json!([ids, cfg])).await?;
            let (slot, reads) = parse_gma(&result, chunk)
                .map_err(|e| e.prefixed(&format!("getMultipleAccounts @ {}", self.host)))?;
            first_slot.get_or_insert(slot);
            out.extend(reads);
        }
        Ok((first_slot.or(min_context_slot).unwrap_or(0), out))
    }

    /// `getProgramAccounts` keys only (`dataSlice {0,0}`, `withContext`).
    /// `filters` = RPC filter objects (`{"dataSize": n}`, `{"memcmp": {..}}`).
    pub(crate) async fn get_program_account_keys(
        &self,
        program: &Pubkey,
        filters: &[Value],
    ) -> Result<(u64, Vec<Pubkey>)> {
        let cfg = json!({
            "encoding": "base64",
            "commitment": COMMITMENT,
            "withContext": true,
            "dataSlice": {"offset": 0, "length": 0},
            "filters": filters,
        });
        let result = self
            .call("getProgramAccounts", json!([program.to_string(), cfg]))
            .await?;
        let decode = |m: String| {
            RpcError::new(ErrorClass::Decode, m)
                .prefixed(&format!("getProgramAccounts @ {}", self.host))
        };
        let slot = context_slot(&result).map_err(|e| decode(e.message))?;
        let rows = result["value"]
            .as_array()
            .ok_or_else(|| decode("value is not an array".into()))?;
        let keys = rows
            .iter()
            .map(|r| {
                r["pubkey"]
                    .as_str()
                    .ok_or_else(|| "row without pubkey".to_string())
                    .and_then(|s| s.parse::<Pubkey>())
            })
            .collect::<std::result::Result<Vec<_>, String>>()
            .map_err(decode)?;
        Ok((slot, keys))
    }

    /// `getBalance` → (context slot, lamports).
    pub(crate) async fn get_balance(&self, key: &Pubkey) -> Result<(u64, u64)> {
        let result = self
            .call(
                "getBalance",
                json!([key.to_string(), {"commitment": COMMITMENT}]),
            )
            .await?;
        let slot = self.decoded("getBalance", context_slot(&result))?;
        let lamports = self.decoded(
            "getBalance",
            result["value"]
                .as_u64()
                .ok_or_else(|| RpcError::new(ErrorClass::Decode, "value is not a u64")),
        )?;
        Ok((slot, lamports))
    }

    /// `getTokenAccountsByOwner` (jsonParsed) for one token program →
    /// (context slot, the `value` array as returned).
    pub(crate) async fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<(u64, Value)> {
        let result = self
            .call(
                "getTokenAccountsByOwner",
                json!([
                    owner.to_string(),
                    {"programId": token_program.to_string()},
                    {"encoding": "jsonParsed", "commitment": COMMITMENT},
                ]),
            )
            .await?;
        let slot = self.decoded("getTokenAccountsByOwner", context_slot(&result))?;
        Ok((slot, result["value"].clone()))
    }

    /// `getSignatureStatuses` → (context slot, the `value` array: one
    /// status object or `null` per signature). At most 256 signatures.
    pub(crate) async fn get_signature_statuses(
        &self,
        sigs: &[Signature],
        search_history: bool,
    ) -> Result<(u64, Value)> {
        if sigs.len() > SIG_STATUS_LIMIT {
            return Err(RpcError::new(
                ErrorClass::Fatal,
                format!(
                    "getSignatureStatuses takes at most {SIG_STATUS_LIMIT} signatures, got {}",
                    sigs.len()
                ),
            )
            .into());
        }
        let ids: Vec<String> = sigs.iter().map(Signature::to_string).collect();
        let result = self
            .call(
                "getSignatureStatuses",
                json!([ids, {"searchTransactionHistory": search_history}]),
            )
            .await?;
        let slot = self.decoded("getSignatureStatuses", context_slot(&result))?;
        Ok((slot, result["value"].clone()))
    }

    /// `getTransaction` (json, `maxSupportedTransactionVersion` 0,
    /// commitment `confirmed`) → `None` when the node does not have it.
    pub(crate) async fn get_transaction(&self, sig: &Signature) -> Result<Option<Value>> {
        let result = self
            .call(
                "getTransaction",
                json!([
                    sig.to_string(),
                    {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": COMMITMENT},
                ]),
            )
            .await?;
        Ok((!result.is_null()).then_some(result))
    }

    /// `getSlot` (confirmed).
    #[cfg(test)]
    pub(crate) async fn get_slot(&self) -> Result<u64> {
        let result = self
            .call("getSlot", json!([{"commitment": COMMITMENT}]))
            .await?;
        self.decoded(
            "getSlot",
            result
                .as_u64()
                .ok_or_else(|| RpcError::new(ErrorClass::Decode, "result is not a u64")),
        )
    }

    // ── write path (phase 6b) ──────────────────────────────────────

    /// One request, no retry — the send pipeline decides what a failure of
    /// `sendTransaction` means (only a JSON-RPC error answer proves the
    /// node did not forward it).
    async fn call_once(&self, method: &str, params: Value) -> std::result::Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.transport.call(body).await {
            Ok(envelope) => parse_envelope(envelope),
            Err(e) => Err(e),
        }
        .map_err(|e| e.prefixed(&format!("{method} @ {}", self.host)))
    }

    /// `getLatestBlockhash` (confirmed) → (blockhash, `lastValidBlockHeight`).
    pub(crate) async fn get_latest_blockhash(&self) -> Result<([u8; 32], u64)> {
        let result = self
            .call("getLatestBlockhash", json!([{"commitment": COMMITMENT}]))
            .await?;
        let v = &result["value"];
        let hash: Pubkey = v["blockhash"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("getLatestBlockhash @ {}: no blockhash", self.host))?;
        let lvbh = v["lastValidBlockHeight"].as_u64().ok_or_else(|| {
            anyhow!(
                "getLatestBlockhash @ {}: no lastValidBlockHeight",
                self.host
            )
        })?;
        Ok((hash.0, lvbh))
    }

    /// `getBlockHeight` (confirmed).
    pub(crate) async fn get_block_height(&self) -> Result<u64> {
        let result = self
            .call("getBlockHeight", json!([{"commitment": COMMITMENT}]))
            .await?;
        result
            .as_u64()
            .ok_or_else(|| anyhow!("getBlockHeight @ {}: result is not a u64", self.host))
    }

    /// `simulateTransaction` without signature checks and with the
    /// cluster's blockhash (`replaceRecentBlockhash`) — needs no key.
    pub(crate) async fn simulate_transaction(&self, tx: &[u8]) -> Result<Simulation> {
        let result = self
            .call(
                "simulateTransaction",
                json!([
                    B64.encode(tx),
                    {"encoding": "base64", "sigVerify": false, "replaceRecentBlockhash": true,
                     "commitment": COMMITMENT},
                ]),
            )
            .await?;
        let slot = self.decoded("simulateTransaction", context_slot(&result))?;
        Ok(parse_simulation(slot, &result["value"]))
    }

    /// ONE `sendTransaction` attempt (preflight on, `confirmed`, the node
    /// rebroadcasts up to 3 times). Returns the RPC's error untouched.
    pub(crate) async fn send_transaction_once(
        &self,
        tx: &[u8],
    ) -> std::result::Result<Signature, RpcError> {
        let result = self
            .call_once(
                "sendTransaction",
                json!([
                    B64.encode(tx),
                    {"encoding": "base64", "skipPreflight": false,
                     "preflightCommitment": COMMITMENT, "maxRetries": 3},
                ]),
            )
            .await?;
        result.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| {
            RpcError::new(
                ErrorClass::Decode,
                "sendTransaction: result is not a signature",
            )
        })
    }

    /// Helius `getPriorityFeeEstimate` (recommended level) for `tx`, in
    /// micro-lamports per CU. Only Helius serves it.
    pub(crate) async fn get_priority_fee_estimate(&self, tx: &[u8]) -> Result<f64> {
        let result = self
            .call(
                "getPriorityFeeEstimate",
                json!([{"transaction": B64.encode(tx),
                        "options": {"recommended": true, "transactionEncoding": "base64"}}]),
            )
            .await?;
        result["priorityFeeEstimate"]
            .as_f64()
            .ok_or_else(|| anyhow!("getPriorityFeeEstimate @ {}: no estimate", self.host))
    }

    fn decoded<T>(&self, method: &str, r: std::result::Result<T, RpcError>) -> Result<T> {
        r.map_err(|e| e.prefixed(&format!("{method} @ {}", self.host)).into())
    }
}

// ---------------------------------------------------------------------------
// Response parsing (pure)
// ---------------------------------------------------------------------------

/// `result` of a JSON-RPC envelope, or its classified `error`.
pub(crate) fn parse_envelope(envelope: Value) -> std::result::Result<Value, RpcError> {
    if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
        let code = err["code"].as_i64().unwrap_or(0);
        let message = err["message"]
            .as_str()
            .unwrap_or("(no message)")
            .to_string();
        let mut e = RpcError::new(
            classify_rpc_error(code, &message),
            format!("JSON-RPC error {code}: {message}"),
        );
        e.code = Some(code);
        e.data = err.get("data").filter(|d| !d.is_null()).cloned();
        return Err(e);
    }
    match envelope {
        Value::Object(mut o) if o.contains_key("result") => {
            Ok(o.remove("result").unwrap_or(Value::Null))
        }
        _ => Err(RpcError::new(
            ErrorClass::Decode,
            "response has neither result nor error",
        )),
    }
}

/// `result.context.slot`.
pub(crate) fn context_slot(result: &Value) -> std::result::Result<u64, RpcError> {
    result["context"]["slot"]
        .as_u64()
        .ok_or_else(|| RpcError::new(ErrorClass::Decode, "result has no context.slot"))
}

/// `simulateTransaction` outcome.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Simulation {
    pub slot: u64,
    /// `value.err` verbatim; `None` = success.
    pub err: Option<Value>,
    pub logs: Vec<String>,
    pub units: Option<u64>,
}

pub(crate) fn parse_simulation(slot: u64, value: &Value) -> Simulation {
    Simulation {
        slot,
        err: value.get("err").filter(|e| !e.is_null()).cloned(),
        logs: value["logs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        units: value["unitsConsumed"].as_u64(),
    }
}

/// Parse a `getMultipleAccounts` `result` for `keys` (same order). A
/// `null` entry is `Absent`; anything malformed is a `Decode` error.
pub(crate) fn parse_gma(
    result: &Value,
    keys: &[Pubkey],
) -> std::result::Result<(u64, Vec<AccountRead>), RpcError> {
    let slot = context_slot(result)?;
    let decode = |m: String| RpcError::new(ErrorClass::Decode, m);
    let rows = result["value"]
        .as_array()
        .ok_or_else(|| decode("value is not an array".into()))?;
    if rows.len() != keys.len() {
        return Err(decode(format!(
            "{} accounts returned for {} keys",
            rows.len(),
            keys.len()
        )));
    }
    let mut out = Vec::with_capacity(keys.len());
    for (key, row) in keys.iter().zip(rows) {
        let state = if row.is_null() {
            AccountState::Absent
        } else {
            let bad = |what: &str| decode(format!("account {key}: {what}"));
            let owner: Pubkey = row["owner"]
                .as_str()
                .ok_or_else(|| bad("no owner"))?
                .parse()
                .map_err(|_| bad("owner is not a pubkey"))?;
            let lamports = row["lamports"].as_u64().ok_or_else(|| bad("no lamports"))?;
            let data = row["data"]
                .as_array()
                .ok_or_else(|| bad("data is not [b64, encoding]"))?;
            if data.get(1).and_then(Value::as_str) != Some("base64") {
                return Err(bad("data encoding is not base64"));
            }
            let data_b64 = data
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| bad("data is not a string"))?;
            base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|_| bad("data is not valid base64"))?;
            AccountState::Ok {
                owner,
                lamports,
                data_b64: data_b64.to_string(),
                executable: row["executable"].as_bool().unwrap_or(false),
            }
        };
        out.push(AccountRead {
            pubkey: *key,
            slot,
            state,
        });
    }
    Ok((slot, out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    use sha2::{Digest, Sha256};

    use crate::adapters::outbound::http_class::classify_http_status;
    use crate::domain::solana::ids;

    // ── fake transport (shared with accounts.rs tests) ──────────────

    pub(crate) fn ok_envelope(result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": result})
    }

    pub(crate) fn err_envelope(code: i64, message: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": message}})
    }

    /// Scripted replies first; otherwise serves `getMultipleAccounts` from
    /// `accounts` at `slot` (honouring `minContextSlot` with -32016) and
    /// `getSlot`. Records every request body.
    #[derive(Default)]
    pub(crate) struct FakeTransport {
        pub requests: Mutex<Vec<Value>>,
        pub script: Mutex<VecDeque<std::result::Result<Value, RpcError>>>,
        pub accounts: Mutex<BTreeMap<Pubkey, AccountRead>>,
        pub slot: AtomicU64,
    }

    impl FakeTransport {
        pub(crate) fn at_slot(slot: u64) -> Self {
            let f = Self::default();
            f.slot.store(slot, Ordering::SeqCst);
            f
        }
        pub(crate) fn put_account(&self, read: AccountRead) {
            self.accounts.lock().unwrap().insert(read.pubkey, read);
        }
        pub(crate) fn push(&self, reply: std::result::Result<Value, RpcError>) {
            self.script.lock().unwrap().push_back(reply);
        }
        pub(crate) fn requests(&self) -> Vec<Value> {
            self.requests.lock().unwrap().clone()
        }
        /// `params` of every `getMultipleAccounts` request.
        pub(crate) fn gma_params(&self) -> Vec<Value> {
            self.requests()
                .into_iter()
                .filter(|b| b["method"] == "getMultipleAccounts")
                .map(|b| b["params"].clone())
                .collect()
        }
    }

    #[async_trait]
    impl RpcTransport for FakeTransport {
        async fn call(&self, body: Value) -> std::result::Result<Value, RpcError> {
            self.requests.lock().unwrap().push(body.clone());
            if let Some(reply) = self.script.lock().unwrap().pop_front() {
                return reply;
            }
            let slot = self.slot.load(Ordering::SeqCst);
            match body["method"].as_str() {
                Some("getMultipleAccounts") => {
                    if let Some(min) = body["params"][1]["minContextSlot"].as_u64() {
                        if min > slot {
                            return Ok(err_envelope(
                                -32016,
                                "Minimum context slot has not been reached",
                            ));
                        }
                    }
                    let accounts = self.accounts.lock().unwrap();
                    let value: Vec<Value> = body["params"][0]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|k| {
                            let key: Pubkey = k.as_str().unwrap().parse().unwrap();
                            match accounts.get(&key).map(|r| &r.state) {
                                Some(AccountState::Ok {
                                    owner,
                                    lamports,
                                    data_b64,
                                    executable,
                                }) => json!({
                                    "owner": owner.to_string(),
                                    "lamports": lamports,
                                    "data": [data_b64, "base64"],
                                    "executable": executable,
                                    "rentEpoch": u64::MAX,
                                }),
                                _ => Value::Null,
                            }
                        })
                        .collect();
                    Ok(ok_envelope(
                        json!({"context": {"slot": slot}, "value": value}),
                    ))
                }
                Some("getSlot") => Ok(ok_envelope(json!(slot))),
                other => Err(RpcError::new(
                    ErrorClass::Fatal,
                    format!("fake transport: unscripted method {other:?}"),
                )),
            }
        }
    }

    pub(crate) fn fake_rpc(t: &Arc<FakeTransport>) -> SolanaRpc {
        SolanaRpc::with_transport(t.clone(), "fake.rpc").with_backoff_ms(0)
    }

    fn key(i: u32) -> Pubkey {
        Pubkey(Sha256::digest(i.to_le_bytes()).into())
    }

    fn class_of(e: &anyhow::Error) -> ErrorClass {
        read_error("f", e).class
    }

    // ── classification ──────────────────────────────────────────────

    #[test]
    fn classifies_json_rpc_errors() {
        use ErrorClass::*;
        for (code, msg, want) in [
            (-32429, "rate", QuotaExhausted),
            (-32000, "max usage reached", QuotaExhausted),
            (
                429,
                "Too many requests for a specific RPC call",
                RateLimited,
            ),
            (
                -32016,
                "Minimum context slot has not been reached",
                Transient,
            ),
            (-32005, "Node is behind by 42 slots", Transient),
            (-32603, "Internal error", Transient),
            (-32602, "Invalid params", Fatal),
            (-32601, "Method not found", Fatal),
            (-32010, "excluded from account secondary indexes", Fatal),
            (401, "unauthorized", AuthRequired),
        ] {
            assert_eq!(classify_rpc_error(code, msg), want, "{code} {msg}");
        }
        for (status, body, want) in [
            (429, "", RateLimited),
            (
                429,
                r#"{"error":{"code":-32429,"message":"max usage reached"}}"#,
                QuotaExhausted,
            ),
            (401, "", AuthRequired),
            (403, "forbidden", AuthRequired),
            (408, "", Timeout),
            (502, "bad gateway", Transient),
            (503, "", Transient),
            (404, "", Fatal),
            (413, "", Fatal),
        ] {
            assert_eq!(classify_http_status(status, body), want, "{status}");
        }
    }

    #[test]
    fn envelope_parsing() {
        assert_eq!(parse_envelope(ok_envelope(json!(7))).unwrap(), json!(7));
        assert_eq!(
            parse_envelope(ok_envelope(Value::Null)).unwrap(),
            Value::Null
        );
        let e = parse_envelope(err_envelope(-32016, "Minimum context slot")).unwrap_err();
        assert_eq!((e.class, e.code), (ErrorClass::Transient, Some(-32016)));
        let e = parse_envelope(json!({"jsonrpc": "2.0", "id": 1})).unwrap_err();
        assert_eq!(e.class, ErrorClass::Decode);
    }

    #[test]
    fn scrubber_removes_url_path_and_query_secrets() {
        let url = Url::parse(
            "https://mainnet.helius-rpc.com/v1/tok3n-SECRETPATH123/?api-key=SECRETKEY-abcdef12&x=1",
        )
        .unwrap();
        let s = Scrubber::for_rpc(&url);
        let msg = format!(
            "failed for {url} (path {}) key SECRETKEY-abcdef12 seg tok3n-SECRETPATH123",
            url.path()
        );
        let out = s.scrub(&msg);
        assert!(!out.contains("SECRETKEY"), "{out}");
        assert!(!out.contains("SECRETPATH"), "{out}");
        assert!(out.contains("mainnet.helius-rpc.com"), "{out}");
    }

    #[test]
    fn read_error_uses_the_rpc_class() {
        let mut e = RpcError::new(ErrorClass::RateLimited, "slow down");
        e.retry_after_ms = Some(500);
        let any: anyhow::Error = anyhow::Error::new(e).context("reading pool");
        let r = read_error("pool", &any);
        assert_eq!(r.class, ErrorClass::RateLimited);
        assert_eq!(r.retry_after_ms, Some(500));
        assert_eq!(r.field, "pool");
        let r = read_error("pool", &anyhow!("boom"));
        assert_eq!(r.class, ErrorClass::Fatal);
    }

    // ── SolanaRpc over the fake transport ───────────────────────────

    #[tokio::test]
    async fn gma_chunks_by_100_and_pins_later_chunks_to_the_first_slot() {
        let t = Arc::new(FakeTransport::at_slot(1_000));
        let keys: Vec<Pubkey> = (0..250).map(key).collect();
        for (i, k) in keys.iter().enumerate().filter(|(i, _)| i % 3 == 0) {
            t.put_account(AccountRead::from_bytes(
                *k,
                0,
                ids::key(ids::TOKEN),
                i as u64,
                &[i as u8; 5],
            ));
        }
        let rpc = fake_rpc(&t);
        let (slot, reads) = rpc.get_multiple_accounts(&keys, None).await.unwrap();
        assert_eq!(slot, 1_000);
        assert_eq!(reads.len(), 250);
        for (i, r) in reads.iter().enumerate() {
            assert_eq!(r.pubkey, keys[i]);
            assert_eq!(r.slot, 1_000);
            assert_eq!(r.exists(), i % 3 == 0, "key {i}");
            if i % 3 == 0 {
                assert_eq!(r.lamports(), Some(i as u64));
                assert_eq!(r.data().unwrap(), vec![i as u8; 5]);
            }
        }
        let calls = t.gma_params();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .map(|p| p[0].as_array().unwrap().len())
                .collect::<Vec<_>>(),
            vec![100, 100, 50]
        );
        assert!(calls[0][1].get("minContextSlot").is_none());
        assert_eq!(calls[1][1]["minContextSlot"], 1_000);
        assert_eq!(calls[2][1]["minContextSlot"], 1_000);
        assert_eq!(calls[0][1]["encoding"], "base64");
        assert_eq!(calls[0][1]["commitment"], "confirmed");
    }

    #[tokio::test]
    async fn gma_passes_min_context_slot_and_skips_empty_requests() {
        let t = Arc::new(FakeTransport::at_slot(500));
        let rpc = fake_rpc(&t);
        let (slot, reads) = rpc.get_multiple_accounts(&[], Some(9)).await.unwrap();
        assert_eq!((slot, reads.len()), (9, 0));
        assert!(t.requests().is_empty());
        rpc.get_multiple_accounts(&[key(1)], Some(400))
            .await
            .unwrap();
        assert_eq!(t.gma_params()[0][1]["minContextSlot"], 400);
        // Node behind the pin: -32016 is Transient, retried once, then fails.
        let e = rpc
            .get_multiple_accounts(&[key(1)], Some(501))
            .await
            .unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Transient);
        assert_eq!(t.gma_params().len(), 3, "one retry");
        assert!(
            format!("{e}").contains("getMultipleAccounts @ fake.rpc"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn gma_rejects_malformed_results() {
        let t = Arc::new(FakeTransport::at_slot(5));
        let rpc = fake_rpc(&t);
        t.push(Ok(ok_envelope(
            json!({"context": {"slot": 5}, "value": [null]}),
        )));
        let e = rpc
            .get_multiple_accounts(&[key(1), key(2)], None)
            .await
            .unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Decode);
        t.push(Ok(ok_envelope(json!({"context": {"slot": 5}, "value": [
            {"owner": ids::TOKEN, "lamports": 1, "data": ["AAAA", "base58"], "executable": false}
        ]}))));
        let e = rpc
            .get_multiple_accounts(&[key(1)], None)
            .await
            .unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Decode);
        t.push(Ok(ok_envelope(json!({"value": []}))));
        let e = rpc
            .get_multiple_accounts(&[key(1)], None)
            .await
            .unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Decode);
    }

    #[tokio::test]
    async fn retry_policy_by_class() {
        let t = Arc::new(FakeTransport::at_slot(77));
        let rpc = fake_rpc(&t);
        // Transient then success: 2 requests, Ok.
        t.push(Err(RpcError::new(ErrorClass::Transient, "connect reset")));
        assert_eq!(rpc.get_slot().await.unwrap(), 77);
        assert_eq!(t.requests().len(), 2);
        // RateLimited twice: exactly one retry, then the error.
        let mut rl = RpcError::new(ErrorClass::RateLimited, "HTTP 429");
        rl.retry_after_ms = Some(0);
        t.push(Err(rl.clone()));
        t.push(Err(rl));
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::RateLimited);
        assert_eq!(t.requests().len(), 4);
        // QuotaExhausted is never retried (JSON-RPC -32429).
        t.push(Ok(err_envelope(-32429, "max usage reached")));
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::QuotaExhausted);
        assert_eq!(t.requests().len(), 5);
        // Timeout / AuthRequired / Fatal: no retry.
        for class in [
            ErrorClass::Timeout,
            ErrorClass::AuthRequired,
            ErrorClass::Fatal,
        ] {
            let before = t.requests().len();
            t.push(Err(RpcError::new(class, "x")));
            let e = rpc.get_slot().await.unwrap_err();
            assert_eq!(class_of(&e), class);
            assert_eq!(t.requests().len(), before + 1, "{class:?}");
        }
        // Request ids increase; bodies are JSON-RPC 2.0.
        let reqs = t.requests();
        assert!(reqs.iter().all(|b| b["jsonrpc"] == "2.0"));
        assert!(reqs[1]["id"].as_u64() > reqs[0]["id"].as_u64());
    }

    #[tokio::test]
    async fn typed_methods_build_requests_and_parse_results() {
        let t = Arc::new(FakeTransport::at_slot(1));
        let rpc = fake_rpc(&t);
        let wallet = ids::key(ids::JLP_POOL);
        t.push(Ok(ok_envelope(
            json!({"context": {"slot": 10}, "value": 2_039_280u64}),
        )));
        assert_eq!(rpc.get_balance(&wallet).await.unwrap(), (10, 2_039_280));

        t.push(Ok(ok_envelope(json!({"context": {"slot": 11}, "value": [
            {"pubkey": ids::USDC, "account": {"data": ["", "base64"]}},
            {"pubkey": ids::WSOL, "account": {"data": ["", "base64"]}},
        ]}))));
        let filters = [
            json!({"dataSize": 8120}),
            json!({"memcmp": {"offset": 8, "bytes": ids::JLP_POOL}}),
        ];
        let (slot, keys) = rpc
            .get_program_account_keys(&ids::key(ids::DLMM), &filters)
            .await
            .unwrap();
        assert_eq!(
            (slot, keys),
            (11, vec![ids::key(ids::USDC), ids::key(ids::WSOL)])
        );

        t.push(Ok(ok_envelope(
            json!({"context": {"slot": 12}, "value": []}),
        )));
        let (slot, v) = rpc
            .get_token_accounts_by_owner(&wallet, &ids::key(ids::TOKEN))
            .await
            .unwrap();
        assert_eq!((slot, v), (12, json!([])));

        let sig = Signature([9u8; 64]);
        t.push(Ok(ok_envelope(
            json!({"context": {"slot": 13}, "value": [null]}),
        )));
        let (slot, v) = rpc.get_signature_statuses(&[sig], true).await.unwrap();
        assert_eq!((slot, v), (13, json!([null])));
        t.push(Ok(ok_envelope(Value::Null)));
        assert_eq!(rpc.get_transaction(&sig).await.unwrap(), None);
        t.push(Ok(ok_envelope(json!({"slot": 14, "meta": {"fee": 5000}}))));
        assert_eq!(
            rpc.get_transaction(&sig).await.unwrap().unwrap()["meta"]["fee"],
            5000
        );
        let too_many = vec![sig; SIG_STATUS_LIMIT + 1];
        assert!(rpc.get_signature_statuses(&too_many, false).await.is_err());

        let reqs = t.requests();
        let p = |m: &str| {
            reqs.iter()
                .find(|b| b["method"] == m)
                .map(|b| b["params"].clone())
                .unwrap()
        };
        let gpa = p("getProgramAccounts");
        assert_eq!(gpa[0], ids::DLMM);
        assert_eq!(gpa[1]["dataSlice"], json!({"offset": 0, "length": 0}));
        assert_eq!(gpa[1]["withContext"], true);
        assert_eq!(gpa[1]["filters"][0]["dataSize"], 8120);
        let tabo = p("getTokenAccountsByOwner");
        assert_eq!(tabo[1]["programId"], ids::TOKEN);
        assert_eq!(tabo[2]["encoding"], "jsonParsed");
        let gss = p("getSignatureStatuses");
        assert_eq!(gss[0][0], sig.to_string());
        assert_eq!(gss[1]["searchTransactionHistory"], true);
        let gt = p("getTransaction");
        assert_eq!(gt[1]["maxSupportedTransactionVersion"], 0);
        assert_eq!(gt[1]["encoding"], "json");
    }

    // ── real mainnet response (tests/fixtures/solana/core) ─────────

    pub(crate) const CORE_GMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/core/gma.json"
    ));
    pub(crate) const CORE_META: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/core/meta.json"
    ));

    /// The fixture's keys, in request order.
    pub(crate) fn core_keys() -> Vec<Pubkey> {
        let meta: Value = serde_json::from_str(CORE_META).unwrap();
        meta["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().parse().unwrap())
            .collect()
    }

    /// Values below were decoded independently (python `struct` over the
    /// same base64) at capture time: slot 450104084, 2026-09-24.
    #[tokio::test]
    async fn gma_parses_a_real_mainnet_response() {
        let t = Arc::new(FakeTransport::at_slot(0));
        t.push(Ok(serde_json::from_str(CORE_GMA).unwrap()));
        let keys = core_keys();
        let (slot, reads) = fake_rpc(&t)
            .get_multiple_accounts(&keys, None)
            .await
            .unwrap();
        assert_eq!(slot, 450_104_084);
        assert_eq!(reads.len(), 13);
        let by = |s: &str| reads.iter().find(|r| r.pubkey.to_string() == s).unwrap();
        let expect = [
            (
                "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
                ids::DLMM,
                904,
                1_469_254_159u64,
            ),
            (
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
                ids::DLMM,
                904,
                62_371_634,
            ),
            (ids::WSOL, ids::TOKEN, 82, 1_845_235_676_975),
            (ids::USDC, ids::TOKEN, 82, 534_788_257_985),
            (
                "EYj9xKw6ZszwpyNibHY7JD5o3QgTVrSdcBp1fMJhrR9o",
                ids::TOKEN,
                165,
                10_976_634_791_676,
            ),
            (
                "CoaxzEh8p5YyGLcj36Eo3cUThVJxeKCs7qvLAGDYwBcz",
                ids::TOKEN,
                165,
                2_039_396,
            ),
            (
                "DwZz4S1Z1LBXomzmncQRVKCYhjCqSAMQ6RPKbUAadr7H",
                ids::TOKEN,
                165,
                10_152_636_522_616,
            ),
            (
                "4N22J4vW2juHocTntJNmXywSonYjkndCwahjZ2cYLDgb",
                ids::TOKEN,
                165,
                2_039_288,
            ),
            (ids::JLP_POOL, ids::JUP_PERPS, 2000, 102_463_178),
            (ids::JUP_CUSTODY_SOL, ids::JUP_PERPS, 2000, 14_810_884),
            (ids::JUP_CUSTODY_USDC, ids::JUP_PERPS, 2000, 14_810_883),
            (
                "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK",
                ids::JUP_PERPS,
                216,
                2_394_240,
            ),
        ];
        for (k, owner, len, lamports) in expect {
            let r = by(k);
            assert_eq!(r.slot, slot);
            assert_eq!(r.owner(), Some(&ids::key(owner)), "{k}");
            assert_eq!(r.data().unwrap().len(), len, "{k}");
            assert_eq!(r.lamports(), Some(lamports), "{k}");
        }
        // Never-opened long position: null → Absent.
        assert!(!by("FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY").exists());
        // Discriminators and known SPL fields.
        let data = |k: &str| by(k).data().unwrap();
        assert_eq!(
            data("5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6")[..8],
            LB_PAIR_DISC
        );
        assert_eq!(data(ids::JLP_POOL)[..8], anchor_disc("Pool"));
        assert_eq!(data(ids::JUP_CUSTODY_SOL)[..8], anchor_disc("Custody"));
        assert_eq!(
            data("6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK")[..8],
            anchor_disc("Position")
        );
        assert_eq!(data(ids::USDC)[44], 6, "USDC decimals");
        assert_eq!(data(ids::WSOL)[44], 9, "wSOL decimals");
        // SOL-USDC reserves hold the pair's mints (LbPair token_x@88 / reserve_x@152).
        let lb = data("5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6");
        for (mint_off, reserve_off) in [(88, 152), (120, 184)] {
            let reserve = Pubkey::read(&lb, reserve_off).unwrap();
            let r = reads.iter().find(|r| r.pubkey == reserve).unwrap();
            assert_eq!(
                Pubkey::read(&r.data().unwrap(), 0),
                Pubkey::read(&lb, mint_off)
            );
        }
    }

    // ── HttpTransport against a local HTTP server ───────────────────

    pub(crate) use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };

    fn http_rpc(base: &str, path_and_query: &str, scope: ToolScope, timeout_ms: u64) -> SolanaRpc {
        let url = Url::parse(&format!("{base}{path_and_query}")).unwrap();
        let host = url.host_str().unwrap().to_string();
        let t = HttpTransport::new(test_client(), url, scope, Duration::from_millis(timeout_ms));
        SolanaRpc::with_transport(Arc::new(t), host).with_backoff_ms(0)
    }

    const SECRET_PATH: &str = "/v1/SECRETPATHTOKEN42/";
    const SECRET_QUERY: &str = "?api-key=SECRETKEY9876543";

    pub(crate) fn assert_no_secret(e: &anyhow::Error) {
        let text = format!("{e:#} {e:?}");
        assert!(!text.contains("SECRET"), "secret leaked: {text}");
        assert!(!text.contains("http://127.0.0.1"), "url leaked: {text}");
    }

    #[tokio::test]
    async fn http_transport_success_and_status_classes() {
        let (base, seen) = serve(vec![
            canned(200, ok_envelope(json!(4242)).to_string()),
            Canned {
                status: 429,
                headers: "Retry-After: 0\r\n",
                body: "Too many requests SECRETKEY9876543".into(),
                delay_ms: 0,
            },
            canned(429, "slow down"),
            canned(401, r#"{"error":"invalid api key SECRETKEY9876543"}"#),
            canned(
                429,
                r#"{"jsonrpc":"2.0","error":{"code":-32429,"message":"max usage reached"}}"#,
            ),
            canned(200, "<html>not json</html>"),
            canned(503, "unavailable"),
            canned(200, ok_envelope(json!(7)).to_string()),
        ])
        .await;
        let path = format!("{SECRET_PATH}{SECRET_QUERY}");
        let rpc = http_rpc(&base, &path, local_scope(), 2_000);
        assert_eq!(rpc.host(), "127.0.0.1");
        assert_eq!(rpc.get_slot().await.unwrap(), 4242);
        let first = seen.lock().unwrap()[0].clone();
        assert!(
            first.starts_with(&format!("POST {path} HTTP/1.1")),
            "{first}"
        );
        assert!(first.contains(r#""method":"getSlot""#), "{first}");

        // 429 (Retry-After 0) twice → RateLimited after one retry.
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::RateLimited);
        assert_eq!(
            read_error("slot", &e).retry_after_ms,
            None,
            "second 429 had no header"
        );
        assert_no_secret(&e);
        // 401 → AuthRequired, not retried.
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::AuthRequired);
        assert_no_secret(&e);
        // HTTP 429 carrying -32429 → QuotaExhausted, not retried.
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::QuotaExhausted);
        // Non-JSON 200 → Decode, not retried.
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Decode);
        // 503 → Transient, retried once → success.
        assert_eq!(rpc.get_slot().await.unwrap(), 7);
        assert_eq!(seen.lock().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn http_transport_timeout_and_scope_denial_never_leak_the_url() {
        let (base, seen) = serve(vec![Canned {
            status: 200,
            headers: "",
            body: ok_envelope(json!(1)).to_string(),
            delay_ms: 1_500,
        }])
        .await;
        let path = format!("{SECRET_PATH}{SECRET_QUERY}");
        let rpc = http_rpc(&base, &path, local_scope(), 200);
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Timeout);
        assert_no_secret(&e);

        // Host not in net_hosts: refused before any request, Fatal.
        let denied = http_rpc(&base, &path, ToolScope::default(), 200);
        let before = seen.lock().unwrap().len();
        let e = denied.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Fatal);
        assert!(format!("{e}").contains("net_hosts"), "{e}");
        assert_no_secret(&e);
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn json_rpc_error_on_http_200_is_scrubbed() {
        // The provider answers 200 with a JSON-RPC error quoting the
        // request path and the bare key.
        let echo = json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32602,
            "message": format!("bad request {SECRET_PATH}{SECRET_QUERY} key SECRETKEY9876543"),
        }});
        let (base, _) = serve(vec![canned(200, echo.to_string())]).await;
        let path = format!("{SECRET_PATH}{SECRET_QUERY}");
        let rpc = http_rpc(&base, &path, local_scope(), 2_000);
        let e = rpc.get_slot().await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Fatal);
        assert_no_secret(&e);
        assert!(
            format!("{e:#}").contains("JSON-RPC error -32602: bad request 127.0.0.1"),
            "{e:#}"
        );
        assert!(!read_error("slot", &e).message.contains("SECRET"));
    }

    #[test]
    fn rpc_url_honours_env_scope() {
        // Scope without env_reads never reads the env var.
        assert_eq!(rpc_url(&ToolScope::default()), DEFAULT_RPC_URL);
        let bad = SolanaRpc::from_parts(test_client(), "not a url", ToolScope::default());
        assert!(bad.is_err_and(|e| !format!("{e}").contains("not a url")));
        let rpc = SolanaRpc::from_parts(
            test_client(),
            "https://rpc.example.com/SECRETPATHTOKEN42?api-key=SECRETKEY9876543",
            ToolScope::default(),
        )
        .unwrap();
        assert_eq!(rpc.host(), "rpc.example.com");
    }

    // ── live (public mainnet; `cargo test --bin tengu live_ -- --ignored`) ──

    pub(crate) fn live_rpc() -> SolanaRpc {
        let scope = ToolScope {
            net_hosts: vec!["api.mainnet-beta.solana.com".into()],
            ..Default::default()
        };
        let http = egress::policy().tool_client(REQUEST_TIMEOUT).unwrap();
        SolanaRpc::from_parts(http, DEFAULT_RPC_URL, scope).unwrap()
    }

    /// Anchor account discriminator: sha256("account:<Name>")[..8].
    pub(crate) fn anchor_disc(name: &str) -> [u8; 8] {
        let h = Sha256::digest(format!("account:{name}").as_bytes());
        h[..8].try_into().unwrap()
    }

    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const POOL_SOL_USDC: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    const POOL_2: &str = "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y";
    const LB_PAIR_DISC: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
    const BIN_ARRAY_DISC: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];

    #[tokio::test]
    #[ignore]
    async fn live_solcore_gma_jlp() {
        let rpc = live_rpc();
        let keys = [
            ids::key(ids::JLP_POOL),
            ids::key(ids::JUP_CUSTODY_SOL),
            ids::key(ids::JUP_CUSTODY_USDC),
        ];
        let (slot, reads) = rpc.get_multiple_accounts(&keys, None).await.unwrap();
        assert!(slot > 450_000_000, "slot {slot}");
        let perps = ids::key(ids::JUP_PERPS);
        for (read, name) in reads.iter().zip(["Pool", "Custody", "Custody"]) {
            assert_eq!(read.owner(), Some(&perps), "{}", read.pubkey);
            let d = read.data().unwrap();
            assert_eq!(d[..8], anchor_disc(name), "{} {name}", read.pubkey);
        }
        let set = crate::domain::solana::AccountSet {
            accounts: reads.into_iter().map(|r| (r.pubkey, r)).collect(),
            slot_min: slot,
            slot_max: slot,
        };
        assert!(set.data_owned_by(&keys[1], &perps, 1060).is_some());
    }

    #[tokio::test]
    #[ignore]
    async fn live_solcore_pdas() {
        let rpc = live_rpc();
        let w: Pubkey = WALLET.parse().unwrap();
        let pool: Pubkey = POOL_SOL_USDC.parse().unwrap();
        let (_, lb) = rpc.get_multiple_accounts(&[pool], None).await.unwrap();
        let lb = lb[0].data().unwrap();
        assert_eq!(lb[..8], LB_PAIR_DISC);
        let active = i32::from_le_bytes(lb[76..80].try_into().unwrap());
        let idx = crate::domain::lp::gates::bin_array_index(active);
        let arrays: Vec<i64> = vec![idx - 1, idx, idx + 1, -77, -76];
        let mut keys = vec![
            crate::domain::solana::jup_position_pda(&w, false),
            crate::domain::solana::jup_position_pda(&w, true),
        ];
        keys.extend(
            arrays
                .iter()
                .map(|i| crate::domain::solana::bin_array_pda(&pool, *i)),
        );
        let (slot, reads) = rpc.get_multiple_accounts(&keys, None).await.unwrap();
        // Short position: an existing Jupiter Position account (216 B live).
        let short = &reads[0];
        assert_eq!(short.owner(), Some(&ids::key(ids::JUP_PERPS)));
        let d = short.data().unwrap();
        assert_eq!(d[..8], anchor_disc("Position"));
        assert_eq!(Pubkey::read(&d, 8), Some(w), "position owner");
        // Long position: never opened.
        assert!(!reads[1].exists(), "long PDA exists at slot {slot}");
        for (read, i) in reads[2..].iter().zip(&arrays) {
            let d = read
                .data()
                .unwrap_or_else(|| panic!("bin array {i} absent at slot {slot}"));
            assert_eq!(read.owner(), Some(&ids::key(ids::DLMM)));
            assert_eq!(d[..8], BIN_ARRAY_DISC);
            assert_eq!(i64::from_le_bytes(d[8..16].try_into().unwrap()), *i);
            assert_eq!(Pubkey::read(&d, 24), Some(pool), "lb_pair of array {i}");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_solcore_ata() {
        let rpc = live_rpc();
        let w: Pubkey = WALLET.parse().unwrap();
        let mut matched = 0;
        for program in [ids::TOKEN, ids::TOKEN_2022] {
            let p = ids::key(program);
            let (_, rows) = rpc.get_token_accounts_by_owner(&w, &p).await.unwrap();
            for row in rows.as_array().unwrap() {
                let info = &row["account"]["data"]["parsed"]["info"];
                let mint: Pubkey = info["mint"].as_str().unwrap().parse().unwrap();
                let address: Pubkey = row["pubkey"].as_str().unwrap().parse().unwrap();
                assert_eq!(info["owner"], WALLET);
                if crate::domain::solana::ata(&w, &mint, &p) == address {
                    matched += 1;
                }
                if mint == ids::key(ids::USDC) {
                    assert_eq!(
                        address.to_string(),
                        "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1"
                    );
                }
            }
        }
        assert!(matched >= 1, "no ATA matched");
    }

    #[tokio::test]
    #[ignore]
    async fn live_solcore_misc_methods() {
        let rpc = live_rpc();
        let w: Pubkey = WALLET.parse().unwrap();
        let slot = rpc.get_slot().await.unwrap();
        assert!(slot > 450_000_000);
        let (bslot, _lamports) = rpc.get_balance(&w).await.unwrap();
        assert!(bslot + 1_000 > slot);
        let sigs = rpc
            .call("getSignaturesForAddress", json!([WALLET, {"limit": 1}]))
            .await
            .unwrap();
        let sig: Signature = sigs[0]["signature"].as_str().unwrap().parse().unwrap();
        let (_, st) = rpc.get_signature_statuses(&[sig], true).await.unwrap();
        assert!(st[0].is_object(), "status {st}");
        let tx = rpc.get_transaction(&sig).await.unwrap().expect("tx found");
        assert!(tx["meta"]["fee"].as_u64().is_some());
        let (gslot, keys) = rpc
            .get_program_account_keys(
                &ids::key(ids::DLMM),
                &[
                    json!({"dataSize": 8120}),
                    json!({"memcmp": {"offset": 8, "bytes": POOL_SOL_USDC}}),
                ],
            )
            .await
            .unwrap();
        assert!(
            gslot > 0 && !keys.is_empty(),
            "{} position keys",
            keys.len()
        );
    }

    /// Captures `tests/fixtures/solana/core/{gma.json,meta.json}` — one GMA
    /// at one slot over both LbPairs, their mints + reserves, the JLP pool,
    /// both custodies and both Jupiter position PDAs of the wallet. Writes
    /// only with `TENGU_CAPTURE_FIXTURES=1` (so `live_` runs never rewrite
    /// pinned fixtures).
    #[tokio::test]
    #[ignore]
    async fn live_capture_fixtures() {
        let rpc = live_rpc();
        let pairs: Vec<Pubkey> = [POOL_SOL_USDC, POOL_2]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let (_, lbs) = rpc.get_multiple_accounts(&pairs, None).await.unwrap();
        let mut keys = pairs.clone();
        let mut labels = serde_json::Map::new();
        for (lb, name) in lbs.iter().zip(["lb_pair_sol_usdc", "lb_pair_2"]) {
            labels.insert(lb.pubkey.to_string(), json!(name));
            let d = lb.data().unwrap();
            assert_eq!(d[..8], LB_PAIR_DISC);
            for (off, role) in [
                (88, "token_x_mint"),
                (120, "token_y_mint"),
                (152, "reserve_x"),
                (184, "reserve_y"),
            ] {
                let k = Pubkey::read(&d, off).unwrap();
                labels
                    .entry(k.to_string())
                    .or_insert(json!(format!("{name}.{role}")));
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        let w: Pubkey = WALLET.parse().unwrap();
        for (k, role) in [
            (ids::key(ids::JLP_POOL), "jlp_pool"),
            (ids::key(ids::JUP_CUSTODY_SOL), "custody_sol"),
            (ids::key(ids::JUP_CUSTODY_USDC), "custody_usdc"),
            (
                crate::domain::solana::jup_position_pda(&w, false),
                "jup_position_short",
            ),
            (
                crate::domain::solana::jup_position_pda(&w, true),
                "jup_position_long",
            ),
        ] {
            labels.insert(k.to_string(), json!(role));
            keys.push(k);
        }
        let ids_json: Vec<String> = keys.iter().map(Pubkey::to_string).collect();
        let result = rpc
            .call(
                "getMultipleAccounts",
                json!([ids_json, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await
            .unwrap();
        let (slot, reads) = parse_gma(&result, &keys).unwrap();
        // Offsets check: reserves are SPL token accounts of the pair mints.
        for lb in &lbs {
            let d = lb.data().unwrap();
            for (mint_off, reserve_off) in [(88, 152), (120, 184)] {
                let mint = Pubkey::read(&d, mint_off).unwrap();
                let reserve = Pubkey::read(&d, reserve_off).unwrap();
                let r = reads.iter().find(|r| r.pubkey == reserve).unwrap();
                assert_eq!(Pubkey::read(&r.data().unwrap(), 0), Some(mint));
            }
        }
        println!("captured {} accounts at slot {slot}", reads.len());
        if std::env::var("TENGU_CAPTURE_FIXTURES").as_deref() == Ok("1") {
            let dir =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solana/core");
            std::fs::create_dir_all(&dir).unwrap();
            let envelope = json!({"jsonrpc": "2.0", "id": 1, "result": result});
            std::fs::write(
                dir.join("gma.json"),
                serde_json::to_string_pretty(&envelope).unwrap() + "\n",
            )
            .unwrap();
            let meta = json!({
                "method": "getMultipleAccounts",
                "params": {"encoding": "base64", "commitment": "confirmed"},
                "endpoint_host": "api.mainnet-beta.solana.com",
                "slot": slot,
                "captured_at": chrono::Utc::now().to_rfc3339(),
                "captured_by": "TENGU_CAPTURE_FIXTURES=1 cargo test --bin tengu live_capture_fixtures -- --ignored",
                "keys": ids_json,
                "labels": labels,
            });
            std::fs::write(
                dir.join("meta.json"),
                serde_json::to_string_pretty(&meta).unwrap() + "\n",
            )
            .unwrap();
        }
    }

    // ── write path (phase 6b) ───────────────────────────────────────

    #[tokio::test]
    async fn write_path_methods_parse_their_results() {
        let t = Arc::new(FakeTransport::at_slot(10));
        let bh = "EkSnNWid2cvwEVnVx9aBqawnmiCNiDgp3gUdkDPTKN1N";
        t.push(Ok(ok_envelope(json!({"context": {"slot": 10},
            "value": {"blockhash": bh, "lastValidBlockHeight": 3090}}))));
        t.push(Ok(ok_envelope(json!(3000))));
        t.push(Ok(ok_envelope(json!({"context": {"slot": 11}, "value": {
            "err": {"InstructionError": [2, {"Custom": 6001}]},
            "logs": ["Program log: a", "Program log: b"], "unitsConsumed": 4242}}))));
        t.push(Ok(ok_envelope(json!({"priorityFeeEstimate": 12345.5}))));
        let rpc = fake_rpc(&t);
        let (hash, lvbh) = rpc.get_latest_blockhash().await.unwrap();
        assert_eq!(Pubkey(hash).to_string(), bh);
        assert_eq!(lvbh, 3090);
        assert_eq!(rpc.get_block_height().await.unwrap(), 3000);
        let sim = rpc.simulate_transaction(&[1, 2, 3]).await.unwrap();
        assert_eq!(sim.slot, 11);
        assert_eq!(sim.units, Some(4242));
        assert_eq!(sim.logs, vec!["Program log: a", "Program log: b"]);
        assert_eq!(sim.err.unwrap()["InstructionError"][1]["Custom"], 6001);
        assert_eq!(rpc.get_priority_fee_estimate(&[1]).await.unwrap(), 12345.5);
        let reqs = t.requests();
        let sim_cfg = &reqs[2]["params"][1];
        assert_eq!(sim_cfg["sigVerify"], false);
        assert_eq!(sim_cfg["replaceRecentBlockhash"], true);
        assert_eq!(reqs[2]["params"][0], "AQID", "base64 of [1,2,3]");
        assert_eq!(
            reqs[3]["params"][0]["options"]["transactionEncoding"],
            "base64"
        );
    }

    #[tokio::test]
    async fn send_is_one_attempt_and_keeps_preflight_logs() {
        let t = Arc::new(FakeTransport::at_slot(10));
        t.push(Ok(json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32002, "message": "Transaction simulation failed: Error processing Instruction 2",
            "data": {"err": {"InstructionError": [2, {"Custom": 1}]}, "logs": ["Program log: boom"]}}})));
        let rpc = fake_rpc(&t);
        let e = rpc.send_transaction_once(&[9]).await.unwrap_err();
        assert_eq!(e.code, Some(-32002));
        assert_eq!(e.data.as_ref().unwrap()["logs"][0], "Program log: boom");
        assert!(
            e.message.starts_with("sendTransaction @ fake.rpc"),
            "{}",
            e.message
        );
        // A transient transport failure is returned, not retried.
        t.push(Err(RpcError::new(ErrorClass::Timeout, "timed out")));
        let e = rpc.send_transaction_once(&[9]).await.unwrap_err();
        assert_eq!(e.class, ErrorClass::Timeout);
        assert_eq!(e.code, None);
        assert_eq!(t.requests().len(), 2, "exactly one request per attempt");
        let cfg = &t.requests()[0]["params"][1];
        assert_eq!(cfg["preflightCommitment"], "confirmed");
        assert_eq!(cfg["skipPreflight"], false);
        let sig = "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
        t.push(Ok(ok_envelope(json!(sig))));
        assert_eq!(
            rpc.send_transaction_once(&[9]).await.unwrap().to_string(),
            sig
        );
    }
}
