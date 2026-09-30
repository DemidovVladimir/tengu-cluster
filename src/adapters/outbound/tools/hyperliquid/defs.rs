//! The Hyperliquid family's tool interface — names, descriptions and JSON
//! input schemas in one place; tool files take their `ToolDef` from [`def`],
//! the catalog rows advertise [`defs_named`]. Schemas stay in the subset
//! every engine accepts (`tools/schema_lint.rs`): no `oneOf`, string enums,
//! limits in descriptions (enforced in code).

use serde_json::{json, Value};

use crate::domain::message::ToolDef;
use crate::domain::tools as names;

/// Every Hyperliquid tool definition, in catalog order.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![hl_ctx()]
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
        .unwrap_or_else(|| panic!("no Hyperliquid tool definition named {name}"))
}

fn max_age(ttl_secs: u64) -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "description": format!(
            "Max acceptable age of cached rows in seconds; 0 forces a live read. Default: the TTL ({ttl_secs} s)."
        ),
    })
}

fn hl_ctx() -> ToolDef {
    ToolDef::new(
        names::HL_CTX,
        "Hyperliquid market context: mark, oracle, mid, impact prices, basis vs oracle, hourly funding \
         (+ APR), open interest (+ USD, OI-cap use), 24h volume and change, taker fee, listing / \
         OI-cap / category flags. Give coins (≤ 64 full HL names) or dex (a whole perp dex). Each \
         read stores a mkt_ctx/1:hyperliquid:<coin> row (5 s) and a mkt_instrument/1 row for every \
         coin of the dex. Returns the coin's mkt_ctx/1 row, or an hl_sweep/1 summary for a dex or \
         several coins.",
        json!({
            "type": "object",
            "properties": {
                "coins": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "1-64 full Hyperliquid coin names, verbatim: ETH, kPEPE (default dex), xyz:TSLA (HIP-3 dex xyz), @151 or PURR/USDC (spot). Not with a non-empty dex.",
                },
                "dex": {
                    "type": "string",
                    "description": "Sweep one perp dex instead of coins: \"\" or \"default\" = the default dex (BTC, ETH, ...), else a HIP-3 dex name (xyz, para, mkts, io).",
                },
                "max_age_secs": max_age(5),
            },
            "required": [],
            "additionalProperties": false,
        }),
    )
}
