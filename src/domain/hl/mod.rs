//! Hyperliquid wire decoders — pure maps from `POST /info` replies to the
//! cross-venue rows of `domain/market.rs` (`mkt_ctx/1`, `mkt_instrument/1`)
//! and the book of `domain/book.rs`. No IO; the read time is an input. The
//! `hl_*` tools (`adapters/outbound/tools/hyperliquid/`) fetch, cache and
//! store.
//!
//! | File | Replies | Rows |
//! |---|---|---|
//! | `ctx.rs` | `metaAndAssetCtxs {dex}`, `spotMetaAndAssetCtxs`, `perpDexs`, `perpCategories`, `perpsAtOpenInterestCap {dex}` | `mkt_ctx/1`, `mkt_instrument/1`, `hl_perp_meta/1`, `hl_at_oi_cap/1`, `hl_sweep/1` |
//! | `book.rs` | `l2Book {coin}`, `recentTrades {coin}` | `hl_book/1` |
//!
//! | Coin (HL name, verbatim) | Market | Ctx request |
//! |---|---|---|
//! | `ETH`, `kPEPE` | perp, default dex (`""`) | `metaAndAssetCtxs` |
//! | `xyz:TSLA` | perp, HIP-3 builder dex `xyz` | `metaAndAssetCtxs {dex: "xyz"}` |
//! | `@151`, `PURR/USDC` | spot pair | `spotMetaAndAssetCtxs` |
//! | `#67890` | outcome side | not supported (M8) |
//!
//! | Fact | Rule (HL docs / live probes 2026-09-30) |
//! |---|---|
//! | Asset id | default dex: universe index; builder dex at `perpDexs` position p ≥ 1: `100000 + 10000·p + index` (`xyz:TSLA` = 110001); spot: `10000 + index` |
//! | Funding | paid hourly, on the hour |
//! | Quote (collateral token) | 0 USDC · 235 USDE · 268 USDT0 (→ `USDT`) · 360 USDH (`spotMeta` token indexes); other tokens ⇒ unknown |
//! | Fee basis | `[paper] fee_tier` / `staking_discount_pct` (tier 0, no staking without `[paper]`) → `domain::xm::cost::HlUserRates` |

pub(crate) mod book;
pub(crate) mod ctx;

use crate::domain::market::{InstrumentKind, MarketInstrument, QuoteCcy};
use crate::domain::xm::cost::{
    hl_fee_schedule, CostError, FeeSchedule, HlFeeMarket, HlKind, HlUserRates,
};

/// How keys and headlines name HL's default dex (the API's `""`).
pub(crate) const DEFAULT_DEX_LABEL: &str = "default";

/// What an HL coin name addresses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CoinKind {
    /// A perp; `dex` = `""` for the default dex, else the builder dex name.
    Perp {
        dex: String,
    },
    Spot,
    Outcome,
}

/// Classify an HL coin name: `#…` outcome, `@…` or `BASE/QUOTE` spot,
/// `<dex>:<name>` builder perp, anything else a default-dex perp.
pub(crate) fn classify(coin: &str) -> CoinKind {
    if coin.starts_with('#') {
        CoinKind::Outcome
    } else if coin.starts_with('@') || coin.contains('/') {
        CoinKind::Spot
    } else {
        match coin.split_once(':') {
            Some((dex, _)) => CoinKind::Perp {
                dex: dex.to_string(),
            },
            None => CoinKind::Perp { dex: String::new() },
        }
    }
}

/// Key / headline label of a perp dex: `""` → [`DEFAULT_DEX_LABEL`].
pub(crate) fn dex_label(dex: &str) -> &str {
    if dex.is_empty() {
        DEFAULT_DEX_LABEL
    } else {
        dex
    }
}

/// A `dex` argument → the API name: `""` and `"default"` mean the default
/// dex; a builder dex name is ASCII alphanumeric (passed verbatim).
pub(crate) fn parse_dex(arg: &str) -> Result<String, String> {
    let d = arg.trim();
    if d.is_empty() || d == DEFAULT_DEX_LABEL {
        return Ok(String::new());
    }
    if d.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(d.to_string())
    } else {
        Err(format!(
            "dex `{d}` is not a Hyperliquid perp dex name (ASCII letters / digits; \"\" = the default dex)"
        ))
    }
}

/// Perp asset id: universe `index` on the dex at `perpDexs` position
/// `dex_position` (0 = the default dex).
pub(crate) fn perp_asset_id(dex_position: u32, index: u32) -> u64 {
    if dex_position == 0 {
        u64::from(index)
    } else {
        100_000 + 10_000 * u64::from(dex_position) + u64::from(index)
    }
}

/// Spot asset id: `10000 + pair index`.
pub(crate) fn spot_asset_id(index: u32) -> u64 {
    10_000 + u64::from(index)
}

const HOUR_MS: i64 = 3_600_000;

/// Next hourly funding time after `now_ms` (HL pays on the hour).
pub(crate) fn next_funding_ms(now_ms: i64) -> i64 {
    (now_ms.div_euclid(HOUR_MS) + 1) * HOUR_MS
}

/// Perp collateral token (`meta.collateralToken`) → quote currency.
pub(crate) fn collateral_quote(token: u64) -> Option<QuoteCcy> {
    match token {
        0 => Some(QuoteCcy::Usdc),
        235 => Some(QuoteCcy::Usde),
        268 => Some(QuoteCcy::Usdt),
        360 => Some(QuoteCcy::Usdh),
        _ => None,
    }
}

/// Spot token name → quote currency (`USDT0` is HL's USDT).
pub(crate) fn token_quote(name: &str) -> Option<QuoteCcy> {
    match name {
        "USDC" => Some(QuoteCcy::Usdc),
        "USDT0" | "USDT" => Some(QuoteCcy::Usdt),
        "USDH" => Some(QuoteCcy::Usdh),
        "USDE" => Some(QuoteCcy::Usde),
        _ => None,
    }
}

/// The paper account's fee standing (`[paper] fee_tier`,
/// `staking_discount_pct`); default tier 0, no staking.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct FeeBasis {
    pub tier: u8,
    pub staking_discount_pct: f64,
}

impl FeeBasis {
    /// The user's base rates for `kind` — `Err` for a tier / discount HL
    /// does not table (the tools then omit `taker_fee_bps`).
    pub(crate) fn user_rates(&self, kind: HlKind) -> Result<HlUserRates, CostError> {
        HlUserRates::from_tier(kind, self.tier, self.staking_discount_pct)
    }
}

/// `hl_fee_schedule` for the paper account on a USDC-quoted market (no
/// referral discount); `None` on any other quote — the aligned-quote term
/// of USDH / USDE / USDT0 is not modelled — or a rejected input. The one
/// rule behind `mkt_ctx/1.taker_fee_bps` and the paper fills.
pub(crate) fn usdc_fee_schedule(
    rates: HlUserRates,
    quote: Option<QuoteCcy>,
    market: HlFeeMarket,
) -> Option<FeeSchedule> {
    if quote != Some(QuoteCcy::Usdc) {
        return None;
    }
    hl_fee_schedule(rates, 0.0, false, market).ok()
}

/// The paper fee schedule of a Hyperliquid perp from its `mkt_instrument/1`
/// row: `basis` × deployer fee scale × growth mode (`usdc_fee_schedule`);
/// `None` for a spot / outcome market or an unknown input.
pub(crate) fn paper_fees(inst: &MarketInstrument, basis: FeeBasis) -> Option<FeeSchedule> {
    if inst.kind != InstrumentKind::Perp {
        return None;
    }
    let market = HlFeeMarket::Perp {
        deployer_fee_scale: inst.deployer_fee_scale?,
        growth_mode: inst.growth_mode?,
    };
    usdc_fee_schedule(basis.user_rates(HlKind::Perp).ok()?, inst.quote_ccy, market)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coins_classify_by_hl_naming() {
        let perp = |d: &str| CoinKind::Perp { dex: d.into() };
        for (coin, want) in [
            ("ETH", perp("")),
            ("kPEPE", perp("")),
            ("xyz:TSLA", perp("xyz")),
            ("io:ANTH", perp("io")),
            ("@151", CoinKind::Spot),
            ("PURR/USDC", CoinKind::Spot),
            ("#67890", CoinKind::Outcome),
        ] {
            assert_eq!(classify(coin), want, "{coin}");
        }
        assert_eq!(dex_label(""), "default");
        assert_eq!(dex_label("xyz"), "xyz");
    }

    #[test]
    fn dex_args_parse() {
        assert_eq!(parse_dex("").unwrap(), "");
        assert_eq!(parse_dex(" default ").unwrap(), "");
        assert_eq!(parse_dex("xyz").unwrap(), "xyz");
        for bad in ["xyz:TSLA", "x y", "@1"] {
            let e = parse_dex(bad).unwrap_err();
            assert!(e.contains(bad), "{e}");
        }
    }

    #[test]
    fn asset_ids_follow_the_hl_formula() {
        // xyz = perpDexs position 1, TSLA = xyz universe index 1.
        assert_eq!(perp_asset_id(1, 1), 110_001);
        assert_eq!(perp_asset_id(1, 9), 110_009);
        // para = position 8, STX index 10; io = position 10, ANTH index 1.
        assert_eq!(perp_asset_id(8, 10), 180_010);
        assert_eq!(perp_asset_id(10, 1), 200_001);
        assert_eq!(perp_asset_id(0, 15), 15);
        assert_eq!(spot_asset_id(151), 10_151);
    }

    #[test]
    fn funding_is_on_the_hour() {
        // 2026-09-30T13:39:27.699Z → 14:00:00Z.
        assert_eq!(next_funding_ms(1_790_775_567_699), 1_790_776_800_000);
        assert_eq!(next_funding_ms(1_790_776_800_000), 1_790_780_400_000);
    }

    #[test]
    fn quotes_and_fee_basis() {
        assert_eq!(collateral_quote(0), Some(QuoteCcy::Usdc));
        assert_eq!(collateral_quote(360), Some(QuoteCcy::Usdh));
        assert_eq!(collateral_quote(7), None);
        assert_eq!(token_quote("USDT0"), Some(QuoteCcy::Usdt));
        assert_eq!(token_quote("HYPE"), None);
        let tier0 = FeeBasis::default().user_rates(HlKind::Perp).unwrap();
        assert!((tier0.taker * 1e4 - 4.5).abs() < 1e-12);
        let bad = FeeBasis {
            tier: 0,
            staking_discount_pct: 25.0,
        };
        assert!(bad.user_rates(HlKind::Perp).is_err());
    }
}
