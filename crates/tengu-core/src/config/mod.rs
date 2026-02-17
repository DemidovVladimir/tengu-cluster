//! Configuration schema and runtime profile helpers.
//!
//! Potential use case:
//! Import one module to load agent config and choose a runtime profile.

mod profile;
mod schema;

pub use profile::*;
pub use schema::*;
