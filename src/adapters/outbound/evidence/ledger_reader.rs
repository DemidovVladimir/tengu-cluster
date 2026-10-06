//! `SqliteLedgerReader` — `ports::evidence::LedgerSource` over a paper
//! `ledger.db` (`adapters/outbound/paper_store.rs` writes it), opened
//! read-only and immutable ([`super::open_read_only`]) — never through
//! `SqlitePaperLedger`, which adds columns and tables on open. Reads the
//! columns of binary 6fcb455's schema; later ones (`accounts.sandbox`,
//! `orders.fingerprint`, `positions` venue facts) are not needed; the
//! `funding_owed` table is read when it exists (`None` otherwise).

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::{columns, open_read_only};
use crate::domain::xm::grade::{
    AccountRow, CashRow, FillRow, FundingRow, LedgerRows, OrderRow, OwedRow, PositionRow, RiskRow,
};
use crate::ports::evidence::LedgerSource;

pub(crate) struct SqliteLedgerReader {
    path: PathBuf,
}

impl SqliteLedgerReader {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

fn read_rows<T>(
    conn: &Connection,
    sql: &str,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn
        .prepare(sql)
        .with_context(|| format!("prepare `{sql}`"))?;
    let rows = stmt
        .query_map([], f)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

impl LedgerSource for SqliteLedgerReader {
    fn read(&self) -> Result<LedgerRows> {
        let conn = open_read_only(&self.path)?;
        for table in [
            "accounts",
            "cash",
            "orders",
            "fills",
            "funding",
            "positions",
            "risk_decisions",
        ] {
            if columns(&conn, table)?.is_empty() {
                anyhow::bail!(
                    "{}: no table `{table}` — not a paper ledger",
                    self.path.display()
                );
            }
        }
        let accounts = read_rows(
            &conn,
            "SELECT account, initial_cash_usd, created_ms FROM accounts ORDER BY account",
            |r| {
                Ok(AccountRow {
                    account: r.get(0)?,
                    initial_cash_usd: r.get(1)?,
                    created_ms: r.get(2)?,
                })
            },
        )?;
        let cash = read_rows(
            &conn,
            "SELECT id, account, ts_ms, kind, ref, amount_usd, balance_usd FROM cash ORDER BY id",
            |r| {
                Ok(CashRow {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    ts_ms: r.get(2)?,
                    kind: r.get(3)?,
                    reference: r.get(4)?,
                    amount_usd: r.get(5)?,
                    balance_usd: r.get(6)?,
                })
            },
        )?;
        let orders = read_rows(
            &conn,
            "SELECT id, account, client_order_id, call_id, decision_id, ts_ms, instrument, side, kind, \
             reduce_only, status, reason, filled_qty, avg_px, fee_usd FROM orders ORDER BY id",
            |r| {
                Ok(OrderRow {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    client_order_id: r.get(2)?,
                    call_id: r.get(3)?,
                    decision_id: r.get(4)?,
                    ts_ms: r.get(5)?,
                    instrument: r.get(6)?,
                    side: r.get(7)?,
                    kind: r.get(8)?,
                    reduce_only: r.get::<_, i64>(9)? != 0,
                    status: r.get(10)?,
                    reason: r.get(11)?,
                    filled_qty: r.get(12)?,
                    avg_px: r.get(13)?,
                    fee_usd: r.get(14)?,
                })
            },
        )?;
        let fills = read_rows(
            &conn,
            "SELECT id, account, order_id, client_order_id, ts_ms, instrument, side, qty, px, fee_usd, \
             realized_pnl_usd, qty_before, qty_after FROM fills ORDER BY id",
            |r| {
                Ok(FillRow {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    order_id: r.get(2)?,
                    client_order_id: r.get(3)?,
                    ts_ms: r.get(4)?,
                    instrument: r.get(5)?,
                    side: r.get(6)?,
                    qty: r.get(7)?,
                    px: r.get(8)?,
                    fee_usd: r.get(9)?,
                    realized_pnl_usd: r.get(10)?,
                    qty_before: r.get(11)?,
                    qty_after: r.get(12)?,
                })
            },
        )?;
        let funding = read_rows(
            &conn,
            "SELECT account, instrument, hour_ms, rate_1h, oracle_px, qty, payment_usd, ts_ms FROM funding \
             ORDER BY account, instrument, hour_ms",
            |r| {
                Ok(FundingRow {
                    account: r.get(0)?,
                    instrument: r.get(1)?,
                    hour_ms: r.get(2)?,
                    rate_1h: r.get(3)?,
                    oracle_px: r.get(4)?,
                    qty: r.get(5)?,
                    payment_usd: r.get(6)?,
                    ts_ms: r.get(7)?,
                })
            },
        )?;
        let positions = read_rows(
            &conn,
            "SELECT account, instrument, qty, realized_pnl_usd, fees_usd, funding_usd FROM positions \
             ORDER BY account, instrument",
            |r| {
                Ok(PositionRow {
                    account: r.get(0)?,
                    instrument: r.get(1)?,
                    qty: r.get(2)?,
                    realized_pnl_usd: r.get(3)?,
                    fees_usd: r.get(4)?,
                    funding_usd: r.get(5)?,
                })
            },
        )?;
        let tool = if columns(&conn, "risk_decisions")?
            .iter()
            .any(|c| c == "tool")
        {
            "tool"
        } else {
            "NULL"
        };
        let risk_decisions = read_rows(
            &conn,
            &format!(
                "SELECT id, ts_ms, account, client_order_id, instrument, class, allow, rule, {tool} \
                 FROM risk_decisions ORDER BY id"
            ),
            |r| {
                Ok(RiskRow {
                    id: r.get(0)?,
                    ts_ms: r.get(1)?,
                    account: r.get(2)?,
                    client_order_id: r.get(3)?,
                    instrument: r.get(4)?,
                    class: r.get(5)?,
                    allow: r.get::<_, i64>(6)? != 0,
                    rule: r.get(7)?,
                    tool: r.get(8)?,
                })
            },
        )?;
        let funding_owed = if columns(&conn, "funding_owed")?.is_empty() {
            None
        } else {
            Some(read_rows(
                &conn,
                "SELECT account, instrument, hour_ms, qty FROM funding_owed ORDER BY account, instrument, hour_ms",
                |r| {
                    Ok(OwedRow {
                        account: r.get(0)?,
                        instrument: r.get(1)?,
                        hour_ms: r.get(2)?,
                        qty: r.get(3)?,
                    })
                },
            )?)
        };
        Ok(LedgerRows {
            accounts,
            cash,
            orders,
            fills,
            funding,
            positions,
            risk_decisions,
            funding_owed,
        })
    }

    fn describe(&self) -> String {
        let extra = open_read_only(&self.path)
            .and_then(|c| {
                let mut notes = Vec::new();
                for (table, col) in [("accounts", "sandbox"), ("orders", "fingerprint")] {
                    let has = columns(&c, table)?.iter().any(|x| x == col);
                    notes.push(format!("{table}.{col} {}", if has { "yes" } else { "no" }));
                }
                let owed = !columns(&c, "funding_owed")?.is_empty();
                notes.push(format!("funding_owed {}", if owed { "yes" } else { "no" }));
                Ok(notes.join(", "))
            })
            .unwrap_or_else(|e| format!("unreadable: {e}"));
        format!("{} ({extra})", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 6fcb455 schema (no `funding_owed`, no `sandbox`, no `fingerprint`).
    const OLD_SCHEMA: &str = "
CREATE TABLE accounts (account TEXT PRIMARY KEY, initial_cash_usd REAL NOT NULL, created_ms INTEGER NOT NULL);
CREATE TABLE cash (id INTEGER PRIMARY KEY, account TEXT NOT NULL, ts_ms INTEGER NOT NULL, kind TEXT NOT NULL, ref TEXT NOT NULL, amount_usd REAL NOT NULL, balance_usd REAL NOT NULL);
CREATE TABLE positions (account TEXT NOT NULL, instrument TEXT NOT NULL, underlying TEXT NOT NULL, venue TEXT NOT NULL, qty REAL NOT NULL, avg_px REAL, realized_pnl_usd REAL NOT NULL, fees_usd REAL NOT NULL, funding_usd REAL NOT NULL, opened_ms INTEGER, last_funding_hour_ms INTEGER, exit_at_ms INTEGER, updated_ms INTEGER NOT NULL, PRIMARY KEY (account, instrument));
CREATE TABLE orders (id INTEGER PRIMARY KEY, account TEXT NOT NULL, client_order_id TEXT NOT NULL, call_id TEXT, decision_id INTEGER NOT NULL, ts_ms INTEGER NOT NULL, instrument TEXT NOT NULL, underlying TEXT NOT NULL, side TEXT NOT NULL, kind TEXT NOT NULL, reduce_only INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT, filled_qty REAL NOT NULL, avg_px REAL, fee_usd REAL NOT NULL, exit_at_ms INTEGER, result TEXT NOT NULL, UNIQUE (account, client_order_id));
CREATE TABLE fills (id INTEGER PRIMARY KEY, account TEXT NOT NULL, order_id INTEGER NOT NULL, client_order_id TEXT NOT NULL, ts_ms INTEGER NOT NULL, instrument TEXT NOT NULL, underlying TEXT NOT NULL, venue TEXT NOT NULL, side TEXT NOT NULL, qty REAL NOT NULL, px REAL NOT NULL, fee_usd REAL NOT NULL, realized_pnl_usd REAL NOT NULL, qty_before REAL NOT NULL, qty_after REAL NOT NULL);
CREATE TABLE funding (account TEXT NOT NULL, instrument TEXT NOT NULL, hour_ms INTEGER NOT NULL, rate_1h REAL NOT NULL, oracle_px REAL NOT NULL, qty REAL NOT NULL, payment_usd REAL NOT NULL, ts_ms INTEGER NOT NULL, PRIMARY KEY (account, instrument, hour_ms));
CREATE TABLE risk_decisions (id INTEGER PRIMARY KEY, ts_ms INTEGER NOT NULL, account TEXT NOT NULL, client_order_id TEXT NOT NULL, call_id TEXT, instrument TEXT NOT NULL, class TEXT NOT NULL, allow INTEGER NOT NULL, rule TEXT NOT NULL, verdict TEXT NOT NULL, intent TEXT NOT NULL, context TEXT NOT NULL, tool TEXT, session_id TEXT);
INSERT INTO accounts VALUES ('a', 100.0, 1);
INSERT INTO cash VALUES (1, 'a', 1, 'deposit', 'initial', 100.0, 100.0);
INSERT INTO risk_decisions VALUES (1, 5, 'a', 'o1', NULL, 'x:A', 'entry', 1, 'ok', '{}', '{}', '{}', 'paper_order', NULL);
";

    #[test]
    fn reads_the_old_schema_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(OLD_SCHEMA)
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let r = SqliteLedgerReader::new(path.clone());
        let rows = r.read().unwrap();
        assert_eq!(rows.accounts.len(), 1);
        assert_eq!(rows.cash[0].reference, "initial");
        assert_eq!(rows.risk_decisions[0].tool.as_deref(), Some("paper_order"));
        assert!(rows.risk_decisions[0].allow);
        assert!(rows.funding_owed.is_none());
        assert!(r.describe().contains("funding_owed no"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}
