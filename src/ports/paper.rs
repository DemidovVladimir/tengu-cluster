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
//! | [`PaperLedger::place`] | one `BEGIN IMMEDIATE`: an order already stored under the `client_order_id` is returned as is (`replayed`, `decide` not called) — unless its fingerprint differs from the request's (`client_order_id_conflict`, nothing written); else the account and its risk state are re-read, the funding the order's instrument owes is settled at the size held (`PlaceRequest::funding`; review #10 — before the gate and the fill), and `decide(&snapshot)` — synchronous: the gate, then the fill simulation; its one read is the kill-switch file, re-probed for entries (`application/paper.rs`) — returns a [`Decision`]. The verdict row, the funding settled and a changed risk state (the gate's trips, the day roll) are written always; order, fill, position, cash and kept venue facts (`PlaceRequest::facts`) only when the order was `Sent` |
//! | [`PaperLedger::update_risk_state`] | one `BEGIN IMMEDIATE`: `update(&snapshot)` returns the next risk state (the `risk_status` roll + trips, `tengu risk halt / resume`); `Err` writes nothing |
//! | [`PaperLedger::settle_funding`] | one `BEGIN IMMEDIATE`: the HL funding one instrument owes (`domain::xm::ledger::Position::settle_funding`: owed hours booked at the rate, due hours booked or owed), each hour once per (account, instrument) |
//! | [`PaperLedger::trigger_exit`] | one `BEGIN IMMEDIATE`: records a fired stop-loss / take-profit on the open position (`positions.exit_trigger`, review #6); the first one of an opening is kept |
//! | [`PaperLedger::open_account`] | creates the account and its `deposit` cash row once |
//! | reads | [`PaperLedger::snapshot`], [`PaperLedger::order`], [`PaperLedger::stored`] (a replay without the write lock), [`PaperLedger::decisions`], [`PaperLedger::accounts`] |
//!
//! Owners (review #13): a ledger handle opened for a sandbox (the tools':
//! `open_paper_ledger`, owner `SandboxSections::owner`) writes only accounts
//! that sandbox owns — every write above, inside its transaction, claims an
//! account that has no owner yet (new, or stored before owners) and refuses
//! one another sandbox owns ([`ACCOUNT_OWNER_MISMATCH`], nothing written).
//! Reads never check. The operator's handle (`tengu risk`) has no owner: it
//! neither checks nor claims, so a halt always lands.
//!
//! `decide` runs while the ledger's write lock is held: every read (books,
//! ctx rows, the opportunity row, the kill-switch file) happens before
//! `place`, so the closure only computes — but for one stat: an entry
//! re-probes the kill-switch file inside the transaction (review #7).
//!
//! Verdict rows (`risk-audit-verdicts`): each carries the request's
//! `call_id` (joins the decision audit), `tool` and `session_id`; the
//! adapter mirrors every one it writes to `<TENGU_HOME>/logs/risk.jsonl`
//! after the commit (the row stays canonical: `logs/` is pruned, the ledger
//! never).

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::xm::exec::HeldFacts;
use crate::domain::xm::exits::{ExitReason, ExitTrigger};
use crate::domain::xm::ledger::{Fill, FundingRate, FundingSettlement, PaperAccount};
use crate::domain::xm::paper::FillResult;
use crate::domain::xm::risk::{OrderIntent, RiskVerdict};
use crate::domain::xm::risk_state::RiskState;

/// Refusal: the account belongs to another sandbox (module doc: Owners).
pub(crate) const ACCOUNT_OWNER_MISMATCH: &str = "account_owner_mismatch";

/// The account as stored — read inside `place`'s transaction, or by
/// `snapshot`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgerSnapshot {
    pub account: PaperAccount,
    /// The sandbox that owns the account (module doc: Owners); `None` until
    /// a sandbox's handle writes it.
    pub owner: Option<String>,
    /// Exit deadline per open position (full instrument id → ms).
    pub exit_at_ms: BTreeMap<String, i64>,
    /// The fired stop-loss / take-profit of each open position that has one
    /// for its current opening (`trigger_exit`).
    pub exit_triggers: BTreeMap<String, ExitTrigger>,
    /// Venue facts kept with each position (full instrument id; a closed
    /// one keeps its last) — a close falls back to them (review #6).
    pub facts: BTreeMap<String, HeldFacts>,
    /// Entries — orders that are not reduce-only — stored in the 60 s before
    /// `now_ms` (the gate's `orders_last_min`; a denied order stores none,
    /// an exit never counts).
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
    /// The exec tool that placed it (`paper_order`, `xm_exits`, …).
    pub tool: String,
    /// `TENGU_SESSION_ID` of the calling process, when it has one (a
    /// `run-agent` child, its bridge); loop and feed sessions are inside
    /// `call_id`.
    pub session_id: Option<String>,
    /// What the order asks for (`domain::xm::exec::order_fingerprint`),
    /// stored with it: a stored order under the `client_order_id` whose
    /// fingerprint differs is refused (`client_order_id_conflict`), never
    /// replayed. `None` = not checked.
    pub fingerprint: Option<String>,
    /// Full id of the order's instrument (the decision must judge it):
    /// `place` settles the funding its position owes first.
    pub instrument: String,
    /// Its rate from a fresh `mkt_ctx/1` row; `None` ⇒ the hours it owes
    /// are recorded owed, at the size held (review #10).
    pub funding: Option<FundingRate>,
    /// The venue facts the order was priced with from a `mkt_instrument/1`
    /// row: kept with the position when the order is sent (review #6).
    pub facts: Option<HeldFacts>,
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
    /// `PlaceRequest::tool`; `None` on rows written before
    /// `risk-audit-verdicts`.
    pub tool: Option<String>,
    pub session_id: Option<String>,
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
    /// `PlaceRequest::fingerprint`; `None` on orders stored before it.
    pub fingerprint: Option<String>,
}

impl StoredOrder {
    /// Refusal when `fingerprint` asks for something else than this order
    /// did (`None` on either side: not checked — an older row, a caller
    /// without one).
    pub(crate) fn conflict(&self, fingerprint: Option<&str>) -> Option<String> {
        match (self.fingerprint.as_deref(), fingerprint) {
            (Some(stored), Some(asked)) if stored != asked => Some(format!(
                "client_order_id_conflict: order {} of account {} was placed as `{stored}`; \
                 this request is `{asked}` — use a new client_order_id",
                self.client_order_id, self.account
            )),
            _ => None,
        }
    }
    /// The ledger fill this order applied (VWAP, fee); `None` when nothing
    /// filled.
    #[cfg_attr(not(test), allow(dead_code))] // the audit join (`risk-audit-verdicts`)
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
    /// unless it exists; returns the stored account either way. `Err`
    /// [`ACCOUNT_OWNER_MISMATCH`] when another sandbox owns it (module doc:
    /// Owners).
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
    /// The stored order `client_order_id` of `account` (`xm_exits` looks up
    /// its exit attempts).
    async fn order(
        &self,
        account: &str,
        client_order_id: &str,
    ) -> anyhow::Result<Option<StoredOrder>>;
    /// `place`'s replay answer without its write lock: the stored order, the
    /// verdict that allowed it and the account now (`replayed = true`).
    /// `None` = no order under that id — the exec tools check this before
    /// the latency and the book read.
    async fn stored(
        &self,
        account: &str,
        client_order_id: &str,
    ) -> anyhow::Result<Option<Placement>>;
    /// Verdict rows of `account`, newest first, at most `limit`.
    async fn decisions(&self, account: &str, limit: usize) -> anyhow::Result<Vec<StoredDecision>>;
    /// Settle the HL funding `instrument` owes at `now_ms`
    /// (`PaperAccount::settle_funding`): owed hours booked at `rate`, due
    /// hours booked at it — or owed without one —, each hour once per
    /// (account, instrument). What it did; empty = nothing written.
    async fn settle_funding(
        &self,
        account: &str,
        instrument: &str,
        rate: Option<FundingRate>,
        now_ms: i64,
    ) -> anyhow::Result<FundingSettlement>;
    /// Record that `reason` (a stop-loss / take-profit) fired for the open
    /// position of `instrument` opened at `opened_ms`: due until that
    /// opening is closed (`LedgerSnapshot::exit_triggers`). The first
    /// trigger of an opening is kept: the stored one is returned — `None`
    /// when that opening is not open any more.
    async fn trigger_exit(
        &self,
        account: &str,
        instrument: &str,
        opened_ms: i64,
        reason: ExitReason,
        now_ms: i64,
    ) -> anyhow::Result<Option<ExitTrigger>>;
}
