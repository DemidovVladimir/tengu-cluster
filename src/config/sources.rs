//! `[sources]` — the source registry (O2): one row per approved external
//! source, as data. Fetchers (`tengu sources fetch`) read a row; the as-of
//! view reads its revision and listing age ([`SourceEntry::policy`] →
//! `domain::source::SourcePolicy`). Rows ship `enabled = false`: enabling
//! one needs the operator's reviewed terms and both retention periods.
//! Cross-domain (the SOE sandbox now, xmarket `info-*` later); store
//! `<TENGU_HOME>/state/<state>/sources.db`, never `market.db`.
//! `deny_unknown_fields` at every level. Page: `docs/tutorial/source-evidence.html`.
//!
//! ```toml
//! [sources]
//! state = "soe"
//!
//! [sources.registry.sec_edgar]
//! kind = "sec_edgar"
//! class = "company_primary"
//! trust = "primary"
//! revision = "immutable"
//! enabled = false
//! hosts = ["www.sec.gov", "data.sec.gov"]
//! auth = "user_agent_env:SEC_USER_AGENT"
//! rate_limit = "sec"
//! store_raw = true
//! jurisdiction = "US"
//! language = "en"
//! ```
//!
//! | Field | When | Rule (a violation fails `Config::load`) |
//! |---|---|---|
//! | `state` | always | one directory name under `<TENGU_HOME>/state/` (the `[xmarket] state` rule); the dir outside every `fs_roots` and agent `workspace` |
//! | row id | always | `[a-z0-9_]+` — the records' `source_id` |
//! | `kind` | always | `sec_edgar` · `ted_search` |
//! | `class` · `trust` | always | the PRD §5.1 classes; the class must allow the trust (`independent_reporting` never `primary`, `social_inference` only `trigger_only`) |
//! | `revision` | always | `immutable` · `in_place` — the knowable clock of the as-of view (`domain/source/rules.rs`) |
//! | `enabled` | always | `false` = listed, never fetched |
//! | `hosts` | always | at least one bare host; each inside `[egress] allow_hosts` when that is set, none in `deny_hosts` |
//! | `auth` | always | `none` · `user_agent_env:<VAR>` · `api_key_env:<VAR>` (`VAR` = `[A-Z_][A-Z0-9_]*`); `sec_edgar` needs `user_agent_env` (SEC fair access) |
//! | `rate_limit` | always | names a `[rate_limits.<name>]` |
//! | `store_raw` | always | keep the raw bodies (snapshots) |
//! | `jurisdiction` · `language` | always | non-empty tokens; `law_regulator`: an ISO 3166 code or `EU` (PRD §5.1) |
//! | `license` · `terms_url` · `terms_sha256` · `terms_reviewed_at` | `enabled` | required — the reuse terms, an https URL, the sha256 of the page the operator reviewed (64 lowercase hex, whole), `YYYY-MM-DD`; checked whenever present |
//! | `raw_retention_days` · `record_retention_days` | `enabled` | required; `0` = forever |
//! | `listing_max_age_days` | `registry_marketplace` | required, ≥ 1 (PRD §5.1 listing freshness); no other class takes it |
//! | `forms` | `sec_edgar` | each one of `domain::sec::SEC_FORMS`, no repeat (absent = all of them); no other kind takes it |
//! | `query` | `ted_search` | required, holds `{from}` and `{to}` (each replaced by the publication day read, `YYYYMMDD` — TED Search query syntax, `domain/source/ted.rs`); no other kind takes it |
//! | `entities` | `sec_edgar` | each `sec:cik:<10 digits>`; no other kind takes it |
//!
//! A fetcher takes a row through [`SourceEntry::fetch_stamp`]: a disabled
//! row, or one without `license` + `terms_sha256`, is refused before any
//! request; the stamp is what each of its records carries. The operator can
//! also switch a row off at runtime (`tengu sources disable`, a row in
//! `sources.db`) — `outbound/sources::fetch_gate` checks both.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::egress::EgressConfig;
use super::hardening;
use super::paths::resolve_tengu_home;
use super::rate_limits::RateLimitConfig;
use super::Config;
use crate::domain::calendar::parse_date;
use crate::domain::evidence::valid_sha256;
use crate::domain::scope::host_matches;
use crate::domain::sec::SEC_FORMS;
use crate::domain::source::record::valid_source_id;
use crate::domain::source::{
    valid_jurisdiction, Revision, SourceClass, SourcePolicy, SourceStamp, Trust,
};

/// The append-only source store (`adapters/outbound/sources/store.rs`).
pub(crate) const SOURCES_DB: &str = "sources.db";

/// `<state_dir>/sources.db`.
pub(crate) fn sources_db(state_dir: &Path) -> PathBuf {
    state_dir.join(SOURCES_DB)
}

/// `[sources]` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfig {
    /// Directory name under `<TENGU_HOME>/state/` holding `sources.db`.
    pub state: String,
    /// `[sources.registry.<id>]` by source id.
    #[serde(default)]
    pub registry: BTreeMap<String, SourceEntry>,
}

/// What a row fetches with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// SEC EDGAR submissions + filing index (`data.sec.gov`, `www.sec.gov`).
    SecEdgar,
    /// EU TED Search API (`api.ted.europa.eu`, anonymous POST).
    TedSearch,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::SecEdgar => "sec_edgar",
            SourceKind::TedSearch => "ted_search",
        }
    }
}

/// `auth` parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceAuth {
    None,
    /// The `User-Agent` comes from this env var (SEC fair access).
    UserAgentEnv(String),
    /// An API key comes from this env var.
    ApiKeyEnv(String),
}

/// One `[sources.registry.<id>]` row (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    pub kind: SourceKind,
    pub class: SourceClass,
    pub trust: Trust,
    pub revision: Revision,
    pub enabled: bool,
    pub hosts: Vec<String>,
    pub auth: String,
    pub rate_limit: String,
    pub store_raw: bool,
    pub jurisdiction: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_reviewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_retention_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_retention_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing_max_age_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
}

impl SourcesConfig {
    /// `<tengu_home>/state/<state>`.
    pub fn state_dir(&self, tengu_home: &Path) -> PathBuf {
        tengu_home.join("state").join(&self.state)
    }

    /// The as-of view's policy of every row, enabled or not (records of a
    /// row switched off stay readable).
    pub fn policies(&self) -> BTreeMap<String, SourcePolicy> {
        self.registry
            .iter()
            .map(|(id, e)| (id.clone(), e.policy()))
            .collect()
    }

    /// Every problem of the section on its own (rows against `egress` and
    /// `rate_limits`), each `sources…: …`.
    pub fn validation_errors(
        &self,
        egress: &EgressConfig,
        rate_limits: &HashMap<String, RateLimitConfig>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let s = self.state.as_str();
        let ok = !s.is_empty()
            && s != "."
            && s != ".."
            && s != "flows"
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            out.push(format!(
                "sources.state `{s}` must be one directory name ([A-Za-z0-9._-], not `.`, `..` or `flows`)"
            ));
        }
        for (id, e) in &self.registry {
            out.extend(
                e.row_errors(id, egress, rate_limits)
                    .into_iter()
                    .map(|p| format!("sources.registry.{id}.{p}")),
            );
        }
        out
    }
}

/// `[A-Z_][A-Z0-9_]*`.
fn env_name(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A bare host: lowercase labels of `[a-z0-9-]` joined by dots, at least two.
fn bare_host(h: &str) -> bool {
    let labels: Vec<&str> = h.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl SourceEntry {
    pub fn auth(&self) -> Result<SourceAuth, String> {
        let a = self.auth.as_str();
        if a == "none" {
            return Ok(SourceAuth::None);
        }
        let parsed = match a.split_once(':') {
            Some(("user_agent_env", v)) if env_name(v) => SourceAuth::UserAgentEnv(v.into()),
            Some(("api_key_env", v)) if env_name(v) => SourceAuth::ApiKeyEnv(v.into()),
            _ => {
                return Err(format!(
                    "`{a}` is not `none`, `user_agent_env:<VAR>` or `api_key_env:<VAR>` (VAR = [A-Z_][A-Z0-9_]*)"
                ))
            }
        };
        Ok(parsed)
    }

    /// The forms a `sec_edgar` row keeps: its `forms`, else every one of
    /// `SEC_FORMS`; nothing for other kinds.
    pub fn forms(&self) -> Vec<String> {
        match (self.kind, &self.forms) {
            (SourceKind::SecEdgar, Some(f)) => f.clone(),
            (SourceKind::SecEdgar, None) => SEC_FORMS.iter().map(|f| f.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    pub fn policy(&self) -> SourcePolicy {
        SourcePolicy {
            revision: self.revision,
            listing_max_age_ms: self.listing_max_age_days.map(|d| i64::from(d) * 86_400_000),
        }
    }

    /// What every record of row `id` carries ([`SourceStamp`]) — refused
    /// for a disabled row (listed, never fetched) and for one without its
    /// reviewed terms (`license` + `terms_sha256`): no fetch without them.
    pub fn fetch_stamp(&self, id: &str) -> Result<SourceStamp, String> {
        if !self.enabled {
            return Err(format!(
                "source `{id}` is disabled (enabled = false): listed, never fetched"
            ));
        }
        let (Some(license), Some(terms)) = (&self.license, &self.terms_sha256) else {
            return Err(format!(
                "source `{id}` has no reviewed terms (license + terms_sha256): no fetch without them"
            ));
        };
        Ok(SourceStamp {
            source_id: id.to_string(),
            source_class: self.class,
            trust: self.trust,
            jurisdiction: self.jurisdiction.clone(),
            language: self.language.clone(),
            license_or_terms: license.clone(),
            terms_sha256: terms.clone(),
        })
    }

    /// The row's problems, each `<field>: …` (module table).
    fn row_errors(
        &self,
        id: &str,
        egress: &EgressConfig,
        rate_limits: &HashMap<String, RateLimitConfig>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        if !valid_source_id(id) {
            out.push("id: must be [a-z0-9_]+ (it is the records' source_id)".to_string());
        }
        if !self.class.allows(self.trust) {
            out.push(format!(
                "trust: a `{}` source cannot be `{}` (PRD §5.1)",
                self.class.as_str(),
                self.trust.as_str()
            ));
        }
        if self.hosts.is_empty() {
            out.push("hosts: at least one host".into());
        }
        for h in &self.hosts {
            if !bare_host(h) {
                out.push(format!(
                    "hosts: `{h}` is not a bare lowercase host (no scheme, port or path)"
                ));
            } else if !egress.allow_hosts.is_empty()
                && !egress.allow_hosts.iter().any(|p| host_matches(p, h))
            {
                out.push(format!("hosts: `{h}` is outside [egress] allow_hosts"));
            } else if egress.deny_hosts.iter().any(|p| host_matches(p, h)) {
                out.push(format!("hosts: `{h}` is in [egress] deny_hosts"));
            }
        }
        match self.auth() {
            Err(e) => out.push(format!("auth: {e}")),
            Ok(SourceAuth::UserAgentEnv(_)) => {}
            Ok(_) if self.kind == SourceKind::SecEdgar => out.push(
                "auth: sec_edgar needs `user_agent_env:<VAR>` (SEC fair access: a declared User-Agent)".into(),
            ),
            Ok(_) => {}
        }
        if self.rate_limit.is_empty() || !rate_limits.contains_key(&self.rate_limit) {
            out.push(format!(
                "rate_limit: `{}` names no [rate_limits.<name>]",
                self.rate_limit
            ));
        }
        if !token(&self.jurisdiction) {
            out.push("jurisdiction: empty or has whitespace".into());
        } else if self.class == SourceClass::LawRegulator && !valid_jurisdiction(&self.jurisdiction)
        {
            out.push(format!(
                "jurisdiction: `{}` — a law_regulator source needs an ISO 3166 code or EU (PRD §5.1)",
                self.jurisdiction
            ));
        }
        if !token(&self.language) {
            out.push("language: empty or has whitespace".into());
        }

        // Terms + retention: checked when present, required when enabled.
        if self.license.as_deref().is_some_and(|l| l.trim().is_empty()) {
            out.push("license: empty (omit it instead)".into());
        }
        if let Some(u) = self
            .terms_url
            .as_deref()
            .filter(|u| !(token(u) && u.starts_with("https://")))
        {
            out.push(format!("terms_url: `{u}` is not an https URL"));
        }
        if let Some(h) = self.terms_sha256.as_deref().filter(|h| !valid_sha256(h)) {
            out.push(format!("terms_sha256: `{h}` is not 64 lowercase hex"));
        }
        if let Some(d) = self
            .terms_reviewed_at
            .as_deref()
            .filter(|d| parse_date(d).is_none())
        {
            out.push(format!("terms_reviewed_at: `{d}` is not YYYY-MM-DD"));
        }
        if self.enabled {
            let missing: Vec<&str> = [
                ("license", self.license.is_none()),
                ("terms_url", self.terms_url.is_none()),
                ("terms_sha256", self.terms_sha256.is_none()),
                ("terms_reviewed_at", self.terms_reviewed_at.is_none()),
                ("raw_retention_days", self.raw_retention_days.is_none()),
                (
                    "record_retention_days",
                    self.record_retention_days.is_none(),
                ),
            ]
            .into_iter()
            .filter(|(_, gone)| *gone)
            .map(|(k, _)| k)
            .collect();
            if !missing.is_empty() {
                out.push(format!(
                    "enabled: needs {} — the operator reviews the terms and sets retention before any fetch",
                    missing.join(", ")
                ));
            }
        }

        // Class rule: registry listings carry their freshness bound.
        match (self.class, self.listing_max_age_days) {
            (SourceClass::RegistryMarketplace, None | Some(0)) => out.push(
                "listing_max_age_days: a registry_marketplace source needs it (≥ 1, PRD §5.1 listing freshness)".into(),
            ),
            (SourceClass::RegistryMarketplace, Some(_)) | (_, None) => {}
            (_, Some(_)) => out.push("listing_max_age_days: only for a registry_marketplace source".into()),
        }

        // Kind shapes.
        let sec = self.kind == SourceKind::SecEdgar;
        match &self.forms {
            Some(_) if !sec => out.push(format!("forms: not for a `{}` row", self.kind.as_str())),
            Some(f) if f.is_empty() => {
                out.push("forms: empty (omit it for every kept form)".into())
            }
            Some(f) => {
                let mut seen = BTreeSet::new();
                for form in f {
                    if !SEC_FORMS.contains(&form.as_str()) {
                        out.push(format!(
                            "forms: `{form}` is not a kept form ({})",
                            SEC_FORMS.join(", ")
                        ));
                    } else if !seen.insert(form) {
                        out.push(format!("forms: `{form}` listed twice"));
                    }
                }
            }
            None => {}
        }
        match (self.kind, self.query.as_deref()) {
            (SourceKind::TedSearch, None) => {
                out.push("query: a ted_search row needs one, with {from} and {to}".into())
            }
            (SourceKind::TedSearch, Some(q)) if !(q.contains("{from}") && q.contains("{to}")) => {
                out.push("query: must hold {from} and {to} (the publication-date window)".into())
            }
            (SourceKind::SecEdgar, Some(_)) => out.push("query: not for a `sec_edgar` row".into()),
            _ => {}
        }
        for e in &self.entities {
            let cik = e.strip_prefix("sec:cik:");
            if !sec {
                out.push(format!("entities: not for a `{}` row", self.kind.as_str()));
                break;
            }
            if !cik.is_some_and(|c| c.len() == 10 && c.bytes().all(|b| b.is_ascii_digit())) {
                out.push(format!("entities: `{e}` is not sec:cik:<10 digits>"));
            }
        }
        out
    }
}

/// Every `[sources]` load rule (module table), with the state dir under
/// `<TENGU_HOME>`.
pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    rules_at(cfg, &resolve_tengu_home())
}

fn rules_at(cfg: &Config, tengu_home: &Path) -> Vec<String> {
    let Some(sources) = &cfg.sources else {
        return Vec::new();
    };
    let mut errors = sources.validation_errors(&cfg.egress, &cfg.rate_limits);
    // A hardened sandbox keeps all of `<TENGU_HOME>/state` out of reach
    // (`config/hardening.rs`); an invalid `state` is reported above.
    if !hardening::requires_hardened_claude_code(cfg)
        && errors.iter().all(|e| !e.starts_with("sources.state"))
    {
        errors.extend(hardening::dir_reach_errors(
            cfg,
            "the [sources] state dir",
            &sources.state_dir(tengu_home),
            "sources: no tool may reach sources.db",
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::source::IssueCode;

    const HASH: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    const BASE: &str = r#"
        [agents.main]
        engine = "openrouter"
        model = "m"

        [egress]
        network = "open"
        allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]

        [rate_limits.sec]
        per_minute = 300

        [rate_limits.ted]
        per_minute = 60

        [sources]
        state = "soe"
    "#;

    const SEC_ROW: &str = r#"
        [sources.registry.sec_edgar]
        kind = "sec_edgar"
        class = "company_primary"
        trust = "primary"
        revision = "immutable"
        enabled = false
        hosts = ["www.sec.gov", "data.sec.gov"]
        auth = "user_agent_env:SEC_USER_AGENT"
        rate_limit = "sec"
        store_raw = true
        jurisdiction = "US"
        language = "en"
    "#;

    const TED_ROW: &str = r#"
        [sources.registry.ted_search]
        kind = "ted_search"
        class = "law_regulator"
        trust = "primary"
        revision = "immutable"
        enabled = false
        hosts = ["api.ted.europa.eu"]
        auth = "none"
        rate_limit = "ted"
        store_raw = true
        jurisdiction = "EU"
        language = "en"
        query = "publication-date >= {from} AND publication-date <= {to}"
    "#;

    fn terms() -> String {
        format!(
            "license = \"example terms\"\nterms_url = \"https://example.org/terms\"\n\
             terms_sha256 = \"{HASH}\"\nterms_reviewed_at = \"2026-10-08\"\n\
             raw_retention_days = 90\nrecord_retention_days = 0\n"
        )
    }

    fn parse(text: &str) -> Result<Config, String> {
        toml::from_str::<Config>(text).map_err(|e| e.to_string())
    }

    fn errors(text: &str) -> Vec<String> {
        let cfg = parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let home = tempfile::TempDir::new().unwrap();
        rules_at(&cfg, home.path())
    }

    /// `BASE` + `rows`, with `edit` applied to the first row's text.
    fn with(rows: &str) -> String {
        format!("{BASE}\n{rows}")
    }

    #[test]
    fn disabled_source_may_omit_terms() {
        assert_eq!(
            errors(&with(&format!("{SEC_ROW}\n{TED_ROW}"))),
            Vec::<String>::new()
        );
        // Terms that are present are checked even on a disabled row.
        let bad = format!("{SEC_ROW}terms_sha256 = \"{}\"\nterms_reviewed_at = \"08.10.2026\"\nterms_url = \"http://x.example/t\"\n", &HASH[..12]);
        let e = errors(&with(&bad));
        for field in ["terms_sha256", "terms_reviewed_at", "terms_url"] {
            assert!(
                e.iter()
                    .any(|m| m.starts_with(&format!("sources.registry.sec_edgar.{field}"))),
                "{field}: {e:?}"
            );
        }
    }

    #[test]
    fn enabled_source_without_license_terms_or_retention_fails_load() {
        let enabled = SEC_ROW.replace("enabled = false", "enabled = true");
        let e = errors(&with(&enabled));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].starts_with("sources.registry.sec_edgar.enabled: needs license, terms_url, terms_sha256, terms_reviewed_at, raw_retention_days, record_retention_days"), "{e:?}");
        // With every field: loads.
        assert_eq!(
            errors(&with(&format!("{enabled}{}", terms()))),
            Vec::<String>::new()
        );
        // Each one missing is named.
        for drop in [
            "terms_sha256",
            "raw_retention_days",
            "record_retention_days",
            "license",
        ] {
            let t: String = terms()
                .lines()
                .filter(|l| !l.starts_with(drop))
                .map(|l| format!("{l}\n"))
                .collect();
            let e = errors(&with(&format!("{enabled}{t}")));
            assert!(
                e.len() == 1 && e[0].contains(&format!("needs {drop}")),
                "{drop}: {e:?}"
            );
        }
        // Through Config::load too.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, with(&enabled)).unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(
            err.contains("sources.registry.sec_edgar.enabled: needs"),
            "{err}"
        );
    }

    #[test]
    fn hosts_must_sit_inside_egress_allow_hosts() {
        let row = SEC_ROW.replace(
            r#"["www.sec.gov", "data.sec.gov"]"#,
            r#"["www.sec.gov", "efts.sec.gov"]"#,
        );
        let e = errors(&with(&row));
        assert_eq!(
            e,
            vec![
                "sources.registry.sec_edgar.hosts: `efts.sec.gov` is outside [egress] allow_hosts"
                    .to_string()
            ]
        );
        // A wildcard ceiling admits it; an empty ceiling checks nothing.
        let wide = with(&row).replace(
            r#"allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]"#,
            r#"allow_hosts = ["*.sec.gov"]"#,
        );
        assert_eq!(errors(&wide), Vec::<String>::new());
        let open = with(&row).replace(
            r#"allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]"#,
            "",
        );
        assert_eq!(errors(&open), Vec::<String>::new());
        let denied = open.replace(
            "network = \"open\"",
            "network = \"open\"\ndeny_hosts = [\"efts.sec.gov\"]",
        );
        assert!(errors(&denied)[0].ends_with("is in [egress] deny_hosts"));
        for bad in [
            "https://www.sec.gov",
            "www.sec.gov:443",
            "WWW.SEC.GOV",
            "localhost",
            "www.sec.gov/x",
        ] {
            let row = SEC_ROW.replace(r#""data.sec.gov"]"#, &format!(r#""{bad}"]"#));
            assert!(
                errors(&with(&row))
                    .iter()
                    .any(|m| m.contains("not a bare lowercase host")),
                "{bad}"
            );
        }
        let none = SEC_ROW.replace(r#"hosts = ["www.sec.gov", "data.sec.gov"]"#, "hosts = []");
        assert!(errors(&with(&none))[0].ends_with("hosts: at least one host"));
    }

    #[test]
    fn rate_limit_must_name_a_budget() {
        let row = SEC_ROW.replace(r#"rate_limit = "sec""#, r#"rate_limit = "edgar""#);
        assert_eq!(
            errors(&with(&row)),
            vec![
                "sources.registry.sec_edgar.rate_limit: `edgar` names no [rate_limits.<name>]"
                    .to_string()
            ]
        );
    }

    #[test]
    fn unknown_key_is_refused() {
        for (text, key) in [
            (with(&format!("{SEC_ROW}fetch_every = 60\n")), "fetch_every"),
            (format!("{BASE}stat = \"x\"\n"), "stat"),
            (
                with(&SEC_ROW.replace("kind = \"sec_edgar\"", "kind = \"edgar\"")),
                "edgar",
            ),
            (
                with(&SEC_ROW.replace("class = \"company_primary\"", "class = \"news\"")),
                "news",
            ),
            (
                with(&SEC_ROW.replace("revision = \"immutable\"", "revision = \"append\"")),
                "append",
            ),
            (
                with(&SEC_ROW.replace("store_raw = true\n", "")),
                "store_raw",
            ),
        ] {
            let e = parse(&text).unwrap_err();
            assert!(e.contains(key), "{key}: {e}");
        }
        // The whole top-level section is closed too.
        assert!(parse(&format!("{BASE}\n[sourcez]\nstate = \"x\"\n"))
            .unwrap_err()
            .contains("sourcez"));
    }

    #[test]
    fn fetch_stamp_refuses_a_disabled_or_unreviewed_row() {
        let cfg = parse(&with(SEC_ROW)).unwrap();
        let row = &cfg.sources.as_ref().unwrap().registry["sec_edgar"];
        let e = row.fetch_stamp("sec_edgar").unwrap_err();
        assert!(e.contains("disabled"), "{e}");
        let mut on = row.clone();
        on.enabled = true;
        let e = on.fetch_stamp("sec_edgar").unwrap_err();
        assert!(e.contains("no reviewed terms"), "{e}");
        let enabled = SEC_ROW.replace("enabled = false", "enabled = true");
        let cfg = parse(&with(&format!("{enabled}{}", terms()))).unwrap();
        let s = cfg.sources.as_ref().unwrap().registry["sec_edgar"]
            .fetch_stamp("sec_edgar")
            .unwrap();
        assert_eq!(
            s,
            SourceStamp {
                source_id: "sec_edgar".into(),
                source_class: SourceClass::CompanyPrimary,
                trust: Trust::Primary,
                jurisdiction: "US".into(),
                language: "en".into(),
                license_or_terms: "example terms".into(),
                terms_sha256: HASH.into(),
            }
        );
    }

    /// Critic U6, config side: PRD §5.1 class rules hold for every row.
    #[test]
    fn class_rules_hold_for_rows() {
        let social = SEC_ROW.replace(
            "class = \"company_primary\"",
            "class = \"social_inference\"",
        );
        assert!(errors(&with(&social))[0]
            .contains("trust: a `social_inference` source cannot be `primary`"));
        let news = SEC_ROW.replace(
            "class = \"company_primary\"",
            "class = \"independent_reporting\"",
        );
        assert!(errors(&with(&news))[0].contains("cannot be `primary`"));
        let law = TED_ROW.replace("jurisdiction = \"EU\"", "jurisdiction = \"europe\"");
        assert!(errors(&with(&law))[0].contains("needs an ISO 3166 code or EU"));
        let registry = TED_ROW.replace(
            "class = \"law_regulator\"",
            "class = \"registry_marketplace\"",
        );
        assert!(errors(&with(&registry))[0]
            .contains("listing_max_age_days: a registry_marketplace source needs it"));
        let fresh = format!("{registry}listing_max_age_days = 2\n");
        assert_eq!(errors(&with(&fresh)), Vec::<String>::new());
        let cfg = parse(&with(&fresh)).unwrap();
        let policy = cfg.sources.unwrap().policies()["ted_search"];
        assert_eq!(policy.listing_max_age_ms, Some(2 * 86_400_000));
        assert_eq!(policy.revision, Revision::Immutable);
        let stray = format!("{SEC_ROW}listing_max_age_days = 2\n");
        assert!(errors(&with(&stray))[0].contains("only for a registry_marketplace source"));
        // The as-of view raises the same rule on records (`rules.rs`).
        assert_eq!(
            IssueCode::JurisdictionMissing.as_str(),
            "jurisdiction_missing"
        );
    }

    #[test]
    fn kinds_keep_their_own_fields() {
        let sec = format!("{SEC_ROW}forms = [\"8-K\", \"4\"]\nentities = [\"sec:cik:320193\"]\nquery = \"x {{from}} {{to}}\"\n");
        let e = errors(&with(&sec));
        assert!(
            e.iter()
                .any(|m| m.contains("forms: `4` is not a kept form")),
            "{e:?}"
        );
        assert!(
            e.iter().any(|m| m.contains("entities: `sec:cik:320193`")),
            "{e:?}"
        );
        assert!(
            e.iter()
                .any(|m| m.contains("query: not for a `sec_edgar` row")),
            "{e:?}"
        );
        let ted = TED_ROW.replace(
            "query = \"publication-date >= {from} AND publication-date <= {to}\"",
            "query = \"cpv = 72000000\"",
        );
        assert!(errors(&with(&ted))[0].contains("must hold {from} and {to}"));
        let ted = format!("{TED_ROW}forms = [\"8-K\"]\nentities = [\"sec:cik:0000320193\"]\n");
        let e = errors(&with(&ted));
        assert!(
            e.iter()
                .any(|m| m.contains("forms: not for a `ted_search` row")),
            "{e:?}"
        );
        assert!(
            e.iter()
                .any(|m| m.contains("entities: not for a `ted_search` row")),
            "{e:?}"
        );
        let anon = SEC_ROW.replace(
            "auth = \"user_agent_env:SEC_USER_AGENT\"",
            "auth = \"none\"",
        );
        assert!(errors(&with(&anon))[0].contains("sec_edgar needs `user_agent_env:<VAR>`"));
        let bad = SEC_ROW.replace("user_agent_env:SEC_USER_AGENT", "user_agent_env:sec");
        assert!(errors(&with(&bad))[0].contains("auth: `user_agent_env:sec`"));
        let cfg = parse(&with(SEC_ROW)).unwrap();
        let row = &cfg.sources.as_ref().unwrap().registry["sec_edgar"];
        assert_eq!(row.forms(), SEC_FORMS.map(String::from).to_vec());
        assert_eq!(
            row.auth(),
            Ok(SourceAuth::UserAgentEnv("SEC_USER_AGENT".into()))
        );
        let ok =
            format!("{SEC_ROW}forms = [\"8-K\", \"8-K/A\"]\nentities = [\"sec:cik:0000320193\"]\n");
        assert_eq!(errors(&with(&ok)), Vec::<String>::new());
        assert!(errors(&with(
            &SEC_ROW.replace("[sources.registry.sec_edgar]", "[sources.registry.SEC]")
        ))[0]
            .starts_with("sources.registry.SEC.id"));
        assert!(
            errors(&BASE.replace("state = \"soe\"", "state = \"../soe\""))[0]
                .starts_with("sources.state")
        );
    }

    /// The state dir stays out of every tool's reach; every agent's tools
    /// read the same section.
    #[test]
    fn state_dir_is_out_of_reach_and_reaches_every_agent() {
        let home = tempfile::TempDir::new().unwrap();
        let state = home.path().join("state");
        let text = format!(
            "{}\n[default_scopes.read_file]\nfs_roots = [\"{}\"]\n",
            with(SEC_ROW),
            state.display()
        );
        let cfg = parse(&text).unwrap();
        let e = rules_at(&cfg, home.path());
        assert!(
            e.iter().any(|m| m.contains("the [sources] state dir")),
            "{e:?}"
        );

        let mut cfg = parse(&with(SEC_ROW)).unwrap();
        cfg.fold_default_scopes();
        let s = &cfg.agents["main"].sandbox;
        assert!(s.sources_state_dir.as_ref().unwrap().ends_with("state/soe"));
        assert_eq!(s.sources.as_ref().unwrap().registry.len(), 1);
        assert_eq!(sources_db(Path::new("/x")), PathBuf::from("/x/sources.db"));
        let mut plain = Config::default();
        plain.fold_default_scopes();
        assert!(plain.agents["main"].sandbox.sources.is_none());
    }

    /// `sandboxes/soe` (O2): an unbound, closed world — one read-only agent,
    /// no shell, no write / contact / spend / publish tool, every registry
    /// row off, every host inside the egress ceiling.
    #[test]
    fn soe_sandbox_is_a_closed_world() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/soe/config.toml");
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert!(
            cfg.generation.is_none(),
            "unbound until the held lineage step"
        );
        assert!(cfg.risk.is_none() && cfg.xmarket.is_none() && cfg.feeds.is_empty());
        assert!(cfg.decision_loops.is_empty() && cfg.mcp_servers.is_empty());
        assert!(cfg.webhooks.endpoints.is_empty());
        assert_eq!(cfg.egress.network, "open");
        assert_eq!(
            cfg.egress.allow_hosts,
            ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]
        );
        let sources = cfg.sources.as_ref().unwrap();
        assert_eq!(sources.state, "soe");
        assert_eq!(
            sources
                .registry
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["sec_edgar", "ted_search"]
        );
        for (id, row) in &sources.registry {
            assert!(!row.enabled, "{id} ships disabled");
            assert!(
                row.terms_sha256.is_none(),
                "{id}: the operator hashes the terms page"
            );
        }
        const READ_ONLY: [&str; 1] = ["source_evidence"];
        for (id, a) in &cfg.agents {
            assert!(
                !a.tools.is_empty(),
                "{id}: an empty list is every base tool"
            );
            assert!(
                a.tools.iter().all(|t| READ_ONLY.contains(&t.as_str())),
                "{id}: {:?}",
                a.tools
            );
            assert!(a.workspace_tools.is_empty(), "{id}");
            if a.engine == "claude_code" {
                let profile = a
                    .claude_code
                    .as_ref()
                    .map(|c| c.builtin_tools_profile.trim());
                assert_eq!(
                    profile,
                    Some("none"),
                    "{id}: built-in tools run outside tengu scopes"
                );
            }
        }
    }

    /// The commented `[sources]` block of `config.example.toml`, uncommented,
    /// is valid.
    #[test]
    fn example_block_uncommented_is_valid() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [sources]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 20, "{block:?}");
        let base = BASE.replace("[sources]\n        state = \"soe\"", "");
        let cfg = parse(&format!("{base}\n{}", block.join("\n"))).unwrap_or_else(|e| panic!("{e}"));
        let home = tempfile::TempDir::new().unwrap();
        assert_eq!(rules_at(&cfg, home.path()), Vec::<String>::new());
        assert_eq!(cfg.sources.unwrap().registry.len(), 2);
    }
}
