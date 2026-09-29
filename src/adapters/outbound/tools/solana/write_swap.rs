//! `jupiter_swap` — swap through Jupiter Ultra (bot `jupiterSwapper.ts`):
//! `GET /ultra/v1/order` builds the transaction (taker = the wallet), we
//! simulate it as-is, sign our slot only, and `POST /ultra/v1/execute`
//! lands it. Send path: `SendSession::send_prebuilt` + [`UltraSubmitter`].
//!
//! | Rule | Detail |
//! |---|---|
//! | Pairs | any pair simulates; `send` only SOL↔USDC (wSOL / USDC mints) |
//! | Oracle gate | SOL↔USDC: the order's WORST fill (`otherAmountThreshold`) implies a SOL price within `oracle_gate_bps` of the oracle (`gates::check_swap_oracle_gate`; oracle = `price_oracle/1:<wSOL>` row ≤ 10 s, else Jupiter live). No oracle or no amounts ⇒ refused (the bot skipped the gate) |
//! | Fee payer | the wallet must be signature slot 0; a gasless order (Jupiter pays) is refused for `send` |
//! | Keeper requests | an open Jupiter perps request of the wallet ⇒ `send` refused (its keeper pays into the SOL / USDC accounts) |
//! | `/execute` | same `signedTransaction` + `requestId` re-POSTed on a transport failure inside Jupiter's 2-minute idempotency window; `Success` = landed at its slot; `-1/-2/-3/-1002/-1003/-1004` = not sent; anything else = unknown ⇒ poll our RPC until the blockhash expires |
//! | Hosts | `lite-api.jup.ag` (+ the RPC) |

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};

use super::price::require_pubkey;
use super::write_common::{live_keeper_requests, parse_mode, signer_for};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::http_json::{post_json, request_json};
use crate::adapters::outbound::solana::plan::oracle_usd;
use crate::adapters::outbound::solana::rpc::{SolanaRpc, REQUEST_TIMEOUT};
use crate::adapters::outbound::solana::send::{Pipeline, Submitted, Submitter};
use crate::domain::lp::gates::{check_swap_oracle_gate, SwapDirection, SwapOracleGateInput};
use crate::domain::lp::wallet::decode_mint;
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, ObsSource, Observation};
use crate::domain::scope::ToolScope;
use crate::domain::solana::{ata, ids, Pubkey};
use crate::domain::solana_tx::Transaction;
use crate::domain::solana_write::{Check, TxReport, WriteMode, WriteResult, WriteStatus};
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) const ULTRA_BASE: &str = "https://lite-api.jup.ag/ultra/v1";
/// Jupiter: the same signed tx + requestId may be re-submitted "for up to
/// two minutes ... without risking a double execution".
const EXECUTE_WINDOW: Duration = Duration::from_secs(110);
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(60);
/// `/execute` codes that mean the transaction was not sent.
const NOT_SENT_CODES: [i64; 6] = [-1, -2, -3, -1002, -1003, -1004];

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SwapTool {
        def: defs::def(names::JUPITER_SWAP),
        shared: shared.clone(),
    })]
}

// ---------------------------------------------------------------------------
// Jupiter Ultra client
// ---------------------------------------------------------------------------

#[async_trait]
pub(crate) trait UltraApi: Send + Sync {
    async fn order(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_raw: u64,
        taker: &Pubkey,
    ) -> Result<Value>;
    async fn execute_order(&self, signed_b64: &str, request_id: &str) -> Result<Value>;
}

pub(crate) struct HttpUltra {
    pub http: reqwest::Client,
    pub scope: ToolScope,
    pub base: String,
}

#[async_trait]
impl UltraApi for HttpUltra {
    async fn order(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_raw: u64,
        taker: &Pubkey,
    ) -> Result<Value> {
        let url = format!(
            "{}/order?inputMint={input}&outputMint={output}&amount={amount_raw}&taker={taker}",
            self.base
        );
        Ok(
            request_json(&self.http, &self.scope, &url, None, REQUEST_TIMEOUT)
                .await?
                .1,
        )
    }
    async fn execute_order(&self, signed_b64: &str, request_id: &str) -> Result<Value> {
        let body = json!({"signedTransaction": signed_b64, "requestId": request_id});
        let url = format!("{}/execute", self.base);
        Ok(
            post_json(&self.http, &self.scope, &url, &body, EXECUTE_TIMEOUT)
                .await?
                .1,
        )
    }
}

/// `/execute` as a [`Submitter`] (see the module table).
pub(crate) struct UltraSubmitter<'a> {
    pub api: &'a dyn UltraApi,
    pub request_id: String,
    pub window: Duration,
    pub pause: Duration,
}

/// Classify one `/execute` answer.
pub(crate) fn classify_execute(v: &Value) -> Submitted {
    let code = v["code"].as_i64();
    let error = v["error"].as_str().unwrap_or("").to_string();
    match v["status"].as_str() {
        Some("Success") => match v["slot"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v["slot"].as_u64())
        {
            Some(slot) => Submitted::Landed {
                slot,
                err: None,
                note: Some(format!(
                    "Ultra: in {} out {}",
                    v["inputAmountResult"].as_str().unwrap_or("?"),
                    v["outputAmountResult"].as_str().unwrap_or("?")
                )),
            },
            None => Submitted::Unknown {
                note: "Ultra: Success without a slot".into(),
            },
        },
        Some("Failed") if code.is_some_and(|c| NOT_SENT_CODES.contains(&c)) => {
            Submitted::Rejected {
                note: format!("Ultra /execute code {}: {error}", code.unwrap_or_default()),
                err: None,
                logs: Vec::new(),
            }
        }
        _ => Submitted::Unknown {
            note: format!(
                "Ultra /execute status {} code {}: {error}",
                v["status"].as_str().unwrap_or("?"),
                code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
            ),
        },
    }
}

#[async_trait]
impl Submitter for UltraSubmitter<'_> {
    async fn submit(&self, signed: &[u8]) -> Submitted {
        let b64 = B64.encode(signed);
        let started = Instant::now();
        loop {
            let last = match self.api.execute_order(&b64, &self.request_id).await {
                Ok(v) => return classify_execute(&v),
                Err(e) => format!("{e:#}"),
            };
            if started.elapsed() + self.pause >= self.window {
                return Submitted::Unknown {
                    note: format!(
                        "Ultra /execute unanswered within {} s: {last}",
                        self.window.as_secs()
                    ),
                };
            }
            tokio::time::sleep(self.pause).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Request + order parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SwapRequest {
    pub wallet: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// Input token, UI units.
    pub amount: f64,
    pub oracle_gate_bps: f64,
    pub mode: WriteMode,
}

fn positive(args: &Value, key: &str) -> Result<f64> {
    let tool = names::JUPITER_SWAP;
    let v = args
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{tool}: '{key}' is required (number)"))?;
    if !(v.is_finite() && v > 0.0) {
        return Err(anyhow!("{tool}: '{key}' must be > 0, got {v}"));
    }
    Ok(v)
}

impl SwapRequest {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let tool = names::JUPITER_SWAP;
        let input_mint = require_pubkey(args, tool, "input_mint")?;
        let output_mint = require_pubkey(args, tool, "output_mint")?;
        if input_mint == output_mint {
            return Err(anyhow!("{tool}: input_mint and output_mint are the same"));
        }
        Ok(SwapRequest {
            wallet: require_pubkey(args, tool, "wallet")?,
            input_mint,
            output_mint,
            amount: positive(args, "amount")?,
            oracle_gate_bps: positive(args, "oracle_gate_bps")?,
            mode: parse_mode(args, tool)?,
        })
    }

    fn direction(&self) -> Option<SwapDirection> {
        let (wsol, usdc) = (ids::key(ids::WSOL), ids::key(ids::USDC));
        match (self.input_mint, self.output_mint) {
            (i, o) if i == wsol && o == usdc => Some(SwapDirection::SolToUsdc),
            (i, o) if i == usdc && o == wsol => Some(SwapDirection::UsdcToSol),
            _ => None,
        }
    }
}

fn u64_str(v: &Value, key: &str) -> Option<u64> {
    v[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v[key].as_u64())
}

/// Checks on an Ultra order; `oracle` = SOL/USD.
pub(crate) fn order_checks(
    req: &SwapRequest,
    order: &Value,
    amount_raw: u64,
    decimals: (u8, u8),
    oracle: Option<f64>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let tx_ok = order["transaction"].as_str().is_some_and(|t| !t.is_empty());
    checks.push(Check::new(
        "order",
        tx_ok,
        if tx_ok {
            format!("request {}", order["requestId"].as_str().unwrap_or("?"))
        } else {
            format!(
                "no transaction: errorCode {} {}",
                order["errorCode"],
                order["errorMessage"].as_str().unwrap_or("")
            )
        },
    ));
    let taker_ok = order["taker"].as_str() == Some(&req.wallet.to_string());
    checks.push(Check::new(
        "taker",
        taker_ok,
        format!("order taker {} (wallet {})", order["taker"], req.wallet),
    ));
    let in_raw = u64_str(order, "inAmount");
    checks.push(Check::new(
        "in_amount",
        in_raw == Some(amount_raw),
        format!("order inAmount {in_raw:?}, requested {amount_raw}"),
    ));
    let Some(direction) = req.direction() else {
        checks.push(Check::new(
            "oracle_gate",
            true,
            "not applicable (not SOL↔USDC) — this pair only simulates",
        ));
        return checks;
    };
    let min_out = u64_str(order, "otherAmountThreshold");
    let (Some(min_out), Some(oracle)) = (min_out, oracle) else {
        checks.push(Check::new(
            "oracle_gate",
            false,
            format!("missing input: otherAmountThreshold {min_out:?}, oracle {oracle:?}"),
        ));
        return checks;
    };
    let scale = |raw: u64, d: u8| raw as f64 / 10f64.powi(i32::from(d));
    let gate = check_swap_oracle_gate(&SwapOracleGateInput {
        direction,
        input_amount: scale(amount_raw, decimals.0),
        output_amount: scale(min_out, decimals.1),
        oracle_price_usd: oracle,
        tolerance_bps: req.oracle_gate_bps,
    });
    checks.push(Check::new(
        "oracle_gate",
        gate.ok,
        format!(
            "worst fill implies {:?} USD/SOL vs oracle {oracle} ({:?} bps, max {})",
            gate.implied_price_usd, gate.deviation_bps, req.oracle_gate_bps
        ),
    ));
    checks
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

struct SwapTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for SwapTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = SwapRequest::parse(args)?;
        let rpc = Arc::new(SolanaRpc::from_ctx(ctx)?);
        let ultra = HttpUltra {
            http: ctx.http.clone(),
            scope: ctx.scope.clone(),
            base: ULTRA_BASE.to_string(),
        };
        let oracle = match req.direction() {
            Some(_) => oracle_usd(ctx, self.shared.store.as_deref(), ids::WSOL, now_ms()).await,
            None => None,
        };
        let r = swap_with(rpc, &ultra, oracle, ctx.scope, &self.shared, &req).await;
        let now = now_ms();
        Ok(ToolOutput::observed(
            Observation::of(names::JUPITER_SWAP, &r, now, 0, ObsSource::Live),
            now,
        ))
    }
}

async fn mint_decimals(rpc: &SolanaRpc, mint: &Pubkey) -> Result<u8> {
    if *mint == ids::key(ids::WSOL) {
        return Ok(9);
    }
    if *mint == ids::key(ids::USDC) {
        return Ok(6);
    }
    let (_, reads) = rpc.get_multiple_accounts(&[*mint], None).await?;
    let read = reads
        .first()
        .ok_or_else(|| anyhow!("mint {mint}: no read"))?;
    let (Some(owner), Some(data)) = (read.owner(), read.data()) else {
        return Err(anyhow!("mint {mint} does not exist"));
    };
    decode_mint(owner, &data)
        .map(|m| m.decimals)
        .map_err(|e| anyhow!("mint {mint}: {}", e.message))
}

/// The tool over explicit collaborators (tests inject all of them).
pub(crate) async fn swap_with(
    rpc: Arc<SolanaRpc>,
    ultra: &dyn UltraApi,
    oracle: Option<f64>,
    scope: &ToolScope,
    shared: &SolanaShared,
    req: &SwapRequest,
) -> WriteResult {
    let tool = names::JUPITER_SWAP;
    let wallet = req.wallet;
    let mut r = WriteResult::new(tool, &wallet.to_string(), req.mode, now_ms(), rpc.host());
    if req.mode == WriteMode::Send && req.direction().is_none() {
        return r.refuse("pair_not_allowed: send swaps only SOL↔USDC (other pairs simulate)");
    }
    let decimals = match tokio::try_join!(
        mint_decimals(&rpc, &req.input_mint),
        mint_decimals(&rpc, &req.output_mint)
    ) {
        Ok(d) => d,
        Err(e) => return r.refuse(format!("read_failed: {e:#}")),
    };
    let amount_raw = (req.amount * 10f64.powi(i32::from(decimals.0))).floor();
    if !(amount_raw >= 1.0 && amount_raw < u64::MAX as f64) {
        return r.refuse(format!("amount {} is below one base unit", req.amount));
    }
    let amount_raw = amount_raw as u64;
    let pipeline = Pipeline::new(rpc.clone(), tool, wallet);

    // `send`: signer + lease (+ earlier send resolved) before the order.
    let mut session = None;
    if req.mode == WriteMode::Send {
        let signer = match signer_for(scope, shared.signer_key_file.as_ref(), &wallet, tool) {
            Ok(s) => s,
            Err(reason) => return r.refuse(reason),
        };
        let Some(store) = shared.writes.clone() else {
            return r.refuse(
                "write_store_unavailable: <TENGU_HOME>/state/solana-writes.db could not be opened",
            );
        };
        match pipeline.open_session(store, signer).await {
            Ok(s) => {
                r.pending_resolved = s.pending_resolved.clone();
                session = Some(s);
            }
            Err(reason) => return r.refuse(reason),
        }
    }

    let result = async {
        if req.mode == WriteMode::Send {
            match live_keeper_requests(&rpc, &wallet, now_ms() / 1000).await {
                Ok(open) => r.checks.push(Check::new(
                    "keeper_requests",
                    open.is_empty(),
                    if open.is_empty() {
                        "no open Jupiter perps keeper request".to_string()
                    } else {
                        format!(
                            "open keeper request(s) {} — a SOL-leg swap could touch the account the keeper pays into",
                            open.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>().join(", ")
                        )
                    },
                )),
                Err(e) => r.checks.push(Check::new("keeper_requests", false, format!("unreadable: {e:#}"))),
            }
        }
        let order = match ultra.order(&req.input_mint, &req.output_mint, amount_raw, &wallet).await {
            Ok(o) => o,
            Err(e) => return Err(format!("order_failed: {e:#}")),
        };
        r.details = json!({
            "amount_raw": amount_raw,
            "decimals": [decimals.0, decimals.1],
            "oracle_sol_usd": oracle,
            "order": {
                "requestId": order["requestId"], "inAmount": order["inAmount"],
                "outAmount": order["outAmount"], "otherAmountThreshold": order["otherAmountThreshold"],
                "slippageBps": order["slippageBps"], "priceImpactPct": order["priceImpactPct"],
                "router": order["router"], "gasless": order["gasless"], "feeBps": order["feeBps"],
                "prioritizationFeeLamports": order["prioritizationFeeLamports"],
            },
        });
        r.checks.extend(order_checks(req, &order, amount_raw, decimals, oracle));
        Ok(order)
    }
    .await;

    let order = match result {
        Ok(o) => o,
        Err(reason) => {
            r = r.refuse(reason);
            if let Some(s) = session {
                s.close(None, &[]).await;
            }
            return r;
        }
    };
    if let Some(c) = r.failed_check() {
        let reason = format!("{}: {}", c.name, c.detail);
        r = r.refuse(reason);
        if let Some(s) = session {
            s.close(None, &[]).await;
        }
        return r;
    }
    let tx = match order["transaction"].as_str().map(|t| B64.decode(t)) {
        Some(Ok(b)) => b,
        _ => return r.refuse("order transaction is not base64"),
    };

    match session {
        None => {
            let mut t = TxReport::new("swap");
            t.tx_size = Some(tx.len());
            match Transaction::parse(&tx) {
                Err(e) => {
                    t.status = Some(WriteStatus::SimFailed);
                    t.note = Some(format!("order transaction does not parse: {e}"));
                }
                Ok(_) => match rpc.simulate_transaction(&tx).await {
                    Ok(sim) => {
                        t.units = sim.units;
                        t.slot = Some(sim.slot);
                        t.logs_tail = crate::domain::solana_write::logs_tail(&sim.logs);
                        t.status = Some(if sim.err.is_none() {
                            WriteStatus::Simulated
                        } else {
                            WriteStatus::SimFailed
                        });
                        t.err = sim.err;
                    }
                    Err(e) => {
                        t.status = Some(WriteStatus::SimFailed);
                        t.note = Some(format!("{e:#}"));
                    }
                },
            }
            r.txs.push(t);
        }
        Some(mut s) => {
            let submitter = UltraSubmitter {
                api: ultra,
                request_id: order["requestId"].as_str().unwrap_or_default().to_string(),
                window: EXECUTE_WINDOW,
                pause: Duration::from_secs(2),
            };
            let t = s.send_prebuilt("swap", &tx, &submitter).await;
            let sent = t.signature.is_some();
            r.txs.push(t);
            let token = ids::key(ids::TOKEN);
            let stale = [
                format!("solana_wallet/1:{wallet}"),
                format!("acct/1:{wallet}"),
                format!("acct/1:{}", ata(&wallet, &req.input_mint, &token)),
                format!("acct/1:{}", ata(&wallet, &req.output_mint, &token)),
            ];
            s.close(shared.store.as_ref(), if sent { &stale } else { &[] })
                .await;
        }
    }
    r.settle();
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::adapters::outbound::solana::signer::LocalKeypair;
    use crate::adapters::outbound::solana::test_chain::Chain;
    use crate::adapters::outbound::solana::writes_store::SqliteWriteStore;
    use crate::domain::solana::AccountRead;
    use crate::domain::solana_tx::{system_transfer, LegacyMessage};
    use crate::ports::solana_signer::SolanaSigner;

    const ORACLE: f64 = 118.6;

    fn wallet() -> LocalKeypair {
        LocalKeypair::from_seed(&[1; 32])
    }

    /// An unsigned tx with `payer` as fee payer (and the wallet as a
    /// second signer when they differ — Ultra's gasless shape).
    fn order_tx(payer: &Pubkey) -> String {
        let w = wallet().pubkey();
        let mut ix = system_transfer(&w, &Pubkey([9; 32]), 1);
        if payer != &w {
            ix.accounts
                .push(crate::domain::solana_tx::AccountMeta::readonly(
                    *payer, true,
                ));
        }
        let msg = LegacyMessage::compile(payer, &[ix], [7; 32]).unwrap();
        B64.encode(Transaction::unsigned(&msg).serialize())
    }

    struct FakeUltra {
        order: Value,
        execute: Mutex<Vec<Result<Value, String>>>,
        executed: Mutex<Vec<String>>,
    }

    impl FakeUltra {
        fn new(min_out: u64, payer: Option<Pubkey>) -> Self {
            let w = wallet().pubkey();
            FakeUltra {
                order: json!({
                    "requestId": "req-1", "taker": w.to_string(), "inAmount": "10000000",
                    "outAmount": "1186399", "otherAmountThreshold": min_out.to_string(),
                    "slippageBps": 38, "gasless": payer.is_some(), "router": "metis",
                    "transaction": order_tx(&payer.unwrap_or(w)),
                }),
                execute: Mutex::new(vec![]),
                executed: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl UltraApi for FakeUltra {
        async fn order(&self, _: &Pubkey, _: &Pubkey, amount: u64, _: &Pubkey) -> Result<Value> {
            assert_eq!(amount, 10_000_000);
            Ok(self.order.clone())
        }
        async fn execute_order(&self, signed_b64: &str, request_id: &str) -> Result<Value> {
            assert_eq!(request_id, "req-1");
            self.executed.lock().unwrap().push(signed_b64.to_string());
            let mut q = self.execute.lock().unwrap();
            if q.is_empty() {
                return Err(anyhow!("fake: no execute answer scripted"));
            }
            q.remove(0).map_err(|e| anyhow!(e))
        }
    }

    fn req(mode: WriteMode) -> SwapRequest {
        SwapRequest {
            wallet: wallet().pubkey(),
            input_mint: ids::key(ids::WSOL),
            output_mint: ids::key(ids::USDC),
            amount: 0.01,
            oracle_gate_bps: 50.0,
            mode,
        }
    }

    struct Rig {
        chain: Arc<Chain>,
        _dir: tempfile::TempDir,
        shared: SolanaShared,
    }

    fn rig() -> Rig {
        let chain = Arc::new(Chain::new());
        chain.route(|b| {
            (b["method"] == "getProgramAccounts")
                .then(|| json!({"context": {"slot": 500}, "value": []}))
        });
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let p = dir.path().join("signer.json");
        let mut bytes = vec![1u8; 32];
        bytes.extend_from_slice(&wallet().pubkey().0);
        std::fs::write(&p, serde_json::to_vec(&bytes).unwrap()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let shared = SolanaShared {
            store: None,
            writes: Some(Arc::new(
                SqliteWriteStore::open(&dir.path().join("state")).unwrap(),
            )),
            signer_key_file: Some(p),
        };
        Rig {
            chain,
            _dir: dir,
            shared,
        }
    }

    fn granted() -> ToolScope {
        ToolScope {
            wallets: vec![wallet().pubkey().to_string()],
            ..Default::default()
        }
    }

    impl Rig {
        async fn run(&self, u: &FakeUltra, oracle: Option<f64>, req: &SwapRequest) -> WriteResult {
            let rpc = Arc::new(SolanaRpc::with_transport(self.chain.clone(), "fake.rpc"));
            swap_with(rpc, u, oracle, &granted(), &self.shared, req).await
        }
    }

    #[tokio::test]
    async fn simulate_gates_on_the_worst_fill() {
        let r = rig();
        // 0.01 SOL → min 1.181890 USDC ⇒ 118.189 USD/SOL, 34.5 bps under 118.6.
        let out = r
            .run(
                &FakeUltra::new(1_181_890, None),
                Some(ORACLE),
                &req(WriteMode::Simulate),
            )
            .await;
        assert_eq!(
            out.status,
            WriteStatus::Simulated,
            "{:?} {:?}",
            out.refused,
            out.checks
        );
        assert_eq!(out.details["order"]["requestId"], "req-1");
        // 1.17 USDC min ⇒ 117.0 ⇒ 135 bps: refused.
        let out = r
            .run(
                &FakeUltra::new(1_170_000, None),
                Some(ORACLE),
                &req(WriteMode::Simulate),
            )
            .await;
        assert_eq!(out.status, WriteStatus::Refused);
        assert!(
            out.refused.as_deref().unwrap().starts_with("oracle_gate"),
            "{:?}",
            out.refused
        );
        let out = r
            .run(
                &FakeUltra::new(1_181_890, None),
                None,
                &req(WriteMode::Simulate),
            )
            .await;
        assert!(out
            .refused
            .as_deref()
            .unwrap()
            .starts_with("oracle_gate: missing input"));
    }

    #[tokio::test]
    async fn send_lands_through_execute_and_raises_the_fence() {
        let r = rig();
        let u = FakeUltra::new(1_181_890, None);
        u.execute.lock().unwrap().push(Err("timeout".into()));
        u.execute
            .lock()
            .unwrap()
            .push(Ok(json!({"status": "Success", "code": 0, "slot": "901",
            "signature": "x", "inputAmountResult": "10000000", "outputAmountResult": "1186000"})));
        let out = r.run(&u, Some(ORACLE), &req(WriteMode::Send)).await;
        assert_eq!(
            out.status,
            WriteStatus::Confirmed,
            "{:?} {:?}",
            out.refused,
            out.txs
        );
        assert_eq!(out.txs[0].slot, Some(901));
        let sent = u.executed.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "re-POSTed once");
        assert_eq!(sent[0], sent[1], "same signed bytes");
        let (tx, view) = Transaction::parse(&B64.decode(&sent[0]).unwrap()).unwrap();
        assert_eq!(view.signers()[0], wallet().pubkey());
        assert_eq!(tx.signatures[0], wallet().sign(&tx.message).0);
        let w = wallet().pubkey().to_string();
        let writes = r.shared.writes.clone().unwrap();
        assert_eq!(writes.fence(&w).await.unwrap(), Some(901));
        assert_eq!(writes.pending(&w).await.unwrap(), None);
        assert!(
            !r.chain.methods().contains(&"sendTransaction".to_string()),
            "Jupiter lands it"
        );
    }

    #[tokio::test]
    async fn not_sent_codes_are_failed_and_cleared() {
        let r = rig();
        let u = FakeUltra::new(1_181_890, None);
        u.execute.lock().unwrap().push(Ok(
            json!({"status": "Failed", "code": -2, "error": "invalid signed tx"}),
        ));
        let out = r.run(&u, Some(ORACLE), &req(WriteMode::Send)).await;
        assert_eq!(out.status, WriteStatus::Failed);
        assert!(out.txs[0].note.as_deref().unwrap().contains("code -2"));
        let writes = r.shared.writes.clone().unwrap();
        assert_eq!(
            writes
                .pending(&wallet().pubkey().to_string())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn send_refusals() {
        let r = rig();
        // Gasless: Jupiter pays, the wallet is not slot 0.
        let out = r
            .run(
                &FakeUltra::new(1_181_890, Some(Pubkey([77; 32]))),
                Some(ORACLE),
                &req(WriteMode::Send),
            )
            .await;
        assert_eq!(out.status, WriteStatus::Refused, "{:?}", out.txs);
        assert!(out.txs[0]
            .note
            .as_deref()
            .unwrap()
            .contains("not the fee payer"));
        // Other pairs never send.
        let mut other = req(WriteMode::Send);
        other.output_mint = Pubkey([55; 32]);
        let out = r.run(&FakeUltra::new(1, None), None, &other).await;
        assert!(out
            .refused
            .as_deref()
            .unwrap()
            .starts_with("pair_not_allowed"));
    }

    #[tokio::test]
    async fn open_keeper_request_blocks_send() {
        let r = rig();
        let w = wallet().pubkey();
        let req_key = Pubkey([88; 32]);
        r.chain.route(move |b| {
            (b["method"] == "getProgramAccounts").then(|| {
                json!({"context": {"slot": 500}, "value": [{"pubkey": req_key.to_string(),
                    "account": {"data": ["", "base64"], "owner": ids::JUP_PERPS, "lamports": 1, "executable": false}}]})
            })
        });
        let mut data = crate::domain::lp::perps::POSITION_REQUEST_DISC.to_vec();
        data.extend_from_slice(&w.0); // owner
        data.extend_from_slice(&[0u8; 32 * 4]); // pool custody position mint
        data.extend_from_slice(&(now_ms() / 1000).to_le_bytes()); // open_time
        data.extend_from_slice(&0i64.to_le_bytes()); // update_time
        data.extend_from_slice(&[0u8; 16]); // size, collateral
        data.extend_from_slice(&[2, 0, 1]); // decrease, market, long
        data.extend_from_slice(&[0; 6]); // six None options
        data.push(0); // executed = false
        data.extend_from_slice(&7u64.to_le_bytes()); // counter
        data.push(255); // bump
        data.push(0); // referral None
        r.chain.inner.put_account(AccountRead::from_bytes(
            req_key,
            500,
            ids::key(ids::JUP_PERPS),
            1,
            &data,
        ));
        let out = r
            .run(
                &FakeUltra::new(1_181_890, None),
                Some(ORACLE),
                &req(WriteMode::Send),
            )
            .await;
        assert_eq!(out.status, WriteStatus::Refused);
        let why = out.refused.unwrap();
        assert!(
            why.starts_with("keeper_requests") && why.contains(&req_key.to_string()),
            "{why}"
        );
    }

    /// Live, keyless: a real Ultra order for 0.01 SOL → USDC as the funded
    /// operator wallet, gated on the live Jupiter price and simulated on
    /// mainnet. `cargo test --bin tengu -- --ignored live_jupiter_swap --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_jupiter_swap_simulate() {
        let wallet: Pubkey = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S"
            .parse()
            .unwrap();
        let scope = ToolScope {
            net_hosts: vec![
                "lite-api.jup.ag".into(),
                "api.mainnet-beta.solana.com".into(),
            ],
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
        let ultra = HttpUltra {
            http,
            scope: scope.clone(),
            base: ULTRA_BASE.into(),
        };
        let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
        let mut rq = req(WriteMode::Simulate);
        rq.wallet = wallet;
        let out = swap_with(rpc, &ultra, oracle, &scope, &SolanaShared::default(), &rq).await;
        let obs = Observation::of("t", &out, now_ms(), 0, ObsSource::Live);
        println!("{}", obs.render_text(obs.observed_at_ms));
        for c in &out.checks {
            println!("check {} {} — {}", c.name, c.ok, c.detail);
        }
        for t in &out.txs {
            println!(
                "{} {:?} units={:?} size={:?} err={:?}\n{:#?}",
                t.label, t.status, t.units, t.tx_size, t.err, t.logs_tail
            );
        }
        assert_eq!(out.status, WriteStatus::Simulated, "{:?}", out.refused);
    }

    #[test]
    fn classify_execute_answers() {
        assert!(matches!(
            classify_execute(&json!({"status": "Success", "slot": "5", "code": 0})),
            Submitted::Landed { slot: 5, .. }
        ));
        assert!(matches!(
            classify_execute(&json!({"status": "Failed", "code": -1004, "error": "block height"})),
            Submitted::Rejected { .. }
        ));
        for v in [
            json!({"status": "Failed", "code": -1000, "error": "failed to land"}),
            json!({"status": "Failed", "code": 6001, "error": "slippage"}),
            json!({"status": "Success"}),
            json!({}),
        ] {
            assert!(
                matches!(classify_execute(&v), Submitted::Unknown { .. }),
                "{v}"
            );
        }
    }

    #[test]
    fn request_is_validated() {
        let ok = json!({"wallet": wallet().pubkey().to_string(), "input_mint": ids::WSOL,
            "output_mint": ids::USDC, "amount": 0.5, "oracle_gate_bps": 50});
        assert!(SwapRequest::parse(&ok).is_ok());
        for (k, v) in [
            ("amount", json!(0)),
            ("oracle_gate_bps", json!(-1)),
            ("output_mint", json!(ids::WSOL)),
            ("mode", json!("live")),
        ] {
            let mut bad = ok.clone();
            bad[k] = v;
            assert!(SwapRequest::parse(&bad).is_err(), "{k}");
        }
    }
}
