//! xmarket tool family — the risk / paper tools over the install's paper
//! ledger (`outbound/paper_store.rs`, `<xm_state_dir>/ledger.db`) and the
//! workspace observation store. Each tool is opt-in (one catalog row per
//! name); interfaces live in [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `risk_status.rs` | `risk_status` — `risk_state/1:<account>`: halt + kill switch, equity / P&L / loss headroom at fresh `mkt_ctx/1` marks, exposure, order rate |
//! | `exec_common.rs` | `run_exec` — the `[risk]` gate inside every exec tool: gate + fill + ledger write in one transaction, `paper_fill/1:<account>:<client_order_id>` |
//! | `paper.rs` | `paper_order`, `paper_close` (exec tools, through `run_exec`), `paper_positions` — `paper_positions/1:<account>` |
//! | `exits.rs` | `xm_exits` (exec tool) — the exit rules: closes every due open position (deadline, max hold, stop-loss, take-profit) through `run_exec` under a deterministic id; `xm_exits/1:<account>` |
//! | `weekend_fade.rs` | `xm_weekend_fade` (exec tool) — rule W: one step of the window's state machine per call; capped fades through the `[risk]` gate, shadow fades through the shadow gate, shadow exits; `xm_weekend/1:<anchor date>` |
//!
//! The plugin opens the observation store (`open_observation_store`), a
//! history reader (`[recorder]` day files: the weekend fade's anchor) and —
//! only with `[risk]` — the ledger (`open_paper_ledger`) once. No store ⇒
//! rows are not cached (fail-soft). No `[risk]` ⇒ every tool refuses
//! `risk_config_missing` (tracker convention 9); no ledger ⇒
//! `state_dir_missing` (no `[xmarket]`) or `ledger_unavailable`. Scope:
//! `fs_roots` = the workspace (store); exec tools also read books:
//! `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]`.
//! The ledger and the kill-switch file sit outside every fs root by design
//! (convention 12, `config/hardening.rs`): they are the tools' own state,
//! never agent paths.

pub(crate) mod defs;
pub(crate) mod exec_common;
pub(crate) mod exits;
pub(crate) mod paper;
pub(crate) mod risk_status;
pub(crate) mod weekend_fade;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::adapters::outbound::history_sqlite::SqliteHistoryStore;
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::paper_store::open_paper_ledger;
use crate::config::risk::{PaperConfig, RiskConfig};
use crate::ports::history::HistoryStore;
use crate::ports::observation::ObservationStore;
use crate::ports::paper::PaperLedger;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};
use weekend_fade::FadeSetup;

pub(crate) use defs::defs_named;

/// Refusal when the sandbox has no `[risk]` (tracker convention 9).
pub(crate) const RISK_CONFIG_MISSING: &str = "risk_config_missing";
/// Refusal when `[risk]` has no ledger dir (no `[xmarket]`).
pub(crate) const STATE_DIR_MISSING: &str = "state_dir_missing";
/// Refusal when the ledger could not be opened.
pub(crate) const LEDGER_UNAVAILABLE: &str = "ledger_unavailable";

/// Handles every family tool shares.
#[derive(Clone)]
pub(crate) struct XmShared {
    /// Observation cache; `None` = rows are not cached.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The paper ledger, or why there is none.
    pub ledger: Result<Arc<dyn PaperLedger>, String>,
    /// `[risk]`, `kill_switch_file` expanded.
    pub risk: Option<RiskConfig>,
    pub paper: Option<PaperConfig>,
    /// The recorder's day files, read-only; `None` = `[recorder]` off.
    pub history: Option<Arc<dyn HistoryStore>>,
    /// `[xmarket.weekend_fade]` + its calendar; `None` = not configured.
    pub fade: Option<Arc<FadeSetup>>,
}

impl XmShared {
    /// `[risk]`, `[paper]` and the ledger — or the refusal every tool gives.
    pub(crate) fn parts(&self) -> Result<(&RiskConfig, &PaperConfig, &Arc<dyn PaperLedger>)> {
        let (Some(risk), Some(paper)) = (&self.risk, &self.paper) else {
            return Err(anyhow!(
                "{RISK_CONFIG_MISSING}: this sandbox has no [risk] + [paper] section"
            ));
        };
        let ledger = self.ledger.as_ref().map_err(|why| anyhow!("{why}"))?;
        Ok((risk, paper, ledger))
    }
}

/// Plugin grouping the xmarket risk / paper tools; several catalog rows
/// share it.
pub(crate) struct XmPlugin;

#[async_trait]
impl ToolPlugin for XmPlugin {
    fn name(&self) -> &'static str {
        "xm"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let sections = &ctx.config.sandbox;
        let store = match open_observation_store(ctx.workspace, sections) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%error, "observation store unavailable; xmarket rows are not cached");
                None
            }
        };
        let ledger = match (&sections.risk, &sections.xm_state_dir) {
            (None, _) => Err(RISK_CONFIG_MISSING.to_string()),
            (Some(_), None) => Err(format!(
                "{STATE_DIR_MISSING}: paper ledger unavailable: no [xmarket] section — add \
                 [xmarket] state = \"<name>\" (ledger <TENGU_HOME>/state/<name>/ledger.db)"
            )),
            (Some(_), Some(_)) => open_paper_ledger(sections)
                .map_err(|e| format!("{LEDGER_UNAVAILABLE}: paper ledger unavailable: {e:#}")),
        };
        let history = sections
            .history_dir
            .as_deref()
            .map(|dir| Arc::new(SqliteHistoryStore::reader(dir)) as Arc<dyn HistoryStore>);
        let fade = sections.weekend_fade.as_ref().map(|config| {
            let calendar = sections
                .calendars
                .get(&config.calendar)
                .and_then(|c| c.exchange())
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "calendar `{}` is not an exchange [xmarket.calendars.<id>] row",
                        config.calendar
                    )
                });
            Arc::new(FadeSetup {
                config: config.clone(),
                calendar,
            })
        });
        let shared = XmShared {
            store,
            ledger,
            risk: sections.risk.clone(),
            paper: sections.paper.clone(),
            history,
            fade,
        };
        let mut tools = risk_status::tools(&shared);
        tools.extend(paper::tools(&shared));
        tools.extend(exits::tools(&shared));
        tools.extend(weekend_fade::tools(&shared));
        Ok(tools)
    }
}
