//! Typed tool observations — one envelope serves LLM text, decision-loop
//! state and the TTL cache (`ports::observation::ObservationStore`). Plain
//! data; no IO. Failed reads never become 0: per-field `Field<T>`, per-row
//! `ObsStatus`.

use std::collections::BTreeMap;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Scalar-only map (number | bool | short string) for System One state.
/// The `set_*` helpers omit non-finite / `None` values: missing is never
/// encoded as 0.
pub type Features = BTreeMap<String, Value>;

/// Max keys in `Observation::features`.
pub const MAX_FEATURES: usize = 32;
/// Max chars of a string feature value.
pub const MAX_FEATURE_STR: usize = 64;
/// Max chars of `render_text` line 1 (`application/chat/tool_loop.rs` keeps
/// only line 1 of older tool results, capped at 200 chars).
pub const MAX_LINE1_CHARS: usize = 200;
/// `render_text` omits `data` above this many chars (never cut mid-JSON);
/// `compact_text` replaces it when a local engine's per-result cap is
/// exceeded (`application/chat/tool_loop.rs::fit_tool_result`).
pub const MAX_DATA_CHARS: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsSource {
    Live,
    Cache,
    Stream,
}

/// `Ok` = every promised field valid · `Partial` = primary answer valid,
/// some fields failed · `Absent` = subject legitimately does not exist (flat
/// perp side, no positions) · `Error` = primary answer unavailable (never
/// cached, never fresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsStatus {
    Ok,
    Partial,
    Absent,
    Error,
}

impl ObsStatus {
    pub fn usable(self) -> bool {
        !matches!(self, ObsStatus::Error)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ObsStatus::Ok => "ok",
            ObsStatus::Partial => "partial",
            ObsStatus::Absent => "absent",
            ObsStatus::Error => "error",
        }
    }
}

impl ObsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ObsSource::Live => "live",
            ObsSource::Cache => "cache",
            ObsSource::Stream => "stream",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    QuotaExhausted,
    RateLimited,
    AuthRequired,
    Timeout,
    Transient,
    Decode,
    NotApplicable,
    Fatal,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::QuotaExhausted => "quota_exhausted",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::AuthRequired => "auth_required",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Transient => "transient",
            ErrorClass::Decode => "decode",
            ErrorClass::NotApplicable => "not_applicable",
            ErrorClass::Fatal => "fatal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadError {
    pub field: String,
    pub class: ErrorClass,
    /// Never contains a URL (RPC URLs can embed api keys).
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ReadError {
    pub fn new(field: impl Into<String>, class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            class,
            message: message.into(),
            retry_after_ms: None,
        }
    }
}

/// Per-field read result. Failed reads never become 0 (bot BUG-023).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Field<T> {
    Ok { value: T },
    Absent,
    Error { error: ReadError },
}

impl<T> Field<T> {
    pub fn ok(value: T) -> Self {
        Field::Ok { value }
    }
    pub fn err(error: ReadError) -> Self {
        Field::Error { error }
    }
    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Ok { value } => Some(value),
            _ => None,
        }
    }
    pub fn is_error(&self) -> bool {
        matches!(self, Field::Error { .. })
    }
    pub fn error(&self) -> Option<&ReadError> {
        match self {
            Field::Error { error } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// `"<schema>:<subject>"`, e.g.
    /// `"dlmm_pool/1:5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"`.
    pub key: String,
    /// `"<name>/<version>"`.
    pub schema: String,
    pub tool: String,
    pub observed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    pub ttl_ms: u64,
    pub source: ObsSource,
    pub status: ObsStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
    /// LLM line 1: full identifiers, never shortened.
    pub headline: String,
    /// Compact scalars for the decision model.
    pub features: Features,
    /// Full typed payload = `serde_json::to_value(T)`.
    pub data: Value,
}

/// A typed tool result that can travel as an `Observation`.
pub trait Observed: Serialize + DeserializeOwned {
    const SCHEMA: &'static str;
    /// Cache subject: full identifiers joined by `:` (wallet:pool, pool,
    /// mint, signature).
    fn subject(&self) -> String;
    fn headline(&self) -> String;
    fn features(&self) -> Features;
    fn slot(&self) -> Option<u64> {
        None
    }
    fn status(&self) -> ObsStatus {
        ObsStatus::Ok
    }
    fn errors(&self) -> Vec<ReadError> {
        Vec::new()
    }
}

/// Decision-loop / history view of an observation (no payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObsMeta {
    pub key: String,
    pub status: ObsStatus,
    pub source: ObsSource,
    pub age_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
}

impl Observation {
    pub fn key_for(schema: &str, subject: &str) -> String {
        format!("{schema}:{subject}")
    }

    pub fn of<T: Observed>(tool: &str, v: &T, now_ms: i64, ttl_ms: u64, source: ObsSource) -> Self {
        let features = v.features();
        debug_assert!(
            feature_problems(&features).is_empty(),
            "{} features: {:?}",
            T::SCHEMA,
            feature_problems(&features)
        );
        let mut status = v.status();
        let mut errors = v.errors();
        let data = match serde_json::to_value(v) {
            Ok(d) => d,
            Err(e) => {
                status = ObsStatus::Error;
                errors.push(ReadError::new(
                    "data",
                    ErrorClass::Fatal,
                    format!("serialize: {e}"),
                ));
                Value::Null
            }
        };
        Self {
            key: Self::key_for(T::SCHEMA, &v.subject()),
            schema: T::SCHEMA.to_string(),
            tool: tool.to_string(),
            observed_at_ms: now_ms,
            slot: v.slot(),
            ttl_ms,
            source,
            status,
            errors,
            headline: v.headline(),
            features,
            data,
        }
    }

    /// The typed payload. Errors unless `schema == T::SCHEMA`.
    pub fn typed<T: Observed>(&self) -> anyhow::Result<T> {
        if self.schema != T::SCHEMA {
            anyhow::bail!(
                "observation {} has schema {}, expected {}",
                self.key,
                self.schema,
                T::SCHEMA
            );
        }
        serde_json::from_value(self.data.clone())
            .map_err(|e| anyhow::anyhow!("observation {} data: {e}", self.key))
    }

    /// Saturating: a row from the future has age 0.
    pub fn age_ms(&self, now_ms: i64) -> u64 {
        now_ms.saturating_sub(self.observed_at_ms).max(0) as u64
    }

    /// `usable && age <= min(ttl_ms, max_age_ms)`; a zero bound is never
    /// fresh (`max_age_secs = 0` forces a live read).
    pub fn is_fresh(&self, now_ms: i64, max_age_ms: u64) -> bool {
        let limit = self.ttl_ms.min(max_age_ms);
        limit > 0 && self.status.usable() && self.age_ms(now_ms) <= limit
    }

    /// `Live` → `Cache`; `Stream` stays `Stream`.
    pub fn served_from_cache(mut self) -> Self {
        if self.source == ObsSource::Live {
            self.source = ObsSource::Cache;
        }
        self
    }

    pub fn meta(&self, now_ms: i64) -> ObsMeta {
        ObsMeta {
            key: self.key.clone(),
            status: self.status,
            source: self.source,
            age_s: age_secs(self.age_ms(now_ms)),
            slot: self.slot,
        }
    }

    /// LLM text. Line 1 = `{headline} | {status} {age}s slot={slot} {source}`
    /// (≤ 200 chars; when it would be longer the status suffix moves to its
    /// own line — ids are never cut), then features `k=v …`, one line per
    /// `ReadError`, then `data` as compact JSON (omitted whole when large).
    pub fn render_text(&self, now_ms: i64) -> String {
        let mut suffix = format!("{} {}s", self.status.as_str(), self.age_ms(now_ms) / 1000);
        if let Some(slot) = self.slot {
            suffix.push_str(&format!(" slot={slot}"));
        }
        suffix.push(' ');
        suffix.push_str(self.source.as_str());

        let mut lines = Vec::new();
        let line1 = format!("{} | {suffix}", self.headline);
        if line1.chars().count() <= MAX_LINE1_CHARS {
            lines.push(line1);
        } else {
            lines.push(self.headline.clone());
            lines.push(suffix);
        }
        if !self.features.is_empty() {
            lines.push(
                self.features
                    .iter()
                    .map(|(k, v)| format!("{k}={}", scalar_text(v)))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        for e in &self.errors {
            lines.push(format!(
                "error {}: {} {}",
                e.field,
                e.class.as_str(),
                e.message
            ));
        }
        if !self.data.is_null() {
            lines.push(data_line(self.data.to_string()));
        }
        lines.join("\n")
    }

    /// Compact LLM text for small context windows (`engine = "local"`,
    /// `application/chat/tool_loop.rs::fit_tool_result` — only when the full
    /// text exceeds the engine's per-result cap; a row that fits reaches the
    /// model whole): `text` — this
    /// row's `render_text` plus anything the tool appended (the `lp_decide`
    /// commit note) — with the `data` line replaced by
    /// `data: <n> bytes in observation <key>` (full key). Line 1, features,
    /// errors and appended lines stay as rendered; `text` without this row's
    /// data line (custom text) comes back unchanged.
    pub fn compact_text(&self, text: &str) -> String {
        if self.data.is_null() {
            return text.to_string();
        }
        let data = self.data.to_string();
        let pointer = format!("data: {} bytes in observation {}", data.len(), self.key);
        let rendered = data_line(data);
        let mut swapped = false;
        text.split('\n')
            .map(|line| {
                if !swapped && line == rendered {
                    swapped = true;
                    pointer.as_str()
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `{status, age_s, slot?, source, features, errors?}` — floats rounded
    /// to 6 significant digits, nulls dropped.
    pub fn decision_value(&self, now_ms: i64) -> Value {
        let mut out = Map::new();
        out.insert("status".into(), json!(self.status));
        out.insert("age_s".into(), json!(age_secs(self.age_ms(now_ms))));
        if let Some(slot) = self.slot {
            out.insert("slot".into(), json!(slot));
        }
        out.insert("source".into(), json!(self.source));
        let features: Map<String, Value> = self
            .features
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), round_value(v)))
            .collect();
        out.insert("features".into(), Value::Object(features));
        if !self.errors.is_empty() {
            out.insert(
                "errors".into(),
                serde_json::to_value(&self.errors).unwrap_or(Value::Null),
            );
        }
        Value::Object(out)
    }

    /// `decision_value` + `{"data": data}` — the root reducer / slot paths
    /// address (`/data/pools/*`, `/features/usd`).
    pub fn decision_root(&self, now_ms: i64) -> Value {
        let mut v = self.decision_value(now_ms);
        if let Value::Object(o) = &mut v {
            o.insert("data".into(), self.data.clone());
        }
        v
    }
}

/// Cache key + freshness bound for one typed tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct CachePolicy {
    pub key: String,
    pub ttl_ms: u64,
    pub max_age_ms: u64,
}

impl CachePolicy {
    /// Every typed tool accepts an optional `max_age_secs` arg (0 forces a
    /// live read): `max_age_ms = min(ttl_ms, max_age_secs * 1000)`.
    pub fn new(schema: &str, subject: &str, ttl_ms: u64, args: &Value) -> Self {
        let max_age_ms = match args.get("max_age_secs").and_then(Value::as_u64) {
            Some(s) => ttl_ms.min(s.saturating_mul(1000)),
            None => ttl_ms,
        };
        Self {
            key: Observation::key_for(schema, subject),
            ttl_ms,
            max_age_ms,
        }
    }
}

/// Wall clock in ms since the Unix epoch (0 if the clock is before it).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── feature helpers ─────────────────────────────────────────────

/// Insert a finite number; `None` / NaN / ±inf are omitted.
pub fn set_num(f: &mut Features, key: &str, v: Option<f64>) {
    if let Some(x) = v.filter(|x| x.is_finite()) {
        f.insert(key.to_string(), json!(x));
    }
}

pub fn set_int(f: &mut Features, key: &str, v: Option<i64>) {
    if let Some(x) = v {
        f.insert(key.to_string(), json!(x));
    }
}

pub fn set_bool(f: &mut Features, key: &str, v: Option<bool>) {
    if let Some(x) = v {
        f.insert(key.to_string(), json!(x));
    }
}

/// Short enum-like strings only (≤ `MAX_FEATURE_STR` chars); ids belong in
/// `data`, never in features.
pub fn set_str(f: &mut Features, key: &str, v: Option<&str>) {
    if let Some(x) = v {
        f.insert(key.to_string(), json!(x));
    }
}

/// Violations of the features contract: ≤ `MAX_FEATURES` keys, every value
/// a finite number, a bool or a string ≤ `MAX_FEATURE_STR` chars.
pub fn feature_problems(f: &Features) -> Vec<String> {
    let mut out = Vec::new();
    if f.len() > MAX_FEATURES {
        out.push(format!("{} keys > {MAX_FEATURES}", f.len()));
    }
    for (k, v) in f {
        match v {
            Value::Number(_) | Value::Bool(_) => {}
            Value::String(s) if s.chars().count() <= MAX_FEATURE_STR => {}
            Value::String(s) => out.push(format!(
                "{k}: string of {} chars > {MAX_FEATURE_STR}",
                s.chars().count()
            )),
            Value::Null => out.push(format!("{k}: null (omit missing values)")),
            _ => out.push(format!("{k}: nested value")),
        }
    }
    out
}

/// Test helper for every `Observed` impl: panics on a features violation.
#[cfg(test)]
pub(crate) fn assert_features_ok(f: &Features) {
    let p = feature_problems(f);
    assert!(p.is_empty(), "features contract violated: {p:?}");
}

/// `render_text`'s `data` line: the compact JSON, or a size note above
/// `MAX_DATA_CHARS`.
fn data_line(data: String) -> String {
    if data.chars().count() <= MAX_DATA_CHARS {
        data
    } else {
        format!("data: {} bytes omitted", data.len())
    }
}

fn age_secs(age_ms: u64) -> f64 {
    (age_ms as f64 / 100.0).round() / 10.0
}

fn round_sig(x: f64, digits: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mag = x.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits - 1 - mag);
    if !scale.is_finite() || scale == 0.0 {
        return x;
    }
    (x * scale).round() / scale
}

fn round_value(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => n
            .as_f64()
            .map(|x| json!(round_sig(x, 6)))
            .unwrap_or_else(|| v.clone()),
        other => other.clone(),
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => round_value(other).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Probe {
        wallet: String,
        pool: String,
        usd: f64,
        slot: u64,
    }

    impl Observed for Probe {
        const SCHEMA: &'static str = "probe/1";
        fn subject(&self) -> String {
            format!("{}:{}", self.wallet, self.pool)
        }
        fn headline(&self) -> String {
            format!(
                "probe wallet={} pool={} usd={}",
                self.wallet, self.pool, self.usd
            )
        }
        fn features(&self) -> Features {
            let mut f = Features::new();
            set_num(&mut f, "usd", Some(self.usd));
            set_num(&mut f, "nan", Some(f64::NAN));
            set_num(&mut f, "none", None);
            set_bool(&mut f, "in_range", Some(true));
            set_str(&mut f, "regime", Some("neutral"));
            f
        }
        fn slot(&self) -> Option<u64> {
            Some(self.slot)
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Other {
        x: u8,
    }
    impl Observed for Other {
        const SCHEMA: &'static str = "other/1";
        fn subject(&self) -> String {
            "x".into()
        }
        fn headline(&self) -> String {
            "x".into()
        }
        fn features(&self) -> Features {
            Features::new()
        }
    }

    fn probe() -> Probe {
        Probe {
            wallet: WALLET.into(),
            pool: POOL.into(),
            usd: 114.318_612_345,
            slot: 450_040_267,
        }
    }

    #[test]
    fn of_builds_key_and_omits_missing_features() {
        let o = Observation::of("probe", &probe(), 1_000, 5_000, ObsSource::Live);
        assert_eq!(o.key, format!("probe/1:{WALLET}:{POOL}"));
        assert_eq!(o.slot, Some(450_040_267));
        assert!(!o.features.contains_key("nan"));
        assert!(!o.features.contains_key("none"));
        assert_features_ok(&o.features);
        assert_eq!(o.typed::<Probe>().unwrap(), probe());
    }

    #[test]
    fn typed_checks_schema() {
        let o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        let e = o.typed::<Other>().unwrap_err().to_string();
        assert!(e.contains("expected other/1"), "{e}");
    }

    #[test]
    fn freshness_and_age() {
        let o = Observation::of("probe", &probe(), 10_000, 5_000, ObsSource::Live);
        assert_eq!(o.age_ms(12_500), 2_500);
        assert_eq!(o.age_ms(9_000), 0, "saturating");
        assert!(o.is_fresh(12_500, 60_000), "within ttl");
        assert!(!o.is_fresh(15_001, 60_000), "past ttl");
        assert!(!o.is_fresh(12_500, 2_000), "past caller max age");
        assert!(!o.is_fresh(10_000, 0), "zero bound forces live");
        let mut e = o.clone();
        e.status = ObsStatus::Error;
        assert!(!e.is_fresh(10_001, 60_000), "error rows are never fresh");
        assert_eq!(o.meta(12_500).age_s, 2.5);
    }

    #[test]
    fn served_from_cache_flips_live_only() {
        let o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        assert_eq!(o.served_from_cache().source, ObsSource::Cache);
        let s = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Stream);
        assert_eq!(s.served_from_cache().source, ObsSource::Stream);
    }

    #[test]
    fn render_line1_fits_with_full_ids() {
        let o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        let text = o.render_text(3_000);
        let line1 = text.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert!(line1.contains(WALLET) && line1.contains(POOL), "{line1}");
        assert!(line1.ends_with("| ok 3s slot=450040267 live"), "{line1}");
        assert!(text.contains("regime=neutral"), "{text}");
    }

    #[test]
    fn render_moves_suffix_rather_than_cutting_ids() {
        let mut o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        // Two 88-char signatures: headline alone is 185 chars.
        let sig = "5".repeat(88);
        o.headline = format!("tx pair {sig} {sig}");
        let text = o.render_text(0);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), o.headline);
        assert!(lines.next().unwrap().starts_with("ok 0s"));
    }

    #[test]
    fn render_omits_large_data_whole() {
        let mut o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        o.data = json!({"blob": "x".repeat(MAX_DATA_CHARS + 1)});
        let text = o.render_text(0);
        assert!(text.lines().last().unwrap().ends_with("bytes omitted"));
    }

    #[test]
    fn compact_keeps_line1_features_errors_and_full_ids_drops_data() {
        let mut o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        o.errors
            .push(ReadError::new("usd", ErrorClass::Timeout, "rpc slow"));
        o.data = json!({"pool": POOL, "bins": "b".repeat(4_000)});
        let text = o.render_text(3_000);
        let compact = o.compact_text(&text);
        let (full_lines, lines): (Vec<&str>, Vec<&str>) =
            (text.lines().collect(), compact.lines().collect());
        assert_eq!(lines.len(), full_lines.len());
        assert_eq!(lines[0], full_lines[0], "line 1 intact");
        assert!(
            lines[0].contains(WALLET) && lines[0].contains(POOL),
            "{}",
            lines[0]
        );
        assert!(compact.contains("regime=neutral"), "{compact}");
        assert!(compact.contains("error usd: timeout rpc slow"), "{compact}");
        assert!(!compact.contains("bbbb"), "data dropped: {compact}");
        let data_bytes = o.data.to_string().len();
        assert_eq!(
            *lines.last().unwrap(),
            format!("data: {data_bytes} bytes in observation probe/1:{WALLET}:{POOL}")
        );
        assert!(compact.len() < 400, "{} chars", compact.len());
    }

    #[test]
    fn compact_keeps_what_the_tool_appended() {
        let o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        let note = format!("lp_state committed: lp_state/1:{WALLET}:{POOL}");
        let text = format!("{}\n{note}", o.render_text(0));
        let compact = o.compact_text(&text);
        let lines: Vec<&str> = compact.lines().collect();
        assert_eq!(lines[lines.len() - 1], note);
        assert!(lines[lines.len() - 2].starts_with("data: "), "{compact}");
        assert!(lines[lines.len() - 2].ends_with(&format!(" bytes in observation {}", o.key)));
    }

    #[test]
    fn compact_replaces_the_omitted_note_and_passes_other_text_through() {
        let mut o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        o.data = json!({"blob": "x".repeat(MAX_DATA_CHARS + 1)});
        let compact = o.compact_text(&o.render_text(0));
        assert!(
            compact
                .lines()
                .last()
                .unwrap()
                .ends_with(&format!(" bytes in observation {}", o.key)),
            "{compact}"
        );
        assert_eq!(o.compact_text("custom text\nline 2"), "custom text\nline 2");
        o.data = Value::Null;
        let text = o.render_text(0);
        assert_eq!(o.compact_text(&text), text);
    }

    #[test]
    fn decision_value_rounds_and_has_no_data() {
        let o = Observation::of("probe", &probe(), 0, 5_000, ObsSource::Live);
        let v = o.decision_value(1_000);
        assert_eq!(v["features"]["usd"], json!(114.319));
        assert_eq!(v["status"], json!("ok"));
        assert_eq!(v["source"], json!("live"));
        assert!(v.get("data").is_none());
        assert_eq!(o.decision_root(1_000)["data"]["pool"], json!(POOL));
    }

    #[test]
    fn cache_policy_caps_max_age_by_ttl() {
        let p = CachePolicy::new("probe/1", "s", 10_000, &json!({}));
        assert_eq!((p.key.as_str(), p.max_age_ms), ("probe/1:s", 10_000));
        let p = CachePolicy::new("probe/1", "s", 10_000, &json!({"max_age_secs": 3}));
        assert_eq!(p.max_age_ms, 3_000);
        let p = CachePolicy::new("probe/1", "s", 10_000, &json!({"max_age_secs": 60}));
        assert_eq!(p.max_age_ms, 10_000);
        let p = CachePolicy::new("probe/1", "s", 10_000, &json!({"max_age_secs": 0}));
        assert_eq!(p.max_age_ms, 0);
    }

    #[test]
    fn features_validator_rejects_bad_values() {
        let mut f = Features::new();
        f.insert("ok".into(), json!(1.5));
        f.insert("flag".into(), json!(true));
        f.insert("id".into(), json!(POOL));
        assert!(feature_problems(&f).is_empty());
        f.insert("long".into(), json!("y".repeat(MAX_FEATURE_STR + 1)));
        f.insert("nested".into(), json!({"a": 1}));
        f.insert("null".into(), Value::Null);
        let p = feature_problems(&f).join("\n");
        assert!(
            p.contains("long") && p.contains("nested") && p.contains("null"),
            "{p}"
        );
        let many: Features = (0..=MAX_FEATURES)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        assert!(feature_problems(&many)[0].contains("keys"));
    }

    #[test]
    fn field_serialises_tagged() {
        let f: Field<f64> = Field::err(ReadError::new("usd", ErrorClass::Timeout, "slow"));
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            json!({"state": "error", "error": {"field": "usd", "class": "timeout", "message": "slow"}})
        );
        assert_eq!(
            serde_json::to_value(Field::ok(1.0)).unwrap(),
            json!({"state": "ok", "value": 1.0})
        );
        assert!(Field::<u8>::Absent.value().is_none());
    }
}
