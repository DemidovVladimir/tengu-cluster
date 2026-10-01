//! Exec-tool orders (`risk-gate-enforcement`): the pure parts of
//! `adapters/outbound/tools/xm/exec_common.rs::run_exec` — the idempotency
//! key, the venue facts the fill engine needs from the market rows, and the
//! typed results `paper_fill/1:<account>:<client_order_id>` and
//! `paper_close/1:<account>:<client_order_id>`. No IO; the gate
//! is `risk.rs`, the fill `paper.rs`, the ledger closure
//! `application/paper.rs::decide`.
//!
//! | Piece | Rule |
//! |---|---|
//! | `client_order_id` | the tool's arg, else `ToolCtx.call_id` — never a random id (a retry must deduplicate); 1–[`MAX_CLIENT_ORDER_ID`] chars, no whitespace or control chars; an arg never starts with a reserved prefix ([`RESERVED_CLIENT_ORDER_ID_PREFIXES`]: `exit:`, `fade:`, `fade-shadow:`, `feed:`, `mcp:`, `chat:`) |
//! | Fingerprint ([`order_fingerprint`]) | tool, account, full instrument id, `close` or side + notional — stored with the order; a replay asking for something else is refused |
//! | Exit bound | an exit's IOC bound ≤ [`MAX_EXIT_SLIPPAGE_BPS`] (500 bps from mid) |
//! | Underlying | the instrument's ledger position's (a close always matches it); none yet ⇒ the instrument id itself until the catalog maps underlyings (M1) — asset exposure then nets per instrument |
//! | Venue facts | `mkt_instrument/1:<id>` (any age: static facts): a Hyperliquid perp, `sz_decimals`, the fee = the `[paper]` fee basis × HIP-3 deployer scale × growth mode on a USDC-quoted market (`hl_ctx`'s `taker_fee_bps` rule, `domain::hl::paper_fees`); `at_oi_cap` from the `mkt_ctx/1` row when there is one, else the instrument row's (unknown ⇒ opening refused) |
//! | Market state of the fill | the `mkt_ctx/1` row: not listed ⇒ `delisted`; else `open` (no book ⇒ the book decides); no row ⇒ the instrument row's listing |
//! | `paper_fill/1` status | `ok` filled · `partial` partial fill · `error` denied by the gate or rejected by the venue (its reason in `errors`) |
//! | `paper_close/1` (`paper_close` with `all = true`) | one leg per open position, each also its own `paper_fill/1`; `ok` every leg filled (or nothing open) · `partial` some · `error` none |
//!
//! Line 1 of the row: status, side, the full instrument id, the fill, the
//! verdict and the full `client_order_id` (dropped from the headline, never
//! cut, when the line would pass 200 chars — the key keeps it).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::book::Side;
use crate::domain::hl::{paper_fees, FeeBasis};
use crate::domain::market::{InstrumentKind, Listing, MarketCtx, MarketInstrument, HYPERLIQUID};
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError, MAX_LINE1_CHARS,
};
use crate::domain::xm::cost::{FeeSchedule, HlKind};
use crate::domain::xm::paper::{FillResult, FillStatus, MarketStatus, VenueRules};
use crate::domain::xm::risk::{Check, HaltReason, OrderClass, RiskVerdict};

/// Longest `client_order_id` an exec tool takes (bridge ids are
/// `mcp:<32 hex>:<n>`, loop ids `<loop>:<session>:<t>`, a `paper_close`
/// leg `<base>:<instrument>`).
pub(crate) const MAX_CLIENT_ORDER_ID: usize = 256;

/// Prefixes of the ids the tools and the call paths make themselves: exit
/// attempts, the weekend fade's entries, feed / bridge / chat call ids. A
/// `client_order_id` argument may not start with one (review #11): it would
/// replay, or block, an order it did not place.
pub(crate) const RESERVED_CLIENT_ORDER_ID_PREFIXES: [&str; 6] =
    ["exit:", "fade:", "fade-shadow:", "feed:", "mcp:", "chat:"];

/// Hard ceiling of an exit's IOC bound (every reduce-only order: `xm_exits`,
/// `paper_close`, a reduce-only `paper_order`, the shadow exits), bps from
/// the mid (review #9): an argument above it is refused, a configured or
/// default bound above it (`[risk] max_slippage_bps`, `[xmarket.weekend_fade]
/// max_slippage_bps`) is cut to it. A `[risk.exits]` key may replace it
/// after the weekend run.
pub(crate) const MAX_EXIT_SLIPPAGE_BPS: f64 = 500.0;

/// Why `id` cannot key an order; `None` when it can.
pub(crate) fn client_order_id_error(id: &str) -> Option<String> {
    let n = id.chars().count();
    if n == 0 {
        Some("client_order_id is empty".into())
    } else if n > MAX_CLIENT_ORDER_ID {
        Some(format!(
            "client_order_id is {n} chars (at most {MAX_CLIENT_ORDER_ID})"
        ))
    } else if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("client_order_id holds whitespace or a control character".into())
    } else {
        None
    }
}

/// Why a `client_order_id` *argument* is refused: [`client_order_id_error`],
/// or a reserved prefix ([`RESERVED_CLIENT_ORDER_ID_PREFIXES`]).
pub(crate) fn client_order_id_arg_error(id: &str) -> Option<String> {
    client_order_id_error(id).or_else(|| {
        RESERVED_CLIENT_ORDER_ID_PREFIXES
            .iter()
            .find(|p| id.starts_with(*p))
            .map(|p| {
                format!(
                    "client_order_id `{id}` starts with the reserved prefix `{p}` (ids the exit \
                     rules, the weekend fade and the feed / bridge / chat call paths make)"
                )
            })
    })
}

/// What an order asked for (review #11), stored with it: a replay under
/// its `client_order_id` must ask for the same — `<tool> <account> <full
/// instrument id> close` or `… <side> <notional> USD`.
pub(crate) fn order_fingerprint(
    tool: &str,
    account: &str,
    instrument: &str,
    size: Option<(Side, f64)>,
) -> String {
    match size {
        None => format!("{tool} {account} {instrument} close"),
        Some((side, usd)) => format!("{tool} {account} {instrument} {} {usd} USD", side.as_str()),
    }
}

/// The fill engine's view of one instrument.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VenueFacts {
    pub rules: VenueRules,
    pub fees: FeeSchedule,
    pub status: MarketStatus,
}

/// Venue facts from the stored rows (module table). `Err` names what is
/// missing — the exec tool refuses before anything is sent.
pub(crate) fn venue_facts(
    instrument: Option<&MarketInstrument>,
    ctx: Option<&MarketCtx>,
    basis: FeeBasis,
) -> Result<VenueFacts, String> {
    let Some(inst) = instrument else {
        return Err("no mkt_instrument/1 row (read hl_ctx first)".into());
    };
    if inst.id.venue() != HYPERLIQUID || inst.kind != InstrumentKind::Perp {
        return Err(format!(
            "{} is a {} {} market — paper orders take Hyperliquid perps only",
            inst.id,
            inst.id.venue(),
            inst.kind.as_str()
        ));
    }
    let sz_decimals = inst
        .sz_decimals
        .ok_or_else(|| format!("{}: sz_decimals unknown", inst.id))?;
    let fees = paper_fees(inst, basis).ok_or_else(|| {
        format!(
            "{}: taker fee unknown (a USDC-quoted perp with a known deployer fee scale and \
             growth mode is needed)",
            inst.id
        )
    })?;
    let at_oi_cap = ctx.and_then(|c| c.at_oi_cap).or(inst.at_oi_cap);
    let listing = ctx.map_or(inst.listing, |c| c.listing);
    let status = if listing == Listing::Listed {
        MarketStatus::Open
    } else {
        MarketStatus::Delisted
    };
    Ok(VenueFacts {
        rules: VenueRules::hyperliquid(HlKind::Perp, sz_decimals, at_oi_cap),
        fees,
        status,
    })
}

/// The gate's answer as a `paper_fill/1` row carries it (the verdict row in
/// the ledger keeps every check).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GateSummary {
    pub allow: bool,
    pub rule: String,
    pub class: OrderClass,
    pub degraded: bool,
    /// The first failed check, when denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trips: Vec<HaltReason>,
    /// `risk_decisions.id` in `ledger.db`.
    pub decision_id: i64,
}

impl GateSummary {
    pub(crate) fn of(verdict: &RiskVerdict, decision_id: i64) -> Self {
        Self {
            allow: verdict.allow,
            rule: verdict.rule.clone(),
            class: verdict.class,
            degraded: verdict.degraded,
            failed: verdict.failed().cloned(),
            trips: verdict.trips.clone(),
            decision_id,
        }
    }
}

/// `paper_fill/1:<account>:<client_order_id>` — one exec-tool order: the
/// gate's verdict and, when allowed, the paper fill. TTL 0 (never cached).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PaperFillRow {
    pub account: String,
    pub client_order_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Full id (`hyperliquid:xyz:TSLA`).
    pub instrument: String,
    pub side: Side,
    pub reduce_only: bool,
    /// Stored before under this `client_order_id`: nothing was judged,
    /// slept, read or written — the stored result.
    pub replayed: bool,
    pub gate: GateSummary,
    /// The `OrderIntent` as judged (qty, notional, underlying, opportunity).
    pub intent: Value,
    /// The fill engine's result; `None` when denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<FillResult>,
    /// Latency slept before the book read; `None` on a replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Age of the book the gate and the fill used; `None` when none was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_age_ms: Option<u64>,
    /// The instrument's position after the transaction (signed; 0 = flat).
    pub position_qty_after: f64,
    /// The account's equity after it, at fresh marks — never 0 for a failed mark.
    pub equity_usd_after: Field<f64>,
    /// Deadline of the position this order opened (exit rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_at_ms: Option<i64>,
    pub ts_ms: i64,
}

impl PaperFillRow {
    /// `filled` · `partial` · `rejected` (venue) · `denied` (gate).
    pub(crate) fn status_word(&self) -> &'static str {
        match &self.fill {
            None => "denied",
            Some(f) => f.status.as_str(),
        }
    }

    fn intent_num(&self, key: &str) -> Option<f64> {
        self.intent.get(key).and_then(Value::as_f64)
    }
}

impl Observed for PaperFillRow {
    const SCHEMA: &'static str = "paper_fill/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.account, self.client_order_id)
    }

    fn headline(&self) -> String {
        let mut h = format!(
            "paper_fill {} {} {}",
            self.status_word(),
            self.side.as_str(),
            self.instrument
        );
        match &self.fill {
            Some(f) if f.filled_qty > 0.0 => {
                h.push_str(&format!(
                    " qty={} notional={:.2} fee={:.4}",
                    f.filled_qty, f.filled_notional_usd, f.fee_usd
                ));
                if let Some(px) = f.avg_px {
                    h.push_str(&format!(" avg_px={px}"));
                }
                if let Some(s) = f.slippage_bps {
                    h.push_str(&format!(" slip_bps={s:.2}"));
                }
                // A partial fill: why the rest was canceled.
                if let Some(r) = f.reason {
                    h.push_str(&format!(" rest={}", r.as_str()));
                }
            }
            Some(f) => {
                if let Some(r) = f.reason {
                    h.push_str(&format!(" reason={}", r.as_str()));
                }
            }
            None => {
                if let Some(n) = self.intent_num("notional_usd") {
                    h.push_str(&format!(" notional={n:.2}"));
                }
            }
        }
        h.push_str(&format!(
            " risk={} rule={}",
            if self.gate.allow { "allow" } else { "deny" },
            self.gate.rule
        ));
        if self.replayed {
            h.push_str(" replayed");
        }
        let with_id = format!("{h} coid={}", self.client_order_id);
        if with_id.chars().count() <= MAX_LINE1_CHARS {
            with_id
        } else {
            h
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "status", Some(self.status_word()));
        set_str(
            &mut f,
            "risk",
            Some(if self.gate.allow { "allow" } else { "deny" }),
        );
        set_str(&mut f, "risk_rule", Some(&self.gate.rule));
        set_str(&mut f, "class", Some(self.gate.class.as_str()));
        set_bool(&mut f, "degraded", Some(self.gate.degraded));
        set_bool(&mut f, "replayed", Some(self.replayed));
        set_str(&mut f, "side", Some(self.side.as_str()));
        set_bool(&mut f, "reduce_only", Some(self.reduce_only));
        set_num(&mut f, "intent_qty", self.intent_num("qty"));
        set_num(
            &mut f,
            "intent_notional_usd",
            self.intent_num("notional_usd"),
        );
        if let Some(fill) = &self.fill {
            set_num(&mut f, "filled_qty", Some(fill.filled_qty));
            set_num(
                &mut f,
                "filled_notional_usd",
                Some(fill.filled_notional_usd),
            );
            set_num(&mut f, "avg_px", fill.avg_px);
            set_num(&mut f, "mid", fill.mid);
            set_num(&mut f, "slippage_bps", fill.slippage_bps);
            set_num(&mut f, "fee_usd", Some(fill.fee_usd));
            set_int(&mut f, "levels_used", Some(fill.fills.len() as i64));
            set_str(&mut f, "reason", fill.reason.map(|r| r.as_str()));
        }
        set_int(
            &mut f,
            "latency_ms",
            self.latency_ms.and_then(|l| i64::try_from(l).ok()),
        );
        set_int(
            &mut f,
            "book_age_ms",
            self.book_age_ms.and_then(|a| i64::try_from(a).ok()),
        );
        set_num(&mut f, "position_qty_after", Some(self.position_qty_after));
        set_num(
            &mut f,
            "equity_usd_after",
            self.equity_usd_after.value().copied(),
        );
        set_int(&mut f, "exit_at_ms", self.exit_at_ms);
        f
    }

    fn status(&self) -> ObsStatus {
        match self.fill.as_ref().map(|f| f.status) {
            None | Some(FillStatus::Rejected) => ObsStatus::Error,
            Some(FillStatus::Partial) => ObsStatus::Partial,
            Some(FillStatus::Filled) => ObsStatus::Ok,
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        match &self.fill {
            None => {
                let detail = self
                    .gate
                    .failed
                    .as_ref()
                    .map_or(String::new(), |c| format!(": {}", c.detail));
                vec![ReadError::new(
                    "risk",
                    ErrorClass::NotApplicable,
                    format!("denied {}{detail}", self.gate.rule),
                )]
            }
            Some(f) if f.status == FillStatus::Rejected => vec![ReadError::new(
                "fill",
                ErrorClass::NotApplicable,
                rejected_message(f),
            )],
            _ => Vec::new(),
        }
    }
}

/// `rejected <reason>: <message>` — a venue rejection as the row's error
/// reads it (the weekend fade reads stored attempts the same way).
pub(crate) fn rejected_message(f: &FillResult) -> String {
    format!(
        "rejected {}: {}",
        f.reason.map_or("-", |r| r.as_str()),
        f.message.as_deref().unwrap_or("")
    )
}

/// One order of a `paper_close` with `all = true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CloseLeg {
    pub instrument: String,
    pub client_order_id: String,
    /// `filled` · `partial` · `rejected` · `denied`, or `error` when the
    /// order was not placed (the message in `error`).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_px: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `paper_close/1:<account>:<client_order_id>` — `paper_close` with `all =
/// true`: one reduce-only order per open position (each also its own
/// `paper_fill/1` row). TTL 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PaperCloseAll {
    pub account: String,
    /// The base id; each leg's is `<base>:<instrument>`.
    pub client_order_id: String,
    pub legs: Vec<CloseLeg>,
    pub ts_ms: i64,
}

impl PaperCloseAll {
    fn count(&self, status: &str) -> usize {
        self.legs.iter().filter(|l| l.status == status).count()
    }
}

impl Observed for PaperCloseAll {
    const SCHEMA: &'static str = "paper_close/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.account, self.client_order_id)
    }

    fn headline(&self) -> String {
        let mut h = format!(
            "paper_close all account={} positions={} filled={} partial={} rejected={} denied={} errors={}",
            self.account,
            self.legs.len(),
            self.count("filled"),
            self.count("partial"),
            self.count("rejected"),
            self.count("denied"),
            self.count("error"),
        );
        let with_id = format!("{h} coid={}", self.client_order_id);
        if with_id.chars().count() <= MAX_LINE1_CHARS {
            h = with_id;
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "positions", Some(self.legs.len() as i64));
        for s in ["filled", "partial", "rejected", "denied", "error"] {
            set_int(&mut f, s, Some(self.count(s) as i64));
        }
        f
    }

    /// `ok` when every position closed; `partial` when some did; `error`
    /// when none did (nothing open is `ok`).
    fn status(&self) -> ObsStatus {
        let closed = self.count("filled");
        if closed == self.legs.len() {
            ObsStatus::Ok
        } else if closed + self.count("partial") > 0 {
            ObsStatus::Partial
        } else {
            ObsStatus::Error
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.legs
            .iter()
            .filter(|l| l.status != "filled")
            .map(|l| {
                let why = l
                    .error
                    .clone()
                    .or_else(|| l.rule.clone())
                    .unwrap_or_default();
                ReadError::new(
                    format!("close:{}", l.instrument),
                    ErrorClass::NotApplicable,
                    format!("{} {why}", l.status),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{InstrumentId, QuoteCcy};
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation};
    use crate::domain::xm::paper::{FillReason, OrderKind};
    use crate::domain::xm::risk::CheckStatus;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const COID: &str = "mcp:0f1e2d3c4b5a69788796a5b4c3d2e1f0:7";

    fn tsla() -> MarketInstrument {
        let mut i = MarketInstrument::new(
            InstrumentId::parse(TSLA).unwrap(),
            InstrumentKind::Perp,
            Listing::Listed,
        );
        i.sz_decimals = Some(3);
        i.quote_ccy = Some(QuoteCcy::Usdc);
        i.deployer_fee_scale = Some(1.0);
        i.growth_mode = Some(true);
        i.at_oi_cap = Some(false);
        i
    }

    #[test]
    fn client_order_ids_are_checked_never_made_up() {
        assert_eq!(client_order_id_error(COID), None);
        assert_eq!(
            client_order_id_error("xm_entry:0001318605-26-000123:3"),
            None
        );
        for bad in ["", "a b", "tab\tid", &"x".repeat(257)] {
            assert!(client_order_id_error(bad).is_some(), "{bad:?}");
        }
        assert_eq!(client_order_id_error(&"x".repeat(256)), None);
        // Review #11: an argument may not take an id the tools make.
        for reserved in [
            "exit:xmarket:hyperliquid:xyz:TSLA:deadline:1",
            "fade:xmarket:hyperliquid:xyz:TSLA:2026-10-02",
            "fade-shadow:s:hyperliquid:xyz:TSLA:2026-10-02",
            "feed:xm_exits:1790775352605:0",
            COID,
            "chat:0f1e:1:0:call_1",
        ] {
            let e = client_order_id_arg_error(reserved).unwrap();
            assert!(e.contains("reserved prefix"), "{e}");
            assert_eq!(client_order_id_error(reserved), None, "a tool's own id");
        }
        for ok in ["xm_entry:0001318605-26-000123:3", "my-exit:1", "feeder:1"] {
            assert_eq!(client_order_id_arg_error(ok), None, "{ok}");
        }
        assert!(client_order_id_arg_error("a b")
            .unwrap()
            .contains("whitespace"));
        assert_eq!(
            order_fingerprint("paper_order", "xmarket", TSLA, Some((Side::Buy, 25.0))),
            "paper_order xmarket hyperliquid:xyz:TSLA buy 25 USD"
        );
        assert_eq!(
            order_fingerprint("xm_exits", "xmarket", TSLA, None),
            "xm_exits xmarket hyperliquid:xyz:TSLA close"
        );
    }

    /// `xyz:TSLA` tier 0: 4.5 × 2 (scale 1) × 0.1 (growth mode) = 0.9 bps —
    /// the `hl_ctx` number; the ctx row's at-cap state wins over the
    /// instrument row's.
    #[test]
    fn venue_facts_come_from_the_instrument_row() {
        let basis = FeeBasis::default();
        let f = venue_facts(Some(&tsla()), None, basis).unwrap();
        assert!((f.fees.taker_bps - 0.9).abs() < 1e-12, "{f:?}");
        assert_eq!(
            (f.rules.sz_decimals, f.rules.at_oi_cap, f.status),
            (3, Some(false), MarketStatus::Open)
        );
        let mut ctx = MarketCtx::new(InstrumentId::parse(TSLA).unwrap(), 1);
        ctx.at_oi_cap = Some(true);
        let f = venue_facts(Some(&tsla()), Some(&ctx), basis).unwrap();
        assert_eq!(f.rules.at_oi_cap, Some(true));
        ctx.listing = Listing::Delisted;
        let f = venue_facts(Some(&tsla()), Some(&ctx), basis).unwrap();
        assert_eq!(f.status, MarketStatus::Delisted);

        let err = |i: Option<&MarketInstrument>| venue_facts(i, None, basis).unwrap_err();
        assert!(err(None).contains("no mkt_instrument/1 row"));
        let mut no_sz = tsla();
        no_sz.sz_decimals = None;
        assert!(err(Some(&no_sz)).contains("hyperliquid:xyz:TSLA: sz_decimals unknown"));
        let mut usdh = tsla();
        usdh.quote_ccy = Some(QuoteCcy::Usdh);
        assert!(err(Some(&usdh)).contains("taker fee unknown"));
        let mut spot = tsla();
        spot.kind = InstrumentKind::Spot;
        assert!(err(Some(&spot)).contains("Hyperliquid perps only"));
    }

    fn verdict(allow: bool, rule: &str) -> RiskVerdict {
        RiskVerdict {
            allow,
            rule: rule.into(),
            class: OrderClass::Entry,
            degraded: false,
            checks: vec![Check {
                rule: rule.into(),
                status: if allow {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                detail:
                    "edge 4 bps < 10 bps on xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA"
                        .into(),
            }],
            headroom: Default::default(),
            trips: vec![],
        }
    }

    fn fill(status: FillStatus, qty: f64) -> FillResult {
        FillResult {
            client_order_id: COID.into(),
            instrument: InstrumentId::parse(TSLA).unwrap(),
            side: Side::Buy,
            kind: OrderKind::Market,
            reduce_only: false,
            status,
            reason: (status == FillStatus::Rejected).then_some(FillReason::MinTradeNtl),
            message: (status == FillStatus::Rejected)
                .then(|| "Order must have minimum value of $10. (x)".to_string()),
            order_qty: Some(0.072),
            ref_px: Some(347.195),
            bound_px: Some(348.23),
            fills: Vec::new(),
            filled_qty: qty,
            filled_notional_usd: qty * 347.23,
            avg_px: (qty > 0.0).then_some(347.23),
            mid: Some(347.195),
            slippage_bps: (qty > 0.0).then_some(1.008),
            fee_usd: qty * 347.23 * 0.9e-4,
            book_age_ms: Some(20),
        }
    }

    fn row(gate: RiskVerdict, f: Option<FillResult>) -> PaperFillRow {
        PaperFillRow {
            account: "xmarket".into(),
            client_order_id: COID.into(),
            call_id: Some(COID.into()),
            instrument: TSLA.into(),
            side: Side::Buy,
            reduce_only: false,
            replayed: false,
            gate: GateSummary::of(&gate, 7),
            intent: serde_json::json!({"qty": 0.072, "notional_usd": 25.0}),
            fill: f,
            latency_ms: Some(250),
            book_age_ms: Some(20),
            position_qty_after: 0.072,
            equity_usd_after: Field::ok(99.99),
            exit_at_ms: None,
            ts_ms: 1_790_775_353_005,
        }
    }

    #[test]
    fn a_fill_row_says_what_happened_in_line_one() {
        let filled = row(verdict(true, "ok"), Some(fill(FillStatus::Filled, 0.072)));
        let o = Observation::of("paper_order", &filled, filled.ts_ms, 0, ObsSource::Live);
        assert_eq!(o.key, format!("paper_fill/1:xmarket:{COID}"));
        assert_eq!(o.status, ObsStatus::Ok);
        assert_features_ok(&o.features);
        assert_eq!(
            o.headline,
            format!(
                "paper_fill filled buy {TSLA} qty=0.072 notional=25.00 fee=0.0023 avg_px=347.23 \
                 slip_bps=1.01 risk=allow rule=ok coid={COID}"
            )
        );
        for (k, v) in [("risk", "allow"), ("status", "filled"), ("risk_rule", "ok")] {
            assert_eq!(o.features[k], v, "{k}");
        }
        assert_eq!(o.features["levels_used"], 0);
        assert_eq!(o.features["latency_ms"], 250);

        let denied = row(verdict(false, "min_edge"), None);
        let o = Observation::of("paper_order", &denied, denied.ts_ms, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Error);
        assert!(o.headline.starts_with(&format!(
            "paper_fill denied buy {TSLA} notional=25.00 risk=deny rule=min_edge coid={COID}"
        )));
        assert!(
            !o.features.contains_key("filled_notional_usd"),
            "no fill, no 0"
        );
        assert!(o.errors[0]
            .message
            .starts_with("denied min_edge: edge 4 bps"));
        assert_eq!(o.features["risk"], "deny");

        let rejected = row(verdict(true, "ok"), Some(fill(FillStatus::Rejected, 0.0)));
        let o = Observation::of("paper_order", &rejected, 1, 0, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Error);
        assert!(o.headline.contains("rejected buy") && o.headline.contains("reason=MinTradeNtl"));
        assert_eq!(
            o.features["filled_notional_usd"], 0.0,
            "nothing filled is a true 0"
        );
        assert!(o.errors[0].message.starts_with("rejected MinTradeNtl"));

        let partial = row(verdict(true, "ok"), Some(fill(FillStatus::Partial, 0.05)));
        assert_eq!(partial.status(), ObsStatus::Partial);
    }

    /// Line 1 keeps ids whole: an id too long for 200 chars leaves the
    /// headline (the key holds it).
    #[test]
    fn a_long_id_leaves_the_headline_whole() {
        let mut r = row(verdict(true, "ok"), Some(fill(FillStatus::Filled, 0.072)));
        r.client_order_id = format!("loop:{}:3", "e".repeat(120));
        let h = r.headline();
        assert!(h.chars().count() <= MAX_LINE1_CHARS, "{h}");
        assert!(!h.contains("coid="), "{h}");
        assert!(h.contains(TSLA));
    }

    #[test]
    fn close_all_counts_its_legs() {
        let leg = |i: &str, s: &str| CloseLeg {
            instrument: i.into(),
            client_order_id: format!("base:{i}"),
            status: s.into(),
            rule: (s == "denied").then(|| "halted".to_string()),
            filled_qty: None,
            avg_px: None,
            error: None,
        };
        let mut all = PaperCloseAll {
            account: "xmarket".into(),
            client_order_id: "base".into(),
            legs: vec![leg(TSLA, "filled"), leg("hyperliquid:xyz:NVDA", "denied")],
            ts_ms: 1,
        };
        let o = Observation::of("paper_close", &all, 1, 0, ObsSource::Live);
        assert_eq!(o.key, "paper_close/1:xmarket:base");
        assert_eq!(o.status, ObsStatus::Partial);
        assert!(o.headline.starts_with(
            "paper_close all account=xmarket positions=2 filled=1 partial=0 rejected=0 denied=1"
        ));
        assert_eq!(o.errors[0].field, "close:hyperliquid:xyz:NVDA");
        all.legs.clear();
        assert_eq!(all.status(), ObsStatus::Ok, "nothing open");
    }
}
