//! SOE values (`docs/soe-2026-10-08.md` § 5): exact money, currencies, FX,
//! basis points, unknown-safe three-point estimates and schema tags. Money is
//! integer minor units parsed from decimal text — never `f64` — so a
//! threshold rule (`≤ cap`, `≥ target`) holds exactly at its boundary (the
//! trading desk's `f64` + tolerance, `domain/xm/risk.rs`, does not).
//!
//! | Value | TOML | Rule |
//! |---|---|---|
//! | [`Minor`] | `"41250.75"` · `"7.5"` · `2750` (whole units) | ≤ 2 fraction digits, exact; a float, `"1e3"`, `"2,500"`, `"+5"`, `".5"` refused (`invalid_amount`); prints `"41250.75"` |
//! | [`Currency`] | `"EUR"` | closed set EUR USD GBP CHF KZT RUB CNY, each 2 minor digits (`invalid_currency`) |
//! | [`Money`] | `{ amount = "250.00", currency = "EUR" }` | arithmetic and order within one currency only (`currency_mismatch`) |
//! | [`Bps`] | `250` | an integer 0..=10 000 (`invalid_bps`) |
//! | [`FxRate`] | `{ pair = "USD/EUR", rate = "0.92", source = "url:https://…", as_of = "2026-10-01" }` | quote units per 1 base unit, ≤ 8 fraction digits, > 0 (`invalid_rate`); `source` a lineage `Locator`, `as_of` a lineage `Time` |
//! | [`Converted`] | `{ native = {…}, rate = {…}, converted = {…} }` | native amount, the rate (with its source and time) and the result stored apart; the result is the floor or the ceil of the exact product (`invalid_conversion`) |
//! | [`Est`] | `"UNKNOWN"` · `"UNKNOWN: <reason>"` · `{ low = …, base = …, high = … }` | `low ≤ base ≤ high` (`invalid_range`); any other string `invalid_estimate`; an unknown never reads as 0 |
//! | [`Assumption`] | `{ value = <Est>, evidence = [<locator>, …], as_of = <time>, note = "…" }` | `note` optional; `evidence = []` must be written out |
//! | [`SchemaTag`] | `"soe.<record>/1"` | record `[a-z][a-z0-9_]{0,63}` (`invalid_schema`); a version other than [`SCHEMA_V1`] `schema_unsupported`; another record `schema_mismatch` |
//!
//! | Rounding | Rule |
//! |---|---|
//! | [`Flow::Inflow`] (revenue, contribution) | floor ([`div_floor`]) |
//! | [`Flow::Outflow`] (cost, exposure, capital) | ceil ([`div_ceil`]) |
//! | Why | a rounded figure never flatters a candidate |
//! | Scenario pick ([`Est::pick`]) | downside = each input's adverse end, upside = its favorable end, base = base; [`Better`] says which end is favorable |

// Consumers land with the SOE records, economics and gates (O1 W2–W5).
#![allow(dead_code)]

use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::lineage::value::{Locator, Time, UNKNOWN};

/// Error codes ([`ValueError::code`]).
pub mod codes {
    pub const INVALID_AMOUNT: &str = "invalid_amount";
    pub const INVALID_CURRENCY: &str = "invalid_currency";
    pub const CURRENCY_MISMATCH: &str = "currency_mismatch";
    pub const INVALID_BPS: &str = "invalid_bps";
    pub const INVALID_RATE: &str = "invalid_rate";
    pub const INVALID_CONVERSION: &str = "invalid_conversion";
    pub const INVALID_ESTIMATE: &str = "invalid_estimate";
    pub const INVALID_RANGE: &str = "invalid_range";
    pub const INVALID_SCHEMA: &str = "invalid_schema";
    pub const SCHEMA_UNSUPPORTED: &str = "schema_unsupported";
    pub const SCHEMA_MISMATCH: &str = "schema_mismatch";
    pub const OVERFLOW: &str = "overflow";
    // Records (`record.rs` and the record files).
    pub const INVALID_RECORD: &str = "invalid_record";
    pub const INVALID_ID: &str = "invalid_id";
    pub const INVALID_VERSION: &str = "invalid_version";
    pub const INVALID_FIELD: &str = "invalid_field";
    pub const INVALID_TIME: &str = "invalid_time";
    pub const DUPLICATE: &str = "duplicate";
    pub const FUTURE_LEAKAGE: &str = "future_leakage";
    pub const FAKE_RECURRING: &str = "fake_recurring";
    pub const REVENUE_MODEL_MISMATCH: &str = "revenue_model_mismatch";
    pub const DEAL_REVIEW_MISSING: &str = "deal_review_missing";
    pub const PRIVATE_LOCATOR: &str = "private_locator";
    // Gates (`gates.rs`).
    pub const OPERATOR_PROFILE_UNSIGNED: &str = "operator_profile_unsigned";
    // Eval set (`eval.rs`).
    pub const PROFILE_MISMATCH: &str = "profile_mismatch";
    // O3 proposals, challenges, allocation, forecast (`proposal.rs` …).
    pub const COMPUTED_FIELD: &str = "computed_field";
    pub const UNSUPPORTED_EVIDENCE: &str = "unsupported_evidence";
    pub const FACT_WITHOUT_EVIDENCE: &str = "fact_without_evidence";
    pub const UNTRACED_EVIDENCE: &str = "untraced_evidence";
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    pub const UNKNOWN_TARGET: &str = "unknown_target";
    pub const CHAIN_BROKEN: &str = "chain_broken";
    pub const EVIDENCE_BEFORE_FREEZE: &str = "evidence_before_freeze";
    pub const HIT_AFTER_DEADLINE: &str = "hit_after_deadline";
    pub const MISS_BEFORE_DEADLINE: &str = "miss_before_deadline";
    pub const HORIZON_TOO_LONG: &str = "horizon_too_long";
    pub const UNKNOWN_ITEM: &str = "unknown_item";
}

/// A refused value: a [`codes`] entry + what was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    pub code: &'static str,
    pub message: String,
}

impl ValueError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        ValueError {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ValueError {}

/// A string value through `FromStr` (the serde half of `Display` / `FromStr`
/// types here).
fn de_from_str<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr<Err = ValueError>,
{
    String::deserialize(d)?.parse().map_err(de::Error::custom)
}

// ---------------------------------------------------------------------------
// Decimal text
// ---------------------------------------------------------------------------

/// `s` as a decimal with at most `digits` fraction digits, scaled by
/// 10^`digits`: `"7.5"` (2) → 750. A leading `-` only; no `+`, exponent,
/// separator, space, bare `.` or empty part.
fn parse_scaled(s: &str, digits: u32) -> Result<i128, String> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let digits_only = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int) {
        return Err(format!(
            "`{s}` is not a decimal (digits, an optional `.` and at most {digits} fraction digits)"
        ));
    }
    let frac = match frac {
        None => "",
        Some(f) if !digits_only(f) => {
            return Err(format!("`{s}`: digits expected after the `.`"));
        }
        Some(f) if f.len() > digits as usize => {
            return Err(format!(
                "`{s}`: {} fraction digits — at most {digits} (exact, never rounded)",
                f.len()
            ));
        }
        Some(f) => f,
    };
    // 30 integer digits + 8 fraction digits stay far inside i128.
    if int.len() > 30 {
        return Err(format!("`{s}`: too large"));
    }
    let scale = 10i128.pow(digits);
    let whole: i128 = int.parse().map_err(|_| format!("`{s}`: too large"))?;
    let mut part: i128 = 0;
    if !frac.is_empty() {
        part = frac.parse().map_err(|_| format!("`{s}`: bad fraction"))?;
        part *= 10i128.pow(digits - frac.len() as u32);
    }
    let v = whole * scale + part;
    Ok(if neg { -v } else { v })
}

/// `v` / 10^`digits` as text; `trim` drops trailing fraction zeros (and the
/// `.` when nothing is left).
fn format_scaled(v: i128, digits: u32, trim: bool) -> String {
    let scale = 10i128.pow(digits);
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    let (whole, part) = (a / scale as u128, a % scale as u128);
    let mut frac = format!("{part:0width$}", width = digits as usize);
    if trim {
        while frac.ends_with('0') {
            frac.pop();
        }
    }
    if frac.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

// ---------------------------------------------------------------------------
// Rounding
// ---------------------------------------------------------------------------

/// `n / d` rounded toward −∞. Panics when `d = 0`, like `/`.
pub fn div_floor(n: i128, d: i128) -> i128 {
    let q = n / d;
    if n % d != 0 && ((n < 0) != (d < 0)) {
        q - 1
    } else {
        q
    }
}

/// `n / d` rounded toward +∞. Panics when `d = 0`, like `/`.
pub fn div_ceil(n: i128, d: i128) -> i128 {
    let q = n / d;
    if n % d != 0 && ((n < 0) == (d < 0)) {
        q + 1
    } else {
        q
    }
}

/// Which side of a candidate an amount sits on: decides its rounding so a
/// rounded figure never flatters the candidate (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// Revenue, contribution: rounded down.
    Inflow,
    /// Cost, exposure, capital: rounded up.
    Outflow,
}

impl Flow {
    /// `n / d` rounded for this side ([`div_floor`] / [`div_ceil`]).
    pub fn div(self, n: i128, d: i128) -> i128 {
        match self {
            Flow::Inflow => div_floor(n, d),
            Flow::Outflow => div_ceil(n, d),
        }
    }
}

// ---------------------------------------------------------------------------
// Minor units, currencies, money
// ---------------------------------------------------------------------------

/// Fraction digits of every [`Currency`] (ISO 4217 minor units).
pub const MINOR_DIGITS: u32 = 2;
const MINOR_PER_UNIT: i64 = 100;

/// An exact amount in minor units (hundredths) of some currency (module
/// table). Any `i64` is valid; arithmetic is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Minor(pub i64);

impl Minor {
    pub const ZERO: Minor = Minor(0);

    /// `whole` units: `2750` → 2750.00.
    pub fn units(whole: i64) -> Result<Minor, ValueError> {
        whole
            .checked_mul(MINOR_PER_UNIT)
            .map(Minor)
            .ok_or_else(|| ValueError::new(codes::INVALID_AMOUNT, format!("{whole}: too large")))
    }

    fn from_i128(v: i128, what: &str) -> Result<Minor, ValueError> {
        i64::try_from(v)
            .map(Minor)
            .map_err(|_| ValueError::new(codes::OVERFLOW, format!("{what} overflows")))
    }

    pub fn checked_add(self, other: Minor) -> Result<Minor, ValueError> {
        self.0
            .checked_add(other.0)
            .map(Minor)
            .ok_or_else(|| ValueError::new(codes::OVERFLOW, format!("{self} + {other} overflows")))
    }

    pub fn checked_sub(self, other: Minor) -> Result<Minor, ValueError> {
        self.0
            .checked_sub(other.0)
            .map(Minor)
            .ok_or_else(|| ValueError::new(codes::OVERFLOW, format!("{self} − {other} overflows")))
    }

    /// `self × n` (a count: hours, months, customers).
    pub fn checked_mul(self, n: i64) -> Result<Minor, ValueError> {
        self.0
            .checked_mul(n)
            .map(Minor)
            .ok_or_else(|| ValueError::new(codes::OVERFLOW, format!("{self} × {n} overflows")))
    }

    /// `self × num / den`, rounded for `flow`; `den > 0`.
    pub fn mul_div(self, num: i64, den: i64, flow: Flow) -> Result<Minor, ValueError> {
        if den <= 0 {
            return Err(ValueError::new(
                codes::OVERFLOW,
                format!("{self} × {num} / {den}: the divisor must be > 0"),
            ));
        }
        let q = flow.div(self.0 as i128 * num as i128, den as i128);
        Minor::from_i128(q, &format!("{self} × {num} / {den}"))
    }

    /// `self × bps / 10 000`, rounded for `flow` (never overflows: the result
    /// is at most `|self|`).
    pub fn mul_bps(self, bps: Bps, flow: Flow) -> Minor {
        Minor(flow.div(self.0 as i128 * bps.0 as i128, BPS_FULL as i128) as i64)
    }
}

impl FromStr for Minor {
    type Err = ValueError;
    fn from_str(s: &str) -> Result<Self, ValueError> {
        let v = parse_scaled(s, MINOR_DIGITS)
            .map_err(|why| ValueError::new(codes::INVALID_AMOUNT, why))?;
        i64::try_from(v)
            .map(Minor)
            .map_err(|_| ValueError::new(codes::INVALID_AMOUNT, format!("`{s}`: too large")))
    }
}

impl fmt::Display for Minor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&format_scaled(self.0 as i128, MINOR_DIGITS, false))
    }
}

impl Serialize for Minor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Minor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Minor;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("money as a decimal string (\"41250.75\") or whole units (2750)")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Minor, E> {
                v.parse().map_err(E::custom)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Minor, E> {
                Minor::units(v).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Minor, E> {
                i64::try_from(v)
                    .map_err(|_| ValueError::new(codes::INVALID_AMOUNT, format!("{v}: too large")))
                    .and_then(Minor::units)
                    .map_err(E::custom)
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Minor, E> {
                Err(E::custom(ValueError::new(
                    codes::INVALID_AMOUNT,
                    format!(
                        "{v} is a float — give money as a decimal string (\"41250.75\") or whole units"
                    ),
                )))
            }
        }
        d.deserialize_any(V)
    }
}

/// A currency (module table: a closed set, each with 2 minor digits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Currency {
    Eur,
    Usd,
    Gbp,
    Chf,
    Kzt,
    Rub,
    Cny,
}

impl Currency {
    pub const ALL: [Currency; 7] = [
        Currency::Eur,
        Currency::Usd,
        Currency::Gbp,
        Currency::Chf,
        Currency::Kzt,
        Currency::Rub,
        Currency::Cny,
    ];

    /// The ISO 4217 code.
    pub fn code(self) -> &'static str {
        match self {
            Currency::Eur => "EUR",
            Currency::Usd => "USD",
            Currency::Gbp => "GBP",
            Currency::Chf => "CHF",
            Currency::Kzt => "KZT",
            Currency::Rub => "RUB",
            Currency::Cny => "CNY",
        }
    }

    /// Minor digits: 2 for every member — [`Minor`] and [`FxRate::convert`]
    /// rely on it.
    pub fn exponent(self) -> u32 {
        MINOR_DIGITS
    }

    pub fn parse(code: &str) -> Option<Currency> {
        Currency::ALL.into_iter().find(|c| c.code() == code)
    }
}

impl FromStr for Currency {
    type Err = ValueError;
    fn from_str(s: &str) -> Result<Self, ValueError> {
        Currency::parse(s).ok_or_else(|| {
            ValueError::new(
                codes::INVALID_CURRENCY,
                format!(
                    "`{s}` is not one of {}",
                    Currency::ALL.map(|c| c.code()).join(", ")
                ),
            )
        })
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        de_from_str(d)
    }
}

/// An amount in one currency (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Money {
    #[serde(rename = "amount")]
    pub minor: Minor,
    pub currency: Currency,
}

impl Money {
    pub fn new(minor: Minor, currency: Currency) -> Money {
        Money { minor, currency }
    }

    pub fn zero(currency: Currency) -> Money {
        Money::new(Minor::ZERO, currency)
    }

    fn same_currency(self, other: Money, op: &str) -> Result<(), ValueError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(ValueError::new(
                codes::CURRENCY_MISMATCH,
                format!("{self} {op} {other}: convert first"),
            ))
        }
    }

    pub fn checked_add(self, other: Money) -> Result<Money, ValueError> {
        self.same_currency(other, "+")?;
        Ok(Money::new(
            self.minor.checked_add(other.minor)?,
            self.currency,
        ))
    }

    pub fn checked_sub(self, other: Money) -> Result<Money, ValueError> {
        self.same_currency(other, "−")?;
        Ok(Money::new(
            self.minor.checked_sub(other.minor)?,
            self.currency,
        ))
    }

    /// An amount + currency as a source record keeps them (any ISO code,
    /// `domain/source/`): a currency outside the closed set is
    /// [`Est::Unknown`] with the reason, never a guess; a malformed amount
    /// is an error.
    pub fn from_native(amount: &str, currency: &str) -> Result<Est<Money>, ValueError> {
        let minor: Minor = amount.parse()?;
        Ok(match Currency::parse(currency) {
            Some(c) => Est::point(Money::new(minor, c)),
            None => Est::unknown(format!(
                "currency `{currency}` is outside the closed set ({})",
                Currency::ALL.map(|c| c.code()).join(", ")
            )),
        })
    }
}

/// Ordered within one currency only: `None` across currencies.
impl PartialOrd for Money {
    fn partial_cmp(&self, other: &Money) -> Option<Ordering> {
        (self.currency == other.currency).then(|| self.minor.cmp(&other.minor))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.minor, self.currency)
    }
}

// ---------------------------------------------------------------------------
// Basis points
// ---------------------------------------------------------------------------

/// 100 % in basis points.
pub const BPS_FULL: u16 = 10_000;

/// A share in basis points, 0..=10 000 (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Bps(u16);

impl Bps {
    pub const ZERO: Bps = Bps(0);
    pub const FULL: Bps = Bps(BPS_FULL);

    pub fn new(v: i64) -> Result<Bps, ValueError> {
        u16::try_from(v)
            .ok()
            .filter(|b| *b <= BPS_FULL)
            .map(Bps)
            .ok_or_else(|| {
                ValueError::new(
                    codes::INVALID_BPS,
                    format!("{v} bps: an integer 0..={BPS_FULL}"),
                )
            })
    }

    pub fn get(self) -> u16 {
        self.0
    }

    /// `10 000 − self` (churn → retention, loss → collected).
    pub fn complement(self) -> Bps {
        Bps(BPS_FULL - self.0)
    }
}

impl fmt::Display for Bps {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Bps {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(self.0)
    }
}

impl<'de> Deserialize<'de> for Bps {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Bps::new(i64::deserialize(d)?).map_err(de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// FX
// ---------------------------------------------------------------------------

/// Fraction digits of an [`FxRate`].
pub const RATE_DIGITS: u32 = 8;
const RATE_SCALE: i128 = 100_000_000;

/// `BASE/QUOTE` of two different currencies: a rate is quote units per 1
/// base unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CurrencyPair {
    pub base: Currency,
    pub quote: Currency,
}

impl FromStr for CurrencyPair {
    type Err = ValueError;
    fn from_str(s: &str) -> Result<Self, ValueError> {
        let (b, q) = s.split_once('/').ok_or_else(|| {
            ValueError::new(codes::INVALID_RATE, format!("pair `{s}`: `BASE/QUOTE`"))
        })?;
        let (base, quote) = (b.parse()?, q.parse()?);
        if base == quote {
            return Err(ValueError::new(
                codes::INVALID_RATE,
                format!("pair `{s}`: two different currencies"),
            ));
        }
        Ok(CurrencyPair { base, quote })
    }
}

impl fmt::Display for CurrencyPair {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

impl Serialize for CurrencyPair {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CurrencyPair {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        de_from_str(d)
    }
}

/// A rate with where and when it was read (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FxRate {
    pub pair: CurrencyPair,
    /// Quote units per 1 base unit × 10^8 (> 0).
    #[serde(rename = "rate", with = "rate_text")]
    pub rate_e8: u64,
    pub source: Locator,
    pub as_of: Time,
}

/// `FxRate::rate_e8` as decimal text: `"0.92"` ⇄ 92 000 000.
mod rate_text {
    use super::*;

    pub fn parse(s: &str) -> Result<u64, ValueError> {
        let v = parse_scaled(s, RATE_DIGITS)
            .map_err(|why| ValueError::new(codes::INVALID_RATE, why))?;
        u64::try_from(v)
            .ok()
            .filter(|r| *r > 0)
            .ok_or_else(|| ValueError::new(codes::INVALID_RATE, format!("rate `{s}`: must be > 0")))
    }

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_scaled(*v as i128, RATE_DIGITS, true))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        parse(&String::deserialize(d)?).map_err(de::Error::custom)
    }
}

impl FxRate {
    /// `rate` as decimal text (`"0.92"`).
    pub fn new(
        pair: CurrencyPair,
        rate: &str,
        source: Locator,
        as_of: Time,
    ) -> Result<FxRate, ValueError> {
        Ok(FxRate {
            pair,
            rate_e8: rate_text::parse(rate)?,
            source,
            as_of,
        })
    }

    /// The exact product `native × rate` in quote minor units × 10^8.
    fn product(&self, native: Money) -> Result<i128, ValueError> {
        if native.currency != self.pair.base {
            return Err(ValueError::new(
                codes::CURRENCY_MISMATCH,
                format!("{native} through a {} rate", self.pair),
            ));
        }
        // Both currencies have MINOR_DIGITS: minor × rate is quote minor.
        Ok(native.minor.0 as i128 * self.rate_e8 as i128)
    }

    /// `native` (in the base currency) in the quote currency, rounded for
    /// `flow`.
    pub fn convert(&self, native: Money, flow: Flow) -> Result<Money, ValueError> {
        let q = flow.div(self.product(native)?, RATE_SCALE);
        Ok(Money::new(
            Minor::from_i128(q, &format!("{native} × {}", self.rate_text()))?,
            self.pair.quote,
        ))
    }

    /// The rate as decimal text.
    pub fn rate_text(&self) -> String {
        format_scaled(self.rate_e8 as i128, RATE_DIGITS, true)
    }
}

/// A native amount, the rate that converted it and the result, kept apart
/// (module table; PRD § 5.2 data rule). No rate when the currencies match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConvertedRepr")]
pub struct Converted {
    native: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate: Option<FxRate>,
    converted: Money,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConvertedRepr {
    native: Money,
    #[serde(default)]
    rate: Option<FxRate>,
    converted: Money,
}

impl Converted {
    /// `native` in `target`, rounded for `flow`; `rate` required exactly
    /// when the currencies differ, and must be `native`/`target`.
    pub fn new(
        native: Money,
        target: Currency,
        rate: Option<FxRate>,
        flow: Flow,
    ) -> Result<Converted, ValueError> {
        let converted = match &rate {
            None if native.currency == target => native,
            None => {
                return Err(ValueError::new(
                    codes::INVALID_CONVERSION,
                    format!("{native} → {target}: no rate"),
                ))
            }
            Some(_) if native.currency == target => {
                return Err(ValueError::new(
                    codes::INVALID_CONVERSION,
                    format!("{native} → {target}: one currency takes no rate"),
                ))
            }
            Some(r) if r.pair.quote != target => {
                return Err(ValueError::new(
                    codes::CURRENCY_MISMATCH,
                    format!("{native} → {target} through a {} rate", r.pair),
                ))
            }
            Some(r) => r.convert(native, flow)?,
        };
        Ok(Converted {
            native,
            rate,
            converted,
        })
    }

    pub fn native(&self) -> Money {
        self.native
    }

    pub fn rate(&self) -> Option<&FxRate> {
        self.rate.as_ref()
    }

    pub fn converted(&self) -> Money {
        self.converted
    }
}

impl TryFrom<ConvertedRepr> for Converted {
    type Error = ValueError;
    fn try_from(r: ConvertedRepr) -> Result<Self, ValueError> {
        let target = r.converted.currency;
        let floor = Converted::new(r.native, target, r.rate.clone(), Flow::Inflow)?;
        let ceil = Converted::new(r.native, target, r.rate, Flow::Outflow)?;
        if r.converted != floor.converted && r.converted != ceil.converted {
            return Err(ValueError::new(
                codes::INVALID_CONVERSION,
                format!(
                    "{} converts to {} or {}, not {}",
                    r.native, floor.converted, ceil.converted, r.converted
                ),
            ));
        }
        Ok(Converted {
            converted: r.converted,
            ..floor
        })
    }
}

// ---------------------------------------------------------------------------
// Estimates, scenarios, assumptions
// ---------------------------------------------------------------------------

/// The three scenarios of an economic model (PRD § 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Scenario {
    Downside,
    Base,
    Upside,
}

impl Scenario {
    pub const ALL: [Scenario; 3] = [Scenario::Downside, Scenario::Base, Scenario::Upside];

    /// The range end this scenario takes; `None` for base (takes `base`).
    pub fn side(self) -> Option<Side> {
        match self {
            Scenario::Downside => Some(Side::Adverse),
            Scenario::Base => None,
            Scenario::Upside => Some(Side::Favorable),
        }
    }
}

/// An end of a range, seen from the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Favorable,
    Adverse,
}

/// Which way an input helps the candidate: revenue `Higher`, cost `Lower`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Better {
    Higher,
    Lower,
}

/// A three-point estimate or an explicit unknown (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Est<T> {
    Unknown { reason: Option<String> },
    Range { low: T, base: T, high: T },
}

impl<T> Est<T> {
    /// Unknown, with why (trimmed; an empty reason is none).
    pub fn unknown(reason: impl Into<String>) -> Est<T> {
        let reason = reason.into().trim().to_string();
        Est::Unknown {
            reason: (!reason.is_empty()).then_some(reason),
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Est::Range { .. })
    }

    pub fn base(&self) -> Option<&T> {
        match self {
            Est::Range { base, .. } => Some(base),
            Est::Unknown { .. } => None,
        }
    }

    /// The `side` end, for an input where `better` is the favorable way.
    pub fn end(&self, side: Side, better: Better) -> Option<&T> {
        let Est::Range { low, high, .. } = self else {
            return None;
        };
        Some(match (side, better) {
            (Side::Favorable, Better::Higher) | (Side::Adverse, Better::Lower) => high,
            (Side::Favorable, Better::Lower) | (Side::Adverse, Better::Higher) => low,
        })
    }

    /// The value `scenario` takes (module table); `None` when unknown.
    pub fn pick(&self, scenario: Scenario, better: Better) -> Option<&T> {
        match scenario.side() {
            None => self.base(),
            Some(side) => self.end(side, better),
        }
    }
}

impl<T: PartialOrd + fmt::Display> Est<T> {
    /// `low ≤ base ≤ high`, else `invalid_range` (values that do not
    /// compare — money in two currencies — included).
    pub fn range(low: T, base: T, high: T) -> Result<Est<T>, ValueError> {
        let le = |a: &T, b: &T| matches!(a.partial_cmp(b), Some(Ordering::Less | Ordering::Equal));
        if !le(&low, &base) || !le(&base, &high) {
            return Err(ValueError::new(
                codes::INVALID_RANGE,
                format!("low {low}, base {base}, high {high}: need low ≤ base ≤ high"),
            ));
        }
        Ok(Est::Range { low, base, high })
    }
}

impl<T: Clone> Est<T> {
    /// A known single value: `low = base = high`.
    pub fn point(v: T) -> Est<T> {
        Est::Range {
            low: v.clone(),
            base: v.clone(),
            high: v,
        }
    }
}

/// `"UNKNOWN"` → no reason, `"UNKNOWN: <reason>"` → the reason.
fn parse_unknown(s: &str) -> Result<Option<String>, ValueError> {
    if s == UNKNOWN {
        return Ok(None);
    }
    match s.strip_prefix(UNKNOWN).and_then(|r| r.strip_prefix(':')) {
        Some(reason) if !reason.trim().is_empty() => Ok(Some(reason.trim().to_string())),
        _ => Err(ValueError::new(
            codes::INVALID_ESTIMATE,
            format!(
                "`{s}`: an estimate is \"UNKNOWN\", \"UNKNOWN: <reason>\" or {{ low, base, high }}"
            ),
        )),
    }
}

impl<T: Serialize> Serialize for Est<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Est::Unknown { reason: None } => s.serialize_str(UNKNOWN),
            Est::Unknown {
                reason: Some(reason),
            } => s.collect_str(&format_args!("{UNKNOWN}: {reason}")),
            Est::Range { low, base, high } => {
                let mut st = s.serialize_struct("Est", 3)?;
                st.serialize_field("low", low)?;
                st.serialize_field("base", base)?;
                st.serialize_field("high", high)?;
                st.end()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeRepr<T> {
    low: T,
    base: T,
    high: T,
}

impl<'de, T> Deserialize<'de> for Est<T>
where
    T: Deserialize<'de> + PartialOrd + fmt::Display,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T> Visitor<'de> for V<T>
        where
            T: Deserialize<'de> + PartialOrd + fmt::Display,
        {
            type Value = Est<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("\"UNKNOWN\", \"UNKNOWN: <reason>\" or { low, base, high }")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Est<T>, E> {
                parse_unknown(v)
                    .map(|reason| Est::Unknown { reason })
                    .map_err(E::custom)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Est<T>, A::Error> {
                let r = RangeRepr::<T>::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Est::range(r.low, r.base, r.high).map_err(de::Error::custom)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// An estimate with its evidence and freshness (module table; PRD § 7.1:
/// "each assumption is a range with evidence and freshness").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(
        serialize = "T: Serialize",
        deserialize = "T: Deserialize<'de> + PartialOrd + fmt::Display"
    )
)]
pub struct Assumption<T> {
    pub value: Est<T>,
    pub evidence: Vec<Locator>,
    pub as_of: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl<T> Assumption<T> {
    /// At least one evidence locator that is not `UNKNOWN`.
    pub fn is_evidenced(&self) -> bool {
        self.evidence.iter().any(|l| *l != Locator::Unknown)
    }
}

// ---------------------------------------------------------------------------
// Schema tags
// ---------------------------------------------------------------------------

/// The only record schema version this build reads.
pub const SCHEMA_V1: u32 = 1;
const SCHEMA_PREFIX: &str = "soe.";

/// `soe.<record>/<version>` on every SOE record (module table).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaTag {
    record: String,
    version: u32,
}

fn valid_record_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl SchemaTag {
    /// `soe.<record>/1`.
    pub fn v1(record: &str) -> Result<SchemaTag, ValueError> {
        format!("{SCHEMA_PREFIX}{record}/{SCHEMA_V1}").parse()
    }

    pub fn record(&self) -> &str {
        &self.record
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// `Ok` when this tags `record` (`schema_mismatch` otherwise).
    pub fn require(&self, record: &str) -> Result<(), ValueError> {
        if self.record == record {
            Ok(())
        } else {
            Err(ValueError::new(
                codes::SCHEMA_MISMATCH,
                format!("schema `{self}`: a `{record}` record expected"),
            ))
        }
    }
}

impl FromStr for SchemaTag {
    type Err = ValueError;
    fn from_str(s: &str) -> Result<Self, ValueError> {
        let bad = || {
            ValueError::new(
                codes::INVALID_SCHEMA,
                format!("schema `{s}`: `soe.<record>/<version>`, record [a-z][a-z0-9_]*"),
            )
        };
        let (record, version) = s
            .strip_prefix(SCHEMA_PREFIX)
            .and_then(|r| r.split_once('/'))
            .ok_or_else(bad)?;
        let digits_ok = !version.is_empty()
            && version.bytes().all(|b| b.is_ascii_digit())
            && !version.starts_with('0');
        if !valid_record_name(record) || !digits_ok {
            return Err(bad());
        }
        let version: u32 = version.parse().map_err(|_| bad())?;
        if version != SCHEMA_V1 {
            return Err(ValueError::new(
                codes::SCHEMA_UNSUPPORTED,
                format!("schema `{s}`: this build reads version {SCHEMA_V1} only"),
            ));
        }
        Ok(SchemaTag {
            record: record.to_string(),
            version,
        })
    }
}

impl fmt::Display for SchemaTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{SCHEMA_PREFIX}{}/{}", self.record, self.version)
    }
}

impl Serialize for SchemaTag {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SchemaTag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        de_from_str(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Minor {
        s.parse().unwrap()
    }

    fn eur(s: &str) -> Money {
        Money::new(m(s), Currency::Eur)
    }

    fn code_of<T: fmt::Debug>(r: Result<T, ValueError>) -> &'static str {
        r.unwrap_err().code
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct One<T> {
        v: T,
    }

    fn toml_v<T: for<'de> Deserialize<'de>>(rhs: &str) -> Result<T, String> {
        toml::from_str::<One<T>>(&format!("v = {rhs}"))
            .map(|o| o.v)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn decimal_string_parses_exactly_to_minor_units() {
        assert_eq!(m("41250.75"), Minor(4_125_075));
        assert_eq!(m("7.5"), Minor(750));
        assert_eq!(m("0.01"), Minor(1));
        assert_eq!(m("-3.05"), Minor(-305));
        assert_eq!(m("2750"), Minor(275_000));
        assert_eq!(m("-0.00"), Minor(0));
        // Whole units from a TOML / JSON integer.
        assert_eq!(toml_v::<Minor>("2750").unwrap(), Minor(275_000));
        assert_eq!(toml_v::<Minor>("-40").unwrap(), Minor(-4_000));
        assert_eq!(toml_v::<Minor>("\"41250.75\"").unwrap(), Minor(4_125_075));
        assert_eq!(
            serde_json::from_str::<Minor>("2750").unwrap(),
            Minor(275_000)
        );
        // Exact where f64 is not: 0.10 + 0.20 is 0.30.
        assert_eq!(m("0.10").checked_add(m("0.20")).unwrap(), m("0.30"));
        // Prints with both digits; round-trips through TOML and JSON.
        for (text, shown) in [
            ("41250.75", "41250.75"),
            ("7.5", "7.50"),
            ("-3.05", "-3.05"),
            ("2750", "2750.00"),
            ("0.01", "0.01"),
        ] {
            let v = m(text);
            assert_eq!(v.to_string(), shown);
            let t = toml::to_string(&One { v }).unwrap();
            assert_eq!(t.trim(), format!("v = \"{shown}\""));
            assert_eq!(toml::from_str::<One<Minor>>(&t).unwrap().v, v);
            let j = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Minor>(&j).unwrap(), v);
        }
        // The i64 edge: the largest amount parses, one minor unit more does not.
        assert_eq!(m("92233720368547758.07"), Minor(i64::MAX));
        assert_eq!(m("-92233720368547758.08"), Minor(i64::MIN));
        assert_eq!(Minor(i64::MIN).to_string(), "-92233720368547758.08");
        assert_eq!(
            code_of("92233720368547758.08".parse::<Minor>()),
            codes::INVALID_AMOUNT
        );
        assert_eq!(code_of(Minor::units(i64::MAX)), codes::INVALID_AMOUNT);
        assert_eq!(
            code_of(Minor(i64::MAX).checked_add(Minor(1))),
            codes::OVERFLOW
        );
    }

    #[test]
    fn three_fraction_digits_refused() {
        let e = "41250.755".parse::<Minor>().unwrap_err();
        assert_eq!(e.code, codes::INVALID_AMOUNT);
        assert!(e.message.contains("3 fraction digits"), "{e}");
        assert!(toml_v::<Minor>("\"0.001\"")
            .unwrap_err()
            .contains("invalid_amount"));
        // Floats are never money, even when whole.
        for float in ["2750.0", "0.1", "1e3"] {
            let e = toml_v::<Minor>(float).unwrap_err();
            assert!(e.contains("is a float"), "{float}: {e}");
        }
        assert!(serde_json::from_str::<Minor>("0.5").is_err());
        for bad in [
            "", "-", ".", ".5", "5.", "+5", "1e3", "2,500", "2_500", " 5", "5 ", "--5", "5.-1",
            "EUR 5", "0x10",
        ] {
            assert_eq!(
                code_of(bad.parse::<Minor>()),
                codes::INVALID_AMOUNT,
                "{bad:?}"
            );
        }
    }

    #[test]
    #[allow(non_snake_case)] // names the marker it checks
    fn unknown_round_trips_as_UNKNOWN_string() {
        let plain: Est<Minor> = Est::Unknown { reason: None };
        assert_eq!(serde_json::to_string(&plain).unwrap(), "\"UNKNOWN\"");
        let t = toml::to_string(&One { v: plain.clone() }).unwrap();
        assert_eq!(t.trim(), "v = \"UNKNOWN\"");
        assert_eq!(toml::from_str::<One<Est<Minor>>>(&t).unwrap().v, plain);

        let why: Est<Minor> = Est::unknown("  no public price list ");
        assert_eq!(
            why,
            Est::Unknown {
                reason: Some("no public price list".into())
            }
        );
        let j = serde_json::to_string(&why).unwrap();
        assert_eq!(j, "\"UNKNOWN: no public price list\"");
        assert_eq!(serde_json::from_str::<Est<Minor>>(&j).unwrap(), why);
        assert_eq!(Est::<Minor>::unknown(" "), plain);

        // An unknown never yields a value — no scenario reads it as 0.
        for s in Scenario::ALL {
            for b in [Better::Higher, Better::Lower] {
                assert_eq!(why.pick(s, b), None);
            }
        }
        assert!(!why.is_known());

        // Anything else is refused: lower case, an empty reason, a bare
        // amount (a known value is a range).
        for bad in [
            "\"unknown\"",
            "\"UNKNOWN:\"",
            "\"UNKNOWN: \"",
            "\"UNKNOWNx\"",
            "\"41.00\"",
        ] {
            let e = toml_v::<Est<Minor>>(bad).unwrap_err();
            assert!(e.contains("invalid_estimate"), "{bad}: {e}");
        }
        assert!(toml_v::<Est<Minor>>("0").is_err());

        // A range round-trips as a table.
        let r = Est::range(m("10.00"), m("12.50"), m("20.00")).unwrap();
        let t = toml::to_string(&One { v: r.clone() }).unwrap();
        assert_eq!(toml::from_str::<One<Est<Minor>>>(&t).unwrap().v, r);
        assert_eq!(
            toml_v::<Est<u32>>("{ low = 4, base = 6, high = 10 }").unwrap(),
            Est::Range {
                low: 4,
                base: 6,
                high: 10
            }
        );
    }

    #[test]
    fn range_low_above_base_refused() {
        assert_eq!(
            code_of(Est::range(m("20.00"), m("10.00"), m("30.00"))),
            codes::INVALID_RANGE
        );
        assert_eq!(
            code_of(Est::range(m("10.00"), m("40.00"), m("30.00"))),
            codes::INVALID_RANGE
        );
        let e = toml_v::<Est<Minor>>("{ low = \"20.00\", base = \"10.00\", high = \"30.00\" }")
            .unwrap_err();
        assert!(e.contains("invalid_range"), "{e}");
        // Equal ends are a point.
        assert_eq!(Est::range(7u32, 7, 7).unwrap(), Est::point(7u32));
        // Money in two currencies does not compare: refused.
        let usd = Money::new(m("15.00"), Currency::Usd);
        assert_eq!(
            code_of(Est::range(eur("10.00"), usd, eur("20.00"))),
            codes::INVALID_RANGE
        );
        // A range table takes exactly low, base, high.
        assert!(toml_v::<Est<u32>>("{ low = 1, base = 2, high = 3, mid = 2 }").is_err());
        assert!(toml_v::<Est<u32>>("{ low = 1, high = 3 }").is_err());
    }

    #[test]
    fn fx_conversion_rounds_exposure_up_and_revenue_down() {
        let pair: CurrencyPair = "USD/EUR".parse().unwrap();
        let src: Locator = "url:https://example.org/fx/2026-10-01".parse().unwrap();
        let day: Time = "2026-10-01".parse().unwrap();
        let rate = FxRate::new(pair, "0.92345678", src, day).unwrap();
        assert_eq!(rate.rate_e8, 92_345_678);
        let usd = |s: &str| Money::new(m(s), Currency::Usd);
        // 10.01 USD × 0.92345678 = 9.2438023678 EUR.
        assert_eq!(
            rate.convert(usd("10.01"), Flow::Inflow).unwrap(),
            eur("9.24")
        );
        assert_eq!(
            rate.convert(usd("10.01"), Flow::Outflow).unwrap(),
            eur("9.25")
        );
        // A negative amount keeps the unflattering direction.
        assert_eq!(
            rate.convert(usd("-10.01"), Flow::Inflow).unwrap(),
            eur("-9.25")
        );
        assert_eq!(
            rate.convert(usd("-10.01"), Flow::Outflow).unwrap(),
            eur("-9.24")
        );
        // An exact product is not rounded either way.
        let half = FxRate::new(pair, "0.5", Locator::Unknown, Time::Unknown).unwrap();
        for flow in [Flow::Inflow, Flow::Outflow] {
            assert_eq!(half.convert(usd("10.00"), flow).unwrap(), eur("5.00"));
        }
        // The wrong base currency is refused, never silently used.
        assert_eq!(
            code_of(rate.convert(eur("1.00"), Flow::Inflow)),
            codes::CURRENCY_MISMATCH
        );

        // Converted keeps native, rate and result apart.
        let c = Converted::new(
            usd("10.01"),
            Currency::Eur,
            Some(rate.clone()),
            Flow::Outflow,
        )
        .unwrap();
        assert_eq!(
            (c.native(), c.converted(), c.rate()),
            (usd("10.01"), eur("9.25"), Some(&rate))
        );
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["rate"]["rate"], "0.92345678");
        assert_eq!(j["rate"]["source"], "url:https://example.org/fx/2026-10-01");
        assert_eq!(j["rate"]["as_of"], "2026-10-01");
        assert_eq!(j["converted"]["amount"], "9.25");
        assert_eq!(serde_json::from_value::<Converted>(j.clone()).unwrap(), c);
        let t = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<Converted>(&t).unwrap(), c);
        // A stored result off by more than the rounding is refused.
        let mut off = j.clone();
        off["converted"]["amount"] = "9.26".into();
        let e = serde_json::from_value::<Converted>(off).unwrap_err();
        assert!(e.to_string().contains("invalid_conversion"), "{e}");
        // One currency takes no rate; two need one.
        assert_eq!(
            Converted::new(eur("3.00"), Currency::Eur, None, Flow::Inflow)
                .unwrap()
                .converted(),
            eur("3.00")
        );
        assert_eq!(
            code_of(Converted::new(
                usd("3.00"),
                Currency::Eur,
                None,
                Flow::Inflow
            )),
            codes::INVALID_CONVERSION
        );
        assert_eq!(
            code_of(Converted::new(
                eur("3.00"),
                Currency::Eur,
                Some(rate.clone()),
                Flow::Inflow
            )),
            codes::INVALID_CONVERSION
        );
        assert_eq!(
            code_of(Converted::new(
                usd("3.00"),
                Currency::Gbp,
                Some(rate),
                Flow::Inflow
            )),
            codes::CURRENCY_MISMATCH
        );
        // Rates: > 0, ≤ 8 fraction digits, two different currencies.
        for bad in ["0", "0.000000001", "-1", "1e2", ""] {
            assert_eq!(
                code_of(FxRate::new(pair, bad, Locator::Unknown, Time::Unknown)),
                codes::INVALID_RATE,
                "{bad:?}"
            );
        }
        assert_eq!(
            code_of("EUR/EUR".parse::<CurrencyPair>()),
            codes::INVALID_RATE
        );
        assert_eq!(
            code_of("USD/JPY".parse::<CurrencyPair>()),
            codes::INVALID_CURRENCY
        );
    }

    #[test]
    fn schema_tag_other_version_refused() {
        let t: SchemaTag = "soe.operator_profile/1".parse().unwrap();
        assert_eq!((t.record(), t.version()), ("operator_profile", SCHEMA_V1));
        assert_eq!(t.to_string(), "soe.operator_profile/1");
        assert_eq!(SchemaTag::v1("operator_profile").unwrap(), t);
        assert!(t.require("operator_profile").is_ok());
        assert_eq!(code_of(t.require("eval_case")), codes::SCHEMA_MISMATCH);
        assert_eq!(
            code_of("soe.operator_profile/2".parse::<SchemaTag>()),
            codes::SCHEMA_UNSUPPORTED
        );
        let e = toml_v::<SchemaTag>("\"soe.eval_case/3\"").unwrap_err();
        assert!(e.contains("schema_unsupported"), "{e}");
        for bad in [
            "soe.operator_profile",
            "soe.operator_profile/",
            "soe.operator_profile/01",
            "soe.operator_profile/0",
            "soe.Operator/1",
            "soe./1",
            "soe.1x/1",
            "x.operator_profile/1",
            "operator_profile/1",
            "soe.a-b/1",
        ] {
            assert_eq!(
                code_of(bad.parse::<SchemaTag>()),
                codes::INVALID_SCHEMA,
                "{bad}"
            );
        }
        assert_eq!(
            toml::from_str::<One<SchemaTag>>("v = \"soe.opportunity/1\"")
                .unwrap()
                .v,
            SchemaTag::v1("opportunity").unwrap()
        );
    }

    #[test]
    fn bps_are_integers_from_0_to_10000() {
        assert_eq!(Bps::new(250).unwrap().get(), 250);
        assert_eq!(Bps::new(10_000).unwrap(), Bps::FULL);
        for bad in [-1, 10_001, 70_000] {
            assert_eq!(code_of(Bps::new(bad)), codes::INVALID_BPS);
        }
        assert_eq!(toml_v::<Bps>("2500").unwrap(), Bps::new(2500).unwrap());
        assert!(toml_v::<Bps>("10001").unwrap_err().contains("invalid_bps"));
        assert!(toml_v::<Bps>("2.5").is_err());
        assert!(toml_v::<Bps>("\"250\"").is_err());
        assert_eq!(
            Bps::new(1_250).unwrap().complement(),
            Bps::new(8_750).unwrap()
        );
        // 3 % of 0.50 = 0.015: an inflow floors, an outflow ceils.
        let b = Bps::new(300).unwrap();
        assert_eq!(m("0.50").mul_bps(b, Flow::Inflow), m("0.01"));
        assert_eq!(m("0.50").mul_bps(b, Flow::Outflow), m("0.02"));
        assert_eq!(m("-0.50").mul_bps(b, Flow::Inflow), m("-0.02"));
        assert_eq!(
            Minor(i64::MAX).mul_bps(Bps::FULL, Flow::Outflow),
            Minor(i64::MAX)
        );
    }

    #[test]
    fn rounding_never_flatters() {
        assert_eq!((div_floor(7, 2), div_ceil(7, 2)), (3, 4));
        assert_eq!((div_floor(-7, 2), div_ceil(-7, 2)), (-4, -3));
        assert_eq!((div_floor(7, -2), div_ceil(7, -2)), (-4, -3));
        assert_eq!((div_floor(-7, -2), div_ceil(-7, -2)), (3, 4));
        assert_eq!((div_floor(6, 3), div_ceil(6, 3)), (2, 2));
        assert_eq!((div_floor(0, 5), div_ceil(0, 5)), (0, 0));
        // 10.00 over 3 months: revenue 3.33 a month, cost 3.34.
        assert_eq!(m("10.00").mul_div(1, 3, Flow::Inflow).unwrap(), m("3.33"));
        assert_eq!(m("10.00").mul_div(1, 3, Flow::Outflow).unwrap(), m("3.34"));
        assert_eq!(
            code_of(m("1.00").mul_div(1, 0, Flow::Inflow)),
            codes::OVERFLOW
        );
        assert_eq!(
            code_of(Minor(i64::MAX).mul_div(2, 1, Flow::Inflow)),
            codes::OVERFLOW
        );
        assert_eq!(m("2.50").checked_mul(4).unwrap(), m("10.00"));
    }

    #[test]
    fn scenarios_take_the_adverse_end_for_downside() {
        let revenue = Est::range(m("10.00"), m("15.00"), m("25.00")).unwrap();
        let cost = Est::range(m("3.00"), m("4.00"), m("9.00")).unwrap();
        let pick = |e: &Est<Minor>, s, b| *e.pick(s, b).unwrap();
        assert_eq!(
            pick(&revenue, Scenario::Downside, Better::Higher),
            m("10.00")
        );
        assert_eq!(pick(&revenue, Scenario::Base, Better::Higher), m("15.00"));
        assert_eq!(pick(&revenue, Scenario::Upside, Better::Higher), m("25.00"));
        assert_eq!(pick(&cost, Scenario::Downside, Better::Lower), m("9.00"));
        assert_eq!(pick(&cost, Scenario::Base, Better::Lower), m("4.00"));
        assert_eq!(pick(&cost, Scenario::Upside, Better::Lower), m("3.00"));
        assert_eq!(
            serde_json::to_string(&Scenario::ALL).unwrap(),
            r#"["DOWNSIDE","BASE","UPSIDE"]"#
        );
    }

    #[test]
    fn currencies_are_a_closed_set_with_two_minor_digits() {
        for c in Currency::ALL {
            assert_eq!(c.exponent(), MINOR_DIGITS);
            assert_eq!(c.code().parse::<Currency>().unwrap(), c);
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.code())
            );
        }
        assert_eq!(code_of("JPY".parse::<Currency>()), codes::INVALID_CURRENCY);
        assert_eq!(code_of("eur".parse::<Currency>()), codes::INVALID_CURRENCY);
        // A source amount: a known currency is a point, another one unknown
        // with the reason, a malformed amount an error.
        assert_eq!(
            Money::from_native("41250.75", "EUR").unwrap(),
            Est::point(eur("41250.75"))
        );
        match Money::from_native("41250.75", "JPY").unwrap() {
            Est::Unknown { reason: Some(r) } => assert!(r.contains("`JPY`"), "{r}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            code_of(Money::from_native("1e3", "EUR")),
            codes::INVALID_AMOUNT
        );
    }

    #[test]
    fn money_stays_in_one_currency() {
        let usd = Money::new(m("1.00"), Currency::Usd);
        assert_eq!(eur("1.25").checked_add(eur("2.50")).unwrap(), eur("3.75"));
        assert_eq!(eur("1.25").checked_sub(eur("2.50")).unwrap(), eur("-1.25"));
        assert_eq!(
            code_of(eur("1.00").checked_add(usd)),
            codes::CURRENCY_MISMATCH
        );
        assert_eq!(eur("1.00").partial_cmp(&usd), None);
        assert!(eur("1.00") < eur("1.01"));
        let t = toml::to_string(&eur("250.00")).unwrap();
        assert_eq!(toml::from_str::<Money>(&t).unwrap(), eur("250.00"));
        assert_eq!(
            toml::from_str::<Money>("amount = 250\ncurrency = \"EUR\"").unwrap(),
            eur("250.00")
        );
        assert!(toml::from_str::<Money>("amount = 250\ncurrency = \"EUR\"\nfx = 1").is_err());
        assert!(toml::from_str::<Money>("minor = 250\ncurrency = \"EUR\"").is_err());
        assert_eq!(eur("250.00").to_string(), "250.00 EUR");
    }

    #[test]
    fn assumptions_carry_evidence_and_time() {
        let text = r#"
evidence = ["url:https://example.org/pricing", "repo:tests/fixtures/sec/x.htm"]
as_of = "2026-09-30"
note = "list price, example"
value = { low = "10.00", base = "12.50", high = "15.00" }
"#;
        let a: Assumption<Minor> = toml::from_str(text).unwrap();
        assert!(a.is_evidenced());
        assert_eq!(a.as_of.to_string(), "2026-09-30");
        assert_eq!(a.value.base(), Some(&m("12.50")));
        let back: Assumption<Minor> = toml::from_str(&toml::to_string(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        let unknown: Assumption<Bps> = toml::from_str(
            "value = \"UNKNOWN: no churn data\"\nevidence = []\nas_of = \"UNKNOWN\"",
        )
        .unwrap();
        assert!(!unknown.is_evidenced() && !unknown.value.is_known());
        // Unknown fields and a missing evidence list are refused.
        assert!(toml::from_str::<Assumption<Bps>>(
            "value = \"UNKNOWN\"\nevidence = []\nas_of = \"UNKNOWN\"\nsource = \"x\""
        )
        .is_err());
        assert!(
            toml::from_str::<Assumption<Bps>>("value = \"UNKNOWN\"\nas_of = \"UNKNOWN\"").is_err()
        );
    }
}
