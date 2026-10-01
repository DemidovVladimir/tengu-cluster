//! xlab tool family — history-first research over the sandbox's market-data
//! warehouse (`outbound/market_data.rs`, `<xm_state_dir>/market.db`;
//! `docs/xlab-2026-10-01.md` § 8). Read-only: these tools cannot move money.
//! Each tool is opt-in (one catalog row per name); interfaces live in
//! [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `history.rs` | `market_history` — `mkt_history/1:<instrument>:<interval>`: stats, a bar sample and the coverage of one instrument in a window; `fetch = true` backfills the missing part first (`outbound/backfill/`) |
//! | `run.rs` | `backtest` — `backtest/1:<run id>`: a `[backtest.strategies]` name or an inline spec through `application/backtest/` (rules arms; no Jev gate, no network), the run dir `<state dir>/backtests/<run id>/` |
//!
//! The plugin opens the market-data store once (`open_market_data`) and the
//! workspace observation store (`open_observation_store`: rows are recorded
//! when `[recorder]` takes them; ttl 0, never cached). No `[xmarket]` ⇒
//! every tool refuses `state_dir_missing`; a store that does not open ⇒
//! `market_data_unavailable`. Scope: `fs_roots` = the workspace (the
//! observation store); a fetch checks every request against `net_hosts`
//! (`api.hyperliquid.xyz`, `api.geckoterminal.com`) and reads `HL_API_URL` /
//! `GECKO_API_URL` only through `env_reads`. `market.db` and `backtests/`
//! sit in the state dir, outside every fs root by design (like `ledger.db`):
//! the tools' own state, never an agent path. Arguments parse strictly with
//! the helpers below (an unknown key is an error; times are epoch ms, RFC
//! 3339 or a UTC date).

pub(crate) mod defs;
pub(crate) mod history;
pub(crate) mod run;

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::warn;

use crate::adapters::outbound::market_data::open_market_data;
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::tools::xm::STATE_DIR_MISSING;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::MARKET_DB;
use crate::domain::marketdata::parse_time;
use crate::ports::market_data::MarketDataStore;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Refusal when `market.db` could not be opened.
pub(crate) const MARKET_DATA_UNAVAILABLE: &str = "market_data_unavailable";

/// Handles every family tool shares.
#[derive(Clone)]
pub(crate) struct XlabShared {
    /// The market-data warehouse, or the refusal every tool gives.
    pub market: Result<Arc<dyn MarketDataStore>, String>,
    /// Observation store (records rows); `None` = nothing recorded.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The sandbox's sections (`[rate_limits.*]`, `[backtest]`).
    pub sandbox: Arc<SandboxSections>,
}

impl XlabShared {
    /// The warehouse, or the refusal (module doc).
    pub(crate) fn market(&self) -> Result<&dyn MarketDataStore> {
        match &self.market {
            Ok(m) => Ok(m.as_ref()),
            Err(why) => Err(anyhow!("{why}")),
        }
    }

    /// [`market`](Self::market), shared (the backtest use case holds it).
    pub(crate) fn market_arc(&self) -> Result<Arc<dyn MarketDataStore>> {
        match &self.market {
            Ok(m) => Ok(Arc::clone(m)),
            Err(why) => Err(anyhow!("{why}")),
        }
    }
}

// ── Strict arguments, shared by the family ─────────────────────────────

/// `args` as an object whose every key is in `allowed`.
pub(crate) fn object_args<'a>(
    tool: &str,
    args: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>> {
    let o = args
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: arguments must be a JSON object"))?;
    let unknown: Vec<&str> = o
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "{tool}: unknown argument(s) {unknown:?} (allowed: {})",
            allowed.join(", ")
        );
    }
    Ok(o)
}

/// The argument `key`, absent when missing or `null`.
pub(crate) fn field<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

/// An optional non-empty string argument, trimmed.
pub(crate) fn opt_str<'a>(
    tool: &str,
    o: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim())),
        Some(v) => bail!("{tool}: '{key}' must be a non-empty string, got {v}"),
    }
}

/// A whole number from a JSON integer or an integral float.
pub(crate) fn whole(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|x| x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15)
            .map(|x| x as i64)
    })
}

/// A time argument (`from` / `to`): a string `parse_time` takes (RFC 3339,
/// a UTC date, epoch ms) or an epoch-ms number.
pub(crate) fn opt_time(tool: &str, o: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) => parse_time(s)
            .map(Some)
            .map_err(|e| anyhow!("{tool}: '{key}': {e}")),
        Some(v) => whole(v).map(Some).ok_or_else(|| {
            anyhow!("{tool}: '{key}' must be epoch ms, RFC 3339 or a date (2026-09-25), got {v}")
        }),
    }
}

/// The warehouse of `sections`, or the refusal string (module doc).
pub(crate) fn open_market(sections: &SandboxSections) -> Result<Arc<dyn MarketDataStore>, String> {
    if sections.xm_state_dir.is_none() {
        return Err(format!(
            "{STATE_DIR_MISSING}: market data unavailable: no [xmarket] section — add [xmarket] \
             state = \"<name>\" (the warehouse is <TENGU_HOME>/state/<name>/{MARKET_DB})"
        ));
    }
    open_market_data(sections).map_err(|e| format!("{MARKET_DATA_UNAVAILABLE}: {e:#}"))
}

/// Plugin grouping the xlab tools; several catalog rows share it.
pub(crate) struct XlabPlugin;

#[async_trait]
impl ToolPlugin for XlabPlugin {
    fn name(&self) -> &'static str {
        "xlab"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let sections = &ctx.config.sandbox;
        let store = match open_observation_store(ctx.workspace, sections) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%error, "observation store unavailable; xlab rows are not recorded");
                None
            }
        };
        let shared = XlabShared {
            market: open_market(sections),
            store,
            sandbox: Arc::clone(sections),
        };
        let mut tools = history::tools(&shared);
        tools.extend(run::tools(&shared));
        Ok(tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warehouse_needs_xmarket() {
        let e = open_market(&SandboxSections::default()).err().unwrap();
        assert!(
            e.starts_with("state_dir_missing: market data unavailable: no [xmarket] section"),
            "{e}"
        );
        let dir = tempfile::tempdir().unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(dir.path().join("xlab")),
            ..Default::default()
        };
        assert!(open_market(&sections).is_ok());
        assert!(dir.path().join("xlab").join(MARKET_DB).exists());
        // A state dir that cannot be created: the store's own error.
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(blocked.join("xlab")),
            ..Default::default()
        };
        let e = open_market(&sections).err().unwrap();
        assert!(e.starts_with("market_data_unavailable: "), "{e}");
    }
}
