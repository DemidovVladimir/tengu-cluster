//! `hl_ctx` — Hyperliquid market context for a whole perp dex or ≤ 64 coins
//! (row rules: `domain/hl/ctx.rs`).
//!
//! | Step | Rule |
//! |---|---|
//! | Args | `coins` (1–64 HL names, verbatim) or `dex` (`""` / `"default"` = the default dex); `dex = ""` next to `coins` is ignored; `#…` outcome coins are refused |
//! | Cache | coins: each `mkt_ctx/1:hyperliquid:<coin>` row fresh (≤ min(5 s, `max_age_secs`)) is served as is; dex: the `hl_sweep/1:hyperliquid:<dex label>` row (5 s) |
//! | Reads | one `metaAndAssetCtxs {dex}` per needed perp dex (concurrent), `spotMetaAndAssetCtxs` for spot coins — 20 weight each; with it `hl_at_oi_cap/1` (60 s) per perp dex and, for HIP-3 dexes, `hl_perp_meta/1` (1 h) through the cache |
//! | Writes | every coin of each reply: `mkt_ctx/1` (5 s) + `mkt_instrument/1` (60 s), recorded like `observe()`; a requested coin the reply lacks ⇒ `not_found` rows (absent); a failed read ⇒ `error` rows (recorded, never stored) |
//! | Answer | one coin ⇒ its `mkt_ctx/1` row; a dex or several coins ⇒ `hl_sweep/1` (per-coin summary in `data`) |
//! | HL errors | `500 null` (unknown dex) ⇒ the dex's coins are absent (`not_applicable` in `errors`); anything else ⇒ `error` rows with the class (`outbound/hyperliquid/info.rs`) |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use super::{defs, opt_u64_arg, HlShared};
use crate::adapters::outbound::http_class::read_error;
use crate::adapters::outbound::hyperliquid::info::{request_weight, HlInfo, InfoReply};
use crate::application::observe::observe;
use crate::domain::hl::ctx::{
    decode_categories, decode_coin_list, decode_perp_dexs, perp_rows, spot_rows, CoinRows,
    HlAtOiCap, HlPerpMeta, HlSweep, PerpFacts, SweepCoin, SweepRead, SweepScope, AT_OI_CAP_TTL_MS,
    CTX_TTL_MS, INSTRUMENT_TTL_MS, MAX_COINS, PERP_META_TTL_MS,
};
use crate::domain::hl::{classify, dex_label, parse_dex, CoinKind, FeeBasis};
use crate::domain::market::{InstrumentId, InstrumentKind, Listing, MarketCtx, MarketInstrument};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, CachePolicy, ErrorClass, Field, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::tools as names;
use crate::domain::xm::cost::{HlKind, HlUserRates};
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) fn tools(shared: &HlShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(HlCtxTool::new(shared.clone()))]
}

/// What an `hl_ctx` call asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CtxTarget {
    /// Every coin of one perp dex (`""` = the default dex).
    Dex(String),
    /// HL coin names, verbatim, de-duplicated, request order.
    Coins(Vec<String>),
}

impl CtxTarget {
    /// Parse the `coins` / `dex` arguments (module table).
    pub(crate) fn from_args(args: &Value) -> Result<Self> {
        let tool = names::HL_CTX;
        let dex = match args.get("dex") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(parse_dex(s).map_err(|e| anyhow!("{tool}: {e}"))?),
            Some(v) => bail!("{tool}: 'dex' must be a string, got {v}"),
        };
        let coins = match args.get("coins") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut out: Vec<String> = Vec::new();
                for v in items {
                    let c = v
                        .as_str()
                        .ok_or_else(|| anyhow!("{tool}: 'coins' must be an array of strings"))?
                        .trim();
                    InstrumentId::hyperliquid(c).map_err(|e| anyhow!("{tool}: {e}"))?;
                    if classify(c) == CoinKind::Outcome {
                        bail!("{tool}: outcome coin `{c}` is not supported (perps and spot only)");
                    }
                    if !out.iter().any(|o| o == c) {
                        out.push(c.to_string());
                    }
                }
                out
            }
            Some(v) => bail!("{tool}: 'coins' must be an array of strings, got {v}"),
        };
        if coins.len() > MAX_COINS {
            bail!(
                "{tool}: {} coins given, at most {MAX_COINS} per call",
                coins.len()
            );
        }
        match (dex, coins.is_empty()) {
            (Some(d), false) if !d.is_empty() => {
                bail!("{tool}: give coins or dex `{d}`, not both")
            }
            (_, false) => Ok(Self::Coins(coins)),
            (Some(d), true) => Ok(Self::Dex(d)),
            (None, true) => bail!("{tool}: give 'coins' (1-{MAX_COINS} HL coin names) or 'dex'"),
        }
    }
}

pub(crate) struct HlCtxTool {
    def: ToolDef,
    shared: HlShared,
}

impl HlCtxTool {
    pub(crate) fn new(shared: HlShared) -> Self {
        Self {
            def: defs::def(names::HL_CTX),
            shared,
        }
    }
}

#[async_trait]
impl Tool for HlCtxTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let target = CtxTarget::from_args(args)?;
        let max_age_secs = opt_u64_arg(args, names::HL_CTX, "max_age_secs")?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let obs = match HlInfo::from_ctx(ctx) {
            Ok(hl) => hl_ctx_obs(&hl, store, &target, max_age_secs, self.shared.fees, now).await,
            Err(e) => failed(&target, read_error("hl", &e), now),
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

/// `max_age_ms = min(ttl, max_age_secs)` as in `CachePolicy::new`.
fn policy(schema: &str, subject: &str, ttl_ms: u64, max_age_secs: Option<u64>) -> CachePolicy {
    CachePolicy {
        key: Observation::key_for(schema, subject),
        ttl_ms,
        max_age_ms: max_age_secs.map_or(ttl_ms, |s| ttl_ms.min(s.saturating_mul(1000))),
    }
}

/// Every requested row `error` (no HL client: a bad `$HL_API_URL`).
fn failed(target: &CtxTarget, error: ReadError, now_ms: i64) -> Observation {
    let coins: Vec<String> = match target {
        CtxTarget::Dex(_) => Vec::new(),
        CtxTarget::Coins(c) => c.clone(),
    };
    let rows: Vec<MarketCtx> = coins
        .iter()
        .filter_map(|c| InstrumentId::hyperliquid(c).ok())
        .map(|id| MarketCtx::failed(id, now_ms, error.clone()))
        .collect();
    if let [one] = rows.as_slice() {
        return Observation::of(names::HL_CTX, one, now_ms, CTX_TTL_MS, ObsSource::Live);
    }
    let sweep = HlSweep {
        scope: scope_of(target),
        reads: Vec::new(),
        rows_written: 0,
        coins: rows.iter().map(SweepCoin::of).collect(),
        errors: vec![error],
    };
    Observation::of(names::HL_CTX, &sweep, now_ms, 0, ObsSource::Live)
}

fn scope_of(target: &CtxTarget) -> SweepScope {
    match target {
        CtxTarget::Dex(dex) => SweepScope::Dex { dex: dex.clone() },
        CtxTarget::Coins(coins) => SweepScope::Coins {
            coins: coins.clone(),
        },
    }
}

/// `hl_ctx` over `hl` + the cache (module table).
pub(crate) async fn hl_ctx_obs(
    hl: &HlInfo,
    store: Option<&dyn ObservationStore>,
    target: &CtxTarget,
    max_age_secs: Option<u64>,
    fees: FeeBasis,
    now_ms: i64,
) -> Observation {
    let reader = Reader {
        hl,
        store,
        perp_rates: fees.user_rates(HlKind::Perp).ok(),
        spot_rates: fees.user_rates(HlKind::Spot).ok(),
        now_ms,
    };
    match target {
        CtxTarget::Dex(dex) => {
            let scope = scope_of(target);
            let policy = policy(
                HlSweep::SCHEMA,
                &HlSweep::subject_of(&scope),
                CTX_TTL_MS,
                max_age_secs,
            );
            let group = CoinKind::Perp { dex: dex.clone() };
            let fetched = observe(store, names::HL_CTX, &policy, now_ms, || async {
                let mut run = reader.read(std::slice::from_ref(&group)).await;
                let out = run.groups.remove(&group).unwrap_or_default();
                let sweep = HlSweep {
                    scope,
                    reads: run.reads,
                    rows_written: run.written,
                    coins: out.rows.iter().map(|r| SweepCoin::of(&r.ctx)).collect(),
                    errors: run.errors,
                };
                Ok((sweep, CTX_TTL_MS))
            })
            .await;
            fetched.unwrap_or_else(|e| failed(target, read_error("ctx", &e), now_ms))
        }
        CtxTarget::Coins(coins) => reader.coins(coins, max_age_secs).await,
    }
}

/// One `hl_ctx` call's reads.
struct Reader<'a> {
    hl: &'a HlInfo,
    store: Option<&'a dyn ObservationStore>,
    perp_rates: Option<HlUserRates>,
    spot_rates: Option<HlUserRates>,
    now_ms: i64,
}

/// A group's decoded rows, or why there are none.
#[derive(Default)]
struct GroupRows {
    rows: Vec<CoinRows>,
    /// The ctx read failed: every requested coin of the group is `error`.
    failure: Option<ReadError>,
}

/// What [`Reader::read`] did.
struct Run {
    groups: BTreeMap<CoinKind, GroupRows>,
    reads: Vec<SweepRead>,
    errors: Vec<ReadError>,
    written: usize,
}

fn sweep_read(info: &str, dex: Option<&str>, failed: Option<ErrorClass>) -> SweepRead {
    SweepRead {
        info: info.to_string(),
        dex: dex.map(|d| dex_label(d).to_string()),
        weight: request_weight(info),
        failed,
    }
}

fn class_of(e: &anyhow::Error) -> ErrorClass {
    read_error("ctx", e).class
}

/// A side-read failure as the instrument-row error it causes.
fn side_error(field: &str, source: &str, e: &ReadError) -> ReadError {
    ReadError {
        field: field.to_string(),
        class: e.class,
        message: format!("{source}: {}", e.message),
        retry_after_ms: e.retry_after_ms,
    }
}

impl Reader<'_> {
    /// Read each group (concurrently), write every row, collect reads.
    async fn read(&self, groups: &[CoinKind]) -> Run {
        let mut run = Run {
            groups: BTreeMap::new(),
            reads: Vec::new(),
            errors: Vec::new(),
            written: 0,
        };
        let builder = groups
            .iter()
            .any(|g| matches!(g, CoinKind::Perp { dex } if !dex.is_empty()));
        let meta = if builder {
            let (meta, reads) = self.perp_meta().await;
            run.reads.extend(reads);
            Some(meta)
        } else {
            None
        };
        let reads = groups.iter().map(|g| self.read_group(g, meta.as_ref()));
        for (g, (rows, reads, errors)) in groups.iter().zip(futures::future::join_all(reads).await)
        {
            run.reads.extend(reads);
            run.errors.extend(errors);
            for r in &rows.rows {
                run.written += self.write_rows(r).await;
            }
            run.groups.insert(g.clone(), rows);
        }
        run
    }

    async fn read_group(
        &self,
        group: &CoinKind,
        meta: Option<&HlPerpMeta>,
    ) -> (GroupRows, Vec<SweepRead>, Vec<ReadError>) {
        match group {
            CoinKind::Perp { dex } => self.read_perp(dex, meta).await,
            CoinKind::Spot => self.read_spot().await,
            CoinKind::Outcome => (GroupRows::default(), Vec::new(), Vec::new()),
        }
    }

    /// `metaAndAssetCtxs {dex}` + the dex's at-cap row.
    async fn read_perp(
        &self,
        dex: &str,
        meta: Option<&HlPerpMeta>,
    ) -> (GroupRows, Vec<SweepRead>, Vec<ReadError>) {
        let body = if dex.is_empty() {
            json!({"type": "metaAndAssetCtxs"})
        } else {
            json!({"type": "metaAndAssetCtxs", "dex": dex})
        };
        let (reply, (cap, cap_reads)) = tokio::join!(self.hl.post(&body), self.at_cap(dex));
        let mut reads = vec![sweep_read(
            "metaAndAssetCtxs",
            Some(dex),
            reply.as_ref().err().map(class_of),
        )];
        reads.extend(cap_reads);
        let mut errors = Vec::new();
        let mut facts = PerpFacts {
            meta: meta.filter(|_| !dex.is_empty()),
            at_cap: cap.coins.value().map(Vec::as_slice),
            rates: self.perp_rates,
            errors: Vec::new(),
        };
        if let Some(e) = cap.coins.error() {
            facts
                .errors
                .push(side_error("at_oi_cap", "perpsAtOpenInterestCap", e));
        }
        if let Some(m) = facts.meta {
            if let Some(e) = m.dexes.error() {
                facts.errors.push(side_error(
                    "oi_cap_usd",
                    "perpDexs (asset_id and oi_cap_usd unknown)",
                    e,
                ));
            }
            if let Some(e) = m.categories.error() {
                facts
                    .errors
                    .push(side_error("category", "perpCategories", e));
            }
        }
        errors.extend(facts.errors.iter().cloned());
        let label = dex_label(dex);
        let rows = match reply {
            Ok(InfoReply::Json(v)) => match perp_rows(dex, &v, &facts, self.now_ms) {
                Ok(r) => {
                    errors.extend(r.skipped);
                    GroupRows {
                        rows: r.rows,
                        failure: None,
                    }
                }
                Err(e) => failure(&mut errors, e),
            },
            Ok(InfoReply::Null) => {
                errors.push(unknown_dex(label, "200 null"));
                GroupRows::default()
            }
            Err(e) => {
                let e = read_error("ctx", &e);
                if e.class == ErrorClass::NotApplicable {
                    errors.push(unknown_dex(label, &e.message));
                    GroupRows::default()
                } else {
                    failure(&mut errors, e)
                }
            }
        };
        (rows, reads, errors)
    }

    async fn read_spot(&self) -> (GroupRows, Vec<SweepRead>, Vec<ReadError>) {
        let reply = self.hl.post(&json!({"type": "spotMetaAndAssetCtxs"})).await;
        let reads = vec![sweep_read(
            "spotMetaAndAssetCtxs",
            None,
            reply.as_ref().err().map(class_of),
        )];
        let mut errors = Vec::new();
        let rows = match reply {
            Ok(InfoReply::Json(v)) => match spot_rows(&v, self.spot_rates, self.now_ms) {
                Ok(r) => {
                    errors.extend(r.skipped);
                    GroupRows {
                        rows: r.rows,
                        failure: None,
                    }
                }
                Err(e) => failure(&mut errors, e),
            },
            Ok(InfoReply::Null) => failure(
                &mut errors,
                ReadError::new(
                    "ctx",
                    ErrorClass::Decode,
                    "spotMetaAndAssetCtxs returned null",
                ),
            ),
            Err(e) => failure(&mut errors, read_error("ctx", &e)),
        };
        (rows, reads, errors)
    }

    /// `hl_perp_meta/1:hyperliquid` through the cache (both sources
    /// concurrently on a miss).
    async fn perp_meta(&self) -> (HlPerpMeta, Vec<SweepRead>) {
        let policy = policy(HlPerpMeta::SCHEMA, "hyperliquid", PERP_META_TTL_MS, None);
        let fetched = observe(self.store, names::HL_CTX, &policy, self.now_ms, || async {
            let (dexes_body, cats_body) = (
                json!({"type": "perpDexs"}),
                json!({"type": "perpCategories"}),
            );
            let (dexes, cats) = tokio::join!(self.hl.post(&dexes_body), self.hl.post(&cats_body));
            let meta = HlPerpMeta {
                dexes: decoded(dexes, "dexes", decode_perp_dexs),
                categories: decoded(cats, "categories", decode_categories),
            };
            let ttl = meta.ttl_ms();
            Ok((meta, ttl))
        })
        .await;
        match fetched.and_then(|o| Ok((o.typed::<HlPerpMeta>()?, o.source))) {
            Ok((meta, source)) => {
                let reads = if source == ObsSource::Live {
                    vec![
                        sweep_read("perpDexs", None, meta.dexes.error().map(|e| e.class)),
                        sweep_read(
                            "perpCategories",
                            None,
                            meta.categories.error().map(|e| e.class),
                        ),
                    ]
                } else {
                    Vec::new()
                };
                (meta, reads)
            }
            Err(e) => {
                let e = read_error("meta", &e);
                let meta = HlPerpMeta {
                    dexes: Field::err(e.clone()),
                    categories: Field::err(e),
                };
                (meta, Vec::new())
            }
        }
    }

    /// `hl_at_oi_cap/1:hyperliquid:<dex label>` through the cache.
    async fn at_cap(&self, dex: &str) -> (HlAtOiCap, Vec<SweepRead>) {
        let subject = format!("hyperliquid:{}", dex_label(dex));
        let policy = policy(HlAtOiCap::SCHEMA, &subject, AT_OI_CAP_TTL_MS, None);
        let body = if dex.is_empty() {
            json!({"type": "perpsAtOpenInterestCap"})
        } else {
            json!({"type": "perpsAtOpenInterestCap", "dex": dex})
        };
        let fetched = observe(self.store, names::HL_CTX, &policy, self.now_ms, || async {
            let row = HlAtOiCap {
                dex: dex.to_string(),
                coins: decoded(self.hl.post(&body).await, "coins", decode_coin_list),
            };
            Ok((row, AT_OI_CAP_TTL_MS))
        })
        .await;
        match fetched.and_then(|o| Ok((o.typed::<HlAtOiCap>()?, o.source))) {
            Ok((row, source)) => {
                let reads = if source == ObsSource::Live {
                    let failed = row.coins.error().map(|e| e.class);
                    vec![sweep_read("perpsAtOpenInterestCap", Some(dex), failed)]
                } else {
                    Vec::new()
                };
                (row, reads)
            }
            Err(e) => {
                let row = HlAtOiCap {
                    dex: dex.to_string(),
                    coins: Field::err(read_error("coins", &e)),
                };
                (row, Vec::new())
            }
        }
    }

    /// Record + store both rows of a coin as `observe()` does; returns the
    /// number of rows stored.
    async fn write_rows(&self, r: &CoinRows) -> usize {
        let inst = Observation::of(
            names::HL_CTX,
            &r.instrument,
            self.now_ms,
            INSTRUMENT_TTL_MS,
            ObsSource::Live,
        );
        let ctx = Observation::of(
            names::HL_CTX,
            &r.ctx,
            self.now_ms,
            CTX_TTL_MS,
            ObsSource::Live,
        );
        usize::from(store_live(self.store, &inst).await)
            + usize::from(store_live(self.store, &ctx).await)
    }

    /// Coins mode: fresh rows from the cache, one read per group with a
    /// stale coin, then the coin's row (one coin) or an `hl_sweep/1`.
    async fn coins(&self, coins: &[String], max_age_secs: Option<u64>) -> Observation {
        let ids: Vec<InstrumentId> = coins
            .iter()
            .filter_map(|c| InstrumentId::hyperliquid(c).ok())
            .collect();
        let max_age_ms = policy(MarketCtx::SCHEMA, "", CTX_TTL_MS, max_age_secs).max_age_ms;
        let keys: Vec<String> = ids
            .iter()
            .map(|id| Observation::key_for(MarketCtx::SCHEMA, &id.to_string()))
            .collect();
        let cached = match self.store {
            Some(s) => s.get_many(&keys).await.unwrap_or_else(|e| {
                let error = format!("{e:#}");
                warn!(%error, "observation store read failed; reading live");
                vec![None; keys.len()]
            }),
            None => vec![None; keys.len()],
        };
        let fresh: Vec<Option<Observation>> = cached
            .into_iter()
            .map(|o| {
                o.filter(|o| o.is_fresh(self.now_ms, max_age_ms) && o.typed::<MarketCtx>().is_ok())
            })
            .collect();
        let stale: BTreeSet<CoinKind> = ids
            .iter()
            .zip(&fresh)
            .filter(|(_, f)| f.is_none())
            .map(|(id, _)| classify(id.native()))
            .collect();
        let groups: Vec<CoinKind> = stale.into_iter().collect();
        let mut run = if groups.is_empty() {
            Run {
                groups: BTreeMap::new(),
                reads: Vec::new(),
                errors: Vec::new(),
                written: 0,
            }
        } else {
            self.read(&groups).await
        };
        let mut answers = Vec::with_capacity(ids.len());
        for (id, fresh) in ids.iter().zip(fresh) {
            let obs = match fresh {
                Some(o) => o.served_from_cache(),
                None => self.live_row(&mut run, id).await,
            };
            answers.push(obs);
        }
        if let [one] = answers.as_slice() {
            return one.clone();
        }
        let sweep = HlSweep {
            scope: SweepScope::Coins {
                coins: coins.to_vec(),
            },
            coins: answers
                .iter()
                .filter_map(|o| o.typed::<MarketCtx>().ok())
                .map(|c| SweepCoin::of(&c))
                .collect(),
            rows_written: run.written,
            errors: run.errors,
            reads: run.reads,
        };
        let source = if sweep.reads.is_empty() {
            ObsSource::Cache
        } else {
            ObsSource::Live
        };
        Observation::of(names::HL_CTX, &sweep, self.now_ms, 0, source)
    }

    /// The live row of a requested coin after [`Reader::read`]: from its
    /// group's rows; `error` when the group read failed; else `not_found`
    /// (rows written: absent is a real answer).
    async fn live_row(&self, run: &mut Run, id: &InstrumentId) -> Observation {
        let group = classify(id.native());
        let rows = run.groups.get(&group);
        if let Some(r) = rows.and_then(|g| g.rows.iter().find(|r| &r.ctx.id == id)) {
            return Observation::of(
                names::HL_CTX,
                &r.ctx,
                self.now_ms,
                CTX_TTL_MS,
                ObsSource::Live,
            );
        }
        if let Some(e) = rows.and_then(|g| g.failure.clone()) {
            let row = MarketCtx::failed(id.clone(), self.now_ms, e);
            let obs = Observation::of(
                names::HL_CTX,
                &row,
                self.now_ms,
                CTX_TTL_MS,
                ObsSource::Live,
            );
            store_live(self.store, &obs).await;
            return obs;
        }
        let kind = if group == CoinKind::Spot {
            InstrumentKind::Spot
        } else {
            InstrumentKind::Perp
        };
        let rows = CoinRows {
            instrument: MarketInstrument::new(id.clone(), kind, Listing::NotFound),
            ctx: MarketCtx::not_found(id.clone(), self.now_ms),
        };
        run.written += self.write_rows(&rows).await;
        Observation::of(
            names::HL_CTX,
            &rows.ctx,
            self.now_ms,
            CTX_TTL_MS,
            ObsSource::Live,
        )
    }
}

/// The ctx read failed: every requested coin of the group is `error`.
fn failure(errors: &mut Vec<ReadError>, e: ReadError) -> GroupRows {
    errors.push(e.clone());
    GroupRows {
        rows: Vec::new(),
        failure: Some(e),
    }
}

fn unknown_dex(label: &str, why: &str) -> ReadError {
    ReadError::new(
        "ctx",
        ErrorClass::NotApplicable,
        format!("Hyperliquid does not know dex `{label}` ({why})"),
    )
}

/// A side reply decoded into a field: `200 null` or a bad shape ⇒ `decode`.
fn decoded<T>(
    reply: Result<InfoReply>,
    field: &str,
    decode: impl Fn(&Value) -> std::result::Result<T, String>,
) -> Field<T> {
    match reply {
        Ok(InfoReply::Json(v)) => match decode(&v) {
            Ok(x) => Field::ok(x),
            Err(e) => Field::err(ReadError::new(field, ErrorClass::Decode, e)),
        },
        Ok(InfoReply::Null) => {
            Field::err(ReadError::new(field, ErrorClass::Decode, "reply is null"))
        }
        Err(e) => Field::err(read_error(field, &e)),
    }
}

/// Record + store one live row the way `observe()` does (history first;
/// the cache keeps neither `error` nor ttl-0 rows). `true` = stored.
async fn store_live(store: Option<&dyn ObservationStore>, obs: &Observation) -> bool {
    let Some(s) = store else {
        return false;
    };
    if let Err(e) = s.record(obs).await {
        warn!(key = %obs.key, error = %e, "observation history write failed");
    }
    if obs.status == ObsStatus::Error || obs.ttl_ms == 0 {
        return false;
    }
    match s.put(obs).await {
        Ok(stored) => stored,
        Err(e) => {
            warn!(key = %obs.key, error = %e, "observation store write failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::hyperliquid::test_support::{
        count, hl, route, serve_info, Route,
    };
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::observe::tests::MemStore;
    use crate::domain::hl::ctx::tests::{
        AT_CAP_DEFAULT, CATEGORIES, MAC_DEFAULT, MAC_XYZ, NOW, PERP_DEXS, SPOT,
    };
    use crate::domain::market::{Category, QuoteCcy};
    use crate::domain::observation::{assert_features_ok, MAX_FEATURES, MAX_LINE1_CHARS};
    use crate::domain::scope::ToolScope;

    const AT_CAP_XYZ: &str = "[]";

    /// Every reply an `xyz` + default + spot read needs, from the captures.
    fn routes() -> Vec<Route> {
        vec![
            route(
                json!({"type": "metaAndAssetCtxs", "dex": "xyz"}),
                200,
                MAC_XYZ,
            ),
            route(json!({"type": "metaAndAssetCtxs"}), 200, MAC_DEFAULT),
            route(
                json!({"type": "perpsAtOpenInterestCap", "dex": "xyz"}),
                200,
                AT_CAP_XYZ,
            ),
            route(
                json!({"type": "perpsAtOpenInterestCap"}),
                200,
                AT_CAP_DEFAULT,
            ),
            route(json!({"type": "perpDexs"}), 200, PERP_DEXS),
            route(json!({"type": "perpCategories"}), 200, CATEGORIES),
            route(json!({"type": "spotMetaAndAssetCtxs"}), 200, SPOT),
        ]
    }

    fn coins(c: &[&str]) -> CtxTarget {
        CtxTarget::Coins(c.iter().map(|s| s.to_string()).collect())
    }

    fn assert_line1(o: &Observation, ids: &[&str]) {
        let text = o.render_text(o.observed_at_ms);
        let line1 = text.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        for id in ids {
            assert!(line1.contains(id), "{id} missing in {line1}");
        }
        assert!(o.features.len() <= MAX_FEATURES);
        assert_features_ok(&o.features);
    }

    #[tokio::test]
    async fn one_coin_reads_its_dex_writes_every_row_then_caches() {
        let (base, seen) = serve_info(routes()).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let t = coins(&["xyz:TSLA"]);
        let o = hl_ctx_obs(&hl, Some(&store), &t, None, FeeBasis::default(), NOW).await;
        assert_eq!(o.key, "mkt_ctx/1:hyperliquid:xyz:TSLA");
        assert_eq!(
            (o.status, o.source, o.ttl_ms),
            (ObsStatus::Ok, ObsSource::Live, CTX_TTL_MS)
        );
        assert_line1(&o, &["hyperliquid:xyz:TSLA"]);
        let c: MarketCtx = o.typed().unwrap();
        assert_eq!(c.mark, Field::ok(347.19));
        assert_eq!(c.category, Some(Category::Stocks));
        assert_eq!(
            (c.oi_cap_usd, c.at_oi_cap),
            (Some(100_000_000.0), Some(false))
        );
        assert!((c.taker_fee_bps.unwrap() - 0.9).abs() < 1e-12);
        // One ctx read for xyz + the side rows; nothing for other dexes.
        for (t, n) in [
            ("metaAndAssetCtxs", 1),
            ("perpsAtOpenInterestCap", 1),
            ("perpDexs", 1),
            ("perpCategories", 1),
            ("spotMetaAndAssetCtxs", 0),
        ] {
            assert_eq!(count(&seen, t), n, "{t}");
        }
        // Every xyz coin has both rows now; the side rows are cached too.
        for key in [
            "mkt_ctx/1:hyperliquid:xyz:NVDA",
            "mkt_instrument/1:hyperliquid:xyz:TSLA",
            "mkt_ctx/1:hyperliquid:xyz:URANIUM",
            "hl_perp_meta/1:hyperliquid",
            "hl_at_oi_cap/1:hyperliquid:xyz",
        ] {
            assert!(store.get(key).await.unwrap().is_some(), "{key}");
        }
        let inst: MarketInstrument = store
            .get("mkt_instrument/1:hyperliquid:xyz:TSLA")
            .await
            .unwrap()
            .unwrap()
            .typed()
            .unwrap();
        assert_eq!(
            (inst.asset_id, inst.quote_ccy),
            (Some(110_001), Some(QuoteCcy::Usdc))
        );
        // History got every live row (MemStore records).
        assert!(store.1.lock().unwrap().len() >= 2 * 128);

        // Within 5 s: another xyz coin comes from the cache, no request.
        let before = seen.lock().unwrap().len();
        let n = hl_ctx_obs(
            &hl,
            Some(&store),
            &coins(&["xyz:NVDA"]),
            None,
            FeeBasis::default(),
            NOW + 3_000,
        )
        .await;
        assert_eq!((n.source, n.status), (ObsSource::Cache, ObsStatus::Ok));
        assert_eq!(seen.lock().unwrap().len(), before);
        // max_age_secs = 0 forces a live read — the 1 h meta and 60 s cap rows stay cached.
        let l = hl_ctx_obs(
            &hl,
            Some(&store),
            &coins(&["xyz:NVDA"]),
            Some(0),
            FeeBasis::default(),
            NOW + 3_000,
        )
        .await;
        assert_eq!(l.source, ObsSource::Live);
        assert_eq!(count(&seen, "metaAndAssetCtxs"), 2);
        assert_eq!(count(&seen, "perpDexs"), 1);
        assert_eq!(count(&seen, "perpsAtOpenInterestCap"), 1);
    }

    #[tokio::test]
    async fn several_coins_across_dexes_return_a_bounded_summary() {
        let (base, seen) = serve_info(routes()).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let t = coins(&["xyz:TSLA", "ETH", "@151", "xyz:NOPE"]);
        let o = hl_ctx_obs(&hl, Some(&store), &t, None, FeeBasis::default(), NOW).await;
        assert_eq!(o.key, "hl_sweep/1:hyperliquid:@151,ETH,xyz:NOPE,xyz:TSLA");
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Ok, 0));
        assert_line1(&o, &["xyz:TSLA", "ETH", "@151", "xyz:NOPE"]);
        let s: HlSweep = o.typed().unwrap();
        let got: Vec<(&str, ObsStatus)> = s
            .coins
            .iter()
            .map(|c| (c.coin.as_str(), c.status))
            .collect();
        assert_eq!(
            got,
            [
                ("xyz:TSLA", ObsStatus::Ok),
                ("ETH", ObsStatus::Ok),
                ("@151", ObsStatus::Ok),
                ("xyz:NOPE", ObsStatus::Absent)
            ]
        );
        assert!(s.coins[3].not_found);
        // The default dex needs no perp meta; xyz does (once).
        assert_eq!(count(&seen, "metaAndAssetCtxs"), 2);
        assert_eq!(count(&seen, "spotMetaAndAssetCtxs"), 1);
        assert_eq!(count(&seen, "perpDexs"), 1);
        assert_eq!(s.weight(), 20 * 7, "3 ctx + 2 at-cap + 2 meta reads");
        // Rows: xyz 128 + default 40 + spot 7 coins × 2 schemas + xyz:NOPE × 2.
        assert_eq!(s.rows_written, 2 * (128 + 40 + 7) + 2);
        let nope = store
            .get("mkt_ctx/1:hyperliquid:xyz:NOPE")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nope.status, ObsStatus::Absent);
        let eth: MarketCtx = store
            .get("mkt_ctx/1:hyperliquid:ETH")
            .await
            .unwrap()
            .unwrap()
            .typed()
            .unwrap();
        assert!((eth.taker_fee_bps.unwrap() - 4.5).abs() < 1e-12);
        assert!(o.render_text(NOW).len() < 3_000, "summary text stays small");

        // Again within 5 s: all four from the cache, no request.
        let before = seen.lock().unwrap().len();
        let c = hl_ctx_obs(
            &hl,
            Some(&store),
            &t,
            None,
            FeeBasis::default(),
            NOW + 1_000,
        )
        .await;
        assert_eq!((c.source, c.status), (ObsSource::Cache, ObsStatus::Ok));
        assert_eq!(c.features["from_cache"], true);
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn dex_sweep_is_cached_as_one_summary() {
        let (base, seen) = serve_info(routes()).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let t = CtxTarget::Dex("xyz".into());
        let o = hl_ctx_obs(&hl, Some(&store), &t, None, FeeBasis::default(), NOW).await;
        assert_eq!(o.key, "hl_sweep/1:hyperliquid:xyz");
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Ok, CTX_TTL_MS));
        assert_eq!(
            o.headline,
            "hl_ctx dex=xyz 128 coins: 109 ok 19 absent (19 delisted) weight=80"
        );
        assert_line1(&o, &["dex=xyz"]);
        assert_eq!(o.features["rows_written"], 256);
        let c = hl_ctx_obs(
            &hl,
            Some(&store),
            &t,
            None,
            FeeBasis::default(),
            NOW + 2_000,
        )
        .await;
        assert_eq!(c.source, ObsSource::Cache);
        assert_eq!(count(&seen, "metaAndAssetCtxs"), 1);
        // The default dex: no perp meta read.
        let d = hl_ctx_obs(
            &hl,
            Some(&store),
            &CtxTarget::Dex(String::new()),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(d.key, "hl_sweep/1:hyperliquid:default");
        assert!(
            d.headline.starts_with("hl_ctx dex=default 40 coins:"),
            "{}",
            d.headline
        );
        assert_eq!(count(&seen, "perpDexs"), 1);
    }

    #[tokio::test]
    async fn hl_errors_map_to_rows_never_zero() {
        let (unknown_dex, bogus) = (
            route(
                json!({"type": "metaAndAssetCtxs", "dex": "nope"}),
                500,
                "null",
            ),
            route(
                json!({"type": "metaAndAssetCtxs", "dex": "para"}),
                502,
                "bad gateway",
            ),
        );
        let mut r = routes();
        r.extend([
            unknown_dex,
            bogus,
            route(
                json!({"type": "perpsAtOpenInterestCap", "dex": "nope"}),
                500,
                "null",
            ),
            route(
                json!({"type": "perpsAtOpenInterestCap", "dex": "para"}),
                429,
                "slow down",
            ),
        ]);
        let (base, _) = serve_info(r).await;
        let hl = hl(&base);
        let store = MemStore::default();
        // Unknown dex: absent, not an outage.
        let o = hl_ctx_obs(
            &hl,
            Some(&store),
            &CtxTarget::Dex("nope".into()),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(o.status, ObsStatus::Absent);
        assert_eq!(
            o.headline,
            "hl_ctx dex=nope: no markets (HL does not know it)"
        );
        assert!(o
            .errors
            .iter()
            .any(|e| e.class == ErrorClass::NotApplicable));
        let c = hl_ctx_obs(
            &hl,
            Some(&store),
            &coins(&["nope:TSLA"]),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(
            (c.key.as_str(), c.status),
            ("mkt_ctx/1:hyperliquid:nope:TSLA", ObsStatus::Absent)
        );
        // A 502 on the ctx read: error rows with the class, never cached.
        let e = hl_ctx_obs(
            &hl,
            Some(&store),
            &coins(&["para:STX"]),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(e.status, ObsStatus::Error);
        assert_eq!(e.errors[0].class, ErrorClass::Transient);
        assert!(e.features.is_empty() || !e.features.contains_key("mark"));
        assert!(store
            .get("mkt_ctx/1:hyperliquid:para:STX")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|o| o.key == "mkt_ctx/1:hyperliquid:para:STX" && o.status == ObsStatus::Error));
        let mixed = hl_ctx_obs(
            &hl,
            Some(&store),
            &coins(&["para:STX", "xyz:TSLA"]),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(mixed.status, ObsStatus::Partial);
        assert_eq!(mixed.features["n_error"], 1);
        assert_eq!(
            mixed.features["n_failed_reads"], 2,
            "para ctx + para at-cap"
        );
    }

    #[tokio::test]
    async fn failed_side_reads_make_instrument_rows_partial() {
        let r = vec![
            route(
                json!({"type": "metaAndAssetCtxs", "dex": "xyz"}),
                200,
                MAC_XYZ,
            ),
            route(
                json!({"type": "perpsAtOpenInterestCap", "dex": "xyz"}),
                429,
                "slow down",
            ),
            route(json!({"type": "perpDexs"}), 200, PERP_DEXS),
            route(json!({"type": "perpCategories"}), 502, "bad gateway"),
        ];
        let (base, seen) = serve_info(r).await;
        let hl = hl(&base);
        let store = MemStore::default();
        let o = hl_ctx_obs(
            &hl,
            Some(&store),
            &CtxTarget::Dex("xyz".into()),
            None,
            FeeBasis::default(),
            NOW,
        )
        .await;
        assert_eq!(o.status, ObsStatus::Partial);
        let fields: Vec<&str> = o.errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(fields, ["at_oi_cap", "category"]);
        let inst = store
            .get("mkt_instrument/1:hyperliquid:xyz:TSLA")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inst.status, ObsStatus::Partial);
        let i: MarketInstrument = inst.typed().unwrap();
        assert_eq!(
            (i.at_oi_cap, i.category, i.asset_id),
            (None, None, Some(110_001))
        );
        let ctx = store
            .get("mkt_ctx/1:hyperliquid:xyz:TSLA")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ctx.status, ObsStatus::Ok);
        assert!(!ctx.features.contains_key("at_oi_cap") && !ctx.features.contains_key("category"));
        // The partial meta row is kept 60 s, not 1 h; the failed cap row is not stored.
        let meta = store
            .get("hl_perp_meta/1:hyperliquid")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((meta.status, meta.ttl_ms), (ObsStatus::Partial, 60_000));
        assert!(store
            .get("hl_at_oi_cap/1:hyperliquid:xyz")
            .await
            .unwrap()
            .is_none());
        assert_eq!(count(&seen, "perpCategories"), 1);
    }

    #[tokio::test]
    async fn no_store_reads_live_every_time() {
        let (base, seen) = serve_info(routes()).await;
        let hl = hl(&base);
        for _ in 0..2 {
            let o = hl_ctx_obs(&hl, None, &coins(&["ETH"]), None, FeeBasis::default(), NOW).await;
            assert_eq!((o.status, o.source), (ObsStatus::Ok, ObsSource::Live));
        }
        assert_eq!(count(&seen, "metaAndAssetCtxs"), 2);
    }

    #[test]
    fn args_parse_strictly() {
        let ok = |v: Value| CtxTarget::from_args(&v).unwrap();
        let err = |v: Value| CtxTarget::from_args(&v).unwrap_err().to_string();
        assert_eq!(ok(json!({"dex": "xyz"})), CtxTarget::Dex("xyz".into()));
        assert_eq!(ok(json!({"dex": ""})), CtxTarget::Dex(String::new()));
        assert_eq!(ok(json!({"dex": "default"})), CtxTarget::Dex(String::new()));
        assert_eq!(
            ok(json!({"coins": [" ETH ", "xyz:TSLA", "ETH"]})),
            coins(&["ETH", "xyz:TSLA"])
        );
        assert_eq!(ok(json!({"coins": ["ETH"], "dex": ""})), coins(&["ETH"]));
        assert_eq!(
            ok(json!({"coins": [], "dex": "io"})),
            CtxTarget::Dex("io".into())
        );
        assert!(err(json!({})).contains("give 'coins'"));
        assert!(err(json!({"coins": ["ETH"], "dex": "xyz"})).contains("not both"));
        assert!(err(json!({"coins": ["#67890"]})).contains("outcome coin `#67890`"));
        assert!(err(json!({"coins": ["xyz TSLA"]})).contains("hyperliquid:xyz TSLA"));
        assert!(err(json!({"coins": "ETH"})).contains("array of strings"));
        assert!(err(json!({"dex": "xyz:TSLA"})).contains("not a Hyperliquid perp dex"));
        let many: Vec<String> = (0..65).map(|i| format!("xyz:C{i}")).collect();
        assert!(err(json!({"coins": many})).contains("at most 64"));
    }

    #[tokio::test]
    async fn execute_gates_scope_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let tool = HlCtxTool::new(HlShared::default());
        assert_eq!(tool.definition().name, names::HL_CTX);
        let h = TestHarness::with_scope(dir.path(), ToolScope::default());
        assert!(tool
            .execute(&json!({"coins": ["ETH"]}), &h.ctx())
            .await
            .is_err());
        let h = TestHarness::new(dir.path());
        let e = tool
            .execute(&json!({"max_age_secs": 5}), &h.ctx())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("give 'coins'"), "{e}");
        let e = tool
            .execute(&json!({"coins": ["ETH"], "max_age_secs": -1}), &h.ctx())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("max_age_secs"), "{e}");
        // Net scope without the HL host: the read is refused, never sent — an error row.
        let o = tool
            .execute(&json!({"coins": ["ETH"]}), &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(o.errors[0].class, ErrorClass::Fatal);
        assert!(o.errors[0].message.contains("net_hosts"), "{:?}", o.errors);
    }

    // ── live (mainnet; `cargo test --bin tengu live_hl -- --ignored --test-threads 1`) ──

    #[tokio::test]
    #[ignore]
    async fn live_hl_ctx_xyz_sweep_then_coins() {
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
        let tool = HlCtxTool::new(HlShared {
            store: Some(Arc::new(SqliteObservationStore::open(dir.path()).unwrap())),
            fees: FeeBasis::default(),
        });
        let run = |args: Value| {
            let (tool, h) = (&tool, &h);
            async move {
                let o = tool
                    .execute(&args, &h.ctx())
                    .await
                    .unwrap()
                    .observation
                    .unwrap();
                println!(
                    "{}",
                    o.render_text(o.observed_at_ms).lines().next().unwrap()
                );
                assert_features_ok(&o.features);
                o
            }
        };
        let o = run(json!({"dex": "xyz"})).await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        let s: HlSweep = o.typed().unwrap();
        assert!(s.coins.len() > 100, "{} xyz coins", s.coins.len());
        let t = run(json!({"coins": ["xyz:TSLA"]})).await;
        assert_eq!((t.status, t.source), (ObsStatus::Ok, ObsSource::Cache));
        let c: MarketCtx = t.typed().unwrap();
        assert!(c.mark.value().is_some() && c.oracle.value().is_some());
        assert!(c.taker_fee_bps.is_some() && c.category == Some(Category::Stocks));
        let m =
            run(json!({"coins": ["xyz:TSLA", "ETH", "@151", "xyz:NOPE"], "max_age_secs": 0})).await;
        assert_eq!(m.status, ObsStatus::Ok, "{:?}", m.errors);
        assert_eq!(m.features["n_ok"], 3);
        assert_eq!(m.features["n_not_found"], 1);
    }
}
