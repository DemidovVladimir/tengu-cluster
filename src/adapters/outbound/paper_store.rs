//! `SqlitePaperLedger` — `ports::paper::PaperLedger` over ONE database per
//! xmarket state dir, `<xm_state_dir>/ledger.db` (tracker convention 3:
//! `<TENGU_HOME>/state/<xmarket.state>/`, outside every workspace and fs
//! root; `tengu prune` spares `state/`). WAL + busy_timeout; `place`,
//! `update_risk_state`, `accrue_funding` and `open_account` each run in one
//! `BEGIN IMMEDIATE` transaction, so two loops or processes never
//! check-then-act on the same account (convention 9). Rows are never
//! purged. No `[xmarket]` ⇒ [`open_paper_ledger`] refuses ⇒ the exec tools
//! refuse.
//!
//! | Table | Key | Row |
//! |---|---|---|
//! | `accounts` | account | initial cash, created |
//! | `cash` | id | journal: `deposit` / `fill` / `funding` movement + the running balance (the latest row is the cash) |
//! | `positions` | (account, instrument) | `domain::xm::ledger::Position` + `exit_at_ms`; flat rows keep their P&L history |
//! | `orders` | id · UNIQUE (account, client_order_id) | one per allowed order: status, reason, fill summary, the engine's `FillResult` (JSON), the id of the verdict that allowed it |
//! | `fills` | id | the ledger fill (VWAP) of a filled / partial order: qty, px, fee, realized P&L, qty before / after |
//! | `funding` | (account, instrument, hour_ms) | one HL funding payment |
//! | `risk_decisions` | id | every verdict: allow, rule, class, the verdict + intent + context digest (JSON), the call id |
//! | `risk_state` | account | `domain::xm::risk_state::RiskState`: halt reason + since, the UTC day + its starting equity (no row = the default) |

// `place` / `accrue_funding` consumers land in wave W1 (`risk-gate-enforcement`,
// `risk-paper-tools`).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::config::sections::SandboxSections;
use crate::domain::observation::{ErrorClass, Field, ReadError};
use crate::domain::xm::ledger::{exit_deadline, Fill, FillEffect, PaperAccount, Position};
use crate::domain::xm::paper::FillResult;
use crate::domain::xm::risk::{Halt, HaltReason};
use crate::domain::xm::risk_state::RiskState;
use crate::ports::paper::{
    Decide, Decision, LedgerSnapshot, Outcome, PaperLedger, PlaceRequest, Placement, RiskUpdate,
    StoredDecision, StoredOrder,
};

/// File name under the xmarket state dir.
pub(crate) const LEDGER_FILE: &str = "ledger.db";

/// The gate's order-rate window.
const ORDER_RATE_WINDOW_MS: i64 = 60_000;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS accounts (
  account TEXT PRIMARY KEY, initial_cash_usd REAL NOT NULL, created_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS cash (
  id INTEGER PRIMARY KEY, account TEXT NOT NULL, ts_ms INTEGER NOT NULL, kind TEXT NOT NULL,
  ref TEXT NOT NULL, amount_usd REAL NOT NULL, balance_usd REAL NOT NULL);
CREATE INDEX IF NOT EXISTS cash_account ON cash(account, id);
CREATE TABLE IF NOT EXISTS positions (
  account TEXT NOT NULL, instrument TEXT NOT NULL, underlying TEXT NOT NULL, venue TEXT NOT NULL,
  qty REAL NOT NULL, avg_px REAL, realized_pnl_usd REAL NOT NULL, fees_usd REAL NOT NULL,
  funding_usd REAL NOT NULL, opened_ms INTEGER, last_funding_hour_ms INTEGER,
  exit_at_ms INTEGER, updated_ms INTEGER NOT NULL, PRIMARY KEY (account, instrument));
CREATE TABLE IF NOT EXISTS orders (
  id INTEGER PRIMARY KEY, account TEXT NOT NULL, client_order_id TEXT NOT NULL, call_id TEXT,
  decision_id INTEGER NOT NULL, ts_ms INTEGER NOT NULL, instrument TEXT NOT NULL,
  underlying TEXT NOT NULL, side TEXT NOT NULL, kind TEXT NOT NULL, reduce_only INTEGER NOT NULL,
  status TEXT NOT NULL, reason TEXT, filled_qty REAL NOT NULL, avg_px REAL,
  fee_usd REAL NOT NULL, exit_at_ms INTEGER, result TEXT NOT NULL,
  UNIQUE (account, client_order_id));
CREATE INDEX IF NOT EXISTS orders_recent ON orders(account, ts_ms);
CREATE TABLE IF NOT EXISTS fills (
  id INTEGER PRIMARY KEY, account TEXT NOT NULL, order_id INTEGER NOT NULL,
  client_order_id TEXT NOT NULL, ts_ms INTEGER NOT NULL, instrument TEXT NOT NULL,
  underlying TEXT NOT NULL, venue TEXT NOT NULL, side TEXT NOT NULL, qty REAL NOT NULL,
  px REAL NOT NULL, fee_usd REAL NOT NULL, realized_pnl_usd REAL NOT NULL,
  qty_before REAL NOT NULL, qty_after REAL NOT NULL);
CREATE TABLE IF NOT EXISTS funding (
  account TEXT NOT NULL, instrument TEXT NOT NULL, hour_ms INTEGER NOT NULL,
  rate_1h REAL NOT NULL, oracle_px REAL NOT NULL, qty REAL NOT NULL,
  payment_usd REAL NOT NULL, ts_ms INTEGER NOT NULL, PRIMARY KEY (account, instrument, hour_ms));
CREATE TABLE IF NOT EXISTS risk_decisions (
  id INTEGER PRIMARY KEY, ts_ms INTEGER NOT NULL, account TEXT NOT NULL,
  client_order_id TEXT NOT NULL, call_id TEXT, instrument TEXT NOT NULL, class TEXT NOT NULL,
  allow INTEGER NOT NULL, rule TEXT NOT NULL, verdict TEXT NOT NULL, intent TEXT NOT NULL,
  context TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS risk_decisions_account ON risk_decisions(account, id);
CREATE INDEX IF NOT EXISTS risk_decisions_call ON risk_decisions(call_id);
CREATE TABLE IF NOT EXISTS risk_state (
  account TEXT PRIMARY KEY, halt_reason TEXT, halt_since_ms INTEGER, day_utc_ms INTEGER,
  day_start_equity_usd REAL, updated_ms INTEGER NOT NULL);";

const UPSERT_POSITION_SQL: &str = "
INSERT INTO positions(account, instrument, underlying, venue, qty, avg_px, realized_pnl_usd,
  fees_usd, funding_usd, opened_ms, last_funding_hour_ms, exit_at_ms, updated_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
ON CONFLICT(account, instrument) DO UPDATE SET underlying = excluded.underlying,
  venue = excluded.venue, qty = excluded.qty, avg_px = excluded.avg_px,
  realized_pnl_usd = excluded.realized_pnl_usd, fees_usd = excluded.fees_usd,
  funding_usd = excluded.funding_usd, opened_ms = excluded.opened_ms,
  last_funding_hour_ms = excluded.last_funding_hour_ms, exit_at_ms = excluded.exit_at_ms,
  updated_ms = excluded.updated_ms";

const ORDER_COLUMNS: &str =
    "id, account, client_order_id, call_id, decision_id, ts_ms, underlying, exit_at_ms, result";

const DECISION_COLUMNS: &str =
    "id, ts_ms, account, client_order_id, call_id, instrument, verdict, intent, context";

/// `<state_dir>/ledger.db`.
pub(crate) fn ledger_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LEDGER_FILE)
}

/// The sandbox's ledger (`[xmarket]` state dir); refused without `[xmarket]`.
pub(crate) fn open_paper_ledger(sections: &SandboxSections) -> Result<Arc<dyn PaperLedger>> {
    let dir = sections.xm_state_dir.as_deref().ok_or_else(|| {
        anyhow!(
            "no [xmarket] section: the paper ledger lives in \
             <TENGU_HOME>/state/<xmarket.state>/{LEDGER_FILE} — add [xmarket] state = \"<name>\""
        )
    })?;
    Ok(Arc::new(SqlitePaperLedger::open(dir)?))
}

/// `[risk] kill_switch_file` present? Any directory entry at the path
/// counts (a dangling symlink too); `Error` = could not tell — the gate and
/// `risk_status` treat it as halted. Checked on every gate call.
pub(crate) fn kill_switch_state(path: &Path) -> Field<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Field::ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Field::ok(false),
        Err(e) => Field::err(ReadError::new(
            "kill_switch",
            ErrorClass::Fatal,
            format!("{}: {e}", path.display()),
        )),
    }
}

#[derive(Clone)]
pub(crate) struct SqlitePaperLedger {
    conn: Arc<Mutex<Connection>>,
}

impl SqlitePaperLedger {
    /// Open (creating) `<state_dir>/ledger.db`.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let path = ledger_path(state_dir);
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|e| anyhow!("paper ledger lock: {e}"))?;
            f(&mut guard)
        })
        .await
        .context("paper ledger task")?
    }
}

// ── reads ─────────────────────────────────────────────────────────

/// The account and the exit deadline of each open position.
fn load_account(c: &Connection, account: &str) -> Result<(PaperAccount, BTreeMap<String, i64>)> {
    let initial: f64 = c
        .query_row(
            "SELECT initial_cash_usd FROM accounts WHERE account = ?1",
            params![account],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("unknown paper account `{account}` — open_account first"))?;
    let cash: f64 = c
        .query_row(
            "SELECT balance_usd FROM cash WHERE account = ?1 ORDER BY id DESC LIMIT 1",
            params![account],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("paper account `{account}` has no cash row"))?;
    let mut stmt = c.prepare(
        "SELECT instrument, underlying, venue, qty, avg_px, realized_pnl_usd, fees_usd,
                funding_usd, opened_ms, last_funding_hour_ms, exit_at_ms
         FROM positions WHERE account = ?1",
    )?;
    let rows = stmt.query_map(params![account], |r| {
        Ok((
            Position {
                instrument: r.get(0)?,
                underlying: r.get(1)?,
                venue: r.get(2)?,
                qty: r.get(3)?,
                avg_px: r.get(4)?,
                realized_pnl: r.get(5)?,
                fees_paid: r.get(6)?,
                funding_paid: r.get(7)?,
                opened_ms: r.get(8)?,
                last_funding_hour_ms: r.get(9)?,
            },
            r.get::<_, Option<i64>>(10)?,
        ))
    })?;
    let mut positions = BTreeMap::new();
    let mut exits = BTreeMap::new();
    for row in rows {
        let (p, exit) = row?;
        if let Some(t) = exit.filter(|_| !p.is_flat()) {
            exits.insert(p.instrument.clone(), t);
        }
        positions.insert(p.instrument.clone(), p);
    }
    let a = PaperAccount {
        account: account.to_string(),
        initial_cash_usd: initial,
        cash_usd: cash,
        positions,
    };
    Ok((a, exits))
}

fn count<P: rusqlite::Params>(c: &Connection, sql: &str, p: P) -> Result<u32> {
    let n: i64 = c.query_row(sql, p, |r| r.get(0))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

fn load_snapshot(c: &Connection, account: &str, now_ms: i64) -> Result<LedgerSnapshot> {
    let (account_now, exit_at_ms) = load_account(c, account)?;
    let orders_last_min = count(
        c,
        "SELECT COUNT(*) FROM orders WHERE account = ?1 AND ts_ms > ?2",
        params![account, now_ms.saturating_sub(ORDER_RATE_WINDOW_MS)],
    )?;
    // No order rests before GTC / ALO (P1); counted so they are once added.
    let open_orders = count(
        c,
        "SELECT COUNT(*) FROM orders WHERE account = ?1 AND status = 'resting'",
        params![account],
    )?;
    Ok(LedgerSnapshot {
        risk: load_risk_state(c, account)?,
        account: account_now,
        exit_at_ms,
        orders_last_min,
        open_orders,
        now_ms,
    })
}

/// The stored risk state; the default when the account has none yet.
fn load_risk_state(c: &Connection, account: &str) -> Result<RiskState> {
    let row = c
        .query_row(
            "SELECT halt_reason, halt_since_ms, day_utc_ms, day_start_equity_usd, updated_ms
             FROM risk_state WHERE account = ?1",
            params![account],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((reason, since, day_utc_ms, day_start_equity_usd, updated_ms)) = row else {
        return Ok(RiskState::default());
    };
    let halt = match (reason, since) {
        (None, _) => None,
        (Some(r), Some(since_ms)) => Some(Halt {
            reason: HaltReason::parse(&r)
                .ok_or_else(|| anyhow!("risk state of `{account}`: unknown halt reason `{r}`"))?,
            since_ms,
        }),
        (Some(r), None) => bail!("risk state of `{account}`: halt `{r}` without a since time"),
    };
    Ok(RiskState {
        halt,
        day_utc_ms,
        day_start_equity_usd,
        updated_ms,
    })
}

fn write_risk_state(c: &Connection, account: &str, s: &RiskState) -> Result<()> {
    c.execute(
        "INSERT INTO risk_state(account, halt_reason, halt_since_ms, day_utc_ms,
           day_start_equity_usd, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account) DO UPDATE SET halt_reason = excluded.halt_reason,
           halt_since_ms = excluded.halt_since_ms, day_utc_ms = excluded.day_utc_ms,
           day_start_equity_usd = excluded.day_start_equity_usd, updated_ms = excluded.updated_ms",
        params![
            account,
            s.halt.as_ref().map(|h| h.reason.as_str()),
            s.halt.as_ref().map(|h| h.since_ms),
            s.day_utc_ms,
            s.day_start_equity_usd,
            s.updated_ms
        ],
    )?;
    Ok(())
}

/// `ORDER_COLUMNS` as read; `result` is still JSON.
type OrderRow = (
    i64,
    String,
    String,
    Option<String>,
    i64,
    i64,
    String,
    Option<i64>,
    String,
);

fn order_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OrderRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn parse_order(row: OrderRow) -> Result<StoredOrder> {
    let (id, account, client_order_id, call_id, decision_id, ts_ms, underlying, exit_at_ms, json) =
        row;
    let result = serde_json::from_str(&json).with_context(|| {
        format!("order {client_order_id} of {account}: stored result does not parse")
    })?;
    Ok(StoredOrder {
        id,
        account,
        client_order_id,
        call_id,
        decision_id,
        ts_ms,
        underlying,
        result,
        exit_at_ms,
    })
}

fn read_order(c: &Connection, account: &str, client_order_id: &str) -> Result<Option<StoredOrder>> {
    let row = c
        .query_row(
            &format!(
                "SELECT {ORDER_COLUMNS} FROM orders WHERE account = ?1 AND client_order_id = ?2"
            ),
            params![account, client_order_id],
            order_row,
        )
        .optional()?;
    row.map(parse_order).transpose()
}

fn read_order_by_id(c: &Connection, id: i64) -> Result<StoredOrder> {
    let row = c.query_row(
        &format!("SELECT {ORDER_COLUMNS} FROM orders WHERE id = ?1"),
        params![id],
        order_row,
    )?;
    parse_order(row)
}

/// `DECISION_COLUMNS` as read; verdict, intent and context are still JSON.
type DecisionRow = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
);

fn decision_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DecisionRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn parse_decision(row: DecisionRow) -> Result<StoredDecision> {
    let (id, ts_ms, account, client_order_id, call_id, instrument, verdict, intent, context) = row;
    let what = |part: &str| format!("risk decision {id}: stored {part} does not parse");
    Ok(StoredDecision {
        id,
        ts_ms,
        account,
        client_order_id,
        call_id,
        instrument,
        verdict: serde_json::from_str(&verdict).with_context(|| what("verdict"))?,
        intent: serde_json::from_str(&intent).with_context(|| what("intent"))?,
        context: serde_json::from_str(&context).with_context(|| what("context"))?,
    })
}

fn read_decision(c: &Connection, id: i64) -> Result<StoredDecision> {
    let row = c.query_row(
        &format!("SELECT {DECISION_COLUMNS} FROM risk_decisions WHERE id = ?1"),
        params![id],
        decision_row,
    )?;
    parse_decision(row)
}

// ── writes ────────────────────────────────────────────────────────

fn append_cash(
    c: &Connection,
    account: &str,
    ts_ms: i64,
    kind: &str,
    reference: &str,
    amount_usd: f64,
    balance_usd: f64,
) -> Result<()> {
    c.execute(
        "INSERT INTO cash(account, ts_ms, kind, ref, amount_usd, balance_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![account, ts_ms, kind, reference, amount_usd, balance_usd],
    )?;
    Ok(())
}

fn write_position(
    c: &Connection,
    account: &str,
    p: &Position,
    exit_at_ms: Option<i64>,
    now_ms: i64,
) -> Result<()> {
    c.execute(
        UPSERT_POSITION_SQL,
        params![
            account,
            p.instrument,
            p.underlying,
            p.venue,
            p.qty,
            p.avg_px,
            p.realized_pnl,
            p.fees_paid,
            p.funding_paid,
            p.opened_ms,
            p.last_funding_hour_ms,
            exit_at_ms,
            now_ms
        ],
    )?;
    Ok(())
}

fn insert_decision(c: &Connection, req: &PlaceRequest, d: &Decision) -> Result<i64> {
    c.execute(
        "INSERT INTO risk_decisions(ts_ms, account, client_order_id, call_id, instrument, class,
           allow, rule, verdict, intent, context)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            req.now_ms,
            req.account,
            req.client_order_id,
            req.call_id,
            d.intent.instrument,
            d.verdict.class.as_str(),
            d.verdict.allow,
            d.verdict.rule,
            serde_json::to_string(&d.verdict)?,
            serde_json::to_string(&d.intent)?,
            serde_json::to_string(&d.context)?,
        ],
    )?;
    Ok(c.last_insert_rowid())
}

fn insert_order(
    c: &Connection,
    req: &PlaceRequest,
    decision_id: i64,
    d: &Decision,
    result: &FillResult,
    exit_at_ms: Option<i64>,
) -> Result<i64> {
    c.execute(
        "INSERT INTO orders(account, client_order_id, call_id, decision_id, ts_ms, instrument,
           underlying, side, kind, reduce_only, status, reason, filled_qty, avg_px, fee_usd,
           exit_at_ms, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            req.account,
            req.client_order_id,
            req.call_id,
            decision_id,
            req.now_ms,
            d.intent.instrument,
            d.intent.underlying,
            result.side.as_str(),
            result.kind.as_str(),
            result.reduce_only,
            result.status.as_str(),
            result.reason.map(|r| r.as_str()),
            result.filled_qty,
            result.avg_px,
            result.fee_usd,
            exit_at_ms,
            serde_json::to_string(result)?,
        ],
    )?;
    Ok(c.last_insert_rowid())
}

fn insert_fill(
    c: &Connection,
    req: &PlaceRequest,
    order_id: i64,
    fill: &Fill,
    effect: &FillEffect,
) -> Result<()> {
    c.execute(
        "INSERT INTO fills(account, order_id, client_order_id, ts_ms, instrument, underlying,
           venue, side, qty, px, fee_usd, realized_pnl_usd, qty_before, qty_after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            req.account,
            order_id,
            req.client_order_id,
            fill.ts_ms,
            fill.instrument,
            fill.underlying,
            fill.venue,
            fill.side.as_str(),
            fill.qty,
            fill.px,
            fill.fee_usd,
            effect.realized_pnl,
            effect.qty_before,
            effect.qty_after
        ],
    )?;
    Ok(())
}

/// A `Decision` the store refuses to write (nothing is written).
fn check_decision(req: &PlaceRequest, d: &Decision) -> Result<()> {
    if d.intent.account != req.account {
        bail!(
            "decision is for account `{}`, the order for `{}`",
            d.intent.account,
            req.account
        );
    }
    match (&d.outcome, d.verdict.allow) {
        (Outcome::Denied, false) => Ok(()),
        (Outcome::Denied, true) => bail!(
            "the gate allowed {} but nothing was sent",
            req.client_order_id
        ),
        (Outcome::Sent { .. }, false) => bail!(
            "the gate denied {} ({}) but an order was sent",
            req.client_order_id,
            d.verdict.rule
        ),
        (Outcome::Sent { result, .. }, true) => {
            let problems = [
                (result.client_order_id != req.client_order_id)
                    .then(|| format!("client_order_id {}", result.client_order_id)),
                (result.instrument.to_string() != d.intent.instrument)
                    .then(|| format!("instrument {}", result.instrument)),
                (result.side != d.intent.side).then(|| format!("side {}", result.side.as_str())),
                (result.reduce_only != d.intent.reduce_only)
                    .then(|| format!("reduce_only {}", result.reduce_only)),
                (![
                    result.filled_qty,
                    result.filled_notional_usd,
                    result.fee_usd,
                ]
                .iter()
                .all(|x| x.is_finite()))
                .then(|| "non-finite fill numbers".to_string()),
            ];
            let problems: Vec<String> = problems.into_iter().flatten().collect();
            if problems.is_empty() {
                Ok(())
            } else {
                bail!(
                    "fill result of {} disagrees with the judged order: {}",
                    req.client_order_id,
                    problems.join(", ")
                )
            }
        }
    }
}

/// The `place` transaction (module table + port docs).
fn place_tx(conn: &mut Connection, req: PlaceRequest, decide: Decide) -> Result<Placement> {
    if req.client_order_id.trim().is_empty() {
        bail!("client_order_id is empty: an order needs an idempotency key");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(order) = read_order(&tx, &req.account, &req.client_order_id)? {
        let decision = read_decision(&tx, order.decision_id)?;
        let (account, _) = load_account(&tx, &req.account)?;
        tx.commit()?;
        return Ok(Placement {
            replayed: true,
            decision,
            order: Some(order),
            account,
        });
    }
    let snapshot = load_snapshot(&tx, &req.account, req.now_ms)?;
    let decision = decide(&snapshot);
    check_decision(&req, &decision)?;
    let decision_id = insert_decision(&tx, &req, &decision)?;
    if let Some(next) = decision.risk.as_ref().filter(|n| **n != snapshot.risk) {
        write_risk_state(&tx, &req.account, next)?;
    }
    let LedgerSnapshot {
        mut account,
        exit_at_ms: exits,
        ..
    } = snapshot;
    let order_id = match &decision.outcome {
        Outcome::Denied => None,
        Outcome::Sent { result, exit_at_ms } => {
            let order_id = insert_order(&tx, &req, decision_id, &decision, result, *exit_at_ms)?;
            if let Some(fill) = result.ledger_fill(&decision.intent.underlying, req.now_ms) {
                let effect = account
                    .apply_fill(&fill)
                    .with_context(|| format!("order {}", req.client_order_id))?;
                let exit =
                    exit_deadline(&effect, exits.get(&fill.instrument).copied(), *exit_at_ms);
                write_position(
                    &tx,
                    &req.account,
                    &account.positions[&fill.instrument],
                    exit,
                    req.now_ms,
                )?;
                append_cash(
                    &tx,
                    &req.account,
                    req.now_ms,
                    "fill",
                    &req.client_order_id,
                    effect.realized_pnl - effect.fee_usd,
                    account.cash_usd,
                )?;
                insert_fill(&tx, &req, order_id, &fill, &effect)?;
            }
            Some(order_id)
        }
    };
    let decision = read_decision(&tx, decision_id)?;
    let order = order_id.map(|id| read_order_by_id(&tx, id)).transpose()?;
    tx.commit()?;
    Ok(Placement {
        replayed: false,
        decision,
        order,
        account,
    })
}

#[async_trait]
impl PaperLedger for SqlitePaperLedger {
    async fn open_account(
        &self,
        account: &str,
        initial_cash_usd: f64,
        now_ms: i64,
    ) -> Result<PaperAccount> {
        PaperAccount::new(account, initial_cash_usd)?;
        let account = account.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created = tx.execute(
                "INSERT INTO accounts(account, initial_cash_usd, created_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(account) DO NOTHING",
                params![account, initial_cash_usd, now_ms],
            )?;
            if created == 1 {
                append_cash(
                    &tx,
                    &account,
                    now_ms,
                    "deposit",
                    "initial",
                    initial_cash_usd,
                    initial_cash_usd,
                )?;
            }
            let (a, _) = load_account(&tx, &account)?;
            tx.commit()?;
            Ok(a)
        })
        .await
    }

    async fn accounts(&self) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT account FROM accounts ORDER BY account")?;
            let names = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(names)
        })
        .await
    }

    async fn snapshot(&self, account: &str, now_ms: i64) -> Result<LedgerSnapshot> {
        let account = account.to_string();
        self.with_conn(move |conn| {
            // One read transaction: every SELECT sees the same commit.
            let tx = conn.transaction()?;
            let s = load_snapshot(&tx, &account, now_ms)?;
            tx.commit()?;
            Ok(s)
        })
        .await
    }

    async fn place(&self, req: PlaceRequest, decide: Decide) -> Result<Placement> {
        self.with_conn(move |conn| place_tx(conn, req, decide))
            .await
    }

    async fn update_risk_state(
        &self,
        account: &str,
        now_ms: i64,
        update: RiskUpdate,
    ) -> Result<LedgerSnapshot> {
        let account = account.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut snapshot = load_snapshot(&tx, &account, now_ms)?;
            let next = update(&snapshot).map_err(|e| anyhow!("{e}"))?;
            if next != snapshot.risk {
                write_risk_state(&tx, &account, &next)?;
                snapshot.risk = next;
            }
            tx.commit()?;
            Ok(snapshot)
        })
        .await
    }

    async fn order(&self, account: &str, client_order_id: &str) -> Result<Option<StoredOrder>> {
        let (account, id) = (account.to_string(), client_order_id.to_string());
        self.with_conn(move |c| read_order(c, &account, &id)).await
    }

    async fn decisions(&self, account: &str, limit: usize) -> Result<Vec<StoredDecision>> {
        let account = account.to_string();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {DECISION_COLUMNS} FROM risk_decisions WHERE account = ?1
                 ORDER BY id DESC LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![account, limit], decision_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(parse_decision).collect()
        })
        .await
    }

    async fn accrue_funding(
        &self,
        account: &str,
        instrument: &str,
        rate_1h: f64,
        oracle_px: f64,
        hour_ms: i64,
        now_ms: i64,
    ) -> Result<Option<f64>> {
        let (account, instrument) = (account.to_string(), instrument.to_string());
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (mut a, exits) = load_account(&tx, &account)?;
            let Some(paid) = a.accrue_funding(&instrument, rate_1h, oracle_px, hour_ms)? else {
                return Ok(None);
            };
            let p = &a.positions[&instrument];
            write_position(&tx, &account, p, exits.get(&instrument).copied(), now_ms)?;
            append_cash(
                &tx,
                &account,
                now_ms,
                "funding",
                &format!("{instrument}@{hour_ms}"),
                -paid,
                a.cash_usd,
            )?;
            tx.execute(
                "INSERT INTO funding(account, instrument, hour_ms, rate_1h, oracle_px, qty,
                   payment_usd, ts_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![account, instrument, hour_ms, rate_1h, oracle_px, p.qty, paid, now_ms],
            )?;
            tx.commit()?;
            Ok(Some(paid))
        })
        .await
    }
}

/// Also the `risk_status` tool's fixtures (`tools/xm/risk_status.rs`).
#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::book::Side;
    use crate::domain::market::InstrumentId;
    use crate::domain::xm::cost::{FeeSchedule, HlKind};
    use crate::domain::xm::ledger::{Mark, HOUR_MS};
    use crate::domain::xm::paper::{
        simulate_fill, FillEnv, FillStatus, MarketStatus as VenueStatus, OrderKind, OrderSize,
        PaperOrder, Tif, VenueRules,
    };
    use crate::domain::xm::risk::{
        evaluate, rules, BookInput, CtxInput, EdgeInput, LegMarket, MarketStatus, MaxAges,
        OrderIntent, RiskContext, RiskLimits,
    };

    pub(crate) const ACCOUNT: &str = "xmarket";
    const SHADOW: &str = "xmarket-shadow";
    pub(crate) const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";
    const TESLA: &str = "company:tesla";
    const OPP: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";
    /// Venue time of the fixture book (2026-09-30T13:35:52.605Z).
    pub(crate) const BOOK_TS: i64 = 1_790_775_352_605;
    /// Orders are placed 400 ms after it.
    pub(crate) const NOW: i64 = BOOK_TS + 400;
    /// Fixture mid: (347.16 + 347.23) / 2.
    pub(crate) const MID: f64 = 347.195;

    /// The $100 budget (tracker § 7 #3) with a $40 gross cap, so a second
    /// $25 entry breaches it.
    pub(crate) fn limits() -> RiskLimits {
        RiskLimits {
            account: ACCOUNT.into(),
            venues: vec!["hyperliquid".into()],
            min_lifecycle: None,
            instruments_allow: vec![TSLA.into()],
            instruments_deny: vec![],
            max_order_notional_usd: 25.0,
            max_position_notional_usd: 50.0,
            max_asset_exposure_usd: 50.0,
            max_venue_exposure_usd: 100.0,
            max_gross_exposure_usd: 40.0,
            max_net_exposure_usd: 100.0,
            max_leverage: 1.0,
            daily_loss_limit_usd: 10.0,
            total_loss_limit_usd: 25.0,
            min_edge_bps: 10.0,
            max_slippage_bps: 30.0,
            min_depth_usd: 250.0,
            require_hedge_for: vec![],
            max_data_age_ms: MaxAges {
                book: 5_000,
                ctx: 20_000,
                reference: 60_000,
                quote: 20_000,
            },
            max_skew_ms: 5_000,
            max_orders_per_min: 6,
            max_open_orders: 4,
            allow_reduce_degraded: true,
        }
    }

    /// One order: the intent the gate judges and the order the engine fills.
    pub(crate) struct Order {
        coid: String,
        account: String,
        instrument: String,
        side: Side,
        size: OrderSize,
        reduce_only: bool,
        exit_at_ms: Option<i64>,
        /// The kill-switch file is present.
        kill_switch: bool,
    }

    /// A $25 market buy of `instrument` on `account`.
    pub(crate) fn buy(coid: &str, account: &str, instrument: &str) -> Order {
        Order {
            coid: coid.into(),
            account: account.into(),
            instrument: instrument.into(),
            side: Side::Buy,
            size: OrderSize::NotionalUsd(25.0),
            reduce_only: false,
            exit_at_ms: None,
            kill_switch: false,
        }
    }

    /// A reduce-only market sell of `qty` TSLA.
    fn close(coid: &str, qty: f64) -> Order {
        Order {
            side: Side::Sell,
            size: OrderSize::Qty(qty),
            reduce_only: true,
            ..buy(coid, ACCOUNT, TSLA)
        }
    }

    pub(crate) fn req(o: &Order, now_ms: i64) -> PlaceRequest {
        PlaceRequest {
            account: o.account.clone(),
            client_order_id: o.coid.clone(),
            call_id: Some(format!("call-{}", o.coid)),
            now_ms,
        }
    }

    /// What `risk-gate-enforcement` runs inside `place`: value the snapshot
    /// at the fixture mid with the day rolled, gate the intent against fresh
    /// fixture rows, fill against the fixture book, record the trips. `seen`
    /// gets the snapshot.
    pub(crate) fn gate(
        o: &Order,
        limits: RiskLimits,
        seen: Option<Arc<Mutex<Vec<LedgerSnapshot>>>>,
    ) -> Decide {
        let (qty, notional) = match o.size {
            OrderSize::Qty(q) => (q, q * MID),
            OrderSize::NotionalUsd(n) => (n / MID, n),
        };
        let intent = OrderIntent {
            account: o.account.clone(),
            instrument: o.instrument.clone(),
            underlying: TESLA.into(),
            side: o.side,
            qty,
            notional_usd: notional,
            reduce_only: o.reduce_only,
            strategy: Some("overreaction".into()),
            hedge_instrument: None,
            opportunity_key: (!o.reduce_only).then(|| OPP.to_string()),
        };
        let order = PaperOrder {
            client_order_id: o.coid.clone(),
            instrument: InstrumentId::parse(&o.instrument).unwrap(),
            side: o.side,
            size: o.size,
            kind: OrderKind::Market,
            tif: Tif::Ioc,
            limit_px: None,
            reduce_only: o.reduce_only,
            max_slippage_bps: 30.0,
            ref_mid: None,
        };
        let (exit_at_ms, kill) = (o.exit_at_ms, o.kill_switch);
        Box::new(move |snap: &LedgerSnapshot| {
            if let Some(seen) = seen {
                seen.lock().unwrap().push(snap.clone());
            }
            let now = snap.now_ms;
            let marks: BTreeMap<String, Field<Mark>> = snap
                .account
                .positions
                .keys()
                .map(|id| {
                    let m = Mark {
                        px: MID,
                        at_ms: now - 1_000,
                    };
                    (id.clone(), Field::ok(m))
                })
                .collect();
            let (valued, rolled) = snap.risk.value(&snap.account, &marks, now, 20_000);
            let book = tsla_book();
            let leg = LegMarket {
                book: Field::ok(BookInput {
                    key: format!("hl_book/1:{TSLA}"),
                    observed_at_ms: now - 100,
                    book: book.clone(),
                }),
                ctx: Field::ok(CtxInput {
                    key: format!("mkt_ctx/1:{TSLA}"),
                    observed_at_ms: now - 1_000,
                    status: MarketStatus::Open,
                    oi_cap_headroom_usd: None,
                }),
            };
            let ctx = RiskContext {
                account: valued,
                halt: rolled.effective_halt(now).cloned(),
                kill_switch: Field::ok(kill),
                lifecycle: Field::Absent,
                orders_last_min: snap.orders_last_min,
                open_orders: snap.open_orders,
                legs: BTreeMap::from([(TSLA.to_string(), leg)]),
                opportunity: Field::ok(EdgeInput {
                    key: OPP.into(),
                    observed_at_ms: now - 1_000,
                    ttl_ms: 5_000,
                    edge_after_costs_bps: Field::ok(12.0),
                }),
            };
            let verdict = evaluate(&intent, &ctx, &limits, now);
            let outcome = if verdict.allow {
                let position = snap
                    .account
                    .positions
                    .get(&intent.instrument)
                    .cloned()
                    .unwrap_or_else(|| Position::flat(&intent.instrument, TESLA, "hyperliquid"));
                let rules = VenueRules::hyperliquid(HlKind::Perp, 3, Some(false));
                let fees = FeeSchedule::new(0.9, 0.3).unwrap();
                let env = FillEnv {
                    rules: &rules,
                    status: VenueStatus::Open,
                    position: &position,
                    fees: &fees,
                    max_book_age_ms: 5_000,
                };
                let age = (now - book.venue_ts_ms) as u64;
                Outcome::Sent {
                    result: simulate_fill(&order, &book, age, &env),
                    exit_at_ms,
                }
            } else {
                Outcome::Denied
            };
            let next = rolled.trip(&verdict.trips, now);
            Decision {
                intent,
                verdict,
                context: ctx.digest(now),
                outcome,
                risk: (next != snap.risk).then_some(next),
            }
        })
    }

    pub(crate) async fn ledger(dir: &Path) -> SqlitePaperLedger {
        let l = SqlitePaperLedger::open(dir).unwrap();
        l.open_account(ACCOUNT, 100.0, NOW - 60_000).await.unwrap();
        l
    }

    /// Rows per table, for "what did this write".
    fn rows(dir: &Path) -> BTreeMap<&'static str, i64> {
        let c = Connection::open(ledger_path(dir)).unwrap();
        [
            "accounts",
            "cash",
            "positions",
            "orders",
            "fills",
            "funding",
            "risk_decisions",
            "risk_state",
        ]
        .into_iter()
        .map(|t| {
            let n: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap();
            (t, n)
        })
        .collect()
    }

    fn close_to(a: f64, b: f64, what: &str) {
        assert!(
            (a - b).abs() <= 1e-9 * b.abs().max(1.0),
            "{what}: {a} vs {b}"
        );
    }

    #[tokio::test]
    async fn an_allowed_order_writes_verdict_order_fill_position_and_cash() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let mut o = buy("o-1", ACCOUNT, TSLA);
        o.exit_at_ms = Some(NOW + HOUR_MS);
        let p = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        assert!(!p.replayed);
        let v = &p.decision.verdict;
        assert!(v.allow, "{:?}", v.failed());
        assert_eq!(
            (
                p.decision.call_id.as_deref(),
                p.decision.instrument.as_str()
            ),
            (Some("call-o-1"), TSLA)
        );
        assert_eq!(
            p.decision.intent["instrument"], TSLA,
            "full id in the intent"
        );
        // $25 at mid 347.195 → 0.072 (szDecimals 3), taken at the ask 347.23.
        let order = p.order.unwrap();
        assert_eq!(order.result.status, FillStatus::Filled);
        assert_eq!(order.result.filled_qty, 0.072);
        assert_eq!(order.decision_id, p.decision.id);
        assert_eq!(order.exit_at_ms, Some(NOW + HOUR_MS));
        let fill = order.fill().unwrap();
        assert_eq!((fill.instrument.as_str(), fill.px), (TSLA, 347.23));
        let pos = &p.account.positions[TSLA];
        assert_eq!(
            (pos.qty, pos.avg_px, pos.opened_ms),
            (0.072, Some(347.23), Some(NOW))
        );
        close_to(
            p.account.cash_usd,
            100.0 - order.result.fee_usd,
            "cash after the fee",
        );
        let r = rows(dir.path());
        for (t, n) in [
            ("accounts", 1),
            ("cash", 2),
            ("positions", 1),
            ("orders", 1),
            ("fills", 1),
            ("risk_decisions", 1),
            ("risk_state", 1),
        ] {
            assert_eq!(r[t], n, "{t}");
        }
        // Reads agree with what place returned.
        let s = l.snapshot(ACCOUNT, NOW + 1_000).await.unwrap();
        assert_eq!(s.account, p.account);
        assert_eq!(
            s.exit_at_ms,
            BTreeMap::from([(TSLA.to_string(), NOW + HOUR_MS)])
        );
        assert_eq!((s.orders_last_min, s.open_orders), (1, 0));
        assert_eq!(l.order(ACCOUNT, "o-1").await.unwrap(), Some(order));
        assert_eq!(l.decisions(ACCOUNT, 10).await.unwrap(), vec![p.decision]);
    }

    /// A retry with the same `client_order_id` returns the stored result:
    /// `decide` never runs, nothing is written.
    #[tokio::test]
    async fn a_retry_replays_the_stored_order() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let o = buy("xm_entry:session-1:3", ACCOUNT, TSLA);
        let first = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        let before = rows(dir.path());
        let again = l
            .place(
                req(&o, NOW + 5_000),
                Box::new(|_| panic!("decide must not run on a replay")),
            )
            .await
            .unwrap();
        assert!(again.replayed);
        assert_eq!(again.order, first.order);
        assert_eq!(again.decision, first.decision);
        assert_eq!(again.account, first.account);
        assert_eq!(rows(dir.path()), before, "a replay writes nothing");
    }

    /// A deny writes exactly one verdict row — no order, fill, position or
    /// cash — and a retry of a denied id is judged again.
    #[tokio::test]
    async fn a_deny_writes_one_verdict_row_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let o = buy("o-nvda", ACCOUNT, NVDA);
        let p = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        assert!(!p.decision.verdict.allow);
        assert_eq!(p.decision.verdict.rule, rules::INSTRUMENT);
        assert!(p.order.is_none());
        assert!(
            p.decision.context["legs"].get(TSLA).is_some(),
            "digest stored"
        );
        let r = rows(dir.path());
        assert_eq!(
            (
                r["risk_decisions"],
                r["orders"],
                r["fills"],
                r["positions"],
                r["cash"]
            ),
            (1, 0, 0, 0, 1)
        );
        assert_eq!(r["risk_state"], 1, "the UTC day roll is kept");
        assert_eq!(l.order(ACCOUNT, "o-nvda").await.unwrap(), None);
        let again = l
            .place(req(&o, NOW + 1), gate(&o, limits(), None))
            .await
            .unwrap();
        assert!(!again.replayed && !again.decision.verdict.allow);
        assert_eq!(rows(dir.path())["risk_decisions"], 2);
        let d = l.decisions(ACCOUNT, 10).await.unwrap();
        assert_eq!(d.len(), 2);
        assert!(d[0].id > d[1].id, "newest first");
    }

    /// Two connections (two processes): the second order waits for the
    /// first's transaction and is gated against its exposure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_second_of_two_racing_orders_sees_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = ledger(dir.path()).await;
        let b = SqlitePaperLedger::open(dir.path()).unwrap();
        let (inside, entered) = tokio::sync::oneshot::channel::<()>();
        let first = buy("race-a", ACCOUNT, TSLA);
        let mut gate_a = Some(gate(&first, limits(), None));
        let slow: Decide = Box::new(move |snap| {
            inside.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            (gate_a.take().unwrap())(snap)
        });
        let task_a = tokio::spawn(async move { a.place(req(&first, NOW), slow).await });
        entered.await.unwrap();
        let second = buy("race-b", ACCOUNT, TSLA);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let pb = b
            .place(
                req(&second, NOW + 1),
                gate(&second, limits(), Some(seen.clone())),
            )
            .await
            .unwrap();
        let pa = task_a.await.unwrap().unwrap();
        assert!(pa.decision.verdict.allow);
        let snap = &seen.lock().unwrap()[0];
        assert_eq!(
            snap.account.positions[TSLA].qty, 0.072,
            "saw the first fill"
        );
        assert_eq!(snap.orders_last_min, 1);
        // 0.144 × 347.195 = 49.99 gross > the 40 cap.
        assert!(!pb.decision.verdict.allow);
        assert_eq!(pb.decision.verdict.rule, rules::GROSS_EXPOSURE);
        assert_eq!(pb.account.positions[TSLA].qty, 0.072);
    }

    /// Everything survives a reopen; a retry after the reopen still replays.
    #[tokio::test]
    async fn the_ledger_persists_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let before = {
            let l = ledger(dir.path()).await;
            let mut o = buy("o-1", ACCOUNT, TSLA);
            o.exit_at_ms = Some(NOW + HOUR_MS);
            l.place(req(&o, NOW), gate(&o, limits(), None))
                .await
                .unwrap();
            // Close it again: realized P&L, flat row kept, deadline cleared.
            let c = close("o-2", 0.072);
            let p = l
                .place(req(&c, NOW + 1_000), gate(&c, limits(), None))
                .await
                .unwrap();
            assert!(
                p.decision.verdict.allow,
                "{:?}",
                p.decision.verdict.failed()
            );
            assert_eq!(p.decision.verdict.class.as_str(), "exit");
            l.snapshot(ACCOUNT, NOW + 2_000).await.unwrap()
        };
        let pos = &before.account.positions[TSLA];
        assert_eq!((pos.qty, pos.avg_px, pos.opened_ms), (0.0, None, None));
        // Bought at the ask 347.23, sold at the bid 347.16.
        close_to(pos.realized_pnl, 0.072 * (347.16 - 347.23), "realized");
        assert!(before.exit_at_ms.is_empty());
        close_to(
            before.account.cash_usd,
            100.0 + pos.realized_pnl - pos.fees_paid,
            "cash reconciles",
        );
        let l = SqlitePaperLedger::open(dir.path()).unwrap();
        assert_eq!(l.snapshot(ACCOUNT, NOW + 2_000).await.unwrap(), before);
        assert_eq!(l.accounts().await.unwrap(), vec![ACCOUNT.to_string()]);
        let o = buy("o-1", ACCOUNT, TSLA);
        let replay = l
            .place(req(&o, NOW + 3_000), Box::new(|_| panic!("replayed")))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.order.unwrap().result.filled_qty, 0.072);
        // The reopened account is not re-funded.
        let again = l.open_account(ACCOUNT, 500.0, NOW).await.unwrap();
        assert_eq!(again.initial_cash_usd, 100.0);
        assert_eq!(rows(dir.path())["cash"], 3, "deposit + two fills");
    }

    /// The weekend run's capped and shadow accounts share one ledger.
    #[tokio::test]
    async fn accounts_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        l.open_account(SHADOW, 1_000_000.0, NOW).await.unwrap();
        let mut shadow_limits = limits();
        shadow_limits.account = SHADOW.into();
        let o = buy("s-1", SHADOW, TSLA);
        let p = l
            .place(req(&o, NOW), gate(&o, shadow_limits, None))
            .await
            .unwrap();
        assert!(p.decision.verdict.allow);
        // The same client_order_id on the other account is a new order.
        let o = buy("s-1", ACCOUNT, TSLA);
        let q = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        assert!(!q.replayed && q.decision.verdict.allow);
        let capped = l.snapshot(ACCOUNT, NOW).await.unwrap();
        assert_eq!(
            (capped.account.positions[TSLA].qty, capped.orders_last_min),
            (0.072, 1)
        );
        assert_eq!(
            l.accounts().await.unwrap(),
            vec![ACCOUNT.to_string(), SHADOW.to_string()]
        );
        assert!(l.snapshot("nobody", NOW).await.is_err());
        let o = buy("x-1", "nobody", TSLA);
        let e = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("unknown paper account `nobody`"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn orders_count_toward_the_rate_window_for_60_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let mut limits = limits();
        limits.max_gross_exposure_usd = 100.0;
        for (i, t) in [NOW, NOW + 1_000].into_iter().enumerate() {
            let o = buy(&format!("r-{i}"), ACCOUNT, TSLA);
            let p = l
                .place(req(&o, t), gate(&o, limits.clone(), None))
                .await
                .unwrap();
            assert!(
                p.decision.verdict.allow,
                "{:?}",
                p.decision.verdict.failed()
            );
        }
        for (at, n) in [(NOW + 2_000, 2), (NOW + 60_500, 1), (NOW + 61_000, 0)] {
            assert_eq!(
                l.snapshot(ACCOUNT, at).await.unwrap().orders_last_min,
                n,
                "{at}"
            );
        }
    }

    /// Funding books once per (account, instrument, hour) and moves cash.
    #[tokio::test]
    async fn funding_is_booked_once_per_hour() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let o = buy("o-1", ACCOUNT, TSLA);
        let p = l
            .place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        let hour = NOW - NOW.rem_euclid(HOUR_MS) + HOUR_MS;
        // Long 0.072, oracle 350, +0.0001 / h ⇒ the long pays 0.00252.
        let paid = l
            .accrue_funding(ACCOUNT, TSLA, 0.0001, 350.0, hour, hour + 5)
            .await
            .unwrap();
        close_to(paid.unwrap(), 0.072 * 350.0 * 0.0001, "payment");
        for h in [hour, hour - HOUR_MS] {
            let again = l
                .accrue_funding(ACCOUNT, TSLA, 0.0001, 350.0, h, h + 9)
                .await;
            assert_eq!(again.unwrap(), None, "{h}");
        }
        assert_eq!(
            l.accrue_funding(ACCOUNT, NVDA, 0.0001, 1.0, hour, hour)
                .await
                .unwrap(),
            None
        );
        assert!(l
            .accrue_funding(ACCOUNT, TSLA, 0.0001, 350.0, hour + 1, hour)
            .await
            .is_err());
        let s = l.snapshot(ACCOUNT, hour + 10).await.unwrap();
        let pos = &s.account.positions[TSLA];
        assert_eq!(pos.last_funding_hour_ms, Some(hour));
        close_to(pos.funding_paid, paid.unwrap(), "funding on the position");
        close_to(
            s.account.cash_usd,
            p.account.cash_usd - paid.unwrap(),
            "cash",
        );
        let r = rows(dir.path());
        assert_eq!((r["funding"], r["cash"]), (1, 3));
    }

    /// A `Decision` that contradicts itself or the request writes nothing.
    #[tokio::test]
    async fn contradictory_decisions_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let before = rows(dir.path());
        let o = buy("o-1", ACCOUNT, TSLA);
        type Twist = fn(&mut Decision);
        let twists: [(Twist, &str); 4] = [
            (|d| d.outcome = Outcome::Denied, "nothing was sent"),
            (
                |d| d.intent.account = SHADOW.into(),
                "decision is for account",
            ),
            (
                |d| {
                    if let Outcome::Sent { result, .. } = &mut d.outcome {
                        result.client_order_id = "other".into();
                    }
                },
                "client_order_id other",
            ),
            (
                |d| {
                    d.verdict.allow = false;
                },
                "but an order was sent",
            ),
        ];
        for (twist, want) in twists {
            let inner = gate(&o, limits(), None);
            let decide: Decide = Box::new(move |s| {
                let mut d = inner(s);
                twist(&mut d);
                d
            });
            let e = l.place(req(&o, NOW), decide).await.unwrap_err();
            assert!(format!("{e:#}").contains(want), "{want}: {e:#}");
            assert_eq!(rows(dir.path()), before, "{want}");
        }
        let mut empty = buy("", ACCOUNT, TSLA);
        empty.coid = "  ".into();
        let e = l
            .place(req(&empty, NOW), gate(&empty, limits(), None))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("client_order_id is empty"), "{e}");
    }

    #[test]
    fn no_xmarket_state_dir_means_no_ledger() {
        let e = open_paper_ledger(&SandboxSections::default())
            .err()
            .unwrap();
        assert!(e.to_string().contains("no [xmarket] section"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(dir.path().join("xmarket")),
            ..Default::default()
        };
        assert!(open_paper_ledger(&sections).is_ok());
        assert!(ledger_path(&dir.path().join("xmarket")).exists());
    }

    #[test]
    fn the_kill_switch_file_is_probed_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let kill = dir.path().join("KILL");
        assert_eq!(kill_switch_state(&kill), Field::ok(false));
        std::fs::write(&kill, "").unwrap();
        assert_eq!(kill_switch_state(&kill), Field::ok(true));
        std::fs::remove_file(&kill).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("gone"), &kill).unwrap();
            assert_eq!(
                kill_switch_state(&kill),
                Field::ok(true),
                "a dangling link counts"
            );
        }
        // Under a file (not a directory): the probe cannot tell.
        let blocked = dir.path().join("plain");
        std::fs::write(&blocked, "").unwrap();
        let e = kill_switch_state(&blocked.join("KILL"));
        assert!(e.is_error(), "{e:?}");
    }

    /// The kill-switch file denies entries and records a sticky `file` halt
    /// in the same transaction; with the file gone the halt stays until a
    /// resume, and a reduce-only exit passes throughout.
    #[tokio::test]
    async fn a_kill_switch_deny_records_a_sticky_halt() {
        let dir = tempfile::tempdir().unwrap();
        let l = ledger(dir.path()).await;
        let o = buy("o-1", ACCOUNT, TSLA);
        l.place(req(&o, NOW), gate(&o, limits(), None))
            .await
            .unwrap();
        let mut k = buy("o-2", ACCOUNT, TSLA);
        k.kill_switch = true;
        let p = l
            .place(req(&k, NOW + 100), gate(&k, limits(), None))
            .await
            .unwrap();
        assert_eq!(p.decision.verdict.rule, rules::KILL_SWITCH);
        let file = Some(Halt {
            reason: HaltReason::File,
            since_ms: NOW + 100,
        });
        let s = l.snapshot(ACCOUNT, NOW + 200).await.unwrap();
        assert_eq!(s.risk.halt, file);
        let o3 = buy("o-3", ACCOUNT, TSLA);
        let p = l
            .place(req(&o3, NOW + 300), gate(&o3, limits(), None))
            .await
            .unwrap();
        assert_eq!(
            p.decision.verdict.rule,
            rules::HALTED,
            "file gone, halt kept"
        );
        let c = close("o-4", 0.072);
        let p = l
            .place(req(&c, NOW + 400), gate(&c, limits(), None))
            .await
            .unwrap();
        assert_eq!(
            (p.decision.verdict.allow, p.decision.verdict.rule.as_str()),
            (true, rules::ALLOW_REDUCE_DEGRADED)
        );
        let refused = l
            .update_risk_state(
                ACCOUNT,
                NOW + 500,
                Box::new(|s| s.risk.resume(NOW + 500, true)),
            )
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("kill-switch file is present"),
            "{refused}"
        );
        let s = l.snapshot(ACCOUNT, NOW + 500).await.unwrap();
        assert_eq!(s.risk.halt, file);
        let after = l
            .update_risk_state(
                ACCOUNT,
                NOW + 600,
                Box::new(|s| s.risk.resume(NOW + 600, false)),
            )
            .await
            .unwrap();
        assert!(after.risk.halt.is_none());
        let o5 = buy("o-5", ACCOUNT, TSLA);
        let p = l
            .place(req(&o5, NOW + 700), gate(&o5, limits(), None))
            .await
            .unwrap();
        assert!(
            p.decision.verdict.allow,
            "{:?}",
            p.decision.verdict.failed()
        );
    }

    /// `update_risk_state` is a read-modify-write; a refused update writes
    /// nothing; the state survives a reopen and gates the next order.
    #[tokio::test]
    async fn risk_state_updates_persist_and_gate() {
        let dir = tempfile::tempdir().unwrap();
        let operator = Some(Halt {
            reason: HaltReason::Operator,
            since_ms: NOW,
        });
        {
            let l = ledger(dir.path()).await;
            let s = l.snapshot(ACCOUNT, NOW).await.unwrap();
            assert_eq!(s.risk, RiskState::default());
            let s = l
                .update_risk_state(ACCOUNT, NOW, Box::new(|s| Ok(s.risk.halt_operator(NOW))))
                .await
                .unwrap();
            assert_eq!(s.risk.halt, operator);
            let e = l
                .update_risk_state(ACCOUNT, NOW + 1, Box::new(|_| Err("refused".into())))
                .await
                .unwrap_err();
            assert_eq!(e.to_string(), "refused");
            let unknown = l
                .update_risk_state("nobody", NOW, Box::new(|s| Ok(s.risk.clone())))
                .await;
            assert!(unknown.is_err());
        }
        let l = SqlitePaperLedger::open(dir.path()).unwrap();
        let s = l.snapshot(ACCOUNT, NOW + 5).await.unwrap();
        assert_eq!(
            (s.risk.halt.clone(), s.risk.updated_ms),
            (operator.clone(), NOW)
        );
        let o = buy("o-1", ACCOUNT, TSLA);
        let p = l
            .place(req(&o, NOW + 10), gate(&o, limits(), None))
            .await
            .unwrap();
        assert_eq!(p.decision.verdict.rule, rules::HALTED);
        let s = l.snapshot(ACCOUNT, NOW + 20).await.unwrap();
        assert_eq!(s.risk.halt, operator, "the gate call kept the halt");
        assert_eq!(
            s.risk.day_start_equity_usd,
            Some(100.0),
            "and rolled the day"
        );
        assert_eq!(rows(dir.path())["risk_state"], 1);
    }
}
