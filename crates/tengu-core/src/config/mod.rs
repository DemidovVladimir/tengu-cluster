//! Configuration schema and runtime profile helpers.
//!
//! Potential use case:
//! Import one module to load agent config and choose a runtime profile.

mod policy;
mod profile;
mod schema;

pub use policy::*;
pub use profile::*;
pub use schema::*;
