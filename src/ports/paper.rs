//! `PaperLedger` — the paper accounts of an xmarket install
//! (`risk-paper-ledger-store`, tracker conventions 3 + 9): cash, positions,
//! orders, fills, funding and every gate verdict; several accounts per
//! ledger (the weekend run keeps a capped and a shadow account). Impl:
//! `adapters/outbound/paper_store.rs` (`<xm_state_dir>/ledger.db`, outside
//! every workspace and fs root); no `[xmarket]` ⇒ no ledger ⇒ the exec tools
//! refuse.
//!
//! | Call | Transaction |
//! |---|---|
//! | [`PaperLedger::place`] | one `BEGIN IMMEDIATE`: an order already stored under the `client_order_id` is returned as is (`replayed`, `decide` not called); else the account and its risk state are re-read and `decide(&snapshot)` — synchronous and pure: the gate, then the fill simulation — returns a [`Decision`]. The verdict row and a changed risk state (the gate's trips, the day roll) are written always; order, fill, position and cash rows only when the order was `Sent` |
//! | [`PaperLedger::update_risk_state`] | one `BEGIN IMMEDIATE`: `update(&snapshot)` returns the next risk state (the `risk_status` roll + trips, `tengu risk halt / resume`); `Err` writes nothing |
//! | [`PaperLedger::accrue_funding`] | one `BEGIN IMMEDIATE`: the HL funding of one hour boundary, once per (account, instrument, hour) |
//! | [`PaperLedger::open_account`] | creates the account and its `deposit` cash row once |
//! | reads | [`PaperLedger::snapshot`], [`PaperLedger::order`], [`PaperLedger::decisions`], [`PaperLedger::accounts`] |
//!
//! `decide` runs while the ledger's write lock is held: every read (books,
//! ctx rows, the opportunity row, the kill-switch file) happens before
//! `place`, so the closure only computes.

// Consumers land in wave W1 (`risk-gate-enforcement`, `risk-paper-tools`).
#![allow(dead_code)]

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::xm::ledger::{Fill, PaperAccount};
use crate::domain::xm::paper::FillResult;
use crate::domain::xm::risk::{OrderIntent, RiskVerdict};
use crate::domain::xm::risk_state::RiskState;

/// The account as stored — read inside `place`'s transaction, or by
/// `snapshot`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgerSnapshot {
    pub account: PaperAccount,
    /// Exit deadline per open position (full instrument id → ms).
    pub exit_at_ms: BTreeMap<String, i64>,
    /// Orders stored in the 60 s before `now_ms` (the gate's
    /// `orders_last_min`; a denied order stores none).
    pub orders_last_min: u32,
    /// Resting orders (the gate's `open_orders`): none until GTC / ALO (P1).
    pub open_orders: u32,
    /// Halt + day-start equity as stored (the default before the first
    /// write) — roll it with `RiskState::value` before gating.
    pub risk: RiskState,
    pub now_ms: i64,
}

/// What became of an order the gate judged.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// Denied: only the verdict row is written.
    Denied,
    /// Allowed and sent to the paper venue: the fill engine's result —
    /// `filled`, `partial` or `rejected` (a venue rejection is still an
    /// order under its `client_order_id`).
    Sent {
        result: FillResult,
        /// Deadline of the position this order opens (exit rules, the
        /// weekend fade); merged by `domain::xm::ledger::exit_deadline`.
        exit_at_ms: Option<i64>,
    },
}

/// `decide`'s answer: the gate's verdict and what was sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Decision {
    pub intent: OrderIntent,
    pub verdict: RiskVerdict,
    /// `RiskContext::digest` — the row keys, ages and values the gate read.
    pub context: Value,
    /// `Sent` exactly when `verdict.allow`.
    pub outcome: Outcome,
    /// The account's next risk state (the rolled day, the verdict's trips);
    /// `None` = unchanged.
    pub risk: Option<RiskState>,
}

/// The gate + fill step `place` runs inside its transaction.
pub(crate) type Decide = Box<dyn FnOnce(&LedgerSnapshot) -> Decision + Send>;

/// A risk-state change `update_risk_state` runs inside its transaction:
/// the next state, or why it is refused.
pub(crate) type RiskUpdate = Box<dyn FnOnce(&LedgerSnapshot) -> Result<RiskState, String> + Send>;

/// One `place` call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlaceRequest {
    pub account: String,
    /// Idempotency key: the tool's `client_order_id` arg, else
    /// `ToolCtx.call_id` — never a random id (a retry must deduplicate).
    pub client_order_id: String,
    /// `ToolCtx.call_id`: joins the verdict to the decision audit.
    pub call_id: Option<String>,
    pub now_ms: i64,
}

/// A verdict row (`risk_decisions`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredDecision {
    pub id: i64,
    pub ts_ms: i64,
    pub account: String,
    pub client_order_id: String,
    pub call_id: Option<String>,
    pub instrument: String,
    pub verdict: RiskVerdict,
    /// The `OrderIntent` as judged (JSON: a malformed intent stays readable).
    pub intent: Value,
    pub context: Value,
}

/// An order the gate allowed (`orders`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredOrder {
    pub id: i64,
    pub account: String,
    pub client_order_id: String,
    pub call_id: Option<String>,
    /// The `risk_decisions` row that allowed it.
    pub decision_id: i64,
    pub ts_ms: i64,
    /// Full underlying id (`company:tesla`).
    pub underlying: String,
    pub result: FillResult,
    pub exit_at_ms: Option<i64>,
}

impl StoredOrder {
    /// The ledger fill this order applied (VWAP, fee); `None` when nothing
    /// filled.
    pub(crate) fn fill(&self) -> Option<Fill> {
        self.result.ledger_fill(&self.underlying, self.ts_ms)
    }
}

/// `place`'s answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placement {
    /// The `client_order_id` was stored before: nothing was evaluated or
    /// written.
    pub replayed: bool,
    /// This call's verdict row — on a replay, the one that allowed the order.
    pub decision: StoredDecision,
    /// The stored order; `None` when denied.
    pub order: Option<StoredOrder>,
    /// The account after the transaction.
    pub account: PaperAccount,
}

#[async_trait]
pub(crate) trait PaperLedger: Send + Sync {
    /// Create `account` with `initial_cash_usd` (and its `deposit` cash row)
    /// unless it exists; returns the stored account either way.
    async fn open_account(
        &self,
        account: &str,
        initial_cash_usd: f64,
        now_ms: i64,
    ) -> anyhow::Result<PaperAccount>;
    /// Every account, by name.
    async fn accounts(&self) -> anyhow::Result<Vec<String>>;
    /// The account now, without writing. `Err` for an unknown account.
    async fn snapshot(&self, account: &str, now_ms: i64) -> anyhow::Result<LedgerSnapshot>;
    /// Gate + fill + write in one transaction (module table). `Err` =
    /// nothing written: an unknown account or empty `client_order_id`, a
    /// store failure, or a `Decision` that contradicts itself or the request.
    async fn place(&self, req: PlaceRequest, decide: Decide) -> anyhow::Result<Placement>;
    /// Read-modify-write of `account`'s risk state in one transaction; the
    /// snapshot after it. `Err` (a refused update, an unknown account) =
    /// nothing written.
    async fn update_risk_state(
        &self,
        account: &str,
        now_ms: i64,
        update: RiskUpdate,
    ) -> anyhow::Result<LedgerSnapshot>;
    /// The stored order `client_order_id` of `account`.
    async fn order(
        &self,
        account: &str,
        client_order_id: &str,
    ) -> anyhow::Result<Option<StoredOrder>>;
    /// Verdict rows of `account`, newest first, at most `limit`.
    async fn decisions(&self, account: &str, limit: usize) -> anyhow::Result<Vec<StoredDecision>>;
    /// Book the HL funding of hour boundary `hour_ms` for `instrument`
    /// (`PaperAccount::accrue_funding` rules), once per (account,
    /// instrument, hour): the payment, or `Ok(None)` when nothing is due.
    /// Book every due hour, in order, before a later fill.
    async fn accrue_funding(
        &self,
        account: &str,
        instrument: &str,
        rate_1h: f64,
        oracle_px: f64,
        hour_ms: i64,
        now_ms: i64,
    ) -> anyhow::Result<Option<f64>>;
}
