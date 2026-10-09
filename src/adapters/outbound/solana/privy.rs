//! Privy wallet signer (`ports::solana_signer::SolanaSigner`) — the Solana
//! write tools' `mode = "send"` signs with the `[solana] privy_wallet_id`
//! wallet through the seal proxy's `privy` route: the Worker adds the Privy
//! app secret, this process holds none (`docs/sealed-keys-2026-10-09.md`).
//!
//! | Step | Rule |
//! |---|---|
//! | build ([`PrivySigner::for_send`]) | `PRIVY_API_URL` and `PRIVY_APP_ID` read through the tool's `env_reads`; `PRIVY_API_URL` must be on the seal proxy with a session (`keys::auth_header_for`) — never Privy directly, never a local app secret |
//! | each call | `[egress]` `check_url` + the tool's `net_hosts`, one audit record (`tool`, host, path, verdict, status, ms) |
//! | request | `POST <proxy>/privy/v1/wallets/<id>/rpc` `{"method":"signTransaction","params":{"transaction":<base64>,"encoding":"base64"}}` — the message with zeroed signature slots |
//! | reply ([`signed_slot`]) | the message must come back byte for byte; only the wallet's slot is taken, and it must verify (ed25519) for the wallet — else nothing is signed |

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, VerifyingKey};
use serde_json::{json, Value};

use crate::adapters::outbound::{egress, keys};
use crate::domain::scope::ToolScope;
use crate::domain::solana::{Pubkey, Signature};
use crate::domain::solana_tx::{MessageView, Transaction};
use crate::domain::solana_write::valid_privy_wallet_id;
use crate::ports::solana_signer::SolanaSigner;

/// Env var naming the Privy base URL — `[keys.env] PRIVY_API_URL = "privy"`.
pub(crate) const API_URL_ENV: &str = "PRIVY_API_URL";
/// Env var with the Privy app id (public; sent as `privy-app-id`).
pub(crate) const APP_ID_ENV: &str = "PRIVY_APP_ID";
const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct PrivySigner {
    wallet: Pubkey,
    url: reqwest::Url,
    app_id: String,
    /// `Bearer <session>` for the seal proxy.
    auth: String,
    http: reqwest::Client,
    scope: ToolScope,
    /// The write tool that asked (audit).
    tool: String,
}

impl std::fmt::Debug for PrivySigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PrivySigner({})", self.wallet)
    }
}

impl PrivySigner {
    /// The signer for `wallet` (Privy wallet `wallet_id`), or the refusal
    /// reason. Reads the env through `scope`.
    pub(crate) fn for_send(
        http: reqwest::Client,
        scope: &ToolScope,
        wallet_id: &str,
        wallet: Pubkey,
        tool: &str,
    ) -> std::result::Result<Self, String> {
        let env = |name: &str| -> std::result::Result<String, String> {
            scope.check_env_read(name).map_err(|_| {
                format!("signer_unavailable: {name} is not in this agent's env_reads for {tool}")
            })?;
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("signer_unavailable: {name} is not set"))
        };
        let base = env(API_URL_ENV)?;
        let app_id = env(APP_ID_ENV)?;
        let url = rpc_url(&base, wallet_id).map_err(|e| format!("signer_unavailable: {e}"))?;
        let auth = keys::auth_header_for(url.as_str()).ok_or_else(|| {
            "signer_unavailable: PRIVY_API_URL is not the seal proxy with a session \
             ([keys.env] PRIVY_API_URL = \"privy\"; `tengu keys status`) — Privy is \
             never called with a local app secret"
                .to_string()
        })?;
        Ok(Self::new(
            http,
            url,
            app_id,
            auth,
            wallet,
            scope.clone(),
            tool,
        ))
    }

    pub(crate) fn new(
        http: reqwest::Client,
        url: reqwest::Url,
        app_id: String,
        auth: String,
        wallet: Pubkey,
        scope: ToolScope,
        tool: &str,
    ) -> Self {
        PrivySigner {
            wallet,
            url,
            app_id,
            auth,
            http,
            scope,
            tool: tool.to_string(),
        }
    }

    fn audit(&self, verdict: &str, status: Option<u16>, reason: Option<String>, ms: Option<u64>) {
        egress::policy().audit(json!({
            "tool": self.tool, "host": self.url.host_str(), "path": self.url.path(),
            "verdict": verdict, "status": status, "reason": reason, "ms": ms,
        }));
    }
}

/// `<base>/v1/wallets/<wallet_id>/rpc`; the id must be a plain path segment.
pub(crate) fn rpc_url(base: &str, wallet_id: &str) -> Result<reqwest::Url> {
    if !valid_privy_wallet_id(wallet_id) {
        bail!("privy wallet id must be 1-64 chars of A-Z a-z 0-9 _ -");
    }
    reqwest::Url::parse(&format!(
        "{}/v1/wallets/{wallet_id}/rpc",
        base.trim_end_matches('/')
    ))
    .map_err(|_| anyhow!("{API_URL_ENV} is not a valid URL"))
}

/// The `signTransaction` body for `message`: zeroed signature slots.
pub(crate) fn request_body(message: &[u8]) -> Result<Value> {
    let view = MessageView::parse(message).map_err(|e| anyhow!("message does not parse: {e}"))?;
    let tx = Transaction {
        signatures: vec![[0u8; 64]; usize::from(view.header.num_required_signatures)],
        message: message.to_vec(),
    };
    Ok(json!({
        "method": "signTransaction",
        "params": {
            "transaction": base64::engine::general_purpose::STANDARD.encode(tx.serialize()),
            "encoding": "base64",
        },
    }))
}

/// The wallet's signature out of Privy's reply: the message must be ours,
/// byte for byte, and the signature must verify for `wallet`.
pub(crate) fn signed_slot(message: &[u8], wallet: &Pubkey, reply: &Value) -> Result<Signature> {
    let b64 = reply["data"]["signed_transaction"]
        .as_str()
        .ok_or_else(|| anyhow!("privy reply has no data.signed_transaction"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("privy signed_transaction is not base64")?;
    let (signed, view) =
        Transaction::parse(&bytes).map_err(|e| anyhow!("privy signed_transaction: {e}"))?;
    if signed.message != message {
        bail!("privy returned a different message — not signed");
    }
    let i = view
        .signer_index(wallet)
        .ok_or_else(|| anyhow!("{wallet} is not a required signer of this transaction"))?;
    let sig = signed.signatures[i];
    let key = VerifyingKey::from_bytes(&wallet.0)
        .map_err(|_| anyhow!("{wallet} is not an ed25519 public key"))?;
    key.verify_strict(message, &EdSignature::from_bytes(&sig))
        .map_err(|_| {
            anyhow!(
                "privy's signature does not verify for {wallet} — is [solana] privy_wallet_id \
                 this wallet?"
            )
        })?;
    Ok(Signature(sig))
}

#[async_trait]
impl SolanaSigner for PrivySigner {
    fn pubkey(&self) -> Pubkey {
        self.wallet
    }

    async fn sign(&self, message: &[u8]) -> Result<Signature> {
        let host = self.url.host_str().unwrap_or("").to_string();
        if let Err(e) = egress::policy()
            .check_url(&self.url)
            .and_then(|_| self.scope.check_net_host(&host))
        {
            self.audit("denied", None, Some(format!("{e:#}")), None);
            return Err(e);
        }
        let body = request_body(message)?;
        let started = Instant::now();
        let ms = || Some(started.elapsed().as_millis() as u64);
        let resp = self
            .http
            .post(self.url.clone())
            .timeout(TIMEOUT)
            .header(reqwest::header::AUTHORIZATION, &self.auth)
            .header("privy-app-id", &self.app_id)
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let e = e.without_url();
                self.audit("error", None, Some(e.to_string()), ms());
                bail!("privy signTransaction @ {host}: {e}");
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        self.audit("allowed", Some(status.as_u16()), None, ms());
        if !status.is_success() {
            let why = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            bail!("privy signTransaction @ {host}: {status}: {why}");
        }
        let reply: Value = serde_json::from_str(&text)
            .with_context(|| format!("privy signTransaction @ {host}: reply is not JSON"))?;
        signed_slot(message, &self.wallet, &reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::http_class::test_support::{canned, serve, test_client};
    use crate::adapters::outbound::solana::signer::{sign_transaction, LocalKeypair};
    use crate::domain::solana_tx::golden::{golden, ix, key};
    use crate::domain::solana_tx::{Instruction, LegacyMessage};

    /// The `legacy_open` golden message: slots wallet ([1; 32]) + position ([2; 32]).
    fn message() -> Vec<u8> {
        message_with(None)
    }

    /// The same message, optionally with another blockhash.
    fn message_with(blockhash: Option<[u8; 32]>) -> Vec<u8> {
        let m = golden()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "legacy_open")
            .cloned()
            .unwrap();
        let ixs: Vec<Instruction> = m["ixs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| ix(n.as_str().unwrap()))
            .collect();
        let payer = key(m["payer"].as_str().unwrap());
        let bh = blockhash.unwrap_or(key(m["blockhash"].as_str().unwrap()).0);
        LegacyMessage::compile(&payer, &ixs, bh)
            .unwrap()
            .serialize()
    }

    /// What Privy answers: the transaction with `by`'s slot signed.
    fn privy_reply(message: &[u8], by: &LocalKeypair) -> Value {
        let body = request_body(message).unwrap();
        let wire = base64::engine::general_purpose::STANDARD
            .decode(body["params"]["transaction"].as_str().unwrap())
            .unwrap();
        let (mut tx, view) = Transaction::parse(&wire).unwrap();
        if let Some(i) = view.signer_index(&by.pubkey()) {
            tx.signatures[i] = by.sign_now(message).0;
        }
        json!({"method": "signTransaction", "data": {
            "signed_transaction": base64::engine::general_purpose::STANDARD.encode(tx.serialize()),
            "encoding": "base64"}})
    }

    fn wallet() -> LocalKeypair {
        LocalKeypair::from_seed(&[1; 32])
    }

    #[test]
    fn request_carries_the_message_with_zeroed_slots() {
        let msg = message();
        let body = request_body(&msg).unwrap();
        assert_eq!(body["method"], "signTransaction");
        assert_eq!(body["params"]["encoding"], "base64");
        let wire = base64::engine::general_purpose::STANDARD
            .decode(body["params"]["transaction"].as_str().unwrap())
            .unwrap();
        let (tx, _) = Transaction::parse(&wire).unwrap();
        assert_eq!(tx.message, msg);
        assert_eq!(tx.signatures, vec![[0u8; 64]; 2]);
        assert!(request_body(b"junk").is_err());
    }

    #[test]
    fn takes_only_a_verified_wallet_slot_of_our_message() {
        let msg = message();
        let w = wallet();
        let sig = signed_slot(&msg, &w.pubkey(), &privy_reply(&msg, &w)).unwrap();
        assert_eq!(sig, w.sign_now(&msg));

        // Another wallet answered (wrong privy_wallet_id): our slot stays empty.
        let stranger = LocalKeypair::from_seed(&[9; 32]);
        let err = signed_slot(&msg, &w.pubkey(), &privy_reply(&msg, &stranger)).unwrap_err();
        assert!(err.to_string().contains("does not verify"), "{err}");

        // A changed message is refused even though its signature is valid.
        let other = message_with(Some([7; 32]));
        let mut reply = privy_reply(&other, &w);
        assert!(signed_slot(&other, &w.pubkey(), &reply).is_ok());
        let err = signed_slot(&msg, &w.pubkey(), &reply).unwrap_err();
        assert!(err.to_string().contains("different message"), "{err}");

        reply["data"]["signed_transaction"] = json!("not base64!");
        assert!(signed_slot(&msg, &w.pubkey(), &reply).is_err());
        assert!(signed_slot(&msg, &w.pubkey(), &json!({"data": {}})).is_err());
    }

    #[test]
    fn wallet_ids_are_plain_path_segments() {
        assert!(valid_privy_wallet_id("cmabc123-def_45"));
        for bad in ["", "a/b", "..", "a?b", "a b", "a%2f", &"x".repeat(65)] {
            assert!(!valid_privy_wallet_id(bad), "{bad}");
        }
        let u = rpc_url("https://seal.example.workers.dev/privy/", "w1").unwrap();
        assert_eq!(
            u.as_str(),
            "https://seal.example.workers.dev/privy/v1/wallets/w1/rpc"
        );
        assert!(rpc_url("https://seal.example.workers.dev/privy", "../x").is_err());
    }

    #[test]
    fn needs_the_proxy_session_and_env_reads() {
        let scope = ToolScope::default();
        let err =
            PrivySigner::for_send(test_client(), &scope, "w1", wallet().pubkey(), "t").unwrap_err();
        assert!(
            err.starts_with("signer_unavailable: PRIVY_API_URL is not in"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn signs_through_the_route_and_fills_the_wallet_slot() {
        let msg = message();
        let w = wallet();
        let (base, seen) = serve(vec![canned(200, privy_reply(&msg, &w).to_string())]).await;
        let signer = PrivySigner::new(
            test_client(),
            rpc_url(&format!("{base}/privy"), "w1").unwrap(),
            "app-1".into(),
            "Bearer tss1.test".into(),
            w.pubkey(),
            ToolScope {
                net_hosts: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            "jupiter_swap",
        );
        let position = LocalKeypair::from_seed(&[2; 32]);
        let mut tx = Transaction {
            signatures: vec![[0u8; 64]; 2],
            message: msg.clone(),
        };
        let view = MessageView::parse(&msg).unwrap();
        sign_transaction(&mut tx, &view, &[&position, &signer])
            .await
            .unwrap();
        let mut local = tx.clone();
        sign_transaction(&mut local, &view, &[&position, &w])
            .await
            .unwrap();
        assert_eq!(tx, local, "same bytes as a local wallet key would give");

        let req = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("post /privy/v1/wallets/w1/rpc "), "{req}");
        assert!(req.contains("authorization: bearer tss1.test"), "{req}");
        assert!(req.contains("privy-app-id: app-1"), "{req}");
        assert!(!req.contains("basic "), "no app secret: {req}");
    }

    #[tokio::test]
    async fn a_privy_error_or_a_net_host_denial_signs_nothing() {
        let msg = message();
        let w = wallet();
        let (base, _) = serve(vec![canned(400, r#"{"error":"Policy violation"}"#)]).await;
        let mk = |net_hosts: Vec<String>| {
            PrivySigner::new(
                test_client(),
                rpc_url(&format!("{base}/privy"), "w1").unwrap(),
                "app-1".into(),
                "Bearer tss1.test".into(),
                w.pubkey(),
                ToolScope {
                    net_hosts,
                    ..Default::default()
                },
                "jupiter_swap",
            )
        };
        let err = mk(vec!["127.0.0.1".into()]).sign(&msg).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("400 Bad Request: Policy violation"),
            "{err}"
        );
        assert!(mk(vec!["other.host".into()]).sign(&msg).await.is_err());
    }
}

/// A fake Privy for pipeline tests: answers every `signTransaction` by
/// signing the wallet's slot with a local key, and records each request.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::adapters::outbound::solana::signer::LocalKeypair;
    use crate::domain::solana_tx::Transaction;
    use crate::ports::solana_signer::SolanaSigner;

    /// Base URL (`http://127.0.0.1:<port>`) + the raw requests seen.
    pub(crate) async fn fake_privy(wallet: LocalKeypair) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let body = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let Some(pos) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len = text[..pos]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + len {
                        break Some(buf[pos + 4..pos + 4 + len].to_vec());
                    }
                };
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let reply = body
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .map(|req| sign_reply(&wallet, &req))
                    .unwrap_or_else(|| json!({"error": "bad request"}));
                let text = reply.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn sign_reply(wallet: &LocalKeypair, req: &Value) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD;
        let wire = b64
            .decode(req["params"]["transaction"].as_str().unwrap_or(""))
            .unwrap_or_default();
        let Ok((mut tx, view)) = Transaction::parse(&wire) else {
            return json!({"error": "bad transaction"});
        };
        if let Some(i) = view.signer_index(&wallet.pubkey()) {
            tx.signatures[i] = wallet.sign_now(&tx.message).0;
        }
        json!({"method": "signTransaction", "data": {
            "signed_transaction": b64.encode(tx.serialize()), "encoding": "base64"}})
    }
}
