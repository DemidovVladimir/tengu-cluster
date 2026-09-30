//! `hl_ctx` — HL context replies → `mkt_ctx/1` + `mkt_instrument/1` rows,
//! the side rows they copy facts from, and the `hl_sweep/1` summary.
//!
//! | Key | TTL | Source (weight) | Row |
//! |---|---|---|---|
//! | `mkt_ctx/1:hyperliquid:<coin>` | 5 s | `metaAndAssetCtxs {dex}` / `spotMetaAndAssetCtxs` (20) | every coin of the reply |
//! | `mkt_instrument/1:hyperliquid:<coin>` | 60 s | same reply's universe + the side rows below | every coin of the reply |
//! | `hl_perp_meta/1:hyperliquid` | 1 h (60 s when partial) | `perpDexs` + `perpCategories` (20 + 20) | [`HlPerpMeta`]: builder-dex order (asset ids), OI caps, categories |
//! | `hl_at_oi_cap/1:hyperliquid:<dex label>` | 60 s | `perpsAtOpenInterestCap {dex}` (20) | [`HlAtOiCap`] |
//! | `hl_sweep/1:hyperliquid:<dex label>` | 5 s | the call itself | [`HlSweep`], dex sweep |
//! | `hl_sweep/1:hyperliquid:<coin>,<coin>,…` (sorted) | not stored | the call itself | [`HlSweep`], several coins |
//!
//! | Perp universe / ctx field | Row field |
//! |---|---|
//! | `isDelisted: true` | `listing = delisted` (row `absent`) |
//! | `szDecimals`, `maxLeverage` | `sz_decimals`, `max_leverage` |
//! | `marginMode` (`noCross`, `strictIsolated`; absent = normal) · `onlyIsolated` (absent = false) | `margin_mode`, `only_isolated` |
//! | `deployerFeeScale` (decimal string) · `growthMode = "enabled"` (absent = off) | `deployer_fee_scale`, `growth_mode`; default dex (validator-operated) = 0 / off |
//! | `markPx`, `oraclePx`, `midPx`, `impactPxs [bid, ask]`, `prevDayPx`, `premium`, `funding` (per hour), `openInterest` (base), `dayNtlVlm` | `mark`, `oracle`, `mid`, `impact_bid` / `impact_ask`, `prev_day`, `premium`, `funding_1h`, `oi_base`, `vol_24h_usd` |
//! | null `premium` / `midPx` / `impactPxs` on a listed market | `no_book` (row `partial`) |
//! | spot: `markPx`, `midPx`, `prevDayPx`, `dayNtlVlm`; pair `tokens [base, quote]` | `mark`, `mid`, `prev_day`, `vol_24h_usd`; `sz_decimals` = base token's, `quote_ccy` = quote token, `display_name` = `BASE/QUOTE` |
//! | `taker_fee_bps` | `domain::hl::usdc_fee_schedule` (`domain::xm::cost::hl_fee_schedule`) with the paper fee basis, the deployer scale and growth mode; only on USDC-quoted markets (the aligned-quote term of USDH / USDE / USDT0 is not modelled) — else omitted. The paper fills use the same rule (`domain::hl::paper_fees`) |
//!
//! Spot ctx entries without a universe pair (`@71`) and outcome entries
//! (`#…`) get no rows. A failed side read leaves its field unknown and puts
//! a `ReadError` on every instrument row (`partial`); ctx rows stay as the
//! prices allow.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    collateral_quote, dex_label, next_funding_ms, perp_asset_id, spot_asset_id, token_quote,
    usdc_fee_schedule,
};
use crate::domain::lp::market::fmt_sig;
use crate::domain::market::{
    decimal_field, decimal_value, Category, InstrumentId, InstrumentKind, Listing, MarginMode,
    MarketCtx, MarketInstrument, QuoteCcy,
};
use crate::domain::observation::{
    set_bool, set_int, ErrorClass, Features, Field, ObsStatus, Observed, ReadError,
};
use crate::domain::xm::cost::{HlFeeMarket, HlUserRates};

/// `mkt_ctx/1` TTL.
pub(crate) const CTX_TTL_MS: u64 = 5_000;
/// `mkt_instrument/1` TTL (rows are rewritten by every ctx read).
pub(crate) const INSTRUMENT_TTL_MS: u64 = 60_000;
/// `hl_at_oi_cap/1` TTL.
pub(crate) const AT_OI_CAP_TTL_MS: u64 = 60_000;
/// `hl_perp_meta/1` TTL when both sources read.
pub(crate) const PERP_META_TTL_MS: u64 = 3_600_000;
/// `hl_perp_meta/1` TTL when a source failed (retry soon).
pub(crate) const PERP_META_RETRY_TTL_MS: u64 = 60_000;
/// Max coins per `hl_ctx` call.
pub(crate) const MAX_COINS: usize = 64;
/// Headlines above this list counts, not coin names (line 1 ≤ 200).
const MAX_HEADLINE: usize = 180;

fn decode(field: &str, message: impl Into<String>) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, message)
}

// ---------------------------------------------------------------------------
// Side rows: perpDexs + perpCategories, perpsAtOpenInterestCap
// ---------------------------------------------------------------------------

/// One builder dex from `perpDexs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HlDex {
    pub name: String,
    /// Position in `perpDexs` (the default dex is 0) — sets asset ids.
    pub position: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    /// `assetToStreamingOiCap`: coin → OI cap in USD notional.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub oi_caps: BTreeMap<String, f64>,
}

/// `perpDexs` reply → builder dexes (index 0 is `null`, the default dex).
/// An OI cap that is not a decimal is left out (unknown, never 0).
pub(crate) fn decode_perp_dexs(v: &Value) -> Result<Vec<HlDex>, String> {
    let entries = v.as_array().ok_or("perpDexs reply is not a list")?;
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if e.is_null() {
            continue;
        }
        let name = e["name"]
            .as_str()
            .ok_or_else(|| format!("perpDexs[{i}] has no name"))?;
        let oi_caps = e["assetToStreamingOiCap"]
            .as_array()
            .map(|pairs| {
                pairs
                    .iter()
                    .filter_map(|p| Some((p[0].as_str()?.to_string(), decimal_value(&p[1])?)))
                    .collect()
            })
            .unwrap_or_default();
        out.push(HlDex {
            name: name.to_string(),
            position: i as u32,
            full_name: e["fullName"].as_str().map(str::to_string),
            oi_caps,
        });
    }
    Ok(out)
}

/// `perpCategories` reply (`[[coin, label], …]`) → coin → HL label verbatim.
pub(crate) fn decode_categories(v: &Value) -> Result<BTreeMap<String, String>, String> {
    let pairs = v.as_array().ok_or("perpCategories reply is not a list")?;
    Ok(pairs
        .iter()
        .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
        .collect())
}

/// A list of coin names (`perpsAtOpenInterestCap`).
pub(crate) fn decode_coin_list(v: &Value) -> Result<Vec<String>, String> {
    let items = v.as_array().ok_or("reply is not a list of coins")?;
    items
        .iter()
        .map(|c| {
            c.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("coin list entry {c} is not a string"))
        })
        .collect()
}

/// `hl_perp_meta/1:hyperliquid` — slow-moving perp universe facts: builder
/// dexes in `perpDexs` order (asset ids, OI caps) and `perpCategories`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HlPerpMeta {
    pub dexes: Field<Vec<HlDex>>,
    /// Coin → HL label verbatim (`stocks`, `stock`, `FX`, …); HIP-3 coins only.
    pub categories: Field<BTreeMap<String, String>>,
}

impl HlPerpMeta {
    /// The builder dex named `name` (`None`: unknown, or `perpDexs` failed).
    pub(crate) fn dex(&self, name: &str) -> Option<&HlDex> {
        self.dexes.value()?.iter().find(|d| d.name == name)
    }

    /// Normalised category of `coin`; unknown label or coin ⇒ `None`.
    pub(crate) fn category(&self, coin: &str) -> Option<Category> {
        Category::parse(self.categories.value()?.get(coin)?)
    }

    /// 1 h when both sources read, else 60 s (an `Error` row is never cached).
    pub(crate) fn ttl_ms(&self) -> u64 {
        if self.status() == ObsStatus::Ok {
            PERP_META_TTL_MS
        } else {
            PERP_META_RETRY_TTL_MS
        }
    }
}

impl Observed for HlPerpMeta {
    const SCHEMA: &'static str = "hl_perp_meta/1";

    fn subject(&self) -> String {
        "hyperliquid".into()
    }

    fn headline(&self) -> String {
        let dexes = match self.dexes.value() {
            Some(d) => {
                let caps: usize = d.iter().map(|x| x.oi_caps.len()).sum();
                format!("{} builder dexes, {caps} OI caps", d.len())
            }
            None => "perpDexs unavailable".into(),
        };
        let cats = match self.categories.value() {
            Some(c) => format!("{} categories", c.len()),
            None => "perpCategories unavailable".into(),
        };
        format!("hl perp meta: {dexes}, {cats}")
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        if let Some(d) = self.dexes.value() {
            set_int(&mut f, "n_dexes", Some(d.len() as i64));
            let caps: usize = d.iter().map(|x| x.oi_caps.len()).sum();
            set_int(&mut f, "n_oi_caps", Some(caps as i64));
        }
        if let Some(c) = self.categories.value() {
            set_int(&mut f, "n_categories", Some(c.len() as i64));
        }
        f
    }

    fn status(&self) -> ObsStatus {
        match (self.dexes.is_error(), self.categories.is_error()) {
            (false, false) => ObsStatus::Ok,
            (true, true) => ObsStatus::Error,
            _ => ObsStatus::Partial,
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        [self.dexes.error(), self.categories.error()]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

/// `hl_at_oi_cap/1:hyperliquid:<dex label>` — perps of one dex at their
/// open-interest cap (`perpsAtOpenInterestCap`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HlAtOiCap {
    /// API name (`""` = the default dex).
    pub dex: String,
    pub coins: Field<Vec<String>>,
}

impl Observed for HlAtOiCap {
    const SCHEMA: &'static str = "hl_at_oi_cap/1";

    fn subject(&self) -> String {
        format!("hyperliquid:{}", dex_label(&self.dex))
    }

    fn headline(&self) -> String {
        let head = format!("hl at OI cap dex={}", dex_label(&self.dex));
        match self.coins.value() {
            None => format!("{head}: unavailable"),
            Some(c) if c.is_empty() => format!("{head}: none"),
            Some(c) => {
                let full = format!("{head}: {} ({})", c.len(), c.join(" "));
                if full.chars().count() <= MAX_HEADLINE {
                    full
                } else {
                    format!("{head}: {} coins", c.len())
                }
            }
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(
            &mut f,
            "n_at_cap",
            self.coins.value().map(|c| c.len() as i64),
        );
        f
    }

    fn status(&self) -> ObsStatus {
        if self.coins.is_error() {
            ObsStatus::Error
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.coins.error().cloned().into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// Context rows
// ---------------------------------------------------------------------------

/// One coin's rows from a ctx read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoinRows {
    pub instrument: MarketInstrument,
    pub ctx: MarketCtx,
}

/// A decoded ctx reply: rows in universe order + universe entries that did
/// not decode (skipped, never guessed).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CtxRows {
    pub rows: Vec<CoinRows>,
    pub skipped: Vec<ReadError>,
}

/// Facts perp rows copy from the side reads; `None` = unknown.
#[derive(Debug, Clone, Default)]
pub(crate) struct PerpFacts<'a> {
    /// `hl_perp_meta/1` — builder dexes need it for asset ids, OI caps and
    /// categories; the default dex needs none of it.
    pub meta: Option<&'a HlPerpMeta>,
    /// This dex's `hl_at_oi_cap/1` coins; `None` = the read failed.
    pub at_cap: Option<&'a [String]>,
    /// Perp user rates (`FeeBasis::user_rates`); `None` ⇒ no `taker_fee_bps`.
    pub rates: Option<HlUserRates>,
    /// Side reads that failed, put on every instrument row (⇒ `partial`).
    pub errors: Vec<ReadError>,
}

/// `[universe-or-meta, ctxs]` of a `*MetaAndAssetCtxs` reply.
fn split_reply<'a>(reply: &'a Value, what: &str) -> Result<(&'a Value, &'a [Value]), ReadError> {
    let bad = || decode("ctx", format!("{what} reply is not [meta, assetCtxs]"));
    let parts = reply.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
    let ctxs = parts[1].as_array().ok_or_else(bad)?;
    Ok((&parts[0], ctxs))
}

fn is_null(c: &Value, key: &str) -> bool {
    c.get(key).is_none_or(Value::is_null)
}

/// `impactPxs = [bid, ask]` (the prices to sell / buy HL's impact notional).
fn impact(c: &Value) -> (Field<f64>, Field<f64>) {
    let bad = || {
        let e = decode("impact", "impactPxs is not [bid, ask] decimals");
        (Field::err(e.clone()), Field::err(e))
    };
    match c.get("impactPxs") {
        None | Some(Value::Null) => (Field::Absent, Field::Absent),
        Some(Value::Array(a)) if a.len() == 2 => {
            match (decimal_value(&a[0]), decimal_value(&a[1])) {
                (Some(b), Some(s)) => (Field::ok(b), Field::ok(s)),
                _ => bad(),
            }
        }
        Some(_) => bad(),
    }
}

/// HL flags: absent / `null` ⇒ `default`; a bool ⇒ it; anything else ⇒ unknown.
fn flag(u: &Value, key: &str, default: bool) -> Option<bool> {
    match u.get(key) {
        None | Some(Value::Null) => Some(default),
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => None,
    }
}

fn small_u32(u: &Value, key: &str) -> Option<u32> {
    u.get(key)?.as_u64().and_then(|x| u32::try_from(x).ok())
}

/// Taker fee in bps on a USDC-quoted market (`usdc_fee_schedule`, the rule
/// the paper fills use); `None` when an input is unknown.
fn taker_bps(
    rates: Option<HlUserRates>,
    quote: Option<QuoteCcy>,
    market: HlFeeMarket,
) -> Option<f64> {
    usdc_fee_schedule(rates?, quote, market).map(|f| f.taker_bps)
}

/// `metaAndAssetCtxs {dex}` → one [`CoinRows`] per universe entry (the ctx
/// list is index-aligned with the universe). `Err` when the reply has no
/// `[meta, assetCtxs]` shape.
pub(crate) fn perp_rows(
    dex: &str,
    reply: &Value,
    facts: &PerpFacts<'_>,
    now_ms: i64,
) -> Result<CtxRows, ReadError> {
    let (meta, ctxs) = split_reply(reply, "metaAndAssetCtxs")?;
    let universe = meta["universe"]
        .as_array()
        .ok_or_else(|| decode("ctx", "metaAndAssetCtxs meta has no universe list"))?;
    let quote = meta["collateralToken"].as_u64().and_then(collateral_quote);
    let builder = (!dex.is_empty()).then(|| facts.meta.and_then(|m| m.dex(dex)));
    let position = match builder {
        None => Some(0),
        Some(d) => d.map(|d| d.position),
    };
    let caps = builder.flatten().map(|d| &d.oi_caps);
    let mut out = CtxRows::default();
    for (i, u) in universe.iter().enumerate() {
        let Some(name) = u["name"].as_str() else {
            out.skipped
                .push(decode("universe", format!("universe[{i}] has no name")));
            continue;
        };
        let id = match InstrumentId::hyperliquid(name) {
            Ok(id) => id,
            Err(e) => {
                out.skipped.push(decode("universe", e));
                continue;
            }
        };
        let listing = if u["isDelisted"] == Value::Bool(true) {
            Listing::Delisted
        } else {
            Listing::Listed
        };
        let mut inst = MarketInstrument::new(id.clone(), InstrumentKind::Perp, listing);
        inst.dex = (!dex.is_empty()).then(|| dex.to_string());
        inst.asset_id = position.map(|p| perp_asset_id(p, i as u32));
        inst.category = facts.meta.and_then(|m| m.category(name));
        inst.quote_ccy = quote;
        inst.sz_decimals = small_u32(u, "szDecimals");
        inst.max_leverage = small_u32(u, "maxLeverage");
        inst.margin_mode = match u.get("marginMode") {
            None | Some(Value::Null) => Some(MarginMode::Normal),
            Some(Value::String(s)) => MarginMode::parse(s),
            Some(_) => None,
        };
        inst.only_isolated = flag(u, "onlyIsolated", false);
        inst.oi_cap_usd = caps.and_then(|c| c.get(name).copied());
        inst.at_oi_cap = facts.at_cap.map(|l| l.iter().any(|c| c == name));
        if dex.is_empty() {
            // Validator-operated perps: no deployer scale, no growth mode.
            inst.deployer_fee_scale = Some(0.0);
            inst.growth_mode = Some(false);
        } else {
            inst.deployer_fee_scale = u.get("deployerFeeScale").and_then(decimal_value);
            inst.growth_mode = match u.get("growthMode") {
                None | Some(Value::Null) => Some(false),
                Some(Value::String(s)) => Some(s == "enabled"),
                Some(_) => None,
            };
        }
        inst.errors = facts.errors.clone();
        let fee = match (inst.deployer_fee_scale, inst.growth_mode) {
            (Some(s), Some(g)) => taker_bps(
                facts.rates,
                quote,
                HlFeeMarket::Perp {
                    deployer_fee_scale: s,
                    growth_mode: g,
                },
            ),
            _ => None,
        };
        let mut ctx = match ctxs.get(i).filter(|c| c.is_object()) {
            Some(c) => perp_ctx(id, c, listing, now_ms),
            None => MarketCtx::failed(
                id,
                now_ms,
                decode(
                    "ctx",
                    format!("metaAndAssetCtxs has no asset ctx for {name} (index {i})"),
                ),
            ),
        };
        ctx.listing = listing;
        copy_facts(&mut ctx, &inst, fee);
        out.rows.push(CoinRows {
            instrument: inst,
            ctx,
        });
    }
    Ok(out)
}

fn perp_ctx(id: InstrumentId, c: &Value, listing: Listing, now_ms: i64) -> MarketCtx {
    let mut ctx = MarketCtx::new(id, now_ms);
    ctx.mark = decimal_field(c, "markPx", "mark");
    ctx.oracle = decimal_field(c, "oraclePx", "oracle");
    ctx.mid = decimal_field(c, "midPx", "mid");
    (ctx.impact_bid, ctx.impact_ask) = impact(c);
    ctx.prev_day = decimal_field(c, "prevDayPx", "prev_day");
    ctx.premium = decimal_field(c, "premium", "premium");
    ctx.funding_1h = decimal_field(c, "funding", "funding_1h");
    ctx.funding_interval_h = Some(1.0);
    ctx.next_funding_ms = Some(next_funding_ms(now_ms));
    ctx.oi_base = decimal_field(c, "openInterest", "oi_base");
    ctx.vol_24h_usd = decimal_field(c, "dayNtlVlm", "vol_24h_usd");
    ctx.no_book = listing == Listing::Listed
        && ["premium", "midPx", "impactPxs"]
            .iter()
            .any(|k| is_null(c, k));
    ctx
}

/// Instrument facts every ctx row carries.
fn copy_facts(ctx: &mut MarketCtx, inst: &MarketInstrument, taker_fee_bps: Option<f64>) {
    ctx.oi_cap_usd = inst.oi_cap_usd;
    ctx.at_oi_cap = inst.at_oi_cap;
    ctx.max_leverage = inst.max_leverage;
    ctx.only_isolated = inst.only_isolated;
    ctx.category = inst.category;
    ctx.growth_mode = inst.growth_mode;
    ctx.taker_fee_bps = taker_fee_bps;
}

/// A spot token: name + size decimals.
struct Token<'a> {
    name: &'a str,
    sz_decimals: Option<u32>,
}

/// `spotMetaAndAssetCtxs` → one [`CoinRows`] per universe pair (ctxs are
/// matched by `coin`). `rates` = spot user rates.
pub(crate) fn spot_rows(
    reply: &Value,
    rates: Option<HlUserRates>,
    now_ms: i64,
) -> Result<CtxRows, ReadError> {
    let (meta, ctxs) = split_reply(reply, "spotMetaAndAssetCtxs")?;
    let (Some(tokens), Some(universe)) = (meta["tokens"].as_array(), meta["universe"].as_array())
    else {
        return Err(decode(
            "ctx",
            "spotMetaAndAssetCtxs meta has no tokens / universe lists",
        ));
    };
    let tokens: HashMap<u64, Token<'_>> = tokens
        .iter()
        .filter_map(|t| {
            Some((
                t["index"].as_u64()?,
                Token {
                    name: t["name"].as_str()?,
                    sz_decimals: small_u32(t, "szDecimals"),
                },
            ))
        })
        .collect();
    let by_coin: HashMap<&str, &Value> = ctxs
        .iter()
        .filter_map(|c| Some((c["coin"].as_str()?, c)))
        .collect();
    let mut out = CtxRows::default();
    for (i, u) in universe.iter().enumerate() {
        let (Some(name), Some(index)) = (u["name"].as_str(), u["index"].as_u64()) else {
            out.skipped.push(decode(
                "universe",
                format!("spot universe[{i}] has no name / index"),
            ));
            continue;
        };
        let id = match InstrumentId::hyperliquid(name) {
            Ok(id) => id,
            Err(e) => {
                out.skipped.push(decode("universe", e));
                continue;
            }
        };
        let pair = u["tokens"].as_array();
        let token = |k: usize| {
            pair.and_then(|p| p.get(k))
                .and_then(Value::as_u64)
                .and_then(|t| tokens.get(&t))
        };
        let (base, quote) = (token(0), token(1));
        let mut inst = MarketInstrument::new(id.clone(), InstrumentKind::Spot, Listing::Listed);
        inst.asset_id = u32::try_from(index).ok().map(spot_asset_id);
        inst.sz_decimals = base.and_then(|b| b.sz_decimals);
        inst.quote_ccy = quote.and_then(|q| token_quote(q.name));
        if let (Some(b), Some(q)) = (base, quote) {
            inst.display_name = Some(format!("{}/{}", b.name, q.name));
        }
        let fee = base.and_then(|b| {
            taker_bps(
                rates,
                inst.quote_ccy,
                HlFeeMarket::Spot {
                    stable_pair: token_quote(b.name).is_some(),
                },
            )
        });
        let mut ctx = match by_coin.get(name) {
            Some(c) => {
                let mut ctx = MarketCtx::new(id, now_ms);
                ctx.mark = decimal_field(c, "markPx", "mark");
                ctx.mid = decimal_field(c, "midPx", "mid");
                ctx.prev_day = decimal_field(c, "prevDayPx", "prev_day");
                ctx.vol_24h_usd = decimal_field(c, "dayNtlVlm", "vol_24h_usd");
                ctx.no_book = is_null(c, "midPx");
                ctx
            }
            None => MarketCtx::failed(
                id,
                now_ms,
                decode(
                    "ctx",
                    format!("spotMetaAndAssetCtxs has no asset ctx for {name}"),
                ),
            ),
        };
        copy_facts(&mut ctx, &inst, fee);
        out.rows.push(CoinRows {
            instrument: inst,
            ctx,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// hl_sweep/1 — the summary of a dex sweep or a multi-coin call
// ---------------------------------------------------------------------------

/// What an `hl_ctx` call covered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SweepScope {
    /// Every coin of one perp dex (`""` = the default dex).
    Dex { dex: String },
    /// The requested coins, HL names verbatim, request order.
    Coins { coins: Vec<String> },
}

/// One info request the call sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SweepRead {
    /// Info `type` (`metaAndAssetCtxs`, `perpDexs`, …).
    pub info: String,
    /// Dex label of a per-dex request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex: Option<String>,
    pub weight: u32,
    /// Error class when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<ErrorClass>,
}

/// One coin of the summary; the full row is `mkt_ctx/1:hyperliquid:<coin>`.
/// `basis_bps` / `funding_apr_pct` rounded to 0.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SweepCoin {
    pub coin: String,
    pub status: ObsStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_apr_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delisted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_found: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_book: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at_oi_cap: bool,
}

fn round1(x: Option<f64>) -> Option<f64> {
    x.map(|v| (v * 10.0).round() / 10.0)
}

impl SweepCoin {
    /// Summary entry of a `mkt_ctx/1` row; prices only for listed markets.
    pub(crate) fn of(ctx: &MarketCtx) -> Self {
        let listed = ctx.listing == Listing::Listed;
        Self {
            coin: ctx.id.native().to_string(),
            status: ctx.status(),
            mark: ctx.mark.value().copied().filter(|_| listed),
            basis_bps: round1(ctx.basis_bps().filter(|_| listed)),
            funding_apr_pct: round1(ctx.funding_apr_pct().filter(|_| listed)),
            delisted: ctx.listing == Listing::Delisted,
            not_found: ctx.listing == Listing::NotFound,
            no_book: ctx.no_book,
            at_oi_cap: ctx.at_oi_cap == Some(true),
        }
    }
}

/// `hl_sweep/1` — what one `hl_ctx` call returns for a dex sweep or
/// several coins. The per-coin rows are in the store.
///
/// | Status | When |
/// |---|---|
/// | `error` | no coin row, or every coin row `error` (the ctx read failed) |
/// | `absent` | every coin absent (delisted / unknown), or HL does not know the dex |
/// | `partial` | a coin row `error` / `partial`, or a failed side read (`errors`) |
/// | `ok` | otherwise |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HlSweep {
    pub scope: SweepScope,
    /// Info requests sent (empty: every row came from the cache).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<SweepRead>,
    /// `mkt_ctx/1` + `mkt_instrument/1` rows written to the store.
    pub rows_written: usize,
    pub coins: Vec<SweepCoin>,
    /// Failed reads and skipped universe entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

impl HlSweep {
    /// Σ request weight of [`HlSweep::reads`].
    pub(crate) fn weight(&self) -> u32 {
        self.reads.iter().map(|r| r.weight).sum()
    }

    fn count(&self, status: ObsStatus) -> usize {
        self.coins.iter().filter(|c| c.status == status).count()
    }

    fn count_by(&self, pred: impl Fn(&SweepCoin) -> bool) -> usize {
        self.coins.iter().filter(|c| pred(c)).count()
    }

    /// Errors that degrade the answer (`not_applicable` = HL does not know
    /// a dex / coin — the coins are absent, not failed).
    fn degrading_errors(&self) -> usize {
        self.errors
            .iter()
            .filter(|e| e.class != ErrorClass::NotApplicable)
            .count()
    }

    /// `hl_sweep/1` subject: `hyperliquid:<dex label>` or the sorted,
    /// de-duplicated coins joined by `,`.
    pub(crate) fn subject_of(scope: &SweepScope) -> String {
        match scope {
            SweepScope::Dex { dex } => format!("hyperliquid:{}", dex_label(dex)),
            SweepScope::Coins { coins } => {
                let set: BTreeSet<&str> = coins.iter().map(String::as_str).collect();
                format!(
                    "hyperliquid:{}",
                    set.into_iter().collect::<Vec<_>>().join(",")
                )
            }
        }
    }
}

impl Observed for HlSweep {
    const SCHEMA: &'static str = "hl_sweep/1";

    fn subject(&self) -> String {
        Self::subject_of(&self.scope)
    }

    fn headline(&self) -> String {
        let head = match &self.scope {
            SweepScope::Dex { dex } => format!("hl_ctx dex={}", dex_label(dex)),
            SweepScope::Coins { .. } => "hl_ctx".to_string(),
        };
        let n = self.coins.len();
        if n == 0 {
            let why = if self.degrading_errors() > 0 {
                "unavailable"
            } else {
                "no markets (HL does not know it)"
            };
            return format!("{head}: {why}");
        }
        let mut h = format!("{head} {n} coins:");
        for (s, label) in [
            (ObsStatus::Ok, "ok"),
            (ObsStatus::Partial, "partial"),
            (ObsStatus::Absent, "absent"),
            (ObsStatus::Error, "error"),
        ] {
            let k = self.count(s);
            if k > 0 {
                h.push_str(&format!(" {k} {label}"));
            }
        }
        let delisted = self.count_by(|c| c.delisted);
        if delisted > 0 {
            h.push_str(&format!(" ({delisted} delisted)"));
        }
        let capped = self.count_by(|c| c.at_oi_cap);
        if capped > 0 {
            h.push_str(&format!(" {capped} at OI cap"));
        }
        h.push_str(&format!(" weight={}", self.weight()));
        if let SweepScope::Coins { .. } = self.scope {
            let names: Vec<String> = self
                .coins
                .iter()
                .map(|c| match c.mark {
                    Some(m) => format!("{}={}", c.coin, fmt_sig(m, 6)),
                    None => format!("{}={}", c.coin, c.status.as_str()),
                })
                .collect();
            let full = format!("{h} [{}]", names.join(" "));
            if full.chars().count() <= MAX_HEADLINE {
                return full;
            }
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let n = |x: usize| Some(x as i64);
        set_int(&mut f, "n_coins", n(self.coins.len()));
        set_int(&mut f, "n_ok", n(self.count(ObsStatus::Ok)));
        set_int(&mut f, "n_partial", n(self.count(ObsStatus::Partial)));
        set_int(&mut f, "n_absent", n(self.count(ObsStatus::Absent)));
        set_int(&mut f, "n_error", n(self.count(ObsStatus::Error)));
        set_int(&mut f, "n_delisted", n(self.count_by(|c| c.delisted)));
        set_int(&mut f, "n_not_found", n(self.count_by(|c| c.not_found)));
        set_int(&mut f, "n_no_book", n(self.count_by(|c| c.no_book)));
        set_int(&mut f, "n_at_oi_cap", n(self.count_by(|c| c.at_oi_cap)));
        set_int(&mut f, "n_reads", n(self.reads.len()));
        set_int(
            &mut f,
            "n_failed_reads",
            n(self.reads.iter().filter(|r| r.failed.is_some()).count()),
        );
        set_int(&mut f, "weight", Some(i64::from(self.weight())));
        set_int(&mut f, "rows_written", n(self.rows_written));
        set_bool(&mut f, "from_cache", Some(self.reads.is_empty()));
        f
    }

    fn status(&self) -> ObsStatus {
        let n = self.coins.len();
        if n == 0 {
            return if self.degrading_errors() > 0 {
                ObsStatus::Error
            } else {
                ObsStatus::Absent
            };
        }
        let (ok, partial, err) = (
            self.count(ObsStatus::Ok),
            self.count(ObsStatus::Partial),
            self.count(ObsStatus::Error),
        );
        if err == n {
            ObsStatus::Error
        } else if ok + partial == 0 && err == 0 {
            ObsStatus::Absent
        } else if err > 0 || partial > 0 || self.degrading_errors() > 0 {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::hl::{CoinKind, FeeBasis};
    use crate::domain::observation::{
        assert_features_ok, ObsSource, Observation, MAX_FEATURES, MAX_LINE1_CHARS,
    };
    use crate::domain::xm::cost::HlKind;
    use serde_json::json;

    pub(crate) const MAC_XYZ: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/metaAndAssetCtxs_xyz.json"
    ));
    pub(crate) const MAC_DEFAULT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/metaAndAssetCtxs_default.json"
    ));
    pub(crate) const PERP_DEXS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/perpDexs.json"
    ));
    pub(crate) const CATEGORIES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/perpCategories.json"
    ));
    pub(crate) const AT_CAP_DEFAULT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/perpsAtOpenInterestCap_default.json"
    ));
    pub(crate) const SPOT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hyperliquid/spotMetaAndAssetCtxs_trimmed.json"
    ));
    /// Read time the ctx goldens use (the `xyz:TSLA` l2Book capture).
    pub(crate) const NOW: i64 = 1_790_775_567_699;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    pub(crate) fn meta() -> HlPerpMeta {
        HlPerpMeta {
            dexes: Field::ok(decode_perp_dexs(&j(PERP_DEXS)).unwrap()),
            categories: Field::ok(decode_categories(&j(CATEGORIES)).unwrap()),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    fn row<'a>(rows: &'a CtxRows, coin: &str) -> &'a CoinRows {
        rows.rows
            .iter()
            .find(|r| r.ctx.id.native() == coin)
            .unwrap_or_else(|| panic!("{coin} missing"))
    }

    fn tier0() -> Option<HlUserRates> {
        FeeBasis::default().user_rates(HlKind::Perp).ok()
    }

    #[test]
    fn side_replies_decode() {
        let dexes = decode_perp_dexs(&j(PERP_DEXS)).unwrap();
        assert_eq!(dexes.len(), 10);
        let names: Vec<(&str, u32)> = dexes
            .iter()
            .map(|d| (d.name.as_str(), d.position))
            .collect();
        assert_eq!(names[0], ("xyz", 1));
        assert!(names.contains(&("para", 8)) && names.contains(&("io", 10)));
        assert_eq!(dexes[0].oi_caps["xyz:TSLA"], 100_000_000.0);
        assert_eq!(dexes[0].full_name.as_deref(), Some("XYZ"));
        let cats = decode_categories(&j(CATEGORIES)).unwrap();
        assert_eq!(cats["xyz:TSLA"], "stocks");
        assert_eq!(cats["xyz:EUR"], "fx");
        assert!(
            !cats.contains_key("ETH"),
            "default-dex coins have no category"
        );
        let m = meta();
        assert_eq!(m.category("xyz:TSLA"), Some(Category::Stocks));
        assert_eq!(m.category("ETH"), None);
        assert_eq!(m.dex("io").map(|d| d.position), Some(10));
        assert_eq!(m.status(), ObsStatus::Ok);
        assert_eq!(m.ttl_ms(), PERP_META_TTL_MS);
        let o = Observation::of("hl_ctx", &m, NOW, m.ttl_ms(), ObsSource::Live);
        assert_eq!(o.key, "hl_perp_meta/1:hyperliquid");
        assert_eq!(o.typed::<HlPerpMeta>().unwrap(), m);
        assert_eq!(
            o.headline,
            format!(
                "hl perp meta: 10 builder dexes, {} OI caps, {} categories",
                o.features["n_oi_caps"], o.features["n_categories"]
            )
        );
        let caps = decode_coin_list(&j(AT_CAP_DEFAULT)).unwrap();
        assert_eq!(caps.len(), 7);
        for bad in [json!({}), json!([1]), json!(null)] {
            assert!(decode_coin_list(&bad).is_err());
        }
        assert!(decode_perp_dexs(&json!({})).is_err());
        assert!(decode_categories(&json!("x")).is_err());
    }

    #[test]
    fn side_rows_status_and_ttl() {
        let e = || {
            ReadError::new(
                "categories",
                ErrorClass::Transient,
                "perpCategories: HTTP 502",
            )
        };
        let mut m = meta();
        m.categories = Field::err(e());
        assert_eq!(m.status(), ObsStatus::Partial);
        assert_eq!(m.ttl_ms(), PERP_META_RETRY_TTL_MS);
        assert_eq!(m.category("xyz:TSLA"), None);
        m.dexes = Field::err(e());
        assert_eq!(m.status(), ObsStatus::Error);
        assert_eq!(m.errors().len(), 2);
        assert!(m.headline().contains("perpDexs unavailable"));

        let cap = HlAtOiCap {
            dex: String::new(),
            coins: Field::ok(decode_coin_list(&j(AT_CAP_DEFAULT)).unwrap()),
        };
        let o = Observation::of("hl_ctx", &cap, NOW, AT_OI_CAP_TTL_MS, ObsSource::Live);
        assert_eq!(o.key, "hl_at_oi_cap/1:hyperliquid:default");
        assert_eq!(
            o.headline,
            "hl at OI cap dex=default: 7 (CANTO FTM JELLY LOOM RLB VINE ZEREBRO)"
        );
        let none = HlAtOiCap {
            dex: "xyz".into(),
            coins: Field::ok(vec![]),
        };
        assert_eq!(none.headline(), "hl at OI cap dex=xyz: none");
        assert_eq!(none.features()["n_at_cap"], 0);
        let failed = HlAtOiCap {
            dex: "xyz".into(),
            coins: Field::err(e()),
        };
        assert_eq!(failed.status(), ObsStatus::Error);
        let many = HlAtOiCap {
            dex: "xyz".into(),
            coins: Field::ok((0..40).map(|i| format!("xyz:LONGNAME{i}")).collect()),
        };
        assert_eq!(many.headline(), "hl at OI cap dex=xyz: 40 coins");
    }

    /// `xyz:TSLA` against the captured replies: the `domain/market.rs`
    /// goldens (same capture) plus the fee and asset-id rules.
    #[test]
    fn xyz_rows_from_the_fixture() {
        let m = meta();
        let facts = PerpFacts {
            meta: Some(&m),
            at_cap: Some(&[]),
            rates: tier0(),
            errors: vec![],
        };
        let rows = perp_rows("xyz", &j(MAC_XYZ), &facts, NOW).unwrap();
        assert_eq!(rows.rows.len(), 128);
        assert!(rows.skipped.is_empty());
        let tsla = row(&rows, "xyz:TSLA");
        let i = &tsla.instrument;
        assert_eq!(i.id.to_string(), "hyperliquid:xyz:TSLA");
        assert_eq!(
            (i.kind, i.listing, i.dex.as_deref(), i.asset_id),
            (
                InstrumentKind::Perp,
                Listing::Listed,
                Some("xyz"),
                Some(110_001)
            )
        );
        assert_eq!((i.sz_decimals, i.max_leverage), (Some(3), Some(20)));
        assert_eq!(i.margin_mode, Some(MarginMode::Normal));
        assert_eq!(i.only_isolated, Some(false));
        assert_eq!(i.quote_ccy, Some(QuoteCcy::Usdc));
        assert_eq!(i.category, Some(Category::Stocks));
        assert_eq!(
            (i.oi_cap_usd, i.at_oi_cap),
            (Some(100_000_000.0), Some(false))
        );
        assert_eq!(
            (i.deployer_fee_scale, i.growth_mode),
            (Some(1.0), Some(true))
        );
        assert_eq!(i.status(), ObsStatus::Ok);

        let c = &tsla.ctx;
        assert_eq!(c.status(), ObsStatus::Ok);
        let f = c.features();
        assert_features_ok(&f);
        let num = |k: &str| f[k].as_f64().unwrap();
        assert_eq!(num("mark"), 347.19);
        assert_eq!(num("oracle"), 346.91);
        assert!(close(num("basis_bps"), 8.071_257_674_900_485));
        assert!(close(num("premium_bps"), 7.653_282));
        assert!(close(num("funding_apr_pct"), 21.253_074));
        assert!(close(num("oi_usd"), 54_721_695.660_9));
        assert!(close(num("oi_cap_used_pct"), 54.721_695_660_9));
        assert!(close(num("change_24h_pct"), -1.570_606_412_837_016_7));
        assert!(close(num("impact_spread_bps"), 0.835_312_399_636_390_3));
        assert_eq!(num("next_funding_s"), 1_232.301);
        // Tier 0: 4.5 × 2 (scale 1) × 0.1 (growth mode) = 0.9 bps (meta.json).
        assert!(close(num("taker_fee_bps"), 0.9));
        assert_eq!(f["category"], "stocks");
        assert_eq!(f["at_oi_cap"], false);

        // xyz:MSTR: scale 1.0, no growth mode ⇒ 9 bps.
        let mstr = row(&rows, "xyz:MSTR");
        assert_eq!(mstr.instrument.growth_mode, Some(false));
        assert!(close(mstr.ctx.taker_fee_bps.unwrap(), 9.0));
        // noCross markets are isolated-only.
        let hood = &row(&rows, "xyz:HOOD").instrument;
        assert_eq!(
            (hood.margin_mode, hood.only_isolated),
            (Some(MarginMode::NoCross), Some(true))
        );
        // Delisted: flagged, strict isolated, no book, row absent.
        let u = row(&rows, "xyz:URANIUM");
        assert_eq!(u.instrument.listing, Listing::Delisted);
        assert_eq!(u.instrument.margin_mode, Some(MarginMode::StrictIsolated));
        assert_eq!(u.ctx.status(), ObsStatus::Absent);
        assert!(!u.ctx.no_book, "no_book is for listed markets");
        let delisted = rows
            .rows
            .iter()
            .filter(|r| r.instrument.listing == Listing::Delisted)
            .count();
        assert_eq!(delisted, 19);
        let keys = Observation::of("hl_ctx", &tsla.ctx, NOW, CTX_TTL_MS, ObsSource::Live);
        assert_eq!(keys.key, "mkt_ctx/1:hyperliquid:xyz:TSLA");
    }

    #[test]
    fn default_dex_rows_need_no_side_meta() {
        let caps = decode_coin_list(&j(AT_CAP_DEFAULT)).unwrap();
        let facts = PerpFacts {
            meta: None,
            at_cap: Some(&caps),
            rates: tier0(),
            errors: vec![],
        };
        let rows = perp_rows("", &j(MAC_DEFAULT), &facts, NOW).unwrap();
        assert_eq!(rows.rows.len(), 40);
        let eth = row(&rows, "ETH");
        assert_eq!(eth.instrument.id.to_string(), "hyperliquid:ETH");
        assert_eq!(eth.instrument.dex, None);
        assert_eq!(eth.instrument.asset_id, Some(1));
        assert_eq!(eth.instrument.category, None, "not guessed");
        assert_eq!(
            (
                eth.instrument.deployer_fee_scale,
                eth.instrument.growth_mode
            ),
            (Some(0.0), Some(false))
        );
        assert!(
            close(eth.ctx.taker_fee_bps.unwrap(), 4.5),
            "validator perp, tier 0"
        );
        assert_eq!(eth.ctx.status(), ObsStatus::Ok);
        let kpepe = row(&rows, "kPEPE");
        assert_eq!(kpepe.instrument.asset_id, Some(15));
        let ftm = row(&rows, "FTM");
        assert_eq!(ftm.instrument.listing, Listing::Delisted);
        assert_eq!(ftm.instrument.at_oi_cap, Some(true));
        assert_eq!(ftm.ctx.status(), ObsStatus::Absent);
    }

    #[test]
    fn failed_side_reads_leave_fields_unknown() {
        let e = ReadError::new(
            "at_oi_cap",
            ErrorClass::RateLimited,
            "perpsAtOpenInterestCap: HTTP 429",
        );
        let facts = PerpFacts {
            meta: None,
            at_cap: None,
            rates: None,
            errors: vec![e.clone()],
        };
        let rows = perp_rows("xyz", &j(MAC_XYZ), &facts, NOW).unwrap();
        let t = row(&rows, "xyz:TSLA");
        assert_eq!(t.instrument.asset_id, None, "perpDexs position unknown");
        assert_eq!(
            (t.instrument.oi_cap_usd, t.instrument.at_oi_cap),
            (None, None)
        );
        assert_eq!(t.instrument.category, None);
        assert_eq!(t.instrument.errors, vec![e]);
        assert_eq!(t.instrument.status(), ObsStatus::Partial);
        let f = t.ctx.features();
        for k in ["taker_fee_bps", "at_oi_cap", "oi_cap_used_pct", "category"] {
            assert!(!f.contains_key(k), "{k} must be omitted, never guessed");
        }
        assert_eq!(t.ctx.status(), ObsStatus::Ok, "prices are fine");
    }

    #[test]
    fn listed_market_without_book_and_bad_decimals() {
        let reply = json!([
            {"universe": [
                {"name": "xyz:AAA", "szDecimals": 2, "maxLeverage": 10, "deployerFeeScale": "0.5"},
                {"name": "xyz:BBB", "szDecimals": 2, "maxLeverage": 10},
                {"name": "xyz:CCC", "szDecimals": 2, "maxLeverage": 10},
                {"szDecimals": 2}
            ], "collateralToken": 0},
            [
                {"markPx": "10.0", "oraclePx": "10.1", "midPx": null, "impactPxs": null,
                 "premium": null, "funding": "0.0", "openInterest": "5", "prevDayPx": "9", "dayNtlVlm": "0"},
                {"markPx": "abc", "oraclePx": "10.1", "midPx": "10", "impactPxs": ["9.9", 10.1],
                 "premium": "0.001", "funding": "0.0001", "openInterest": "5", "prevDayPx": "9", "dayNtlVlm": "0"}
            ]
        ]);
        let facts = PerpFacts {
            rates: tier0(),
            ..Default::default()
        };
        let rows = perp_rows("xyz", &reply, &facts, NOW).unwrap();
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(rows.skipped.len(), 1, "the nameless entry");
        let a = row(&rows, "xyz:AAA");
        assert!(a.ctx.no_book);
        assert_eq!(a.ctx.status(), ObsStatus::Partial);
        // Scale 0.5 ⇒ × 1.5, no growth mode: 6.75 bps.
        assert!(close(a.ctx.taker_fee_bps.unwrap(), 6.75));
        let b = row(&rows, "xyz:BBB");
        assert!(b.ctx.mark.is_error());
        assert_eq!(b.ctx.status(), ObsStatus::Partial, "oracle still readable");
        assert_eq!(b.ctx.impact_ask, Field::ok(10.1));
        assert_eq!(
            b.instrument.deployer_fee_scale, None,
            "no scale in the meta"
        );
        assert_eq!(b.ctx.taker_fee_bps, None);
        // No asset ctx at index 2.
        let c = row(&rows, "xyz:CCC");
        assert_eq!(c.ctx.status(), ObsStatus::Error);
        assert!(c.ctx.errors()[0].message.contains("xyz:CCC (index 2)"));
        for bad in [
            json!(null),
            json!([{}]),
            json!([{}, {}]),
            json!([{"universe": 1}, []]),
        ] {
            assert_eq!(
                perp_rows("xyz", &bad, &facts, NOW).unwrap_err().class,
                ErrorClass::Decode,
                "{bad}"
            );
        }
    }

    #[test]
    fn spot_rows_from_the_trimmed_fixture() {
        let rates = FeeBasis::default().user_rates(HlKind::Spot).ok();
        let rows = spot_rows(&j(SPOT), rates, NOW).unwrap();
        let coins: Vec<&str> = rows.rows.iter().map(|r| r.ctx.id.native()).collect();
        // Universe pairs only: no `@71` (ctx without a pair), no `#…` outcome.
        assert_eq!(
            coins,
            ["PURR/USDC", "@1", "@107", "@141", "@151", "@166", "@207"]
        );
        let ueth = row(&rows, "@151");
        let i = &ueth.instrument;
        assert_eq!(i.id.to_string(), "hyperliquid:@151");
        assert_eq!((i.kind, i.asset_id), (InstrumentKind::Spot, Some(10_151)));
        assert_eq!(i.display_name.as_deref(), Some("UETH/USDC"));
        assert_eq!(
            (i.sz_decimals, i.quote_ccy),
            (Some(4), Some(QuoteCcy::Usdc))
        );
        assert_eq!(ueth.ctx.mark, Field::ok(2_673.6));
        assert_eq!(ueth.ctx.mid, Field::ok(2_673.65));
        assert_eq!(ueth.ctx.status(), ObsStatus::Ok);
        assert!(close(ueth.ctx.taker_fee_bps.unwrap(), 7.0), "spot tier 0");
        assert!(ueth.ctx.funding_1h == Field::Absent && ueth.ctx.oracle == Field::Absent);
        let o = Observation::of("hl_ctx", &ueth.ctx, NOW, CTX_TTL_MS, ObsSource::Live);
        assert_eq!(o.key, "mkt_ctx/1:hyperliquid:@151");
        // USDT0/USDC: a stable pair (× 0.2).
        let stable = row(&rows, "@166");
        assert_eq!(
            stable.instrument.display_name.as_deref(),
            Some("USDT0/USDC")
        );
        assert!(close(stable.ctx.taker_fee_bps.unwrap(), 1.4));
        // HYPE/USDT0: quote USDT, fee not modelled.
        let hype = row(&rows, "@207");
        assert_eq!(hype.instrument.quote_ccy, Some(QuoteCcy::Usdt));
        assert_eq!(hype.ctx.taker_fee_bps, None);
        // Listed pair with no book.
        let god = row(&rows, "@141");
        assert!(god.ctx.no_book);
        assert_eq!(god.ctx.status(), ObsStatus::Partial);
        assert!(matches!(
            crate::domain::hl::classify("@151"),
            CoinKind::Spot
        ));
    }

    fn sweep(coins: Vec<SweepCoin>, errors: Vec<ReadError>) -> HlSweep {
        HlSweep {
            scope: SweepScope::Dex { dex: "xyz".into() },
            reads: vec![
                SweepRead {
                    info: "metaAndAssetCtxs".into(),
                    dex: Some("xyz".into()),
                    weight: 20,
                    failed: None,
                },
                SweepRead {
                    info: "perpsAtOpenInterestCap".into(),
                    dex: Some("xyz".into()),
                    weight: 20,
                    failed: None,
                },
            ],
            rows_written: 2 * coins.len(),
            coins,
            errors,
        }
    }

    #[test]
    fn sweep_summary_of_the_xyz_fixture() {
        let m = meta();
        let facts = PerpFacts {
            meta: Some(&m),
            at_cap: Some(&[]),
            rates: tier0(),
            errors: vec![],
        };
        let rows = perp_rows("xyz", &j(MAC_XYZ), &facts, NOW).unwrap();
        let coins: Vec<SweepCoin> = rows.rows.iter().map(|r| SweepCoin::of(&r.ctx)).collect();
        let s = sweep(coins, vec![]);
        assert_eq!(s.status(), ObsStatus::Ok);
        let o = Observation::of("hl_ctx", &s, NOW, CTX_TTL_MS, ObsSource::Live);
        assert_eq!(o.key, "hl_sweep/1:hyperliquid:xyz");
        assert_eq!(
            o.headline,
            "hl_ctx dex=xyz 128 coins: 109 ok 19 absent (19 delisted) weight=40"
        );
        assert_features_ok(&o.features);
        assert!(o.features.len() <= MAX_FEATURES);
        assert_eq!(o.features["n_delisted"], 19);
        assert_eq!(o.features["from_cache"], false);
        let tsla = &s.coins[1];
        assert_eq!(
            serde_json::to_value(tsla).unwrap(),
            json!({"coin": "xyz:TSLA", "status": "ok", "mark": 347.19, "basis_bps": 8.1, "funding_apr_pct": 21.3})
        );
        let u = s.coins.iter().find(|c| c.coin == "xyz:URANIUM").unwrap();
        assert_eq!(
            serde_json::to_value(u).unwrap(),
            json!({"coin": "xyz:URANIUM", "status": "absent", "delisted": true})
        );
        // Bounded text: the whole data line (10 720 chars) fits the render
        // limit; local engines get a store pointer instead (`compact_text`).
        let text = o.render_text(NOW);
        assert!(text.len() < 12_000, "{} chars", text.len());
        let compact = o.compact_text(&text);
        assert!(compact.len() < 600, "{compact}");
        assert!(compact.ends_with("bytes in observation hl_sweep/1:hyperliquid:xyz"));
        assert!(text.lines().next().unwrap().chars().count() <= MAX_LINE1_CHARS);
        assert_eq!(o.typed::<HlSweep>().unwrap(), s);
    }

    #[test]
    fn sweep_statuses() {
        let ok = |coin: &str| SweepCoin {
            coin: coin.into(),
            status: ObsStatus::Ok,
            mark: Some(1.0),
            basis_bps: None,
            funding_apr_pct: None,
            delisted: false,
            not_found: false,
            no_book: false,
            at_oi_cap: false,
        };
        let with = |status| SweepCoin {
            status,
            mark: None,
            ..ok("xyz:X")
        };
        let side = ReadError::new(
            "at_oi_cap",
            ErrorClass::Transient,
            "perpsAtOpenInterestCap: HTTP 502",
        );
        let unknown = ReadError::new(
            "ctx",
            ErrorClass::NotApplicable,
            "HL does not know dex nope",
        );
        assert_eq!(sweep(vec![ok("xyz:A")], vec![]).status(), ObsStatus::Ok);
        assert_eq!(
            sweep(vec![ok("xyz:A")], vec![side.clone()]).status(),
            ObsStatus::Partial
        );
        assert_eq!(
            sweep(vec![ok("xyz:A"), with(ObsStatus::Error)], vec![]).status(),
            ObsStatus::Partial
        );
        assert_eq!(
            sweep(vec![with(ObsStatus::Error)], vec![]).status(),
            ObsStatus::Error
        );
        assert_eq!(
            sweep(vec![with(ObsStatus::Absent)], vec![]).status(),
            ObsStatus::Absent
        );
        assert_eq!(
            sweep(vec![], vec![unknown.clone()]).status(),
            ObsStatus::Absent
        );
        assert_eq!(
            sweep(vec![], vec![unknown]).headline(),
            "hl_ctx dex=xyz: no markets (HL does not know it)"
        );
        let failed = sweep(vec![], vec![side]);
        assert_eq!(failed.status(), ObsStatus::Error);
        assert_eq!(failed.headline(), "hl_ctx dex=xyz: unavailable");
    }

    #[test]
    fn coin_sweeps_key_by_the_sorted_coin_set_and_keep_full_ids() {
        let coins = vec![
            "xyz:TSLA".to_string(),
            "ETH".to_string(),
            "xyz:TSLA".to_string(),
        ];
        let scope = SweepScope::Coins { coins };
        assert_eq!(HlSweep::subject_of(&scope), "hyperliquid:ETH,xyz:TSLA");
        let mk = |coin: &str, mark: Option<f64>, status| SweepCoin {
            coin: coin.into(),
            status,
            mark,
            basis_bps: None,
            funding_apr_pct: None,
            delisted: false,
            not_found: mark.is_none(),
            no_book: false,
            at_oi_cap: false,
        };
        let s = HlSweep {
            scope,
            reads: vec![],
            rows_written: 0,
            coins: vec![
                mk("xyz:TSLA", Some(347.19), ObsStatus::Ok),
                mk("ETH", Some(4_512.3), ObsStatus::Ok),
                mk("xyz:NOPE", None, ObsStatus::Absent),
            ],
            errors: vec![],
        };
        assert_eq!(
            s.headline(),
            "hl_ctx 3 coins: 2 ok 1 absent weight=0 [xyz:TSLA=347.190 ETH=4512.30 xyz:NOPE=absent]"
        );
        assert_eq!(s.status(), ObsStatus::Ok);
        assert_eq!(s.features()["from_cache"], true);
        // 64 long names: counts only, never a cut id.
        let many = HlSweep {
            coins: (0..64)
                .map(|i| mk(&format!("xyz:NAME{i:02}"), Some(1.0), ObsStatus::Ok))
                .collect(),
            ..s
        };
        let h = many.headline();
        assert_eq!(h, "hl_ctx 64 coins: 64 ok weight=0");
        assert!(h.chars().count() <= MAX_HEADLINE);
    }
}
