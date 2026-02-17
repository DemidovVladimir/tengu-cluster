//! Configuration schema and runtime profile resolution.
//!
//! TODO(epic-config-validation): Add explicit validation pass for cross-field constraints
//! (engine availability, lens/budget sanity, store path checks).
mod profile;
mod schema;

pub use profile::*;
pub use schema::*;
