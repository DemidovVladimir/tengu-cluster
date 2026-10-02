//! `risk_status` — the `[risk]` account's `risk_state/1:<account>` row
//! (`domain/xm/risk_state.rs`).
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[risk]` ⇒ `risk_config_missing`; no ledger ⇒ `state_dir_missing` (no `[xmarket]`) / `ledger_unavailable` |
//! | Account | `[risk] account`, created with `[paper] initial_cash_usd` on first use |
//! | Marks | the open positions' `mkt_ctx/1:<id>` rows in the workspace store — never fetched: `mark` at the row's time; missing, failed or older than `[risk] max_data_age_ms.ctx` ⇒ that position's numbers are omitted (`partial`), never 0 |
//! | Kill switch | `kill_switch_file` present ⇒ a `file` halt; unreadable ⇒ an error field, reported halted |
//! | Write | one ledger transaction: roll the UTC day, record the halts the losses or the file call for (`RiskState::value` + `valuation_trips` + `trip` — the transitions a gate call applies) |
//! | Row | `risk_state/1:<account>`, stored 2 s so loops read it through `world` |

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::{defs, XmShared};
use crate::adapters::outbound::paper_store::kill_switch_state;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::domain::market::MarketCtx;
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, ErrorClass, Field, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::tools as names;
use crate::domain::xm::ledger::{Mark, PaperAccount};
use crate::domain::xm::risk_state::{valuation_trips, RiskStatus};
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// How long a `risk_state/1` row stays fresh in the store.
pub(crate) const RISK_STATE_TTL_MS: u64 = 2_000;

pub(crate) fn tools(shared: &XmShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(RiskStatusTool {
        def: defs::def(names::RISK_STATUS),
        shared: shared.clone(),
    })]
}

pub(crate) struct RiskStatusTool {
    def: ToolDef,
    shared: XmShared,
}

#[async_trait]
impl Tool for RiskStatusTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let now = now_ms();
        let obs = read_status(&self.shared, now).await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

/// The module table at `now_ms`: the stored `risk_state/1:<account>` row.
pub(crate) async fn read_status(shared: &XmShared, now_ms: i64) -> Result<Observation> {
    let (risk, paper, ledger) = shared.parts()?;
    let account = risk.account.clone();
    ledger
        .open_account(&account, paper.initial_cash_usd, now_ms)
        .await?;
    let before = ledger.snapshot(&account, now_ms).await?;
    let marks = read_marks(shared.store.as_deref(), &before.account).await;
    let kill = kill_switch_state(&risk.kill_switch_file);
    let limits = risk.limits();
    let max_age = risk.max_data_age_ms.ctx;
    let (m, k, l) = (marks.clone(), kill.clone(), limits.clone());
    let after = ledger
        .update_risk_state(
            &account,
            now_ms,
            Box::new(move |s| {
                let (valued, rolled) = s.risk.value(&s.account, &m, now_ms, max_age);
                Ok(rolled.trip(&valuation_trips(&valued, &k, &l), now_ms))
            }),
        )
        .await?;
    let (valued, _) = after.risk.value(&after.account, &marks, now_ms, max_age);
    let status = RiskStatus::new(
        valued,
        after.risk.clone(),
        kill,
        &limits,
        after.orders_last_min,
        after.open_orders,
        now_ms,
    );
    let obs = Observation::of(
        names::RISK_STATUS,
        &status,
        now_ms,
        RISK_STATE_TTL_MS,
        ObsSource::Live,
    );
    store_live(shared.store.as_deref(), &obs).await;
    Ok(obs)
}

/// The open positions' marks from their `mkt_ctx/1` rows (module table);
/// no store ⇒ every mark failed.
pub(crate) async fn read_marks(
    store: Option<&dyn ObservationStore>,
    account: &PaperAccount,
) -> BTreeMap<String, Field<Mark>> {
    let ids: Vec<String> = account
        .open_positions()
        .map(|p| p.instrument.clone())
        .collect();
    if ids.is_empty() {
        return BTreeMap::new();
    }
    let keys: Vec<String> = ids
        .iter()
        .map(|id| Observation::key_for(MarketCtx::SCHEMA, id))
        .collect();
    let failed = |class, message: String| {
        let e = ReadError::new("mkt_ctx", class, message);
        ids.iter()
            .map(|id| (id.clone(), Field::err(e.clone())))
            .collect()
    };
    let Some(store) = store else {
        return failed(
            ErrorClass::NotApplicable,
            "no observation store".to_string(),
        );
    };
    match store.get_many(&keys).await {
        Ok(rows) => ids
            .iter()
            .zip(rows)
            .map(|(id, row)| (id.clone(), row.as_ref().map_or(Field::Absent, mark_of)))
            .collect(),
        Err(e) => failed(ErrorClass::Transient, format!("{e:#}")),
    }
}

/// `mark` of a `mkt_ctx/1` row, at the row's time.
pub(crate) fn mark_of(row: &Observation) -> Field<Mark> {
    if row.status == ObsStatus::Error {
        return Field::err(row.errors.first().cloned().unwrap_or_else(|| {
            ReadError::new(
                "mkt_ctx",
                ErrorClass::Transient,
                format!("row {} has status error", row.key),
            )
        }));
    }
    match row.typed::<MarketCtx>() {
        Ok(ctx) => match ctx.mark {
            Field::Ok { value } => Field::ok(Mark {
                px: value,
                at_ms: row.observed_at_ms,
            }),
            Field::Absent => Field::Absent,
            Field::Error { error } => Field::err(error),
        },
        Err(e) => Field::err(ReadError::new("mkt_ctx", ErrorClass::Decode, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::outbound::paper_store::tests::{
        buy, gate, ledger, limits, req, ACCOUNT, NOW, TSLA,
    };
    use crate::adapters::outbound::paper_store::SqlitePaperLedger;
    use crate::application::observe::tests::MemStore;
    use crate::config::risk::{PaperConfig, RiskConfig};
    use crate::domain::market::InstrumentId;
    use crate::domain::xm::risk::{Halt, HaltReason};
    use crate::ports::paper::PaperLedger;

    fn risk(kill: &Path) -> RiskConfig {
        let toml = format!(
            r#"
account = "xmarket"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["hyperliquid:xyz:TSLA"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 1
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = ["convergence"]
max_skew_ms = 5000
max_orders_per_min = 6
max_open_orders = 4
kill_switch_file = "{}"
allow_reduce_degraded = true
exits = {{ take_profit_bps = 200, stop_loss_bps = 100, max_hold_secs = 86400 }}
max_data_age_ms = {{ book = 5000, ctx = 20000, reference = 60000, quote = 20000 }}
"#,
            kill.display()
        );
        toml::from_str::<RiskConfig>(&toml).unwrap().resolved()
    }

    fn paper() -> PaperConfig {
        toml::from_str(
            "initial_cash_usd = 100\nlatency_ms = 250\nlatency_jitter_ms = 100\nfee_tier = 0\n\
             staking_discount_pct = 0\norder_types = [\"market\", \"ioc\"]\n",
        )
        .unwrap()
    }

    /// A fresh `mkt_ctx/1:hyperliquid:xyz:TSLA` row marking `px` at `at_ms`.
    fn ctx_row(px: f64, at_ms: i64) -> Observation {
        let mut ctx = MarketCtx::new(InstrumentId::parse(TSLA).unwrap(), at_ms);
        ctx.mark = Field::ok(px);
        Observation::of(names::HL_CTX, &ctx, at_ms, 5_000, ObsSource::Live)
    }

    /// A ledger with one filled $25 TSLA buy (0.072 at 347.23) placed at
    /// `NOW` — the gate call rolled the day at equity 100.
    async fn with_position(dir: &Path) -> Arc<SqlitePaperLedger> {
        let l = ledger(&dir.join("state")).await;
        let o = buy("o-1", ACCOUNT, TSLA);
        let p = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        assert!(p.decision.verdict.allow);
        Arc::new(l)
    }

    fn shared(
        store: Option<Arc<MemStore>>,
        ledger: Arc<SqlitePaperLedger>,
        dir: &Path,
    ) -> XmShared {
        XmShared {
            store: store.map(|s| s as Arc<dyn ObservationStore>),
            ledger: Ok(ledger as Arc<dyn PaperLedger>),
            risk: Some(risk(&dir.join("KILL"))),
            paper: Some(paper()),
            history: None,
            fade: None,
        }
    }

    #[tokio::test]
    async fn a_position_at_a_fresh_mark_values_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let l = with_position(dir.path()).await;
        let store = Arc::new(MemStore::default());
        store.put(&ctx_row(350.0, NOW + 100)).await.unwrap();
        let s = shared(Some(store.clone()), l.clone(), dir.path());
        let obs = read_status(&s, NOW + 500).await.unwrap();
        assert_eq!(obs.key, format!("risk_state/1:{ACCOUNT}"));
        assert_eq!(obs.status, ObsStatus::Ok, "{:?}", obs.errors);
        let st: RiskStatus = obs.typed().unwrap();
        let fee = 0.072 * 347.23 * 0.9e-4;
        let equity = 100.0 - fee + 0.072 * (350.0 - 347.23);
        let f = &obs.features;
        for (k, v) in [
            ("equity_usd", equity),
            ("daily_pnl_usd", equity - 100.0),
            ("total_pnl_usd", equity - 100.0),
            ("gross_exposure_usd", 0.072 * 350.0),
            ("day_start_equity_usd", 100.0),
        ] {
            let got = f[k].as_f64().unwrap();
            assert!((got - v).abs() < 1e-9, "{k}: {got} vs {v}");
        }
        assert_eq!(
            (f["halted"].clone(), f["orders_last_min"].clone()),
            (false.into(), 1.into())
        );
        assert!(st.positions.positions[0].instrument == TSLA);
        // Stored for `world`, 2 s.
        let stored = store.get(&obs.key).await.unwrap().unwrap();
        assert_eq!(
            (stored.ttl_ms, stored.status),
            (RISK_STATE_TTL_MS, ObsStatus::Ok)
        );
        assert!(obs.render_text(NOW + 500).contains(TSLA), "full id in data");
    }

    /// No mark ⇒ partial, numbers omitted; the kill-switch file ⇒ halted
    /// `file`, recorded in the ledger.
    #[tokio::test]
    async fn a_missing_mark_is_partial_and_the_kill_switch_file_halts() {
        let dir = tempfile::tempdir().unwrap();
        let l = with_position(dir.path()).await;
        let store = Arc::new(MemStore::default());
        let s = shared(Some(store), l.clone(), dir.path());
        let obs = read_status(&s, NOW + 500).await.unwrap();
        assert_eq!(obs.status, ObsStatus::Partial);
        for k in ["equity_usd", "daily_pnl_usd", "loss_headroom_usd"] {
            assert!(!obs.features.contains_key(k), "{k} omitted, never 0");
        }
        assert!(obs.errors.iter().any(|e| e.field == format!("mark:{TSLA}")));
        std::fs::write(dir.path().join("KILL"), "").unwrap();
        let obs = read_status(&s, NOW + 600).await.unwrap();
        assert_eq!(
            (
                obs.features["halted"].clone(),
                obs.features["reason"].clone()
            ),
            (true.into(), "file".into())
        );
        let snap = l.snapshot(ACCOUNT, NOW + 700).await.unwrap();
        assert_eq!(
            snap.risk.halt,
            Some(Halt {
                reason: HaltReason::File,
                since_ms: NOW + 600
            })
        );
        // Without a store the marks fail the same way.
        let blind = shared(None, l, dir.path());
        let obs = read_status(&blind, NOW + 800).await.unwrap();
        assert_eq!(obs.status, ObsStatus::Partial);
    }

    #[tokio::test]
    async fn the_account_is_opened_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let l = Arc::new(SqlitePaperLedger::open(dir.path()).unwrap());
        let s = shared(None, l.clone(), dir.path());
        let obs = read_status(&s, NOW).await.unwrap();
        assert_eq!(obs.status, ObsStatus::Ok, "{:?}", obs.errors);
        assert!(obs
            .headline
            .starts_with("risk account=xmarket halt=none equity=100.00 daily_pnl=0.00"));
        assert_eq!(l.accounts().await.unwrap(), vec![ACCOUNT.to_string()]);
    }

    #[tokio::test]
    async fn refused_without_risk_or_a_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let l = Arc::new(SqlitePaperLedger::open(dir.path()).unwrap());
        let mut s = shared(None, l, dir.path());
        s.ledger = Err("state_dir_missing: paper ledger unavailable: no [xmarket] section".into());
        let e = read_status(&s, NOW).await.unwrap_err();
        assert!(
            e.to_string()
                .starts_with("state_dir_missing: paper ledger unavailable: no [xmarket]"),
            "{e}"
        );
        s.risk = None;
        let e = read_status(&s, NOW).await.unwrap_err();
        assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
    }

    #[test]
    fn marks_come_from_ctx_rows_at_their_time() {
        let row = ctx_row(350.0, NOW);
        assert_eq!(
            mark_of(&row),
            Field::ok(Mark {
                px: 350.0,
                at_ms: NOW
            })
        );
        let mut no_mark = MarketCtx::new(InstrumentId::parse(TSLA).unwrap(), NOW);
        no_mark.mark = Field::Absent;
        let row = Observation::of(names::HL_CTX, &no_mark, NOW, 5_000, ObsSource::Live);
        assert_eq!(mark_of(&row), Field::Absent);
        let mut bad = ctx_row(350.0, NOW);
        bad.schema = "hl_book/1".into();
        assert!(mark_of(&bad).is_error());
    }
}
