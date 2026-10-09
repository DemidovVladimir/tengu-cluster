//! Source layer (O2) — pure provenance types for facts read from approved
//! external sources (SEC EDGAR, EU TED), and the as-of evidence view over
//! them. Cross-domain: the Software Opportunity Engine (`domain/soe/`) and
//! later xmarket `info-*` reuse it; imports go soe → source, never back. The
//! fetch, the raw snapshots and the append-only store live outside `domain`;
//! the registry rows are `[sources]` (`config/sources.rs`).
//!
//! | Module | Holds |
//! |---|---|
//! | `record` | [`SourceRecord`] (`source_record/1`, PRD §5.2 lossless), [`Fact`] (`SecFiling` · `TedNotice` · `Unparsed` · `Withdrawn`), [`NativeAmount`], [`Place`], [`Origin`] (syndication), [`Inference`] (never a fact), [`SourceClass`], [`Trust`], [`ParseStatus`], [`AccessMethod`], [`SourceRecord::withdrawal`] |
//! | `rules` | per-class rules of PRD §5.1 (jurisdiction for law / regulator, listing freshness for registries, aggregate demand) as [`Issue`]s; [`Revision`] (`immutable` · `in_place`) and the [`SourcePolicy`] of a registry row |
//! | `asof` | the two-clock view at t ([`AsOfMode`] `captured` · `knowable`): current version per item, corrections, states (in force · pending · expired · withdrawn · unparsed), issues, confidence per event, conflicts; [`Coverage`], [`Purge`] |
//! | `packet` | [`EvidencePacket`] `source_asof/1` (an `Observed` row): facts, superseded, events, conflicts, issues, demand, freshness, citations, inferences apart; `page` keeps the newest records (the rest `omitted`); `render_text` fences every free text ([`fence_untrusted`]) |
//! | `sec_records` | SEC EDGAR filing → record (`sec-submissions/1`): accession = native id and event, `sec:cik:` entity, index `Accepted` time; an unread index ⇒ `partial` with a time never before the acceptance |
//! | `ted` | EU TED Search notice → record (`ted-search/1`): the one-day search body ([`ted::TED_FIELDS`], no contact field), the page decoder (a bad notice is `partial` / `unparsed`, never a dropped page), publication = the end of its date, procedure = event (`ted:notice:` for a planning notice), change notices `supersedes` the record they correct |
//! | `testkit` · `checks` · `eval` | tests only: fixture sources and worlds; split-world no-lookahead checks; the 12 dated replay cases of `tests/fixtures/sources/eval/cases.json` (O2 exit) |

pub(crate) mod asof;
#[cfg(test)]
mod checks;
#[cfg(test)]
mod eval;
pub(crate) mod packet;
pub(crate) mod record;
pub(crate) mod rules;
pub(crate) mod sec_records;
pub(crate) mod ted;
#[cfg(test)]
pub(crate) mod testkit;

// The source layer's surface: `domain/soe/` (lane B, O1 / O3) imports from
// here; not every name has a consumer in this build yet.
#[allow(unused_imports)]
pub(crate) use asof::{
    as_of, AsOfInput, AsOfMode, AsOfView, Confidence, Conflict, Coverage, EventRow, Purge,
    RecordState, SupersededBy, Supersession,
};
#[allow(unused_imports)]
pub(crate) use packet::{
    fence_untrusted, AsOfQuery, Citation, DemandSummary, EvidencePacket, FactRow, Freshness,
    QueryFreshness, UnparsedRow, WithdrawnRow, FENCE_NOTE, PACKET_SCHEMA,
};
#[allow(unused_imports)]
pub(crate) use record::{
    AccessMethod, Fact, Inference, NativeAmount, Origin, ParseError, ParseStatus, Place, PlaceRole,
    PlaceScheme, SecFiling, SourceClass, SourceRecord, SourceStamp, TedNotice, Trust, Withdrawn,
    WithdrawnHow,
};
#[allow(unused_imports)]
pub(crate) use rules::{
    class_issues, valid_jurisdiction, DemandStatus, Issue, IssueCode, Revision, SourcePolicy,
    MIN_DEMAND_EVENTS,
};
