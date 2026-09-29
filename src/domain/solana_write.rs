//! Solana write path — typed results and send policy (phase 6b). Pure.
//!
//! Every write tool returns one [`WriteResult`] as observation `write/1`
//! (ttl 0: never cached). `mode = "simulate"` (default) builds and
//! simulates, no key; `mode = "send"` signs and sends through the pipeline
//! in `adapters/outbound/solana/send.rs`.
//!
//! | `WriteStatus` | Meaning | Observation status |
//! |---|---|---|
//! | `simulated` | simulation succeeded (nothing sent) | ok |
//! | `sim_failed` | simulation failed — never sent | error |
//! | `refused` | a check / gate / lease / signer rule said no — never sent | error |
//! | `failed` | rejected before landing (preflight) or landed with an error | error |
//! | `expired` | blockhash expired unseen — did not land, safe to retry | error |
//! | `unconfirmed` | sent, outcome unknown — do NOT retry; the next send resolves it first | partial |
//! | `partial` | some transactions of a multi-tx write confirmed, then one did not | partial |
//! | `confirmed` | every transaction confirmed | ok |

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::observation::{
    set_int, set_str, ErrorClass, Features, ObsStatus, Observed, ReadError,
};
use crate::domain::solana_tx::MAX_COMPUTE_UNITS;

/// Lease / pending / fence resource of a wallet.
pub fn wallet_resource(wallet: &str) -> String {
    format!("wallet:{wallet}")
}

/// Lease TTL: covers a blockhash's validity (~150 blocks ≈ 60–90 s) plus
/// confirmation polling; renewed before every transaction.
pub const LEASE_TTL_MS: i64 = 150_000;
/// Priority-fee clamp, micro-lamports per CU (bot `sendOptimized.ts`).
pub const CU_PRICE_FLOOR: u64 = 1_000;
pub const CU_PRICE_CEIL: u64 = 5_000_000;
/// Program log lines kept in a report (the tail — where errors are).
pub const LOG_TAIL: usize = 15;

/// CU limit for a transaction that simulated at `units`:
/// `min(1.4M, ceil(units × 1.1))` (bot `sendOptimized.ts`).
pub fn cu_limit_for(units: u64) -> u32 {
    let buffered = units.saturating_mul(11).div_ceil(10);
    buffered.min(u64::from(MAX_COMPUTE_UNITS)) as u32
}

/// A priority-fee estimate clamped to [`CU_PRICE_FLOOR`, `CU_PRICE_CEIL`];
/// a non-finite / negative estimate is the floor.
pub fn clamp_cu_price(estimate: f64) -> u64 {
    if !estimate.is_finite() || estimate <= 0.0 {
        return CU_PRICE_FLOOR;
    }
    (estimate.ceil() as u64).clamp(CU_PRICE_FLOOR, CU_PRICE_CEIL)
}

/// Last [`LOG_TAIL`] lines, whole (lines are never cut — they hold ids).
pub fn logs_tail(logs: &[String]) -> Vec<String> {
    logs[logs.len().saturating_sub(LOG_TAIL)..].to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Simulate,
    Send,
}

impl WriteMode {
    /// The `mode` argument; absent = `simulate`.
    pub fn parse(v: Option<&str>) -> Result<Self, String> {
        match v {
            None | Some("simulate") => Ok(WriteMode::Simulate),
            Some("send") => Ok(WriteMode::Send),
            Some(other) => Err(format!(
                "mode must be \"simulate\" or \"send\", got \"{other}\""
            )),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            WriteMode::Simulate => "simulate",
            WriteMode::Send => "send",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStatus {
    Simulated,
    SimFailed,
    Refused,
    Unconfirmed,
    Confirmed,
    Failed,
    Expired,
    Partial,
}

impl WriteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteStatus::Simulated => "simulated",
            WriteStatus::SimFailed => "sim_failed",
            WriteStatus::Refused => "refused",
            WriteStatus::Unconfirmed => "unconfirmed",
            WriteStatus::Confirmed => "confirmed",
            WriteStatus::Failed => "failed",
            WriteStatus::Expired => "expired",
            WriteStatus::Partial => "partial",
        }
    }
    pub fn obs_status(self) -> ObsStatus {
        match self {
            WriteStatus::Simulated | WriteStatus::Confirmed => ObsStatus::Ok,
            WriteStatus::Unconfirmed | WriteStatus::Partial => ObsStatus::Partial,
            _ => ObsStatus::Error,
        }
    }
}

/// One pre-send check (gate). A failed check refuses the write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn new(name: &str, ok: bool, detail: impl Into<String>) -> Self {
        Check {
            name: name.to_string(),
            ok,
            detail: detail.into(),
        }
    }
}

/// One transaction of a write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TxReport {
    pub label: String,
    pub status: Option<WriteStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cu_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cu_price_micro_lamports: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_size: Option<usize>,
    /// On-chain / simulation `err` object, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs_tail: Vec<String>,
    /// Unsigned message (base64) — simulate only, for inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl TxReport {
    pub fn new(label: &str) -> Self {
        TxReport {
            label: label.to_string(),
            ..Default::default()
        }
    }
    pub fn status(&self) -> WriteStatus {
        self.status.unwrap_or(WriteStatus::Refused)
    }
}

/// `write/1:<tool>:<wallet>:<started_at_ms>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteResult {
    pub tool: String,
    pub wallet: String,
    pub mode: WriteMode,
    pub status: WriteStatus,
    pub started_at_ms: i64,
    /// RPC host (never the URL — it may hold a key).
    pub rpc_host: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub txs: Vec<TxReport>,
    /// Refusal reason code (`lease_held`, `signer_not_allowed`, a failed
    /// check's name, …) with a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// How an earlier in-flight send of this wallet was resolved first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_resolved: Option<String>,
    /// Tool-specific payload (position key, amounts, quote, …).
    #[serde(default)]
    pub details: Value,
}

impl WriteResult {
    pub fn new(
        tool: &str,
        wallet: &str,
        mode: WriteMode,
        started_at_ms: i64,
        rpc_host: &str,
    ) -> Self {
        WriteResult {
            tool: tool.to_string(),
            wallet: wallet.to_string(),
            mode,
            status: WriteStatus::Refused,
            started_at_ms,
            rpc_host: rpc_host.to_string(),
            checks: Vec::new(),
            txs: Vec::new(),
            refused: None,
            pending_resolved: None,
            details: Value::Null,
        }
    }

    /// Refuse with `reason` (never sent).
    pub fn refuse(mut self, reason: impl Into<String>) -> Self {
        self.status = WriteStatus::Refused;
        self.refused = Some(reason.into());
        self
    }

    /// First failed check, if any.
    pub fn failed_check(&self) -> Option<&Check> {
        self.checks.iter().find(|c| !c.ok)
    }

    /// Overall status from the per-transaction reports (set after the txs
    /// ran): all simulated / confirmed ⇒ that; a failure after at least one
    /// confirmation ⇒ `partial`; else the first non-success status.
    pub fn settle(&mut self) {
        let Some(first_bad) = self
            .txs
            .iter()
            .map(TxReport::status)
            .find(|s| !matches!(s, WriteStatus::Simulated | WriteStatus::Confirmed))
        else {
            self.status = match self.mode {
                WriteMode::Simulate => WriteStatus::Simulated,
                WriteMode::Send => WriteStatus::Confirmed,
            };
            if self.txs.is_empty() {
                self.status = WriteStatus::Refused;
                self.refused.get_or_insert_with(|| "nothing_to_do".into());
            }
            return;
        };
        let confirmed = self
            .txs
            .iter()
            .any(|t| t.status() == WriteStatus::Confirmed);
        self.status = if confirmed && first_bad != WriteStatus::Unconfirmed {
            WriteStatus::Partial
        } else {
            first_bad
        };
    }
}

impl Observed for WriteResult {
    const SCHEMA: &'static str = "write/1";

    fn subject(&self) -> String {
        format!("{}:{}:{}", self.tool, self.wallet, self.started_at_ms)
    }

    fn headline(&self) -> String {
        let mut h = format!(
            "{} {} {} wallet={}",
            self.tool,
            self.mode.as_str(),
            self.status.as_str(),
            self.wallet
        );
        if let Some(sig) = self.txs.iter().find_map(|t| t.signature.as_deref()) {
            h.push_str(&format!(" tx={sig}"));
        }
        if let Some(r) = &self.refused {
            h.push_str(&format!(" refused: {r}"));
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "mode", Some(self.mode.as_str()));
        set_str(&mut f, "status", Some(self.status.as_str()));
        set_int(&mut f, "txs", Some(self.txs.len() as i64));
        let confirmed = self
            .txs
            .iter()
            .filter(|t| t.status() == WriteStatus::Confirmed)
            .count();
        set_int(&mut f, "confirmed_txs", Some(confirmed as i64));
        let units: Option<u64> = self
            .txs
            .iter()
            .map(|t| t.units)
            .try_fold(0u64, |a, u| u.map(|u| a + u));
        set_int(&mut f, "units", units.map(|u| u as i64));
        set_int(
            &mut f,
            "cu_price",
            self.txs
                .iter()
                .filter_map(|t| t.cu_price_micro_lamports)
                .max()
                .map(|p| p as i64),
        );
        set_int(
            &mut f,
            "checks_failed",
            Some(self.checks.iter().filter(|c| !c.ok).count() as i64),
        );
        if let Some(c) = self.failed_check() {
            set_str(&mut f, "failed_check", Some(&c.name));
        }
        f
    }

    fn status(&self) -> ObsStatus {
        self.status.obs_status()
    }

    fn errors(&self) -> Vec<ReadError> {
        let class = match self.status {
            WriteStatus::Refused => ErrorClass::NotApplicable,
            WriteStatus::Expired => ErrorClass::Transient,
            WriteStatus::SimFailed | WriteStatus::Failed => ErrorClass::Fatal,
            _ => return Vec::new(),
        };
        let message = self
            .refused
            .clone()
            .or_else(|| {
                self.txs
                    .iter()
                    .find(|t| t.status() == self.status)
                    .and_then(|t| {
                        t.note
                            .clone()
                            .or_else(|| t.err.as_ref().map(Value::to_string))
                    })
            })
            .unwrap_or_else(|| self.status.as_str().to_string());
        vec![ReadError::new("write", class, message)]
    }
}

/// Single-writer lease on a resource (`wallet:<address>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub resource: String,
    pub holder: String,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
    pub granted: bool,
    /// Who holds it now (us when granted).
    pub current_holder: String,
}

/// A signed transaction handed to the RPC whose outcome is not yet known.
/// Written BEFORE `sendTransaction`; the next send of the wallet resolves
/// it first (review H3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSend {
    pub wallet: String,
    pub tool: String,
    pub label: String,
    pub signature: String,
    pub last_valid_block_height: u64,
    pub sent_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation};

    #[test]
    fn cu_limit_buffers_and_caps() {
        assert_eq!(cu_limit_for(0), 0);
        assert_eq!(cu_limit_for(100_000), 110_000);
        assert_eq!(cu_limit_for(100_001), 110_002);
        assert_eq!(cu_limit_for(1_300_000), 1_400_000);
        assert_eq!(cu_limit_for(u64::MAX), 1_400_000);
    }

    #[test]
    fn cu_price_is_clamped() {
        assert_eq!(clamp_cu_price(f64::NAN), 1_000);
        assert_eq!(clamp_cu_price(-5.0), 1_000);
        assert_eq!(clamp_cu_price(12.2), 1_000);
        assert_eq!(clamp_cu_price(123_456.1), 123_457);
        assert_eq!(clamp_cu_price(9e9), 5_000_000);
    }

    #[test]
    fn mode_defaults_to_simulate() {
        assert_eq!(WriteMode::parse(None), Ok(WriteMode::Simulate));
        assert_eq!(WriteMode::parse(Some("send")), Ok(WriteMode::Send));
        assert!(WriteMode::parse(Some("live")).is_err());
    }

    fn tx(status: WriteStatus) -> TxReport {
        TxReport {
            status: Some(status),
            ..TxReport::new("t")
        }
    }

    #[test]
    fn settle_rules() {
        let w = || WriteResult::new("dlmm_close_position", "W", WriteMode::Send, 1, "h");
        let settle = |txs: Vec<TxReport>| {
            let mut r = w();
            r.txs = txs;
            r.settle();
            r.status
        };
        use WriteStatus::*;
        assert_eq!(settle(vec![tx(Confirmed), tx(Confirmed)]), Confirmed);
        assert_eq!(settle(vec![tx(Confirmed), tx(Failed)]), Partial);
        assert_eq!(settle(vec![tx(Confirmed), tx(Unconfirmed)]), Unconfirmed);
        assert_eq!(settle(vec![tx(Expired)]), Expired);
        assert_eq!(settle(vec![tx(SimFailed)]), SimFailed);
        assert_eq!(settle(vec![]), Refused);
        let mut s = WriteResult::new("x", "W", WriteMode::Simulate, 1, "h");
        s.txs = vec![tx(Simulated)];
        s.settle();
        assert_eq!(s.status, Simulated);
    }

    #[test]
    fn observation_carries_full_ids_and_status() {
        let sig = "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
        let wallet = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";
        let mut r = WriteResult::new("jupiter_swap", wallet, WriteMode::Send, 42, "rpc.example");
        r.txs = vec![TxReport {
            signature: Some(sig.into()),
            units: Some(1000),
            cu_price_micro_lamports: Some(2000),
            ..tx(WriteStatus::Unconfirmed)
        }];
        r.settle();
        let obs = Observation::of("jupiter_swap", &r, 50, 0, ObsSource::Live);
        assert_eq!(obs.key, format!("write/1:jupiter_swap:{wallet}:42"));
        assert_eq!(obs.status, ObsStatus::Partial);
        assert!(obs.headline.contains(sig) && obs.headline.contains(wallet));
        assert_features_ok(&obs.features);
        let text = obs.render_text(50);
        assert!(text.contains(sig), "{text}");

        let refused = WriteResult::new("jupiter_swap", wallet, WriteMode::Send, 42, "h")
            .refuse("lease_held: another process is sending for this wallet");
        let obs = Observation::of("jupiter_swap", &refused, 50, 0, ObsSource::Live);
        assert_eq!(obs.status, ObsStatus::Error);
        assert_eq!(obs.errors[0].class, ErrorClass::NotApplicable);
        assert!(obs.headline.contains("refused: lease_held"));
    }

    #[test]
    fn logs_tail_keeps_whole_last_lines() {
        let logs: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let t = logs_tail(&logs);
        assert_eq!(t.len(), LOG_TAIL);
        assert_eq!(t[0], "line 5");
        assert_eq!(logs_tail(&logs[..3]).len(), 3);
    }
}
