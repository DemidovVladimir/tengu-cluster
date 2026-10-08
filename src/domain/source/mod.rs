//! Source layer (O2) — pure provenance types for facts read from approved
//! external sources (SEC EDGAR, EU TED). Cross-domain: the Software
//! Opportunity Engine (`domain/soe/`) and later xmarket `info-*` reuse it;
//! imports go soe → source, never back. The fetch, the raw snapshots and the
//! append-only store live outside `domain`.
//!
//! | Module | Holds |
//! |---|---|
//! | `record` | [`SourceRecord`] (`source_record/1`, PRD §5.2 lossless), [`Fact`] (`SecFiling` · `TedNotice` · `Unparsed`), [`NativeAmount`], [`Place`], [`Origin`] (syndication), [`Inference`] (never a fact), [`SourceClass`], [`Trust`], [`ParseStatus`], [`AccessMethod`] |

pub(crate) mod record;

// Consumers (as-of view, store, `domain/soe/`) land with the next O2 / O1 steps.
#[allow(unused_imports)]
pub(crate) use record::{
    AccessMethod, Fact, Inference, NativeAmount, Origin, ParseError, ParseStatus, Place, PlaceRole,
    PlaceScheme, SecFiling, SourceClass, SourceRecord, TedNotice, Trust,
};
