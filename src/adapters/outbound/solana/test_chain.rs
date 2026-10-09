//! Test-only fake cluster for the write path (`send.rs` and the write
//! tools): simulation result, `sendTransaction` behaviour, a signature
//! status that appears after `confirm_after` polls, a block height rising
//! per call, answers by request content (`route`), and account reads served
//! by the shared `rpc::tests::FakeTransport` (`inner`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};

use super::rpc::tests::{ok_envelope, FakeTransport};
use super::rpc::{RpcError, RpcTransport};
use crate::domain::observation::ErrorClass;
use crate::domain::solana::Signature;
use crate::domain::solana_tx::Transaction;

/// How the fake cluster answers `sendTransaction`.
#[derive(Clone, Copy)]
pub(crate) enum SendMode {
    Accept,
    Preflight,
    TimeoutThenAccept,
}

/// A tiny cluster: simulation result, send behaviour, a status that
/// appears after `confirm_after` polls, block height rising per call.
pub(crate) struct Chain {
    pub requests: Mutex<Vec<Value>>,
    pub sent: Mutex<Vec<Vec<u8>>>,
    pub sim_err: Option<Value>,
    pub sim_units: Option<u64>,
    pub send: Mutex<VecDeque<SendMode>>,
    pub confirm_after: Option<u64>,
    pub tx_err: Option<Value>,
    pub polls: AtomicU64,
    pub height: AtomicU64,
    pub height_step: u64,
    pub lvbh: u64,
    pub fee_estimate: f64,
    /// Account reads (`getMultipleAccounts`, `getSlot`) and anything else.
    pub inner: FakeTransport,
    /// Answers a request by content (`Some(result)`), before everything else.
    #[allow(clippy::type_complexity)]
    pub route: Mutex<Option<Box<dyn Fn(&Value) -> Option<Value> + Send + Sync>>>,
}

impl Chain {
    pub(crate) fn new() -> Self {
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
            inner: FakeTransport::at_slot(500),
            route: Mutex::new(None),
        }
    }
    pub(crate) fn route(&self, f: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) {
        *self.route.lock().unwrap() = Some(Box::new(f));
    }
    pub(crate) fn methods(&self) -> Vec<String> {
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
        let method = body["method"].as_str().unwrap().to_string();
        if let Some(result) = self.route.lock().unwrap().as_ref().and_then(|f| f(&body)) {
            return Ok(ok_envelope(result));
        }
        match method.as_str() {
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
            _ => self.inner.call(body).await,
        }
    }
}
