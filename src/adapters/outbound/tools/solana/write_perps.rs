//! `jup_perps_order` — a Jupiter perps SOL market order as a keeper request
//! (bot `jupiterPerpsEngine.ts`): TX1 escrows collateral into the request;
//! a Jupiter keeper fills it (TX2) at oracle price bounded by the slippage
//! price. Encoders: `domain/lp/perps_ix.rs`; runner: `write_common.rs`.
//!
//! | Rule | Detail |
//! |---|---|
//! | Actions | `increase` (open / add: `size_usd` + `collateral` in the side's token — SOL for long, USDC for short), `decrease` (`size_usd` + `collateral` in USD to withdraw), `close` (entire position) |
//! | Price bound | oracle × (1 ∓ `slippage_bps`): short increase / long decrease = floor, long increase / short decrease = ceiling — also on `close` (the bot closed at any price) |
//! | Checks | oracle price; no open keeper request of the wallet (a second request could double the hedge move); decrease / close need an open position (decrease ≤ its size); increase: post-order size ≤ `max_notional_usd`; funds (long: SOL ≥ collateral + 0.02 buffer; short: USDC ATA ≥ collateral) |
//! | Instructions | long increase: wSOL ATA idempotent → transfer → SyncNative → request; long decrease / close: wSOL ATA idempotent (NOT closed — the keeper pays into it) → request; short: the request |
//! | After a landed request | `lp_state.last_hedge_action = {action, live, signatures, position_request, counter}` via `merge_lp_state` — the snapshot's `PendingRequest` guard then waits for the keeper (request-aware cooldown) |

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::price::require_pubkey;
use super::write_common::{
    live_keeper_requests, merge_lp_state, parse_mode, run_write, Built, WriteBuilder,
};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::adapters::outbound::solana::send::TxPlan;
use crate::domain::lp::perps::{decode_position, Side, USD_PRECISION};
use crate::domain::lp::perps_ix::{
    collateral_mint, decrease_market_request, increase_market_request, position_request_pda,
    DecreaseParams, IncreaseParams, RequestChange,
};
use crate::domain::lp::snapshot::HedgeActionRecord;
use crate::domain::message::ToolDef;
use crate::domain::observation::now_ms;
use crate::domain::solana::{ata, ids, jup_position_pda, Pubkey};
use crate::domain::solana_tx::{
    ata_create_idempotent, spl_sync_native, system_transfer, Instruction,
};
use crate::domain::solana_write::{Check, WriteResult, WriteStatus};
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// SOL kept on top of a long's collateral (request + ATA rents, fees).
const LONG_SOL_BUFFER_LAMPORTS: u64 = 20_000_000;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(PerpsOrderTool {
        def: defs::def(names::JUP_PERPS_ORDER),
        shared: shared.clone(),
    })]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Increase,
    Decrease,
    Close,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Increase => "increase",
            Action::Decrease => "decrease",
            Action::Close => "close",
        }
    }
}

struct PerpsOrderTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for PerpsOrderTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::JUP_PERPS_ORDER;
        let mut builder = PerpsBuilder::parse(args)?;
        let mode = parse_mode(args, tool)?;
        builder.oracle = crate::adapters::outbound::solana::plan::oracle_usd(
            ctx,
            self.shared.store.as_deref(),
            ids::WSOL,
            now_ms(),
        )
        .await;
        let wallet = builder.wallet;
        run_write(ctx, &self.shared, tool, wallet, mode, &builder).await
    }
}

pub(crate) struct PerpsBuilder {
    pub wallet: Pubkey,
    /// The LP pool this hedge belongs to (`lp_state/1:<wallet>:<pool>`).
    pub pool: Pubkey,
    pub side: Side,
    pub action: Action,
    /// USD notional to add / remove (ignored for `close`).
    pub size_usd: f64,
    /// increase: side token (SOL / USDC); decrease: USD to withdraw.
    pub collateral: f64,
    pub slippage_bps: f64,
    pub max_notional_usd: f64,
    pub oracle: Option<f64>,
    /// The request of the last build (for `lp_state`).
    request: Mutex<Option<(Pubkey, u64)>>,
}

fn number(args: &Value, key: &str) -> Result<f64> {
    let tool = names::JUP_PERPS_ORDER;
    let v = args
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{tool}: '{key}' is required (number)"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(anyhow!("{tool}: '{key}' must be >= 0, got {v}"));
    }
    Ok(v)
}

impl PerpsBuilder {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let tool = names::JUP_PERPS_ORDER;
        let side = match args.get("side").and_then(Value::as_str) {
            Some("long") => Side::Long,
            Some("short") => Side::Short,
            _ => return Err(anyhow!("{tool}: 'side' must be long | short")),
        };
        let action = match args.get("action").and_then(Value::as_str) {
            Some("increase") => Action::Increase,
            Some("decrease") => Action::Decrease,
            Some("close") => Action::Close,
            _ => {
                return Err(anyhow!(
                    "{tool}: 'action' must be increase | decrease | close"
                ))
            }
        };
        let slippage_bps = number(args, "slippage_bps")?;
        if !(slippage_bps > 0.0 && slippage_bps < 10_000.0) {
            return Err(anyhow!("{tool}: 'slippage_bps' must be in (0, 10000)"));
        }
        let max_notional_usd = number(args, "max_notional_usd")?;
        if max_notional_usd <= 0.0 {
            return Err(anyhow!(
                "{tool}: 'max_notional_usd' must be > 0 (0 is not \"no cap\" here)"
            ));
        }
        let (size_usd, collateral) = match action {
            Action::Close => (0.0, 0.0),
            _ => (number(args, "size_usd")?, number(args, "collateral")?),
        };
        if action == Action::Increase && !(size_usd > 0.0 && collateral > 0.0) {
            return Err(anyhow!(
                "{tool}: increase needs size_usd > 0 and collateral > 0"
            ));
        }
        if action == Action::Decrease && !(size_usd > 0.0 || collateral > 0.0) {
            return Err(anyhow!("{tool}: decrease needs size_usd or collateral > 0"));
        }
        Ok(PerpsBuilder {
            wallet: require_pubkey(args, tool, "wallet")?,
            pool: require_pubkey(args, tool, "pool")?,
            side,
            action,
            size_usd,
            collateral,
            slippage_bps,
            max_notional_usd,
            oracle: None,
            request: Mutex::new(None),
        })
    }

    fn action_name(&self) -> String {
        format!("{}_{}", self.action.as_str(), self.side.as_str())
    }

    /// The keeper's fill bound, 6-dp USD: selling exposure (short increase,
    /// long decrease) = floor below the oracle, buying = ceiling above.
    pub(crate) fn price_bound(&self, oracle: f64) -> u64 {
        let f = self.slippage_bps / 10_000.0;
        let sells = matches!(
            (self.side, self.action),
            (Side::Short, Action::Increase)
                | (Side::Long, Action::Decrease)
                | (Side::Long, Action::Close)
        );
        let price = if sells {
            oracle * (1.0 - f)
        } else {
            oracle * (1.0 + f)
        };
        (price * USD_PRECISION).round() as u64
    }
}

fn random_counter() -> Result<u64> {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).map_err(|e| anyhow!("OS random source failed: {e}"))?;
    Ok(u64::from_le_bytes(b) % 1_000_000_000)
}

/// Current position (size, collateral, 6-dp USD) of the side; `None` = no
/// account or size 0.
struct PerpsReads {
    position: Option<(u64, u64)>,
    lamports: u64,
    usdc_raw: u64,
    keeper_requests: Result<Vec<Pubkey>, String>,
}

#[async_trait]
impl WriteBuilder for PerpsBuilder {
    async fn build(&self, rpc: &SolanaRpc, fence: Option<u64>) -> Result<Built> {
        let w = self.wallet;
        let token = ids::key(ids::TOKEN);
        let position = jup_position_pda(&w, self.side == Side::Long);
        let usdc_ata = ata(&w, &ids::key(ids::USDC), &token);
        let (_, reads) = rpc
            .get_multiple_accounts(&[position, usdc_ata], fence)
            .await?;
        let pos = match reads[0].data() {
            None => None,
            Some(d) => {
                let p = decode_position(&d).map_err(|e| anyhow!("position {position}: {e}"))?;
                (p.size_usd > 0).then_some((p.size_usd, p.collateral_usd))
            }
        };
        let usdc_raw = match reads[1].data() {
            Some(d) if d.len() >= 72 => u64::from_le_bytes(d[64..72].try_into().expect("8 bytes")),
            _ => 0,
        };
        let (_, lamports) = rpc.get_balance(&w).await?;
        let keeper_requests = live_keeper_requests(rpc, &w, now_ms() / 1000)
            .await
            .map(|v| v.into_iter().map(|(k, _)| k).collect())
            .map_err(|e| format!("{e:#}"));
        let rd = PerpsReads {
            position: pos,
            lamports,
            usdc_raw,
            keeper_requests,
        };
        let counter = random_counter()?;
        let (checks, ixs, details) = self.plan(&rd, counter);
        let change = if self.action == Action::Increase {
            RequestChange::Increase
        } else {
            RequestChange::Decrease
        };
        let request = position_request_pda(&position, counter, change);
        *self.request.lock().unwrap() = Some((request, counter));
        let stale = vec![
            format!("jup_perps/1:{w}"),
            format!("lp_snapshot/1:{w}:{}", self.pool),
            format!("solana_wallet/1:{w}"),
            format!("acct/1:{w}"),
            format!("acct/1:{position}"),
            format!("acct/1:{usdc_ata}"),
            format!("acct/1:{}", ata(&w, &ids::key(ids::WSOL), &token)),
        ];
        Ok(Built {
            checks,
            plans: vec![TxPlan::new(&self.action_name(), ixs)],
            details,
            stale_keys: stale,
            independent: false,
        })
    }

    async fn after_send(&self, result: &WriteResult, shared: &SolanaShared) -> Option<String> {
        if result.status != WriteStatus::Confirmed {
            return Some("last_hedge_action NOT recorded: the request did not land".into());
        }
        let (request, counter) = (*self.request.lock().unwrap())?;
        let signatures: Vec<String> = result
            .txs
            .iter()
            .filter_map(|t| t.signature.clone())
            .collect();
        let action = self.action_name();
        let now = now_ms();
        Some(
            merge_lp_state(
                shared.store.as_ref(),
                names::JUP_PERPS_ORDER,
                &self.wallet,
                &self.pool,
                |s| {
                    s.last_hedge_action = Some(HedgeActionRecord {
                        at_ms: now,
                        action: action.clone(),
                        live: true,
                        signatures: signatures.clone(),
                        position_request: Some(request.to_string()),
                        counter: Some(counter),
                    })
                },
            )
            .await,
        )
    }
}

impl PerpsBuilder {
    /// Pure: checks, instructions, details for `counter`.
    fn plan(&self, rd: &PerpsReads, counter: u64) -> (Vec<Check>, Vec<Instruction>, Value) {
        let w = self.wallet;
        let token = ids::key(ids::TOKEN);
        let position = jup_position_pda(&w, self.side == Side::Long);
        let mut checks = Vec::new();
        let oracle = self.oracle.filter(|o| o.is_finite() && *o > 0.0);
        checks.push(Check::new(
            "oracle",
            oracle.is_some(),
            match oracle {
                Some(o) => format!("SOL/USD {o}"),
                None => "no SOL/USD oracle price — the keeper bound cannot be set".into(),
            },
        ));
        checks.push(match &rd.keeper_requests {
            Ok(v) if v.is_empty() => Check::new("keeper_requests", true, "no open keeper request"),
            Ok(v) => Check::new(
                "keeper_requests",
                false,
                format!(
                    "open keeper request(s) {} — wait for the keeper before another order",
                    v.iter()
                        .map(Pubkey::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            Err(e) => Check::new("keeper_requests", false, format!("unreadable: {e}")),
        });
        let (size_now, collateral_now) = rd.position.unwrap_or((0, 0));
        let size_delta = (self.size_usd * USD_PRECISION).round() as u64;
        match self.action {
            Action::Increase => {
                let post = (size_now + size_delta) as f64 / USD_PRECISION;
                checks.push(Check::new(
                    "max_notional",
                    post <= self.max_notional_usd,
                    format!(
                        "post-order {} size {post} USD (max {})",
                        self.side.as_str(),
                        self.max_notional_usd
                    ),
                ));
                let (ok, detail) = match self.side {
                    Side::Long => {
                        let need =
                            (self.collateral * 1e9).round() as u64 + LONG_SOL_BUFFER_LAMPORTS;
                        (
                            rd.lamports >= need,
                            format!(
                                "wallet {} SOL; needs {} (collateral + 0.02 buffer)",
                                rd.lamports as f64 / 1e9,
                                need as f64 / 1e9
                            ),
                        )
                    }
                    Side::Short => {
                        let need = (self.collateral * 1e6).round() as u64;
                        (
                            rd.usdc_raw >= need,
                            format!("USDC ATA holds {} raw; needs {need}", rd.usdc_raw),
                        )
                    }
                };
                checks.push(Check::new("funds", ok, detail));
            }
            Action::Decrease | Action::Close => {
                checks.push(Check::new(
                    "position",
                    rd.position.is_some(),
                    match rd.position {
                        Some((s, c)) => format!(
                            "{} position size {} USD, collateral {} USD",
                            self.side.as_str(),
                            s as f64 / USD_PRECISION,
                            c as f64 / USD_PRECISION
                        ),
                        None => format!("no open {} position", self.side.as_str()),
                    },
                ));
                if self.action == Action::Decrease {
                    checks.push(Check::new(
                        "decrease_size",
                        size_delta <= size_now,
                        format!(
                            "decrease {} USD of {} (use close for all)",
                            self.size_usd,
                            size_now as f64 / USD_PRECISION
                        ),
                    ));
                }
            }
        }
        let bound = oracle.map(|o| self.price_bound(o)).unwrap_or(0);
        let mint = collateral_mint(self.side);
        let funding = ata(&w, &mint, &token);
        let mut ixs = Vec::new();
        match self.action {
            Action::Increase => {
                let collateral_raw = match self.side {
                    Side::Long => (self.collateral * 1e9).round() as u64,
                    Side::Short => (self.collateral * 1e6).round() as u64,
                };
                if self.side == Side::Long {
                    ixs.push(ata_create_idempotent(&w, &funding, &w, &mint, &token));
                    ixs.push(system_transfer(&w, &funding, collateral_raw));
                    ixs.push(spl_sync_native(&funding));
                }
                ixs.push(increase_market_request(
                    &w,
                    &position,
                    &IncreaseParams {
                        size_usd_delta: size_delta,
                        collateral_token_delta: collateral_raw,
                        side: self.side,
                        price_slippage: bound,
                        jupiter_minimum_out: None,
                        counter,
                    },
                ));
            }
            Action::Decrease | Action::Close => {
                if self.side == Side::Long {
                    // The keeper pays into it; never closed here.
                    ixs.push(ata_create_idempotent(&w, &funding, &w, &mint, &token));
                }
                let entire = self.action == Action::Close;
                ixs.push(decrease_market_request(
                    &w,
                    &position,
                    self.side,
                    &DecreaseParams {
                        collateral_usd_delta: if entire {
                            0
                        } else {
                            (self.collateral * USD_PRECISION).round() as u64
                        },
                        size_usd_delta: if entire { 0 } else { size_delta },
                        price_slippage: bound,
                        jupiter_minimum_out: None,
                        entire_position: entire.then_some(true),
                        counter,
                    },
                ));
            }
        }
        let change = if self.action == Action::Increase {
            RequestChange::Increase
        } else {
            RequestChange::Decrease
        };
        let details = json!({
            "action": self.action_name(),
            "position": position,
            "position_request": position_request_pda(&position, counter, change),
            "counter": counter,
            "size_usd_delta": size_delta,
            "price_bound_usd": bound as f64 / USD_PRECISION,
            "oracle_sol_usd": oracle,
            "position_before": rd.position.map(|(s, c)| json!({"size_usd": s as f64 / USD_PRECISION, "collateral_usd": c as f64 / USD_PRECISION})),
            "collateral_before_usd": collateral_now as f64 / USD_PRECISION,
        });
        (checks, ixs, details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lp::perps_ix::{DECREASE_MARKET, INCREASE_MARKET};

    const W: &str = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";
    const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";

    fn b(extra: Value) -> PerpsBuilder {
        let mut a = json!({"wallet": W, "pool": POOL, "side": "short", "action": "increase",
            "size_usd": 100.0, "collateral": 30.0, "slippage_bps": 50, "max_notional_usd": 500});
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        let mut b = PerpsBuilder::parse(&a).unwrap();
        b.oracle = Some(120.0);
        b
    }

    fn reads() -> PerpsReads {
        PerpsReads {
            position: Some((200_000_000, 50_000_000)),
            lamports: 2_000_000_000,
            usdc_raw: 100_000_000,
            keeper_requests: Ok(vec![]),
        }
    }

    fn failed(c: &[Check]) -> Vec<String> {
        c.iter().filter(|c| !c.ok).map(|c| c.name.clone()).collect()
    }

    /// Live, keyless: simulate a small short increase (and a close when a
    /// short is open) as the operator wallet on mainnet. `cargo test --bin
    /// tengu -- --ignored live_perps_order --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_perps_order_simulate() {
        use crate::adapters::outbound::solana::http_json::request_json;
        use crate::adapters::outbound::solana::rpc::REQUEST_TIMEOUT;
        use crate::adapters::outbound::tools::solana::write_common::run_write_with;
        use crate::domain::scope::ToolScope;
        use crate::domain::solana_write::WriteMode;
        let scope = ToolScope {
            net_hosts: vec!["lite-api.jup.ag".into()],
            ..Default::default()
        };
        let http = crate::adapters::outbound::egress::policy()
            .tool_client(REQUEST_TIMEOUT)
            .unwrap();
        let (_, price) = request_json(
            &http,
            &scope,
            &format!("https://lite-api.jup.ag/price/v3?ids={}", ids::WSOL),
            None,
            REQUEST_TIMEOUT,
        )
        .await
        .unwrap();
        let oracle = price[ids::WSOL]["usdPrice"].as_f64();
        let wallet = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
        for args in [
            json!({"wallet": wallet, "pool": POOL, "side": "short", "action": "increase",
                   "size_usd": 15.0, "collateral": 5.0, "slippage_bps": 50, "max_notional_usd": 10000}),
            json!({"wallet": wallet, "pool": POOL, "side": "short", "action": "close",
                   "slippage_bps": 50, "max_notional_usd": 10000}),
        ] {
            let mut bb = PerpsBuilder::parse(&args).unwrap();
            bb.oracle = oracle;
            let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
            let out = run_write_with(
                rpc,
                &ToolScope::default(),
                &SolanaShared::default(),
                names::JUP_PERPS_ORDER,
                bb.wallet,
                WriteMode::Simulate,
                &bb,
            )
            .await;
            println!(
                "== {} → {} {:?}",
                bb.action_name(),
                out.status.as_str(),
                out.refused
            );
            println!("{}", serde_json::to_string(&out.details).unwrap());
            for t in &out.txs {
                println!(
                    "{} {:?} units={:?} size={:?} err={:?}\n{:#?}",
                    t.label, t.status, t.units, t.tx_size, t.err, t.logs_tail
                );
            }
            let refused_for_state = out.refused.as_deref().is_some_and(|r| {
                r.starts_with("position")
                    || r.starts_with("funds")
                    || r.starts_with("keeper_requests")
            });
            assert!(
                out.status == WriteStatus::Simulated || refused_for_state,
                "{:?} {:?}",
                out.refused,
                out.txs
            );
        }
    }

    #[test]
    fn price_bounds_follow_the_bot() {
        let sell = b(json!({}));
        assert_eq!(
            sell.price_bound(120.0),
            119_400_000,
            "short increase = floor"
        );
        let buy = b(json!({"side": "long"}));
        assert_eq!(
            buy.price_bound(120.0),
            120_600_000,
            "long increase = ceiling"
        );
        assert_eq!(
            b(json!({"action": "decrease"})).price_bound(120.0),
            120_600_000,
            "short decrease = ceiling"
        );
        assert_eq!(
            b(json!({"side": "long", "action": "close"})).price_bound(120.0),
            119_400_000,
            "long close = floor"
        );
    }

    #[test]
    fn short_increase_is_one_request() {
        let bb = b(json!({}));
        let (checks, ixs, details) = bb.plan(&reads(), 7);
        assert!(failed(&checks).is_empty(), "{checks:?}");
        assert_eq!(ixs.len(), 1);
        assert_eq!(ixs[0].data[..8], INCREASE_MARKET);
        assert_eq!(
            details["position_request"],
            position_request_pda(
                &jup_position_pda(&bb.wallet, false),
                7,
                RequestChange::Increase
            )
            .to_string()
        );
    }

    #[test]
    fn long_increase_wraps_and_long_close_keeps_the_wsol_account() {
        let (_, ixs, _) = b(json!({"side": "long", "collateral": 0.25})).plan(&reads(), 7);
        let programs: Vec<String> = ixs.iter().map(|i| i.program_id.to_string()).collect();
        assert_eq!(
            programs,
            vec![ids::ATA, ids::SYSTEM, ids::TOKEN, ids::JUP_PERPS]
        );
        assert_eq!(ixs[1].data[4..12], 250_000_000u64.to_le_bytes());
        let (checks, ixs, _) = b(json!({"side": "long", "action": "close"})).plan(&reads(), 7);
        assert!(failed(&checks).is_empty());
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id.to_string(), ids::ATA);
        assert_eq!(ixs[1].data[..8], DECREASE_MARKET);
        assert!(ixs.iter().all(|i| i.data != vec![9]), "no CloseAccount");
    }

    #[test]
    fn refusals() {
        let run = |extra: Value, f: &dyn Fn(&mut PerpsReads)| {
            let mut r = reads();
            f(&mut r);
            failed(&b(extra).plan(&r, 7).0)
        };
        assert_eq!(run(json!({"size_usd": 400}), &|_| {}), vec!["max_notional"]);
        assert_eq!(run(json!({}), &|r| r.usdc_raw = 29_999_999), vec!["funds"]);
        assert_eq!(
            run(json!({}), &|r| r.keeper_requests =
                Ok(vec![Pubkey([3; 32])])),
            vec!["keeper_requests"]
        );
        assert_eq!(
            run(json!({}), &|r| r.keeper_requests = Err("rpc down".into())),
            vec!["keeper_requests"]
        );
        assert_eq!(
            run(json!({"action": "close"}), &|r| r.position = None),
            vec!["position"]
        );
        assert_eq!(
            run(json!({"action": "decrease", "size_usd": 250}), &|_| {}),
            vec!["decrease_size"]
        );
        let mut no_oracle = b(json!({}));
        no_oracle.oracle = None;
        assert_eq!(failed(&no_oracle.plan(&reads(), 7).0), vec!["oracle"]);
    }

    #[test]
    fn args_are_required() {
        let base = json!({"wallet": W, "pool": POOL, "side": "short", "action": "increase",
            "size_usd": 100.0, "collateral": 30.0, "slippage_bps": 50, "max_notional_usd": 500});
        assert!(PerpsBuilder::parse(&base).is_ok());
        for (k, v) in [
            ("max_notional_usd", json!(0)),
            ("slippage_bps", json!(0)),
            ("side", json!("up")),
            ("action", json!("flip")),
            ("collateral", json!(0)),
        ] {
            let mut a = base.clone();
            a[k] = v;
            assert!(PerpsBuilder::parse(&a).is_err(), "{k}");
        }
        let close = json!({"wallet": W, "pool": POOL, "side": "short", "action": "close",
            "slippage_bps": 50, "max_notional_usd": 500});
        assert!(PerpsBuilder::parse(&close).is_ok(), "close needs no size");
    }
}
