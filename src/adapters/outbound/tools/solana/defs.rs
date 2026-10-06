//! The Solana LP family's tool interface — names, descriptions and JSON input
//! schemas of the ten read tools and the write tools, defined in one place. Family files
//! (`price.rs`, `pools.rs`, `dlmm.rs`, `perps.rs`, `wallet.rs`, `lp.rs`) take
//! their `ToolDef` from [`def`]; the catalog rows advertise [`defs_named`].
//!
//! Every tool returns a typed `Observation` (`ToolOutput::observed`) and
//! accepts an optional `max_age_secs` (0 forces a live read). Strategy knobs
//! of `hedge_decide` / `lp_decide` are REQUIRED — no hidden defaults; the
//! decision-loop TOML `args` supply them.

use serde_json::{json, Map, Value};

use crate::domain::message::ToolDef;
use crate::domain::tools as names;

/// Wrapped SOL mint — `sol_price` default.
const WSOL: &str = "So11111111111111111111111111111111111111112";
/// USDC mint — `solana_wallet` default (with wSOL).
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Every Solana tool definition (reads, then writes), in catalog order.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![
        solana_close_token_accounts(),
        jupiter_swap(),
        dlmm_close_position(),
        dlmm_open_position(),
        jup_perps_order(),
        sol_price(),
        dlmm_pools(),
        dlmm_pool(),
        dlmm_positions(),
        jup_perps(),
        solana_wallet(),
        solana_tx(),
        lp_snapshot(),
        lp_swap_plan(),
        hedge_decide(),
        lp_decide(),
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
        .unwrap_or_else(|| panic!("no Solana tool definition named {name}"))
}

// ── shared schema pieces ─────────────────────────────────────────

fn pubkey(what: &str) -> Value {
    json!({"type": "string", "description": format!("{what} (base58, full 32-byte address)")})
}

fn pubkeys(what: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": format!("{what} (base58, full addresses)")})
}

fn max_age(ttl_secs: u64) -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "description": format!(
            "Max acceptable age of a cached result in seconds; 0 forces a live read. Default: the tool TTL ({ttl_secs} s)."
        ),
    })
}

fn min_context_slot() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "description": "Read-after-write: account reads must be at or after this slot (RPC minContextSlot); cached rows with a lower slot are bypassed.",
    })
}

fn commit() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Persist controller state (regime, timers, anchors) to lp_state. Default false: a dry evaluation that changes nothing.",
    })
}

fn num(min: Option<f64>, max: Option<f64>, description: &str) -> Value {
    let mut o = Map::new();
    o.insert("type".into(), json!("number"));
    if let Some(m) = min {
        o.insert("minimum".into(), json!(m));
    }
    if let Some(m) = max {
        o.insert("maximum".into(), json!(m));
    }
    o.insert("description".into(), json!(description));
    Value::Object(o)
}

fn int(min: i64, description: &str) -> Value {
    json!({"type": "integer", "minimum": min, "description": description})
}

fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// `mode` of every write tool.
fn mode() -> Value {
    json!({
        "type": "string",
        "enum": ["simulate", "send"],
        "default": "simulate",
        "description": "simulate (default): build and simulate as the wallet — no key, nothing sent. send: sign with [solana] signer_key_file and send — only for an agent whose scope for this tool lists the wallet.",
    })
}

// ── write tools (phase 6b) ───────────────────────────────────────

fn solana_close_token_accounts() -> ToolDef {
    ToolDef::new(
        names::SOLANA_CLOSE_TOKEN_ACCOUNTS,
        "Close the wallet's EMPTY SPL token accounts (Token + Token-2022) and reclaim their rent to the wallet. Never touches wSOL, USDC or keep_mints; skips non-zero, frozen and foreign-authority accounts. 8 accounts per transaction. Returns write/1 (status simulated | confirmed | noop | refused | ...).",
        object(
            json!({
                "wallet": pubkey("Wallet (token-account authority and fee payer)"),
                "keep_mints": pubkeys("Extra mints never to close (e.g. the open LP pool's mints); wSOL and USDC are always kept"),
                "mode": mode(),
            }),
            &["wallet"],
        ),
    )
}

fn jupiter_swap() -> ToolDef {
    ToolDef::new(
        names::JUPITER_SWAP,
        "Swap through Jupiter Ultra (order → simulate as-is → sign → Ultra execute). Any pair simulates; send is SOL↔USDC only and needs the order's WORST fill (otherAmountThreshold) within oracle_gate_bps of the SOL/USD oracle, the wallet as fee payer (no gasless), and no open Jupiter perps keeper request. Returns write/1.",
        object(
            json!({
                "wallet": pubkey("Wallet (taker, signer and fee payer)"),
                "input_mint": pubkey(&format!("Mint sold (wSOL {WSOL} or USDC {USDC} to send)")),
                "output_mint": pubkey("Mint bought"),
                "amount": num(Some(0.0), None, "Amount of input_mint to sell, in token units (e.g. 0.5 SOL), floored to base units"),
                "oracle_gate_bps": num(Some(0.0), None, "Max deviation of the worst-fill implied SOL price from the oracle, bps (bot default 50). Required — no default."),
                "mode": mode(),
            }),
            &["wallet", "input_mint", "output_mint", "amount", "oracle_gate_bps"],
        ),
    )
}

fn dlmm_close_position() -> ToolDef {
    ToolDef::new(
        names::DLMM_CLOSE_POSITION,
        "Close a Meteora DLMM position: remove all liquidity, claim fees and rewards, close the position (rent back), unwrap wSOL (kept open while a Jupiter perps keeper request is pending). Positions > 70 bins close in chunks. Owner must be the wallet. Returns write/1.",
        object(
            json!({
                "wallet": pubkey("Wallet (position owner, signer and fee payer)"),
                "pool": pubkey("DLMM pool (LbPair) of the position"),
                "position": pubkey("Position account"),
                "arm_reentry": boolean("On a landed close, arm the lp_state re-entry wait (a close-only recenter). Required — no default."),
                "mode": mode(),
            }),
            &["wallet", "pool", "position", "arm_reentry"],
        ),
    )
}

fn dlmm_open_position() -> ToolDef {
    ToolDef::new(
        names::DLMM_OPEN_POSITION,
        "Open a Meteora DLMM position centred on the active bin (≤ 70 bins) and add liquidity (spot / curve / bidask): initializes missing bin arrays, the position (fresh key), ATAs, wraps SOL, adds, unwraps. Refuses when the wallet already has a position in the pool (unless allow_existing), when SOL does not cover legs + rent + fees + min_wallet_sol, when a token leg exceeds the ATA, or (SOL/USDC) when the pool price diverges from the oracle. Returns write/1.",
        object(
            json!({
                "wallet": pubkey("Wallet (position owner, signer and fee payer)"),
                "pool": pubkey("DLMM pool (LbPair)"),
                "amount_x": num(Some(0.0), None, "Token X to deposit, token units (0 allowed; not both 0)"),
                "amount_y": num(Some(0.0), None, "Token Y to deposit, token units"),
                "bin_count": int(1, "Bins, centred on the active bin; 1..=70 (bot default 20)"),
                "strategy": {"type": "string", "enum": ["spot", "curve", "bidask"], "description": "Liquidity shape (the SDK's *ImBalanced variants)"},
                "max_active_bin_slippage": int(0, "Max bins the active bin may move before the add fails (bot effective 1)"),
                "min_wallet_sol": num(Some(0.0), None, "SOL that must remain in the wallet after legs, rent and fees"),
                "max_new_bin_arrays": int(0, "Max bin arrays this open may create (0.0714 SOL rent each, not refunded); 0..=2"),
                "max_divergence_bps": num(Some(0.0), None, "SOL/USDC pools: max |pool price − oracle| in bps (> 0)"),
                "allow_existing": boolean("Open even when the wallet already has a position in this pool (default false)"),
                "mode": mode(),
            }),
            &["wallet", "pool", "amount_x", "amount_y", "bin_count", "strategy", "max_active_bin_slippage", "min_wallet_sol", "max_new_bin_arrays", "max_divergence_bps"],
        ),
    )
}

fn jup_perps_order() -> ToolDef {
    ToolDef::new(
        names::JUP_PERPS_ORDER,
        "Jupiter perps SOL market order as a keeper request (TX1; a Jupiter keeper fills it at oracle price bounded by slippage_bps): increase (size_usd + collateral in SOL for long / USDC for short), decrease (size_usd + collateral USD to withdraw) or close (entire position, still bounded). Refuses while the wallet has an open keeper request, without an oracle price, or when the post-order size exceeds max_notional_usd. On a landed request records lp_state.last_hedge_action (request-aware cooldown). Returns write/1.",
        object(
            json!({
                "wallet": pubkey("Wallet (position owner, signer and fee payer)"),
                "pool": pubkey("The LP pool this hedge belongs to (lp_state/1:<wallet>:<pool>)"),
                "side": {"type": "string", "enum": ["long", "short"], "description": "SOL position side"},
                "action": {"type": "string", "enum": ["increase", "decrease", "close"], "description": "Order type"},
                "size_usd": num(Some(0.0), None, "USD notional to add (increase) or remove (decrease); ignored for close"),
                "collateral": num(Some(0.0), None, "increase: collateral in the side's token (SOL for long, USDC for short); decrease: USD of collateral to withdraw; ignored for close"),
                "slippage_bps": num(Some(0.0), Some(10000.0), "Keeper fill bound around the oracle, bps (bot default 50); applies to close too. Required."),
                "max_notional_usd": num(Some(0.0), None, "Cap on the side's post-order size, USD (> 0). Required."),
                "mode": mode(),
            }),
            &["wallet", "pool", "side", "action", "slippage_bps", "max_notional_usd"],
        ),
    )
}

/// A knobs object: every property is required.
fn knobs(properties: Value, description: &str) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "description": description,
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

// ── the ten tools ────────────────────────────────────────────────

fn sol_price() -> ToolDef {
    ToolDef::new(
        names::SOL_PRICE,
        "USD oracle price of a token mint (Jupiter price v3), with an optional Meteora DLMM pool's \
         active price as a second source (pool_price, pool_vs_oracle_bps) and a 6-minute sample \
         ring (move_5m_pct). Typed observation price_oracle/1:<mint> (price_oracle/1:<mint>:<pool> with a pool); cached 10 s.",
        object(
            json!({
                "mint": {
                    "type": "string",
                    "default": WSOL,
                    "description": format!("Token mint (base58). Default wSOL {WSOL}."),
                },
                "pool": pubkey("Optional DLMM pool (LbPair) whose active price is the second source"),
                "pyth_feed_id": {
                    "type": "string",
                    "description": "Optional Pyth Hermes price feed id (64 hex chars, 0x optional). Pyth is queried only when given.",
                },
                "max_age_secs": max_age(10),
            }),
            &[],
        ),
    )
}

fn dlmm_pools() -> ToolDef {
    ToolDef::new(
        names::DLMM_POOLS,
        "Search Meteora DLMM pools (dlmm.datapi.meteora.ag): address, bin step, fees, TVL, 24h \
         volume, 24h fee/TVL %. Typed observation dlmm_pools/1:<query>|<sort>|<limit>|<min_tvl>; \
         cached 60 s.",
        object(
            json!({
                "query": {"type": "string", "description": "Pair or token query, e.g. SOL-USDC."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "Max pools returned (1-50). Default 10."},
                "min_tvl_usd": num(Some(0.0), None, "Drop pools with TVL below this many USD."),
                "sort": {
                    "type": "string",
                    "enum": ["fee_tvl_24h", "tvl", "volume_24h"],
                    "default": "fee_tvl_24h",
                    "description": "Sort order, descending. Default fee_tvl_24h (24h fees / TVL).",
                },
                "max_age_secs": max_age(60),
            }),
            &["query"],
        ),
    )
}

fn dlmm_pool() -> ToolDef {
    ToolDef::new(
        names::DLMM_POOL,
        "On-chain state of one Meteora DLMM pool (LbPair): token pair roles, active bin and price, \
         bin step, base + variable fee rate, reserves, depth around the active bin. Typed \
         observation dlmm_pool/1:<pool>; cached 5 s.",
        object(
            json!({
                "pool": pubkey("DLMM pool (LbPair) account"),
                "max_age_secs": max_age(5),
            }),
            &["pool"],
        ),
    )
}

fn dlmm_positions() -> ToolDef {
    ToolDef::new(
        names::DLMM_POSITIONS,
        "A wallet's Meteora DLMM positions in one pool: discovery (found / empty / error), per \
         position range, amounts, unclaimed fees and composition, total LP exposure, anomalies. \
         Typed observation dlmm_positions/1:<wallet>:<pool>; cached 10 s.",
        object(
            json!({
                "wallet": pubkey("Position owner wallet"),
                "pool": pubkey("DLMM pool (LbPair) account"),
                "positions": pubkeys("Explicit PositionV2 accounts; bypasses discovery"),
                "min_context_slot": min_context_slot(),
                "max_age_secs": max_age(10),
            }),
            &["wallet", "pool"],
        ),
    )
}

fn jup_perps() -> ToolDef {
    ToolDef::new(
        names::JUP_PERPS,
        "A wallet's Jupiter perps SOL long and short positions (size, collateral, entry, PnL, \
         borrow fee, liquidation price) plus SOL / USDC custody rates and the pool's max request \
         execution time. Typed observation jup_perps/1:<wallet>; cached 5 s.",
        object(
            json!({
                "wallet": pubkey("Perps position owner wallet"),
                "max_age_secs": max_age(5),
            }),
            &["wallet"],
        ),
    )
}

fn solana_wallet() -> ToolDef {
    ToolDef::new(
        names::SOLANA_WALLET,
        "A wallet's native SOL and SPL token accounts (Token + Token-2022), plus a balance per \
         requested mint. Typed observation solana_wallet/1:<wallet>; cached 5 s.",
        object(
            json!({
                "wallet": pubkey("Wallet address"),
                "mints": {
                    "type": "array",
                    "items": {"type": "string"},
                    "default": [WSOL, USDC],
                    "description": format!("Mints to report balances for (base58). Default wSOL {WSOL} and USDC {USDC}."),
                },
                "max_age_secs": max_age(5),
            }),
            &["wallet"],
        ),
    )
}

fn solana_tx() -> ToolDef {
    ToolDef::new(
        names::SOLANA_TX,
        "Status of one Solana transaction: found, slot, confirmation level, error, fee, compute \
         units, block time. Typed observation solana_tx/1:<signature>; cached 2 s (1 day once \
         finalized).",
        object(
            json!({
                "signature": {"type": "string", "description": "Transaction signature (base58, full 64-byte signature)."},
                "max_age_secs": max_age(2),
            }),
            &["signature"],
        ),
    )
}

fn lp_snapshot() -> ToolDef {
    ToolDef::new(
        names::LP_SNAPSHOT,
        "Canonical wallet x DLMM pool state from one consistent account read: pool price vs \
         oracle, wallet balances, positions + exposure, Jupiter perps hedge (long / short, \
         custodies), anomalies. Call before hedge_decide / lp_decide. Typed observation \
         lp_snapshot/1:<wallet>:<pool>; cached 10 s.",
        object(
            json!({
                "wallet": pubkey("LP + hedge wallet"),
                "pool": pubkey("DLMM pool (LbPair) account"),
                "positions": pubkeys("Explicit PositionV2 accounts; bypasses discovery"),
                "min_context_slot": min_context_slot(),
                "max_age_secs": max_age(10),
            }),
            &["wallet", "pool"],
        ),
    )
}

fn lp_swap_plan() -> ToolDef {
    ToolDef::new(
        names::LP_SWAP_PLAN,
        "Deterministically plan the optional Jupiter swap needed before a Meteora LP open/recenter. Reserves permanent SOL, transaction/rent SOL, refundable position rent and hedge-collateral USDC; blocks underfunded plans instead of inventing an amount. Pure: never reads, signs or sends. Merge its single data.swaps route with wallet, oracle_gate_bps and mode to call jupiter_swap, which independently applies the live oracle gate.",
        object(
            json!({
                "wallet_sol": num(Some(0.0), None, "Live wallet SOL in token units"),
                "wallet_usdc": num(Some(0.0), None, "Live wallet USDC in token units"),
                "target_sol": num(Some(0.0), None, "SOL the next LP position will deposit"),
                "target_usdc": num(Some(0.0), None, "USDC the next LP position will deposit"),
                "permanent_minimum_sol": num(Some(0.0), None, "Permanent SOL floor that may not fund the deposit or swap"),
                "rent_reserve_sol": num(Some(0.0), None, "Temporary SOL reserve for transaction fees and account creation"),
                "position_rent_sol": {"type": "number", "minimum": 0, "default": 0, "description": "Refundable SOL rent locked by the new DLMM position account; production bot measured/padded value 0.0575"},
                "reserve_usdc": {"type": "number", "minimum": 0, "default": 0, "description": "USDC kept out of the deposit for the next short-hedge collateral increase"},
                "current_price": num(Some(0.0), None, "Current base-token price in quote units; for SOL/USDC this is SOL/USD"),
                "slippage_buffer_pct": num(Some(0.0), None, "Fraction added to swap input, e.g. 0.02 = 2%"),
                "context": {"type": "string", "enum": ["initial_position", "rebalance"], "description": "Which LP path owns this plan"},
                "base_mint": {"type": "string", "default": WSOL, "description": format!("Base mint; default wSOL {WSOL}")},
                "quote_mint": {"type": "string", "default": USDC, "description": format!("Quote mint; default USDC {USDC}")},
            }),
            &[
                "wallet_sol", "wallet_usdc", "target_sol", "target_usdc",
                "permanent_minimum_sol", "rent_reserve_sol", "current_price",
                "slippage_buffer_pct", "context",
            ],
        ),
    )
}

fn hedge_knobs() -> Value {
    knobs(
        json!({
            "target_delta_sol": num(None, None, "Net SOL delta to steer toward; 0 = delta-neutral, positive = long tilt, negative = short tilt."),
            "delta_threshold_sol": num(Some(0.0), None, "Floor of the dead band: max |net delta - target| tolerated before rebalancing (SOL)."),
            "band_bins": int(0, "Auto band = this many bins' worth of LP delta (LP full value in SOL / bin_count x band_bins), floored by delta_threshold_sol; 0 = fixed band."),
            "bin_count": int(1, "LP range width in bins (used by the auto band and cap)."),
            "cap_mult": num(Some(0.0), None, "Per-side notional cap = cap_mult x (idle wallet SOL + LP full value in SOL + |target|) x price."),
            "max_notional_usd": num(Some(0.0), None, "Hard per-side notional ceiling in USD; 0 = no hard ceiling (auto cap only)."),
            "min_collateral_ratio": num(Some(0.0), Some(1.0), "Minimum collateral / notional on the side being grown (0-1)."),
            "target_collateral_ratio": num(Some(0.0), None, "Collateral / notional sized on an increase; must be >= min_collateral_ratio."),
            "carry_cap_bps": num(Some(0.0), None, "Refuse to increase a side whose annualised borrow APR exceeds this (bps); 0 = disabled."),
            "cooldown_ms": int(0, "Minimum ms between live hedge mutations (keeper fill + churn throttle)."),
            "lp_input": {"type": "string", "enum": ["live", "midpoint"], "description": "LP figure the controller hedges: live = current LP SOL amount; midpoint = SOL half of LP value."},
            "include_wallet_sol": boolean("Add idle wallet SOL (above min_wallet_sol + rent_reserve_sol) to the hedged delta."),
            "min_wallet_sol": num(Some(0.0), None, "Wallet SOL kept aside, never counted as idle."),
            "rent_reserve_sol": num(Some(0.0), None, "SOL reserved for rent and fees, never counted as idle."),
            "max_divergence_bps": num(Some(0.0), None, "Block when |pool price vs oracle| exceeds this many bps."),
            "max_snapshot_age_secs": int(0, "Block when the lp_snapshot row is older than this many seconds."),
            "trend_confirm_ms": int(0, "lp_input midpoint: a clamp-regime change (LP left its range below / above) commits only after the new regime persisted this long; storms bypass it; 0 = commit at once."),
            "no_lp_grace_ms": int(0, "With no LP position but one seen less than this many ms ago (and no re-entry wait), hold the hedge (action none) instead of trading on the no-LP read — a failed re-open mid-move must not unwind the protective hedge; 0 = off."),
        }),
        "Hedge controller knobs. ALL fields are required (no defaults).",
    )
}

/// Optional on `hedge_decide`: the same object `lp_decide` takes. When given,
/// the hedge computes the storm latch (ADR-023 bypass) and the imbalance
/// freeze (ADR-025) from this snapshot's price samples itself instead of
/// relying on an earlier `lp_decide` commit.
fn lp_knobs_for_hedge() -> Value {
    let mut v = lp_knobs();
    v["description"] = json!(
        "Optional: lp_decide's knobs (all fields when present). Lets hedge_decide compute the \
         storm latch and imbalance freeze from this cycle's price samples."
    );
    v
}

fn lp_knobs() -> Value {
    knobs(
        json!({
            "imbalance_threshold": num(Some(0.0), Some(1.0), "Recenter when one side's share of a position reaches this fraction (0-1)."),
            "bin_count": int(1, "Width of a new / recentered range in bins (capped at 70)."),
            "storm_pct_5m": num(Some(0.0), None, "Pause LP recentering while |5-minute price move| exceeds this %; exit below half; 0 = disabled."),
            "trend_confirm_ms": int(0, "An imbalance must persist this long before a recenter; 0 = act on first sight."),
            "reentry_confirm_ms": int(0, "After a close, reopen only once the price stayed within the calm corridor this long; 0 = reopen immediately."),
            "reentry_tol_frac": num(Some(0.0), Some(1.0), "Calm-corridor half width as a fraction of the full range width (0-1)."),
            "max_divergence_bps": num(Some(0.0), None, "Pause when |pool price vs oracle| exceeds this many bps."),
            "max_snapshot_age_secs": int(0, "Block when the lp_snapshot row is older than this many seconds."),
            "min_wallet_sol": num(Some(0.0), None, "Wallet SOL kept aside from deposits."),
            "rent_reserve_sol": num(Some(0.0), None, "SOL reserved for rent and fees, never deposited."),
        }),
        "LP policy knobs. ALL fields are required (no defaults).",
    )
}

fn hedge_decide() -> ToolDef {
    ToolDef::new(
        names::HEDGE_DECIDE,
        "Pure hedge decision (no network I/O) from the cached lp_snapshot row: net delta, band, \
         regime, cap, and one action (none / blocked with the guard / increase or decrease \
         long / short with size). Blocks on stale or invalid reads. Typed observation \
         hedge_decide/1:<wallet>:<pool>; never cached.",
        object(
            json!({
                "wallet": pubkey("LP + hedge wallet"),
                "pool": pubkey("DLMM pool (LbPair) account"),
                "knobs": hedge_knobs(),
                "lp_knobs": lp_knobs_for_hedge(),
                "commit": commit(),
                "max_age_secs": max_age(0),
            }),
            &["wallet", "pool", "knobs"],
        ),
    )
}

fn lp_decide() -> ToolDef {
    ToolDef::new(
        names::LP_DECIDE,
        "Pure LP decision (no network I/O) from the cached lp_snapshot and price rows: position \
         health, storm state, wallet split and one verdict (hold / recenter / wait_trend / paused \
         / reentry / open with a range plan / blocked). Typed observation \
         lp_decide/1:<wallet>:<pool>; never cached.",
        object(
            json!({
                "wallet": pubkey("LP wallet"),
                "pool": pubkey("DLMM pool (LbPair) account"),
                "knobs": lp_knobs(),
                "commit": commit(),
                "max_age_secs": max_age(0),
            }),
            &["wallet", "pool", "knobs"],
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 11] = [
        names::SOL_PRICE,
        names::DLMM_POOLS,
        names::DLMM_POOL,
        names::DLMM_POSITIONS,
        names::JUP_PERPS,
        names::SOLANA_WALLET,
        names::SOLANA_TX,
        names::LP_SNAPSHOT,
        names::LP_SWAP_PLAN,
        names::HEDGE_DECIDE,
        names::LP_DECIDE,
    ];

    fn required(d: &ToolDef) -> Vec<&str> {
        d.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    #[test]
    fn every_name_has_exactly_one_def() {
        let writes = crate::domain::tools::SOLANA_WRITE_TOOLS;
        let built: Vec<&str> = writes
            .iter()
            .copied()
            .filter(|n| !defs_named(n).is_empty())
            .collect();
        assert_eq!(tool_defs().len(), ALL.len() + built.len());
        for &n in ALL.iter().chain(built.iter()) {
            assert_eq!(defs_named(n).len(), 1, "{n}");
            assert_eq!(def(n).name, n);
            assert!(crate::domain::tools::WORKSPACE_TOOLS.contains(&n), "{n}");
        }
        assert!(defs_named("nope").is_empty());
    }

    #[test]
    fn every_read_tool_takes_optional_max_age_secs() {
        for d in tool_defs()
            .into_iter()
            .filter(|d| ALL.contains(&d.name.as_str()) && d.name != names::LP_SWAP_PLAN)
        {
            let p = &d.parameters["properties"]["max_age_secs"];
            assert_eq!(p["type"], json!("integer"), "{}", d.name);
            assert_eq!(p["minimum"], json!(0), "{}", d.name);
            assert!(!required(&d).contains(&"max_age_secs"), "{}", d.name);
        }
    }

    /// Write tools read live (no `max_age_secs`); `mode` is optional and
    /// defaults to simulate; `wallet` is required.
    #[test]
    fn every_write_tool_defaults_to_simulate() {
        for d in tool_defs()
            .into_iter()
            .filter(|d| crate::domain::tools::SOLANA_WRITE_TOOLS.contains(&d.name.as_str()))
        {
            let props = &d.parameters["properties"];
            assert!(props.get("max_age_secs").is_none(), "{}", d.name);
            assert_eq!(props["mode"]["default"], json!("simulate"), "{}", d.name);
            assert_eq!(
                props["mode"]["enum"],
                json!(["simulate", "send"]),
                "{}",
                d.name
            );
            assert!(!required(&d).contains(&"mode"), "{}", d.name);
            assert!(required(&d).contains(&"wallet"), "{}", d.name);
        }
    }

    #[test]
    fn required_args_match_the_spec() {
        let want: [(&str, &[&str]); 10] = [
            (names::SOL_PRICE, &[]),
            (names::DLMM_POOLS, &["query"]),
            (names::DLMM_POOL, &["pool"]),
            (names::DLMM_POSITIONS, &["wallet", "pool"]),
            (names::JUP_PERPS, &["wallet"]),
            (names::SOLANA_WALLET, &["wallet"]),
            (names::SOLANA_TX, &["signature"]),
            (names::LP_SNAPSHOT, &["wallet", "pool"]),
            (names::HEDGE_DECIDE, &["wallet", "pool", "knobs"]),
            (names::LP_DECIDE, &["wallet", "pool", "knobs"]),
        ];
        for (n, req) in want {
            assert_eq!(required(&def(n)), req, "{n}");
        }
        let p = &def(names::DLMM_POOLS).parameters["properties"];
        assert_eq!(p["limit"]["maximum"], json!(50));
        assert_eq!(p["sort"]["default"], json!("fee_tvl_24h"));
        assert_eq!(
            def(names::SOL_PRICE).parameters["properties"]["mint"]["default"],
            json!(WSOL)
        );
    }

    #[test]
    fn knobs_are_all_required() {
        let hedge = [
            "target_delta_sol",
            "delta_threshold_sol",
            "band_bins",
            "bin_count",
            "cap_mult",
            "max_notional_usd",
            "min_collateral_ratio",
            "target_collateral_ratio",
            "carry_cap_bps",
            "cooldown_ms",
            "lp_input",
            "include_wallet_sol",
            "min_wallet_sol",
            "rent_reserve_sol",
            "max_divergence_bps",
            "max_snapshot_age_secs",
            "trend_confirm_ms",
            "no_lp_grace_ms",
        ];
        let lp = [
            "imbalance_threshold",
            "bin_count",
            "storm_pct_5m",
            "trend_confirm_ms",
            "reentry_confirm_ms",
            "reentry_tol_frac",
            "max_divergence_bps",
            "max_snapshot_age_secs",
            "min_wallet_sol",
            "rent_reserve_sol",
        ];
        for (n, fields) in [
            (names::HEDGE_DECIDE, &hedge[..]),
            (names::LP_DECIDE, &lp[..]),
        ] {
            let k = &def(n).parameters["properties"]["knobs"];
            let knobs_def = ToolDef::new("k", "", k.clone());
            let mut req = required(&knobs_def);
            req.sort_unstable();
            let mut want = fields.to_vec();
            want.sort_unstable();
            assert_eq!(req, want, "{n}");
            assert_eq!(k["properties"].as_object().unwrap().len(), fields.len());
            assert_eq!(
                def(n).parameters["properties"]["commit"]["default"],
                json!(false)
            );
        }
    }
}
