//! Send pipeline of the Solana write tools (phase 6b). Doc:
//! `docs/typed-observations-2026-09-24.md` § Write tools.
//!
//! `simulate` needs no key: the plan is compiled with a 1.4M CU limit and
//! simulated as the wallet (`sigVerify = false`, cluster blockhash).
//!
//! A [`SendSession`] owns the wallet for one tool call:
//!
//! | Step | Rule |
//! |---|---|
//! | open | signer = wallet; take the lease `wallet:<address>` (held ⇒ refused); resolve an earlier in-flight send first (still in flight ⇒ refused) |
//! | per transaction | renew lease → simulate (fail / no `unitsConsumed` ⇒ never sent) → CU limit `min(1.4M, ⌈units×1.1⌉)` → CU price (Helius estimate on a `helius` host, clamped; else the floor) → blockhash (confirmed) → size ≤ 1232 → sign → **pending record** → `sendTransaction` |
//! | send outcome | a JSON-RPC error on the first attempt = the node did not forward it (`failed`, not sent); a transport failure is ambiguous ⇒ one resend of the same bytes, then poll |
//! | confirm | poll `getSignatureStatuses` until confirmed / finalized (on-chain `err` ⇒ `failed`, landed); block height > `lastValidBlockHeight` + a final history lookup ⇒ `expired`; still unknown after [`MAX_POLL`] ⇒ `unconfirmed` (record kept) |
//! | landed | raise the wallet fence to the landing slot, clear the record |
//! | close | drop the cache rows the tool names, release the lease |

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::Value;

use crate::domain::observation::now_ms;
use crate::domain::solana::{Pubkey, Signature};
use crate::domain::solana_tx::{
    cu_limit, cu_price, Instruction, LegacyMessage, Transaction, MAX_COMPUTE_UNITS,
    PACKET_DATA_SIZE,
};
use crate::domain::solana_write::{
    clamp_cu_price, cu_limit_for, logs_tail, wallet_resource, PendingSend, TxReport, WriteStatus,
    CU_PRICE_FLOOR, LEASE_TTL_MS,
};
use crate::ports::observation::ObservationStore;
use crate::ports::solana_signer::SolanaSigner;
use crate::ports::solana_writes::SolanaWriteStore;

use super::rpc::SolanaRpc;
use super::signer::sign_transaction;

/// Longest confirmation wait before a send is reported `unconfirmed`.
pub(crate) const MAX_POLL: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One transaction of a write: instructions WITHOUT ComputeBudget (the
/// pipeline adds limit + price) and any signer besides the wallet.
pub(crate) struct TxPlan {
    pub label: String,
    pub ixs: Vec<Instruction>,
    pub extra_signers: Vec<Arc<dyn SolanaSigner>>,
}

impl TxPlan {
    pub(crate) fn new(label: &str, ixs: Vec<Instruction>) -> Self {
        TxPlan {
            label: label.to_string(),
            ixs,
            extra_signers: Vec::new(),
        }
    }
}

pub(crate) struct Pipeline {
    pub rpc: Arc<SolanaRpc>,
    pub tool: String,
    pub wallet: Pubkey,
    pub poll: Duration,
    pub max_poll: Duration,
}

/// `[limit, price, ...ixs]` compiled with `wallet` as payer.
fn compile(
    wallet: &Pubkey,
    ixs: &[Instruction],
    limit: u32,
    price: u64,
    blockhash: [u8; 32],
) -> Result<LegacyMessage, String> {
    let mut all = vec![cu_limit(limit), cu_price(price)];
    all.extend_from_slice(ixs);
    LegacyMessage::compile(wallet, &all, blockhash)
}

impl Pipeline {
    pub(crate) fn new(rpc: Arc<SolanaRpc>, tool: &str, wallet: Pubkey) -> Self {
        Pipeline {
            rpc,
            tool: tool.to_string(),
            wallet,
            poll: POLL_INTERVAL,
            max_poll: MAX_POLL,
        }
    }

    /// Simulate `plan` as the wallet — no key, nothing sent.
    pub(crate) async fn simulate(&self, plan: &TxPlan) -> TxReport {
        let mut r = TxReport::new(&plan.label);
        let msg = match compile(
            &self.wallet,
            &plan.ixs,
            MAX_COMPUTE_UNITS,
            CU_PRICE_FLOOR,
            [0; 32],
        ) {
            Ok(m) => m,
            Err(e) => return failed_sim(r, format!("compile: {e}")),
        };
        r.tx_size = Some(msg.tx_size());
        r.message_b64 = Some(B64.encode(msg.serialize()));
        if msg.tx_size() > PACKET_DATA_SIZE {
            return failed_sim(
                r,
                format!(
                    "transaction is {} bytes, over the {PACKET_DATA_SIZE}-byte limit",
                    msg.tx_size()
                ),
            );
        }
        match self
            .rpc
            .simulate_transaction(&Transaction::unsigned(&msg).serialize())
            .await
        {
            Ok(sim) => {
                r.units = sim.units;
                r.slot = Some(sim.slot);
                r.logs_tail = logs_tail(&sim.logs);
                r.status = Some(if sim.err.is_none() {
                    WriteStatus::Simulated
                } else {
                    WriteStatus::SimFailed
                });
                r.err = sim.err;
                r
            }
            Err(e) => failed_sim(r, format!("{e:#}")),
        }
    }

    /// Take the wallet for a send: signer check, lease, then resolve an
    /// earlier in-flight send. `Err` = refusal reason (nothing sent).
    pub(crate) async fn open_session(
        &self,
        store: Arc<dyn SolanaWriteStore>,
        signer: Arc<dyn SolanaSigner>,
    ) -> Result<SendSession<'_>, String> {
        if signer.pubkey() != self.wallet {
            return Err(format!(
                "signer_mismatch: the key is {}, not {}",
                signer.pubkey(),
                self.wallet
            ));
        }
        let wallet = self.wallet.to_string();
        let resource = wallet_resource(&wallet);
        let holder = format!("{}:{}", std::process::id(), uuid::Uuid::new_v4());
        let lease = store
            .acquire(&resource, &holder, LEASE_TTL_MS, now_ms())
            .await
            .map_err(|e| format!("write_store_unavailable: {e:#}"))?;
        if !lease.granted {
            return Err(format!(
                "lease_held: {} is sending for {wallet} (lease until {} ms)",
                lease.current_holder, lease.expires_at_ms
            ));
        }
        let mut session = SendSession {
            p: self,
            store,
            signer,
            holder,
            resource,
            pending_resolved: None,
        };
        if let Err(reason) = session.resolve_pending().await {
            session.release().await;
            return Err(reason);
        }
        Ok(session)
    }
}

fn failed_sim(mut r: TxReport, note: String) -> TxReport {
    r.status = Some(WriteStatus::SimFailed);
    r.note = Some(note);
    r
}

/// What a signature's status says.
enum Seen {
    /// Landed at `slot`; `err` = the on-chain error, if any.
    Landed {
        slot: u64,
        err: Option<Value>,
    },
    NotYet,
}

fn parse_status(v: &Value) -> Seen {
    let s = &v[0];
    let done = matches!(
        s["confirmationStatus"].as_str(),
        Some("confirmed") | Some("finalized")
    );
    match (done, s["slot"].as_u64()) {
        (true, Some(slot)) => Seen::Landed {
            slot,
            err: s.get("err").filter(|e| !e.is_null()).cloned(),
        },
        _ => Seen::NotYet,
    }
}

pub(crate) struct SendSession<'a> {
    p: &'a Pipeline,
    store: Arc<dyn SolanaWriteStore>,
    signer: Arc<dyn SolanaSigner>,
    holder: String,
    resource: String,
    /// How an earlier in-flight send was resolved (for the result).
    pub pending_resolved: Option<String>,
}

impl SendSession<'_> {
    fn wallet(&self) -> String {
        self.p.wallet.to_string()
    }

    async fn resolve_pending(&mut self) -> Result<(), String> {
        let wallet = self.wallet();
        let pending = self
            .store
            .pending(&wallet)
            .await
            .map_err(|e| format!("write_store_unavailable: {e:#}"))?;
        let Some(p) = pending else {
            return Ok(());
        };
        let sig: Signature = p
            .signature
            .parse()
            .map_err(|e| format!("pending_unresolved: bad stored signature: {e}"))?;
        let unresolved = |why: String| {
            format!(
                "pending_unresolved: {} of {} ({}) sent at {} ms {why}",
                p.signature, p.tool, p.label, p.sent_at_ms
            )
        };
        let status = self
            .p
            .rpc
            .get_signature_statuses(&[sig], true)
            .await
            .map_err(|e| unresolved(format!("— status check failed: {e:#}")))?;
        let note = match parse_status(&status.1) {
            Seen::Landed { slot, err } => {
                self.raise_fence(slot).await;
                match err {
                    None => format!("{} ({}) landed at slot {slot}", p.signature, p.tool),
                    Some(e) => format!(
                        "{} ({}) landed at slot {slot} with error {e}",
                        p.signature, p.tool
                    ),
                }
            }
            Seen::NotYet => {
                let height = self
                    .p
                    .rpc
                    .get_block_height()
                    .await
                    .map_err(|e| unresolved(format!("— block height failed: {e:#}")))?;
                if height <= p.last_valid_block_height {
                    return Err(unresolved(format!(
                        "is still in flight (block height {height} ≤ {})",
                        p.last_valid_block_height
                    )));
                }
                format!(
                    "{} ({}) expired unseen (block height {height} > {})",
                    p.signature, p.tool, p.last_valid_block_height
                )
            }
        };
        let _ = self.store.clear_pending(&wallet, &p.signature).await;
        self.pending_resolved = Some(note);
        Ok(())
    }

    async fn raise_fence(&self, slot: u64) {
        if let Err(e) = self.store.raise_fence(&self.wallet(), slot, now_ms()).await {
            tracing::warn!(error = %format!("{e:#}"), slot, "solana write fence not raised");
        }
    }

    async fn release(&self) {
        if let Err(e) = self.store.release(&self.resource, &self.holder).await {
            tracing::warn!(error = %format!("{e:#}"), "solana write lease not released");
        }
    }

    /// Simulate, sign, record, send and confirm one transaction.
    pub(crate) async fn send(&mut self, plan: &TxPlan) -> TxReport {
        let mut r = TxReport::new(&plan.label);
        match self
            .store
            .acquire(&self.resource, &self.holder, LEASE_TTL_MS, now_ms())
            .await
        {
            Ok(l) if l.granted => {}
            Ok(l) => {
                r.status = Some(WriteStatus::Refused);
                r.note = Some(format!("lease lost to {}; not sent", l.current_holder));
                return r;
            }
            Err(e) => {
                r.status = Some(WriteStatus::Refused);
                r.note = Some(format!("write store unavailable: {e:#}; not sent"));
                return r;
            }
        }
        let sim = self.p.simulate(plan).await;
        if sim.status() != WriteStatus::Simulated {
            return sim;
        }
        let Some(units) = sim.units else {
            return failed_sim(sim, "simulation reported no unitsConsumed; not sent".into());
        };
        let limit = cu_limit_for(units);
        let price = self.cu_price(&plan.ixs, limit).await;
        r.units = Some(units);
        r.cu_limit = Some(limit);
        r.cu_price_micro_lamports = Some(price);

        let (blockhash, lvbh) = match self.p.rpc.get_latest_blockhash().await {
            Ok(b) => b,
            Err(e) => {
                r.status = Some(WriteStatus::Refused);
                r.note = Some(format!("{e:#}; not sent"));
                return r;
            }
        };
        let msg = match compile(&self.p.wallet, &plan.ixs, limit, price, blockhash) {
            Ok(m) => m,
            Err(e) => {
                r.status = Some(WriteStatus::Refused);
                r.note = Some(format!("compile: {e}; not sent"));
                return r;
            }
        };
        r.tx_size = Some(msg.tx_size());
        if msg.tx_size() > PACKET_DATA_SIZE {
            r.status = Some(WriteStatus::Refused);
            r.note = Some(format!(
                "transaction is {} bytes, over the {PACKET_DATA_SIZE}-byte limit; not sent",
                msg.tx_size()
            ));
            return r;
        }
        let mut tx = Transaction::unsigned(&msg);
        let view = match Transaction::parse(&tx.serialize()) {
            Ok((_, v)) => v,
            Err(e) => {
                r.status = Some(WriteStatus::Refused);
                r.note = Some(format!("re-parse: {e}; not sent"));
                return r;
            }
        };
        let mut signers: Vec<&dyn SolanaSigner> = vec![self.signer.as_ref()];
        signers.extend(plan.extra_signers.iter().map(|s| s.as_ref()));
        if let Err(e) = sign_transaction(&mut tx, &view, &signers) {
            r.status = Some(WriteStatus::Refused);
            r.note = Some(format!("{e:#}; not sent"));
            return r;
        }
        let sig = Signature(tx.id().expect("a compiled message has a fee payer"));
        let pending = PendingSend {
            wallet: self.wallet(),
            tool: self.p.tool.clone(),
            label: plan.label.clone(),
            signature: sig.to_string(),
            last_valid_block_height: lvbh,
            sent_at_ms: now_ms(),
        };
        if let Err(e) = self.store.put_pending(&pending).await {
            r.status = Some(WriteStatus::Refused);
            r.note = Some(format!("write store unavailable: {e:#}; not sent"));
            return r;
        }
        r.signature = Some(sig.to_string());
        let bytes = tx.serialize();

        match self.p.rpc.send_transaction_once(&bytes).await {
            Ok(_) => {}
            Err(e) if e.code.is_some() => {
                // The node answered with an error: it did not forward it.
                r.status = Some(WriteStatus::Failed);
                r.note = Some(format!("not sent: {}", e.message));
                if let Some(d) = &e.data {
                    r.err = d.get("err").filter(|x| !x.is_null()).cloned();
                    if let Some(logs) = d["logs"].as_array() {
                        let logs: Vec<String> = logs
                            .iter()
                            .filter_map(|l| l.as_str().map(str::to_string))
                            .collect();
                        r.logs_tail = logs_tail(&logs);
                    }
                }
                let _ = self
                    .store
                    .clear_pending(&pending.wallet, &pending.signature)
                    .await;
                return r;
            }
            Err(first) => {
                // Ambiguous: it may have been forwarded. Resend the same
                // bytes once (same signature — the cluster dedups), then poll.
                tokio::time::sleep(self.p.poll).await;
                if let Err(second) = self.p.rpc.send_transaction_once(&bytes).await {
                    r.note = Some(format!(
                        "send outcome unknown: {}; resend: {}",
                        first.message, second.message
                    ));
                }
            }
        }
        self.confirm(&mut r, &sig, &pending).await;
        r
    }

    async fn cu_price(&self, ixs: &[Instruction], limit: u32) -> u64 {
        if !self.p.rpc.host().contains("helius") {
            return CU_PRICE_FLOOR;
        }
        let Ok(msg) = compile(&self.p.wallet, ixs, limit, CU_PRICE_FLOOR, [0; 32]) else {
            return CU_PRICE_FLOOR;
        };
        match self
            .p
            .rpc
            .get_priority_fee_estimate(&Transaction::unsigned(&msg).serialize())
            .await
        {
            Ok(est) => clamp_cu_price(est),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "priority fee estimate failed; floor");
                CU_PRICE_FLOOR
            }
        }
    }

    async fn confirm(&self, r: &mut TxReport, sig: &Signature, pending: &PendingSend) {
        let started = Instant::now();
        loop {
            if let Ok((_, v)) = self.p.rpc.get_signature_statuses(&[*sig], false).await {
                if let Seen::Landed { slot, err } = parse_status(&v) {
                    return self.landed(r, pending, slot, err).await;
                }
            }
            if let Ok(h) = self.p.rpc.get_block_height().await {
                if h > pending.last_valid_block_height {
                    // Last look in history before calling it expired.
                    if let Ok((_, v)) = self.p.rpc.get_signature_statuses(&[*sig], true).await {
                        if let Seen::Landed { slot, err } = parse_status(&v) {
                            return self.landed(r, pending, slot, err).await;
                        }
                        r.status = Some(WriteStatus::Expired);
                        r.note = Some(format!(
                            "blockhash expired unseen (block height {h} > {}); did not land",
                            pending.last_valid_block_height
                        ));
                        let _ = self
                            .store
                            .clear_pending(&pending.wallet, &pending.signature)
                            .await;
                        return;
                    }
                }
            }
            if started.elapsed() >= self.p.max_poll {
                r.status = Some(WriteStatus::Unconfirmed);
                r.note = Some(format!(
                    "not confirmed after {} s; the next send of this wallet resolves it first",
                    self.p.max_poll.as_secs()
                ));
                return;
            }
            tokio::time::sleep(self.p.poll).await;
        }
    }

    async fn landed(&self, r: &mut TxReport, pending: &PendingSend, slot: u64, err: Option<Value>) {
        r.slot = Some(slot);
        r.status = Some(if err.is_none() {
            WriteStatus::Confirmed
        } else {
            WriteStatus::Failed
        });
        if err.is_some() {
            r.note = Some("landed with an on-chain error (fees paid)".into());
        }
        r.err = err;
        self.raise_fence(slot).await;
        let _ = self
            .store
            .clear_pending(&pending.wallet, &pending.signature)
            .await;
    }

    /// Drop `stale_keys` from the observation cache and release the lease.
    pub(crate) async fn close(
        self,
        cache: Option<&Arc<dyn ObservationStore>>,
        stale_keys: &[String],
    ) {
        if let (Some(c), false) = (cache, stale_keys.is_empty()) {
            if let Err(e) = c.remove(stale_keys).await {
                tracing::warn!(error = %format!("{e:#}"), "stale observation rows not removed");
            }
        }
        self.release().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use crate::adapters::outbound::solana::rpc::tests::{err_envelope, ok_envelope};
    use crate::adapters::outbound::solana::rpc::{RpcError, RpcTransport};
    use crate::adapters::outbound::solana::signer::LocalKeypair;
    use crate::adapters::outbound::solana::writes_store::SqliteWriteStore;
    use crate::domain::observation::ErrorClass;
    use crate::domain::solana_tx::{spl_sync_native, system_transfer, MessageView};

    /// How the fake cluster answers `sendTransaction`.
    #[derive(Clone, Copy)]
    enum SendMode {
        Accept,
        Preflight,
        TimeoutThenAccept,
    }

    /// A tiny cluster: simulation result, send behaviour, a status that
    /// appears after `confirm_after` polls, block height rising per call.
    struct Chain {
        requests: Mutex<Vec<Value>>,
        sent: Mutex<Vec<Vec<u8>>>,
        sim_err: Option<Value>,
        sim_units: Option<u64>,
        send: Mutex<VecDeque<SendMode>>,
        confirm_after: Option<u64>,
        tx_err: Option<Value>,
        polls: AtomicU64,
        height: AtomicU64,
        height_step: u64,
        lvbh: u64,
        fee_estimate: f64,
    }

    impl Chain {
        fn new() -> Self {
            Chain {
                requests: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                sim_err: None,
                sim_units: Some(50_000),
                send: Mutex::new(VecDeque::from([SendMode::Accept])),
                confirm_after: Some(1),
                tx_err: None,
                polls: AtomicU64::new(0),
                height: AtomicU64::new(1_000),
                height_step: 1,
                lvbh: 1_150,
                fee_estimate: 25_000.0,
            }
        }
        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|b| b["method"].as_str().unwrap().to_string())
                .collect()
        }
    }

    #[async_trait]
    impl RpcTransport for Chain {
        async fn call(&self, body: Value) -> Result<Value, RpcError> {
            self.requests.lock().unwrap().push(body.clone());
            let ctx = json!({"slot": 500});
            match body["method"].as_str().unwrap() {
                "simulateTransaction" => Ok(ok_envelope(json!({"context": ctx, "value": {
                    "err": self.sim_err, "logs": ["Program log: sim"],
                    "unitsConsumed": self.sim_units}}))),
                "getLatestBlockhash" => Ok(ok_envelope(json!({"context": ctx, "value": {
                    "blockhash": "EkSnNWid2cvwEVnVx9aBqawnmiCNiDgp3gUdkDPTKN1N",
                    "lastValidBlockHeight": self.lvbh}}))),
                "getPriorityFeeEstimate" => Ok(ok_envelope(
                    json!({"priorityFeeEstimate": self.fee_estimate}),
                )),
                "sendTransaction" => {
                    let bytes = B64.decode(body["params"][0].as_str().unwrap()).unwrap();
                    let mode = self
                        .send
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(SendMode::Accept);
                    match mode {
                        SendMode::Preflight => Ok(json!({"jsonrpc": "2.0", "id": 1, "error": {
                            "code": -32002, "message": "Transaction simulation failed",
                            "data": {"err": {"InstructionError": [2, {"Custom": 1}]},
                                     "logs": ["Program log: preflight"]}}})),
                        SendMode::TimeoutThenAccept => {
                            self.sent.lock().unwrap().push(bytes);
                            Err(RpcError::new(ErrorClass::Timeout, "timed out"))
                        }
                        SendMode::Accept => {
                            let (tx, _) = Transaction::parse(&bytes).unwrap();
                            self.sent.lock().unwrap().push(bytes);
                            Ok(ok_envelope(json!(Signature(tx.id().unwrap()).to_string())))
                        }
                    }
                }
                "getSignatureStatuses" => {
                    let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
                    let seen = !self.sent.lock().unwrap().is_empty()
                        && self.confirm_after.is_some_and(|k| n >= k);
                    let v = if seen {
                        json!([{"slot": 777, "confirmations": 0, "err": self.tx_err,
                                "confirmationStatus": "confirmed"}])
                    } else {
                        json!([null])
                    };
                    Ok(ok_envelope(json!({"context": ctx, "value": v})))
                }
                "getBlockHeight" => Ok(ok_envelope(json!(self
                    .height
                    .fetch_add(self.height_step, Ordering::SeqCst)))),
                m => Ok(err_envelope(-32601, &format!("fake chain: {m}"))),
            }
        }
    }

    const WALLET_SEED: [u8; 32] = [1; 32];

    fn wallet() -> Arc<LocalKeypair> {
        Arc::new(LocalKeypair::from_seed(&WALLET_SEED))
    }

    fn pipeline(chain: &Arc<Chain>, host: &str) -> Pipeline {
        let rpc = Arc::new(SolanaRpc::with_transport(chain.clone(), host).with_backoff_ms(0));
        let mut p = Pipeline::new(rpc, "test_tool", wallet().pubkey());
        p.poll = Duration::from_millis(1);
        p.max_poll = Duration::from_millis(200);
        p
    }

    fn plan() -> TxPlan {
        let w = wallet().pubkey();
        let to: Pubkey = "9hSR6S7WPtxmTojgo6GG3k4yDPecgJY292j7xrsUGWBu"
            .parse()
            .unwrap();
        TxPlan::new(
            "transfer",
            vec![system_transfer(&w, &to, 1), spl_sync_native(&to)],
        )
    }

    fn store() -> (tempfile::TempDir, Arc<dyn SolanaWriteStore>) {
        let d = tempfile::tempdir().unwrap();
        let s = SqliteWriteStore::open(d.path()).unwrap();
        (d, Arc::new(s))
    }

    #[tokio::test]
    async fn simulate_needs_no_key_and_reports() {
        let chain = Arc::new(Chain::new());
        let r = pipeline(&chain, "api.mainnet-beta.solana.com")
            .simulate(&plan())
            .await;
        assert_eq!(r.status(), WriteStatus::Simulated);
        assert_eq!(r.units, Some(50_000));
        assert_eq!(r.logs_tail, vec!["Program log: sim"]);
        let size = r.tx_size.unwrap();
        let msg = B64.decode(r.message_b64.unwrap()).unwrap();
        let view = MessageView::parse(&msg).unwrap();
        assert_eq!(view.signers(), &[wallet().pubkey()]);
        assert!(size <= PACKET_DATA_SIZE);
        assert_eq!(chain.methods(), vec!["simulateTransaction"]);
    }

    #[tokio::test]
    async fn oversized_transaction_is_never_simulated() {
        let chain = Arc::new(Chain::new());
        let w = wallet().pubkey();
        let ixs = (0..40u8)
            .map(|i| system_transfer(&w, &Pubkey([i; 32]), 1))
            .collect();
        let r = pipeline(&chain, "h")
            .simulate(&TxPlan::new("big", ixs))
            .await;
        assert_eq!(r.status(), WriteStatus::SimFailed);
        assert!(r.note.unwrap().contains("over the 1232-byte limit"));
        assert!(chain.methods().is_empty());
    }

    #[tokio::test]
    async fn send_confirms_raises_fence_and_cleans_up() {
        let chain = Arc::new(Chain::new());
        let (_d, st) = store();
        let p = pipeline(&chain, "mainnet.helius-rpc.com");
        let mut s = p.open_session(st.clone(), wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::Confirmed, "{:?}", r.note);
        assert_eq!(r.slot, Some(777));
        assert_eq!(r.cu_limit, Some(55_000));
        assert_eq!(r.cu_price_micro_lamports, Some(25_000), "helius estimate");
        s.close(None, &[]).await;
        let w = wallet().pubkey().to_string();
        assert_eq!(st.fence(&w).await.unwrap(), Some(777));
        assert_eq!(st.pending(&w).await.unwrap(), None);
        assert!(
            st.acquire(&wallet_resource(&w), "other", 1_000, now_ms())
                .await
                .unwrap()
                .granted
        );
        // The sent bytes: CU limit + price first, signed by the wallet.
        let sent = chain.sent.lock().unwrap()[0].clone();
        let (tx, view) = Transaction::parse(&sent).unwrap();
        assert_eq!(view.signers(), &[wallet().pubkey()]);
        assert_eq!(tx.signatures[0], wallet().sign(&tx.message).0);
        assert_eq!(
            r.signature.unwrap(),
            Signature(tx.signatures[0]).to_string()
        );
    }

    #[tokio::test]
    async fn public_rpc_pays_the_floor_price() {
        let chain = Arc::new(Chain::new());
        let (_d, st) = store();
        let p = pipeline(&chain, "api.mainnet-beta.solana.com");
        let mut s = p.open_session(st, wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.cu_price_micro_lamports, Some(CU_PRICE_FLOOR));
        assert!(!chain
            .methods()
            .contains(&"getPriorityFeeEstimate".to_string()));
    }

    #[tokio::test]
    async fn failed_simulation_is_never_sent() {
        let mut c = Chain::new();
        c.sim_err = Some(json!({"InstructionError": [0, {"Custom": 7}]}));
        let chain = Arc::new(c);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st.clone(), wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::SimFailed);
        assert!(!chain.methods().contains(&"sendTransaction".to_string()));
        assert_eq!(
            st.pending(&wallet().pubkey().to_string()).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn missing_units_is_never_sent() {
        let mut c = Chain::new();
        c.sim_units = None;
        let chain = Arc::new(c);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st, wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::SimFailed);
        assert!(r.note.unwrap().contains("no unitsConsumed"));
        assert!(!chain.methods().contains(&"sendTransaction".to_string()));
    }

    #[tokio::test]
    async fn preflight_rejection_is_failed_not_sent() {
        let chain = Arc::new(Chain::new());
        *chain.send.lock().unwrap() = VecDeque::from([SendMode::Preflight]);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st.clone(), wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::Failed);
        assert!(r.note.as_deref().unwrap().starts_with("not sent"));
        assert_eq!(r.logs_tail, vec!["Program log: preflight"]);
        assert_eq!(
            st.pending(&wallet().pubkey().to_string()).await.unwrap(),
            None
        );
        assert!(!chain
            .methods()
            .contains(&"getSignatureStatuses".to_string()));
    }

    #[tokio::test]
    async fn ambiguous_send_is_resent_once_then_confirmed() {
        let chain = Arc::new(Chain::new());
        *chain.send.lock().unwrap() =
            VecDeque::from([SendMode::TimeoutThenAccept, SendMode::Accept]);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st, wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::Confirmed, "{:?}", r.note);
        let sent = chain.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1], "the same signed bytes");
    }

    #[tokio::test]
    async fn unseen_past_last_valid_height_is_expired() {
        let mut c = Chain::new();
        c.confirm_after = None;
        c.height = AtomicU64::new(1_149);
        let chain = Arc::new(c);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st.clone(), wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::Expired, "{:?}", r.note);
        assert_eq!(
            st.pending(&wallet().pubkey().to_string()).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn never_seen_within_the_poll_budget_stays_pending() {
        let mut c = Chain::new();
        c.confirm_after = None;
        c.height_step = 0;
        let chain = Arc::new(c);
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let mut s = p.open_session(st.clone(), wallet()).await.unwrap();
        let r = s.send(&plan()).await;
        assert_eq!(r.status(), WriteStatus::Unconfirmed);
        let w = wallet().pubkey().to_string();
        let pending = st.pending(&w).await.unwrap().unwrap();
        assert_eq!(Some(pending.signature.clone()), r.signature);
        s.close(None, &[]).await;
        // The next session refuses while it is still in flight.
        let reason = match p.open_session(st.clone(), wallet()).await {
            Err(r) => r,
            Ok(_) => panic!("opened with an unresolved send"),
        };
        assert!(reason.starts_with("pending_unresolved"), "{reason}");
        assert!(reason.contains(&pending.signature));
        // The refused session released its lease.
        assert!(
            st.acquire(&wallet_resource(&w), "x", 1_000, now_ms())
                .await
                .unwrap()
                .granted
        );
    }

    #[tokio::test]
    async fn open_session_resolves_a_landed_or_expired_record_first() {
        let chain = Arc::new(Chain::new());
        chain.sent.lock().unwrap().push(vec![0]); // status appears on poll 1
        let (_d, st) = store();
        let w = wallet().pubkey().to_string();
        let sig = "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
        let rec = PendingSend {
            wallet: w.clone(),
            tool: "dlmm_open_position".into(),
            label: "open".into(),
            signature: sig.into(),
            last_valid_block_height: 900,
            sent_at_ms: 1,
        };
        st.put_pending(&rec).await.unwrap();
        let p = pipeline(&chain, "h");
        let s = p.open_session(st.clone(), wallet()).await.unwrap();
        assert!(s
            .pending_resolved
            .as_deref()
            .unwrap()
            .contains("landed at slot 777"));
        s.close(None, &[]).await;
        assert_eq!(st.fence(&w).await.unwrap(), Some(777));
        assert_eq!(st.pending(&w).await.unwrap(), None);

        let mut c = Chain::new();
        c.confirm_after = None; // never seen; height 1000 > 900
        let chain = Arc::new(c);
        st.put_pending(&rec).await.unwrap();
        let p = pipeline(&chain, "h");
        let s = p.open_session(st.clone(), wallet()).await.unwrap();
        assert!(s
            .pending_resolved
            .as_deref()
            .unwrap()
            .contains("expired unseen"));
    }

    #[tokio::test]
    async fn lease_and_signer_refusals() {
        let chain = Arc::new(Chain::new());
        let (_d, st) = store();
        let p = pipeline(&chain, "h");
        let w = wallet().pubkey().to_string();
        assert!(
            st.acquire(&wallet_resource(&w), "someone", 60_000, now_ms())
                .await
                .unwrap()
                .granted
        );
        let reason = p.open_session(st.clone(), wallet()).await.err().unwrap();
        assert!(reason.starts_with("lease_held: someone"), "{reason}");
        let stranger = Arc::new(LocalKeypair::from_seed(&[9; 32]));
        let reason = p.open_session(st, stranger).await.err().unwrap();
        assert!(reason.starts_with("signer_mismatch"), "{reason}");
    }
}
