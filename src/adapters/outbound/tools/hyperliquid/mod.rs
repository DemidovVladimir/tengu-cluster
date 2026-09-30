//! Hyperliquid tool family — typed, cached market reads over `POST /info`
//! (`outbound/hyperliquid/info.rs`: egress gate, `[rate_limits.hyperliquid]`
//! budget, HL error mapping). Decoders are pure (`domain/hl/`); rows use the
//! cross-venue schemas of `domain/market.rs`. Each tool is opt-in (one
//! catalog row per name); interfaces live in [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `ctx.rs` | `hl_ctx` — `mkt_ctx/1` + `mkt_instrument/1` rows for a dex sweep or ≤ 64 coins |
//! | `book.rs` | `hl_book` — `hl_book/1` L2 book, depth, slippage; [`book::fresh_book`] = the paper fill engine's live book |
//!
//! The plugin opens the workspace observation store once
//! (`open_observation_store`: plus the history recorder when `[recorder]` is
//! on); when that fails the tools read live without caching (fail-soft). The
//! paper fee basis (`[paper] fee_tier`, `staking_discount_pct`; tier 0 without
//! `[paper]`) is read here too. Scope: `fs_roots` = the workspace (store),
//! `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]`
//! (testnet override; unset = mainnet).

pub(crate) mod book;
pub(crate) mod ctx;
pub(crate) mod defs;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use crate::adapters::outbound::observations::open_observation_store;
use crate::domain::hl::FeeBasis;
use crate::domain::observation::{CachePolicy, ObsStatus, Observation};
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// `max_age_ms = min(ttl, max_age_secs)` as in `CachePolicy::new`.
pub(crate) fn policy(
    schema: &str,
    subject: &str,
    ttl_ms: u64,
    max_age_secs: Option<u64>,
) -> CachePolicy {
    CachePolicy {
        key: Observation::key_for(schema, subject),
        ttl_ms,
        max_age_ms: max_age_secs.map_or(ttl_ms, |s| ttl_ms.min(s.saturating_mul(1000))),
    }
}

/// Record + store one live row the way `observe()` does (history first;
/// the cache keeps neither `error` nor ttl-0 rows). `true` = stored.
pub(crate) async fn store_live(store: Option<&dyn ObservationStore>, obs: &Observation) -> bool {
    let Some(s) = store else {
        return false;
    };
    if let Err(e) = s.record(obs).await {
        warn!(key = %obs.key, error = %e, "observation history write failed");
    }
    if obs.status == ObsStatus::Error || obs.ttl_ms == 0 {
        return false;
    }
    match s.put(obs).await {
        Ok(stored) => stored,
        Err(e) => {
            warn!(key = %obs.key, error = %e, "observation store write failed");
            false
        }
    }
}

/// An optional non-negative integer argument (absent / `null` ⇒ `None`).
pub(crate) fn opt_u64_arg(args: &Value, tool: &str, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("{tool}: '{key}' must be a non-negative integer, got {v}")),
    }
}

/// Handles every family tool shares.
#[derive(Clone, Default)]
pub(crate) struct HlShared {
    /// Observation cache; `None` = read live, never cache.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The paper account's fee standing (`taker_fee_bps`).
    pub fees: FeeBasis,
}

/// Plugin grouping the Hyperliquid tools; several catalog rows share it.
pub(crate) struct HyperliquidPlugin;

#[async_trait]
impl ToolPlugin for HyperliquidPlugin {
    fn name(&self) -> &'static str {
        "hyperliquid"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let store = match open_observation_store(ctx.workspace, &ctx.config.sandbox) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(%error, "observation store unavailable; Hyperliquid tools read live");
                None
            }
        };
        let fees = ctx
            .config
            .sandbox
            .paper
            .as_ref()
            .map(|p| FeeBasis {
                tier: p.fee_tier,
                staking_discount_pct: p.staking_discount_pct,
            })
            .unwrap_or_default();
        let shared = HlShared { store, fees };
        let mut tools = ctx::tools(&shared);
        tools.extend(book::tools(&shared));
        Ok(tools)
    }
}

/// A local `POST /info` server that answers by request body (tools send
/// several requests, some concurrently). No network.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::adapters::outbound::http_class::test_support::{local_scope, test_client};
    use crate::adapters::outbound::hyperliquid::info::HlInfo;

    /// One route: a request whose body contains every key of `when` with an
    /// equal value (and, with `exact`, no other key) gets `status` + `body`.
    #[derive(Clone)]
    pub(crate) struct Route {
        pub when: Value,
        pub exact: bool,
        pub status: u16,
        pub body: String,
    }

    /// Route matching `when` exactly (no other body keys).
    pub(crate) fn route(when: Value, status: u16, body: impl Into<String>) -> Route {
        Route {
            when,
            exact: true,
            status,
            body: body.into(),
        }
    }

    fn matches(r: &Route, body: &Value) -> bool {
        let (Some(want), Some(got)) = (r.when.as_object(), body.as_object()) else {
            return false;
        };
        want.iter().all(|(k, v)| got.get(k) == Some(v)) && (!r.exact || want.len() == got.len())
    }

    /// Serves until the test ends; unrouted requests get HTTP 599. Returns
    /// the base URL and every request body received.
    pub(crate) async fn serve_info(routes: Vec<Route>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (routes, log) = (routes.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let body = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break None;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let Some(pos) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = text[..pos]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + len {
                            break Some(buf[pos + 4..pos + 4 + len].to_vec());
                        }
                    };
                    let body: Value = body
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or(Value::Null);
                    log.lock().unwrap().push(body.clone());
                    let (status, reply) = routes
                        .iter()
                        .find(|r| matches(r, &body))
                        .map(|r| (r.status, r.body.clone()))
                        .unwrap_or((599, format!("unrouted request {body}")));
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// An unbudgeted client for `base` (no proxy, 127.0.0.1 allowed).
    pub(crate) fn hl(base: &str) -> HlInfo {
        HlInfo::new(test_client(), base, local_scope(), None).unwrap()
    }

    /// Requests seen with info `type` = `t`.
    pub(crate) fn count(seen: &Arc<Mutex<Vec<Value>>>, t: &str) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == t)
            .count()
    }
}
