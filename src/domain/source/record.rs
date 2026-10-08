//! Source records (O2): one row per thing an approved source published, as a
//! parser read it — the [`Fact`] plus every PRD §5.2 provenance field,
//! lossless (ids, hashes and amounts as written, never shortened, never a
//! float). Pure: fetch, raw snapshots and the append-only store are adapters.
//!
//! | PRD §5.2 field | Here |
//! |---|---|
//! | `source_id`, `source_class`, `url` | `source_id` (the `[sources.registry]` row id), [`SourceClass`] + [`Trust`] (copied from the row at parse time), `url` |
//! | `published_at`, `observed_at`, `valid_from`, `valid_until?` | `published_ms`, `observed_ms` (fetch time of the newest raw snapshot it rests on), `valid_from_ms`, `valid_until_ms?`; plus `parsed_ms` |
//! | `jurisdiction`, `language`, `currency?` | same names; `currency` = the fact's amount currency when it has one |
//! | `content_hash`, `parser_version` | sha256 of the record's content ([`SourceRecord::content_hash_now`]); `parser_version` = `<parser>/<n>` (`sec-submissions/1`) |
//! | `license_or_terms`, `access_method` | same names + `terms_sha256` (the reviewed terms page) |
//! | `fact` | [`Fact`], typed per source; [`Fact::Unparsed`] when the parse failed; [`Fact::Withdrawn`] when the item left the source |
//! | `inference` | never in a record: [`Inference`] is its own type with no path into a [`Fact`] |
//! | `confidence`, `contradictions[]` | computed over many records by the as-of view, never stored on one |
//!
//! | Rule | Value |
//! |---|---|
//! | Schema | `schema = "source_record/1"`; an unknown key is refused at every level |
//! | Identity | `record_id = "<source_id>:<native_id>:<content_hash>"`, each part in full; the same content read twice is the same record |
//! | Content hash | sha256 of the canonical JSON of what the source said: `source_id`, `native_id`, `event_key`, `entities`, `url`, `published_ms`, `valid_from_ms`, `valid_until_ms`, `jurisdiction`, `language`, `currency`, `fact`, `origin`, `supersedes` — never the fetch / parse metadata (clocks, snapshots, parser, terms, class, trust) |
//! | Ids | `source_id` `[a-z0-9_]+`; `native_id` no whitespace (accession, publication number, in full); `event_key` / entities `<scheme>:<kind>:<id>` (`sec:filing:<accession>`, `sec:cik:<10 digits>`, `ted:procedure:<id>`, `ted:buyer:<country>:<id>`) |
//! | Correction | a new record with `supersedes = <old record_id>`; the old record stays |
//! | Withdrawal | an item an earlier read had is gone or moved: a new version of the same native id with fact [`Fact::Withdrawn`] (`how`, `http_status?`, `moved_to?`, `reason`) — [`SourceRecord::withdrawal`]; the earlier versions stay |
//! | Syndication | `origin` names the source a copy came from (`derived_from`); the as-of view counts copies once |
//! | Money | [`NativeAmount`]: the amount as written (plain decimal text) + an ISO 4217 code; O2 never converts (`domain/soe/value.rs` does) |
//! | Places | [`Place`]: role (buyer · performance · legal · delivery) × scheme (ISO 3166 · NUTS) — never one folded field |
//! | Trust by class (PRD §5.1) | `independent_reporting` never `primary`; `social_inference` only `trigger_only` |
//! | Parse | `ok` ⇒ no errors + a typed fact (`withdrawn` included); `partial` ⇒ errors + a typed fact; `error` ⇒ errors + `Unparsed` |
//! | Clocks | `parsed_ms ≥ observed_ms`; `valid_until_ms > valid_from_ms` |

// Consumers (as-of view, store, parsers, `domain/soe/`) land with the next
// O2 / O1 steps.
#![allow(dead_code)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::canonical::canonical_sha256;
use crate::domain::evidence::valid_sha256;
use crate::domain::observation::{ErrorClass, ReadError};

/// The record schema; a new one is a new major version.
pub const RECORD_SCHEMA: &str = "source_record/1";

/// What a source can establish (PRD §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceClass {
    /// Rule, deadline, filing, tender, grant, enforcement.
    LawRegulator,
    /// Product, pricing, shutdown, hiring, filing, incident.
    CompanyPrimary,
    /// Company / asset / listing facts.
    RegistryMarketplace,
    /// Jobs, tenders, reviews, forums, search / support patterns.
    CustomerDemand,
    /// Context and discovery: a trigger until primary evidence is found.
    IndependentReporting,
    /// Weak signal or hypothesis: never sufficient on its own.
    SocialInference,
}

impl SourceClass {
    pub const ALL: [SourceClass; 6] = [
        SourceClass::LawRegulator,
        SourceClass::CompanyPrimary,
        SourceClass::RegistryMarketplace,
        SourceClass::CustomerDemand,
        SourceClass::IndependentReporting,
        SourceClass::SocialInference,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceClass::LawRegulator => "law_regulator",
            SourceClass::CompanyPrimary => "company_primary",
            SourceClass::RegistryMarketplace => "registry_marketplace",
            SourceClass::CustomerDemand => "customer_demand",
            SourceClass::IndependentReporting => "independent_reporting",
            SourceClass::SocialInference => "social_inference",
        }
    }

    /// Whether a source of this class may carry `trust` (module table).
    pub fn allows(self, trust: Trust) -> bool {
        match self {
            SourceClass::IndependentReporting => trust != Trust::Primary,
            SourceClass::SocialInference => trust == Trust::TriggerOnly,
            _ => true,
        }
    }
}

/// How far one record of a source counts toward an event's confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Establishes the fact on its own.
    Primary,
    /// Counts toward two independent confirmations.
    Corroborating,
    /// Starts a search; confirms nothing.
    TriggerOnly,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Primary => "primary",
            Trust::Corroborating => "corroborating",
            Trust::TriggerOnly => "trigger_only",
        }
    }
}

/// How the parse went (module table: parse rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Ok,
    Partial,
    Error,
}

/// How the raw bytes were obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMethod {
    /// An API or page read with GET (SEC EDGAR).
    HttpGet,
    /// A search API read with POST (EU TED Search).
    HttpPost,
    /// A bulk / archive file.
    BulkFile,
}

/// One field a parser could not read. Shown, never dropped: a `partial` or
/// `error` record carries at least one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseError {
    /// The source field (`acceptanceDateTime`, `notice-type`, …).
    pub field: String,
    /// Never a URL.
    pub message: String,
}

impl ParseError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }

    /// As an observation error (class `decode`).
    pub fn to_read_error(&self) -> ReadError {
        ReadError::new(self.field.clone(), ErrorClass::Decode, self.message.clone())
    }
}

/// An amount as the source wrote it (module table: money).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAmount {
    /// Plain decimal text as written: `150000.00` stays `150000.00`.
    pub amount: String,
    /// ISO 4217 code, three uppercase letters (`EUR`, `PLN`, `USD`).
    pub currency: String,
}

impl NativeAmount {
    pub fn new(amount: impl Into<String>, currency: impl Into<String>) -> Self {
        Self {
            amount: amount.into(),
            currency: currency.into(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !valid_decimal(&self.amount) {
            return Err(format!(
                "amount `{}` is not a plain decimal (digits, one optional `.` between digits, optional leading `-`)",
                self.amount
            ));
        }
        if !valid_currency(&self.currency) {
            return Err(format!(
                "currency `{}` is not an ISO 4217 code (three uppercase letters)",
                self.currency
            ));
        }
        Ok(())
    }
}

/// What a place is to the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceRole {
    Buyer,
    Performance,
    Legal,
    Delivery,
}

impl PlaceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaceRole::Buyer => "buyer",
            PlaceRole::Performance => "performance",
            PlaceRole::Legal => "legal",
            PlaceRole::Delivery => "delivery",
        }
    }
}

/// The code system of a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceScheme {
    /// ISO 3166-1 alpha-2 or alpha-3 (`DE`, `DEU`).
    Iso3166,
    /// EU NUTS: a two-letter country + up to three levels (`DE`, `DE2`, `DE21`, `DE212`).
    Nuts,
}

impl PlaceScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaceScheme::Iso3166 => "iso3166",
            PlaceScheme::Nuts => "nuts",
        }
    }
}

/// One place of a fact (module table: places).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    pub role: PlaceRole,
    pub scheme: PlaceScheme,
    pub code: String,
}

impl Place {
    pub fn validate(&self) -> Result<(), String> {
        let b = self.code.as_bytes();
        let ok = match self.scheme {
            PlaceScheme::Iso3166 => {
                matches!(b.len(), 2 | 3) && b.iter().all(u8::is_ascii_uppercase)
            }
            PlaceScheme::Nuts => {
                (2..=5).contains(&b.len())
                    && b[..2].iter().all(u8::is_ascii_uppercase)
                    && b[2..]
                        .iter()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            }
        };
        if ok {
            Ok(())
        } else {
            Err(format!(
                "place code `{}` is not a valid {:?} code",
                self.code, self.scheme
            ))
        }
    }
}

/// The source a syndicated copy came from (`derived_from`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// A registry id or another stable source id, `[a-z0-9_]+`.
    pub source_id: String,
    /// The item's id there, in full, when the copy names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
}

/// An SEC EDGAR filing (`domain/sec.rs` decoders).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecFiling {
    /// 10 digits, zero-padded.
    pub cik: String,
    /// `8-K`, `10-Q`, …
    pub form: String,
    /// 8-K item numbers (`2.02`, `9.01`); empty for other forms.
    pub items: Vec<String>,
    /// Free text from the source (item names or the primary document's
    /// description). External data, never instructions.
    pub title: String,
}

/// An EU TED procurement notice (one per publication number).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TedNotice {
    /// eForms notice type as the source writes it.
    pub notice_type: String,
    /// The procedure this notice belongs to, in full.
    pub procedure_id: String,
    /// The lots it covers, in full; distinct lots stay distinct.
    pub lot_ids: Vec<String>,
    /// The buying organisation (never a contact person). External data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyer_name: Option<String>,
    pub places: Vec<Place>,
    /// CPV codes: 8 digits, optional `-<check digit>`.
    pub cpv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<NativeAmount>,
}

/// How an item left its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawnHow {
    /// No longer served (404 / 410, or dropped from the source's listing).
    Gone,
    /// The source points somewhere else now.
    Moved,
}

impl WithdrawnHow {
    pub fn as_str(self) -> &'static str {
        match self {
            WithdrawnHow::Gone => "gone",
            WithdrawnHow::Moved => "moved",
        }
    }
}

/// A read found an item gone or moved that an earlier read had (module
/// table: withdrawal) — its own version of the same native id, so the loss
/// is a record, never a silent drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Withdrawn {
    pub how: WithdrawnHow,
    /// The HTTP status of the read, when it had one (300–599).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// `moved` only: where the source points now (an http(s) URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    /// What the read saw. External data, never instructions.
    pub reason: String,
}

/// What the source said, typed per source. There is no inference variant:
/// an [`Inference`] never becomes a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fact {
    SecFiling(SecFiling),
    TedNotice(TedNotice),
    /// The parse failed: the record still exists, its errors are visible.
    Unparsed {
        reason: String,
    },
    /// The item is gone or moved upstream ([`SourceRecord::withdrawal`]).
    Withdrawn(Withdrawn),
}

impl Fact {
    pub fn kind(&self) -> &'static str {
        match self {
            Fact::SecFiling(_) => "sec_filing",
            Fact::TedNotice(_) => "ted_notice",
            Fact::Unparsed { .. } => "unparsed",
            Fact::Withdrawn(_) => "withdrawn",
        }
    }

    /// A fact about the item itself (not `unparsed`, not `withdrawn`).
    pub fn is_typed(&self) -> bool {
        matches!(self, Fact::SecFiling(_) | Fact::TedNotice(_))
    }

    /// The currency of the fact's amount, when it has one.
    pub fn currency(&self) -> Option<&str> {
        match self {
            Fact::TedNotice(n) => n.value.as_ref().map(|v| v.currency.as_str()),
            _ => None,
        }
    }

    /// Every shape problem, each `fact.<field>: …`.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Fact::SecFiling(f) => {
                if !(f.cik.len() == 10 && f.cik.bytes().all(|c| c.is_ascii_digit())) {
                    out.push(format!("fact.cik: `{}` is not 10 digits", f.cik));
                }
                if !valid_token(&f.form) {
                    out.push(format!(
                        "fact.form: `{}` is empty or has whitespace",
                        f.form
                    ));
                }
                if let Some(i) = f.items.iter().find(|i| !valid_token(i)) {
                    out.push(format!("fact.items: `{i}` is empty or has whitespace"));
                }
            }
            Fact::TedNotice(n) => {
                if !valid_token(&n.notice_type) {
                    out.push(format!(
                        "fact.notice_type: `{}` is empty or has whitespace",
                        n.notice_type
                    ));
                }
                if !valid_token(&n.procedure_id) {
                    out.push(format!(
                        "fact.procedure_id: `{}` is empty or has whitespace",
                        n.procedure_id
                    ));
                }
                if let Some(l) = n.lot_ids.iter().find(|l| !valid_token(l)) {
                    out.push(format!("fact.lot_ids: `{l}` is empty or has whitespace"));
                }
                if let Some(l) = first_duplicate(&n.lot_ids) {
                    out.push(format!("fact.lot_ids: `{l}` listed twice"));
                }
                if n.buyer_name.as_deref().is_some_and(|b| b.trim().is_empty()) {
                    out.push("fact.buyer_name: empty (omit it instead)".into());
                }
                for p in &n.places {
                    if let Err(e) = p.validate() {
                        out.push(format!("fact.places: {e}"));
                    }
                }
                if let Some(c) = n.cpv.iter().find(|c| !valid_cpv(c)) {
                    out.push(format!(
                        "fact.cpv: `{c}` is not 8 digits with an optional `-<check digit>`"
                    ));
                }
                if let Some(Err(e)) = n.value.as_ref().map(NativeAmount::validate) {
                    out.push(format!("fact.value: {e}"));
                }
            }
            Fact::Unparsed { reason } => {
                if reason.trim().is_empty() {
                    out.push("fact.reason: empty".into());
                }
            }
            Fact::Withdrawn(w) => {
                if w.reason.trim().is_empty() {
                    out.push("fact.reason: empty".into());
                }
                match (w.how, w.moved_to.as_deref()) {
                    (WithdrawnHow::Moved, None) => {
                        out.push("fact.moved_to: required when `how = moved`".into())
                    }
                    (WithdrawnHow::Gone, Some(_)) => {
                        out.push("fact.moved_to: only when `how = moved`".into())
                    }
                    (_, Some(u)) if !valid_url(u) => {
                        out.push(format!("fact.moved_to: `{u}` is not an http(s) URL"))
                    }
                    _ => {}
                }
                if let Some(s) = w.http_status.filter(|s| !(300..=599).contains(s)) {
                    out.push(format!("fact.http_status: {s} is not 300–599"));
                }
            }
        }
        out
    }
}

/// A model's reading of facts: analysis, never evidence. Kept apart from
/// [`Fact`] by type (no conversion exists) and never stored on a
/// [`SourceRecord`] (no field takes it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inference {
    pub text: String,
    /// The model id in full (`anthropic/claude-sonnet-4-6`).
    pub model: String,
    /// sha256 of the prompt that produced it, 64 lowercase hex.
    pub prompt_sha256: String,
    /// The lineage generation it ran under, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// The as-of instant of the facts it read.
    pub as_of_ms: i64,
}

impl Inference {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut out = Vec::new();
        if self.text.trim().is_empty() {
            out.push("text: empty".to_string());
        }
        if !valid_token(&self.model) {
            out.push(format!(
                "model: `{}` is empty or has whitespace",
                self.model
            ));
        }
        if !valid_sha256(&self.prompt_sha256) {
            out.push(format!(
                "prompt_sha256: `{}` is not 64 lowercase hex",
                self.prompt_sha256
            ));
        }
        if self.generation.as_deref().is_some_and(|g| !valid_token(g)) {
            out.push("generation: empty or has whitespace (omit it instead)".into());
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }
}

/// One fact from one source with its full provenance (module tables).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    /// [`RECORD_SCHEMA`].
    pub schema: String,
    /// `<source_id>:<native_id>:<content_hash>`, in full.
    pub record_id: String,
    pub source_id: String,
    pub source_class: SourceClass,
    pub trust: Trust,
    /// The source's own id, in full (accession, publication number).
    pub native_id: String,
    /// The real-world event this record is about (`sec:filing:<accession>`,
    /// `ted:procedure:<id>`); corrections and copies share it.
    pub event_key: String,
    /// `sec:cik:<10 digits>`, `ted:buyer:<country>:<id>`.
    pub entities: Vec<String>,
    pub url: String,
    /// When the source made it public.
    pub published_ms: i64,
    /// Fetch time of the newest raw snapshot this record rests on.
    pub observed_ms: i64,
    /// When this parser produced the record.
    pub parsed_ms: i64,
    pub valid_from_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until_ms: Option<i64>,
    pub jurisdiction: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// sha256 of the content (module table), 64 lowercase hex.
    pub content_hash: String,
    /// sha256 of each raw body it was parsed from, in full.
    pub snapshots: Vec<String>,
    /// `<parser>/<n>`.
    pub parser_version: String,
    pub license_or_terms: String,
    /// sha256 of the terms page the operator reviewed.
    pub terms_sha256: String,
    pub access_method: AccessMethod,
    pub parse: ParseStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parse_errors: Vec<ParseError>,
    pub fact: Fact,
    /// The `record_id` this record corrects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Set on a syndicated copy (`derived_from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

impl SourceRecord {
    /// What the content hash covers (module table) — what the source said,
    /// never when or how we read it.
    fn content(&self) -> Value {
        json!({
            "source_id": self.source_id,
            "native_id": self.native_id,
            "event_key": self.event_key,
            "entities": self.entities,
            "url": self.url,
            "published_ms": self.published_ms,
            "valid_from_ms": self.valid_from_ms,
            "valid_until_ms": self.valid_until_ms,
            "jurisdiction": self.jurisdiction,
            "language": self.language,
            "currency": self.currency,
            "fact": self.fact,
            "origin": self.origin,
            "supersedes": self.supersedes,
        })
    }

    /// The content hash recomputed from the record's fields.
    pub fn content_hash_now(&self) -> String {
        canonical_sha256(&self.content())
    }

    /// Sets `content_hash` and `record_id` from the content: the last step
    /// of every parser.
    pub fn with_identity(mut self) -> Self {
        self.content_hash = self.content_hash_now();
        self.record_id = record_id_of(&self.source_id, &self.native_id, &self.content_hash);
        self
    }

    /// The version saying this item is gone or moved (module table:
    /// withdrawal): the same source, native id, event, entities, url and
    /// clocks of publication; the fact [`Fact::Withdrawn`], read from
    /// `snapshot` at `observed_ms`. What the source said before stays in its
    /// own record. Reading the same withdrawal again gives the same
    /// `record_id` (the read times are not content).
    pub fn withdrawal(
        &self,
        withdrawn: Withdrawn,
        snapshot: String,
        observed_ms: i64,
        parsed_ms: i64,
    ) -> SourceRecord {
        SourceRecord {
            record_id: String::new(),
            content_hash: String::new(),
            currency: None,
            observed_ms,
            parsed_ms,
            snapshots: vec![snapshot],
            parse: ParseStatus::Ok,
            parse_errors: Vec::new(),
            fact: Fact::Withdrawn(withdrawn),
            supersedes: None,
            ..self.clone()
        }
        .with_identity()
    }

    /// Every rule of the module tables; each problem `<field>: …`.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut out = Vec::new();
        if self.schema != RECORD_SCHEMA {
            out.push(format!(
                "schema: `{}` is not `{RECORD_SCHEMA}`",
                self.schema
            ));
        }
        if !valid_source_id(&self.source_id) {
            out.push(format!("source_id: `{}` is not [a-z0-9_]+", self.source_id));
        }
        if !valid_token(&self.native_id) {
            out.push(format!(
                "native_id: `{}` is empty or has whitespace",
                self.native_id
            ));
        }
        if !valid_key(&self.event_key) {
            out.push(format!(
                "event_key: `{}` is not <scheme>:<kind>:<id>",
                self.event_key
            ));
        }
        if let Some(e) = self.entities.iter().find(|e| !valid_key(e)) {
            out.push(format!("entities: `{e}` is not <scheme>:<kind>:<id>"));
        }
        if let Some(e) = first_duplicate(&self.entities) {
            out.push(format!("entities: `{e}` listed twice"));
        }
        if !valid_url(&self.url) {
            out.push(format!("url: `{}` is not an http(s) URL", self.url));
        }
        if self.parsed_ms < self.observed_ms {
            out.push(format!(
                "parsed_ms: {} is before observed_ms {}",
                self.parsed_ms, self.observed_ms
            ));
        }
        if let Some(until) = self.valid_until_ms.filter(|u| *u <= self.valid_from_ms) {
            out.push(format!(
                "valid_until_ms: {until} is not after valid_from_ms {}",
                self.valid_from_ms
            ));
        }
        if !valid_token(&self.jurisdiction) {
            out.push("jurisdiction: empty or has whitespace".into());
        }
        if !valid_token(&self.language) {
            out.push("language: empty or has whitespace".into());
        }
        if let Some(c) = self.currency.as_deref().filter(|c| !valid_currency(c)) {
            out.push(format!("currency: `{c}` is not an ISO 4217 code"));
        }
        if self.currency.as_deref() != self.fact.currency() && self.fact.currency().is_some() {
            out.push(format!(
                "currency: {:?} differs from the fact's amount currency {:?}",
                self.currency,
                self.fact.currency()
            ));
        }
        if !valid_sha256(&self.content_hash) {
            out.push(format!(
                "content_hash: `{}` is not 64 lowercase hex",
                self.content_hash
            ));
        } else if self.content_hash != self.content_hash_now() {
            out.push(format!(
                "content_hash: `{}` does not match the content (`{}`)",
                self.content_hash,
                self.content_hash_now()
            ));
        }
        let id = record_id_of(&self.source_id, &self.native_id, &self.content_hash);
        if self.record_id != id {
            out.push(format!("record_id: `{}` is not `{id}`", self.record_id));
        }
        if self.snapshots.is_empty() {
            out.push("snapshots: none (a record rests on at least one raw snapshot)".into());
        }
        if let Some(s) = self.snapshots.iter().find(|s| !valid_sha256(s)) {
            out.push(format!("snapshots: `{s}` is not 64 lowercase hex"));
        }
        if let Some(s) = first_duplicate(&self.snapshots) {
            out.push(format!("snapshots: `{s}` listed twice"));
        }
        if split_parser_version(&self.parser_version).is_none() {
            out.push(format!(
                "parser_version: `{}` is not <parser>/<n ≥ 1>",
                self.parser_version
            ));
        }
        if self.license_or_terms.trim().is_empty() {
            out.push("license_or_terms: empty".into());
        }
        if !valid_sha256(&self.terms_sha256) {
            out.push(format!(
                "terms_sha256: `{}` is not 64 lowercase hex",
                self.terms_sha256
            ));
        }
        if !self.source_class.allows(self.trust) {
            out.push(format!(
                "trust: a `{}` source cannot be `{}`",
                self.source_class.as_str(),
                self.trust.as_str()
            ));
        }
        let unparsed = matches!(self.fact, Fact::Unparsed { .. });
        match (self.parse, self.parse_errors.is_empty(), unparsed) {
            (ParseStatus::Ok, true, false)
            | (ParseStatus::Partial, false, false)
            | (ParseStatus::Error, false, true) => {}
            (status, _, _) => out.push(format!(
                "parse: `{status:?}` with {} error(s) and a `{}` fact (ok ⇒ no errors + typed fact; partial ⇒ errors + typed fact; error ⇒ errors + unparsed)",
                self.parse_errors.len(),
                self.fact.kind()
            )),
        }
        if let Some(e) = self
            .parse_errors
            .iter()
            .find(|e| e.field.trim().is_empty() || e.message.trim().is_empty())
        {
            out.push(format!(
                "parse_errors: field and message required (`{}`: `{}`)",
                e.field, e.message
            ));
        }
        out.extend(self.fact.problems());
        if let Some(s) = &self.supersedes {
            if split_record_id(s).is_none() {
                out.push(format!(
                    "supersedes: `{s}` is not <source_id>:<native_id>:<content_hash>"
                ));
            } else if *s == self.record_id {
                out.push("supersedes: the record itself".into());
            }
        }
        if let Some(o) = &self.origin {
            if !valid_source_id(&o.source_id) {
                out.push(format!(
                    "origin.source_id: `{}` is not [a-z0-9_]+",
                    o.source_id
                ));
            }
            if o.native_id.as_deref().is_some_and(|n| !valid_token(n)) {
                out.push("origin.native_id: empty or has whitespace (omit it instead)".into());
            }
            if o.source_id == self.source_id
                && o.native_id.as_deref() == Some(self.native_id.as_str())
            {
                out.push("origin: the record itself".into());
            }
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }
}

/// `<source_id>:<native_id>:<content_hash>`, each part in full.
pub fn record_id_of(source_id: &str, native_id: &str, content_hash: &str) -> String {
    format!("{source_id}:{native_id}:{content_hash}")
}

/// A record id's `(source_id, native_id, content_hash)`; `None` unless each
/// part is valid. `native_id` may itself hold `:`.
pub fn split_record_id(id: &str) -> Option<(&str, &str, &str)> {
    let (source, rest) = id.split_once(':')?;
    let (native, hash) = rest.rsplit_once(':')?;
    (valid_source_id(source) && valid_token(native) && valid_sha256(hash))
        .then_some((source, native, hash))
}

/// `sec:filing:<accession>`.
pub fn sec_filing_key(accession: &str) -> String {
    format!("sec:filing:{accession}")
}

/// `sec:cik:<10 digits>`.
pub fn sec_cik_entity(cik10: &str) -> String {
    format!("sec:cik:{cik10}")
}

/// `ted:procedure:<procedure id>`.
pub fn ted_procedure_key(procedure_id: &str) -> String {
    format!("ted:procedure:{procedure_id}")
}

/// `ted:buyer:<country>:<buyer id>`.
pub fn ted_buyer_entity(country: &str, buyer_id: &str) -> String {
    format!("ted:buyer:{country}:{buyer_id}")
}

/// `<parser>/<n>` → `(parser, n)`; parser `[a-z0-9_-]+`, n ≥ 1 without a
/// sign or leading zero. The as-of view breaks ties on `n`.
pub fn split_parser_version(v: &str) -> Option<(&str, u32)> {
    let (name, n) = v.split_once('/')?;
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_');
    let n_ok = !n.is_empty() && !n.starts_with('0') && n.bytes().all(|c| c.is_ascii_digit());
    if !(name_ok && n_ok) {
        return None;
    }
    n.parse().ok().map(|n| (name, n))
}

/// A registry / record source id: `[a-z0-9_]+`.
pub fn valid_source_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// Non-empty, no whitespace or control character.
fn valid_token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A token starting `https://` or `http://`.
fn valid_url(s: &str) -> bool {
    valid_token(s) && (s.starts_with("https://") || s.starts_with("http://"))
}

/// `<scheme>:<kind>:<id>` — at least three non-empty `:` parts, no whitespace.
fn valid_key(s: &str) -> bool {
    valid_token(s) && s.split(':').count() >= 3 && s.split(':').all(|p| !p.is_empty())
}

/// Digits, one optional `.` between digits, an optional leading `-`.
pub fn valid_decimal(s: &str) -> bool {
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
    let body = s.strip_prefix('-').unwrap_or(s);
    match body.split_once('.') {
        Some((int, frac)) => digits(int) && digits(frac),
        None => digits(body),
    }
}

/// Three uppercase ASCII letters.
pub fn valid_currency(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|c| c.is_ascii_uppercase())
}

/// 8 digits, optionally `-<one digit>` (the CPV check digit).
fn valid_cpv(s: &str) -> bool {
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|c| c.is_ascii_digit());
    match s.split_once('-') {
        Some((code, check)) => digits(code, 8) && digits(check, 1),
        None => digits(s, 8),
    }
}

fn first_duplicate(items: &[String]) -> Option<&String> {
    let mut seen = BTreeSet::new();
    items.iter().find(|i| !seen.insert(i.as_str()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::canonical::{canonical_json, sha256_hex};

    // Synthetic values: round, not real filings or notices.
    const CIK: &str = "0000320193";
    const ACCESSION: &str = "0000320193-26-000018";
    const TED_PUB: &str = "612345-2026";
    const TED_PROC: &str = "1c7b9e2a-4d0f-4b6e-9c3a-2f5d8e7a6b10";

    fn h(tag: &str) -> String {
        sha256_hex(tag)
    }

    fn sec_record() -> SourceRecord {
        SourceRecord {
            schema: RECORD_SCHEMA.into(),
            record_id: String::new(),
            source_id: "sec_edgar".into(),
            source_class: SourceClass::LawRegulator,
            trust: Trust::Primary,
            native_id: ACCESSION.into(),
            event_key: sec_filing_key(ACCESSION),
            entities: vec![sec_cik_entity(CIK)],
            url: format!(
                "https://www.sec.gov/Archives/edgar/data/320193/000032019326000018/{ACCESSION}-index.htm"
            ),
            published_ms: 1_760_000_000_000,
            observed_ms: 1_760_000_600_000,
            parsed_ms: 1_760_000_601_000,
            valid_from_ms: 1_760_000_000_000,
            valid_until_ms: None,
            jurisdiction: "US".into(),
            language: "en".into(),
            currency: None,
            content_hash: String::new(),
            snapshots: vec![h("submissions body"), h("index page body")],
            parser_version: "sec-submissions/1".into(),
            license_or_terms: "public EDGAR data; SEC fair-access policy".into(),
            terms_sha256: h("sec terms page"),
            access_method: AccessMethod::HttpGet,
            parse: ParseStatus::Ok,
            parse_errors: vec![],
            fact: Fact::SecFiling(SecFiling {
                cik: CIK.into(),
                form: "8-K".into(),
                items: vec!["2.02".into(), "9.01".into()],
                title: "Results of operations; Financial statements and exhibits".into(),
            }),
            supersedes: None,
            origin: None,
        }
        .with_identity()
    }

    /// Every optional field set: a partial parse, a correction, a copy.
    fn ted_record() -> SourceRecord {
        SourceRecord {
            schema: RECORD_SCHEMA.into(),
            record_id: String::new(),
            source_id: "ted_search".into(),
            source_class: SourceClass::CustomerDemand,
            trust: Trust::Corroborating,
            native_id: TED_PUB.into(),
            event_key: ted_procedure_key(TED_PROC),
            entities: vec![ted_buyer_entity("DEU", "buyer-0001")],
            url: format!("https://ted.europa.eu/en/notice/-/detail/{TED_PUB}"),
            published_ms: 1_760_000_000_000,
            observed_ms: 1_760_086_400_000,
            parsed_ms: 1_760_086_400_500,
            valid_from_ms: 1_760_000_000_000,
            valid_until_ms: Some(1_762_000_000_000),
            jurisdiction: "EU".into(),
            language: "de".into(),
            currency: Some("EUR".into()),
            content_hash: String::new(),
            snapshots: vec![h("ted page body")],
            parser_version: "ted-search/2".into(),
            license_or_terms: "EU Publications Office reuse policy".into(),
            terms_sha256: h("ted terms page"),
            access_method: AccessMethod::HttpPost,
            parse: ParseStatus::Partial,
            parse_errors: vec![ParseError::new("deadline-receipt-tender", "no time zone")],
            fact: Fact::TedNotice(TedNotice {
                notice_type: "cn-standard".into(),
                procedure_id: TED_PROC.into(),
                lot_ids: vec!["LOT-0001".into(), "LOT-0002".into()],
                buyer_name: Some("Example City Council".into()),
                places: vec![
                    Place {
                        role: PlaceRole::Buyer,
                        scheme: PlaceScheme::Iso3166,
                        code: "DEU".into(),
                    },
                    Place {
                        role: PlaceRole::Performance,
                        scheme: PlaceScheme::Nuts,
                        code: "DE212".into(),
                    },
                ],
                cpv: vec!["72000000".into(), "48000000-8".into()],
                deadline_ms: Some(1_761_000_000_000),
                value: Some(NativeAmount::new("150000.00", "EUR")),
            }),
            supersedes: Some(record_id_of("ted_search", "600000-2026", &h("earlier"))),
            origin: Some(Origin {
                source_id: "ted_bulk".into(),
                native_id: Some(TED_PUB.into()),
            }),
        }
        .with_identity()
    }

    fn unparsed_record() -> SourceRecord {
        let mut r = sec_record();
        r.parse = ParseStatus::Error;
        r.parse_errors = vec![ParseError::new("acceptanceDateTime", "not a date")];
        r.fact = Fact::Unparsed {
            reason: "submissions row 3: bad acceptanceDateTime".into(),
        };
        r.with_identity()
    }

    fn problems(r: &SourceRecord) -> Vec<String> {
        r.validate().err().unwrap_or_default()
    }

    /// `r` (with its identity recomputed) fails with a problem naming `field`.
    fn refused(r: SourceRecord, field: &str) {
        let p = problems(&r.with_identity());
        assert!(
            p.iter().any(|m| m.starts_with(field)),
            "expected a `{field}` problem, got {p:?}"
        );
    }

    #[test]
    fn provenance_round_trips_losslessly() {
        for r in [sec_record(), ted_record(), unparsed_record()] {
            assert_eq!(r.validate(), Ok(()), "{}", r.record_id);
            let text = serde_json::to_string(&r).unwrap();
            let back: SourceRecord = serde_json::from_str(&text).unwrap();
            assert_eq!(back, r);
            assert_eq!(serde_json::to_string(&back).unwrap(), text);
            let (a, b) = (
                serde_json::to_value(&r).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
            assert_eq!(canonical_json(&a), canonical_json(&b));
            assert_eq!(back.content_hash_now(), r.content_hash);
        }
        // Every PRD §5.2 field is on the wire of the fullest record.
        let v = serde_json::to_value(ted_record()).unwrap();
        for k in [
            "source_id",
            "source_class",
            "url",
            "published_ms",
            "observed_ms",
            "valid_from_ms",
            "valid_until_ms",
            "jurisdiction",
            "language",
            "currency",
            "content_hash",
            "parser_version",
            "license_or_terms",
            "access_method",
            "fact",
            "supersedes",
            "origin",
        ] {
            assert!(v.get(k).is_some(), "`{k}` missing on the wire");
        }
    }

    #[test]
    fn unknown_fields_are_refused() {
        let base = serde_json::to_value(ted_record()).unwrap();
        let rejects = |edit: &dyn Fn(&mut Value)| {
            let mut v = base.clone();
            edit(&mut v);
            let r = serde_json::from_value::<SourceRecord>(v.clone());
            assert!(r.is_err(), "accepted {v}");
        };
        // The unedited record parses: each rejection below is its edit's.
        assert!(serde_json::from_value::<SourceRecord>(base.clone()).is_ok());
        rejects(&|v| v["extra"] = json!(1));
        // An inference has no slot in a record, nor in its fact.
        rejects(&|v| v["inference"] = json!({"text": "probably a big buyer"}));
        rejects(&|v| v["fact"]["inference"] = json!("probably a big buyer"));
        rejects(&|v| v["fact"]["kind"] = json!("inference"));
        rejects(&|v| v["fact"]["value"]["converted"] = json!({"amount": "1"}));
        rejects(&|v| v["fact"]["places"][0]["city"] = json!("Example"));
        rejects(&|v| v["origin"]["org"] = json!("x"));
        rejects(&|v| v["parse_errors"][0]["class"] = json!("decode"));
        rejects(&|v| v["source_class"] = json!("news"));
        rejects(&|v| v["trust"] = json!("high"));
        // A number is not an amount as written.
        rejects(&|v| v["fact"]["value"]["amount"] = json!(150000.0));
        // The SEC fact refuses keys too (newtype variant inside the tag).
        let mut sec = serde_json::to_value(sec_record()).unwrap();
        sec["fact"]["accession"] = json!(ACCESSION);
        assert!(serde_json::from_value::<SourceRecord>(sec).is_err());
        let mut unparsed = serde_json::to_value(unparsed_record()).unwrap();
        unparsed["fact"]["raw"] = json!("…");
        assert!(serde_json::from_value::<SourceRecord>(unparsed).is_err());
        // Inference refuses keys of its own.
        let inf =
            json!({"text": "t", "model": "m", "prompt_sha256": h("p"), "as_of_ms": 1, "fact": {}});
        assert!(serde_json::from_value::<Inference>(inf).is_err());
    }

    #[test]
    fn ids_are_kept_in_full() {
        let r = sec_record();
        assert_eq!(r.content_hash.len(), 64);
        assert_eq!(
            r.record_id,
            format!("sec_edgar:{ACCESSION}:{}", r.content_hash)
        );
        assert_eq!(
            split_record_id(&r.record_id),
            Some(("sec_edgar", ACCESSION, r.content_hash.as_str()))
        );
        let text = serde_json::to_string(&r).unwrap();
        for whole in [
            ACCESSION,
            r.content_hash.as_str(),
            r.record_id.as_str(),
            "\"cik\":\"0000320193\"",
            "sec:cik:0000320193",
            "sec:filing:0000320193-26-000018",
        ] {
            assert!(text.contains(whole), "`{whole}` not whole in {text}");
        }
        for s in &r.snapshots {
            assert!(text.contains(s.as_str()));
        }
        // A UUID procedure id and a native id with `:` survive whole.
        let t = ted_record();
        let text = serde_json::to_string(&t).unwrap();
        assert!(text.contains(&format!("ted:procedure:{TED_PROC}")));
        let id = record_id_of("gh_events", "repo:123:456", &h("x"));
        assert_eq!(
            split_record_id(&id),
            Some(("gh_events", "repo:123:456", h("x").as_str()))
        );
        assert_eq!(
            split_record_id(&format!("sec_edgar:{ACCESSION}:{}", &h("x")[..12])),
            None
        );
    }

    #[test]
    fn refetch_of_same_content_keeps_its_identity() {
        let r = sec_record();
        let mut again = r.clone();
        again.observed_ms += 86_400_000;
        again.parsed_ms += 86_400_000;
        again.snapshots = vec![h("refetched body")];
        again.parser_version = "sec-submissions/2".into();
        assert_eq!(again.clone().with_identity().record_id, r.record_id);
        // What the source said changes ⇒ a new record.
        let mut moved = r.clone();
        moved.published_ms += 1;
        let moved = moved.with_identity();
        assert_ne!(moved.record_id, r.record_id);
        let mut edited = r.clone();
        if let Fact::SecFiling(f) = &mut edited.fact {
            f.title.push('.');
        }
        assert_ne!(edited.with_identity().content_hash, r.content_hash);
    }

    #[test]
    fn money_keeps_native_amount_and_currency() {
        let r = ted_record();
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains(r#""value":{"amount":"150000.00","currency":"EUR"}"#));
        let back: SourceRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back.fact.currency(), Some("EUR"));
        let Fact::TedNotice(n) = &back.fact else {
            panic!("ted notice expected")
        };
        assert_eq!(n.value, Some(NativeAmount::new("150000.00", "EUR")));
        for ok in [
            "0",
            "150000",
            "150000.00",
            "0.5",
            "-12.30",
            "12345678901234567890.123",
        ] {
            assert!(NativeAmount::new(ok, "PLN").validate().is_ok(), "{ok}");
        }
        for bad in [
            "", "1e5", "1,000.00", "150000.", ".5", "NaN", " 1", "+1", "1.2.3", "-",
        ] {
            assert!(NativeAmount::new(bad, "EUR").validate().is_err(), "{bad}");
        }
        for bad in ["eur", "EURO", "", "E1R", "€"] {
            assert!(NativeAmount::new("1", bad).validate().is_err(), "{bad}");
        }
        // The record's currency is the fact's.
        let mut none = ted_record();
        none.currency = None;
        refused(none, "currency");
        let mut other = ted_record();
        other.currency = Some("USD".into());
        refused(other, "currency");
        let mut bad = ted_record();
        if let Fact::TedNotice(n) = &mut bad.fact {
            n.value = Some(NativeAmount::new("1,000", "EUR"));
        }
        refused(bad, "fact.value");
    }

    #[test]
    fn validate_refuses_missing_provenance() {
        type Edit = fn(&mut SourceRecord);
        let cases: Vec<(&str, Edit)> = vec![
            ("schema", |r| r.schema = "source_record/2".into()),
            ("source_id", |r| r.source_id = "SEC-Edgar".into()),
            ("native_id", |r| r.native_id = String::new()),
            ("event_key", |r| r.event_key = "filing".into()),
            ("entities", |r| r.entities.push(r.entities[0].clone())),
            ("url", |r| r.url = String::new()),
            ("url", |r| r.url = "ftp://example.org/x".into()),
            ("parsed_ms", |r| r.parsed_ms = r.observed_ms - 1),
            ("valid_until_ms", |r| {
                r.valid_until_ms = Some(r.valid_from_ms)
            }),
            ("jurisdiction", |r| r.jurisdiction = String::new()),
            ("language", |r| r.language = " ".into()),
            ("snapshots", |r| r.snapshots.clear()),
            ("snapshots", |r| r.snapshots[0].truncate(12)),
            ("snapshots", |r| r.snapshots[1] = r.snapshots[0].clone()),
            ("parser_version", |r| {
                r.parser_version = "sec-submissions".into()
            }),
            ("parser_version", |r| {
                r.parser_version = "sec-submissions/0".into()
            }),
            ("license_or_terms", |r| r.license_or_terms = " ".into()),
            ("terms_sha256", |r| r.terms_sha256.truncate(12)),
            ("trust", |r| {
                r.source_class = SourceClass::SocialInference;
                r.trust = Trust::Corroborating;
            }),
            ("trust", |r| {
                r.source_class = SourceClass::IndependentReporting
            }),
            ("parse", |r| {
                r.parse_errors = vec![ParseError::new("form", "unknown")];
            }),
            ("parse", |r| r.parse = ParseStatus::Error),
            ("fact.cik", |r| {
                if let Fact::SecFiling(f) = &mut r.fact {
                    f.cik = "320193".into();
                }
            }),
            ("supersedes", |r| r.supersedes = Some("sec_edgar:x".into())),
            ("origin.source_id", |r| {
                r.origin = Some(Origin {
                    source_id: "Reuters".into(),
                    native_id: None,
                })
            }),
            ("origin", |r| {
                r.origin = Some(Origin {
                    source_id: r.source_id.clone(),
                    native_id: Some(r.native_id.clone()),
                })
            }),
        ];
        for (field, edit) in cases {
            let mut r = sec_record();
            edit(&mut r);
            refused(r, field);
        }
        // Tampered content or identity, without recomputing it.
        let mut r = sec_record();
        r.published_ms -= 1;
        assert!(problems(&r).iter().any(|p| p.starts_with("content_hash")));
        let mut r = sec_record();
        r.record_id = format!("sec_edgar:{ACCESSION}");
        assert!(problems(&r).iter().any(|p| p.starts_with("record_id")));
        let mut r = sec_record();
        r.content_hash = r.content_hash[..12].to_string();
        assert!(problems(&r).iter().any(|p| p.starts_with("content_hash")));
        // A record superseding itself.
        let mut r = sec_record();
        r.supersedes = Some(r.record_id.clone());
        assert!(problems(&r).iter().any(|p| p.starts_with("supersedes")));
        // An unparsed fact with `ok` status.
        let mut r = unparsed_record();
        r.parse = ParseStatus::Ok;
        r.parse_errors.clear();
        refused(r, "parse");
    }

    /// Critic U8: an item an earlier read had and a later read does not is a
    /// visible version of its own, never a dropped row.
    #[test]
    fn a_withdrawal_is_its_own_version() {
        let r = sec_record();
        let gone = Withdrawn {
            how: WithdrawnHow::Gone,
            http_status: Some(404),
            moved_to: None,
            reason: "index page returned 404".into(),
        };
        let w = r.withdrawal(
            gone.clone(),
            h("404 body"),
            r.observed_ms + 86_400_000,
            r.observed_ms + 86_400_500,
        );
        assert_eq!(w.validate(), Ok(()));
        assert_eq!(
            (w.source_id.as_str(), w.native_id.as_str()),
            (r.source_id.as_str(), r.native_id.as_str())
        );
        assert_eq!(
            (w.event_key.as_str(), w.published_ms),
            (r.event_key.as_str(), r.published_ms)
        );
        assert_ne!(w.record_id, r.record_id);
        assert_eq!(w.fact.kind(), "withdrawn");
        assert!(!w.fact.is_typed() && r.fact.is_typed());
        // Read again a day later: the same record.
        let again = r.withdrawal(
            gone,
            h("404 body, later"),
            w.observed_ms + 86_400_000,
            w.parsed_ms + 86_400_000,
        );
        assert_eq!(again.record_id, w.record_id);
        // The wire form, and its own refusals.
        let text = serde_json::to_string(&w).unwrap();
        assert!(
            text.contains(r#""fact":{"kind":"withdrawn","how":"gone","http_status":404"#),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<SourceRecord>(&text).unwrap(), w);
        let mut v = serde_json::to_value(&w).unwrap();
        v["fact"]["note"] = json!("x");
        assert!(serde_json::from_value::<SourceRecord>(v).is_err());
        let moved = |to: Option<&str>, how, status| Withdrawn {
            how,
            http_status: status,
            moved_to: to.map(String::from),
            reason: "redirected".into(),
        };
        let ok = r.withdrawal(
            moved(
                Some("https://www.sec.gov/new/place"),
                WithdrawnHow::Moved,
                Some(301),
            ),
            h("301 body"),
            r.observed_ms + 1,
            r.observed_ms + 2,
        );
        assert_eq!(ok.validate(), Ok(()));
        for bad in [
            moved(None, WithdrawnHow::Moved, Some(301)),
            moved(Some("https://x.example/y"), WithdrawnHow::Gone, Some(404)),
            moved(Some("ftp://x.example/y"), WithdrawnHow::Moved, None),
            moved(Some("https://x.example/y"), WithdrawnHow::Moved, Some(200)),
        ] {
            let w = r.withdrawal(bad.clone(), h("b"), r.observed_ms + 1, r.observed_ms + 2);
            assert!(
                problems(&w).iter().any(|p| p.starts_with("fact.")),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn trust_follows_the_source_class() {
        for class in SourceClass::ALL {
            assert!(class.allows(Trust::TriggerOnly), "{}", class.as_str());
        }
        assert!(!SourceClass::IndependentReporting.allows(Trust::Primary));
        assert!(SourceClass::IndependentReporting.allows(Trust::Corroborating));
        assert!(!SourceClass::SocialInference.allows(Trust::Corroborating));
        assert!(!SourceClass::SocialInference.allows(Trust::Primary));
        assert!(SourceClass::LawRegulator.allows(Trust::Primary));
        assert!(SourceClass::CustomerDemand.allows(Trust::Primary));
    }

    #[test]
    fn an_inference_validates_its_own_provenance() {
        let ok = Inference {
            text: "The buyer runs a yearly tender.".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            prompt_sha256: h("prompt"),
            generation: None,
            as_of_ms: 1_760_000_000_000,
        };
        assert_eq!(ok.validate(), Ok(()));
        let text = serde_json::to_string(&ok).unwrap();
        assert_eq!(serde_json::from_str::<Inference>(&text).unwrap(), ok);
        // Nothing turns it into a fact: its JSON is no fact either.
        assert!(serde_json::from_str::<Fact>(&text).is_err());
        let bad = Inference {
            text: " ".into(),
            model: String::new(),
            prompt_sha256: h("prompt")[..12].into(),
            generation: Some(String::new()),
            as_of_ms: 0,
        };
        assert_eq!(bad.validate().unwrap_err().len(), 4);
    }

    #[test]
    fn places_cpv_codes_and_parser_versions() {
        let place = |scheme, code: &str| Place {
            role: PlaceRole::Performance,
            scheme,
            code: code.into(),
        };
        for ok in ["DE", "DEU"] {
            assert!(place(PlaceScheme::Iso3166, ok).validate().is_ok(), "{ok}");
        }
        for ok in ["DE", "DE2", "DE21", "DE212", "UKZ"] {
            assert!(place(PlaceScheme::Nuts, ok).validate().is_ok(), "{ok}");
        }
        for bad in ["de", "D", "DEUT", ""] {
            assert!(
                place(PlaceScheme::Iso3166, bad).validate().is_err(),
                "{bad}"
            );
        }
        for bad in ["D", "de21", "DE2123", "1E2", "DE-1"] {
            assert!(place(PlaceScheme::Nuts, bad).validate().is_err(), "{bad}");
        }
        assert_eq!(
            split_parser_version("sec-submissions/12"),
            Some(("sec-submissions", 12))
        );
        for bad in [
            "sec", "/1", "sec/", "sec/0", "sec/01", "sec/+1", "Sec/1", "sec/1/2",
        ] {
            assert_eq!(split_parser_version(bad), None, "{bad}");
        }
        for ok in ["72000000", "48000000-8"] {
            assert!(valid_cpv(ok), "{ok}");
        }
        for bad in [
            "7200000",
            "720000000",
            "72000000-",
            "72000000-12",
            "7200000a",
            "-8",
        ] {
            assert!(!valid_cpv(bad), "{bad}");
        }
    }
}
