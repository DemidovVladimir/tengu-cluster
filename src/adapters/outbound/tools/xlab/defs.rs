//! The xlab family's tool interface — names, descriptions and JSON input
//! schemas in one place; tool files take their `ToolDef` from [`def`], the
//! catalog rows advertise [`defs_named`]. Schemas stay in the subset every
//! engine accepts (`tools/schema_lint.rs`): one type per field (times are
//! strings; a number is accepted too), string enums, limits in descriptions
//! (enforced in code). `backtest`'s `spec` is a free-form object (no
//! `properties`: a spec's fields depend on its kind) whose description names
//! the format; the tool also takes it as a string holding the object.

use serde_json::json;

use crate::domain::message::ToolDef;
use crate::domain::tools as names;

/// Every xlab tool definition, in catalog order.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![market_history(), backtest()]
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
        .unwrap_or_else(|| panic!("no xlab tool definition named {name}"))
}

fn market_history() -> ToolDef {
    ToolDef::new(
        names::MARKET_HISTORY,
        "Stored market history of one instrument in a time window, from the sandbox's market \
         warehouse (bars, and Hyperliquid funding): bar count, first / last bar, last close, \
         return, volatility and max drawdown in bps, average volume, mean funding APR, missing \
         bars (gaps), what else is stored, and a table of up to 48 evenly sampled bars (the \
         last always included). fetch = true first backfills the part of the window not stored \
         yet: Hyperliquid bars + funding for hyperliquid:<coin> ids, GeckoTerminal pool bars for \
         solana: / robinhood: ids (give pool). Typed observation \
         mkt_history/1:<instrument>:<interval>, not cached. Read-only research data: it never \
         trades.",
        json!({
            "type": "object",
            "properties": {
                "instrument": {
                    "type": "string",
                    "description": "Full instrument id, verbatim: hyperliquid:xyz:TSLA, hyperliquid:SOL, solana:<mint>, robinhood:<token address>.",
                },
                "interval": {
                    "type": "string",
                    "enum": ["1m", "5m", "15m", "1h", "4h", "1d"],
                    "description": "Bar interval. Default 1h.",
                },
                "from": {
                    "type": "string",
                    "description": "Window start, inclusive: epoch ms, RFC 3339 (2026-09-25T20:00:00Z) or a UTC date (2026-09-25). Default: 7 days before to.",
                },
                "to": {
                    "type": "string",
                    "description": "Window end, exclusive, in the same forms. Default: now.",
                },
                "fetch": {
                    "type": "boolean",
                    "description": "Backfill what the window misses before reading (network: Hyperliquid or GeckoTerminal). Default false: read the store only.",
                },
                "pool": {
                    "type": "string",
                    "description": "GeckoTerminal pool address, verbatim: required with fetch for solana: / robinhood: ids; not for hyperliquid ids.",
                },
                "points": {
                    "type": "integer",
                    "description": "How many bars the observation data holds, 1-200, evenly sampled with the first and the last; default 48. The text table shows at most 48.",
                },
            },
            "required": ["instrument"],
            "additionalProperties": false,
        }),
    )
}

fn backtest() -> ToolDef {
    ToolDef::new(
        names::BACKTEST,
        "Backtest a trading rule on the sandbox's stored market history (no network, no \
         LLM): a named strategy of the sandbox's library (strategy) or your own strategy \
         spec (spec), decisions in [from, to). Fills at bar closes after taker fee, \
         half-spread, slippage and funding; arm research trades every candidate, arm capped \
         applies the [risk] caps to the paper budget; 95 % CI by bootstrap over periods; \
         split reports in-sample and holdout side by side. Writes a run dir (report.json, \
         report.md, trades, candidates, skips) under the state dir's backtests/. Typed \
         observation backtest/1:<run id> (n trades, mean / median net bps, CI, t, hit rate, \
         USD, drawdown, Sharpe, holdout mean), not cached. Read-only research: it never \
         trades. Missing bars: fetch them first with market_history fetch = true.",
        json!({
            "type": "object",
            "properties": {
                "strategy": {
                    "type": "string",
                    "description": "A [backtest.strategies] name of the sandbox's library, e.g. weekend_fade. Give strategy or spec, not both.",
                },
                "spec": {
                    "type": "object",
                    "description": "Your own strategy spec instead of strategy (JSON object): name ([a-z0-9_], default architect_spec), kind (weekend_window, daily_window, move_trigger, funding_carry, pair_spread, event_window), universe (\"@<name>\" or full ids; pair_spread / event_window name theirs), interval (1m 5m 15m 1h 4h 1d), the kind's parameters (move_trigger: lookback_bars, threshold_bps, direction fade|follow, hold_bars), optional notional_usd, exclude, costs. Unknown fields are refused; each error names its field.",
                },
                "from": {
                    "type": "string",
                    "description": "First decision, inclusive: epoch ms, RFC 3339 (2026-07-01T00:00:00Z) or a UTC date (2026-07-01). Default: the earliest stored bar of the run's instruments.",
                },
                "to": {
                    "type": "string",
                    "description": "End of decisions, exclusive, in the same forms. Default: now.",
                },
                "split": {
                    "type": "string",
                    "description": "time:<RFC 3339 | date | ms> (holdout = decided from then) or instruments:<id,id,…> (holdout = trades on those full ids): in-sample and holdout reported side by side.",
                },
            },
            "additionalProperties": false,
        }),
    )
}
