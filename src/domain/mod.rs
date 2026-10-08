//! Domain — plain data and pure policy. Imports nothing from the rest of the
//! crate except `domain` itself, and no IO crates (see
//! `tests/layering_lint.rs`).

pub(crate) mod backoff;
pub(crate) mod backtest;
pub(crate) mod book;
pub(crate) mod calendar;
pub(crate) mod canonical;
pub(crate) mod decision;
pub(crate) mod engine_smoke;
pub(crate) mod evidence;
pub(crate) mod evidence_coverage;
pub(crate) mod hl;
pub(crate) mod lineage;
pub(crate) mod lp;
pub(crate) mod market;
pub(crate) mod marketdata;
pub(crate) mod marketdata_decode;
pub(crate) mod marketdata_stats;
pub(crate) mod memory;
pub(crate) mod message;
pub(crate) mod metrics;
pub(crate) mod observation;
pub(crate) mod plan;
pub(crate) mod runtime;
pub(crate) mod schedule;
pub(crate) mod scope;
pub(crate) mod secrets;
pub(crate) mod session;
pub(crate) mod solana;
pub(crate) mod solana_tx;
pub(crate) mod solana_write;
pub(crate) mod token;
pub(crate) mod tools;
pub(crate) mod tz;
pub(crate) mod usage;
pub(crate) mod xm;
