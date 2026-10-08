//! Weekly portfolio and public brief (PRD § 6, § 8 step 7, § 10). The
//! portfolio is the week's allocation decision — ranked, held and rejected
//! candidates, each with an action; an empty ranked list is a valid `HOLD`
//! week. The public brief is the only shape that may leave the private state:
//! allow-listed keys, `url:` facts only. Types only: the builders are O1 W5 /
//! O3 `allocate`, the brief's sanitizer O7.
//!
//! | `WeeklyPortfolio` | Value |
//! |---|---|
//! | header | `schema = "soe.weekly_portfolio/1"`, `id`, `version` ≥ 1 |
//! | `week`, `as_of` | [`IsoWeek`] `YYYY-Www` · known time |
//! | `currency` | of every amount below (the profile's) |
//! | `profile_sha256`, `inputs_sha256`, `economics_version` | 64 hex each (in full) · ≥ 1 |
//! | `[[ranked]]` | `rank` (1, 2, … in order), `id`, `opportunity_version`, `action`, `[[ranked.keys]] key, value` (the rank inputs as text) |
//! | `[[held]]` · `[[rejected]]` | `id`, `opportunity_version`, `action`, `gates[]` (≥ 1 gate code) |
//! | `[allocation]` | `owner_hours`, `cash` ≥ 0 |
//! | `hold_rationale?` | required when nothing is ranked |
//! | `next_information[]` | what to learn next, no repeats |
//!
//! | [`PortfolioAction`] (`kind`) | Fields | Allowed in |
//! |---|---|---|
//! | `CHEAP_TEST` | `max_cash`, `max_hours` | ranked, held |
//! | `CONTINUE_ACTIVE` | — | ranked |
//! | `HOLD` | — | ranked (over budget), held |
//! | `DILIGENCE` | `questions[]` (≥ 1), `max_next_tranche` | held |
//! | `REPRICE` | `min_price` | rejected |
//! | `REJECT` | — | rejected |
//!
//! No `LAUNCH` action exists. An id appears once across the three lists.
//!
//! | `PublicBrief` | Value |
//! |---|---|
//! | header | `schema = "soe.public_brief/1"`, `id`, `version` ≥ 1 |
//! | keys | [`PUBLIC_BRIEF_KEYS`] only, at every depth |
//! | `[[facts]] text, url` | `url` a `url:https://…` locator — a `repo:` / `state:` / `vault:` / `run:` one is private (`private_locator`) |
//! | `thesis`, `disclosed_assumptions[]`, `experiment_update?`, `generated_from_sha256` | text · text · text · 64 hex |

// Consumers land with ranking and allocation (O1 W5, O3).
#![allow(dead_code)]

use std::fmt;
use std::str::FromStr;

use chrono::{NaiveDate, Weekday};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::profile::RankKey;
use super::record::{Problems, SoeRecord};
use super::value::{codes, Currency, Minor, SchemaTag, ValueError};
use crate::domain::lineage::value::{Locator, Time};

/// An ISO 8601 week: `2026-W41`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IsoWeek {
    year: i32,
    week: u32,
}

impl IsoWeek {
    pub fn new(year: i32, week: u32) -> Result<IsoWeek, ValueError> {
        if (1000..=9999).contains(&year)
            && NaiveDate::from_isoywd_opt(year, week, Weekday::Mon).is_some()
        {
            Ok(IsoWeek { year, week })
        } else {
            Err(ValueError::new(
                codes::INVALID_FIELD,
                format!("{year}-W{week:02} is not an ISO week"),
            ))
        }
    }

    pub fn year(&self) -> i32 {
        self.year
    }

    pub fn week(&self) -> u32 {
        self.week
    }

    /// Its Monday.
    pub fn monday(&self) -> NaiveDate {
        NaiveDate::from_isoywd_opt(self.year, self.week, Weekday::Mon).expect("checked in new")
    }
}

impl FromStr for IsoWeek {
    type Err = ValueError;
    fn from_str(s: &str) -> Result<Self, ValueError> {
        let bad = || ValueError::new(codes::INVALID_FIELD, format!("week `{s}`: `YYYY-Www`"));
        let (y, w) = s.split_once("-W").ok_or_else(bad)?;
        let digits = |t: &str, n: usize| t.len() == n && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(y, 4) || !digits(w, 2) {
            return Err(bad());
        }
        IsoWeek::new(y.parse().map_err(|_| bad())?, w.parse().map_err(|_| bad())?)
    }
}

impl fmt::Display for IsoWeek {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:04}-W{:02}", self.year, self.week)
    }
}

impl Serialize for IsoWeek {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IsoWeek {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What the week does with one candidate (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum PortfolioAction {
    CheapTest {
        max_cash: Minor,
        max_hours: u32,
    },
    ContinueActive,
    Hold,
    Diligence {
        questions: Vec<String>,
        max_next_tranche: Minor,
    },
    Reprice {
        min_price: Minor,
    },
    Reject,
}

impl PortfolioAction {
    pub fn kind(&self) -> &'static str {
        match self {
            PortfolioAction::CheapTest { .. } => "CHEAP_TEST",
            PortfolioAction::ContinueActive => "CONTINUE_ACTIVE",
            PortfolioAction::Hold => "HOLD",
            PortfolioAction::Diligence { .. } => "DILIGENCE",
            PortfolioAction::Reprice { .. } => "REPRICE",
            PortfolioAction::Reject => "REJECT",
        }
    }

    /// The lists it may appear in (module table).
    fn allowed_in(&self) -> &'static [&'static str] {
        match self {
            PortfolioAction::CheapTest { .. } => &["ranked", "held"],
            PortfolioAction::ContinueActive => &["ranked"],
            PortfolioAction::Hold => &["ranked", "held"],
            PortfolioAction::Diligence { .. } => &["held"],
            PortfolioAction::Reprice { .. } | PortfolioAction::Reject => &["rejected"],
        }
    }

    fn problems(&self, at: &str, p: &mut Problems) {
        match self {
            PortfolioAction::CheapTest { max_cash, .. } => {
                p.non_negative(&format!("{at}.max_cash"), *max_cash)
            }
            PortfolioAction::Diligence {
                questions,
                max_next_tranche,
            } => {
                p.check(
                    !questions.is_empty(),
                    codes::INVALID_FIELD,
                    &format!("{at}.questions"),
                    "at least one question",
                );
                p.unique_texts(
                    &format!("{at}.questions"),
                    questions.iter().map(String::as_str),
                );
                p.non_negative(&format!("{at}.max_next_tranche"), *max_next_tranche);
            }
            PortfolioAction::Reprice { min_price } => {
                p.non_negative(&format!("{at}.min_price"), *min_price)
            }
            _ => {}
        }
    }
}

/// One rank input, as text (`EVIDENCE_CONFIDENCE` = `HIGH`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankValue {
    pub key: RankKey,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankedRow {
    pub rank: u32,
    pub id: String,
    pub opportunity_version: u32,
    pub action: PortfolioAction,
    pub keys: Vec<RankValue>,
}

/// A held or rejected candidate and the gates that stopped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatedRow {
    pub id: String,
    pub opportunity_version: u32,
    pub action: PortfolioAction,
    pub gates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub owner_hours: u32,
    pub cash: Minor,
}

/// `soe.weekly_portfolio/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyPortfolio {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub week: IsoWeek,
    pub as_of: Time,
    pub currency: Currency,
    pub profile_sha256: String,
    pub inputs_sha256: String,
    pub economics_version: u32,
    pub ranked: Vec<RankedRow>,
    pub held: Vec<GatedRow>,
    pub rejected: Vec<GatedRow>,
    pub allocation: Allocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_rationale: Option<String>,
    pub next_information: Vec<String>,
}

impl WeeklyPortfolio {
    /// Nothing ranked: a `HOLD` week.
    pub fn is_hold(&self) -> bool {
        self.ranked.is_empty()
    }
}

impl SoeRecord for WeeklyPortfolio {
    const RECORD: &'static str = "weekly_portfolio";

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
        p.known("as_of", &self.as_of);
        p.sha256("profile_sha256", &self.profile_sha256);
        p.sha256("inputs_sha256", &self.inputs_sha256);
        p.check(
            self.economics_version >= 1,
            codes::INVALID_VERSION,
            "economics_version",
            "must be ≥ 1",
        );
        let all_ids = self
            .ranked
            .iter()
            .map(|r| r.id.as_str())
            .chain(self.held.iter().map(|r| r.id.as_str()))
            .chain(self.rejected.iter().map(|r| r.id.as_str()));
        p.unique("ranked + held + rejected", all_ids);
        for (i, r) in self.ranked.iter().enumerate() {
            let f = |x: &str| format!("ranked[{i}].{x}");
            p.id(&f("id"), &r.id);
            p.check(
                r.rank as usize == i + 1,
                codes::INVALID_FIELD,
                &f("rank"),
                format_args!("{} — ranks run 1, 2, … in list order", r.rank),
            );
            row_action(p, "ranked", &f("action"), &r.action);
            p.unique(&f("keys"), r.keys.iter().map(|k| k.key));
        }
        for (list, rows) in [("held", &self.held), ("rejected", &self.rejected)] {
            for (i, r) in rows.iter().enumerate() {
                let f = |x: &str| format!("{list}[{i}].{x}");
                p.id(&f("id"), &r.id);
                row_action(p, list, &f("action"), &r.action);
                p.check(
                    !r.gates.is_empty(),
                    codes::INVALID_FIELD,
                    &f("gates"),
                    "names the gates that stopped it",
                );
                p.unique_texts(&f("gates"), r.gates.iter().map(String::as_str));
            }
        }
        p.non_negative("allocation.cash", self.allocation.cash);
        match &self.hold_rationale {
            Some(h) => p.text("hold_rationale", h),
            None => p.check(
                !self.ranked.is_empty(),
                codes::INVALID_FIELD,
                "hold_rationale",
                "a week with nothing ranked says why it holds",
            ),
        }
        p.unique_texts(
            "next_information",
            self.next_information.iter().map(String::as_str),
        );
    }
}

fn row_action(p: &mut Problems, list: &str, field: &str, a: &PortfolioAction) {
    p.check(
        a.allowed_in().contains(&list),
        codes::INVALID_FIELD,
        field,
        format_args!("{} is not allowed in {list}", a.kind()),
    );
    a.problems(field, p);
}

/// Every key a serialized [`PublicBrief`] may carry, at any depth.
pub const PUBLIC_BRIEF_KEYS: [&str; 11] = [
    "schema",
    "id",
    "version",
    "week",
    "facts",
    "text",
    "url",
    "thesis",
    "disclosed_assumptions",
    "experiment_update",
    "generated_from_sha256",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BriefFact {
    pub text: String,
    /// `url:https://…` only (module table).
    pub url: Locator,
}

/// `soe.public_brief/1` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicBrief {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub week: IsoWeek,
    pub facts: Vec<BriefFact>,
    pub thesis: String,
    pub disclosed_assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_update: Option<String>,
    pub generated_from_sha256: String,
}

impl SoeRecord for PublicBrief {
    const RECORD: &'static str = "public_brief";

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
        for (i, fact) in self.facts.iter().enumerate() {
            p.text(&format!("facts[{i}].text"), &fact.text);
            p.check(
                matches!(fact.url, Locator::Url(_)),
                codes::PRIVATE_LOCATOR,
                &format!("facts[{i}].url"),
                format_args!("`{}` — a public brief cites url:https://… only", fact.url),
            );
        }
        p.text("thesis", &self.thesis);
        p.unique_texts(
            "disclosed_assumptions",
            self.disclosed_assumptions.iter().map(String::as_str),
        );
        if let Some(u) = &self.experiment_update {
            p.text("experiment_update", u);
        }
        p.sha256("generated_from_sha256", &self.generated_from_sha256);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;
    use crate::domain::soe::record::{from_json, from_toml, validate};

    const H: &str = "0000000000000000000000000000000000000000000000000000000000000003";

    fn portfolio() -> WeeklyPortfolio {
        let text = format!(
            r#"
schema = "soe.weekly_portfolio/1"
id = "2026-W41"
version = 1
week = "2026-W41"
as_of = "2026-10-05T18:00:00Z"
currency = "EUR"
profile_sha256 = "{H}"
inputs_sha256 = "{H}"
economics_version = 1
next_information = ["economics.revenue.price_per_month"]

[[ranked]]
rank = 1
id = "example-automation"
opportunity_version = 1
action = {{ kind = "CHEAP_TEST", max_cash = "250.00", max_hours = 7 }}
keys = [{{ key = "EVIDENCE_CONFIDENCE", value = "HIGH" }}]

[[held]]
id = "example-acquisition"
opportunity_version = 2
action = {{ kind = "DILIGENCE", questions = ["who holds the domain"], max_next_tranche = "500.00" }}
gates = ["LEGAL_UNRESOLVED"]

[[rejected]]
id = "example-delivery"
opportunity_version = 1
action = {{ kind = "REJECT" }}
gates = ["CONTRIBUTION_BELOW_TARGET"]

[allocation]
owner_hours = 7
cash = "250.00"
"#
        );
        from_toml(&text).unwrap()
    }

    fn codes_of(r: Result<(), Vec<ValueError>>) -> Vec<&'static str> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.code)
            .collect()
    }

    #[test]
    fn iso_weeks_parse_strictly() {
        let w: IsoWeek = "2026-W41".parse().unwrap();
        assert_eq!(
            (w.year(), w.week(), w.to_string()),
            (2026, 41, "2026-W41".into())
        );
        assert_eq!(w.monday().to_string(), "2026-10-05");
        assert!("2026-W53".parse::<IsoWeek>().is_ok()); // 2026 has 53 ISO weeks
        for bad in [
            "2027-W53",
            "2026-W00",
            "2026-W5",
            "2026W41",
            "26-W41",
            "2026-w41",
            "2026-W41 ",
        ] {
            assert!(bad.parse::<IsoWeek>().is_err(), "{bad}");
        }
    }

    #[test]
    fn portfolio_rows_actions_and_hold_week() {
        let p = portfolio();
        assert!(!p.is_hold());
        let back: WeeklyPortfolio = from_toml(&toml::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(from_json::<WeeklyPortfolio>(&j).unwrap(), p);
        assert_eq!(
            serde_json::to_value(&p.rejected[0].action).unwrap(),
            serde_json::json!({"kind": "REJECT"})
        );

        // A HOLD week: nothing ranked, so it says why.
        let mut hold = p.clone();
        hold.ranked.clear();
        assert_eq!(codes_of(validate(&hold)), vec!["invalid_field"]);
        hold.hold_rationale = Some("no candidate passed the gates".into());
        assert!(validate(&hold).is_ok() && hold.is_hold());

        // Wrong rank, an action outside its list, a repeated id, no gates.
        let mut bad = p.clone();
        bad.ranked[0].rank = 2;
        bad.ranked[0].action = PortfolioAction::Reject;
        bad.held[0].id = "example-delivery".into();
        bad.rejected[0].gates.clear();
        assert_eq!(
            codes_of(validate(&bad)),
            vec![
                "duplicate",
                "invalid_field",
                "invalid_field",
                "invalid_field"
            ]
        );
        // No LAUNCH action exists.
        let launch = toml::to_string(&p)
            .unwrap()
            .replace("kind = \"CHEAP_TEST\"", "kind = \"LAUNCH\"");
        assert!(from_toml::<WeeklyPortfolio>(&launch).is_err());
    }

    fn keys(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    out.insert(k.clone());
                    keys(x, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
            _ => {}
        }
    }

    #[test]
    fn public_brief_serializes_only_allow_listed_keys() {
        let text = format!(
            r#"
schema = "soe.public_brief/1"
id = "brief-2026-W41"
version = 1
week = "2026-W41"
thesis = "a filing-format change creates paid automation work"
disclosed_assumptions = ["conversion 10–25 % (example)"]
experiment_update = "two interviews done"
generated_from_sha256 = "{H}"

[[facts]]
text = "the regulator published the new format"
url = "url:https://example.org/notice"
"#
        );
        let b: PublicBrief = from_toml(&text).unwrap();
        let mut seen = BTreeSet::new();
        keys(&serde_json::to_value(&b).unwrap(), &mut seen);
        let allowed: BTreeSet<String> = PUBLIC_BRIEF_KEYS.iter().map(|k| k.to_string()).collect();
        assert!(seen.is_subset(&allowed), "{:?}", seen.difference(&allowed));
        assert_eq!(
            seen, allowed,
            "every allow-listed key is used by this example"
        );

        // A private locator and an extra key are refused.
        let private = text.replace("url:https://example.org/notice", "state:soe/cycles/x.json");
        let e = from_toml::<PublicBrief>(&private).unwrap_err();
        assert_eq!(e[0].code, codes::PRIVATE_LOCATOR, "{e:?}");
        let extra = text.replace("thesis =", "deal_terms = \"x\"\nthesis =");
        assert!(from_toml::<PublicBrief>(&extra).is_err());
    }
}
