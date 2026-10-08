//! Source-record fixtures for the as-of, packet and split-world tests
//! (compiled for tests only). Synthetic: round ids, amounts and names — no
//! real filing, notice or listing.
//!
//! | World | Holds |
//! |---|---|
//! | `sec` | an immutable filing source over 7 days: 24 filings for two CIKs read 5 min, 2 h or 9 h after publication; amendments (their own accession, `supersedes` the original, same event); wire copies (`origin` = the filing); an independent outlet that disagrees once; social posts; reparses 3 days later; a filing that failed to parse and was fixed by a reparse of its bytes; a withdrawal; daily coverage per CIK with failed fetches; a raw purge |
//! | `ted` | an immutable procurement source over 12 days: four procedures — notices with one or two lots, one procedure with a notice per lot, a duplicate notice that disagrees, corrigenda (`supersedes`), awards after the deadline, a notice in force only from a later date, a buyer without a jurisdiction; three demand posts; daily coverage with an uncovered day and a failed fetch |
//! | `in_place` | an in-place registry listing (max age 2 days) over 8 days: four items re-read every 12 h, edited every 3 days, one gone, one moved; coverage with a 3-day hole (stale listings) |

use std::collections::BTreeMap;

use super::asof::{AsOfInput, AsOfMode, Coverage, Purge};
use super::packet::{AsOfQuery, EvidencePacket};
use super::record::{
    sec_cik_entity, sec_filing_key, split_parser_version, ted_buyer_entity, ted_procedure_key,
    AccessMethod, Fact, NativeAmount, Origin, ParseError, ParseStatus, Place, PlaceRole,
    PlaceScheme, SecFiling, SourceClass, SourceRecord, TedNotice, Trust, Withdrawn, WithdrawnHow,
    RECORD_SCHEMA,
};
use super::rules::{Revision, SourcePolicy};
use crate::domain::canonical::sha256_hex;

pub(crate) const H: i64 = 3_600_000;
pub(crate) const D: i64 = 24 * H;
/// 2026-01-05T00:00:00Z.
pub(crate) const T0: i64 = 1_767_571_200_000;
const MIN: i64 = 60_000;

const CIKS: [&str; 2] = ["0000000001", "0000000002"];
const PROC: &str = "00000000-0000-4000-8000-000000000000";

/// The shape of a fixture source's facts and keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `sec_filing`, `sec:filing:<native>`.
    Sec,
    /// `ted_notice`, `ted:procedure:<PROC>`.
    Ted,
    /// A demand post (`ted_notice` shape, type `forum-post`), `forum:post:<native>`.
    Post,
    /// A registry listing page (`sec_filing` shape, form `listing`), `repo:item:<native>`.
    Listing,
}

/// A fixture source = one registry row.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Src {
    pub id: &'static str,
    pub class: SourceClass,
    pub trust: Trust,
    pub revision: Revision,
    pub kind: Kind,
}

impl Src {
    pub const SEC: Src = Src {
        id: "sec_edgar",
        class: SourceClass::CompanyPrimary,
        trust: Trust::Primary,
        revision: Revision::Immutable,
        kind: Kind::Sec,
    };
    pub const TED: Src = Src {
        id: "ted_search",
        class: SourceClass::LawRegulator,
        trust: Trust::Primary,
        revision: Revision::Immutable,
        kind: Kind::Ted,
    };
    /// Copies SEC filings (`origin` set).
    pub const WIRE: Src = Src {
        id: "news_wire",
        class: SourceClass::IndependentReporting,
        trust: Trust::Corroborating,
        revision: Revision::InPlace,
        kind: Kind::Sec,
    };
    /// Reports filings on its own (no origin).
    pub const DAILY: Src = Src {
        id: "trade_daily",
        class: SourceClass::IndependentReporting,
        trust: Trust::Corroborating,
        revision: Revision::InPlace,
        kind: Kind::Sec,
    };
    pub const SOCIAL: Src = Src {
        id: "social",
        class: SourceClass::SocialInference,
        trust: Trust::TriggerOnly,
        revision: Revision::InPlace,
        kind: Kind::Sec,
    };
    pub const FORUM: Src = Src {
        id: "buyer_forum",
        class: SourceClass::CustomerDemand,
        trust: Trust::Corroborating,
        revision: Revision::InPlace,
        kind: Kind::Post,
    };
    pub const REPO: Src = Src {
        id: "repo_listing",
        class: SourceClass::RegistryMarketplace,
        trust: Trust::Primary,
        revision: Revision::InPlace,
        kind: Kind::Listing,
    };

    pub fn policy(&self) -> SourcePolicy {
        SourcePolicy {
            revision: self.revision,
            listing_max_age_ms: (self.class == SourceClass::RegistryMarketplace).then_some(2 * D),
        }
    }
}

pub(crate) fn policies(srcs: &[Src]) -> BTreeMap<String, SourcePolicy> {
    srcs.iter()
        .map(|s| (s.id.to_string(), s.policy()))
        .collect()
}

/// The raw body sha256 of one read.
pub(crate) fn snap(source: &str, native: &str, observed: i64, tag: &str) -> String {
    sha256_hex(&format!("{source}:{native}:{observed}:{tag}"))
}

/// A first read of `native` (module defaults per [`Kind`]), identity set.
pub(crate) fn rec(
    src: Src,
    native: &str,
    published: i64,
    observed: i64,
    parsed: i64,
) -> SourceRecord {
    let ted = |notice_type: &str,
               procedure: &str,
               lots: Vec<String>,
               buyer: Option<&str>,
               value: Option<NativeAmount>| {
        Fact::TedNotice(TedNotice {
            notice_type: notice_type.into(),
            procedure_id: Some(procedure.into()),
            lot_ids: lots,
            buyer_name: buyer.map(String::from),
            places: vec![Place {
                role: PlaceRole::Buyer,
                scheme: PlaceScheme::Iso3166,
                code: "DEU".into(),
            }],
            cpv: vec!["72000000".into()],
            deadline_ms: None,
            value,
        })
    };
    let (event_key, entities, fact, jurisdiction, language, access_method) = match src.kind {
        Kind::Sec => (
            sec_filing_key(native),
            vec![sec_cik_entity(CIKS[0])],
            Fact::SecFiling(SecFiling {
                cik: CIKS[0].into(),
                form: "8-K".into(),
                items: vec!["8.01".into()],
                title: "Other events".into(),
            }),
            "US",
            "en",
            AccessMethod::HttpGet,
        ),
        Kind::Ted => (
            ted_procedure_key(PROC),
            vec![ted_buyer_entity("DEU", "buyer-0001")],
            ted(
                "cn-standard",
                PROC,
                vec!["LOT-0001".into()],
                Some("Example Buyer"),
                Some(NativeAmount::new("100000.00", "EUR")),
            ),
            "EU",
            "de",
            AccessMethod::HttpPost,
        ),
        Kind::Post => (
            format!("forum:post:{native}"),
            vec!["forum:board:it".to_string()],
            ted("forum-post", native, Vec::new(), None, None),
            "EU",
            "en",
            AccessMethod::HttpGet,
        ),
        Kind::Listing => (
            format!("repo:item:{native}"),
            vec!["repo:owner:example".to_string()],
            Fact::SecFiling(SecFiling {
                cik: "0000000009".into(),
                form: "listing".into(),
                items: Vec::new(),
                title: "release r0".into(),
            }),
            "US",
            "en",
            AccessMethod::HttpGet,
        ),
    };
    let currency = fact.currency().map(String::from);
    SourceRecord {
        schema: RECORD_SCHEMA.into(),
        record_id: String::new(),
        source_id: src.id.into(),
        source_class: src.class,
        trust: src.trust,
        native_id: native.into(),
        event_key,
        entities,
        url: format!("https://example.org/{}/{native}", src.id),
        published_ms: published,
        observed_ms: observed,
        parsed_ms: parsed,
        valid_from_ms: published,
        valid_until_ms: None,
        jurisdiction: jurisdiction.into(),
        language: language.into(),
        currency,
        content_hash: String::new(),
        snapshots: vec![snap(src.id, native, observed, "first")],
        parser_version: match src.kind {
            Kind::Ted | Kind::Post => "ted-search/1".into(),
            Kind::Sec | Kind::Listing => "sec-submissions/1".into(),
        },
        license_or_terms: "synthetic test terms".into(),
        terms_sha256: sha256_hex("synthetic terms page"),
        access_method,
        parse: ParseStatus::Ok,
        parse_errors: Vec::new(),
        fact,
        supersedes: None,
        origin: None,
    }
    .with_identity()
}

pub(crate) fn sec_mut(r: &mut SourceRecord) -> &mut SecFiling {
    match &mut r.fact {
        Fact::SecFiling(f) => f,
        other => panic!("sec_filing expected, got {}", other.kind()),
    }
}

pub(crate) fn ted_mut(r: &mut SourceRecord) -> &mut TedNotice {
    match &mut r.fact {
        Fact::TedNotice(n) => n,
        other => panic!("ted_notice expected, got {}", other.kind()),
    }
}

/// Changes what a fact says: a SEC-shaped title, a TED-shaped buyer name;
/// an unparsed / withdrawn reason.
pub(crate) fn retext(r: &mut SourceRecord, text: &str) {
    match &mut r.fact {
        Fact::SecFiling(f) => f.title = text.into(),
        Fact::TedNotice(n) => n.buyer_name = Some(text.into()),
        Fact::Unparsed { reason } => *reason = text.into(),
        Fact::Withdrawn(w) => w.reason = text.into(),
    }
}

/// The text [`retext`] sets.
fn text_of(r: &SourceRecord) -> String {
    match &r.fact {
        Fact::SecFiling(f) => f.title.clone(),
        Fact::TedNotice(n) => n.buyer_name.clone().unwrap_or_default(),
        Fact::Unparsed { reason } => reason.clone(),
        Fact::Withdrawn(w) => w.reason.clone(),
    }
}

/// A later read of the same item with new bytes and new content (an edit).
pub(crate) fn edited(r: &SourceRecord, observed: i64, text: &str) -> SourceRecord {
    let mut e = r.clone();
    retext(&mut e, text);
    e.observed_ms = observed;
    e.parsed_ms = observed + 1_000;
    e.snapshots = vec![snap(&r.source_id, &r.native_id, observed, text)];
    e.with_identity()
}

/// The same bytes parsed again by the next parser version, `parsed` later,
/// reading a little more (content differs).
pub(crate) fn reparsed(r: &SourceRecord, parsed: i64) -> SourceRecord {
    let mut e = r.clone();
    let (name, n) = split_parser_version(&r.parser_version).expect("parser version");
    e.parser_version = format!("{name}/{}", n + 1);
    e.parsed_ms = parsed;
    let text = format!("{} [parser {}]", text_of(r), n + 1);
    retext(&mut e, &text);
    e.with_identity()
}

/// A correction of `of`: a new item of the same source and event, naming
/// `of` in `supersedes`; `tweak` sets what it corrects.
pub(crate) fn correction_of(
    of: &SourceRecord,
    native: &str,
    published: i64,
    observed: i64,
    tweak: impl FnOnce(&mut SourceRecord),
) -> SourceRecord {
    let mut c = of.clone();
    c.native_id = native.into();
    c.url = format!("https://example.org/{}/{native}", of.source_id);
    c.published_ms = published;
    c.observed_ms = observed;
    c.parsed_ms = observed;
    c.valid_from_ms = published.max(of.valid_from_ms);
    c.snapshots = vec![snap(&of.source_id, native, observed, "correction")];
    c.supersedes = Some(of.record_id.clone());
    c.origin = None;
    tweak(&mut c);
    c.currency = c.fact.currency().map(String::from);
    if let Some(u) = c.valid_until_ms.filter(|u| *u <= c.valid_from_ms) {
        c.valid_until_ms = Some(u.max(c.valid_from_ms + 1));
    }
    c.with_identity()
}

/// A copy of `of` by another source (`origin` = `of`).
pub(crate) fn copy_of(
    of: &SourceRecord,
    src: Src,
    native: &str,
    published: i64,
    observed: i64,
) -> SourceRecord {
    let mut c = of.clone();
    c.source_id = src.id.into();
    c.source_class = src.class;
    c.trust = src.trust;
    c.native_id = native.into();
    c.url = format!("https://example.org/{}/{native}", src.id);
    c.published_ms = published;
    c.observed_ms = observed;
    c.parsed_ms = observed;
    c.valid_from_ms = published;
    if let Some(u) = c.valid_until_ms.filter(|u| *u <= published) {
        c.valid_until_ms = Some(u.max(published + 1));
    }
    c.snapshots = vec![snap(src.id, native, observed, "copy")];
    c.supersedes = None;
    c.origin = Some(Origin {
        source_id: of.source_id.clone(),
        native_id: Some(of.native_id.clone()),
    });
    c.with_identity()
}

/// `r`'s bytes that failed to parse: the same item and read, fact `unparsed`.
pub(crate) fn unparsed_of(r: &SourceRecord, reason: &str) -> SourceRecord {
    let mut u = r.clone();
    u.parse = ParseStatus::Error;
    u.parse_errors = vec![ParseError::new("acceptanceDateTime", "not a date")];
    u.fact = Fact::Unparsed {
        reason: reason.into(),
    };
    u.currency = None;
    u.with_identity()
}

pub(crate) fn coverage(
    src: Src,
    query: &str,
    fetched: i64,
    from: i64,
    to: i64,
    complete: bool,
) -> Coverage {
    Coverage {
        source_id: src.id.into(),
        query_key: query.into(),
        fetched_ms: fetched,
        from_ms: from,
        to_ms: to,
        complete,
        error_class: (!complete).then(|| "rate_limited".to_string()),
    }
}

/// `r` read again at `observed`: gone (404) or moved (301).
pub(crate) fn gone(r: &SourceRecord, observed: i64, how: WithdrawnHow) -> SourceRecord {
    let w = Withdrawn {
        how,
        http_status: Some(if how == WithdrawnHow::Gone { 404 } else { 301 }),
        moved_to: (how == WithdrawnHow::Moved).then(|| format!("{}/moved", r.url)),
        reason: format!(
            "read returned {}",
            if how == WithdrawnHow::Gone { 404 } else { 301 }
        ),
    };
    r.withdrawal(
        w,
        snap(&r.source_id, &r.native_id, observed, "withdrawn"),
        observed,
        observed + 1_000,
    )
}

/// One world of the split-world checks (module table).
#[derive(Debug, Clone)]
pub(crate) struct World {
    pub name: &'static str,
    pub records: Vec<SourceRecord>,
    pub coverage: Vec<Coverage>,
    pub purges: Vec<Purge>,
    pub policies: BTreeMap<String, SourcePolicy>,
    /// The instants checked lie inside.
    pub span: (i64, i64),
}

impl World {
    pub fn input(&self) -> AsOfInput<'_> {
        AsOfInput {
            records: &self.records,
            coverage: &self.coverage,
            purges: &self.purges,
            policies: &self.policies,
        }
    }

    pub fn packet(&self, t: i64, mode: AsOfMode) -> EvidencePacket {
        EvidencePacket::build(&self.input(), t, mode, &AsOfQuery::default())
    }

    /// Every record must be well-formed, and no correction may name a
    /// record outside the world.
    fn checked(self) -> Self {
        for r in &self.records {
            assert_eq!(r.validate(), Ok(()), "{} {}", self.name, r.record_id);
        }
        let ids: std::collections::BTreeSet<&str> =
            self.records.iter().map(|r| r.record_id.as_str()).collect();
        for r in &self.records {
            if let Some(s) = &r.supersedes {
                assert!(
                    ids.contains(s.as_str()),
                    "{}: {} names {s}",
                    self.name,
                    r.record_id
                );
            }
        }
        self
    }
}

pub(crate) fn worlds() -> Vec<World> {
    vec![sec_world(), ted_world(), in_place_world()]
}

/// The `sec` world (module table).
pub(crate) fn sec_world() -> World {
    let mut out = Vec::new();
    for i in 0..24i64 {
        let cik = CIKS[(i % 2) as usize];
        let accession = format!("{cik}-26-{:06}", i + 1);
        let published = T0 + i * 7 * H + (i % 3) * 17 * MIN;
        let lag = [5 * MIN, 2 * H, 9 * H][(i % 3) as usize];
        let mut f = rec(
            Src::SEC,
            &accession,
            published,
            published + lag,
            published + lag + 1_000,
        );
        f.entities = vec![sec_cik_entity(cik)];
        let items: Vec<String> = [vec!["2.02", "9.01"], vec!["8.01"], vec!["5.02"]]
            [(i % 3) as usize]
            .iter()
            .map(|x| x.to_string())
            .collect();
        let s = sec_mut(&mut f);
        s.cik = cik.into();
        s.items = items.clone();
        s.title = format!("Filing {} of CIK {cik}", i + 1);
        let f = f.with_identity();
        if i == 11 {
            // First read failed to parse; a reparse of the same bytes fixed it.
            out.push(unparsed_of(&f, "row 3: bad acceptanceDateTime"));
            let mut fixed = f.clone();
            fixed.parser_version = "sec-submissions/2".into();
            fixed.parsed_ms = f.observed_ms + D;
            out.push(fixed.with_identity());
        } else {
            out.push(f.clone());
        }
        if i % 5 == 0 {
            let amended = correction_of(
                &f,
                &format!("{cik}-26-{:06}", i + 101),
                published + 30 * H,
                published + 30 * H + 5 * MIN,
                |r| {
                    let s = sec_mut(r);
                    s.form = "8-K/A".into();
                    s.title = format!("Amended filing {} of CIK {cik}", i + 1);
                },
            );
            out.push(amended);
        }
        if i % 4 == 0 {
            out.push(copy_of(
                &f,
                Src::WIRE,
                &format!("wire-{i}"),
                published + 20 * MIN,
                published + 30 * MIN,
            ));
        }
        if i % 8 == 0 {
            let mut daily = rec(
                Src::DAILY,
                &format!("daily-{i}"),
                published + 3 * H,
                published + 3 * H,
                published + 3 * H,
            );
            daily.event_key = f.event_key.clone();
            daily.entities = f.entities.clone();
            let s = sec_mut(&mut daily);
            s.cik = cik.into();
            s.form = if i == 8 { "10-Q".into() } else { "8-K".into() };
            s.items = items.clone();
            s.title = format!("Outlet report on filing {}", i + 1);
            out.push(daily.with_identity());
        }
        if i % 6 == 3 {
            let mut post = rec(
                Src::SOCIAL,
                &format!("post-{i}"),
                published + H,
                published + H,
                published + H,
            );
            post.event_key = f.event_key.clone();
            out.push(post.with_identity());
        }
        if i % 6 == 0 && i != 0 {
            out.push(reparsed(&f, f.parsed_ms + 3 * D));
        }
        if i == 7 {
            out.push(gone(&f, f.observed_ms + 2 * D, WithdrawnHow::Gone));
        }
    }
    // Two social posts about rumoured events no filing confirms.
    for k in 0..2 {
        let at = T0 + (2 + 3 * k) * D;
        out.push(rec(
            Src::SOCIAL,
            &format!("rumour-{k}"),
            at,
            at + 10 * MIN,
            at + 10 * MIN,
        ));
    }
    let mut coverage = Vec::new();
    for cik in CIKS {
        for d in 0..8 {
            let fetched = T0 + d * D + 6 * H + if cik == CIKS[1] { 30 * MIN } else { 0 };
            coverage.push(self::coverage(
                Src::SEC,
                &format!("cik:{cik}"),
                fetched,
                T0 - 30 * D,
                fetched,
                d % 4 != 3,
            ));
        }
    }
    World {
        name: "sec",
        records: out,
        coverage,
        purges: vec![Purge {
            source_id: Src::SEC.id.into(),
            purged_ms: T0 + 5 * D,
            raw_before_ms: Some(T0 + 2 * D),
            records_before_ms: None,
            snapshots: 12,
            records: 0,
            reason: "raw_retention_days".into(),
        }],
        policies: policies(&[Src::SEC, Src::WIRE, Src::DAILY, Src::SOCIAL]),
        span: (T0 - H, T0 + 8 * D),
    }
    .checked()
}

/// The `ted` world (module table).
pub(crate) fn ted_world() -> World {
    let notice = |native: &str, p: i64, published: i64, lots: &[&str], amount: &str| {
        let lag = [2 * H, 30 * MIN, 7 * H, 3 * H][p as usize];
        let mut r = rec(
            Src::TED,
            native,
            published,
            published + lag,
            published + lag + 1_000,
        );
        let procedure = format!("00000000-0000-4000-8000-00000000000{p}");
        r.event_key = ted_procedure_key(&procedure);
        let country = ["DEU", "FRA", "POL", "AUT"][p as usize];
        r.entities = vec![ted_buyer_entity(country, &format!("buyer-000{p}"))];
        let deadline = published + 5 * D;
        r.valid_until_ms = Some(deadline);
        let n = ted_mut(&mut r);
        n.procedure_id = Some(procedure);
        n.lot_ids = lots.iter().map(|l| l.to_string()).collect();
        n.places[0].code = country.into();
        n.deadline_ms = Some(deadline);
        n.value = Some(NativeAmount::new(amount, "EUR"));
        n.buyer_name = Some(format!("Example buyer {p}"));
        r.with_identity()
    };
    let mut out = Vec::new();
    for p in 0..4i64 {
        let published = T0 + p * D + 9 * H;
        let lots: &[&str] = if p == 0 {
            &["LOT-0001", "LOT-0002"]
        } else {
            &["LOT-0001"]
        };
        let mut cn = notice(
            &format!("{}-2026", 100 + p),
            p,
            published,
            lots,
            "100000.00",
        );
        if p == 3 {
            cn.jurisdiction = "unknown".into();
            cn = cn.with_identity();
        }
        out.push(cn.clone());
        if p == 2 {
            // A second lot, its own notice, in force only from 3 days on.
            let mut b = notice(
                &format!("{}-2026", 200 + p),
                p,
                published + H,
                &["LOT-0002"],
                "250000.00",
            );
            b.valid_from_ms = published + 3 * D;
            out.push(b.with_identity());
        }
        if p == 1 {
            // A duplicate notice of the same lot at another amount.
            out.push(notice(
                &format!("{}-2026", 500 + p),
                p,
                published + H,
                &["LOT-0001"],
                "100500.00",
            ));
        }
        if p <= 1 {
            let fix = correction_of(
                &cn,
                &format!("{}-2026", 300 + p),
                published + 2 * D,
                published + 2 * D + H,
                |r| {
                    let n = ted_mut(r);
                    n.value = Some(NativeAmount::new("120000.00", "EUR"));
                    let later = n.deadline_ms.expect("deadline") + 2 * D;
                    n.deadline_ms = Some(later);
                    r.valid_until_ms = Some(later);
                },
            );
            out.push(fix);
        }
        if p % 2 == 1 {
            let at = published + 6 * D;
            let mut award = notice(
                &format!("{}-2026", 400 + p),
                p,
                at,
                &["LOT-0001"],
                "95000.00",
            );
            ted_mut(&mut award).notice_type = "can-standard".into();
            ted_mut(&mut award).deadline_ms = None;
            award.valid_until_ms = None;
            out.push(award.with_identity());
        }
    }
    for (k, day) in [1i64, 3, 6].into_iter().enumerate() {
        let at = T0 + day * D + 14 * H;
        out.push(rec(
            Src::FORUM,
            &format!("post-{k}"),
            at,
            at + 20 * MIN,
            at + 20 * MIN,
        ));
    }
    let mut coverage = Vec::new();
    for d in 0..12 {
        if d == 4 {
            continue; // an uncovered day
        }
        let fetched = T0 + (d + 1) * D + H;
        coverage.push(self::coverage(
            Src::TED,
            "cpv:72000000",
            fetched,
            T0 + d * D,
            T0 + (d + 1) * D - 1,
            d != 6,
        ));
    }
    for d in 0..12 {
        let fetched = T0 + d * D + 23 * H;
        coverage.push(self::coverage(
            Src::FORUM,
            "board:it",
            fetched,
            T0 - D,
            fetched,
            true,
        ));
    }
    World {
        name: "ted",
        records: out,
        coverage,
        purges: vec![Purge {
            source_id: Src::TED.id.into(),
            purged_ms: T0 + 8 * D,
            raw_before_ms: Some(T0 + D),
            records_before_ms: None,
            snapshots: 4,
            records: 0,
            reason: "raw_retention_days".into(),
        }],
        policies: policies(&[Src::TED, Src::FORUM]),
        span: (T0 - H, T0 + 12 * D),
    }
    .checked()
}

/// The `in_place` world (module table).
pub(crate) fn in_place_world() -> World {
    let mut out = Vec::new();
    let mut coverage = Vec::new();
    for k in 0..4i64 {
        let published = T0 + k * 5 * H;
        let first = rec(
            Src::REPO,
            &format!("repo-{k}"),
            published,
            published + k * H,
            published + k * H + 1_000,
        );
        let mut last = first.clone();
        out.push(first);
        for n in 1..16i64 {
            let read = published + n * 12 * H;
            if k == 2 && n == 10 {
                out.push(gone(&last, read, WithdrawnHow::Gone));
                break;
            }
            if k == 3 && n == 12 {
                out.push(gone(&last, read, WithdrawnHow::Moved));
                break;
            }
            if n % 6 == 0 {
                // An edit: the listing changed since the last read (every 3 days).
                last = edited(&last, read, &format!("release r{n}"));
                out.push(last.clone());
            } else {
                // The same content again: the same record id (deduplicated).
                let mut same = last.clone();
                same.observed_ms = read;
                same.parsed_ms = read + 1_000;
                same.snapshots = vec![snap(&last.source_id, &last.native_id, read, "same")];
                out.push(same.with_identity());
            }
        }
    }
    for d in 0..9i64 {
        if (3..6).contains(&d) {
            continue; // three days without a complete read: listings go stale
        }
        for half in 0..2 {
            let fetched = T0 + d * D + half * 12 * H + 2 * H;
            coverage.push(self::coverage(
                Src::REPO,
                "repos",
                fetched,
                T0 - D,
                fetched,
                true,
            ));
        }
    }
    coverage.push(self::coverage(
        Src::REPO,
        "repos",
        T0 + 4 * D,
        T0 - D,
        T0 + 4 * D,
        false,
    ));
    World {
        name: "in_place",
        records: out,
        coverage,
        purges: Vec::new(),
        policies: policies(&[Src::REPO]),
        span: (T0 - H, T0 + 9 * D),
    }
    .checked()
}
