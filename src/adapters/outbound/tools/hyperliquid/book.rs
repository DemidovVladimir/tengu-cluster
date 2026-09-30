//! `hl_book` — one coin's L2 book through the cache (row rules:
//! `domain/hl/book.rs`).
//!
//! | Step | Rule |
//! |---|---|
//! | Args | `coin`* (HL name, verbatim), `notional_usd` (≤ 3, each > 0; absent = `[100, 1000, 10000]`, `[]` = none, a single number is accepted), `include_trades` (false), `max_age_secs` |
//! | Cache | a fresh `hl_book/1:hyperliquid:<coin>` row (≤ min(2 s, `max_age_secs`)) is re-derived for the requested notionals — no request; `include_trades` on a row without a last trade reads live; a trades error is dropped when trades were not asked for |
//! | Reads | `l2Book {coin}` (weight 2, full precision, ≤ 20 levels per side); with `include_trades` also `recentTrades {coin}` (20 + 1 per 20 trades), concurrently |
//! | Writes | the live row (2 s), recorded like `observe()`; an `error` row is recorded, never stored. The stored row carries the notionals of the call that wrote it (`notional_usd_k`) |
//! | HL errors | `200 null` / `500 null` ⇒ `not_found` (absent); anything else ⇒ `error` with the class (`outbound/hyperliquid/info.rs`) |
//! | Paper fill engine | [`HlBookSource`] (the exec tools' live `BookSource`) → [`fresh_book`]: `l2Book` now (never the cache), stored + recorded with the default notionals, `(Observation, L2Book)` or the `ReadError` a `BookSource` returns |

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use super::{defs, opt_u64_arg, policy, store_live, HlShared};
use crate::adapters::outbound::http_class::read_error;
use crate::adapters::outbound::hyperliquid::info::{HlInfo, InfoReply};
use crate::adapters::outbound::tools::args::require_str;
use crate::domain::book::L2Book;
use crate::domain::hl::book::{
    decode_l2_book, decode_last_trade, HlBook, BOOK_TTL_MS, DEFAULT_NOTIONALS_USD, LAST_FIELD,
    MAX_NOTIONALS,
};
use crate::domain::market::{InstrumentId, HYPERLIQUID};
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, ErrorClass, ObsSource, Observation, Observed, ReadError};
use crate::domain::tools as names;
use crate::ports::book::{BookRead, BookSource};
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) fn tools(shared: &HlShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(HlBookTool::new(shared.clone()))]
}

/// One `hl_book` read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BookRequest {
    pub id: InstrumentId,
    /// USD notionals the slippage features walk (≤ 3).
    pub notional_usd: Vec<f64>,
    pub include_trades: bool,
    /// `Some(0)` forces a live read.
    pub max_age_secs: Option<u64>,
}

impl BookRequest {
    /// Parse the tool arguments (module table).
    pub(crate) fn from_args(args: &Value) -> Result<Self> {
        let tool = names::HL_BOOK;
        let coin = require_str(args, tool, "coin")?.trim();
        let id = InstrumentId::hyperliquid(coin).map_err(|e| anyhow!("{tool}: {e}"))?;
        let positive = |v: &Value| {
            v.as_f64()
                .filter(|x| x.is_finite() && *x > 0.0)
                .ok_or_else(|| {
                    anyhow!("{tool}: 'notional_usd' entries must be numbers > 0, got {v}")
                })
        };
        let notional_usd = match args.get("notional_usd") {
            None | Some(Value::Null) => DEFAULT_NOTIONALS_USD.to_vec(),
            Some(Value::Array(items)) => {
                if items.len() > MAX_NOTIONALS {
                    bail!(
                        "{tool}: {} notionals given, at most {MAX_NOTIONALS}",
                        items.len()
                    );
                }
                items.iter().map(positive).collect::<Result<Vec<_>>>()?
            }
            Some(v @ Value::Number(_)) => vec![positive(v)?],
            Some(v) => bail!("{tool}: 'notional_usd' must be an array of numbers, got {v}"),
        };
        let include_trades = match args.get("include_trades") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(v) => bail!("{tool}: 'include_trades' must be a boolean, got {v}"),
        };
        Ok(Self {
            id,
            notional_usd,
            include_trades,
            max_age_secs: opt_u64_arg(args, tool, "max_age_secs")?,
        })
    }

    /// The paper fill engine's read: a live book, no trades; the default
    /// notionals keep the stored row's features the shape `world` expects.
    pub(crate) fn fresh(id: InstrumentId) -> Self {
        Self {
            id,
            notional_usd: DEFAULT_NOTIONALS_USD.to_vec(),
            include_trades: false,
            max_age_secs: Some(0),
        }
    }
}

pub(crate) struct HlBookTool {
    def: ToolDef,
    shared: HlShared,
}

impl HlBookTool {
    pub(crate) fn new(shared: HlShared) -> Self {
        Self {
            def: defs::def(names::HL_BOOK),
            shared,
        }
    }
}

#[async_trait]
impl Tool for HlBookTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = BookRequest::from_args(args)?;
        let now = now_ms();
        let obs = match HlInfo::from_ctx(ctx) {
            Ok(hl) => {
                read_book(&hl, self.shared.store.as_deref(), &req, now)
                    .await
                    .0
            }
            Err(e) => {
                let row = HlBook::failed(
                    req.id.clone(),
                    now,
                    req.notional_usd.clone(),
                    read_error("book", &e),
                );
                Observation::of(names::HL_BOOK, &row, now, BOOK_TTL_MS, ObsSource::Live)
            }
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

/// Read `req.id`'s book through the cache (module table). Returns the
/// `hl_book/1` observation and its book — `None` when HL does not know the
/// coin or the read failed.
pub(crate) async fn read_book(
    hl: &HlInfo,
    store: Option<&dyn ObservationStore>,
    req: &BookRequest,
    now_ms: i64,
) -> (Observation, Option<L2Book>) {
    let policy = policy(
        HlBook::SCHEMA,
        &req.id.to_string(),
        BOOK_TTL_MS,
        req.max_age_secs,
    );
    if let Some(s) = store {
        match s.get(&policy.key).await {
            Ok(Some(row)) if row.is_fresh(now_ms, policy.max_age_ms) => {
                if let Some(hit) = rederive(&row, req) {
                    return hit;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let error = format!("{e:#}");
                warn!(key = %policy.key, %error, "observation store read failed; reading live");
            }
        }
    }
    let row = read_live(hl, req, now_ms).await;
    let obs = Observation::of(names::HL_BOOK, &row, now_ms, BOOK_TTL_MS, ObsSource::Live);
    store_live(store, &obs).await;
    (obs, row.book)
}

/// The paper fill engine's live book (behind [`HlBookSource`]): `l2Book`
/// for `id` now — never the cache — recorded and stored as `hl_book/1:<id>`
/// like any `hl_book` read ([`BookRequest::fresh`]). An empty book
/// (delisted / halted) is a book. `Err` = no book: `not_applicable` when
/// `id` is not a Hyperliquid instrument or HL does not know it, else the
/// failed read's class.
pub(crate) async fn fresh_book(
    hl: &HlInfo,
    store: Option<&dyn ObservationStore>,
    id: &InstrumentId,
    now_ms: i64,
) -> std::result::Result<(Observation, L2Book), ReadError> {
    if id.venue() != HYPERLIQUID {
        return Err(ReadError::new(
            "book",
            ErrorClass::NotApplicable,
            format!("{id} is not a Hyperliquid instrument"),
        ));
    }
    let (obs, book) = read_book(hl, store, &BookRequest::fresh(id.clone()), now_ms).await;
    match book {
        Some(b) => Ok((obs, b)),
        None => Err(obs.errors.first().cloned().unwrap_or_else(|| {
            ReadError::new(
                "book",
                ErrorClass::NotApplicable,
                format!("Hyperliquid does not list {id}"),
            )
        })),
    }
}

/// The live `ports::book::BookSource` of the exec tools (tracker convention
/// 16): [`fresh_book`] per read, on the calling tool's scope and HL budget.
/// A client that could not be built (a bad `HL_API_URL`) fails every read
/// with that error — the gate then denies `missing:book`.
// Constructed by the exec tools (`risk-paper-tools`).
#[allow(dead_code)]
pub(crate) struct HlBookSource {
    pub hl: std::result::Result<HlInfo, ReadError>,
    pub store: Option<Arc<dyn ObservationStore>>,
}

#[allow(dead_code)]
impl HlBookSource {
    /// The source for one tool call (`HlInfo::from_ctx`).
    pub(crate) fn for_call(ctx: &ToolCtx<'_>, store: Option<Arc<dyn ObservationStore>>) -> Self {
        Self {
            hl: HlInfo::from_ctx(ctx).map_err(|e| read_error("book", &e)),
            store,
        }
    }
}

#[async_trait]
impl BookSource for HlBookSource {
    async fn fresh_book(&self, id: &InstrumentId) -> std::result::Result<BookRead, ReadError> {
        let hl = self.hl.as_ref().map_err(Clone::clone)?;
        let (obs, book) = fresh_book(hl, self.store.as_deref(), id, now_ms()).await?;
        Ok(BookRead {
            book,
            observed_at_ms: obs.observed_at_ms,
        })
    }
}

/// A cached row re-derived for `req`'s notionals; `None` when it lacks the
/// trade `req` asks for, does not parse or holds an invalid book (read
/// live). A trades error stays only when `req` asks for trades.
fn rederive(row: &Observation, req: &BookRequest) -> Option<(Observation, Option<L2Book>)> {
    let mut b: HlBook = row.typed().ok()?;
    if req.include_trades && b.last.is_none() {
        return None;
    }
    if let Some(book) = &b.book {
        book.validate().ok()?;
    }
    if !req.include_trades {
        b.errors.retain(|e| e.field != LAST_FIELD);
    }
    b.notional_usd = req.notional_usd.clone();
    let source = match row.source {
        ObsSource::Stream => ObsSource::Stream,
        _ => ObsSource::Cache,
    };
    let obs = Observation::of(names::HL_BOOK, &b, row.observed_at_ms, row.ttl_ms, source);
    Some((obs, b.book))
}

async fn read_live(hl: &HlInfo, req: &BookRequest, now_ms: i64) -> HlBook {
    let coin = req.id.native();
    let book_body = json!({"type": "l2Book", "coin": coin});
    let trades_body = json!({"type": "recentTrades", "coin": coin});
    let started = Instant::now();
    let (book_reply, trades_reply) = if req.include_trades {
        let (b, t) = tokio::join!(hl.post(&book_body), hl.post(&trades_body));
        (b, Some(t))
    } else {
        (hl.post(&book_body).await, None)
    };
    let read_ms = now_ms.saturating_add(started.elapsed().as_millis() as i64);
    let (id, notionals) = (req.id.clone(), req.notional_usd.clone());
    let mut row = match book_reply {
        Ok(InfoReply::Json(v)) => match decode_l2_book(coin, &v) {
            Ok(book) => HlBook::of(id, read_ms, book, notionals),
            Err(e) => HlBook::failed(id, read_ms, notionals, e),
        },
        Ok(InfoReply::Null) => HlBook::not_found(id, read_ms, notionals),
        Err(e) => {
            let e = read_error("book", &e);
            if e.class == ErrorClass::NotApplicable {
                HlBook::not_found(id, read_ms, notionals)
            } else {
                HlBook::failed(id, read_ms, notionals, e)
            }
        }
    };
    if let (Some(t), Some(_)) = (trades_reply, &row.book) {
        match t {
            Ok(InfoReply::Json(v)) => match decode_last_trade(&v) {
                Ok(last) => row.last = last,
                Err(e) => row.errors.push(e),
            },
            Ok(InfoReply::Null) => {}
            Err(e) => {
                let e = read_error(LAST_FIELD, &e);
                row.errors.push(ReadError {
                    message: format!("recentTrades: {}", e.message),
                    ..e
                });
            }
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::hyperliquid::test_support::{
        count, hl, route, serve_info,
    };
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::observe::tests::MemStore;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::hl::book::tests::{GOLDENS, L2_FLX_TSLA, L2_TSLA, READ_MS, TRADES_TSLA};
    use crate::domain::observation::{assert_features_ok, ObsStatus, MAX_LINE1_CHARS};
    use crate::domain::scope::ToolScope;

    const TSLA: &str = "hyperliquid:xyz:TSLA";

    fn tsla_route() -> crate::adapters::outbound::tools::hyperliquid::test_support::Route {
        route(json!({"type": "l2Book", "coin": "xyz:TSLA"}), 200, L2_TSLA)
    }

    fn req(v: Value) -> BookRequest {
        BookRequest::from_args(&v).unwrap()
    }

    fn num(o: &Observation, k: &str) -> f64 {
        o.features[k]
            .as_f64()
            .unwrap_or_else(|| panic!("{k} missing in {:?}", o.features))
    }

    fn golden(i: usize) -> f64 {
        let g: Value = serde_json::from_str(GOLDENS).unwrap();
        g["walks"][i]["slippage_bps_vs_mid"].as_f64().unwrap()
    }

    #[tokio::test]
    async fn reads_the_book_stores_it_then_rederives_from_the_cache() {
        let (base, seen) = serve_info(vec![tsla_route()]).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let (o, book) = read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA"})),
            READ_MS,
        )
        .await;
        assert_eq!(o.key, format!("hl_book/1:{TSLA}"));
        assert_eq!(
            (o.status, o.source, o.ttl_ms),
            (ObsStatus::Ok, ObsSource::Live, BOOK_TTL_MS)
        );
        assert_eq!(book.unwrap(), tsla_book(), "the replay fixture's book");
        assert_features_ok(&o.features);
        let line1 = o.render_text(READ_MS);
        let line1 = line1.lines().next().unwrap();
        assert!(
            line1.contains(TSLA) && line1.chars().count() <= MAX_LINE1_CHARS,
            "{line1}"
        );
        // Default notionals [100, 1000, 10000]; $1 000 / $10 000 buys = goldens.
        assert_eq!(num(&o, "notional_usd_1"), 100.0);
        assert!((num(&o, "buy_slip_bps_2") - golden(0)).abs() < 1e-9);
        assert!((num(&o, "buy_slip_bps_3") - golden(1)).abs() < 1e-9);
        assert!(store.get(&o.key).await.unwrap().is_some());
        assert_eq!(store.1.lock().unwrap().len(), 1, "recorded once");

        // Within 2 s: other notionals re-walk the cached levels, no request.
        let (c, cbook) = read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA", "notional_usd": [1000, 10000, 50000]})),
            READ_MS + 1_500,
        )
        .await;
        assert_eq!((c.source, c.observed_at_ms), (ObsSource::Cache, READ_MS));
        assert!(
            (num(&c, "sell_slip_bps_3") - golden(4)).abs() < 1e-9,
            "sell $50 000"
        );
        assert_eq!(num(&c, "notional_usd_3"), 50_000.0);
        assert_eq!(cbook.unwrap(), tsla_book());
        assert_eq!(count(&seen, "l2Book"), 1);
        // The stored row still carries the writer's notionals.
        let stored = store.get(&o.key).await.unwrap().unwrap();
        assert_eq!(stored.features["notional_usd_3"], 10_000.0);
        // max_age_secs = 0 and the fill engine's fresh read go live.
        let (l, _) = read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA", "max_age_secs": 0})),
            READ_MS + 1_500,
        )
        .await;
        assert_eq!(l.source, ObsSource::Live);
        let id = InstrumentId::parse(TSLA).unwrap();
        let (f, fbook) = fresh_book(&hl, Some(&store), &id, READ_MS + 1_600)
            .await
            .unwrap();
        assert_eq!((f.source, f.status), (ObsSource::Live, ObsStatus::Ok));
        assert_eq!(fbook, tsla_book());
        assert_eq!(count(&seen, "l2Book"), 3);
        // The fill engine's row replaced the cached one, default notionals.
        let stored = store.get(&o.key).await.unwrap().unwrap();
        assert_eq!(stored.observed_at_ms, READ_MS + 1_600);
        assert_eq!(stored.features["notional_usd_3"], 10_000.0);
        assert!(stored.features.contains_key("sell_slip_bps_3"));
    }

    #[tokio::test]
    async fn an_invalid_cached_book_is_read_again() {
        let (base, seen) = serve_info(vec![tsla_route()]).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let plain = req(json!({"coin": "xyz:TSLA"}));
        let (o, _) = read_book(&hl, Some(&store), &plain, READ_MS).await;
        // A crossed book in the cache (never built by `L2Book::new`).
        let mut bad = o.clone();
        bad.data["book"]["bids"][0]["px"] = json!(999.0);
        store.put(&bad).await.unwrap();
        let (again, book) = read_book(&hl, Some(&store), &plain, READ_MS + 500).await;
        assert_eq!(
            (again.source, again.status),
            (ObsSource::Live, ObsStatus::Ok)
        );
        assert_eq!(book.unwrap(), tsla_book());
        assert_eq!(count(&seen, "l2Book"), 2);
    }

    #[tokio::test]
    async fn fresh_book_reads_live_and_maps_no_book_to_read_errors() {
        let (base, seen) = serve_info(vec![
            tsla_route(),
            route(json!({"type": "l2Book", "coin": "xyz:NOPE"}), 200, "null"),
            route(
                json!({"type": "l2Book", "coin": "flx:TSLA"}),
                200,
                L2_FLX_TSLA,
            ),
            route(
                json!({"type": "l2Book", "coin": "para:STX"}),
                502,
                "bad gateway",
            ),
        ])
        .await;
        let hl = hl(&base);
        let store = MemStore::default();
        let id = |s: &str| InstrumentId::parse(s).unwrap();
        // Never the cache: two reads within the TTL are two requests.
        for t in [READ_MS, READ_MS + 500] {
            let (o, book) = fresh_book(&hl, Some(&store), &id(TSLA), t).await.unwrap();
            assert_eq!((o.source, o.observed_at_ms), (ObsSource::Live, t));
            assert_eq!(book, tsla_book());
        }
        assert_eq!(count(&seen, "l2Book"), 2);
        // An empty book is a book (the engine refuses the fill), row absent.
        let (e, book) = fresh_book(&hl, Some(&store), &id("hyperliquid:flx:TSLA"), READ_MS)
            .await
            .unwrap();
        assert!(book.is_empty());
        assert_eq!(e.status, ObsStatus::Absent);
        // No book: HL does not know the coin, the read failed, not HL.
        let err = |s: &'static str| {
            let (hl, store) = (&hl, &store);
            async move {
                fresh_book(hl, Some(store), &id(s), READ_MS)
                    .await
                    .unwrap_err()
            }
        };
        let nope = err("hyperliquid:xyz:NOPE").await;
        assert_eq!(nope.class, ErrorClass::NotApplicable);
        assert!(
            nope.message.contains("hyperliquid:xyz:NOPE"),
            "{}",
            nope.message
        );
        assert_eq!(
            err("hyperliquid:para:STX").await.class,
            ErrorClass::Transient
        );
        let cex = err("binance-usdm:TSLAUSDT").await;
        assert_eq!(cex.class, ErrorClass::NotApplicable);
        assert!(
            cex.message.contains("binance-usdm:TSLAUSDT"),
            "{}",
            cex.message
        );
        assert_eq!(count(&seen, "l2Book"), 5, "nothing sent for a CEX id");
        // Every live read was recorded (the failed one too, never stored).
        assert_eq!(store.1.lock().unwrap().len(), 5);
        assert!(store
            .get("hl_book/1:hyperliquid:para:STX")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn trades_are_read_with_the_book_when_asked() {
        let (base, seen) = serve_info(vec![
            tsla_route(),
            route(
                json!({"type": "recentTrades", "coin": "xyz:TSLA"}),
                200,
                TRADES_TSLA,
            ),
        ])
        .await;
        let hl = hl(&base);
        let store = MemStore::default();
        // A cached row without trades does not answer include_trades.
        read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA"})),
            READ_MS,
        )
        .await;
        let newest = 1_790_779_608_796i64;
        let (o, _) = read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA", "include_trades": true})),
            newest + 3_000,
        )
        .await;
        assert_eq!((o.source, o.status), (ObsSource::Live, ObsStatus::Ok));
        assert_eq!(num(&o, "last"), 349.77);
        let age = num(&o, "last_age_s");
        assert!((3.0..5.0).contains(&age), "{age}");
        assert!(o.headline.ends_with(" last=349.77"), "{}", o.headline);
        assert_eq!(
            (count(&seen, "l2Book"), count(&seen, "recentTrades")),
            (2, 1)
        );
        // Now the cached row has a trade: served from the cache.
        let (c, _) = read_book(
            &hl,
            Some(&store),
            &req(json!({"coin": "xyz:TSLA", "include_trades": true})),
            newest + 4_000,
        )
        .await;
        assert_eq!(c.source, ObsSource::Cache);
        assert_eq!(count(&seen, "recentTrades"), 1);
    }

    #[tokio::test]
    async fn hl_replies_map_to_rows_never_zero() {
        let crossed = json!({"coin": "xyz:BAD", "time": 1, "levels": [
            [{"px": "10.0", "sz": "1", "n": 1}], [{"px": "9.0", "sz": "1", "n": 1}]]});
        let (base, _) = serve_info(vec![
            tsla_route(),
            route(json!({"type": "l2Book", "coin": "xyz:NOPE"}), 200, "null"),
            route(json!({"type": "l2Book", "coin": "nope:TSLA"}), 500, "null"),
            route(
                json!({"type": "l2Book", "coin": "flx:TSLA"}),
                200,
                L2_FLX_TSLA,
            ),
            route(
                json!({"type": "l2Book", "coin": "para:STX"}),
                502,
                "bad gateway",
            ),
            route(
                json!({"type": "l2Book", "coin": "xyz:BAD"}),
                200,
                crossed.to_string(),
            ),
            route(
                json!({"type": "recentTrades", "coin": "xyz:TSLA"}),
                429,
                "slow down",
            ),
        ])
        .await;
        let hl = hl(&base);
        let store = MemStore::default();
        let read = |coin: &str, trades: bool| {
            let (hl, store) = (&hl, &store);
            let r = req(json!({"coin": coin, "include_trades": trades, "max_age_secs": 0}));
            async move { read_book(hl, Some(store), &r, READ_MS).await }
        };
        for coin in ["xyz:NOPE", "nope:TSLA"] {
            let (o, book) = read(coin, false).await;
            assert_eq!((o.status, book), (ObsStatus::Absent, None), "{coin}");
            assert!(
                o.headline.ends_with("not listed by the venue"),
                "{}",
                o.headline
            );
        }
        let (e, book) = read("flx:TSLA", false).await;
        assert_eq!(e.status, ObsStatus::Absent);
        assert_eq!(e.features["book_empty"], true);
        assert!(book.unwrap().is_empty());
        assert!(!e.features.contains_key("mid"), "never 0");
        for (coin, class) in [
            ("para:STX", ErrorClass::Transient),
            ("xyz:BAD", ErrorClass::Decode),
        ] {
            let (o, book) = read(coin, false).await;
            assert_eq!((o.status, book), (ObsStatus::Error, None), "{coin}");
            assert_eq!(o.errors[0].class, class, "{coin}");
            assert!(o.features.is_empty(), "{coin}: no numbers");
            assert!(
                store.get(&o.key).await.unwrap().is_none(),
                "{coin}: never stored"
            );
        }
        assert!(store
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|o| o.key == "hl_book/1:hyperliquid:para:STX" && o.status == ObsStatus::Error));
        // The book is fine, the trades read is rate limited: partial.
        let (p, book) = read("xyz:TSLA", true).await;
        assert_eq!(p.status, ObsStatus::Partial);
        assert!(book.is_some());
        assert_eq!(p.errors[0].field, "last");
        assert_eq!(p.errors[0].class, ErrorClass::RateLimited);
        assert!(
            p.errors[0].message.starts_with("recentTrades: HTTP 429"),
            "{:?}",
            p.errors
        );
        assert!(!p.features.contains_key("last"));
        // Trades not asked for: the cached row answers without that error.
        let plain = req(json!({"coin": "xyz:TSLA"}));
        let (c, _) = read_book(&hl, Some(&store), &plain, READ_MS + 1_000).await;
        assert_eq!((c.source, c.status), (ObsSource::Cache, ObsStatus::Ok));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
    }

    #[test]
    fn args_parse_strictly() {
        let r = req(json!({"coin": " xyz:TSLA "}));
        assert_eq!(r.id.to_string(), TSLA);
        assert_eq!(r.notional_usd, DEFAULT_NOTIONALS_USD.to_vec());
        assert!(!r.include_trades && r.max_age_secs.is_none());
        let r = req(
            json!({"coin": "ETH", "notional_usd": [], "include_trades": true, "max_age_secs": 0}),
        );
        assert_eq!(
            (r.notional_usd.len(), r.include_trades, r.max_age_secs),
            (0, true, Some(0))
        );
        assert_eq!(
            req(json!({"coin": "ETH", "notional_usd": 2500})).notional_usd,
            [2_500.0]
        );
        let err = |v: Value| BookRequest::from_args(&v).unwrap_err().to_string();
        assert!(err(json!({})).contains("'coin' is required"));
        assert!(err(json!({"coin": "xyz TSLA"})).contains("hyperliquid:xyz TSLA"));
        assert!(err(json!({"coin": "ETH", "notional_usd": [1, 2, 3, 4]})).contains("at most 3"));
        assert!(err(json!({"coin": "ETH", "notional_usd": [0]})).contains("> 0"));
        assert!(err(json!({"coin": "ETH", "notional_usd": ["100"]})).contains("> 0"));
        assert!(err(json!({"coin": "ETH", "include_trades": "yes"})).contains("boolean"));
        assert!(err(json!({"coin": "ETH", "max_age_secs": 1.5})).contains("max_age_secs"));
    }

    #[tokio::test]
    async fn execute_gates_scope_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let tool = HlBookTool::new(HlShared::default());
        assert_eq!(tool.definition().name, names::HL_BOOK);
        let h = TestHarness::with_scope(dir.path(), ToolScope::default());
        assert!(tool
            .execute(&json!({"coin": "ETH"}), &h.ctx())
            .await
            .is_err());
        let h = TestHarness::new(dir.path());
        assert!(tool.execute(&json!({}), &h.ctx()).await.is_err());
        let o = tool
            .execute(&json!({"coin": "ETH"}), &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        assert_eq!(
            (o.key.as_str(), o.status),
            ("hl_book/1:hyperliquid:ETH", ObsStatus::Error)
        );
        assert_eq!(o.errors[0].class, ErrorClass::Fatal);
        assert!(o.errors[0].message.contains("net_hosts"), "{:?}", o.errors);
    }

    // ── live (mainnet; `cargo test --bin tengu live_hl -- --ignored --test-threads 1`) ──

    #[tokio::test]
    #[ignore]
    async fn live_hl_book_xyz_tsla() {
        use crate::adapters::outbound::egress;
        use crate::adapters::outbound::observations::SqliteObservationStore;

        let dir = tempfile::tempdir().unwrap();
        let scope = ToolScope {
            fs_roots: vec![dir.path().to_path_buf()],
            net_hosts: vec!["api.hyperliquid.xyz".into()],
            ..Default::default()
        };
        let mut h = TestHarness::with_scope(dir.path(), scope);
        h.http = egress::policy()
            .tool_client(std::time::Duration::from_secs(15))
            .unwrap();
        let tool = HlBookTool::new(HlShared {
            store: Some(Arc::new(SqliteObservationStore::open(dir.path()).unwrap())),
            fees: Default::default(),
        });
        let args = json!({"coin": "xyz:TSLA", "include_trades": true});
        let o = tool
            .execute(&args, &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        println!(
            "{}",
            o.render_text(o.observed_at_ms)
                .lines()
                .take(2)
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        assert_features_ok(&o.features);
        for k in [
            "bid",
            "ask",
            "spread_bps",
            "depth_usd_10bps_bid",
            "buy_slip_bps_1",
            "last",
        ] {
            assert!(o.features.contains_key(k), "{k}");
        }
        let b: HlBook = o.typed().unwrap();
        assert!(b.book.unwrap().bids.len() > 5);
        let c = tool
            .execute(&args, &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        assert_eq!(c.source, ObsSource::Cache);
        let e = tool
            .execute(&json!({"coin": "flx:TSLA"}), &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        println!(
            "{}",
            e.render_text(e.observed_at_ms).lines().next().unwrap()
        );
        assert_eq!(e.status, ObsStatus::Absent);
    }
}
