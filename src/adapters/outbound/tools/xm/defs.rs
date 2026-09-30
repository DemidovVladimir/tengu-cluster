//! The xmarket family's tool interface — names, descriptions and JSON input
//! schemas in one place; tool files take their `ToolDef` from [`def`], the
//! catalog rows advertise [`defs_named`]. Schemas stay in the subset every
//! engine accepts (`tools/schema_lint.rs`).

use serde_json::json;

use crate::domain::message::ToolDef;
use crate::domain::tools as names;

/// Every xmarket tool definition, in catalog order.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![risk_status()]
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
