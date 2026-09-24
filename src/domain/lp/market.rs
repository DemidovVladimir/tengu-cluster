//! Market typed outputs — `sol_price` oracle price (`price_oracle/1`) and the
//! `dlmm_pools` pool list (`dlmm_pools/1`). Pure parsers over already-fetched
//! JSON; the tool glue (`tools/solana/{price,pools}.rs`) does the HTTP and
//! turns transport failures into `Field::Error` / `ReadError`.
//!
//! | Item | Input | Notes |
//! |---|---|---|
//! | [`parse_jupiter_price`] | `lite-api.jup.ag/price/v3?ids=<mint>` body | unpriced mint ⇒ `Absent` (Jupiter omits it) |
//! | [`parse_pyth`] | Hermes v2 `updates/price/latest` body | Hermes answers 401 today ⇒ the glue passes `Field::Error(AuthRequired)` |
//! | [`combine_price`] | both fields + optional DLMM pool price + previous row | Pyth only if ≤ 60 s old and conf/price ≤ 1 %, else Jupiter; no source ⇒ `usd = None` (never a stale price at any age) |
//! | [`parse_datapi_pools`] | `dlmm.datapi.meteora.ag/pools` body | datapi `apr` = 24 h fees / TVL in % per DAY ⇒ `fee_tvl_24h_pct`; `apr_pct` = × 365; sort + filter + limit here |
//!
//! Failed or meaningless values (0 price, 0 TVL) decode to `None`, never 0.

// Consumed by the `sol_price` / `dlmm_pools` tool glue and `lp_snapshot`
// (stage 3); unused in the binary until then.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::gates::{storm_update, PriceSample, StormInput};
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError,
};
#[cfg(test)]
use crate::domain::solana::ids;
use crate::domain::solana::Pubkey;

// ---------------------------------------------------------------------------
// Endpoints, TTLs, source-selection constants
// ---------------------------------------------------------------------------

/// `sol_price` cache TTL.
pub(crate) const PRICE_TTL_MS: u64 = 10_000;
/// `dlmm_pools` cache TTL.
pub(crate) const POOLS_TTL_MS: u64 = 60_000;

pub(crate) const JUPITER_PRICE_V3_URL: &str = "https://lite-api.jup.ag/price/v3";
pub(crate) const PYTH_HERMES_LATEST_URL: &str =
    "https://hermes.pyth.network/v2/updates/price/latest";
pub(crate) const DATAPI_POOLS_URL: &str = "https://dlmm.datapi.meteora.ag/pools";
/// Rows the glue asks datapi for; sort / filter / limit run over them.
/// datapi's own order is 24 h volume, so a page this large covers every
/// meaningful pool of a pair (SOL-USDC had 129 matches on 2026-09-24).
pub(crate) const DATAPI_PAGE_SIZE: u32 = 100;
/// `dlmm_pools` `limit` bounds (tool schema: 1-50, default 10).
pub(crate) const POOLS_LIMIT_MAX: u32 = 50;
pub(crate) const POOLS_LIMIT_DEFAULT: u32 = 10;

/// Pyth is selected only when its `publish_time` is within this many
/// seconds of now (either side: clock skew).
pub(crate) const PYTH_MAX_AGE_S: i64 = 60;
/// … and its confidence interval is at most this fraction of the price.
pub(crate) const PYTH_MAX_CONF_FRAC: f64 = 0.01;
/// The sample ring keeps the first sample of each bucket this long plus the
/// newest, bounding it to ≤ 74 samples over the 6-min window.
pub(crate) const MIN_SAMPLE_SPACING_MS: i64 = 5_000;
/// Relative tolerance of the datapi `apr` == fees_24h / tvl × 100 check.
const APR_CROSS_CHECK_REL_TOL: f64 = 1e-6;

/// Pyth Hermes feed ids (verified 2026-09-24 via
/// `benchmarks.pyth.network/v1/price_feeds`): `Crypto.SOL/USD`.
#[cfg(test)]
pub(crate) const PYTH_FEED_SOL_USD: &str =
    "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
/// `Crypto.USDC/USD`.
#[cfg(test)]
pub(crate) const PYTH_FEED_USDC_USD: &str =
    "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";

/// Hermes feed id for a mint; `None` ⇒ the glue passes `Field::Absent`.
/// (The `sol_price` tool takes an explicit `pyth_feed_id` arg instead.)
#[cfg(test)]
pub(crate) fn pyth_feed_id(mint: &str) -> Option<&'static str> {
    match mint {
        ids::WSOL => Some(PYTH_FEED_SOL_USD),
        ids::USDC => Some(PYTH_FEED_USDC_USD),
        _ => None,
    }
}

/// `https://lite-api.jup.ag/price/v3?ids=<mint>`.
pub(crate) fn jupiter_price_url(mint: &str) -> String {
    format!("{JUPITER_PRICE_V3_URL}?ids={}", percent_encode(mint))
}

/// `https://hermes.pyth.network/v2/updates/price/latest?ids[]=0x<feed>`.
pub(crate) fn pyth_latest_url(feed_id: &str) -> String {
    let feed = feed_id.trim_start_matches("0x");
    format!(
        "{PYTH_HERMES_LATEST_URL}?ids%5B%5D=0x{}",
        percent_encode(feed)
    )
}

/// `https://dlmm.datapi.meteora.ag/pools?page=1&page_size=<n>&query=<q>`.
pub(crate) fn datapi_pools_url(query: &str, page_size: u32) -> String {
    format!(
        "{DATAPI_POOLS_URL}?page=1&page_size={page_size}&query={}",
        percent_encode(query.trim())
    )
}

/// RFC 3986 percent-encoding of everything but the unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode_err(field: &str, message: impl Into<String>) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, message)
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

fn positive(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x > 0.0)
}

/// A short service error text from a JSON error body (`error` / `message` /
/// `detail`), whole or not at all (never cut).
fn service_error(v: &Value) -> Option<String> {
    ["error", "message", "detail"].iter().find_map(|k| {
        let e = v.get(*k)?;
        let s = match e {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        Some(if s.chars().count() <= 160 {
            s
        } else {
            format!("{} chars", s.chars().count())
        })
    })
}

// ---------------------------------------------------------------------------
// price_oracle/1
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PriceSource {
    Pyth,
    Jupiter,
}

impl PriceSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PriceSource::Pyth => "pyth",
            PriceSource::Jupiter => "jupiter",
        }
    }
}

/// Jupiter price v3 entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct JupiterPrice {
    /// `usdPrice` (> 0).
    pub usd: f64,
    /// `blockId` — the Jupiter backend's slot for this price.
    pub block_id: Option<u64>,
    /// `priceChange24h`, in percent.
    pub change_24h_pct: Option<f64>,
    /// `liquidity`, USD.
    pub liquidity_usd: Option<f64>,
}

/// Pyth Hermes latest price for one feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PythPrice {
    /// Full 64-hex-char feed id, no `0x`.
    pub feed_id: String,
    /// `price × 10^expo` (> 0).
    pub usd: f64,
    /// `conf × 10^expo` (≥ 0).
    pub conf_usd: f64,
    /// Unix seconds.
    pub publish_time: i64,
}

/// Why a successfully read Pyth price was not selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PythRejection {
    /// `publish_time` more than `PYTH_MAX_AGE_S` from now.
    Stale,
    /// `conf / price > PYTH_MAX_CONF_FRAC`.
    WideConf,
}

/// The DLMM pool price used as the second source: USD per `mint` (the glue
/// passes it only for a pool whose base is `mint` and whose quote is a USD
/// stable, else `Field::Error(NotApplicable)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PoolQuote {
    pub pool: String,
    pub price: Field<f64>,
}

/// `price_oracle/1:<mint>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OraclePrice {
    pub mint: String,
    /// Selected USD price; `None` (never 0) when no source is usable.
    pub usd: Option<f64>,
    pub source: Option<PriceSource>,
    pub jupiter: Field<JupiterPrice>,
    /// `Absent` when the mint has no Pyth feed.
    pub pyth: Field<PythPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pyth_rejected: Option<PythRejection>,
    /// |jupiter − pyth| / pyth × 10⁴, only when both are usable.
    pub divergence_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolQuote>,
    /// USD per mint from `pool` (when it read Ok and is > 0).
    pub pool_price: Option<f64>,
    /// (pool_price − usd) / usd × 10⁴, signed: positive = pool above oracle.
    pub pool_vs_oracle_bps: Option<f64>,
    /// Usable sources: fresh + tight Pyth, Jupiter, pool price.
    pub sources_ok: u32,
    /// `usd` is missing or rests on a single source (no cross-check).
    pub degraded: bool,
    /// Selected prices over the last ≤ 6 min, oldest first, carried from the
    /// previous row (first sample per `MIN_SAMPLE_SPACING_MS` bucket + the
    /// newest).
    pub samples: Vec<PriceSample>,
    /// |usd / reference − 1| × 100 against the oldest sample ≥ 4 min old
    /// (`gates::storm_update`); `None` without such a sample.
    pub move_5m_pct: Option<f64>,
}

/// Parse a Jupiter price v3 body for `mint`. The mint missing from a
/// well-formed map ⇒ `Absent` (Jupiter has no price for it); anything else
/// malformed ⇒ `Error(Decode)`.
pub(crate) fn parse_jupiter_price(v: &Value, mint: &str) -> Field<JupiterPrice> {
    const F: &str = "jupiter";
    let Some(map) = v.as_object() else {
        return Field::err(decode_err(F, "jupiter price v3: body is not a JSON object"));
    };
    let Some(entry) = map.get(mint).filter(|e| !e.is_null()) else {
        // `{}` or a map of other mints' entries = legitimately unpriced.
        let well_formed = map
            .values()
            .all(|e| e.get("usdPrice").is_some_and(Value::is_number));
        return if well_formed {
            Field::Absent
        } else {
            let msg = match service_error(v) {
                Some(e) => format!("jupiter price v3: error response: {e}"),
                None => "jupiter price v3: unexpected body shape".to_string(),
            };
            Field::err(decode_err(F, msg))
        };
    };
    let Some(usd) = positive(entry.get("usdPrice").and_then(Value::as_f64)) else {
        return Field::err(decode_err(
            F,
            format!("jupiter price v3: usdPrice missing or not > 0 for {mint}"),
        ));
    };
    Field::ok(JupiterPrice {
        usd,
        block_id: entry.get("blockId").and_then(Value::as_u64),
        change_24h_pct: finite(entry.get("priceChange24h").and_then(Value::as_f64)),
        liquidity_usd: finite(entry.get("liquidity").and_then(Value::as_f64)),
    })
}

/// An integer that Hermes sends as a decimal string (or, tolerated, a
/// JSON number).
fn int_str(v: Option<&Value>) -> Option<i128> {
    match v? {
        Value::String(s) => s.trim().parse::<i128>().ok(),
        Value::Number(n) => n.as_i64().map(i128::from),
        _ => None,
    }
}

/// Parse a Hermes v2 `updates/price/latest` body for `feed_id` (with or
/// without `0x`, any case): `parsed[i].price.{price, conf, expo,
/// publish_time}` with `usd = price × 10^expo`.
pub(crate) fn parse_pyth(v: &Value, feed_id: &str) -> Field<PythPrice> {
    const F: &str = "pyth";
    let want = feed_id.trim_start_matches("0x").to_ascii_lowercase();
    let Some(parsed) = v.get("parsed").and_then(Value::as_array) else {
        let msg = match service_error(v) {
            Some(e) => format!("pyth hermes: error response: {e}"),
            None => "pyth hermes: no `parsed` array".to_string(),
        };
        return Field::err(decode_err(F, msg));
    };
    let Some(entry) = parsed.iter().find(|e| {
        e.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.trim_start_matches("0x").eq_ignore_ascii_case(&want))
    }) else {
        return Field::err(decode_err(
            F,
            format!("pyth hermes: feed {want} not in response"),
        ));
    };
    let p = entry.get("price");
    let price = int_str(p.and_then(|p| p.get("price")));
    let conf = int_str(p.and_then(|p| p.get("conf")));
    let expo = p.and_then(|p| p.get("expo")).and_then(Value::as_i64);
    let publish_time = p
        .and_then(|p| p.get("publish_time"))
        .and_then(Value::as_i64);
    let (Some(price), Some(conf), Some(expo), Some(publish_time)) =
        (price, conf, expo, publish_time)
    else {
        return Field::err(decode_err(
            F,
            format!("pyth hermes: feed {want}: price/conf/expo/publish_time missing"),
        ));
    };
    if !(-30..=30).contains(&expo) {
        return Field::err(decode_err(
            F,
            format!("pyth hermes: feed {want}: expo {expo} out of range"),
        ));
    }
    let scale = 10f64.powi(expo as i32);
    let usd = price as f64 * scale;
    let conf_usd = conf as f64 * scale;
    if !(usd.is_finite() && usd > 0.0 && conf_usd.is_finite() && conf_usd >= 0.0) {
        return Field::err(decode_err(
            F,
            format!("pyth hermes: feed {want}: non-positive price or negative conf"),
        ));
    }
    Field::ok(PythPrice {
        feed_id: want,
        usd,
        conf_usd,
        publish_time,
    })
}

/// Why `p` may not be selected at `now_ms`; `None` = usable.
pub(crate) fn pyth_rejection(p: &PythPrice, now_ms: i64) -> Option<PythRejection> {
    let age_s = now_ms.div_euclid(1000) - p.publish_time;
    let conf_frac = p.conf_usd / p.usd;
    if age_s.abs() > PYTH_MAX_AGE_S {
        Some(PythRejection::Stale)
    } else if !conf_frac.is_finite() || conf_frac > PYTH_MAX_CONF_FRAC {
        Some(PythRejection::WideConf)
    } else {
        None
    }
}

/// Keep the first (oldest) sample of every `MIN_SAMPLE_SPACING_MS` bucket
/// plus the newest sample; two samples at the same instant keep the later
/// one. Input oldest first. The oldest sample — the storm reference
/// candidate — is always kept.
fn thin_samples(samples: Vec<PriceSample>) -> Vec<PriceSample> {
    let bucket = |s: &PriceSample| s.t_ms.div_euclid(MIN_SAMPLE_SPACING_MS);
    let n = samples.len();
    let mut out: Vec<PriceSample> = Vec::with_capacity(n);
    for (i, s) in samples.into_iter().enumerate() {
        let newest = i + 1 == n;
        match out.last_mut() {
            Some(k) if k.t_ms == s.t_ms => *k = s,
            Some(k) if !newest && bucket(k) == bucket(&s) => {}
            _ => out.push(s),
        }
    }
    out
}

/// Select the oracle price for `mint` from Pyth (≤ 60 s old, conf ≤ 1 %)
/// else Jupiter, cross-check against the optional pool price, and advance
/// the sample ring carried in `prev` (ignored unless `prev.mint == mint`).
/// `prev.usd` is NEVER used as a price: with no usable source `usd = None`.
///
/// Glue contract: transport failures arrive as `Field::Error` with
/// `ReadError.field` = `"jupiter"` / `"pyth"` / `"pool_price"` (Hermes 401 ⇒
/// `AuthRequired`); a mint without a Pyth feed ([`pyth_feed_id`] = `None`)
/// passes `Field::Absent`; `prev` = the stored `price_oracle/1:<mint>` row
/// at any age (only its samples are used).
pub(crate) fn combine_price(
    mint: &str,
    jupiter: Field<JupiterPrice>,
    pyth: Field<PythPrice>,
    pool: Option<PoolQuote>,
    prev: Option<&OraclePrice>,
    now_ms: i64,
) -> OraclePrice {
    let pyth_rejected = pyth.value().and_then(|p| pyth_rejection(p, now_ms));
    let pyth_ok = pyth.value().filter(|_| pyth_rejected.is_none());
    let jup_ok = jupiter.value();
    let (usd, source) = match (pyth_ok, jup_ok) {
        (Some(p), _) => (Some(p.usd), Some(PriceSource::Pyth)),
        (None, Some(j)) => (Some(j.usd), Some(PriceSource::Jupiter)),
        (None, None) => (None, None),
    };
    let divergence_bps = match (pyth_ok, jup_ok) {
        (Some(p), Some(j)) => finite(Some((j.usd - p.usd).abs() / p.usd * 1e4)),
        _ => None,
    };

    let pool_price = pool
        .as_ref()
        .and_then(|q| positive(q.price.value().copied()));
    let pool_vs_oracle_bps = match (pool_price, usd) {
        (Some(pp), Some(u)) => finite(Some((pp - u) / u * 1e4)),
        _ => None,
    };
    let sources_ok =
        pyth_ok.is_some() as u32 + jup_ok.is_some() as u32 + pool_price.is_some() as u32;

    let carried: Vec<PriceSample> = prev
        .filter(|p| p.mint == mint)
        .map(|p| {
            p.samples
                .iter()
                .copied()
                .filter(|s| s.t_ms <= now_ms && s.usd.is_finite() && s.usd > 0.0)
                .collect()
        })
        .unwrap_or_default();
    let storm = storm_update(&StormInput {
        now_ms,
        price: usd.unwrap_or(f64::NAN),
        samples: carried,
        // ≤ 0 disables the storm state; only the window + move are used.
        threshold_pct: 0.0,
        active: false,
    });

    OraclePrice {
        mint: mint.to_string(),
        usd,
        source,
        jupiter,
        pyth,
        pyth_rejected,
        divergence_bps,
        pool,
        pool_price,
        pool_vs_oracle_bps,
        sources_ok,
        degraded: usd.is_none() || sources_ok < 2,
        samples: thin_samples(storm.samples),
        move_5m_pct: storm.move_5m_pct,
    }
}

impl OraclePrice {
    fn source_errors(&self) -> Vec<ReadError> {
        let mut out = Vec::new();
        if let Some(e) = self.jupiter.error() {
            out.push(e.clone());
        }
        if let Some(e) = self.pyth.error() {
            out.push(e.clone());
        }
        if let Some(q) = &self.pool {
            match &q.price {
                Field::Error { error } => out.push(error.clone()),
                Field::Ok { value } if self.pool_price.is_none() => out.push(decode_err(
                    "pool_price",
                    format!("pool {} price {value} is not > 0", q.pool),
                )),
                _ => {}
            }
        }
        out
    }
}

impl Observed for OraclePrice {
    const SCHEMA: &'static str = "price_oracle/1";

    fn subject(&self) -> String {
        self.mint.clone()
    }

    fn headline(&self) -> String {
        let mut h = format!("price {}", self.mint);
        match (self.usd, self.source) {
            (Some(u), Some(s)) => h.push_str(&format!(" usd={u:.6} via {}", s.as_str())),
            _ => h.push_str(" usd=unavailable"),
        }
        if let Some(q) = &self.pool {
            h.push_str(&format!(" pool={}", q.pool));
            if let Some(bps) = self.pool_vs_oracle_bps {
                h.push_str(&format!(" pool_vs_oracle_bps={bps:+.1}"));
            }
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_num(&mut f, "usd", self.usd);
        set_str(&mut f, "source", self.source.map(PriceSource::as_str));
        set_int(&mut f, "sources_ok", Some(self.sources_ok as i64));
        set_bool(&mut f, "degraded", Some(self.degraded));
        set_num(&mut f, "divergence_bps", self.divergence_bps);
        set_num(&mut f, "move_5m_pct", self.move_5m_pct);
        set_int(&mut f, "n_samples", Some(self.samples.len() as i64));
        set_num(&mut f, "pool_price", self.pool_price);
        set_num(&mut f, "pool_vs_oracle_bps", self.pool_vs_oracle_bps);
        if let Some(j) = self.jupiter.value() {
            set_num(&mut f, "jupiter_usd", Some(j.usd));
            set_num(&mut f, "jupiter_change_24h_pct", j.change_24h_pct);
        }
        if let Some(p) = self.pyth.value() {
            set_num(&mut f, "pyth_usd", Some(p.usd));
            set_num(&mut f, "pyth_conf_bps", Some(p.conf_usd / p.usd * 1e4));
        }
        set_str(
            &mut f,
            "pyth_rejected",
            self.pyth_rejected.map(|r| match r {
                PythRejection::Stale => "stale",
                PythRejection::WideConf => "wide_conf",
            }),
        );
        f
    }

    fn status(&self) -> ObsStatus {
        let source_errors = !self.source_errors().is_empty();
        match self.usd {
            Some(_) if source_errors => ObsStatus::Partial,
            Some(_) => ObsStatus::Ok,
            None if !source_errors
                && self.pyth_rejected.is_none()
                && matches!(self.jupiter, Field::Absent) =>
            {
                // No source has a price for this mint at all.
                ObsStatus::Absent
            }
            None => ObsStatus::Error,
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        let mut out = self.source_errors();
        if self.usd.is_none() && out.is_empty() {
            if let Some(r) = self.pyth_rejected {
                out.push(ReadError::new(
                    "usd",
                    ErrorClass::Transient,
                    format!(
                        "no usable source: pyth {} and no jupiter price",
                        match r {
                            PythRejection::Stale => "stale",
                            PythRejection::WideConf => "confidence too wide",
                        }
                    ),
                ));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// dlmm_pools/1
// ---------------------------------------------------------------------------

/// `dlmm_pools` sort order (always descending; `None` values last, ties by
/// address).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PoolSort {
    #[serde(rename = "fee_tvl_24h")]
    FeeTvl24h,
    #[serde(rename = "tvl")]
    Tvl,
    #[serde(rename = "volume_24h")]
    Volume24h,
}

impl PoolSort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PoolSort::FeeTvl24h => "fee_tvl_24h",
            PoolSort::Tvl => "tvl",
            PoolSort::Volume24h => "volume_24h",
        }
    }
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "fee_tvl_24h" => Some(PoolSort::FeeTvl24h),
            "tvl" => Some(PoolSort::Tvl),
            "volume_24h" => Some(PoolSort::Volume24h),
            _ => None,
        }
    }
    fn key(self, r: &DlmmPoolRow) -> Option<f64> {
        match self {
            PoolSort::FeeTvl24h => r.fee_tvl_24h_pct,
            PoolSort::Tvl => Some(r.tvl_usd),
            PoolSort::Volume24h => r.volume_24h_usd,
        }
    }
}

/// One datapi pool row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmPoolRow {
    pub address: String,
    pub name: String,
    pub mint_x: String,
    pub mint_y: String,
    pub bin_step: u16,
    /// `pool_config.base_fee_pct`, percent.
    pub base_fee_pct: f64,
    /// `dynamic_fee_pct`, percent.
    pub dynamic_fee_pct: Option<f64>,
    pub tvl_usd: f64,
    pub volume_24h_usd: Option<f64>,
    pub fees_24h_usd: Option<f64>,
    /// datapi `apr` = 24 h fees / TVL × 100 (percent per DAY); `None` when
    /// TVL is 0 or `apr` disagrees with fees / TVL.
    pub fee_tvl_24h_pct: Option<f64>,
    /// `fee_tvl_24h_pct × 365` (simple, not compounded).
    pub apr_pct: Option<f64>,
    /// datapi `current_price` (quote per base); `None` when ≤ 0.
    pub current_price: Option<f64>,
    pub is_blacklisted: bool,
}

/// `dlmm_pools/1:<query>|<sort>|<limit>|<min_tvl>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmPoolList {
    pub query: String,
    pub sort: PoolSort,
    pub limit: u32,
    pub min_tvl_usd: Option<f64>,
    /// datapi `total` (server-side matches for the query, all pages).
    pub total: u32,
    /// Rows in the fetched page (sort / filter ran over these).
    pub fetched: u32,
    /// Decoded rows passing `min_tvl_usd`, before `limit`.
    pub matched: u32,
    pub pools: Vec<DlmmPoolRow>,
    pub status: ObsStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

impl DlmmPoolList {
    /// Cache subject for the tool args (the glue builds its `CachePolicy`
    /// before fetching). `min_tvl` renders `0` when absent.
    pub(crate) fn subject_for(
        query: &str,
        sort: PoolSort,
        limit: u32,
        min_tvl_usd: Option<f64>,
    ) -> String {
        format!(
            "{}|{}|{}|{}",
            query.trim(),
            sort.as_str(),
            clamp_limit(limit),
            min_tvl_usd
                .filter(|m| m.is_finite() && *m > 0.0)
                .unwrap_or(0.0)
        )
    }

    /// The `Error` row for a failed fetch (HTTP error, timeout, non-JSON
    /// body): no pools, the error kept. Never cached (`observe` skips
    /// `Error` rows).
    pub(crate) fn failed(
        query: &str,
        sort: PoolSort,
        limit: u32,
        min_tvl_usd: Option<f64>,
        error: ReadError,
    ) -> Self {
        let mut list = Self::empty(query, sort, limit, min_tvl_usd);
        list.status = ObsStatus::Error;
        list.errors.push(error);
        list
    }

    fn empty(query: &str, sort: PoolSort, limit: u32, min_tvl_usd: Option<f64>) -> Self {
        DlmmPoolList {
            query: query.trim().to_string(),
            sort,
            limit: clamp_limit(limit),
            min_tvl_usd: min_tvl_usd.filter(|m| m.is_finite() && *m > 0.0),
            total: 0,
            fetched: 0,
            matched: 0,
            pools: Vec::new(),
            status: ObsStatus::Ok,
            errors: Vec::new(),
        }
    }
}

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, POOLS_LIMIT_MAX)
}

/// Decode one datapi row. Required: address, token_x/y address, bin step,
/// base fee, tvl.
fn parse_pool_row(p: &Value) -> Result<(DlmmPoolRow, bool), String> {
    let pubkey = |v: Option<&Value>, what: &str| -> Result<String, String> {
        let s = v
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{what} missing"))?;
        s.parse::<Pubkey>()
            .map(|k| k.to_string())
            .map_err(|_| format!("{what} {s} is not a pubkey"))
    };
    let address = pubkey(p.get("address"), "address")?;
    let mint_x = pubkey(p.pointer("/token_x/address"), "token_x.address")?;
    let mint_y = pubkey(p.pointer("/token_y/address"), "token_y.address")?;
    let bin_step = p
        .pointer("/pool_config/bin_step")
        .and_then(Value::as_u64)
        .and_then(|b| u16::try_from(b).ok())
        .ok_or_else(|| format!("pool {address}: pool_config.bin_step missing"))?;
    let base_fee_pct = finite(
        p.pointer("/pool_config/base_fee_pct")
            .and_then(Value::as_f64),
    )
    .ok_or_else(|| format!("pool {address}: pool_config.base_fee_pct missing"))?;
    let tvl_usd = finite(p.get("tvl").and_then(Value::as_f64))
        .filter(|t| *t >= 0.0)
        .ok_or_else(|| format!("pool {address}: tvl missing"))?;
    let fees_24h_usd = finite(p.pointer("/fees/24h").and_then(Value::as_f64));
    let apr = finite(p.get("apr").and_then(Value::as_f64));

    // datapi `apr` is the daily fee/TVL percent; cross-check it against
    // fees / tvl so a semantics change never passes silently.
    let mut apr_mismatch = false;
    let fee_tvl_24h_pct = if tvl_usd > 0.0 {
        match (apr, fees_24h_usd) {
            (Some(a), Some(fees)) => {
                let calc = fees / tvl_usd * 100.0;
                if (a - calc).abs() <= APR_CROSS_CHECK_REL_TOL * a.abs().max(calc.abs()).max(1.0) {
                    Some(a)
                } else {
                    apr_mismatch = true;
                    None
                }
            }
            (Some(a), None) => Some(a),
            (None, Some(fees)) => Some(fees / tvl_usd * 100.0),
            (None, None) => None,
        }
    } else {
        None
    };

    Ok((
        DlmmPoolRow {
            name: p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            address,
            mint_x,
            mint_y,
            bin_step,
            base_fee_pct,
            dynamic_fee_pct: finite(p.get("dynamic_fee_pct").and_then(Value::as_f64)),
            tvl_usd,
            volume_24h_usd: finite(p.pointer("/volume/24h").and_then(Value::as_f64)),
            fees_24h_usd,
            fee_tvl_24h_pct,
            apr_pct: fee_tvl_24h_pct.map(|d| d * 365.0),
            current_price: positive(p.get("current_price").and_then(Value::as_f64)),
            is_blacklisted: p
                .get("is_blacklisted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        apr_mismatch,
    ))
}

/// Parse a datapi `/pools` body, then filter (`tvl_usd ≥ min_tvl_usd`), sort
/// (descending by `sort`, `None` last, ties by address) and keep `limit`
/// (clamped to 1-50). Undecodable rows are skipped and reported
/// (`Partial`); a body without a `data` array is `Error`; no rows left is
/// `Absent`.
pub(crate) fn parse_datapi_pools(
    v: &Value,
    query: &str,
    sort: PoolSort,
    limit: u32,
    min_tvl_usd: Option<f64>,
) -> DlmmPoolList {
    let Some(data) = v.get("data").and_then(Value::as_array) else {
        let msg = match service_error(v) {
            Some(e) => format!("datapi pools: error response: {e}"),
            None => "datapi pools: no `data` array".to_string(),
        };
        return DlmmPoolList::failed(query, sort, limit, min_tvl_usd, decode_err("pools", msg));
    };
    let mut list = DlmmPoolList::empty(query, sort, limit, min_tvl_usd);
    list.fetched = data.len() as u32;
    list.total = v
        .get("total")
        .and_then(Value::as_u64)
        .map(|t| t.min(u32::MAX as u64) as u32)
        .unwrap_or(list.fetched);

    let mut rows = Vec::with_capacity(data.len());
    let (mut bad, mut first_bad, mut mismatched) = (0u32, None::<String>, Vec::<String>::new());
    for p in data {
        match parse_pool_row(p) {
            Ok((row, apr_mismatch)) => {
                if apr_mismatch {
                    mismatched.push(row.address.clone());
                }
                rows.push(row);
            }
            Err(e) => {
                bad += 1;
                first_bad.get_or_insert(e);
            }
        }
    }
    if bad > 0 {
        list.errors.push(decode_err(
            "pools",
            format!(
                "{bad} datapi row(s) skipped; first: {}",
                first_bad.unwrap_or_default()
            ),
        ));
    }
    if !mismatched.is_empty() {
        list.errors.push(decode_err(
            "fee_tvl_24h_pct",
            format!(
                "datapi apr != fees_24h / tvl x 100 for {} pool(s), first {}; fee/TVL left empty",
                mismatched.len(),
                mismatched[0]
            ),
        ));
    }

    if let Some(min) = list.min_tvl_usd {
        rows.retain(|r| r.tvl_usd >= min);
    }
    list.matched = rows.len() as u32;
    rows.sort_by(|a, b| {
        let (ka, kb) = (sort.key(a), sort.key(b));
        match (ka, kb) {
            (Some(x), Some(y)) => y.total_cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.address.cmp(&b.address))
    });
    rows.truncate(list.limit as usize);
    list.pools = rows;
    list.status = if !list.errors.is_empty() {
        ObsStatus::Partial
    } else if list.pools.is_empty() {
        ObsStatus::Absent
    } else {
        ObsStatus::Ok
    };
    list
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    Some(if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    })
}

impl Observed for DlmmPoolList {
    const SCHEMA: &'static str = "dlmm_pools/1";

    fn subject(&self) -> String {
        Self::subject_for(&self.query, self.sort, self.limit, self.min_tvl_usd)
    }

    fn headline(&self) -> String {
        let tail = {
            let mut t = format!(
                " sort={} n={}/{}",
                self.sort.as_str(),
                self.pools.len(),
                self.total
            );
            if let Some(top) = self.pools.first() {
                t.push_str(&format!(" top={}", top.address));
                if let Some(ft) = top.fee_tvl_24h_pct {
                    t.push_str(&format!(" fee_tvl_24h_pct={ft:.4}"));
                }
            }
            t
        };
        let with_query = format!("dlmm_pools query={}{tail}", self.query);
        // The query is free text, not an id: drop it (it is in `data`)
        // rather than push line 1 past its budget.
        if with_query.chars().count() <= 165 {
            with_query
        } else {
            format!("dlmm_pools{tail}")
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "n_pools", Some(self.pools.len() as i64));
        set_int(&mut f, "total", Some(self.total as i64));
        set_int(&mut f, "matched", Some(self.matched as i64));
        set_str(&mut f, "sort", Some(self.sort.as_str()));
        set_num(
            &mut f,
            "max_fee_tvl_24h_pct",
            self.pools
                .iter()
                .filter_map(|r| r.fee_tvl_24h_pct)
                .max_by(f64::total_cmp),
        );
        set_num(
            &mut f,
            "median_tvl_usd",
            median(self.pools.iter().map(|r| r.tvl_usd).collect()),
        );
        set_int(
            &mut f,
            "n_blacklisted",
            Some(self.pools.iter().filter(|r| r.is_blacklisted).count() as i64),
        );
        if let Some(top) = self.pools.first() {
            set_num(&mut f, "top_fee_tvl_24h_pct", top.fee_tvl_24h_pct);
            set_num(&mut f, "top_apr_pct", top.apr_pct);
            set_num(&mut f, "top_tvl_usd", Some(top.tvl_usd));
            set_num(&mut f, "top_volume_24h_usd", top.volume_24h_usd);
            set_int(&mut f, "top_bin_step", Some(top.bin_step as i64));
            set_num(&mut f, "top_base_fee_pct", Some(top.base_fee_pct));
        }
        f
    }

    fn status(&self) -> ObsStatus {
        self.status
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_LINE1_CHARS};

    const JUP_V3: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/jupiter_price_v3.json"
    ));
    const JUP_V3_UNKNOWN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/jupiter_price_v3_unknown.json"
    ));
    const PYTH_DOC: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/pyth_hermes_v2_latest_documented.json"
    ));
    const POOLS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/datapi_pools_sol_usdc.json"
    ));
    const POOLS_P7: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/datapi_pools_sol_usdc_p7.json"
    ));
    const POOLS_EMPTY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/datapi_pools_empty.json"
    ));

    /// Capture time of the Jupiter fixture (unix s) — also the documented
    /// Pyth fixture's `publish_time`.
    const T0_S: i64 = 1_790_272_034;
    const T0_MS: i64 = T0_S * 1000;
    const POOL_SOL_USDC: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    const UNKNOWN_MINT: &str = "11111111111111111111111111111111";

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn jup(usd: f64) -> Field<JupiterPrice> {
        Field::ok(JupiterPrice {
            usd,
            block_id: Some(450_100_683),
            change_24h_pct: Some(1.5),
            liquidity_usd: None,
        })
    }

    fn pyth(usd: f64, conf_usd: f64, publish_time: i64) -> Field<PythPrice> {
        Field::ok(PythPrice {
            feed_id: PYTH_FEED_SOL_USD.into(),
            usd,
            conf_usd,
            publish_time,
        })
    }

    fn auth_err() -> Field<PythPrice> {
        Field::err(ReadError::new(
            "pyth",
            ErrorClass::AuthRequired,
            "hermes.pyth.network answered HTTP 401",
        ))
    }

    fn line1_ok(o: &Observation, now_ms: i64) {
        let text = o.render_text(now_ms);
        let line1 = text.lines().next().unwrap();
        assert!(
            line1.chars().count() <= MAX_LINE1_CHARS,
            "{} chars: {line1}",
            line1.chars().count()
        );
    }

    // ── Jupiter ──────────────────────────────────────────────────

    #[test]
    fn jupiter_fixture_parses_wsol_and_usdc() {
        let body = v(JUP_V3);
        let sol = parse_jupiter_price(&body, ids::WSOL);
        let sol = sol.value().expect("wSOL priced");
        // Values read independently from the raw fixture (python json).
        assert_eq!(sol.usd, 116.6084160651512);
        assert_eq!(sol.block_id, Some(450_100_683));
        assert_eq!(sol.change_24h_pct, Some(1.674225995566499));
        assert_eq!(sol.liquidity_usd, Some(949037209.8592238));
        let usdc = parse_jupiter_price(&body, ids::USDC);
        assert_eq!(usdc.value().unwrap().usd, 0.9998057536289426);
    }

    #[test]
    fn jupiter_unpriced_mint_is_absent_not_zero() {
        assert_eq!(
            parse_jupiter_price(&v(JUP_V3_UNKNOWN), UNKNOWN_MINT),
            Field::Absent
        );
        // A mint missing from a map of other well-formed entries: Absent too.
        assert_eq!(parse_jupiter_price(&v(JUP_V3), UNKNOWN_MINT), Field::Absent);
    }

    #[test]
    fn jupiter_malformed_bodies_are_decode_errors() {
        for body in [
            json!([1, 2]),
            json!({"error": "rate limited"}),
            json!({"data": {ids::WSOL: {"price": "1"}}}),
            json!({(ids::WSOL): {"usdPrice": 0.0}}),
            json!({(ids::WSOL): {"usdPrice": -3.0}}),
            json!({(ids::WSOL): {"blockId": 1}}),
        ] {
            let f = parse_jupiter_price(&body, ids::WSOL);
            let e = f.error().unwrap_or_else(|| panic!("{body}: {f:?}"));
            assert_eq!(e.class, ErrorClass::Decode, "{body}");
            assert_eq!(e.field, "jupiter");
        }
        let e = parse_jupiter_price(&json!({"error": "rate limited"}), ids::WSOL);
        assert!(e.error().unwrap().message.contains("rate limited"));
    }

    // ── Pyth ─────────────────────────────────────────────────────

    #[test]
    fn pyth_documented_shape_parses() {
        let body = v(PYTH_DOC);
        for id in [
            PYTH_FEED_SOL_USD.to_string(),
            format!("0x{PYTH_FEED_SOL_USD}"),
            PYTH_FEED_SOL_USD.to_ascii_uppercase(),
        ] {
            let p = parse_pyth(&body, &id);
            let p = p.value().unwrap_or_else(|| panic!("{id}: {p:?}"));
            assert_eq!(p.feed_id, PYTH_FEED_SOL_USD);
            // 11660841606 × 10^-8 and 5834021 × 10^-8 (python).
            assert!((p.usd - 116.60841606).abs() < 1e-9, "{}", p.usd);
            assert!((p.conf_usd - 0.05834021).abs() < 1e-12, "{}", p.conf_usd);
            assert_eq!(p.publish_time, T0_S);
        }
    }

    #[test]
    fn pyth_failures_are_decode_errors() {
        let other = parse_pyth(&v(PYTH_DOC), PYTH_FEED_USDC_USD);
        assert!(other.error().unwrap().message.contains(PYTH_FEED_USDC_USD));
        let mut neg = v(PYTH_DOC);
        neg["parsed"][0]["price"]["price"] = json!("-5");
        assert!(parse_pyth(&neg, PYTH_FEED_SOL_USD).is_error());
        let mut zero = v(PYTH_DOC);
        zero["parsed"][0]["price"]["price"] = json!("0");
        assert!(parse_pyth(&zero, PYTH_FEED_SOL_USD).is_error());
        let mut no_expo = v(PYTH_DOC);
        no_expo["parsed"][0]["price"]
            .as_object_mut()
            .unwrap()
            .remove("expo");
        assert!(parse_pyth(&no_expo, PYTH_FEED_SOL_USD).is_error());
        let e = parse_pyth(&json!("unauthorized"), PYTH_FEED_SOL_USD);
        assert_eq!(e.error().unwrap().class, ErrorClass::Decode);
    }

    #[test]
    fn pyth_feed_map_and_urls() {
        assert_eq!(pyth_feed_id(ids::WSOL), Some(PYTH_FEED_SOL_USD));
        assert_eq!(pyth_feed_id(ids::USDC), Some(PYTH_FEED_USDC_USD));
        assert_eq!(pyth_feed_id(UNKNOWN_MINT), None);
        assert_eq!(
            jupiter_price_url(ids::WSOL),
            "https://lite-api.jup.ag/price/v3?ids=So11111111111111111111111111111111111111112"
        );
        assert_eq!(
            pyth_latest_url(&format!("0x{PYTH_FEED_SOL_USD}")),
            "https://hermes.pyth.network/v2/updates/price/latest?ids%5B%5D=0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"
        );
        assert_eq!(
            datapi_pools_url(" SOL-USDC ", DATAPI_PAGE_SIZE),
            "https://dlmm.datapi.meteora.ag/pools?page=1&page_size=100&query=SOL-USDC"
        );
        assert_eq!(
            datapi_pools_url("SOL/USDC&x=1 #", 10),
            "https://dlmm.datapi.meteora.ag/pools?page=1&page_size=10&query=SOL%2FUSDC%26x%3D1%20%23"
        );
    }

    // ── combine_price ────────────────────────────────────────────

    #[test]
    fn fixtures_combine_to_pyth_with_divergence() {
        let o = combine_price(
            ids::WSOL,
            parse_jupiter_price(&v(JUP_V3), ids::WSOL),
            parse_pyth(&v(PYTH_DOC), PYTH_FEED_SOL_USD),
            None,
            None,
            T0_MS + 5_000,
        );
        assert_eq!(o.source, Some(PriceSource::Pyth));
        assert!((o.usd.unwrap() - 116.60841606).abs() < 1e-9);
        // |116.6084160651512 − 116.60841606| / 116.60841606 × 1e4 (python).
        assert!((o.divergence_bps.unwrap() - 4.417526311252498e-07).abs() < 1e-12);
        assert_eq!(o.sources_ok, 2);
        assert!(!o.degraded);
        assert_eq!(o.status(), ObsStatus::Ok);
        assert!(o.errors().is_empty());
    }

    #[test]
    fn stale_or_wide_pyth_falls_back_to_jupiter() {
        let now = T0_MS;
        let stale = combine_price(
            ids::WSOL,
            jup(116.0),
            pyth(116.5, 0.05, T0_S - 61),
            None,
            None,
            now,
        );
        assert_eq!(stale.source, Some(PriceSource::Jupiter));
        assert_eq!(stale.usd, Some(116.0));
        assert_eq!(stale.pyth_rejected, Some(PythRejection::Stale));
        assert_eq!(stale.divergence_bps, None, "only usable sources compare");
        assert!(stale.degraded, "single usable source");
        assert_eq!(
            stale.status(),
            ObsStatus::Ok,
            "stale pyth is not a read error"
        );

        let edge = combine_price(
            ids::WSOL,
            jup(116.0),
            pyth(116.5, 0.05, T0_S - 60),
            None,
            None,
            now,
        );
        assert_eq!(
            edge.source,
            Some(PriceSource::Pyth),
            "60 s old is still fresh"
        );

        let wide = combine_price(
            ids::WSOL,
            jup(116.0),
            pyth(116.5, 116.5 * 0.0101, T0_S),
            None,
            None,
            now,
        );
        assert_eq!(wide.source, Some(PriceSource::Jupiter));
        assert_eq!(wide.pyth_rejected, Some(PythRejection::WideConf));
        let tight = combine_price(
            ids::WSOL,
            jup(116.0),
            pyth(100.0, 1.0, T0_S),
            None,
            None,
            now,
        );
        assert_eq!(
            tight.source,
            Some(PriceSource::Pyth),
            "conf/price == 1 % is allowed"
        );
    }

    #[test]
    fn pyth_401_gives_partial_jupiter_price() {
        let o = combine_price(
            ids::WSOL,
            parse_jupiter_price(&v(JUP_V3), ids::WSOL),
            auth_err(),
            None,
            None,
            T0_MS,
        );
        assert_eq!(o.usd, Some(116.6084160651512));
        assert_eq!(o.source, Some(PriceSource::Jupiter));
        assert_eq!(o.status(), ObsStatus::Partial);
        let errs = o.errors();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].class, ErrorClass::AuthRequired);
        let obs = Observation::of("sol_price", &o, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert_eq!(obs.key, format!("price_oracle/1:{}", ids::WSOL));
        assert_eq!(obs.slot, None, "an off-chain price has no row slot");
        assert!(obs.render_text(T0_MS).contains("error pyth: auth_required"));
    }

    #[test]
    fn no_usable_source_is_none_never_the_previous_price() {
        let prev = combine_price(ids::WSOL, jup(116.0), auth_err(), None, None, T0_MS);
        let jup_err = Field::err(ReadError::new("jupiter", ErrorClass::Timeout, "20 s"));
        let o = combine_price(
            ids::WSOL,
            jup_err,
            auth_err(),
            None,
            Some(&prev),
            T0_MS + 30_000,
        );
        assert_eq!(o.usd, None, "never stale-at-any-age");
        assert_eq!(o.source, None);
        assert!(o.degraded);
        assert_eq!(o.status(), ObsStatus::Error);
        assert_eq!(o.errors().len(), 2);
        assert_eq!(o.samples, prev.samples, "ring carried, nothing appended");
        assert_eq!(o.move_5m_pct, None);
        let obs = Observation::of("sol_price", &o, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert!(
            !obs.features.contains_key("usd"),
            "missing is omitted, not 0"
        );
        assert!(obs.headline.contains("usd=unavailable"));

        // Only a stale Pyth and no Jupiter entry: Error with a usd reason.
        let o = combine_price(
            ids::WSOL,
            Field::Absent,
            pyth(116.5, 0.05, T0_S - 600),
            None,
            None,
            T0_MS,
        );
        assert_eq!(o.status(), ObsStatus::Error);
        assert_eq!(o.errors()[0].field, "usd");
    }

    #[test]
    fn unpriced_mint_is_absent() {
        let o = combine_price(
            UNKNOWN_MINT,
            parse_jupiter_price(&v(JUP_V3_UNKNOWN), UNKNOWN_MINT),
            Field::Absent,
            None,
            None,
            T0_MS,
        );
        assert_eq!((o.usd, o.status()), (None, ObsStatus::Absent));
        assert!(o.errors().is_empty());
    }

    #[test]
    fn pool_price_cross_check() {
        let q = PoolQuote {
            pool: POOL_SOL_USDC.into(),
            price: Field::ok(116.1),
        };
        let o = combine_price(ids::WSOL, jup(116.0), auth_err(), Some(q), None, T0_MS);
        assert_eq!(o.pool_price, Some(116.1));
        // (116.1 − 116.0) / 116.0 × 1e4 = +8.6207 bps, signed.
        assert!((o.pool_vs_oracle_bps.unwrap() - 8.620689655172).abs() < 1e-6);
        assert_eq!(o.sources_ok, 2);
        assert!(!o.degraded, "jupiter + pool cross-check");
        let obs = Observation::of("sol_price", &o, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert!(obs.headline.contains(POOL_SOL_USDC), "{}", obs.headline);
        assert!(obs.headline.contains(ids::WSOL));
        assert!(
            obs.headline.contains("pool_vs_oracle_bps=+8.6"),
            "{}",
            obs.headline
        );
        assert!(obs.headline.chars().count() <= 165, "{}", obs.headline);
        line1_ok(&obs, T0_MS + 9_000);

        let failed = PoolQuote {
            pool: POOL_SOL_USDC.into(),
            price: Field::err(ReadError::new("pool_price", ErrorClass::Transient, "rpc")),
        };
        let o = combine_price(
            ids::WSOL,
            jup(116.0),
            Field::Absent,
            Some(failed),
            None,
            T0_MS,
        );
        assert_eq!((o.pool_price, o.pool_vs_oracle_bps), (None, None));
        assert_eq!(o.status(), ObsStatus::Partial);
        assert_eq!(o.errors()[0].field, "pool_price");

        let zero = PoolQuote {
            pool: POOL_SOL_USDC.into(),
            price: Field::ok(0.0),
        };
        let o = combine_price(
            ids::WSOL,
            jup(116.0),
            Field::Absent,
            Some(zero),
            None,
            T0_MS,
        );
        assert_eq!(o.pool_price, None, "a 0 pool price is not a price");
        assert_eq!(o.status(), ObsStatus::Partial);
    }

    #[test]
    fn sample_ring_gives_5m_move_and_stays_bounded() {
        // One call per 30 s for 10 min, price rising 0.1 % per call.
        let mut prev: Option<OraclePrice> = None;
        let mut price = 100.0;
        for i in 0..=20 {
            let now = T0_MS + i * 30_000;
            let o = combine_price(
                ids::WSOL,
                jup(price),
                Field::Absent,
                None,
                prev.as_ref(),
                now,
            );
            assert!(o.samples.windows(2).all(|w| w[0].t_ms < w[1].t_ms));
            assert!(o.samples.iter().all(|s| now - s.t_ms <= 6 * 60_000));
            if i < 8 {
                assert_eq!(o.move_5m_pct, None, "call {i}: no 4-min-old reference yet");
            } else {
                // Reference = oldest sample ≥ 4 min old = the one 12 calls
                // back (6 min window) — independent arithmetic.
                let back = (i - 12).max(0);
                let ref_price = 100.0 * 1.001f64.powi(back as i32);
                let want = (price / ref_price - 1.0).abs() * 100.0;
                let got = o.move_5m_pct.unwrap();
                assert!((got - want).abs() < 1e-9, "call {i}: {got} vs {want}");
            }
            prev = Some(o);
            price *= 1.001;
        }

        // One call per second for 10 min: spacing keeps the ring bounded.
        let mut prev: Option<OraclePrice> = None;
        for i in 0..600 {
            let o = combine_price(
                ids::WSOL,
                jup(100.0),
                Field::Absent,
                None,
                prev.as_ref(),
                T0_MS + i * 1_000,
            );
            assert!(o.samples.len() <= 74, "{} samples", o.samples.len());
            assert_eq!(
                o.samples.last().unwrap().t_ms,
                T0_MS + i * 1_000,
                "newest kept"
            );
            prev = Some(o);
        }
        let last = prev.unwrap();
        assert_eq!(last.move_5m_pct, Some(0.0));
        let obs = Observation::of("sol_price", &last, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert!(
            !obs.render_text(T0_MS).contains("bytes omitted"),
            "data stays renderable"
        );
    }

    #[test]
    fn ring_ignores_other_mints_and_future_samples() {
        let other = combine_price(ids::USDC, jup(1.0), Field::Absent, None, None, T0_MS);
        let o = combine_price(
            ids::WSOL,
            jup(116.0),
            Field::Absent,
            None,
            Some(&other),
            T0_MS + 60_000,
        );
        assert_eq!(o.samples.len(), 1);
        assert_eq!(o.samples[0].usd, 116.0);
        let mut future = o.clone();
        future.samples.push(PriceSample {
            t_ms: T0_MS + 10_000_000,
            usd: 1.0,
        });
        let o2 = combine_price(
            ids::WSOL,
            jup(117.0),
            Field::Absent,
            None,
            Some(&future),
            T0_MS + 120_000,
        );
        assert!(o2.samples.iter().all(|s| s.t_ms <= T0_MS + 120_000));
        assert_eq!(o2.samples.len(), 2);
    }

    #[test]
    fn oracle_price_observation_contract() {
        let o = combine_price(
            ids::WSOL,
            parse_jupiter_price(&v(JUP_V3), ids::WSOL),
            parse_pyth(&v(PYTH_DOC), PYTH_FEED_SOL_USD),
            Some(PoolQuote {
                pool: POOL_SOL_USDC.into(),
                price: Field::ok(116.62748941349128),
            }),
            None,
            T0_MS,
        );
        let obs = Observation::of("sol_price", &o, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert_features_ok(&obs.features);
        for k in [
            "usd",
            "sources_ok",
            "divergence_bps",
            "degraded",
            "pool_vs_oracle_bps",
        ] {
            assert!(obs.features.contains_key(k), "feature {k}");
        }
        assert_eq!(obs.features["sources_ok"], json!(3));
        line1_ok(&obs, T0_MS + 9_999);
        let back: OraclePrice = obs.typed().unwrap();
        assert_eq!(back, o, "round-trips through the cache body");
    }

    // ── dlmm_pools ───────────────────────────────────────────────

    #[test]
    fn datapi_apr_is_daily_fee_over_tvl() {
        // Independent check on the raw fixture: apr == fees.24h / tvl × 100.
        let body = v(POOLS);
        for p in body["data"].as_array().unwrap() {
            let (apr, fees, tvl) = (
                p["apr"].as_f64().unwrap(),
                p["fees"]["24h"].as_f64().unwrap(),
                p["tvl"].as_f64().unwrap(),
            );
            assert!(
                (apr - fees / tvl * 100.0).abs() <= 1e-12 * apr.max(1.0),
                "{}",
                p["address"]
            );
            // Same double in python; serde_json's default float parse can
            // differ by an ulp between the two spellings datapi prints.
            let ratio = p["fee_tvl_ratio"]["24h"].as_f64().unwrap();
            assert!((apr - ratio).abs() <= 1e-15 * apr.max(1.0), "{apr} {ratio}");
        }
        let list = parse_datapi_pools(&body, "SOL-USDC", PoolSort::FeeTvl24h, 50, None);
        assert_eq!((list.fetched, list.matched, list.total), (20, 20, 129));
        assert_eq!(list.status, ObsStatus::Ok);
        for r in &list.pools {
            let raw = body["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["address"] == r.address.as_str())
                .unwrap();
            assert_eq!(r.fee_tvl_24h_pct, raw["apr"].as_f64());
            assert_eq!(r.apr_pct, Some(raw["apr"].as_f64().unwrap() * 365.0));
            assert_eq!(
                r.bin_step as u64,
                raw["pool_config"]["bin_step"].as_u64().unwrap()
            );
            assert_eq!(r.mint_x, raw["token_x"]["address"].as_str().unwrap());
            assert_eq!(r.mint_y, raw["token_y"]["address"].as_str().unwrap());
        }
        let sol_usdc = list
            .pools
            .iter()
            .find(|r| r.address == POOL_SOL_USDC)
            .unwrap();
        assert_eq!(sol_usdc.bin_step, 4);
        assert_eq!(sol_usdc.base_fee_pct, 0.04);
        assert_eq!(sol_usdc.dynamic_fee_pct, Some(1e-6));
        assert_eq!(sol_usdc.tvl_usd, 7036050.561671851);
        assert_eq!(sol_usdc.volume_24h_usd, Some(36838101.987413265));
        assert_eq!(sol_usdc.fees_24h_usd, Some(14347.301107755924));
        assert_eq!(sol_usdc.fee_tvl_24h_pct, Some(0.2039112849175835));
        assert_eq!(sol_usdc.current_price, Some(116.62748941349128));
        assert!(!sol_usdc.is_blacklisted);
    }

    /// Expected orders computed independently in python from the raw
    /// fixture (scratch `market_golden.py`).
    #[test]
    fn datapi_sort_filter_limit() {
        let body = v(POOLS);
        let addrs = |l: &DlmmPoolList| {
            l.pools
                .iter()
                .map(|r| r.address.clone())
                .collect::<Vec<_>>()
        };

        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::FeeTvl24h, 5, None);
        assert_eq!(
            addrs(&l),
            [
                "3M9nHQhxRMrK66hxRVTGLEmrvEK6Pimds6C3f3WaaLyt",
                "CLM92hJx6CGNBqTifR6Lvcvs3BuFbGWw1U4zLHELzQFL",
                "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
                "EYRZ7TiMxfaergZb5j9UQga3dXAtGbiaeWrWDMKrNUVm",
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
            ]
        );
        let obs = Observation::of("dlmm_pools", &l, T0_MS, POOLS_TTL_MS, ObsSource::Live);
        assert_eq!(obs.features["median_tvl_usd"], json!(17741.936695607823));
        assert_eq!(
            obs.features["max_fee_tvl_24h_pct"],
            json!(1.1786092376419195)
        );

        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::FeeTvl24h, 5, Some(10_000.0));
        assert_eq!(l.matched, 16);
        assert_eq!(
            addrs(&l),
            [
                "CLM92hJx6CGNBqTifR6Lvcvs3BuFbGWw1U4zLHELzQFL",
                "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
                POOL_SOL_USDC,
                "FoSDw2L5DmTuQTFe55gWPDXf88euaxAEKFre74CnvQbX",
            ]
        );
        assert!((l.pools[0].apr_pct.unwrap() - 306.0527619058674).abs() < 1e-9);

        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::Tvl, 3, None);
        assert_eq!(
            addrs(&l),
            [
                POOL_SOL_USDC,
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
                "BVRbyLjjfSBcoyiYFuxbgKYnWuiFaF9CSXEa5vdSZ9Hh",
            ]
        );
        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::Volume24h, 3, None);
        assert_eq!(
            addrs(&l),
            [
                POOL_SOL_USDC,
                "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
            ]
        );
        assert_eq!(
            parse_datapi_pools(&body, "q", PoolSort::Tvl, 0, None)
                .pools
                .len(),
            1
        );
        assert_eq!(
            parse_datapi_pools(&body, "q", PoolSort::Tvl, 500, None).limit,
            50
        );
    }

    #[test]
    fn datapi_zero_tvl_and_zero_price_are_none() {
        let body = v(POOLS_P7);
        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::FeeTvl24h, 50, None);
        assert_eq!((l.fetched, l.status), (9, ObsStatus::Ok));
        let row = |l: &DlmmPoolList, a: &str| l.pools.iter().find(|r| r.address == a).cloned();
        // Dust TVL (raw 0.000071845639316639 USD), no fees, datapi price 0.0.
        let dust = row(&l, "58w3C24X3pvnayoktnhq1uVgAjAehRYpZpgFbcteftFB").unwrap();
        assert!((dust.tvl_usd - 0.000071845639316639).abs() < 1e-18);
        assert_eq!(dust.fee_tvl_24h_pct, Some(0.0), "no fees on TVL > 0 is 0 %");
        assert_eq!(dust.current_price, None, "datapi 0.0 price is not a price");
        let idle = row(&l, "8zzQv7m3M1JaMUCaQGwCTgyzW9BG9RhQ4X9G1hkFekxb").unwrap();
        assert_eq!((idle.fee_tvl_24h_pct, idle.apr_pct), (Some(0.0), Some(0.0)));
        assert_eq!(idle.current_price, Some(115.65551280539104));

        // Exactly zero TVL (none live on 2026-09-24; forced here): 0/0 is
        // not 0 %, whatever datapi's `apr` placeholder says.
        let mut zeroed = body.clone();
        zeroed["data"][0]["tvl"] = json!(0.0);
        let l = parse_datapi_pools(&zeroed, "SOL-USDC", PoolSort::FeeTvl24h, 50, None);
        let zero = row(&l, "58w3C24X3pvnayoktnhq1uVgAjAehRYpZpgFbcteftFB").unwrap();
        assert_eq!((zero.fee_tvl_24h_pct, zero.apr_pct), (None, None));
        // None keys sort last under fee_tvl_24h.
        assert_eq!(
            l.pools.last().unwrap().address,
            "58w3C24X3pvnayoktnhq1uVgAjAehRYpZpgFbcteftFB"
        );
    }

    #[test]
    fn datapi_empty_bad_rows_and_bad_body() {
        let l = parse_datapi_pools(
            &v(POOLS_EMPTY),
            "ZZNOTAPAIRZZ",
            PoolSort::FeeTvl24h,
            10,
            None,
        );
        assert_eq!((l.total, l.fetched, l.status), (0, 0, ObsStatus::Absent));
        let obs = Observation::of("dlmm_pools", &l, T0_MS, POOLS_TTL_MS, ObsSource::Live);
        assert_features_ok(&obs.features);
        assert!(!obs.features.contains_key("median_tvl_usd"));

        let mut body = v(POOLS);
        let rows = body["data"].as_array_mut().unwrap();
        rows[0].as_object_mut().unwrap().remove("tvl");
        rows[1]["address"] = json!("not-a-pubkey");
        rows[2]["apr"] = json!(74.4); // annualised by mistake: 365 × daily
        let l = parse_datapi_pools(&body, "SOL-USDC", PoolSort::FeeTvl24h, 50, None);
        assert_eq!(l.status, ObsStatus::Partial);
        assert_eq!(l.pools.len(), 18);
        assert_eq!(l.errors.len(), 2);
        assert!(l.errors[0].message.starts_with("2 datapi row(s) skipped"));
        let bgm = l
            .pools
            .iter()
            .find(|r| r.address == "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y")
            .unwrap();
        assert_eq!(
            bgm.fee_tvl_24h_pct, None,
            "apr semantics change never passes silently"
        );
        assert!(l.errors[1]
            .message
            .contains("BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y"));

        for bad in [
            json!({"detail": "Not Found"}),
            json!([]),
            json!({"data": {}}),
        ] {
            let l = parse_datapi_pools(&bad, "SOL-USDC", PoolSort::Tvl, 10, None);
            assert_eq!(l.status, ObsStatus::Error, "{bad}");
            assert_eq!(l.errors[0].class, ErrorClass::Decode);
        }
    }

    #[test]
    fn pool_list_observation_contract() {
        let l = parse_datapi_pools(&v(POOLS), "SOL-USDC", PoolSort::FeeTvl24h, 10, Some(1000.0));
        let obs = Observation::of("dlmm_pools", &l, T0_MS, POOLS_TTL_MS, ObsSource::Live);
        assert_eq!(obs.key, "dlmm_pools/1:SOL-USDC|fee_tvl_24h|10|1000");
        assert_eq!(
            obs.key,
            Observation::key_for(
                DlmmPoolList::SCHEMA,
                &DlmmPoolList::subject_for("SOL-USDC", PoolSort::FeeTvl24h, 10, Some(1000.0))
            )
        );
        assert_eq!(
            DlmmPoolList::subject_for(" SOL-USDC ", PoolSort::Tvl, 99, None),
            "SOL-USDC|tvl|50|0"
        );
        assert_features_ok(&obs.features);
        assert!(obs
            .headline
            .contains("CLM92hJx6CGNBqTifR6Lvcvs3BuFbGWw1U4zLHELzQFL"));
        assert!(obs.headline.contains("query=SOL-USDC"));
        line1_ok(&obs, T0_MS + 59_000);
        let back: DlmmPoolList = obs.typed().unwrap();
        assert_eq!(back, l);

        // A long free-text query is dropped from line 1, the top id stays.
        let q = "q".repeat(120);
        let l = parse_datapi_pools(&v(POOLS), &q, PoolSort::FeeTvl24h, 10, None);
        let obs = Observation::of("dlmm_pools", &l, T0_MS, POOLS_TTL_MS, ObsSource::Live);
        assert!(obs
            .headline
            .contains("3M9nHQhxRMrK66hxRVTGLEmrvEK6Pimds6C3f3WaaLyt"));
        assert!(!obs.headline.contains(&q));
        line1_ok(&obs, T0_MS);
    }

    #[test]
    fn worst_case_headlines_keep_full_ids_within_budget() {
        // 44-char mint + 44-char pool, large price, large negative spread.
        let q = PoolQuote {
            pool: POOL_SOL_USDC.into(),
            price: Field::ok(12_000.0),
        };
        let o = combine_price(
            ids::USDC,
            jup(123_456.123_456),
            auth_err(),
            Some(q),
            None,
            T0_MS,
        );
        let obs = Observation::of("sol_price", &o, T0_MS, PRICE_TTL_MS, ObsSource::Live);
        assert_eq!(ids::USDC.len(), 44);
        assert!(obs.headline.contains(ids::USDC) && obs.headline.contains(POOL_SOL_USDC));
        assert!(obs.headline.chars().count() <= 165, "{}", obs.headline);
        line1_ok(&obs, T0_MS + 9_999);

        let l = DlmmPoolList::failed(
            "SOL-USDC",
            PoolSort::Volume24h,
            50,
            Some(-1.0),
            ReadError::new("pools", ErrorClass::Timeout, "datapi timed out after 20 s"),
        );
        assert_eq!(
            (l.status, l.min_tvl_usd, l.limit),
            (ObsStatus::Error, None, 50)
        );
        let obs = Observation::of("dlmm_pools", &l, T0_MS, POOLS_TTL_MS, ObsSource::Live);
        assert_eq!(obs.status, ObsStatus::Error);
        assert_eq!(obs.key, "dlmm_pools/1:SOL-USDC|volume_24h|50|0");
        assert_features_ok(&obs.features);
        line1_ok(&obs, T0_MS);
        assert!(obs.render_text(T0_MS).contains("error pools: timeout"));
    }

    #[test]
    fn same_instant_samples_collapse() {
        let a = combine_price(ids::WSOL, jup(100.0), Field::Absent, None, None, T0_MS);
        let b = combine_price(ids::WSOL, jup(101.0), Field::Absent, None, Some(&a), T0_MS);
        assert_eq!(
            b.samples,
            vec![PriceSample {
                t_ms: T0_MS,
                usd: 101.0
            }]
        );
        // Negative / non-finite min TVL is "no filter".
        let l = parse_datapi_pools(&v(POOLS), "SOL-USDC", PoolSort::Tvl, 50, Some(f64::NAN));
        assert_eq!((l.min_tvl_usd, l.matched), (None, 20));
    }

    #[test]
    fn pool_sort_names_round_trip() {
        for s in [PoolSort::FeeTvl24h, PoolSort::Tvl, PoolSort::Volume24h] {
            assert_eq!(PoolSort::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert_eq!(PoolSort::parse("apr"), None);
    }
}
