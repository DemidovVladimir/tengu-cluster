//! `SecretRegistry` — secret values to redact from tool output, transcripts
//! and memory. Pure in-memory; the encrypted vault file that feeds it lives
//! in `adapters/outbound/secrets.rs`, which also registers the credentials
//! the process env names ([`is_env_secret`]: `.env` and exported keys).

use std::collections::HashSet;

use serde_json::Value;

use crate::domain::observation::Observation;

/// Env var name suffixes (ASCII case-insensitive) whose values are
/// credentials: [`is_env_secret`].
pub(crate) const SECRET_ENV_SUFFIXES: [&str; 5] =
    ["_API_KEY", "_SECRET", "_TOKEN", "_PASSWORD", "_PRIVATE_KEY"];

/// Shortest env value [`is_env_secret`] registers: a shorter one would
/// redact common words and numbers out of every tool result.
pub(crate) const MIN_ENV_SECRET_CHARS: usize = 8;

/// Placeholder values (lower-case) that are never credentials.
const PLACEHOLDERS: [&str; 18] = [
    "changeme",
    "change-me",
    "change_me",
    "password",
    "passw0rd",
    "postgres",
    "placeholder",
    "example",
    "undefined",
    "not-used",
    "not_used",
    "notused",
    "unused",
    "disabled",
    "anonymous",
    "localhost",
    "replace-me",
    "replaceme",
];

/// Whether env var `name` = `value` is a credential to redact from tool
/// output (`adapters/outbound/secrets.rs::process_secret_registry`, every
/// surface). Public on-chain ids are not secrets and stay visible in full
/// (operator rule): never an EVM address, and under `*_TOKEN` (a token is
/// also an asset: `USDC_TOKEN=<mint>`) never a Solana address / signature or
/// a `0x` 32-byte hash. Under the other names a 32 / 64-byte value is key
/// material (a seed, a keypair, an EVM private key) and is registered.
///
/// | Check | Rule |
/// |---|---|
/// | Name | ends with a [`SECRET_ENV_SUFFIXES`] entry |
/// | Value (trimmed) | ≥ [`MIN_ENV_SECRET_CHARS`] chars |
/// | Never | a placeholder (`changeme`, `postgres`, `your-…`), one repeated char, a plain number, a template (`$VAR`, `${…}`, `<…>`), an EVM address (`0x` + 40 hex) |
/// | `*_TOKEN` only, never | base58 of 32 or 64 bytes (Solana address, signature), `0x` + 64 hex |
pub(crate) fn is_env_secret(name: &str, value: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Some(suffix) = SECRET_ENV_SUFFIXES
        .iter()
        .find(|s| upper.len() > s.len() && upper.ends_with(*s))
    else {
        return false;
    };
    let v = value.trim();
    if v.chars().count() < MIN_ENV_SECRET_CHARS || is_placeholder(v) || is_evm_address(v) {
        return false;
    }
    !(*suffix == "_TOKEN" && is_chain_id(v))
}

fn is_placeholder(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    let mut chars = v.chars();
    let first = chars.next();
    PLACEHOLDERS.contains(&lower.as_str())
        || lower.starts_with("your")
        || is_template(v)
        || chars.all(|c| Some(c) == first)
        || v.parse::<f64>().is_ok()
}

/// An unexpanded reference: `${…}`, `$NAME` (upper-case name), `<…>`.
fn is_template(v: &str) -> bool {
    let var = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && s.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    };
    (v.starts_with("${") && v.ends_with('}'))
        || v.strip_prefix('$').is_some_and(var)
        || (v.starts_with('<') && v.ends_with('>'))
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `0x` + 40 hex: an EVM account or contract address.
fn is_evm_address(v: &str) -> bool {
    v.strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .is_some_and(|h| h.len() == 40 && is_hex(h))
}

/// A Solana address (32 bytes) or signature (64 bytes) in base58, or an
/// EVM 32-byte hash (`0x` + 64 hex).
fn is_chain_id(v: &str) -> bool {
    if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        return h.len() == 64 && is_hex(h);
    }
    (32..=88).contains(&v.len())
        && crate::domain::solana::bs58_decode(v).is_ok_and(|b| b.len() == 32 || b.len() == 64)
}

/// Registry of secret values that must never appear in tool output,
/// flow transcripts, or memory storage.
pub(crate) struct SecretRegistry {
    /// Sorted longest-first to avoid partial-match issues during redaction.
    values: Vec<String>,
    seen: HashSet<String>,
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self {
            values: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Register a secret value for redaction. Empty or duplicate values are ignored.
    pub fn register(&mut self, value: String) {
        if value.is_empty() || self.seen.contains(&value) {
            return;
        }
        self.seen.insert(value.clone());
        self.values.push(value);
        // Re-sort longest-first so longer secrets are replaced before
        // any shorter substring match.
        self.values.sort_by(|a, b| b.len().cmp(&a.len()));
    }

    /// Replace every occurrence of any registered secret value with `[REDACTED]`.
    pub fn redact(&self, text: &str) -> String {
        if self.values.is_empty() {
            return text.to_string();
        }
        let mut result = text.to_string();
        for secret in &self.values {
            result = result.replace(secret.as_str(), "[REDACTED]");
        }
        result
    }

    /// `redact` every string leaf of `v` in place.
    pub fn redact_value(&self, v: &mut Value) {
        if self.values.is_empty() {
            return;
        }
        match v {
            Value::String(s) => {
                if self.values.iter().any(|x| s.contains(x.as_str())) {
                    *s = self.redact(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.redact_value(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.redact_value(x)),
            _ => {}
        }
    }

    /// Redact a typed observation: headline, `errors[].message`, string
    /// features and the whole `data` payload. The cache key is left alone.
    pub fn redact_observation(&self, obs: &mut Observation) {
        if self.values.is_empty() {
            return;
        }
        obs.headline = self.redact(&obs.headline);
        for e in &mut obs.errors {
            e.message = self.redact(&e.message);
        }
        obs.features.values_mut().for_each(|v| self.redact_value(v));
        self.redact_value(&mut obs.data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, ReadError};
    use serde_json::json;

    // Public on-chain ids, in full: the USDC mint, a wallet and a transaction
    // signature (Solana mainnet), USDC's Ethereum contract, an EVM tx-hash
    // shaped value.
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const SIGNATURE: &str =
        "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi";
    const EVM_USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const EVM_TX: &str = "0x5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060";

    #[test]
    fn env_secret_names_and_values() {
        let yes = [
            ("OPENROUTER_API_KEY", "sk-or-v1-0123456789abcdef"),
            ("PRIVY_APP_SECRET", "privy-app-secret-0123456789"),
            (
                "TELEGRAM_BOT_TOKEN",
                "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw",
            ),
            ("GITHUB_TOKEN", "ghp_0123456789abcdefABCDEF0123456789abcd"),
            ("POSTGRES_PASSWORD", "s3cr3t-Pass!"),
            ("my_api_key", "lower-case-name-123"),
            ("TENGU_MASTER_PASSWORD", "master-password-1"),
            ("PASSWORD_A_PASSWORD", "  padded-secret-9  "),
            ("PIN_PASSWORD", "$ecretPass1"),
            // Key material under a credential name, whatever its shape: a
            // 32-byte seed / keypair in base58, an EVM private key.
            ("SOLANA_PRIVATE_KEY", WALLET),
            ("SOLANA_PRIVATE_KEY", SIGNATURE),
            ("WALLET_SECRET", SIGNATURE),
            ("ETH_PRIVATE_KEY", EVM_TX),
            ("SIGNER_API_KEY", USDC_MINT),
        ];
        for (name, value) in yes {
            assert!(is_env_secret(name, value), "{name}={value} must register");
        }
        let no = [
            // Not a credential name.
            ("HOME", "/Users/operator/home"),
            ("MAX_TOKENS", "16000000"),
            ("TOKEN", "bare-name-0123456"),
            ("_TOKEN", "empty-prefix-0123"),
            ("PRIVATE_KEY", "bare-name-0123456"),
            ("SOLANA_RPC_URL", "https://rpc.example/?api-key=0123456789"),
            // Too short, placeholders, numbers, templates.
            ("POSTGRES_PASSWORD", "tengu"),
            ("OLLAMA_API_KEY", "ollama"),
            ("POSTGRES_PASSWORD", "postgres"),
            ("DB_PASSWORD", "ChangeMe"),
            ("OPENROUTER_API_KEY", "your-openrouter-key"),
            ("SOME_API_KEY", "xxxxxxxxxxxx"),
            ("CHAT_TOKEN", "1234567890"),
            ("KEY_SECRET", "${OPENROUTER_API_KEY}"),
            ("KEY_SECRET", "$OPENROUTER_API_KEY"),
            ("KEY_SECRET", "<paste your key>"),
            ("EMPTY_SECRET", "   "),
            // Public on-chain ids: an EVM address under any name; Solana
            // addresses / signatures and 32-byte hashes under `*_TOKEN`.
            ("REWARD_TOKEN", EVM_USDC),
            ("TREASURY_SECRET", EVM_USDC),
            ("USDC_TOKEN", USDC_MINT),
            ("LP_OWNER_TOKEN", WALLET),
            ("LAST_TX_TOKEN", SIGNATURE),
            ("LAST_TX_TOKEN", EVM_TX),
        ];
        for (name, value) in no {
            assert!(
                !is_env_secret(name, value),
                "{name}={value} must not register"
            );
        }
    }

    /// The env rule never registers a public id under `*_TOKEN`, so a tool
    /// result naming the mint, a wallet or a signature keeps them in full
    /// while the credentials beside them are redacted.
    #[test]
    fn env_secrets_redact_keys_and_leave_chain_ids_whole() {
        let env = [
            ("OPENROUTER_API_KEY", "sk-or-v1-0123456789abcdef"),
            ("USDC_TOKEN", USDC_MINT),
            ("BASE_TOKEN", EVM_USDC),
            (
                "TELEGRAM_BOT_TOKEN",
                "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw",
            ),
        ];
        let mut r = SecretRegistry::new();
        for (name, value) in env {
            if is_env_secret(name, value) {
                r.register(value.trim().to_string());
            }
        }
        let text = format!(
            "swap {USDC_MINT} for {WALLET} sig {SIGNATURE} via {EVM_USDC} tx {EVM_TX} \
             key=sk-or-v1-0123456789abcdef bot=123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw"
        );
        let out = r.redact(&text);
        for id in [USDC_MINT, WALLET, SIGNATURE, EVM_USDC, EVM_TX] {
            assert!(out.contains(id), "{id} mangled: {out}");
        }
        assert!(!out.contains("sk-or-v1-0123456789abcdef"), "{out}");
        assert!(!out.contains("AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw"), "{out}");
        assert_eq!(out.matches("[REDACTED]").count(), 2, "{out}");
    }

    #[test]
    fn redact_value_walks_string_leaves() {
        let mut r = SecretRegistry::new();
        r.register("sk-secret".into());
        let mut v = json!({"a": ["x sk-secret y", 1], "b": {"c": "sk-secret"}});
        r.redact_value(&mut v);
        assert_eq!(
            v,
            json!({"a": ["x [REDACTED] y", 1], "b": {"c": "[REDACTED]"}})
        );
    }

    #[test]
    fn redact_observation_covers_every_text_field() {
        let mut r = SecretRegistry::new();
        r.register("sk-secret".into());
        let mut obs = Observation {
            key: "k/1:s".into(),
            schema: "k/1".into(),
            tool: "t".into(),
            observed_at_ms: 0,
            slot: None,
            ttl_ms: 1,
            source: ObsSource::Live,
            status: ObsStatus::Partial,
            errors: vec![ReadError::new("f", ErrorClass::Fatal, "bad sk-secret")],
            headline: "h sk-secret".into(),
            features: [("s".to_string(), json!("sk-secret"))].into(),
            data: json!({"url": "https://rpc/?api-key=sk-secret"}),
        };
        r.redact_observation(&mut obs);
        let all = serde_json::to_string(&obs).unwrap();
        assert!(!all.contains("sk-secret"), "{all}");
    }
}
