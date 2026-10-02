//! `[recorder]` — which observations the history recorder keeps
//! (`ops-history-recorder`, tracker conventions 3 + 5). Rows land in
//! `<TENGU_HOME>/state/<xmarket.state>/history/<YYYYMMDD>.db`
//! (`adapters/outbound/history_sqlite.rs`) through `RecordingObservationStore`
//! (`adapters/outbound/observations.rs`). `deny_unknown_fields`.
//!
//! | Field | Default | Effect |
//! |---|---|---|
//! | `enabled` | `false` | needs `[xmarket]` (the state dir) and a non-empty `schemas` |
//! | `schemas` | `[]` | observation schemas to record (`mkt_ctx/1`, `hl_book/1`, …); `"*"` = all |
//! | `keep_data` | `[]` | schemas whose full `data` payload is stored; others keep features only |
//! | `change_only` | `true` | skip a row equal to its key's last recorded row (status, errors, features without `venue_ts_ms`, kept data) … |
//! | `heartbeat_secs` | `300` | … until that row is this old, so `asof` always finds a recent row; `0` = never |
//! | `min_interval_secs` | `{}` | per schema: at most one row per key per interval (changed or not) |
//! | `retention_days` | `30` | day files older than this are deleted (on open, at each new day); `0` = keep all |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `[recorder]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub schemas: Vec<String>,
    #[serde(default)]
    pub keep_data: Vec<String>,
    #[serde(default = "default_change_only")]
    pub change_only: bool,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default)]
    pub min_interval_secs: BTreeMap<String, u64>,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schemas: Vec::new(),
            keep_data: Vec::new(),
            change_only: default_change_only(),
            heartbeat_secs: default_heartbeat_secs(),
            min_interval_secs: BTreeMap::new(),
            retention_days: default_retention_days(),
        }
    }
}

fn default_change_only() -> bool {
    true
}

fn default_heartbeat_secs() -> u64 {
    300
}

fn default_retention_days() -> u32 {
    30
}

fn listed(list: &[String], schema: &str) -> bool {
    list.iter().any(|s| s == "*" || s == schema)
}

impl RecorderConfig {
    /// Rows of `schema` are recorded.
    pub fn records(&self, schema: &str) -> bool {
        self.enabled && listed(&self.schemas, schema)
    }

    /// Rows of `schema` keep their `data` payload.
    pub fn keeps_data(&self, schema: &str) -> bool {
        listed(&self.keep_data, schema)
    }

    pub fn min_interval_ms(&self, schema: &str) -> u64 {
        self.min_interval_secs
            .get(schema)
            .map_or(0, |s| s.saturating_mul(1000))
    }

    pub fn heartbeat_ms(&self) -> u64 {
        self.heartbeat_secs.saturating_mul(1000)
    }

    /// `has_xmarket`: the config has an `[xmarket]` section (the state dir).
    pub fn validation_errors(&self, has_xmarket: bool) -> Vec<String> {
        let mut errors = Vec::new();
        if self.enabled && !has_xmarket {
            errors.push(
                "recorder.enabled needs [xmarket] (history lives in <TENGU_HOME>/state/<xmarket.state>/history)"
                    .to_string(),
            );
        }
        if self.enabled && self.schemas.is_empty() {
            errors.push("recorder.schemas is empty: nothing would be recorded".to_string());
        }
        for s in &self.schemas {
            if s != "*" && !is_schema(s) {
                errors.push(format!(
                    "recorder.schemas `{s}` is not <name>/<version> (or \"*\")"
                ));
            }
        }
        let others = self.keep_data.iter().map(|s| ("keep_data", s)).chain(
            self.min_interval_secs
                .keys()
                .map(|s| ("min_interval_secs", s)),
        );
        for (field, s) in others {
            if !(s == "*" && field == "keep_data") && !listed(&self.schemas, s) {
                errors.push(format!("recorder.{field} `{s}` is not in recorder.schemas"));
            }
        }
        errors
    }
}

/// `<name>/<version>`: name `[A-Za-z0-9_]+`, version digits (`mkt_ctx/1`).
fn is_schema(s: &str) -> bool {
    s.split_once('/').is_some_and(|(name, version)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !version.is_empty()
            && version.chars().all(|c| c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_record_nothing() {
        let r: RecorderConfig = toml::from_str("").unwrap();
        assert_eq!(r, RecorderConfig::default());
        assert!(!r.enabled && r.change_only);
        assert_eq!((r.heartbeat_ms(), r.retention_days), (300_000, 30));
        assert!(!r.records("mkt_ctx/1"));
        assert!(r.validation_errors(false).is_empty());
    }

    #[test]
    fn schema_lists_and_intervals() {
        let r: RecorderConfig = toml::from_str(
            r#"
            enabled = true
            schemas = ["mkt_ctx/1", "hl_book/1"]
            keep_data = ["hl_book/1"]
            min_interval_secs = { "hl_book/1" = 60 }
            "#,
        )
        .unwrap();
        assert!(r.records("hl_book/1") && !r.records("dlmm_pool/1"));
        assert!(r.keeps_data("hl_book/1") && !r.keeps_data("mkt_ctx/1"));
        assert_eq!(r.min_interval_ms("hl_book/1"), 60_000);
        assert_eq!(r.min_interval_ms("mkt_ctx/1"), 0);
        assert!(r.validation_errors(true).is_empty());
        let all = RecorderConfig {
            enabled: true,
            schemas: vec!["*".into()],
            keep_data: vec!["*".into()],
            ..RecorderConfig::default()
        };
        assert!(all.records("anything/7") && all.keeps_data("anything/7"));
        assert!(all.validation_errors(true).is_empty());
    }

    #[test]
    fn validation() {
        let r = RecorderConfig {
            enabled: true,
            ..RecorderConfig::default()
        };
        let e = r.validation_errors(false);
        assert!(e.iter().any(|m| m.contains("needs [xmarket]")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("schemas is empty")), "{e:?}");
        let r = RecorderConfig {
            enabled: true,
            schemas: vec!["mkt_ctx".into(), "hl_book/1".into()],
            keep_data: vec!["hl_bok/1".into()],
            min_interval_secs: [("mkt_ctx/2".to_string(), 5)].into(),
            ..RecorderConfig::default()
        };
        let e = r.validation_errors(true);
        assert!(
            e.iter()
                .any(|m| m.contains("`mkt_ctx` is not <name>/<version>")),
            "{e:?}"
        );
        assert!(
            e.iter()
                .any(|m| m.contains("keep_data `hl_bok/1` is not in")),
            "{e:?}"
        );
        assert!(
            e.iter()
                .any(|m| m.contains("min_interval_secs `mkt_ctx/2` is not in")),
            "{e:?}"
        );
        assert!(toml::from_str::<RecorderConfig>("enable = true").is_err());
    }
}
