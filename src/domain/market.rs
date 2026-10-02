//! Cross-venue market rows keyed by the instrument id of tracker convention 1:
//! `<venue>:<native id verbatim>` (`hyperliquid:xyz:TSLA`, `hyperliquid:@151`,
//! `robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d`, `ref:XNAS:TSLA`).
//! Pure data + derived features; venue decoders (`domain/hl/`, CEX, RH) map
//! into these. Order books live in `domain/book.rs`.
//!
//! | Schema | Key | Row |
//! |---|---|---|
//! | `mkt_instrument/1` | `mkt_instrument/1:<id>` | what a venue lists: kind, listing, category, quote, leverage, margin mode, OI cap, fee scale, growth mode |
//! | `mkt_ctx/1` | `mkt_ctx/1:<id>` | live context: one `Field<f64>` per price, funding, OI, volume + instrument facts copied from `mkt_instrument/1` |
//!
//! | `mkt_ctx/1` status | When |
//! |---|---|
//! | `absent` | delisted, or the venue does not know the id (HL `200 null`) |
//! | `error` | no price readable and at least one read failed |
//! | `partial` | a price is readable but another field failed, or a listed market has no book (HL: null `premium` / `midPx` / `impactPxs`) |
//! | `ok` | otherwise |
//!
//! Derived features never replace a missing input with 0: `basis_bps` needs
//! `mark` and a reference (`oracle`, else `index`), `oi_usd` needs `oi_base`
//! and `mark`, and so on — otherwise the key is omitted.
//!
//! Venue decimal strings (HL, Binance, Bybit send prices as strings):
//! [`parse_decimal`] / [`decimal_field`] → `f64` for features and
//! arithmetic; decoders keep the raw string where exactness matters (tick
//! and lot rounding).

// Consumers land next wave (`hl-ctx-tool`, `hl-book-tool`, `kg-sync-*`).
#![allow(dead_code)]

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::lp::market::fmt_sig;
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError, MAX_FEATURE_STR,
};

// ---------------------------------------------------------------------------
// Venue decimal strings
// ---------------------------------------------------------------------------

/// A venue decimal string (`"347.19"`, `"-0.0000015073"`, `"1e-5"`) → finite
/// `f64`. Only `[+-]digits[.digits][(e|E)[+-]digits]` parses: no spaces,
/// no `NaN` / `inf`, no empty string.
pub(crate) fn parse_decimal(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int = digits(i);
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = digits(i);
        i += frac;
    }
    if int + frac == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = digits(i);
        if exp == 0 {
            return None;
        }
        i += exp;
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok().filter(|x| x.is_finite())
}

/// A decimal string or a JSON number → finite `f64`; anything else `None`.
pub(crate) fn decimal_value(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => parse_decimal(s),
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// `obj[key]` as a field: a decimal ⇒ `Ok`, `null` / missing ⇒ `Absent`,
/// anything else ⇒ a `Decode` error named `field` — never 0.
pub(crate) fn decimal_field(obj: &Value, key: &str, field: &str) -> Field<f64> {
    let v = match obj.get(key) {
        None | Some(Value::Null) => return Field::Absent,
        Some(v) => v,
    };
    match decimal_value(v) {
        Some(x) => Field::ok(x),
        None => {
            let what = match v {
                Value::String(s) => format!("the string \"{s}\""),
                Value::Number(n) => format!("the number {n}"),
                Value::Bool(_) => "a bool".to_string(),
                Value::Array(_) => "an array".to_string(),
                Value::Object(_) => "an object".to_string(),
                Value::Null => "null".to_string(),
            };
            Field::err(ReadError::new(
                field,
                ErrorClass::Decode,
                format!("{key} is {what}, not a decimal"),
            ))
        }
    }
}

/// Venue ids of convention 1 (a venue slice adds its own here). Reference
/// listings use `ref:<MIC>` (ISO 10383, four characters).
pub(crate) const VENUES: &[&str] = &[
    "hyperliquid",
    "robinhood",
    "solana",
    "binance-spot",
    "binance-usdm",
    "bybit-spot",
    "bybit-linear",
    "okx-spot",
    "okx-swap",
    "coinbase",
    "coinbase-intx",
];

pub(crate) const HYPERLIQUID: &str = "hyperliquid";

// ---------------------------------------------------------------------------
// Instrument id
// ---------------------------------------------------------------------------

/// `<venue>:<native id verbatim>`. The native id is never normalised: HL
/// coins keep their dex prefix (`xyz:TSLA`), spot pairs their `@` index, EVM
/// addresses their checksum case. Serialises as the joined string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct InstrumentId {
    venue: String,
    native: String,
}

impl InstrumentId {
    /// Errors name the whole id, never a part of it.
    pub(crate) fn new(venue: &str, native: &str) -> Result<Self, String> {
        let problem = check_venue(venue).err().or_else(|| {
            (native.is_empty() || native.chars().any(|c| c.is_whitespace() || c.is_control()))
                .then(|| "the native id must be non-empty without whitespace".to_string())
        });
        match problem {
            Some(p) => Err(format!("instrument id `{venue}:{native}`: {p}")),
            None => Ok(Self {
                venue: venue.to_string(),
                native: native.to_string(),
            }),
        }
    }

    /// Hyperliquid coin as the API names it: `ETH`, `xyz:TSLA`, `@151`.
    pub(crate) fn hyperliquid(coin: &str) -> Result<Self, String> {
        Self::new(HYPERLIQUID, coin)
    }

    /// Parse `<venue>:<native>`; `ref:<MIC>:<ticker>` keeps `ref:<MIC>` as
    /// the venue. The native part may itself contain `:` (`xyz:TSLA`).
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("instrument id `{s}` is not `<venue>:<native id>`");
        let (venue, native) = match s.strip_prefix("ref:") {
            Some(rest) => {
                let (mic, native) = rest.split_once(':').ok_or_else(bad)?;
                (format!("ref:{mic}"), native)
            }
            None => {
                let (venue, native) = s.split_once(':').ok_or_else(bad)?;
                (venue.to_string(), native)
            }
        };
        Self::new(&venue, native)
    }

    pub(crate) fn venue(&self) -> &str {
        &self.venue
    }

    /// The venue's own id, verbatim (HL coin, CEX symbol, token address).
    pub(crate) fn native(&self) -> &str {
        &self.native
    }
}

fn check_venue(venue: &str) -> Result<(), String> {
    if VENUES.contains(&venue) {
        return Ok(());
    }
    if let Some(mic) = venue.strip_prefix("ref:") {
        if mic.len() == 4
            && mic
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Ok(());
        }
    }
    Err(format!(
        "unknown venue `{venue}` (known: {}, ref:<MIC>)",
        VENUES.join(", ")
    ))
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.venue, self.native)
    }
}

impl TryFrom<String> for InstrumentId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s)
    }
}

impl From<InstrumentId> for String {
    fn from(id: InstrumentId) -> String {
        id.to_string()
    }
}

// ---------------------------------------------------------------------------
// Instrument attributes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstrumentKind {
    Perp,
    Spot,
    Outcome,
}

impl InstrumentKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            InstrumentKind::Perp => "perp",
            InstrumentKind::Spot => "spot",
            InstrumentKind::Outcome => "outcome",
        }
    }
}

/// `not_found` = the venue does not know the id (HL `200 null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Listing {
    Listed,
    Delisted,
    NotFound,
}

impl Listing {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Listing::Listed => "listed",
            Listing::Delisted => "delisted",
            Listing::NotFound => "not_found",
        }
    }
}

/// Asset class. Venue labels are normalised by [`Category::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Category {
    Stocks,
    Etf,
    Indices,
    Commodities,
    Fx,
    Rates,
    Preipo,
    Crypto,
}

impl Category {
    /// Venue label → category, case-insensitive; singular forms fold into
    /// the plural (HL sends both `stock` and `stocks`, `fx` and `FX`).
    /// Unknown labels → `None` (never guessed).
    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "stocks" | "stock" | "equity" | "equities" => Some(Category::Stocks),
            "etf" | "etfs" => Some(Category::Etf),
            "indices" | "index" => Some(Category::Indices),
            "commodities" | "commodity" => Some(Category::Commodities),
            "fx" | "forex" => Some(Category::Fx),
            "rates" | "rate" => Some(Category::Rates),
            "preipo" | "pre-ipo" | "pre_ipo" => Some(Category::Preipo),
            "crypto" => Some(Category::Crypto),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Category::Stocks => "stocks",
            Category::Etf => "etf",
            Category::Indices => "indices",
            Category::Commodities => "commodities",
            Category::Fx => "fx",
            Category::Rates => "rates",
            Category::Preipo => "preipo",
            Category::Crypto => "crypto",
        }
    }
}

/// Quote (collateral) currency. USDT / USDC / USDG quotes differ by a few
/// bps — never compare prices across quotes without an FX anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum QuoteCcy {
    Usdc,
    Usdt,
    Usdh,
    Usde,
    Usd,
}

impl QuoteCcy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            QuoteCcy::Usdc => "USDC",
            QuoteCcy::Usdt => "USDT",
            QuoteCcy::Usdh => "USDH",
            QuoteCcy::Usde => "USDE",
            QuoteCcy::Usd => "USD",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MarginMode {
    Normal,
    NoCross,
    StrictIsolated,
}

impl MarginMode {
    /// HL `marginMode` (`noCross`, `strictIsolated`; absent = `normal`) or
    /// the snake_case name.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(MarginMode::Normal),
            "noCross" | "no_cross" => Some(MarginMode::NoCross),
            "strictIsolated" | "strict_isolated" => Some(MarginMode::StrictIsolated),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MarginMode::Normal => "normal",
            MarginMode::NoCross => "no_cross",
            MarginMode::StrictIsolated => "strict_isolated",
        }
    }
}

/// What the instrument tracks, when known.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Underlying {
    /// Reference listing as annotated (`Nasdaq`, `KRX`, `Cboe BZX`) or a MIC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
    /// Ticker on that listing, verbatim (`TSLA`, `005930`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticker: Option<String>,
    /// Underlying units per instrument unit, decimal string verbatim (HL `k`
    /// prefix `1000`, an ADS `0.1`, RH multiplier `1.000566080061092436`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<String>,
    /// Price converted from a non-USD listing (KRW → USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_converted: Option<bool>,
}

// ---------------------------------------------------------------------------
// mkt_instrument/1
// ---------------------------------------------------------------------------

/// One listed (or delisted) instrument. `None` = unknown, never a default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MarketInstrument {
    pub id: InstrumentId,
    pub kind: InstrumentKind,
    pub listing: Listing,
    /// Venue sub-market: HL builder dex (`xyz`); `None` = HL's default dex
    /// or a venue without sub-markets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex: Option<String>,
    /// Venue numeric asset id (HL perps: index, `100000 + 10000·dex + index`
    /// on builder dexes; spot `10000 + index`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlying: Option<Underlying>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_ccy: Option<QuoteCcy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sz_decimals: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_leverage: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<MarginMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_isolated: Option<bool>,
    /// Open-interest cap in USD notional (HL `assetToStreamingOiCap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oi_cap_usd: Option<f64>,
    /// Listed in the venue's at-cap set (HL `perpsAtOpenInterestCap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_oi_cap: Option<bool>,
    /// HIP-3 deployer fee scale (HL `deployerFeeScale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer_fee_scale: Option<f64>,
    /// HIP-3 growth mode (HL `growthMode = "enabled"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growth_mode: Option<bool>,
    /// Sources that failed while building the row (fields stay `None`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

impl MarketInstrument {
    /// Every attribute unknown — decoders fill what the venue sent.
    pub(crate) fn new(id: InstrumentId, kind: InstrumentKind, listing: Listing) -> Self {
        Self {
            id,
            kind,
            listing,
            dex: None,
            asset_id: None,
            category: None,
            display_name: None,
            keywords: Vec::new(),
            underlying: None,
            quote_ccy: None,
            sz_decimals: None,
            max_leverage: None,
            margin_mode: None,
            only_isolated: None,
            oi_cap_usd: None,
            at_oi_cap: None,
            deployer_fee_scale: None,
            growth_mode: None,
            errors: Vec::new(),
        }
    }

    /// `underlying.ratio` as a number (features only; data keeps the string).
    pub(crate) fn underlying_ratio(&self) -> Option<f64> {
        let r = self.underlying.as_ref()?.ratio.as_deref()?;
        r.trim().parse::<f64>().ok().filter(|x| x.is_finite())
    }
}

impl Observed for MarketInstrument {
    const SCHEMA: &'static str = "mkt_instrument/1";

    fn subject(&self) -> String {
        self.id.to_string()
    }

    fn headline(&self) -> String {
        let mut h = format!(
            "instrument {} {} {}",
            self.id,
            self.kind.as_str(),
            self.listing.as_str()
        );
        if let Some(c) = self.category {
            h.push_str(&format!(" {}", c.as_str()));
        }
        if let Some(q) = self.quote_ccy {
            h.push_str(&format!(" quote={}", q.as_str()));
        }
        if let Some(l) = self.max_leverage {
            h.push_str(&format!(" max_lev={l}"));
        }
        if let Some(cap) = self.oi_cap_usd {
            h.push_str(&format!(" oi_cap_usd={}", fmt_sig(cap, 4)));
        }
        if self.at_oi_cap == Some(true) {
            h.push_str(" at_oi_cap");
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "kind", Some(self.kind.as_str()));
        set_str(&mut f, "listing", Some(self.listing.as_str()));
        set_bool(&mut f, "delisted", Some(self.listing == Listing::Delisted));
        set_str(&mut f, "dex", short(self.dex.as_deref()));
        set_str(&mut f, "category", self.category.map(Category::as_str));
        set_str(&mut f, "quote_ccy", self.quote_ccy.map(QuoteCcy::as_str));
        set_int(&mut f, "sz_decimals", self.sz_decimals.map(i64::from));
        set_int(&mut f, "max_leverage", self.max_leverage.map(i64::from));
        set_str(
            &mut f,
            "margin_mode",
            self.margin_mode.map(MarginMode::as_str),
        );
        set_bool(&mut f, "only_isolated", self.only_isolated);
        set_num(&mut f, "oi_cap_usd", self.oi_cap_usd);
        set_bool(&mut f, "at_oi_cap", self.at_oi_cap);
        set_num(&mut f, "deployer_fee_scale", self.deployer_fee_scale);
        set_bool(&mut f, "growth_mode", self.growth_mode);
        set_num(&mut f, "underlying_ratio", self.underlying_ratio());
        set_bool(
            &mut f,
            "fx_converted",
            self.underlying.as_ref().and_then(|u| u.fx_converted),
        );
        f
    }

    fn status(&self) -> ObsStatus {
        match self.listing {
            Listing::NotFound => ObsStatus::Absent,
            _ if !self.errors.is_empty() => ObsStatus::Partial,
            _ => ObsStatus::Ok,
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}

// ---------------------------------------------------------------------------
// mkt_ctx/1
// ---------------------------------------------------------------------------

fn absent<T>() -> Field<T> {
    Field::Absent
}

fn is_absent<T>(f: &Field<T>) -> bool {
    matches!(f, Field::Absent)
}

/// Live market context. Prices are in the instrument's quote currency.
/// Instrument facts (`oi_cap_usd` … `session`) are copied from the
/// `mkt_instrument/1` row / calendar by the tool; `None` = unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MarketCtx {
    pub id: InstrumentId,
    /// Venue timestamp of the context, else the read time (ms).
    pub as_of_ms: i64,
    pub listing: Listing,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub mark: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub oracle: Field<f64>,
    /// Index / reference price (CEX `indexPrice`); HL has `oracle` instead.
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub index: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub mid: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub bid: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub ask: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub last: Field<f64>,
    /// Price to sell / buy the venue's impact notional (HL `impactPxs`).
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub impact_bid: Field<f64>,
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub impact_ask: Field<f64>,
    /// Price 24 h ago (HL `prevDayPx`).
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub prev_day: Field<f64>,
    /// Venue premium as a fraction (HL `premium`).
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub premium: Field<f64>,
    /// Funding rate per hour as a fraction (HL is hourly; an 8 h rate ÷ 8).
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub funding_1h: Field<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_interval_h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_funding_ms: Option<i64>,
    /// Open interest in base units.
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub oi_base: Field<f64>,
    /// 24 h notional volume in USD (HL `dayNtlVlm`).
    #[serde(default = "absent", skip_serializing_if = "is_absent")]
    pub vol_24h_usd: Field<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oi_cap_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_oi_cap: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_leverage: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_isolated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growth_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker_fee_bps: Option<f64>,
    /// Session label from the calendar (`regular`, `closed`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// A listed market without a book (HL: null `premium` / `midPx` /
    /// `impactPxs`) — the row is `partial`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_book: bool,
}

fn val(f: &Field<f64>) -> Option<f64> {
    f.value().copied().filter(|x| x.is_finite())
}

fn pos(f: &Field<f64>) -> Option<f64> {
    val(f).filter(|x| *x > 0.0)
}

fn spread_bps(bid: f64, ask: f64) -> Option<f64> {
    let mid = (bid + ask) / 2.0;
    (mid > 0.0).then(|| (ask - bid) / mid * 1e4)
}

/// Short enum-like strings only; a longer value is omitted, never cut.
fn short(s: Option<&str>) -> Option<&str> {
    s.filter(|s| s.chars().count() <= MAX_FEATURE_STR)
}

impl MarketCtx {
    /// Listed, every field absent — decoders fill what the venue sent.
    pub(crate) fn new(id: InstrumentId, as_of_ms: i64) -> Self {
        Self {
            id,
            as_of_ms,
            listing: Listing::Listed,
            mark: Field::Absent,
            oracle: Field::Absent,
            index: Field::Absent,
            mid: Field::Absent,
            bid: Field::Absent,
            ask: Field::Absent,
            last: Field::Absent,
            impact_bid: Field::Absent,
            impact_ask: Field::Absent,
            prev_day: Field::Absent,
            premium: Field::Absent,
            funding_1h: Field::Absent,
            funding_interval_h: None,
            next_funding_ms: None,
            oi_base: Field::Absent,
            vol_24h_usd: Field::Absent,
            oi_cap_usd: None,
            at_oi_cap: None,
            max_leverage: None,
            only_isolated: None,
            category: None,
            growth_mode: None,
            taker_fee_bps: None,
            session: None,
            no_book: false,
        }
    }

    /// The venue does not know `id` (HL `200 null`) — an `absent` row.
    pub(crate) fn not_found(id: InstrumentId, as_of_ms: i64) -> Self {
        Self {
            listing: Listing::NotFound,
            ..Self::new(id, as_of_ms)
        }
    }

    /// The read failed — an `error` row carrying `error`.
    pub(crate) fn failed(id: InstrumentId, as_of_ms: i64, error: ReadError) -> Self {
        Self {
            mark: Field::err(error),
            ..Self::new(id, as_of_ms)
        }
    }

    fn prices(&self) -> [&Field<f64>; 7] {
        [
            &self.mark,
            &self.oracle,
            &self.index,
            &self.mid,
            &self.bid,
            &self.ask,
            &self.last,
        ]
    }

    fn fields(&self) -> [&Field<f64>; 14] {
        [
            &self.mark,
            &self.oracle,
            &self.index,
            &self.mid,
            &self.bid,
            &self.ask,
            &self.last,
            &self.impact_bid,
            &self.impact_ask,
            &self.prev_day,
            &self.premium,
            &self.funding_1h,
            &self.oi_base,
            &self.vol_24h_usd,
        ]
    }

    /// `oracle`, else `index` — what the venue marks against.
    pub(crate) fn reference(&self) -> Option<f64> {
        pos(&self.oracle).or_else(|| pos(&self.index))
    }

    /// `(mark − reference) / reference` in bps.
    pub(crate) fn basis_bps(&self) -> Option<f64> {
        let (m, r) = (pos(&self.mark)?, self.reference()?);
        Some((m - r) / r * 1e4)
    }

    pub(crate) fn spread_bps(&self) -> Option<f64> {
        spread_bps(pos(&self.bid)?, pos(&self.ask)?)
    }

    pub(crate) fn impact_spread_bps(&self) -> Option<f64> {
        spread_bps(pos(&self.impact_bid)?, pos(&self.impact_ask)?)
    }

    pub(crate) fn premium_bps(&self) -> Option<f64> {
        val(&self.premium).map(|p| p * 1e4)
    }

    /// `funding_1h × 8760 × 100`.
    pub(crate) fn funding_apr_pct(&self) -> Option<f64> {
        val(&self.funding_1h).map(|r| r * 8_760.0 * 100.0)
    }

    pub(crate) fn next_funding_s(&self) -> Option<f64> {
        self.next_funding_ms
            .map(|t| t.saturating_sub(self.as_of_ms).max(0) as f64 / 1000.0)
    }

    /// `oi_base × mark`.
    pub(crate) fn oi_usd(&self) -> Option<f64> {
        let oi = val(&self.oi_base).filter(|x| *x >= 0.0)?;
        Some(oi * pos(&self.mark)?)
    }

    pub(crate) fn oi_cap_used_pct(&self) -> Option<f64> {
        let cap = self.oi_cap_usd.filter(|c| c.is_finite() && *c > 0.0)?;
        Some(self.oi_usd()? / cap * 100.0)
    }

    /// `(mark / prev_day − 1) × 100`.
    pub(crate) fn change_24h_pct(&self) -> Option<f64> {
        let (m, p) = (pos(&self.mark)?, pos(&self.prev_day)?);
        Some((m / p - 1.0) * 100.0)
    }

    /// Mark pinned to the oracle (HL: no book, off-hours or delisted).
    pub(crate) fn oracle_eq_mark(&self) -> Option<bool> {
        let (m, o) = (pos(&self.mark)?, pos(&self.oracle)?);
        Some((m - o).abs() <= 1e-12 * m.max(o))
    }
}

impl Observed for MarketCtx {
    const SCHEMA: &'static str = "mkt_ctx/1";

    fn subject(&self) -> String {
        self.id.to_string()
    }

    fn headline(&self) -> String {
        match self.listing {
            Listing::Delisted => return format!("mkt {} delisted", self.id),
            Listing::NotFound => return format!("mkt {} not listed by the venue", self.id),
            Listing::Listed => {}
        }
        if self.prices().iter().all(|f| val(f).is_none()) {
            return format!("mkt {} unavailable", self.id);
        }
        let mut h = format!("mkt {}", self.id);
        for (name, f) in [
            ("mark", &self.mark),
            ("oracle", &self.oracle),
            ("index", &self.index),
        ] {
            if let Some(x) = val(f) {
                h.push_str(&format!(" {name}={}", fmt_sig(x, 7)));
            }
        }
        if let Some(b) = self.basis_bps() {
            h.push_str(&format!(" basis_bps={b:+.1}"));
        }
        if let Some(a) = self.funding_apr_pct() {
            h.push_str(&format!(" funding_apr_pct={a:+.2}"));
        }
        if let Some(oi) = self.oi_usd() {
            h.push_str(&format!(" oi_usd={}", fmt_sig(oi, 4)));
        }
        if self.no_book {
            h.push_str(" no_book");
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        for (name, field) in [
            ("mark", &self.mark),
            ("oracle", &self.oracle),
            ("index", &self.index),
            ("mid", &self.mid),
            ("bid", &self.bid),
            ("ask", &self.ask),
            ("last", &self.last),
            ("impact_bid", &self.impact_bid),
            ("impact_ask", &self.impact_ask),
            ("funding_1h", &self.funding_1h),
            ("oi_base", &self.oi_base),
            ("vol_24h_usd", &self.vol_24h_usd),
        ] {
            set_num(&mut f, name, val(field));
        }
        set_num(&mut f, "spread_bps", self.spread_bps());
        set_num(&mut f, "impact_spread_bps", self.impact_spread_bps());
        set_num(&mut f, "basis_bps", self.basis_bps());
        set_num(&mut f, "premium_bps", self.premium_bps());
        set_num(&mut f, "funding_apr_pct", self.funding_apr_pct());
        set_num(&mut f, "funding_interval_h", self.funding_interval_h);
        set_num(&mut f, "next_funding_s", self.next_funding_s());
        set_num(&mut f, "oi_usd", self.oi_usd());
        set_num(&mut f, "oi_cap_used_pct", self.oi_cap_used_pct());
        set_bool(&mut f, "at_oi_cap", self.at_oi_cap);
        set_num(&mut f, "change_24h_pct", self.change_24h_pct());
        set_int(&mut f, "max_leverage", self.max_leverage.map(i64::from));
        set_bool(&mut f, "only_isolated", self.only_isolated);
        set_bool(&mut f, "delisted", Some(self.listing == Listing::Delisted));
        set_str(&mut f, "session", short(self.session.as_deref()));
        set_str(&mut f, "category", self.category.map(Category::as_str));
        set_bool(&mut f, "growth_mode", self.growth_mode);
        set_num(&mut f, "taker_fee_bps", self.taker_fee_bps);
        set_bool(&mut f, "oracle_eq_mark", self.oracle_eq_mark());
        f
    }

    fn status(&self) -> ObsStatus {
        if self.listing != Listing::Listed {
            return ObsStatus::Absent;
        }
        let failed = self.fields().iter().any(|f| f.is_error());
        if self.prices().iter().all(|f| val(f).is_none()) {
            return if failed {
                ObsStatus::Error
            } else {
                ObsStatus::Absent
            };
        }
        if failed || self.no_book {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.fields()
            .iter()
            .filter_map(|f| f.error().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{
        assert_features_ok, ErrorClass, ObsSource, Observation, MAX_FEATURES, MAX_LINE1_CHARS,
    };

    const RH_TSLA: &str = "robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d";

    fn id(s: &str) -> InstrumentId {
        InstrumentId::parse(s).unwrap()
    }

    /// `xyz:TSLA` from `metaAndAssetCtxs` dex `xyz` + `perpDexs`, captured
    /// 2026-09-30 (`tests/fixtures/hyperliquid/`).
    fn tsla_ctx() -> MarketCtx {
        let mut c = MarketCtx::new(id("hyperliquid:xyz:TSLA"), 1_790_775_567_699);
        c.mark = Field::ok(347.19);
        c.oracle = Field::ok(346.91);
        c.mid = Field::ok(347.185);
        c.impact_bid = Field::ok(347.161);
        c.impact_ask = Field::ok(347.19);
        c.prev_day = Field::ok(352.73);
        c.premium = Field::ok(0.000_765_328_2);
        c.funding_1h = Field::ok(0.000_024_261_5);
        c.funding_interval_h = Some(1.0);
        c.next_funding_ms = Some(1_790_776_800_000);
        c.oi_base = Field::ok(157_613.11);
        c.vol_24h_usd = Field::ok(18_451_939.342_430_003);
        c.oi_cap_usd = Some(100_000_000.0);
        c.at_oi_cap = Some(false);
        c.max_leverage = Some(20);
        c.only_isolated = Some(false);
        c.category = Some(Category::Stocks);
        c.growth_mode = Some(true);
        c
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    fn num(f: &Features, k: &str) -> f64 {
        f[k].as_f64()
            .unwrap_or_else(|| panic!("{k} missing in {f:?}"))
    }

    #[test]
    fn decimal_strings_parse_strictly() {
        for (s, want) in [
            ("347.19", Some(347.19)),
            ("-0.0000015073", Some(-0.000_001_507_3)),
            ("0.0", Some(0.0)),
            ("+5", Some(5.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("1e-5", Some(1e-5)),
            ("2.5E3", Some(2_500.0)),
            ("20249511.1999999993", Some(20_249_511.199_999_999_3)),
            ("", None),
            ("-", None),
            (".", None),
            (" 1", None),
            ("1 ", None),
            ("NaN", None),
            ("inf", None),
            ("1e", None),
            ("1e+", None),
            ("0x10", None),
            ("1,5", None),
            ("1e999", None),
        ] {
            assert_eq!(parse_decimal(s), want, "{s:?}");
        }
        let ctx = serde_json::json!({
            "markPx": "347.19", "n": 2.5, "midPx": null, "bad": "abc", "obj": {"a": 1}
        });
        assert_eq!(decimal_field(&ctx, "markPx", "mark"), Field::ok(347.19));
        assert_eq!(decimal_field(&ctx, "n", "n"), Field::ok(2.5));
        assert_eq!(decimal_field(&ctx, "midPx", "mid"), Field::Absent);
        assert_eq!(decimal_field(&ctx, "impactPxs", "impact"), Field::Absent);
        let e = decimal_field(&ctx, "bad", "oracle");
        let e = e.error().unwrap();
        assert_eq!((e.field.as_str(), e.class), ("oracle", ErrorClass::Decode));
        assert_eq!(e.message, "bad is the string \"abc\", not a decimal");
        assert!(decimal_field(&ctx, "obj", "x").is_error());
    }

    #[test]
    fn instrument_ids_parse_and_render_verbatim() {
        for (s, venue, native) in [
            ("hyperliquid:xyz:TSLA", "hyperliquid", "xyz:TSLA"),
            ("hyperliquid:ETH", "hyperliquid", "ETH"),
            ("hyperliquid:@151", "hyperliquid", "@151"),
            (
                RH_TSLA,
                "robinhood",
                "0x322F0929c4625eD5bAd873c95208D54E1c003b2d",
            ),
            ("binance-usdm:TSLAUSDT", "binance-usdm", "TSLAUSDT"),
            ("ref:XNAS:TSLA", "ref:XNAS", "TSLA"),
        ] {
            let i = InstrumentId::parse(s).unwrap();
            assert_eq!((i.venue(), i.native()), (venue, native), "{s}");
            assert_eq!(i.to_string(), s);
            let json = serde_json::to_value(&i).unwrap();
            assert_eq!(json, serde_json::json!(s));
            assert_eq!(serde_json::from_value::<InstrumentId>(json).unwrap(), i);
        }
        assert_eq!(
            InstrumentId::hyperliquid("xyz:TSLA").unwrap(),
            id("hyperliquid:xyz:TSLA")
        );
        for bad in [
            "",
            "TSLA",
            "hl:xyz:TSLA",
            "hyperliquid:",
            "hyperliquid:xyz TSLA",
            "ref:NASDAQ:TSLA",
            "ref:xnas:TSLA",
            "ref:XNAS",
        ] {
            let e = InstrumentId::parse(bad).unwrap_err();
            assert!(e.contains(bad) || bad.is_empty(), "{bad}: {e}");
        }
        assert!(serde_json::from_value::<InstrumentId>(serde_json::json!("hl:ETH")).is_err());
    }

    #[test]
    fn keys_carry_the_full_instrument_id() {
        let o = Observation::of("hl_ctx", &tsla_ctx(), 0, 5_000, ObsSource::Live);
        assert_eq!(o.key, "mkt_ctx/1:hyperliquid:xyz:TSLA");
        let i = MarketInstrument::new(id(RH_TSLA), InstrumentKind::Spot, Listing::Listed);
        let o = Observation::of("rh_sync", &i, 0, 5_000, ObsSource::Live);
        assert_eq!(
            o.key,
            "mkt_instrument/1:robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d"
        );
        let spot = MarketCtx::new(id("hyperliquid:@151"), 0);
        assert_eq!(
            Observation::of("hl_ctx", &spot, 0, 5_000, ObsSource::Live).key,
            "mkt_ctx/1:hyperliquid:@151"
        );
    }

    #[test]
    fn category_normalises_venue_labels() {
        use Category::*;
        for (label, want) in [
            ("stocks", Some(Stocks)),
            ("stock", Some(Stocks)),
            ("FX", Some(Fx)),
            ("fx", Some(Fx)),
            ("indices", Some(Indices)),
            ("commodities", Some(Commodities)),
            ("preipo", Some(Preipo)),
            ("crypto", Some(Crypto)),
            ("rates", Some(Rates)),
            ("ETF", Some(Etf)),
            ("memes", None),
            ("", None),
        ] {
            assert_eq!(Category::parse(label), want, "{label:?}");
        }
        assert_eq!(MarginMode::parse("noCross"), Some(MarginMode::NoCross));
        assert_eq!(
            MarginMode::parse("strictIsolated"),
            Some(MarginMode::StrictIsolated)
        );
        assert_eq!(MarginMode::parse("cross"), None);
    }

    #[test]
    fn ctx_features_from_the_tsla_probe() {
        let c = tsla_ctx();
        let f = c.features();
        assert_features_ok(&f);
        assert!(close(num(&f, "basis_bps"), 8.071_257_674_900_485));
        assert!(close(num(&f, "premium_bps"), 7.653_282));
        assert!(close(num(&f, "funding_apr_pct"), 21.253_074));
        assert!(close(num(&f, "oi_usd"), 54_721_695.660_9));
        assert!(close(num(&f, "oi_cap_used_pct"), 54.721_695_660_9));
        assert!(close(num(&f, "change_24h_pct"), -1.570_606_412_837_016_7));
        assert!(close(num(&f, "impact_spread_bps"), 0.835_312_399_636_390_3));
        assert_eq!(num(&f, "next_funding_s"), 1_232.301);
        assert_eq!(f["category"], "stocks");
        assert_eq!(f["delisted"], false);
        assert_eq!(f["oracle_eq_mark"], false);
        assert_eq!(f["max_leverage"], 20);
        // HL ctx has no top of book: no bid / ask / spread, never 0.
        for k in ["bid", "ask", "spread_bps", "last", "index"] {
            assert!(!f.contains_key(k), "{k}");
        }
        assert_eq!(c.status(), ObsStatus::Ok);
        let h = c.headline();
        assert!(
            h.starts_with("mkt hyperliquid:xyz:TSLA mark=347.1900 oracle=346.9100 basis_bps=+8.1"),
            "{h}"
        );
    }

    #[test]
    fn every_field_set_fits_the_features_contract() {
        let mut c = tsla_ctx();
        c.index = Field::ok(346.95);
        c.bid = Field::ok(347.18);
        c.ask = Field::ok(347.19);
        c.last = Field::ok(347.18);
        c.taker_fee_bps = Some(4.5);
        c.session = Some("regular".into());
        let f = c.features();
        assert_features_ok(&f);
        assert_eq!(f.len(), 31);
        assert!(f.len() <= MAX_FEATURES);
        assert!(close(num(&f, "spread_bps"), 0.01 / 347.185 * 1e4));
        // Reference = oracle when both exist.
        assert!(close(num(&f, "basis_bps"), 8.071_257_674_900_485));
        c.oracle = Field::Absent;
        assert!(close(
            c.basis_bps().unwrap(),
            (347.19 - 346.95) / 346.95 * 1e4
        ));
        c.session = Some("s".repeat(MAX_FEATURE_STR + 1));
        assert!(
            !c.features().contains_key("session"),
            "long labels are omitted, not cut"
        );
    }

    #[test]
    fn ctx_statuses() {
        let e = || ReadError::new("oracle", ErrorClass::Decode, "not a decimal");
        assert_eq!(tsla_ctx().status(), ObsStatus::Ok);

        let mut no_book = tsla_ctx();
        no_book.mid = Field::Absent;
        no_book.impact_bid = Field::Absent;
        no_book.impact_ask = Field::Absent;
        no_book.premium = Field::Absent;
        no_book.no_book = true;
        assert_eq!(no_book.status(), ObsStatus::Partial);
        assert!(no_book.headline().ends_with(" no_book"));

        let mut bad_oracle = tsla_ctx();
        bad_oracle.oracle = Field::err(e());
        assert_eq!(bad_oracle.status(), ObsStatus::Partial);
        assert_eq!(bad_oracle.errors()[0].field, "oracle");
        assert!(!bad_oracle.features().contains_key("basis_bps"));

        let failed = MarketCtx::failed(
            id("hyperliquid:xyz:TSLA"),
            0,
            ReadError::new("ctx", ErrorClass::Timeout, "slow"),
        );
        assert_eq!(failed.status(), ObsStatus::Error);
        assert_eq!(failed.errors().len(), 1);
        assert_eq!(failed.headline(), "mkt hyperliquid:xyz:TSLA unavailable");

        let mut delisted = tsla_ctx();
        delisted.listing = Listing::Delisted;
        assert_eq!(delisted.status(), ObsStatus::Absent);
        assert_eq!(delisted.headline(), "mkt hyperliquid:xyz:TSLA delisted");
        assert_eq!(delisted.features()["delisted"], true);

        let nf = MarketCtx::not_found(id("hyperliquid:xyz:NOPE"), 0);
        assert_eq!(nf.status(), ObsStatus::Absent);
        assert_eq!(
            nf.headline(),
            "mkt hyperliquid:xyz:NOPE not listed by the venue"
        );

        // Listed, nothing read, nothing failed: absent (nothing to report).
        assert_eq!(
            MarketCtx::new(id("hyperliquid:ETH"), 0).status(),
            ObsStatus::Absent
        );
    }

    #[test]
    fn missing_inputs_are_omitted_never_zero() {
        let mut c = tsla_ctx();
        c.mark = Field::err(ReadError::new("mark", ErrorClass::Decode, "bad"));
        let f = c.features();
        for k in [
            "mark",
            "basis_bps",
            "oi_usd",
            "oi_cap_used_pct",
            "change_24h_pct",
            "oracle_eq_mark",
        ] {
            assert!(!f.contains_key(k), "{k} present: {f:?}");
        }
        assert_eq!(c.status(), ObsStatus::Partial, "oracle still readable");
        let mut z = tsla_ctx();
        z.prev_day = Field::ok(0.0);
        assert!(!z.features().contains_key("change_24h_pct"));
    }

    #[test]
    fn typed_round_trip_and_compact_data() {
        let o = Observation::of("hl_ctx", &tsla_ctx(), 0, 5_000, ObsSource::Live);
        assert_eq!(o.typed::<MarketCtx>().unwrap(), tsla_ctx());
        assert_eq!(o.data["id"], "hyperliquid:xyz:TSLA");
        assert!(
            o.data.get("bid").is_none(),
            "absent fields are not serialised"
        );
        assert!(o.data.get("no_book").is_none());
        let nf = MarketCtx::not_found(id("hyperliquid:xyz:NOPE"), 5);
        let o = Observation::of("hl_ctx", &nf, 0, 5_000, ObsSource::Live);
        assert_eq!(o.typed::<MarketCtx>().unwrap(), nf);
        assert_eq!(o.status, ObsStatus::Absent);
    }

    #[test]
    fn line1_keeps_full_ids() {
        let mut c = MarketCtx::new(id(RH_TSLA), 0);
        c.mark = Field::ok(347.123_456);
        c.oracle = Field::ok(347.0);
        c.funding_1h = Field::ok(0.000_1);
        c.oi_base = Field::ok(123_456_789.0);
        c.no_book = true;
        let o = Observation::of("rh_quote", &c, 0, 5_000, ObsSource::Live);
        let text = o.render_text(0);
        let line1 = text.lines().next().unwrap();
        assert!(line1.contains(RH_TSLA), "{line1}");
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
    }

    #[test]
    fn instrument_row_features_status_and_headline() {
        let mut i = MarketInstrument::new(
            id("hyperliquid:xyz:TSLA"),
            InstrumentKind::Perp,
            Listing::Listed,
        );
        i.dex = Some("xyz".into());
        i.asset_id = Some(110_001);
        i.category = Some(Category::Stocks);
        i.display_name = Some("Tesla".into());
        i.keywords = vec!["tesla".into(), "ev".into()];
        i.underlying = Some(Underlying {
            listing: Some("Nasdaq".into()),
            ticker: Some("TSLA".into()),
            ratio: Some("1.000000000000000000".into()),
            fx_converted: Some(false),
        });
        i.quote_ccy = Some(QuoteCcy::Usdc);
        i.sz_decimals = Some(3);
        i.max_leverage = Some(20);
        i.margin_mode = Some(MarginMode::Normal);
        i.only_isolated = Some(false);
        i.oi_cap_usd = Some(100_000_000.0);
        i.at_oi_cap = Some(false);
        i.deployer_fee_scale = Some(1.0);
        i.growth_mode = Some(true);
        let f = i.features();
        assert_features_ok(&f);
        assert!(f.len() <= MAX_FEATURES);
        assert_eq!(f["quote_ccy"], "USDC");
        assert_eq!(f["margin_mode"], "normal");
        assert_eq!(f["underlying_ratio"], 1.0);
        assert_eq!(i.status(), ObsStatus::Ok);
        assert_eq!(
            i.headline(),
            "instrument hyperliquid:xyz:TSLA perp listed stocks quote=USDC max_lev=20 oi_cap_usd=100000000"
        );
        let o = Observation::of("hl_ctx", &i, 0, 300_000, ObsSource::Live);
        assert_eq!(o.typed::<MarketInstrument>().unwrap(), i);
        assert_eq!(o.data["underlying"]["ratio"], "1.000000000000000000");

        i.at_oi_cap = None;
        i.errors.push(ReadError::new(
            "at_oi_cap",
            ErrorClass::Transient,
            "perpsAtOpenInterestCap failed",
        ));
        assert_eq!(i.status(), ObsStatus::Partial);
        assert!(!i.features().contains_key("at_oi_cap"));

        let nf = MarketInstrument::new(
            id("hyperliquid:xyz:NOPE"),
            InstrumentKind::Perp,
            Listing::NotFound,
        );
        assert_eq!(nf.status(), ObsStatus::Absent);
    }
}
