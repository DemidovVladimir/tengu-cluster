//! Pin hashes, pure (`docs/lineage-2026-10-06.md` § 2): what a generation's
//! `[[pins]]`, a frozen manifest and a seal hash. The adapters read the files
//! (`adapters/outbound/lineage/probe.rs`, `config/lineage.rs`) and hand the
//! text here.
//!
//! | Target | sha256 (64 hex, never shortened) of |
//! |---|---|
//! | a record file ([`toml_digest`]) | the canonical JSON (`domain/canonical.rs`) of the file parsed TOML → JSON — comments and layout do not count |
//! | `config:<sandbox>/<dotted path>` ([`config_pin`]) | the canonical JSON of that value of `sandboxes/<sandbox>/config.toml` (raw text: `${VAR}` not substituted) |
//! | `spec:<sandbox>/<strategy>` ([`spec_pin`]) | the normalized spec = a run's `spec_sha256` (`StrategySpec::from_value` → `spec::spec_sha256`) |
//! | `tool_schema:<tool>` ([`schema_pin`]) | the canonical JSON of the catalog tool's input schema |
//! | `skill:<name>` · `repo:<path>` ([`bytes_sha256`]) | the file's bytes |

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::domain::backtest::spec::{spec_sha256, StrategySpec};
use crate::domain::canonical::canonical_sha256;

/// TOML text → JSON (tables → objects, arrays kept in order).
pub fn toml_to_json(text: &str) -> Result<Value, String> {
    toml::from_str::<Value>(text).map_err(|e| format!("TOML: {e}"))
}

/// A record file's digest (module table).
pub fn toml_digest(text: &str) -> Result<String, String> {
    toml_to_json(text).map(|v| canonical_sha256(&v))
}

/// sha256 of bytes, 64 lowercase hex.
pub fn bytes_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The value at `path` of a TOML text.
pub fn config_value(text: &str, path: &[String]) -> Result<Value, String> {
    let mut v = &toml_to_json(text)?;
    for (i, seg) in path.iter().enumerate() {
        v = v.get(seg.as_str()).ok_or_else(|| {
            let at = path[..=i]
                .iter()
                .map(|s| quote_segment(s))
                .collect::<Vec<_>>()
                .join(".");
            format!("no value at `{at}`")
        })?;
    }
    Ok(v.clone())
}

/// A segment as the dotted syntax writes it: bare when it can be.
pub fn quote_segment(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// `config:` pin (module table).
pub fn config_pin(text: &str, path: &[String]) -> Result<String, String> {
    config_value(text, path).map(|v| canonical_sha256(&v))
}

/// `spec:` pin (module table): the strategy parsed and validated as a run
/// parses it.
pub fn spec_pin(text: &str, strategy: &str) -> Result<String, String> {
    let path = ["backtest", "strategies", strategy].map(String::from);
    let value = config_value(text, &path)?;
    let spec = StrategySpec::from_value(strategy, &value).map_err(|e| e.join("; "))?;
    Ok(spec_sha256(&spec.to_value()))
}

/// `tool_schema:` pin (module table).
pub fn schema_pin(schema: &Value) -> String {
    canonical_sha256(schema)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CONFIG: &str = r#"
# a comment never counts
[backtest.costs."hyperliquid:xyz:"]
taker_fee_bps = 0.9
half_spread = { model = "fixed", bps = 1.0 }

[backtest.strategies.weekend_fade]
kind = "weekend_window"
universe = ["hyperliquid:xyz:AAPL"]
interval = "1h"
calendar = "us_equity"
direction = "fade"
"#;

    #[test]
    fn a_config_pin_is_the_canonical_value_whatever_the_layout() {
        let path = ["backtest", "costs", "hyperliquid:xyz:"].map(String::from);
        let pin = config_pin(CONFIG, &path).unwrap();
        assert_eq!(
            pin,
            canonical_sha256(
                &json!({"half_spread": {"bps": 1.0, "model": "fixed"}, "taker_fee_bps": 0.9})
            )
        );
        let relaid = CONFIG.replace(
            "half_spread = { model = \"fixed\", bps = 1.0 }",
            "half_spread.bps = 1.0\nhalf_spread.model = \"fixed\"",
        );
        assert_eq!(config_pin(&relaid, &path).unwrap(), pin);
        assert_eq!(toml_digest(CONFIG).unwrap(), toml_digest(&relaid).unwrap());
        let changed = CONFIG.replace("0.9", "1.0");
        assert_ne!(config_pin(&changed, &path).unwrap(), pin);
        let missing = ["backtest", "costs", "solana:"].map(String::from);
        assert_eq!(
            config_pin(CONFIG, &missing).unwrap_err(),
            "no value at `backtest.costs.\"solana:\"`"
        );
    }

    #[test]
    fn a_spec_pin_is_the_runs_spec_sha256() {
        let pin = spec_pin(CONFIG, "weekend_fade").unwrap();
        let value = config_value(
            CONFIG,
            &["backtest", "strategies", "weekend_fade"].map(String::from),
        )
        .unwrap();
        let spec = StrategySpec::from_value("weekend_fade", &value).unwrap();
        assert_eq!(pin, canonical_sha256(&spec.to_value()));
        assert_eq!(pin.len(), 64);
        // Defaults filled: writing one out keeps the hash.
        let explicit = CONFIG.replace(
            "direction = \"fade\"",
            "direction = \"fade\"\nmin_abs_signal_bps = 0",
        );
        assert_eq!(spec_pin(&explicit, "weekend_fade").unwrap(), pin);
        assert!(spec_pin(CONFIG, "nope").is_err());
        assert_eq!(
            bytes_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
