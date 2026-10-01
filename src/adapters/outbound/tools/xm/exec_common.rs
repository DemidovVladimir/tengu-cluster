//! `run_exec` — the `[risk]` gate inside every exec tool
//! (`risk-gate-enforcement`, tracker conventions 9, 12, 16, 20): one order,
//! gate + fill + ledger write in one `BEGIN IMMEDIATE`
//! (`PaperLedger::place`), the same in-process and through
//! `tengu mcp-bridge` (the tool runs in either process). Callers:
//! `paper_order`, `paper_close` (`risk-paper-tools`), strategies.
//!
//! | Step | Rule | Refused (tool error, nothing written) |
//! |---|---|---|
//! | 1 | `[risk]` + `[paper]` and the ledger (`[xmarket]`) | `risk_config_missing` · `state_dir_missing` · `ledger_unavailable` |
//! | 2 | the calling agent is private — no `description`, not `default` (the load rule again: a planner step's `compose.tools` can hand any tool to a routable agent); the gate fits the account (review #5): `ExecGate::Shadow` only with the paper engine's [`PaperFills`] (`[risk] mode = "paper"`) and never on the `[risk]` account, `ExecGate::Risk` only on it | `exec_agent_not_private` · `shadow_not_paper` · `gate_account_mismatch` |
//! | 3 | `client_order_id` = the arg, else `ToolCtx.call_id` (bridge: `mcp:<process nonce>:<JSON-RPC id>`) — never random; the request's fingerprint (`exec::order_fingerprint`: tool, account, full id, `close` or side + notional) is stored with the order | `no_client_order_id` · `invalid_client_order_id` |
//! | 4 | the account (`limits.account`) opened on first use with `[paper] initial_cash_usd` (a shadow account, `ExecGate::Shadow`: its own cash); an order stored under the id ⇒ its row, `replayed` — no latency, no book read, nothing written — unless it was placed with another fingerprint (review #11) | `client_order_id_conflict` |
//! | 5 | store reads, never fetched: `mkt_ctx/1` of the open positions, of the instruments that owe funding and of the order's (and hedge) instrument, `mkt_instrument/1` of the instrument (`domain::xm::exec::order_venue_facts`: a reduce-only order falls back to the facts kept with its position when the row is missing, older or partial — review #6), the `opportunity` row | `missing:mkt_instrument` |
//! | 6 | funding owed booked first: every owed and due hour of the account at a fresh `mkt_ctx/1` rate + oracle (`ledger::Position::settle_funding`; a row stamped > 1 s ahead is not fresh); without one nothing is written here — `place` settles the order's instrument at the size held, owed when no rate is known (review #10) | — |
//! | 7 | the order checked before the latency (`[paper] order_types`, `check_order`); a close sized from the position; a reduce-only order's IOC bound cut to `exec::MAX_EXIT_SLIPPAGE_BPS` (500 bps, review #9) | `order_type` · `invalid_order` · `no_position` |
//! | 8 | `fill_with_latency` on the `Clock` + `BookSource` (live: `SystemClock`, `hyperliquid::book::HlBookSource`, the `hl_book/1` read recorded + stored); a hedge leg's book right after | — a failed read is the gate's `missing:book` |
//! | 8b | [`ExecOrder::tp_sl_on_book`] (`xm_exits` without a fresh mark, review #6): take-profit / stop-loss judged on that book's mid (`exits::tp_sl_due`; the book within `max_data_age_ms.book`) — nothing fires, or no fresh two-sided book ⇒ [`Executed::NotCalled`], nothing placed or written; one fires ⇒ recorded (`PaperLedger::trigger_exit`), the id `exit:<account>:<instrument>:<reason>:<opened_ms>` | — |
//! | 9 | kill-switch probe, then `place(decide(plan))` (`application/paper.rs`): funding the instrument owes settled first, value, gate (`[risk]`, or the shadow gate for `ExecGate::Shadow`), fill, write — an entry probes the kill-switch file again inside the transaction (review #7); a deny writes one verdict row (with the call id, the tool and `TENGU_SESSION_ID`; mirrored to `<TENGU_HOME>/logs/risk.jsonl`); a sent order keeps the row's venue facts with the position | — |
//! | 10 | row `paper_fill/1:<account>:<client_order_id>` (ttl 0: recorded, never cached) — `domain/xm/exec.rs` | — |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};

use super::risk_status::mark_of;
use super::XmShared;
use crate::adapters::outbound::paper_store::kill_switch_state;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::application::paper::{decide, fill_with_latency, ExecPlan, PaperFill};
use crate::config::risk::{PaperConfig, RiskConfig, RiskMode};
use crate::domain::book::Side;
use crate::domain::hl::FeeBasis;
use crate::domain::market::{InstrumentId, MarketCtx, MarketInstrument, HYPERLIQUID};
use crate::domain::observation::{
    ErrorClass, Field, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::xm::exec::{
    client_order_id_error, order_fingerprint, order_venue_facts, GateSummary, PaperFillRow,
    MAX_EXIT_SLIPPAGE_BPS,
};
use crate::domain::xm::exits::{
    exit_client_order_id, tp_sl_due, ExitReason, ExitRules, ExitTrigger,
};
use crate::domain::xm::ledger::{
    due_funding_hours, stamp_age_ms, FundingRate, Mark, PaperAccount, PaperPositions, Position,
};
use crate::domain::xm::paper::{
    jittered_latency_ms, FillEnv, OrderKind, OrderSize, PaperOrder, Tif,
};
use crate::domain::xm::risk::{
    BookInput, CtxInput, EdgeInput, GateKind, LegMarket, OrderIntent, RiskLimits,
};
use crate::ports::book::{BookRead, BookSource};
use crate::ports::clock::Clock;
use crate::ports::observation::ObservationStore;
use crate::ports::paper::{PaperLedger, PlaceRequest, Placement};
use crate::ports::tool::ToolCtx;

/// Refusal: the calling agent may not place orders.
pub(crate) const EXEC_AGENT_NOT_PRIVATE: &str = "exec_agent_not_private";
/// Refusal: neither a `client_order_id` argument nor a call id.
pub(crate) const NO_CLIENT_ORDER_ID: &str = "no_client_order_id";

/// Size of an exec order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ExecSize {
    /// USD at the book mid, rounded down to the lot size.
    NotionalUsd(f64),
    /// The whole open position, the opposite side, reduce-only (`paper_close`).
    Close,
}

/// Proof that `run_exec` fills on the paper engine (`application/paper.rs`:
/// the latency on the clock, the book after it, `simulate_fill`, the paper
/// ledger) — what a shadow order needs (review #5). Only [`PaperFills::of`]
/// makes one, and only for `[risk] mode = "paper"`: a live engine (M3b) gets
/// none, so the uncapped shadow gate can never send a venue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaperFills(());

impl PaperFills {
    /// `Some` only when `[risk] mode = "paper"` (the paper engine fills).
    pub(crate) fn of(risk: &RiskConfig) -> Option<Self> {
        (risk.mode == RiskMode::Paper).then_some(Self(()))
    }
}

/// Which gate an exec order passes and how its account opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ExecGate {
    /// The `[risk]` gate, every rule, on the `[risk]` account; a new account
    /// opens with `[paper] initial_cash_usd`.
    Risk,
    /// A measurement account (the weekend fade's shadow ledger):
    /// `domain::xm::risk::evaluate_shadow`; a new account opens with this
    /// cash. Paper only (`paper`), never the `[risk]` account — `run_exec`
    /// checks both.
    Shadow {
        initial_cash_usd: f64,
        paper: PaperFills,
    },
}

impl ExecGate {
    fn kind(self) -> GateKind {
        match self {
            ExecGate::Risk => GateKind::Risk,
            ExecGate::Shadow { .. } => GateKind::Shadow,
        }
    }
}

/// Refusal: a shadow order off the paper engine.
pub(crate) const SHADOW_NOT_PAPER: &str = "shadow_not_paper";
/// Refusal: a gate on the wrong account (a shadow order on the `[risk]`
/// account, a `[risk]` order on another).
pub(crate) const GATE_ACCOUNT_MISMATCH: &str = "gate_account_mismatch";

/// Review #5: the shadow gate runs only on the paper engine and never on
/// the `[risk]` account; the `[risk]` gate only on it.
fn check_gate(order: &ExecOrder, risk: &RiskConfig) -> Result<()> {
    let account = &order.limits.account;
    match order.gate {
        ExecGate::Shadow { .. } if PaperFills::of(risk).is_none() => bail!(
            "{SHADOW_NOT_PAPER}: a shadow order fills on the paper engine only — [risk] mode is \
             {:?}",
            risk.mode
        ),
        ExecGate::Shadow { .. } if *account == risk.account => bail!(
            "{GATE_ACCOUNT_MISMATCH}: shadow account `{account}` is the [risk] account — the \
             shadow gate skips the budget, so it never trades the capped account"
        ),
        ExecGate::Risk if *account != risk.account => bail!(
            "{GATE_ACCOUNT_MISMATCH}: the [risk] gate trades the [risk] account `{}`, not \
             `{account}`",
            risk.account
        ),
        _ => Ok(()),
    }
}

/// One order for [`run_exec`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExecOrder {
    /// The calling tool (the row's `tool`).
    pub tool: &'static str,
    pub gate: ExecGate,
    /// The gate's limits and the ledger account: `[risk]` (`limits()`), or a
    /// shadow account's own.
    pub limits: RiskLimits,
    pub instrument: InstrumentId,
    /// Ignored for `Close` (the position decides).
    pub side: Side,
    pub size: ExecSize,
    pub kind: OrderKind,
    pub limit_px: Option<f64>,
    pub reduce_only: bool,
    /// The order's IOC bound vs the book mid.
    pub max_slippage_bps: f64,
    pub strategy: Option<String>,
    pub hedge_instrument: Option<InstrumentId>,
    /// Key of the row carrying `edge_after_costs_bps` (`min_edge`).
    pub opportunity_key: Option<String>,
    /// The tool's arg; `None` ⇒ `ToolCtx.call_id`. Ignored with
    /// `tp_sl_on_book` (the reason that fires picks the id).
    pub client_order_id: Option<String>,
    /// Deadline of the position this order opens (exit rules).
    pub exit_at_ms: Option<i64>,
    /// A close that fires only when these take-profit / stop-loss rules
    /// hold at the mid of the book read after the latency (module table:
    /// 8b) — `xm_exits` on a stale or missing mark. `None` = always placed.
    pub tp_sl_on_book: Option<ExitRules>,
}

/// What [`exec`] did with one order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Executed {
    /// The order's `paper_fill/1` row: placed (allowed or denied), or
    /// replayed.
    Row(Observation),
    /// [`ExecOrder::tp_sl_on_book`]: the book's `mid` fired `trigger`
    /// (recorded in the ledger — the stored one when another caller fired
    /// first); the close's row.
    Called {
        trigger: ExitTrigger,
        mid: f64,
        row: Observation,
    },
    /// [`ExecOrder::tp_sl_on_book`]: the book's mid — or why there is none
    /// — fired nothing; nothing placed or written.
    NotCalled { mid: Field<f64> },
}

/// Where time and books come from: live = `SystemClock` + `HlBookSource` +
/// `rate_limit::jitter01()`; tests = `ManualClock` + `ScriptedBooks`.
pub(crate) struct ExecIo<'a> {
    pub clock: &'a dyn Clock,
    pub books: &'a dyn BookSource,
    /// Latency jitter draw in [0, 1].
    pub rand01: f64,
}

/// The module table: one order through the gate. `Ok` = the typed row
/// (`denied` / `rejected` are rows with status `error`); `Err` = refused
/// before the gate, nothing written. An order judged on the book
/// (`tp_sl_on_book`) goes through [`exec`].
pub(crate) async fn run_exec(
    shared: &XmShared,
    ctx: &ToolCtx<'_>,
    io: &ExecIo<'_>,
    order: ExecOrder,
) -> Result<Observation> {
    match exec(shared, ctx, io, order).await? {
        Executed::Row(row) | Executed::Called { row, .. } => Ok(row),
        Executed::NotCalled { .. } => {
            bail!("not_called: the book fired no take-profit / stop-loss — nothing was placed")
        }
    }
}

/// The mid of the book read after the latency — at most `max_age_ms` old
/// at `now_ms` — or why there is none (a stale book judges nothing, like a
/// stale mark).
fn book_mid(
    book: &std::result::Result<BookRead, ReadError>,
    now_ms: i64,
    max_age_ms: u64,
) -> Field<f64> {
    let missing = |why: String| Field::err(ReadError::new("book", ErrorClass::Transient, why));
    match book {
        Ok(read) if read.age_ms(now_ms) > max_age_ms => missing(format!(
            "stale: book age {} ms > {max_age_ms} ms",
            read.age_ms(now_ms)
        )),
        Ok(read) => read
            .book
            .mid()
            .map_or_else(|| missing("one-sided book: no mid".into()), Field::ok),
        Err(e) => Field::err(e.clone()),
    }
}

/// [`run_exec`], with what an order judged on the book did (module table:
/// 8b).
pub(crate) async fn exec(
    shared: &XmShared,
    ctx: &ToolCtx<'_>,
    io: &ExecIo<'_>,
    order: ExecOrder,
) -> Result<Executed> {
    let (risk, paper, ledger) = shared.parts()?;
    check_private_agent(ctx)?;
    check_gate(&order, risk)?;
    // Judged on the book: the reason that fires picks the id (step 8b).
    let fixed_coid = match order.tp_sl_on_book {
        None => Some(client_order_id(
            order.client_order_id.as_deref(),
            ctx.call_id,
        )?),
        Some(_) => None,
    };
    let call_id = ctx.call_id.map(str::to_string);
    let account = order.limits.account.clone();
    let id = order.instrument.to_string();
    let max_ctx_ms = order.limits.max_data_age_ms.ctx;
    let store = shared.store.as_deref();
    let fingerprint = order_fingerprint(
        order.tool,
        &account,
        &id,
        match order.size {
            ExecSize::Close => None,
            ExecSize::NotionalUsd(n) => Some((order.side, n)),
        },
    );

    let now = io.clock.now_ms();
    let initial_cash_usd = match order.gate {
        ExecGate::Risk => paper.initial_cash_usd,
        ExecGate::Shadow {
            initial_cash_usd, ..
        } => initial_cash_usd,
    };
    ledger.open_account(&account, initial_cash_usd, now).await?;
    if let Some(coid) = &fixed_coid {
        if let Some(p) = ledger.stored(&account, coid).await? {
            // Review #11: another request's order under this id is refused.
            if let Some(why) = p
                .order
                .as_ref()
                .and_then(|o| o.conflict(Some(&fingerprint)))
            {
                bail!("{why}");
            }
            let ids = open_ids(&p.account);
            let rows = MarketRows::read(store, &ids, None, None).await;
            let row = row_of(
                &p,
                coid,
                call_id,
                None,
                None,
                &rows.marks(&ids),
                now,
                max_ctx_ms,
            );
            return Ok(Executed::Row(finish(store, order.tool, &row, now).await));
        }
    }

    let snapshot = ledger.snapshot(&account, now).await?;
    let mut ids = open_ids(&snapshot.account);
    ids.insert(id.clone());
    if let Some(h) = &order.hedge_instrument {
        ids.insert(h.to_string());
    }
    // Funding is settled on every instrument that owes it, a closed one too.
    let mut row_ids = ids.clone();
    row_ids.extend(snapshot.account.funding_ids());
    let rows = MarketRows::read(store, &row_ids, Some(&id), order.opportunity_key.as_deref()).await;
    let reduce_only = order.reduce_only || order.size == ExecSize::Close;
    // Review #6: a reduce-only order falls back to the facts kept with its
    // position when the instrument row is missing, older or partial.
    let (facts, keep_facts) = order_venue_facts(
        rows.instrument.as_ref().map(|(i, at)| (i, *at)),
        rows.ctx_of(&id).as_ref(),
        fee_basis(paper),
        snapshot.facts.get(&id),
        reduce_only,
    )
    .map_err(|why| anyhow!("missing:mkt_instrument: {why}"))?;
    accrue_due_funding(ledger.as_ref(), &snapshot.account, &rows, now, max_ctx_ms).await?;
    // The order instrument's rate, for the hours `place` settles (step 9).
    let funding = rows.funding_rate(&id, now, max_ctx_ms);

    let held = snapshot.account.positions.get(&id);
    let (side, size, close) = match order.size {
        ExecSize::Close => match held.map_or(0.0, |p| p.qty) {
            q if q == 0.0 => bail!("no_position: account {account} holds no {id}"),
            q => (
                if q > 0.0 { Side::Sell } else { Side::Buy },
                OrderSize::Qty(q.abs()),
                true,
            ),
        },
        ExecSize::NotionalUsd(n) => (order.side, OrderSize::NotionalUsd(n), false),
    };
    // Review #9: an exit's IOC bound never passes the hard ceiling (the
    // gate skips slippage for exits; the arg parsers refuse a larger one).
    let max_slippage_bps = if reduce_only {
        order.max_slippage_bps.min(MAX_EXIT_SLIPPAGE_BPS)
    } else {
        order.max_slippage_bps
    };
    let opened_ms = held.and_then(|p| p.opened_ms);
    let mut paper_order = PaperOrder {
        // Judged on the book: the stop-loss id until the book decides.
        client_order_id: fixed_coid.clone().unwrap_or_else(|| {
            exit_client_order_id(
                &account,
                &id,
                ExitReason::StopLoss,
                opened_ms.unwrap_or_default(),
                1,
            )
        }),
        instrument: order.instrument.clone(),
        side,
        size,
        kind: order.kind,
        tif: Tif::Ioc,
        limit_px: order.limit_px,
        reduce_only,
        max_slippage_bps,
        ref_mid: None,
    };
    let underlying = held.map_or_else(|| id.clone(), |p| p.underlying.clone());
    let position = held
        .cloned()
        .unwrap_or_else(|| Position::flat(&id, &underlying, HYPERLIQUID));
    let env = FillEnv {
        rules: &facts.rules,
        status: facts.status,
        position: &position,
        fees: &facts.fees,
        max_book_age_ms: order.limits.max_data_age_ms.book,
    };
    let (latency_ms, book) =
        match fill_with_latency(io.clock, io.books, paper, &paper_order, &env, io.rand01).await {
            Ok(PaperFill {
                book: Some(read),
                latency_ms,
                ..
            }) => (latency_ms, Ok(read)),
            // Refused before sending: `order_type` / `invalid_order`.
            Ok(PaperFill { result, .. }) => bail!(
                "{}: {}",
                result.reason.map_or("invalid_order", |r| r.as_str()),
                result.message.unwrap_or_default()
            ),
            Err(e) => (
                jittered_latency_ms(paper.latency_ms, paper.latency_jitter_ms, io.rand01),
                Err(e),
            ),
        };
    let hedge = match &order.hedge_instrument {
        Some(h) => Some((h.to_string(), io.books.fresh_book(h).await)),
        None => None,
    };

    // Step 8b: take-profit / stop-loss on this book's mid.
    let mut called = None;
    let coid = match (fixed_coid, order.tp_sl_on_book) {
        (Some(coid), _) => coid,
        (None, rules) => {
            let rules = rules.ok_or_else(|| anyhow!("an order needs an id or exit rules"))?;
            let mid = book_mid(&book, io.clock.now_ms(), order.limits.max_data_age_ms.book);
            let fired = mid
                .value()
                .and_then(|px| Some((*px, tp_sl_due(&position, *px, &rules)?)));
            let Some((px, reason)) = fired else {
                return Ok(Executed::NotCalled { mid });
            };
            let opened = opened_ms.ok_or_else(|| anyhow!("open position {id} has no opened_ms"))?;
            let trigger = ledger
                .trigger_exit(&account, &id, opened, reason, io.clock.now_ms())
                .await?
                .ok_or_else(|| {
                    anyhow!("no_position: account {account} holds no {id} opened at {opened}")
                })?;
            called = Some((trigger, px));
            exit_client_order_id(&account, &id, trigger.reason, opened, 1)
        }
    };
    paper_order.client_order_id = coid.clone();

    let now = io.clock.now_ms();
    let mut legs = BTreeMap::from([(
        id.clone(),
        LegMarket {
            book: book_input(&id, &book),
            ctx: rows.ctx_input(&id),
        },
    )]);
    if let Some((hid, hbook)) = &hedge {
        legs.insert(
            hid.clone(),
            LegMarket {
                book: book_input(hid, hbook),
                ctx: rows.ctx_input(hid),
            },
        );
    }
    let opportunity = match &order.opportunity_key {
        Some(key) => match &rows.failed {
            Some(e) => Field::err(e.clone()),
            None => EdgeInput::from_row(key, rows.opportunity.as_ref()),
        },
        None => Field::Absent,
    };
    let marks = rows.marks(&ids);
    let plan = ExecPlan {
        gate: order.gate.kind(),
        limits: order.limits.clone(),
        order: paper_order,
        close,
        strategy: order.strategy.clone(),
        hedge_instrument: order.hedge_instrument.as_ref().map(|h| h.to_string()),
        opportunity_key: order.opportunity_key.clone(),
        exit_at_ms: order.exit_at_ms,
        marks: marks.clone(),
        legs,
        opportunity,
        kill_switch: kill_switch_state(&risk.kill_switch_file),
        // Review #7: an entry probes the file again inside the transaction.
        kill_recheck: Some({
            let file = risk.kill_switch_file.clone();
            Box::new(move || kill_switch_state(&file))
        }),
        rules: facts.rules,
        fees: facts.fees,
        status: facts.status,
        book: book.clone(),
    };
    let req = PlaceRequest {
        account,
        client_order_id: coid.clone(),
        call_id: call_id.clone(),
        tool: order.tool.to_string(),
        session_id: std::env::var("TENGU_SESSION_ID").ok(),
        fingerprint: Some(fingerprint),
        instrument: id,
        funding,
        facts: keep_facts,
        now_ms: now,
    };
    let placement = ledger.place(req, decide(plan)).await?;
    let book_age_ms = book.as_ref().ok().map(|b| b.age_ms(now));
    let row = row_of(
        &placement,
        &coid,
        call_id,
        Some(latency_ms),
        book_age_ms,
        &marks,
        now,
        max_ctx_ms,
    );
    let row = finish(store, order.tool, &row, now).await;
    Ok(match called {
        Some((trigger, mid)) => Executed::Called { trigger, mid, row },
        None => Executed::Row(row),
    })
}

/// Step 2 of the module table.
pub(crate) fn check_private_agent(ctx: &ToolCtx<'_>) -> Result<()> {
    let why = match ctx.agent_config {
        None => "no calling agent is known",
        Some(a) if a.description.is_some() => {
            "the calling agent has a `description` (planner-routable)"
        }
        Some(a) if a.default => "the calling agent is the default chat agent",
        Some(_) => return Ok(()),
    };
    bail!(
        "{EXEC_AGENT_NOT_PRIVATE}: {why} — exec tools run only as a private agent (no \
         `description`, not `default`, no webhook endpoint's `agent`)"
    )
}

/// Step 3 of the module table.
pub(crate) fn client_order_id(arg: Option<&str>, call_id: Option<&str>) -> Result<String> {
    let Some(id) = arg.or(call_id) else {
        bail!(
            "{NO_CLIENT_ORDER_ID}: no client_order_id argument and no call id — an order needs \
             an idempotency key, never a random one"
        );
    };
    if let Some(why) = client_order_id_error(id) {
        bail!("invalid_client_order_id: {why}");
    }
    Ok(id.to_string())
}

pub(crate) fn fee_basis(paper: &PaperConfig) -> FeeBasis {
    FeeBasis {
        tier: paper.fee_tier,
        staking_discount_pct: paper.staking_discount_pct,
    }
}

fn open_ids(account: &PaperAccount) -> BTreeSet<String> {
    account
        .open_positions()
        .map(|p| p.instrument.clone())
        .collect()
}

/// The book leg of the gate: the read after the latency, keyed like the
/// `hl_book/1` row it wrote.
fn book_input(id: &str, book: &std::result::Result<BookRead, ReadError>) -> Field<BookInput> {
    match book {
        Ok(read) => Field::ok(BookInput {
            key: format!("hl_book/1:{id}"),
            observed_at_ms: read.observed_at_ms,
            book: read.book.clone(),
        }),
        Err(e) => Field::err(e.clone()),
    }
}

/// The stored rows one order reads (step 5), never fetched.
pub(crate) struct MarketRows {
    /// `mkt_ctx/1` rows by full id, as stored (the gate and the marks judge
    /// their age).
    ctx: BTreeMap<String, Observation>,
    /// The order instrument's `mkt_instrument/1`, decoded, and when it was
    /// observed.
    instrument: Option<(MarketInstrument, i64)>,
    opportunity: Option<Observation>,
    /// No store, or the read failed: every row is this error.
    failed: Option<ReadError>,
}

impl MarketRows {
    /// `mkt_ctx/1` of `ids`, the `mkt_instrument/1` of `instrument`, the
    /// `opportunity` row — one store read.
    pub(crate) async fn read(
        store: Option<&dyn ObservationStore>,
        ids: &BTreeSet<String>,
        instrument: Option<&str>,
        opportunity: Option<&str>,
    ) -> Self {
        let mut out = Self {
            ctx: BTreeMap::new(),
            instrument: None,
            opportunity: None,
            failed: None,
        };
        let Some(store) = store else {
            out.failed = Some(ReadError::new(
                "store",
                ErrorClass::NotApplicable,
                "no observation store",
            ));
            return out;
        };
        let ctx_keys: Vec<String> = ids
            .iter()
            .map(|id| Observation::key_for(MarketCtx::SCHEMA, id))
            .collect();
        let mut keys = ctx_keys.clone();
        if let Some(i) = instrument {
            keys.push(Observation::key_for(MarketInstrument::SCHEMA, i));
        }
        if let Some(k) = opportunity {
            keys.push(k.to_string());
        }
        let rows = match store.get_many(&keys).await {
            Ok(rows) => rows,
            Err(e) => {
                out.failed = Some(ReadError::new(
                    "store",
                    ErrorClass::Transient,
                    format!("{e:#}"),
                ));
                return out;
            }
        };
        let mut rows = rows.into_iter();
        for id in ids {
            if let Some(Some(row)) = rows.next() {
                out.ctx.insert(id.clone(), row);
            }
        }
        if instrument.is_some() {
            out.instrument = rows
                .next()
                .flatten()
                .filter(|r| r.status != ObsStatus::Error)
                .and_then(|r| {
                    let at = r.observed_at_ms;
                    r.typed::<MarketInstrument>().ok().map(|i| (i, at))
                });
        }
        out.opportunity = rows.next().flatten();
        out
    }

    /// The instrument's context, decoded (any age).
    fn ctx_of(&self, id: &str) -> Option<MarketCtx> {
        self.ctx
            .get(id)
            .filter(|r| r.status != ObsStatus::Error)
            .and_then(|r| r.typed::<MarketCtx>().ok())
    }

    /// The instrument's context, fresh at `now_ms` within `max_age_ms`; a
    /// row stamped more than 1 s ahead is never fresh (`stamp_age_ms`).
    fn fresh_ctx(&self, id: &str, now_ms: i64, max_age_ms: u64) -> Option<MarketCtx> {
        let row = self.ctx.get(id)?;
        let age = stamp_age_ms(now_ms, row.observed_at_ms)?;
        (age <= max_age_ms).then(|| self.ctx_of(id)).flatten()
    }

    /// The instrument's funding rate + oracle from a fresh row; `None` when
    /// the row is stale, missing or has no valid pair.
    pub(crate) fn funding_rate(
        &self,
        id: &str,
        now_ms: i64,
        max_age_ms: u64,
    ) -> Option<FundingRate> {
        let c = self.fresh_ctx(id, now_ms, max_age_ms)?;
        FundingRate::new(*c.funding_1h.value()?, *c.oracle.value()?)
    }

    fn missing(&self, what: &str) -> ReadError {
        self.failed
            .clone()
            .unwrap_or_else(|| ReadError::new(what, ErrorClass::Transient, "absent"))
    }

    /// The gate's context leg of `id`.
    fn ctx_input(&self, id: &str) -> Field<CtxInput> {
        match (self.ctx.get(id), &self.failed) {
            (Some(row), _) => CtxInput::from_row(row),
            (None, Some(e)) => Field::err(e.clone()),
            (None, None) => Field::Absent,
        }
    }

    /// Marks of `ids` from their `mkt_ctx/1` rows, at each row's time.
    pub(crate) fn marks(&self, ids: &BTreeSet<String>) -> BTreeMap<String, Field<Mark>> {
        ids.iter()
            .map(|id| {
                let mark = match (self.ctx.get(id), &self.failed) {
                    (Some(row), _) => mark_of(row),
                    (None, Some(_)) => Field::err(self.missing("mkt_ctx")),
                    (None, None) => Field::Absent,
                };
                (id.clone(), mark)
            })
            .collect()
    }
}

/// Step 6: the funding `account` owes (`PaperAccount::funding_ids`: open
/// positions, and closed ones with hours owed), settled per instrument at a
/// fresh `mkt_ctx/1` rate + oracle — owed hours and due hours booked. A
/// stale or missing row writes nothing: the hours stay due (a fill settles
/// them owed first, inside `place`; the next fresh rate books them). HL's
/// per-hour history is not read: past hours are booked at the current
/// rate. The hours booked.
pub(crate) async fn accrue_due_funding(
    ledger: &dyn PaperLedger,
    account: &PaperAccount,
    rows: &MarketRows,
    now_ms: i64,
    max_age_ms: u64,
) -> Result<usize> {
    let mut booked = 0;
    for p in account.positions.values().filter(|p| !p.is_settled()) {
        let Some(rate) = rows.funding_rate(&p.instrument, now_ms, max_age_ms) else {
            continue;
        };
        if p.funding_owed.is_empty() && due_funding_hours(p, now_ms).is_empty() {
            continue;
        }
        booked += ledger
            .settle_funding(&account.account, &p.instrument, Some(rate), now_ms)
            .await?
            .booked
            .len();
    }
    Ok(booked)
}

/// The `paper_fill/1` row of a placement (a replay: no latency, no book).
#[allow(clippy::too_many_arguments)]
fn row_of(
    p: &Placement,
    coid: &str,
    call_id: Option<String>,
    latency_ms: Option<u64>,
    book_age_ms: Option<u64>,
    marks: &BTreeMap<String, Field<Mark>>,
    now_ms: i64,
    max_ctx_ms: u64,
) -> PaperFillRow {
    let d = &p.decision;
    let intent: Option<OrderIntent> = serde_json::from_value(d.intent.clone()).ok();
    let fill = p.order.as_ref().map(|o| o.result.clone());
    let side = intent
        .as_ref()
        .map(|i| i.side)
        .or(fill.as_ref().map(|f| f.side))
        .unwrap_or(Side::Buy);
    let reduce_only = intent
        .as_ref()
        .map(|i| i.reduce_only)
        .or(fill.as_ref().map(|f| f.reduce_only))
        .unwrap_or(false);
    let valued = PaperPositions::build(&p.account, marks, now_ms, max_ctx_ms, None, None);
    PaperFillRow {
        account: d.account.clone(),
        client_order_id: coid.to_string(),
        call_id: p.order.as_ref().map_or(call_id, |o| o.call_id.clone()),
        instrument: d.instrument.clone(),
        side,
        reduce_only,
        replayed: p.replayed,
        gate: GateSummary::of(&d.verdict, d.id),
        intent: d.intent.clone(),
        book_age_ms: book_age_ms.or(fill.as_ref().and_then(|f| f.book_age_ms)),
        fill,
        latency_ms,
        position_qty_after: p
            .account
            .positions
            .get(&d.instrument)
            .map_or(0.0, |x| x.qty),
        equity_usd_after: valued.equity_usd,
        exit_at_ms: p.order.as_ref().and_then(|o| o.exit_at_ms),
        ts_ms: p.order.as_ref().map_or(d.ts_ms, |o| o.ts_ms),
    }
}

/// The observation, recorded like `observe()` (ttl 0: never cached).
pub(crate) async fn finish<T: Observed>(
    store: Option<&dyn ObservationStore>,
    tool: &str,
    row: &T,
    now_ms: i64,
) -> Observation {
    let obs = Observation::of(tool, row, now_ms, 0, ObsSource::Live);
    store_live(store, &obs).await;
    obs
}

/// Live IO for one tool call: the system clock, the HL book source on the
/// tool's scope, a fresh jitter draw.
pub(crate) fn live_books(
    ctx: &ToolCtx<'_>,
    store: Option<Arc<dyn ObservationStore>>,
) -> crate::adapters::outbound::tools::hyperliquid::book::HlBookSource {
    crate::adapters::outbound::tools::hyperliquid::book::HlBookSource::for_call(ctx, store)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::outbound::paper_store::tests::audit_lines;
    use crate::adapters::outbound::paper_store::{SqlitePaperLedger, RISK_LOG_FILE};
    use crate::adapters::outbound::shell::LocalShellExecutor;
    use crate::adapters::outbound::tools::workspace::test_support::NoopActivity;
    use crate::application::observe::tests::MemStore;
    use crate::config::risk::RiskConfig;
    use crate::config::AgentConfig;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::market::{InstrumentKind, Listing, QuoteCcy};
    use crate::domain::observation::Features;
    use crate::domain::scope::ToolScope;
    use crate::domain::secrets::SecretRegistry;
    use crate::domain::tools as names;
    use crate::domain::xm::paper::FillStatus;
    use crate::domain::xm::risk::rules;
    use crate::ports::book::ScriptedBooks;
    use crate::ports::clock::ManualClock;
    use crate::ports::shell::ShellExecutionPort;

    pub(crate) const TSLA: &str = "hyperliquid:xyz:TSLA";
    pub(crate) const OPP: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";
    /// Venue time of the fixture book (2026-09-30T13:35:52.605Z).
    pub(crate) const BOOK_TS: i64 = 1_790_775_352_605;
    /// Orders start 400 ms after it.
    pub(crate) const NOW: i64 = BOOK_TS + 400;

    pub(crate) fn risk(kill: &Path, max_order: u32) -> RiskConfig {
        let toml = format!(
            r#"
account = "xmarket"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["hyperliquid:xyz:TSLA"]
instruments_deny = []
max_order_notional_usd = {max_order}
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 1
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = []
max_skew_ms = 5000
max_orders_per_min = 6
max_open_orders = 4
kill_switch_file = "{}"
allow_reduce_degraded = true
exits = {{ take_profit_bps = 200, stop_loss_bps = 100, max_hold_secs = 86400 }}
max_data_age_ms = {{ book = 5000, ctx = 20000, reference = 60000, quote = 20000 }}
"#,
            kill.display()
        );
        toml::from_str::<RiskConfig>(&toml).unwrap().resolved()
    }

    pub(crate) fn paper() -> PaperConfig {
        toml::from_str(
            "initial_cash_usd = 100\nlatency_ms = 250\nlatency_jitter_ms = 100\nfee_tier = 0\n\
             staking_discount_pct = 0\norder_types = [\"market\", \"ioc\"]\n",
        )
        .unwrap()
    }

    /// `mkt_ctx/1` + `mkt_instrument/1` of TSLA at `at_ms` (mark `px`,
    /// funding 1 bp / h, oracle `px`).
    pub(crate) fn market_rows(px: f64, at_ms: i64) -> Vec<Observation> {
        let id = InstrumentId::parse(TSLA).unwrap();
        let mut c = MarketCtx::new(id.clone(), at_ms);
        c.mark = Field::ok(px);
        c.oracle = Field::ok(px);
        c.funding_1h = Field::ok(0.0001);
        c.at_oi_cap = Some(false);
        let mut i = MarketInstrument::new(id, InstrumentKind::Perp, Listing::Listed);
        i.sz_decimals = Some(3);
        i.quote_ccy = Some(QuoteCcy::Usdc);
        i.deployer_fee_scale = Some(1.0);
        i.growth_mode = Some(true);
        i.at_oi_cap = Some(false);
        vec![
            Observation::of(names::HL_CTX, &c, at_ms, 5_000, ObsSource::Live),
            Observation::of(names::HL_CTX, &i, at_ms, 60_000, ObsSource::Live),
        ]
    }

    /// An opportunity row naming TSLA with `edge` bps after costs.
    pub(crate) fn opportunity(edge: f64, at_ms: i64) -> Observation {
        let mut features = Features::new();
        features.insert("edge_after_costs_bps".into(), serde_json::json!(edge));
        Observation {
            key: OPP.into(),
            schema: "xm_compare/1".into(),
            tool: "xm_compare".into(),
            observed_at_ms: at_ms,
            slot: None,
            ttl_ms: 60_000,
            source: ObsSource::Live,
            status: ObsStatus::Ok,
            errors: Vec::new(),
            headline: format!("compare {TSLA} edge={edge}"),
            features,
            data: serde_json::Value::Null,
        }
    }

    pub(crate) struct Rig {
        pub dir: tempfile::TempDir,
        pub store: Arc<MemStore>,
        pub ledger: Arc<SqlitePaperLedger>,
        pub shared: XmShared,
        pub clock: Arc<ManualClock>,
        pub books: ScriptedBooks,
        pub agent: AgentConfig,
        pub scope: ToolScope,
        shell: Arc<dyn ShellExecutionPort>,
        http: reqwest::Client,
        secrets: SecretRegistry,
        activity: NoopActivity,
    }

    impl Rig {
        /// The $100 budget ($`max_order` per order), fresh TSLA rows and an
        /// opportunity row, the fixture book forever; clock at `NOW`.
        pub(crate) async fn new(max_order: u32) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(MemStore::default());
            for row in market_rows(347.2, NOW - 1_000) {
                store.put(&row).await.unwrap();
            }
            store.put(&opportunity(12.0, NOW - 1_000)).await.unwrap();
            let ledger = Arc::new(
                SqlitePaperLedger::open(&dir.path().join("state"))
                    .unwrap()
                    .with_audit(dir.path().join("logs").join(RISK_LOG_FILE)),
            );
            let shared = XmShared {
                store: Some(store.clone() as Arc<dyn ObservationStore>),
                ledger: Ok(ledger.clone() as Arc<dyn PaperLedger>),
                risk: Some(risk(&dir.path().join("KILL"), max_order)),
                paper: Some(paper()),
                history: None,
                fade: None,
            };
            let clock = Arc::new(ManualClock::at(NOW));
            let books = ScriptedBooks::new(
                clock.clone(),
                InstrumentId::parse(TSLA).unwrap(),
                vec![(0, tsla_book())],
            );
            let agent: AgentConfig =
                toml::from_str("engine = \"openrouter\"\nmodel = \"m\"\n").unwrap();
            Rig {
                scope: ToolScope {
                    fs_roots: vec![dir.path().to_path_buf()],
                    ..Default::default()
                },
                dir,
                store,
                ledger,
                shared,
                clock,
                books,
                agent,
                shell: Arc::new(LocalShellExecutor::new()),
                http: reqwest::Client::new(),
                secrets: SecretRegistry::new(),
                activity: NoopActivity,
            }
        }

        pub(crate) fn ctx<'a>(&'a self, call_id: Option<&'a str>) -> ToolCtx<'a> {
            ToolCtx {
                workspace: self.dir.path(),
                scope: &self.scope,
                shell: self.shell.as_ref(),
                http: &self.http,
                memory_manager: None,
                secret_registry: &self.secrets,
                activity: &self.activity,
                conversation: crate::ports::tool::ConversationView::empty(),
                agent_config: Some(&self.agent),
                call_id,
            }
        }

        /// A $`usd` market buy of TSLA naming the opportunity row.
        pub(crate) fn buy(&self, usd: f64) -> ExecOrder {
            ExecOrder {
                tool: names::PAPER_ORDER,
                gate: ExecGate::Risk,
                limits: self.shared.risk.as_ref().unwrap().limits(),
                instrument: InstrumentId::parse(TSLA).unwrap(),
                side: Side::Buy,
                size: ExecSize::NotionalUsd(usd),
                kind: OrderKind::Market,
                limit_px: None,
                reduce_only: false,
                max_slippage_bps: 30.0,
                strategy: Some("overreaction".into()),
                hedge_instrument: None,
                opportunity_key: Some(OPP.into()),
                client_order_id: None,
                exit_at_ms: None,
                tp_sl_on_book: None,
            }
        }

        pub(crate) fn close(&self) -> ExecOrder {
            ExecOrder {
                tool: names::PAPER_CLOSE,
                size: ExecSize::Close,
                reduce_only: true,
                opportunity_key: None,
                strategy: None,
                ..self.buy(0.0)
            }
        }

        pub(crate) async fn run(&self, order: ExecOrder, call_id: &str) -> Result<Observation> {
            let io = ExecIo {
                clock: self.clock.as_ref(),
                books: &self.books,
                rand01: 0.5,
            };
            run_exec(&self.shared, &self.ctx(Some(call_id)), &io, order).await
        }

        /// The `risk.jsonl` lines the ledger mirrored.
        pub(crate) fn risk_lines(&self) -> Vec<serde_json::Value> {
            audit_lines(&self.dir.path().join("logs").join(RISK_LOG_FILE))
        }

        /// Rows per ledger table.
        pub(crate) fn rows(&self) -> BTreeMap<&'static str, i64> {
            let c = rusqlite::Connection::open(self.dir.path().join("state/ledger.db")).unwrap();
            ["orders", "fills", "positions", "risk_decisions", "funding"]
                .into_iter()
                .map(|t| {
                    let n: i64 = c
                        .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                        .unwrap();
                    (t, n)
                })
                .collect()
        }
    }

    fn row(o: &Observation) -> PaperFillRow {
        o.typed().unwrap()
    }

    /// Allowed: gate + fill + write; the book read after the 250 ms latency;
    /// the row carries the fill and the account after it.
    #[tokio::test]
    async fn an_allowed_order_fills_after_the_latency_and_writes_once() {
        let rig = Rig::new(25).await;
        let o = rig.run(rig.buy(25.0), "mcp:n:2").await.unwrap();
        assert_eq!(o.key, "paper_fill/1:xmarket:mcp:n:2");
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        let r = row(&o);
        assert!(r.gate.allow && !r.replayed, "{:?}", r.gate);
        assert_eq!(rig.books.reads(), [NOW + 250], "one read after the latency");
        let f = r.fill.as_ref().unwrap();
        assert_eq!(f.status, FillStatus::Filled);
        assert_eq!((f.filled_qty, f.avg_px), (0.072, Some(347.23)));
        assert_eq!(r.position_qty_after, 0.072);
        assert_eq!((r.latency_ms, r.book_age_ms), (Some(250), Some(650)));
        for (k, v) in [("risk", "allow"), ("risk_rule", "ok"), ("status", "filled")] {
            assert_eq!(o.features[k], v, "{k}");
        }
        assert_eq!(o.features["levels_used"], 1);
        let equity = o.features["equity_usd_after"].as_f64().unwrap();
        let want = 100.0 - f.fee_usd + 0.072 * (347.2 - 347.23);
        assert!((equity - want).abs() < 1e-9, "{equity} vs {want}");
        let n = rig.rows();
        assert_eq!(
            (n["orders"], n["fills"], n["positions"], n["risk_decisions"]),
            (1, 1, 1, 1)
        );
        let d = rig.ledger.decisions("xmarket", 1).await.unwrap();
        assert_eq!(d[0].call_id.as_deref(), Some("mcp:n:2"), "joins the audit");
        assert!(
            o.headline.ends_with("risk=allow rule=ok coid=mcp:n:2"),
            "{}",
            o.headline
        );
    }

    /// Denied: exactly one verdict row and one `risk.jsonl` line, no order /
    /// fill / position; the row has status error and names the rule.
    #[tokio::test]
    async fn a_denied_intent_writes_one_verdict_row_and_no_order() {
        let rig = Rig::new(25).await;
        let o = rig.run(rig.buy(30.0), "loop:s:1").await.unwrap();
        assert_eq!(o.status, ObsStatus::Error);
        let r = row(&o);
        assert!(!r.gate.allow && r.fill.is_none());
        assert_eq!(r.gate.rule, rules::ORDER_NOTIONAL);
        assert_eq!(
            (o.features["risk"].clone(), o.features["risk_rule"].clone()),
            ("deny".into(), rules::ORDER_NOTIONAL.into())
        );
        assert!(
            o.errors[0].message.starts_with("denied order_notional"),
            "{:?}",
            o.errors
        );
        let n = rig.rows();
        assert_eq!(
            (n["orders"], n["fills"], n["positions"], n["risk_decisions"]),
            (0, 0, 0, 1)
        );
        let lines = rig.risk_lines();
        assert_eq!(lines.len(), 1, "one mirror line per verdict row");
        let line = &lines[0];
        for (k, v) in [
            ("verdict", "deny"),
            ("rule", rules::ORDER_NOTIONAL),
            ("call_id", "loop:s:1"),
            ("client_order_id", "loop:s:1"),
            ("tool", names::PAPER_ORDER),
            ("instrument", TSLA),
        ] {
            assert_eq!(line[k], v, "{k}");
        }
        assert_eq!(line["decision_id"], r.gate.decision_id);
        assert!(line["fill"].is_null(), "no order, no fill");
        assert_eq!(line["intent"]["notional_usd"], 30.0);
        // No opportunity row: an entry is denied `missing:edge_after_costs_bps`.
        let mut blind = rig.buy(20.0);
        blind.opportunity_key = None;
        let o = rig.run(blind, "loop:s:2").await.unwrap();
        assert_eq!(row(&o).gate.rule, "missing:edge_after_costs_bps");
        assert_eq!(rig.rows()["risk_decisions"], 2);
    }

    /// The kill-switch file denies the next entry `kill_switch` and records
    /// a sticky `file` halt; a reduce-only close still passes (degraded).
    #[tokio::test]
    async fn the_kill_switch_file_denies_entries_but_not_closes() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "c:1").await.unwrap();
        std::fs::write(rig.dir.path().join("KILL"), "").unwrap();
        let o = rig.run(rig.buy(20.0), "c:2").await.unwrap();
        assert_eq!(row(&o).gate.rule, rules::KILL_SWITCH);
        let snap = rig.ledger.snapshot("xmarket", NOW + 10_000).await.unwrap();
        assert_eq!(
            snap.risk.halt.map(|h| h.reason),
            Some(crate::domain::xm::risk::HaltReason::File)
        );
        let o = rig.run(rig.close(), "c:3").await.unwrap();
        let r = row(&o);
        assert!(r.gate.allow && r.gate.degraded, "{:?}", r.gate);
        assert_eq!(r.gate.rule, rules::ALLOW_REDUCE_DEGRADED);
        assert_eq!(r.fill.as_ref().unwrap().status, FillStatus::Filled);
        assert_eq!(r.position_qty_after, 0.0, "flat");
    }

    /// A retry with the same id returns the stored fill: no latency, no book
    /// read, nothing written.
    #[tokio::test]
    async fn a_replay_returns_the_stored_fill() {
        let rig = Rig::new(25).await;
        let first = row(&rig.run(rig.buy(25.0), "mcp:n:2").await.unwrap());
        let (t, reads, rows) = (rig.clock.now_ms(), rig.books.reads(), rig.rows());
        let o = rig.run(rig.buy(25.0), "mcp:n:2").await.unwrap();
        let again = row(&o);
        assert!(again.replayed && o.headline.contains(" replayed"));
        assert_eq!(again.fill, first.fill);
        assert_eq!(again.gate, first.gate);
        assert_eq!(
            (rig.clock.now_ms(), rig.books.reads(), rig.rows()),
            (t, reads, rows)
        );
        assert_eq!(rig.risk_lines().len(), 1, "a replay mirrors nothing");
        assert_eq!(again.latency_ms, None);
        // An explicit client_order_id wins over the call id.
        let mut named = rig.buy(25.0);
        named.client_order_id = Some("mcp:n:2".into());
        assert!(row(&rig.run(named, "other").await.unwrap()).replayed);
    }

    #[tokio::test]
    async fn refusals_happen_before_the_gate() {
        let rig = Rig::new(25).await;
        let io = ExecIo {
            clock: rig.clock.as_ref(),
            books: &rig.books,
            rand01: 0.5,
        };
        let err = |r: Result<Observation>| r.unwrap_err().to_string();
        let e = err(run_exec(&rig.shared, &rig.ctx(None), &io, rig.buy(20.0)).await);
        assert!(e.starts_with("no_client_order_id"), "{e}");
        let mut bad = rig.buy(20.0);
        bad.client_order_id = Some("two words".into());
        assert!(err(rig.run(bad, "x").await).starts_with("invalid_client_order_id"));
        // A routable or default agent may not place orders.
        let mut routable = Rig::new(25).await;
        routable.agent.description = Some("routable".into());
        let e = err(routable.run(routable.buy(20.0), "x").await);
        assert!(
            e.starts_with("exec_agent_not_private: the calling agent has a `description`"),
            "{e}"
        );
        routable.agent.description = None;
        routable.agent.default = true;
        assert!(err(routable.run(routable.buy(20.0), "x").await).contains("default chat agent"));
        // No position to close, no instrument row.
        assert!(err(rig.run(rig.close(), "y").await).starts_with("no_position"));
        let mut nvda = rig.buy(20.0);
        nvda.instrument = InstrumentId::parse("hyperliquid:xyz:NVDA").unwrap();
        assert!(err(rig.run(nvda, "z").await).starts_with("missing:mkt_instrument"));
        // Order types off.
        let mut ioc = rig.buy(20.0);
        ioc.kind = OrderKind::Limit;
        ioc.limit_px = Some(347.3);
        let mut shared = rig.shared.clone();
        shared.paper.as_mut().unwrap().order_types = vec![crate::config::risk::OrderType::Market];
        let e = err(run_exec(&shared, &rig.ctx(Some("w")), &io, ioc).await);
        assert!(e.starts_with("order_type"), "{e}");
        assert_eq!(rig.rows()["risk_decisions"], 0, "nothing judged");
        // No [risk] / no ledger.
        let mut s = rig.shared.clone();
        s.ledger = Err(format!(
            "{}: no [xmarket] section",
            super::super::STATE_DIR_MISSING
        ));
        assert!(
            err(run_exec(&s, &rig.ctx(Some("v")), &io, rig.buy(20.0)).await)
                .starts_with("state_dir_missing")
        );
        s.risk = None;
        assert!(
            err(run_exec(&s, &rig.ctx(Some("v")), &io, rig.buy(20.0)).await)
                .starts_with("risk_config_missing")
        );
    }

    /// A failed book read: an entry is denied `missing:book`; a close under
    /// `allow_reduce_degraded` is sent and refused `stale_book`.
    #[tokio::test]
    async fn no_book_after_the_latency() {
        let mut rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "b:1").await.unwrap();
        rig.books = ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![],
        )
        .failing(ReadError::new("book", ErrorClass::Timeout, "slow"));
        let o = rig.run(rig.buy(20.0), "b:2").await.unwrap();
        assert_eq!(row(&o).gate.rule, "missing:book");
        let o = rig.run(rig.close(), "b:3").await.unwrap();
        let r = row(&o);
        assert!(r.gate.allow && r.gate.degraded);
        let f = r.fill.as_ref().unwrap();
        assert_eq!(
            (f.status, f.reason.map(|x| x.as_str())),
            (FillStatus::Rejected, Some("stale_book"))
        );
        assert!(
            f.message.as_deref().unwrap().contains("timeout slow"),
            "{f:?}"
        );
        // $20 at mid 347.195 → 0.057 (szDecimals 3): still open.
        assert_eq!(r.position_qty_after, 0.057, "nothing closed");
    }

    /// Funding owed at a fresh rate is booked before the next order.
    #[tokio::test]
    async fn funding_is_booked_before_the_next_order() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "f:1").await.unwrap();
        // Two hour boundaries later, a fresh ctx row.
        let later = NOW + 2 * 3_600_000;
        rig.clock.set(later);
        for r in market_rows(347.2, later - 500) {
            rig.store.put(&r).await.unwrap();
        }
        // The fixture book is two hours old now: the order itself is denied
        // `book_age` — the funding was booked before the gate.
        let o = rig.run(rig.buy(10.0), "f:2").await.unwrap();
        assert_eq!(row(&o).gate.rule, rules::BOOK_AGE);
        assert_eq!(rig.rows()["funding"], 2, "{}", o.render_text(later));
        let snap = rig.ledger.snapshot("xmarket", later + 1_000).await.unwrap();
        assert!(
            snap.account.positions[TSLA].funding_paid > 0.0,
            "a long pays"
        );
    }

    /// Review #10: a close two hours after the entry on a stale ctx row (a
    /// degraded exit) records both hours owed at the size it held — not
    /// dropped with the position — and a fresh rate later books them.
    #[tokio::test]
    async fn a_degraded_close_keeps_the_funding_it_owes() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "f:1").await.unwrap();
        let later = NOW + 2 * 3_600_000;
        rig.clock.set(later);
        let mut book = tsla_book();
        book.venue_ts_ms = later;
        let books = ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![(0, book)],
        );
        let fresh = ExecIo {
            clock: rig.clock.as_ref(),
            books: &books,
            rand01: 0.5,
        };
        let o = run_exec(&rig.shared, &rig.ctx(Some("f:2")), &fresh, rig.close())
            .await
            .unwrap();
        let r = row(&o);
        assert!(r.gate.allow && r.gate.degraded, "ctx 2 h old: {:?}", r.gate);
        assert_eq!(r.position_qty_after, 0.0);
        assert_eq!(rig.rows()["funding"], 0, "no rate: nothing booked");
        let snap = rig.ledger.snapshot("xmarket", later + 1_000).await.unwrap();
        let owed = &snap.account.positions[TSLA].funding_owed;
        assert_eq!(owed.len(), 2, "{owed:?}");
        assert!(owed.iter().all(|h| h.qty == 0.057), "{owed:?}");
        // A fresh rate: both hours booked at the size held then.
        for r in market_rows(347.2, later + 2_000) {
            rig.store.put(&r).await.unwrap();
        }
        let store = rig.store.as_ref() as &dyn ObservationStore;
        let rows = MarketRows::read(Some(store), &snap.account.funding_ids(), None, None).await;
        let booked = accrue_due_funding(
            rig.ledger.as_ref(),
            &snap.account,
            &rows,
            later + 2_500,
            20_000,
        )
        .await
        .unwrap();
        assert_eq!(booked, 2);
        let a = rig
            .ledger
            .snapshot("xmarket", later + 3_000)
            .await
            .unwrap()
            .account;
        let p = &a.positions[TSLA];
        assert!(p.is_settled());
        let want = 2.0 * 0.057 * 347.2 * 0.0001;
        assert!((p.funding_paid - want).abs() < 1e-12, "{}", p.funding_paid);
        assert_eq!(rig.rows()["funding"], 2);
    }

    fn io(rig: &Rig) -> ExecIo<'_> {
        ExecIo {
            clock: rig.clock.as_ref(),
            books: &rig.books,
            rand01: 0.5,
        }
    }

    /// Review #5: the shadow gate (no budget) runs only on the paper engine
    /// and never on the `[risk]` account; the `[risk]` gate only on it.
    /// Refusals write nothing.
    #[tokio::test]
    async fn the_shadow_gate_is_paper_only_and_never_the_risk_account() {
        let rig = Rig::new(25).await;
        let risk = rig.shared.risk.clone().unwrap();
        let paper = PaperFills::of(&risk).expect("[risk] mode = paper");
        let shadow = |account: &str| ExecOrder {
            gate: ExecGate::Shadow {
                initial_cash_usd: 10_000.0,
                paper,
            },
            limits: RiskLimits {
                account: account.into(),
                ..risk.limits()
            },
            ..rig.buy(20.0)
        };
        let err = |r: Result<Observation>| r.unwrap_err().to_string();
        let e = err(rig.run(shadow("xmarket"), "s:1").await);
        assert!(
            e.starts_with("gate_account_mismatch: shadow account `xmarket` is the [risk] account"),
            "{e}"
        );
        let mut elsewhere = rig.buy(20.0);
        elsewhere.limits.account = "elsewhere".into();
        let e = err(rig.run(elsewhere, "s:2").await);
        assert!(
            e.starts_with("gate_account_mismatch: the [risk] gate trades the [risk] account"),
            "{e}"
        );
        // Off the paper engine: no proof is handed out, and one made before
        // is refused at run time.
        let mut live = rig.shared.clone();
        live.risk.as_mut().unwrap().mode = crate::config::risk::RiskMode::Live;
        assert_eq!(PaperFills::of(live.risk.as_ref().unwrap()), None);
        let e = err(run_exec(&live, &rig.ctx(Some("s:3")), &io(&rig), shadow("xm-shadow")).await);
        assert!(e.starts_with("shadow_not_paper"), "{e}");
        let n = rig.rows();
        assert_eq!((n["orders"], n["risk_decisions"]), (0, 0), "nothing judged");
        assert_eq!(
            rig.ledger.accounts().await.unwrap(),
            Vec::<String>::new(),
            "no account opened"
        );
        // A shadow account on the paper engine: placed through its gate.
        let o = rig.run(shadow("xm-shadow"), "s:4").await.unwrap();
        let r = row(&o);
        assert!(r.gate.allow, "{:?}", r.gate);
        assert_eq!(r.account, "xm-shadow");
        assert_eq!(rig.ledger.accounts().await.unwrap(), ["xm-shadow"]);
    }

    /// A ledger whose `place` first creates the kill-switch file: the
    /// operator `touch`es it while the order waits for the write lock.
    struct KillOnPlace {
        inner: Arc<SqlitePaperLedger>,
        kill: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl PaperLedger for KillOnPlace {
        async fn open_account(&self, a: &str, cash: f64, now: i64) -> Result<PaperAccount> {
            self.inner.open_account(a, cash, now).await
        }
        async fn accounts(&self) -> Result<Vec<String>> {
            self.inner.accounts().await
        }
        async fn snapshot(&self, a: &str, now: i64) -> Result<crate::ports::paper::LedgerSnapshot> {
            self.inner.snapshot(a, now).await
        }
        async fn place(
            &self,
            req: PlaceRequest,
            decide: crate::ports::paper::Decide,
        ) -> Result<Placement> {
            std::fs::write(&self.kill, "").unwrap();
            self.inner.place(req, decide).await
        }
        async fn update_risk_state(
            &self,
            a: &str,
            now: i64,
            update: crate::ports::paper::RiskUpdate,
        ) -> Result<crate::ports::paper::LedgerSnapshot> {
            self.inner.update_risk_state(a, now, update).await
        }
        async fn order(
            &self,
            a: &str,
            id: &str,
        ) -> Result<Option<crate::ports::paper::StoredOrder>> {
            self.inner.order(a, id).await
        }
        async fn stored(&self, a: &str, id: &str) -> Result<Option<Placement>> {
            self.inner.stored(a, id).await
        }
        async fn decisions(
            &self,
            a: &str,
            limit: usize,
        ) -> Result<Vec<crate::ports::paper::StoredDecision>> {
            self.inner.decisions(a, limit).await
        }
        async fn settle_funding(
            &self,
            a: &str,
            i: &str,
            rate: Option<FundingRate>,
            now: i64,
        ) -> Result<crate::domain::xm::ledger::FundingSettlement> {
            self.inner.settle_funding(a, i, rate, now).await
        }
        async fn trigger_exit(
            &self,
            a: &str,
            i: &str,
            opened_ms: i64,
            reason: ExitReason,
            now: i64,
        ) -> Result<Option<ExitTrigger>> {
            self.inner.trigger_exit(a, i, opened_ms, reason, now).await
        }
    }

    /// Review #7: the kill-switch file `touch`ed after the probe before
    /// `place` still denies an entry — probed again inside the transaction —
    /// and trips the sticky `file` halt; a reduce-only exit keeps the probe
    /// made before `place` (today's semantics).
    #[tokio::test]
    async fn the_kill_switch_is_probed_again_inside_the_transaction() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "k:1").await.unwrap();
        let kill = rig.dir.path().join("KILL");
        let mut late = rig.shared.clone();
        late.ledger = Ok(Arc::new(KillOnPlace {
            inner: rig.ledger.clone(),
            kill: kill.clone(),
        }) as Arc<dyn PaperLedger>);
        let o = run_exec(&late, &rig.ctx(Some("k:2")), &io(&rig), rig.close())
            .await
            .unwrap();
        let r = row(&o);
        assert!(r.gate.allow && !r.gate.degraded, "{:?}", r.gate);
        assert_eq!((r.gate.rule.as_str(), r.position_qty_after), ("ok", 0.0));
        assert!(kill.exists());
        std::fs::remove_file(&kill).unwrap();
        let o = run_exec(&late, &rig.ctx(Some("k:3")), &io(&rig), rig.buy(20.0))
            .await
            .unwrap();
        let r = row(&o);
        assert_eq!(r.gate.rule, rules::KILL_SWITCH, "{:?}", r.gate);
        assert_eq!(
            r.gate.trips,
            vec![crate::domain::xm::risk::HaltReason::File]
        );
        let snap = rig.ledger.snapshot("xmarket", NOW + 10_000).await.unwrap();
        assert_eq!(
            snap.risk.halt.map(|h| h.reason),
            Some(crate::domain::xm::risk::HaltReason::File)
        );
    }

    /// Review #11: an id stored by another request is refused, never
    /// replayed as the foreign order — nothing read or written; the same
    /// request still replays.
    #[tokio::test]
    async fn a_replay_of_another_request_is_refused() {
        let rig = Rig::new(25).await;
        let first = row(&rig.run(rig.buy(20.0), "mcp:n:9").await.unwrap());
        let (reads, rows) = (rig.books.reads(), rig.rows());
        let e = rig
            .run(rig.buy(25.0), "mcp:n:9")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "client_order_id_conflict: order mcp:n:9 of account xmarket was placed as \
             `paper_order xmarket hyperliquid:xyz:TSLA buy 20 USD`; this request is \
             `paper_order xmarket hyperliquid:xyz:TSLA buy 25 USD` — use a new client_order_id"
        );
        let mut close = rig.close();
        close.client_order_id = Some("mcp:n:9".into());
        let e = rig.run(close, "c:1").await.unwrap_err().to_string();
        assert!(
            e.contains("this request is `paper_close xmarket hyperliquid:xyz:TSLA close`"),
            "{e}"
        );
        assert_eq!((rig.books.reads(), rig.rows()), (reads, rows));
        let again = row(&rig.run(rig.buy(20.0), "mcp:n:9").await.unwrap());
        assert!(again.replayed && again.fill == first.fill);
    }

    /// Review #9: an exit's IOC bound is cut to 500 bps from mid, whatever
    /// the order asked.
    #[tokio::test]
    async fn an_exit_bound_is_cut_to_the_ceiling() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "b:1").await.unwrap();
        let mut close = rig.close();
        close.max_slippage_bps = 5_000.0;
        let r = row(&rig.run(close, "b:2").await.unwrap());
        let f = r.fill.unwrap();
        assert_eq!(f.status, FillStatus::Filled);
        // Sell bound = mid 347.195 × (1 − 500 bps) = 329.835…, rounded up
        // on the tick grid — not × (1 − 5 000 bps) = 173.6.
        let bound = f.bound_px.unwrap();
        assert!((329.8..329.9).contains(&bound), "{bound}");
        assert_eq!(MAX_EXIT_SLIPPAGE_BPS, 500.0);
    }

    /// Review #12: a funding rate stamped in the future books nothing.
    #[tokio::test]
    async fn a_future_stamped_ctx_row_books_no_funding() {
        let rig = Rig::new(25).await;
        rig.run(rig.buy(20.0), "f:1").await.unwrap();
        let later = NOW + 2 * 3_600_000;
        rig.clock.set(later);
        for r in market_rows(347.2, later + 5_000) {
            rig.store.put(&r).await.unwrap();
        }
        rig.run(rig.buy(10.0), "f:2").await.unwrap();
        assert_eq!(
            rig.rows()["funding"],
            0,
            "a rate from the future is no rate"
        );
    }
}
