//! What every SOE record shares (`docs/soe-2026-10-08.md` § 5): the header,
//! the parse entry points, the problem list and the small closed values more
//! than one record uses. Pure: text in, record or problems out; the files are
//! read by `config/soe.rs`.
//!
//! | Rule | Value |
//! |---|---|
//! | Header | `schema = "soe.<record>/1"` — this record's (`schema_mismatch`); `id` a lineage id `^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$` (`invalid_id`); `version` ≥ 1 (`invalid_version`) |
//! | Shape | serde: an unknown key is refused at every level (`deny_unknown_fields`); a missing required field fails the parse (`invalid_record`) |
//! | Meaning | [`SoeRecord::problems`]: every problem listed, each `<code>: <field>: <why>` |
//! | Parse | [`from_toml`] / [`from_json`] = shape, then header, then meaning |
//!
//! | Value | Values |
//! |---|---|
//! | [`Tier`] | `LOW` `MEDIUM` `HIGH` `UNKNOWN` (defensibility, reversibility, evidence strength) |
//! | [`Verdict`] | `PASS` `HOLD` `REJECT` |

// Consumers land with the economics, gates, ranking and loader (O1 W3–W7).
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::value::{codes, Minor, SchemaTag, ValueError};
use crate::domain::evidence::valid_sha256;
use crate::domain::lineage::value::{valid_id, Time, TimeOrder, UNKNOWN};

/// A top-level SOE record (module table).
pub trait SoeRecord {
    /// `<record>` of `soe.<record>/1`.
    const RECORD: &'static str;
    fn schema(&self) -> &SchemaTag;
    fn id(&self) -> &str;
    fn version(&self) -> u32;
    /// The record's own rules, beyond the header.
    fn problems(&self, p: &mut Problems);
}

/// Header + the record's own rules; `Err` lists every problem.
pub fn validate<R: SoeRecord>(r: &R) -> Result<(), Vec<ValueError>> {
    let mut p = Problems::default();
    if let Err(e) = r.schema().require(R::RECORD) {
        p.0.push(ValueError::new(e.code, format!("schema: {}", e.message)));
    }
    p.id("id", r.id());
    p.check(
        r.version() >= 1,
        codes::INVALID_VERSION,
        "version",
        "must be ≥ 1",
    );
    r.problems(&mut p);
    p.into_result()
}

fn parse_error(e: impl fmt::Display) -> Vec<ValueError> {
    vec![ValueError::new(
        codes::INVALID_RECORD,
        e.to_string().trim_end().to_string(),
    )]
}

/// A record from TOML text (module table).
pub fn from_toml<R: SoeRecord + DeserializeOwned>(text: &str) -> Result<R, Vec<ValueError>> {
    let r: R = toml::from_str(text).map_err(parse_error)?;
    validate(&r)?;
    Ok(r)
}

/// A record from JSON text (module table).
pub fn from_json<R: SoeRecord + DeserializeOwned>(text: &str) -> Result<R, Vec<ValueError>> {
    let r: R = serde_json::from_str(text).map_err(parse_error)?;
    validate(&r)?;
    Ok(r)
}

/// The problems of one record, each `<code>: <field>: <why>`.
#[derive(Debug, Default)]
pub struct Problems(Vec<ValueError>);

impl Problems {
    pub fn push(&mut self, code: &'static str, field: &str, why: impl fmt::Display) {
        self.0
            .push(ValueError::new(code, format!("{field}: {why}")));
    }

    pub fn check(&mut self, ok: bool, code: &'static str, field: &str, why: impl fmt::Display) {
        if !ok {
            self.push(code, field, why);
        }
    }

    /// Non-empty after trimming.
    pub fn text(&mut self, field: &str, s: &str) {
        self.check(
            !s.trim().is_empty(),
            codes::INVALID_FIELD,
            field,
            "must not be empty",
        );
    }

    /// A lineage id (`valid_id`).
    pub fn id(&mut self, field: &str, s: &str) {
        self.check(
            valid_id(s),
            codes::INVALID_ID,
            field,
            format_args!("`{s}` is not an id (^[A-Za-z0-9][A-Za-z0-9._-]{{0,79}}$)"),
        );
    }

    /// 64 lowercase hex.
    pub fn sha256(&mut self, field: &str, s: &str) {
        self.check(
            valid_sha256(s),
            codes::INVALID_FIELD,
            field,
            format_args!("`{s}` is not a sha256 (64 lowercase hex)"),
        );
    }

    /// A known time.
    pub fn known(&mut self, field: &str, t: &Time) {
        self.check(
            t.is_known(),
            codes::INVALID_TIME,
            field,
            "must be a known time, not UNKNOWN",
        );
    }

    /// `≥ 0`.
    pub fn non_negative(&mut self, field: &str, m: Minor) {
        self.check(
            m.0 >= 0,
            codes::INVALID_FIELD,
            field,
            format_args!("{m} is negative"),
        );
    }

    /// Each entry non-empty, none twice.
    pub fn unique_texts<'a>(&mut self, field: &str, items: impl IntoIterator<Item = &'a str>) {
        let mut seen = BTreeSet::new();
        for (i, s) in items.into_iter().enumerate() {
            self.text(&format!("{field}[{i}]"), s);
            if !seen.insert(s) {
                self.push(codes::DUPLICATE, field, format_args!("`{s}` twice"));
            }
        }
    }

    /// No value twice.
    pub fn unique<T: Ord + fmt::Debug>(&mut self, field: &str, items: impl IntoIterator<Item = T>) {
        let mut seen = BTreeSet::new();
        for x in items {
            if seen.contains(&x) {
                self.push(codes::DUPLICATE, field, format_args!("{x:?} twice"));
            } else {
                seen.insert(x);
            }
        }
    }

    /// Refused when `t` is surely after `bound` (a day against an instant
    /// inside it is undecidable, so allowed; unknown is allowed).
    pub fn not_after(&mut self, field: &str, t: &Time, bound_field: &str, bound: &Time) {
        self.check(
            t.order(bound) != TimeOrder::After,
            codes::FUTURE_LEAKAGE,
            field,
            format_args!("{t} is after {bound_field} {bound}"),
        );
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_result(self) -> Result<(), Vec<ValueError>> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.0)
        }
    }
}

/// A stated text: not empty, not `UNKNOWN` (a downside, a reputation risk).
pub fn stated(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s != UNKNOWN
}

/// An ordinal level (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Tier {
    Low,
    Medium,
    High,
    Unknown,
}

impl Tier {
    /// `LOW` 1 … `HIGH` 3; `UNKNOWN` none (never ranks as a level).
    pub fn level(self) -> Option<u8> {
        match self {
            Tier::Low => Some(1),
            Tier::Medium => Some(2),
            Tier::High => Some(3),
            Tier::Unknown => None,
        }
    }
}

/// The outcome of the hard gates (module table; PRD § 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Hold,
    Reject,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        schema: SchemaTag,
        id: String,
        version: u32,
        note: String,
    }

    impl SoeRecord for Probe {
        const RECORD: &'static str = "probe";
        fn schema(&self) -> &SchemaTag {
            &self.schema
        }
        fn id(&self) -> &str {
            &self.id
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn problems(&self, p: &mut Problems) {
            p.text("note", &self.note);
        }
    }

    fn codes_of(r: Result<Probe, Vec<ValueError>>) -> Vec<&'static str> {
        r.unwrap_err().into_iter().map(|e| e.code).collect()
    }

    #[test]
    fn header_and_own_rules_are_all_listed() {
        let ok = "schema = \"soe.probe/1\"\nid = \"p-1\"\nversion = 1\nnote = \"x\"";
        assert_eq!(from_toml::<Probe>(ok).unwrap().id, "p-1");
        let bad = "schema = \"soe.opportunity/1\"\nid = \"-p\"\nversion = 0\nnote = \" \"";
        assert_eq!(
            codes_of(from_toml::<Probe>(bad)),
            vec![
                codes::SCHEMA_MISMATCH,
                codes::INVALID_ID,
                codes::INVALID_VERSION,
                codes::INVALID_FIELD
            ]
        );
        let e = from_toml::<Probe>(bad).unwrap_err();
        assert_eq!(e[3].to_string(), "invalid_field: note: must not be empty");
        // Shape errors: an unknown key, a missing field, another version.
        for text in [
            "schema = \"soe.probe/1\"\nid = \"p\"\nversion = 1\nnote = \"x\"\nextra = 1",
            "schema = \"soe.probe/1\"\nid = \"p\"\nversion = 1",
            "schema = \"soe.probe/2\"\nid = \"p\"\nversion = 1\nnote = \"x\"",
        ] {
            assert_eq!(
                codes_of(from_toml::<Probe>(text)),
                vec![codes::INVALID_RECORD]
            );
        }
        let j = r#"{"schema":"soe.probe/1","id":"p","version":1,"note":"x"}"#;
        assert!(from_json::<Probe>(j).is_ok());
    }

    #[test]
    fn helpers_name_the_field() {
        let mut p = Problems::default();
        p.unique_texts("langs", ["de", "en", "de", ""]);
        p.unique("keys", [1, 2, 1]);
        p.sha256("h", "abc");
        p.known("t", &Time::Unknown);
        p.non_negative("m", Minor(-1));
        let day: Time = "2026-10-02".parse().unwrap();
        let before: Time = "2026-10-01T12:00:00Z".parse().unwrap();
        p.not_after("a", &day, "b", &before);
        p.not_after("ok", &before, "b", &day);
        let msgs: Vec<String> = p
            .into_result()
            .unwrap_err()
            .iter()
            .map(|e| e.to_string())
            .collect();
        assert_eq!(
            msgs,
            vec![
                "duplicate: langs: `de` twice",
                "invalid_field: langs[3]: must not be empty",
                "duplicate: keys: 1 twice",
                "invalid_field: h: `abc` is not a sha256 (64 lowercase hex)",
                "invalid_time: t: must be a known time, not UNKNOWN",
                "invalid_field: m: -0.01 is negative",
                "future_leakage: a: 2026-10-02 is after b 2026-10-01T12:00:00Z",
            ]
        );
        assert!(stated("a reputational hit with one client"));
        assert!(!stated("UNKNOWN") && !stated(" "));
        assert_eq!(Tier::Unknown.level(), None);
        assert!(Tier::High.level() > Tier::Low.level());
    }
}
