//! The xmarket family's tool interface — names, descriptions and JSON input
//! schemas in one place; tool files take their `ToolDef` from [`def`], the
//! catalog rows advertise [`defs_named`]. Schemas stay in the subset every
//! engine accepts (`tools/schema_lint.rs`).

use serde_json::json;

use crate::config::risk::STRATEGIES;
use crate::domain::message::ToolDef;
use crate::domain::tools as names;

/// Every xmarket tool definition, in catalog order.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![
        risk_status(),
        paper_order(),
        paper_close(),
        xm_exits(),
        paper_positions(),
    ]
}

/// The definition named `name` as a one-element vec (catalog rows); empty
/// for an unknown name (`catalog_tests` catch that).
pub(crate) fn defs_named(name: &str) -> Vec<ToolDef> {
    tool_defs().into_iter().filter(|d| d.name == name).collect()
}

/// The definition of one family tool. `name` is a `domain::tools` constant.
pub(crate) fn def(name: &str) -> ToolDef {
    defs_named(name)
        .pop()
        .unwrap_or_else(|| panic!("no xmarket tool definition named {name}"))
}

fn risk_status() -> ToolDef {
    ToolDef::new(
        names::RISK_STATUS,
        "Risk state of the [risk] paper account: halted or not and why (daily_loss, total_loss, \
         operator, kill-switch file), equity, daily and total P&L, loss headroom under the daily \
         and total loss limits, gross / net exposure, leverage, orders in the last minute. Marks \
         come from fresh mkt_ctx/1 rows in the store (read hl_ctx first); a missing or stale \
         mark leaves those numbers out (partial), never 0. Each call also rolls the UTC day and \
         records the halts the losses or the kill-switch file call for. Typed observation \
         risk_state/1:<account>, stored 2 s. No arguments.",
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false,
        }),
    )
}

fn slippage() -> serde_json::Value {
    json!({
        "type": "number",
        "description": "Worst fill vs the book mid in bps (> 0, < 10000): the IOC price bound. The [risk] gate also caps the walk at its own max_slippage_bps.",
    })
}

fn client_order_id() -> serde_json::Value {
    json!({
        "type": "string",
        "description": "Idempotency key (1-256 chars, no spaces). Default: this call's id. Retrying with the same id returns the stored result and places nothing.",
    })
}

fn paper_order() -> ToolDef {
    ToolDef::new(
        names::PAPER_ORDER,
        "Place a paper order through the [risk] gate: kill switch, halts, permission, losses, \
         order rate, data age, market status, min edge (opportunity row), depth / slippage, \
         caps after the fill, leverage. Allowed orders fill after the paper latency against a \
         live Hyperliquid book (market or limit IOC, depth walk, HL rejection codes). Needs \
         fresh mkt_ctx/1 + mkt_instrument/1 rows (read hl_ctx first) and, for an entry, an \
         opportunity row with edge_after_costs_bps. Typed observation \
         paper_fill/1:<account>:<client_order_id>: status (filled, partial, rejected, denied), \
         risk (allow / deny), risk_rule, fill price, fee, slippage, position and equity after.",
        json!({
            "type": "object",
            "properties": {
                "instrument": {
                    "type": "string",
                    "description": "Full instrument id, verbatim: hyperliquid:xyz:TSLA (Hyperliquid perps only).",
                },
                "side": {"type": "string", "enum": ["buy", "sell"]},
                "notional_usd": {
                    "type": "number",
                    "description": "Order size in USD (> 0), converted at the book mid and rounded down to the lot size.",
                },
                "kind": {"type": "string", "enum": ["market", "limit"]},
                "limit_px": {
                    "type": "number",
                    "description": "Limit price, for kind = limit only (an IOC: the rest is canceled).",
                },
                "tif": {"type": "string", "enum": ["ioc"], "description": "Time in force; ioc only."},
                "reduce_only": {
                    "type": "boolean",
                    "description": "Only reduce the open position (default false).",
                },
                "max_slippage_bps": slippage(),
                "strategy": {
                    "type": "string",
                    "enum": STRATEGIES,
                    "description": "Opportunity type; [risk] require_hedge_for names the ones that need a hedge leg.",
                },
                "hedge_instrument": {
                    "type": "string",
                    "description": "Full id of the hedge leg (its book is read and gated too).",
                },
                "opportunity": {
                    "type": "string",
                    "description": "Observation key of the row with edge_after_costs_bps that justifies the entry, e.g. xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA.",
                },
                "client_order_id": client_order_id(),
                "exit_at_ms": {
                    "type": "integer",
                    "description": "Deadline of the position this order opens, epoch ms; the exit rules close it then.",
                },
            },
            "required": ["instrument", "side", "notional_usd", "kind", "max_slippage_bps"],
            "additionalProperties": false,
        }),
    )
}

fn paper_close() -> ToolDef {
    ToolDef::new(
        names::PAPER_CLOSE,
        "Close a paper position: a reduce-only market IOC of the whole position through the \
         same [risk] gate (exits pass while halted or on stale data when allow_reduce_degraded \
         is set). Give instrument, or all = true for every open position. Typed observation \
         paper_fill/1:<account>:<client_order_id> (one position) or paper_close/1 (all; each \
         leg id is <client_order_id>:<instrument>).",
        json!({
            "type": "object",
            "properties": {
                "instrument": {
                    "type": "string",
                    "description": "Full instrument id of the position, verbatim: hyperliquid:xyz:TSLA. Not with all.",
                },
                "all": {
                    "type": "boolean",
                    "description": "true = close every open position of the [risk] account.",
                },
                "max_slippage_bps": slippage(),
                "client_order_id": client_order_id(),
            },
            "required": ["max_slippage_bps"],
            "additionalProperties": false,
        }),
    )
}

fn xm_exits() -> ToolDef {
    ToolDef::new(
        names::XM_EXITS,
        "Exit rules for the [risk] paper account: close every open position that is due — its \
         exit_at_ms deadline passed, it is older than [risk.exits] max_hold_secs, or its P&L at a \
         fresh mark reached stop_loss_bps / take_profit_bps (mkt_ctx/1 rows from the store; a \
         missing or stale mark never triggers those two). Each close is a reduce-only market IOC \
         of the whole position through the same [risk] gate, keyed \
         exit:<account>:<instrument>:<reason>:<opened_ms>, so a retry never closes twice. Typed \
         observation xm_exits/1:<account>: n_open, n_due, n_closed, n_failed, n_stale_marks; per \
         position the reason, the close and its gate rule.",
        json!({
            "type": "object",
            "properties": {
                "max_slippage_bps": {
                    "type": "number",
                    "description": "IOC bound of each close vs the book mid in bps (> 0, < 10000); default [risk] max_slippage_bps.",
                },
            },
            "required": [],
            "additionalProperties": false,
        }),
    )
}

fn paper_positions() -> ToolDef {
    ToolDef::new(
        names::PAPER_POSITIONS,
        "Paper account and its open positions at fresh marks: cash, equity, unrealized / \
         realized P&L, fees, funding, gross / net exposure, leverage, daily P&L; per position \
         the full id, qty, entry, mark, notional, P&L and exit deadline. Marks come from fresh \
         mkt_ctx/1 rows (read hl_ctx first); a missing or stale mark leaves its numbers out \
         (partial), never 0. Books the hourly funding the positions owe first. Typed \
         observation paper_positions/1:<account>, stored 2 s.",
        json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Ledger account; default the [risk] account.",
                },
            },
            "required": [],
            "additionalProperties": false,
        }),
    )
}
